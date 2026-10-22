//! `convert-game`: migrate a full game installation into an `.akpak` pack.
//!
//! Discovers `STAGE1`..`STAGE7`, `ENEMY`, `PLAYERS`, `sound`, `objspr`,
//! `ITEM_M1`, `effspr`, `voice`, `movie` and `DATA` (case-insensitively, up to
//! two levels below the root), stores every `ROOM####.RDT`, converts the camera
//! backgrounds of every distinct room once, converts the room mask pages of
//! every camera that has sprite groups, copies the door animations named by the
//! door type table, copies the `BGM_*.WAV` music files, the 68 named sound
//! effects, the four player models plus the two no-weapon locomotion clips,
//! the fifteen scripted character (NPC) models, the 33 effect-sheet TIMs, the
//! `core00` weapon-effect metadata and the `KAGE.TIM` player-shadow coverage
//! page. Stages 6 and 7 reuse the backgrounds and mask pages of STAGE1/STAGE2
//! with the stage digit reduced by 5.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};

use crate::enemy;
use crate::items;
use crate::manifest;
use crate::model::Texture8;
use crate::movie;
use crate::music;
use crate::pack::PackWriter;
use crate::progress::{Progress, format_duration};
use crate::sfx;
use crate::state::{Image, RoomId};
use crate::voice;
use crate::voice_table;
use crate::{bmp, door, lzw, rdt, tim};

/// How many directory levels below the root are searched for the stage and
/// `sound` directories.
///
/// Two levels cover the single-tree layouts (`<root>/DATA`, `<root>/JPN/DATA`);
/// three cover the Steam release's install root, whose language trees each
/// hold a complete game (`<root>/english/USA/Data`, `<root>/japanese/JPN/DATA`).
const MAX_DEPTH: usize = 3;

/// Expected camera background width.
const CUT_WIDTH: u32 = 320;
/// Expected camera background height.
const CUT_HEIGHT: u32 = 240;

/// Byte offset of the bit depth field in a BMP header.
const BMP_BPP_OFFSET: usize = 28;

/// Bytes buffered while streaming the finished pack to disk.
const WRITE_CHUNK: usize = 1 << 20;

/// Entry extensions stored raw in the pack: these formats are already
/// compressed with their own codec, so zstd cannot shrink them and would only
/// cost conversion time. The list grows as more formats are supported.
///
/// `.wav` is deliberately not exempt: the shipped PCM is not otherwise
/// compressed, and zstd takes the RE1 pack's 178.4 MB of WAVs to 122.1 MB
/// (68.5%). Expanding the 8-bit files to 16-bit PCM first was strictly worse
/// (18.2 MB vs 15.5 MB for those 60 files) and recovers no quality, so the
/// pack stores the shipped samples as they are.
const PRECOMPRESSED_EXTENSIONS: &[&str] = &["avi", "mp4", "ogg", "png", "webp"];

/// Whether an entry's format already carries its own compression, in which
/// case `convert-game` stores it raw instead of zstd-compressing it.
fn is_precompressed(entry: &str) -> bool {
    Path::new(entry).extension().is_some_and(|extension| {
        PRECOMPRESSED_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

/// Resolve the requested worker count: `0` selects one worker per CPU.
fn worker_count(jobs: usize) -> usize {
    if jobs != 0 {
        return jobs.max(1);
    }
    std::thread::available_parallelism().map_or(1, |count| count.get())
}

/// One parallel job's result: the progress label, the phase units it completes
/// and the value it produces.
type JobResult<T> = Result<(String, u64, T)>;

/// Run `job(0..count)` on up to `jobs` scoped workers, returning the results in
/// job order and advancing `progress` as each job completes.
///
/// The first error returned by a job is reported, so failures do not depend on
/// thread scheduling; once a job fails no further jobs are claimed, though work
/// already in flight still finishes. A panicking job unwinds through the scope.
fn parallel_map<T: Send>(
    count: usize,
    jobs: usize,
    progress: &mut Progress,
    job: impl Fn(usize) -> JobResult<T> + Sync,
) -> Result<Vec<(String, T)>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let threads = jobs.clamp(1, count);
    if threads == 1 {
        let mut out = Vec::with_capacity(count);
        for index in 0..count {
            let (label, units, value) = job(index)?;
            progress.advance_named(units, &label);
            out.push((label, value));
        }
        return Ok(out);
    }

    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let progress = Mutex::new(progress);
    let results = Mutex::new(Vec::with_capacity(count));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        return;
                    }
                    let result = job(index);
                    match &result {
                        Ok((label, units, _)) => {
                            progress
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .advance_named(*units, label);
                        }
                        Err(_) => stop.store(true, Ordering::Relaxed),
                    }
                    results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((index, result));
                }
            });
        }
    });

    let mut collected = results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    collected.sort_by_key(|(index, _)| *index);
    let mut out = Vec::with_capacity(count);
    for (_, result) in collected {
        let (label, _, value) = result?;
        out.push((label, value));
    }
    Ok(out)
}

/// A writer that advances a byte-based progress phase as data is written.
struct ProgressWriter<'a, W> {
    inner: W,
    progress: &'a mut Progress,
}

impl<W: Write> Write for ProgressWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.progress.advance_by(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Read and add `files` to the pack on the worker pool.
fn copy_raw_files(
    files: Vec<(String, PathBuf)>,
    label: &str,
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    progress.begin(label, files.len() as u64);
    let results = parallel_map(files.len(), jobs, progress, |index| {
        let (entry, source) = &files[index];
        let data =
            fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
        Ok((entry.clone(), 1, data))
    })?;
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (entry, data) in results {
        bytes += data.len();
        count += 1;
        writer
            .add(&entry, data)
            .with_context(|| format!("failed to add {entry}"))?;
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// One `--verbose` report line:
/// `path: plain -> stored bytes (75.0% smaller, zstd)`.
///
/// Entries stored as-is are marked `stored raw`: that is every format that
/// carries its own compression plus the entries whose zstd frame did not pay
/// for itself.
fn entry_report_line(path: &str, plain_len: usize, stored_len: usize, compressed: bool) -> String {
    if compressed {
        let saved = 100.0 - stored_len as f64 * 100.0 / plain_len.max(1) as f64;
        format!("{path}: {plain_len} -> {stored_len} bytes ({saved:.1}% smaller, zstd)")
    } else {
        format!("{path}: {plain_len} -> {stored_len} bytes (stored raw)")
    }
}

/// Write the `--verbose` per-file report: one line per packed entry, in
/// processing order, with its plain and stored size.
fn write_entry_report(writer: &PackWriter, out: &mut impl Write) -> std::io::Result<()> {
    for entry in writer.entries() {
        writeln!(
            out,
            "{}",
            entry_report_line(
                entry.path(),
                entry.plain_len(),
                entry.stored_len(),
                entry.compressed(),
            )
        )?;
    }
    Ok(())
}

/// Flush the buffered stdout report.
///
/// The pack is already written and renamed, so a closed stdout (a `| head`
/// downstream exiting early) is not an error: the report is simply dropped.
/// Any other write failure is reported.
fn flush_report(out: &mut impl Write, report: &[u8]) -> Result<()> {
    let written = out.write_all(report).and_then(|()| out.flush());
    if let Err(error) = written
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(error).context("failed to write the conversion report");
    }
    Ok(())
}

/// Format a byte count as mebibytes with one decimal, e.g. `461.8 MiB`.
fn format_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MiB", bytes as f64 / MIB)
}

/// Convert the game installation under `root` into the `.akpak` pack `out`.
///
/// The pack carries no map or combat tables: the engine's built-in copies are
/// the sole source, so no original executable is needed.
pub fn convert_game(root: &Path, out: &Path) -> Result<()> {
    convert_game_with_options(root, out, 0, false)
}

/// Like [`convert_game`], but with an explicit worker count and whether the
/// verbose per-file report is printed.
///
/// A `jobs` of `0` selects one worker per available CPU, and the output pack
/// is byte-identical whatever the worker count. Every asset category — rooms,
/// audio, voice and films — lands in the one output pack. Progress is
/// reported on stderr; with `verbose` the run also prints the stage and
/// category totals and one line per packed file with its plain and stored
/// size on stdout.
///
/// The root may be a single game tree (`<root>/DATA`, `<root>/JPN/DATA`), a
/// tree's own directory, or the Steam release's install root whose language
/// trees (`english/USA`, `japanese/JPN`, ...) each hold a complete game. Every
/// game-text sheet the root carries is packed — the tree the data comes from
/// is the default and the other script's sheets ride along as `font/latin.tim`
/// / `font/jpn.tim` — so the one pack renders every language with its own
/// sheet. The other trees also fill in assets the conversion tree omits (the
/// Japanese document pages) and their release-specific film names resolve to
/// the canonical `movie/` entries.
pub fn convert_game_with_options(
    root: &Path,
    out: &Path,
    jobs: usize,
    verbose: bool,
) -> Result<()> {
    let jobs = worker_count(jobs);
    // The phase count is only known once the plan is built; `build_plan` fills
    // it in before reporting the scan, the first phase.
    let mut progress = Progress::new("convert-game", 0);
    let Plan {
        rooms,
        sound,
        item_m1,
        item_m2_dirs,
        players,
        weapons,
        npc,
        enemy_models,
        roommask,
        effects,
        data,
        fonts,
        language,
        voice,
        voice_unreferenced,
        voice_dir_found,
        movie,
        movie_dir_found,
        mut warnings,
    } = build_plan_with_progress(root, jobs, &mut progress)?;
    if !voice_dir_found {
        warnings.push(format!(
            "no voice directory found; {} voice line(s) will be missing",
            voice_table::entry_count()
        ));
    }
    if !movie_dir_found {
        warnings.push(format!(
            "no movie directory found; {} film(s) will be missing",
            movie::required_name_count()
        ));
    }

    let mut writer = PackWriter::new();
    let mut stage_counts = [(0usize, 0usize); RoomId::MAX_STAGE as usize];
    let mut rdt_count = 0usize;
    let mut rdt_bytes = 0usize;
    let mut cut_count = 0usize;
    let mut cut_bytes = 0usize;
    let mut mask_count = 0usize;
    let mut mask_bytes = 0usize;

    // Group the cut jobs by source file first, so stages 6 and 7 that reuse a
    // STAGE1/STAGE2 background decode it once and reuse the BMP for every
    // entry. Entry order does not matter: the pack writer sorts by path.
    let mut cut_sources: BTreeMap<PathBuf, Vec<(RoomId, usize)>> = BTreeMap::new();
    let mut cut_total = 0u64;
    for room in &rooms {
        for (camera, pak) in room.paks.iter().enumerate() {
            cut_sources
                .entry(pak.clone())
                .or_default()
                .push((room.id, camera));
            cut_total += 1;
        }
    }

    let rdt_total = rooms.iter().map(|room| room.rdts.len() as u64).sum();
    progress.begin("room", rdt_total);
    for room in rooms {
        for rdt in room.rdts {
            let entry = rdt.id.rdt_entry();
            rdt_bytes += rdt.bytes.len();
            rdt_count += 1;
            stage_counts[rdt.id.stage_index() as usize].0 += 1;
            writer
                .add(&entry, rdt.bytes)
                .with_context(|| format!("failed to add {entry}"))?;
            progress.advance(&entry);
        }
    }
    progress.end_phase();

    progress.begin("roomcut", cut_total);
    let cut_groups: Vec<(PathBuf, Vec<(RoomId, usize)>)> = cut_sources.into_iter().collect();
    let cut_results = parallel_map(cut_groups.len(), jobs, &mut progress, |index| {
        let (pak, targets) = &cut_groups[index];
        let mut bmp_bytes = convert_camera(pak)?;
        let label = targets
            .first()
            .map(|(id, camera)| id.cut_entry(*camera))
            .unwrap_or_default();
        debug_assert!(!targets.is_empty(), "cut groups are never empty");
        let mut entries = Vec::with_capacity(targets.len());
        let last = targets.len().saturating_sub(1);
        for (position, (id, camera)) in targets.iter().enumerate() {
            let entry = id.cut_entry(*camera);
            let data = if position == last {
                std::mem::take(&mut bmp_bytes)
            } else {
                bmp_bytes.clone()
            };
            entries.push((*id, entry, data));
        }
        Ok((label, targets.len() as u64, entries))
    })?;
    for (_label, entries) in cut_results {
        for (id, entry, bytes) in entries {
            cut_bytes += bytes.len();
            cut_count += 1;
            stage_counts[id.stage_index() as usize].1 += 1;
            writer
                .add(&entry, bytes)
                .with_context(|| format!("failed to add {entry}"))?;
        }
    }
    progress.end_phase();

    // Mask pages sharing a source `OSP*.pak` decode once too.
    let mut mask_sources: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for asset in &roommask {
        mask_sources
            .entry(asset.source.clone())
            .or_default()
            .push(asset.entry.clone());
    }
    progress.begin("roommask", roommask.len() as u64);
    let mask_groups: Vec<(PathBuf, Vec<String>)> = mask_sources.into_iter().collect();
    let mask_results = parallel_map(mask_groups.len(), jobs, &mut progress, |index| {
        let (pak, targets) = &mask_groups[index];
        let mut bmp_bytes = convert_roommask(pak)?;
        let label = targets.first().cloned().unwrap_or_default();
        debug_assert!(!targets.is_empty(), "mask groups are never empty");
        let mut entries = Vec::with_capacity(targets.len());
        let last = targets.len().saturating_sub(1);
        for (position, entry) in targets.iter().enumerate() {
            let data = if position == last {
                std::mem::take(&mut bmp_bytes)
            } else {
                bmp_bytes.clone()
            };
            entries.push((entry.clone(), data));
        }
        Ok((label, targets.len() as u64, entries))
    })?;
    for (_label, entries) in mask_results {
        for (entry, bytes) in entries {
            mask_bytes += bytes.len();
            mask_count += 1;
            writer
                .add(&entry, bytes)
                .with_context(|| format!("failed to add {entry}"))?;
        }
    }
    progress.end_phase();

    let (bgm_count, bgm_bytes) = copy_music(&sound, &mut writer, &mut progress, jobs)?;
    let (se_count, se_bytes) = copy_se(&sound, &mut writer, &mut progress, jobs)?;
    let (door_count, door_bytes) = copy_doors(&item_m1, &mut writer, &mut progress, jobs)?;
    let (player_count, player_bytes) = copy_players(&players, &mut writer, &mut progress, jobs)?;
    let (weapon_count, weapon_bytes) =
        copy_weapon_assets(&weapons, &mut writer, &mut progress, jobs)?;
    let (npc_count, npc_bytes) = copy_npc_models(&npc, &mut writer, &mut progress, jobs)?;
    let (enemy_count, enemy_bytes) =
        copy_enemy_models(&enemy_models, &mut writer, &mut progress, jobs)?;
    let (enemylua_count, enemylua_bytes) = copy_scripts(
        "enemylua",
        crate::enemy::ENEMY_SCRIPTS,
        &mut writer,
        &mut progress,
    )?;
    let (uilua_count, uilua_bytes) =
        copy_scripts("uilua", crate::ui::UI_SCRIPTS, &mut writer, &mut progress)?;
    let (effect_count, effect_bytes) =
        copy_effect_sheets(&effects, &mut writer, &mut progress, jobs)?;
    let (font_count, font_bytes) = copy_font(&fonts, &mut writer, &mut progress)?;
    let (ui_count, ui_bytes) = copy_ui_art(&data, &mut writer, &mut progress, jobs)?;
    let (item_count, item_bytes) = copy_item_art(&data, &mut writer, &mut progress, jobs)?;
    let (data_count, data_bytes) = copy_bio_card(&data, &mut writer, &mut progress)?;
    let (demo_count, demo_bytes) = copy_demos(&data, &mut writer, &mut progress)?;
    let (core_count, core_bytes) = copy_core_effects(&data, &mut writer, &mut progress, jobs)?;
    let (shadow_count, shadow_bytes) = copy_shadow(&data, &mut writer, &mut progress)?;
    let (language_count, language_bytes) = copy_language_files(&mut writer, &mut progress)?;
    let (ivm_count, ivm_bytes) = copy_item_models(
        &item_m2_dirs,
        &mut writer,
        &mut progress,
        &mut warnings,
        jobs,
    )?;
    let (file_count, file_bytes) = copy_file_art(
        &item_m2_dirs,
        &mut writer,
        &mut progress,
        &mut warnings,
        jobs,
    )?;
    let (map_count, map_bytes) = copy_map_art(
        &item_m2_dirs,
        &mut writer,
        &mut progress,
        &mut warnings,
        jobs,
    )?;
    let (manifest_count, manifest_bytes) = copy_manifest(&mut writer, &language)?;

    // Voice and film land in the same pack as every other category, so a
    // converted game is a single self-contained `.akpak`. Both phases are
    // always reported (empty ones draw nothing) so the overall bar's phase
    // count is fixed before the run starts.
    let (voice_count, voice_bytes) = copy_voice(&voice, &mut writer, &mut progress, jobs)?;
    let (movie_count, movie_bytes) = copy_movies(&movie, &mut writer, &mut progress, jobs)?;

    // Zstd-compress every entry except the formats that already carry their
    // own compression, spread over the worker pool. Entries that would not
    // shrink stay raw, so this can only help.
    let entries = writer.len();
    progress.begin("compress", entries as u64);
    let compressed = {
        // The pass runs on its own worker pool, so the reporter is shared;
        // only the bar sees the completion order, which does not matter.
        let progress = Mutex::new(&mut progress);
        writer.compress_entries_reporting(jobs, is_precompressed, |entry| {
            progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .advance(entry);
        })?
    };
    progress.end_phase();

    // Stream the pack straight to disk: the entry data is already in memory,
    // so materializing a second serialized copy would only cost memory and a
    // full memcpy. The bytes land in a sibling temporary file that is renamed
    // over `out` once the stream is complete, so a failed conversion leaves no
    // partial pack behind.
    let size = writer.pack_size()?;
    progress.begin("write", size as u64);
    let temp = crate::atomic::temp_path(out)?;
    let file =
        fs::File::create(&temp).with_context(|| format!("failed to create {}", temp.display()))?;
    let mut file = ProgressWriter {
        inner: BufWriter::with_capacity(WRITE_CHUNK, file),
        progress: &mut progress,
    };
    let result = writer
        .stream_to(&mut file)
        .with_context(|| format!("failed to write {}", out.display()))
        .and_then(|()| {
            file.flush()
                .with_context(|| format!("failed to write {}", out.display()))
        });
    drop(file);
    if let Err(err) = result {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    fs::rename(&temp, out).with_context(|| {
        let _ = fs::remove_file(&temp);
        format!("failed to move the written pack into {}", out.display())
    })?;
    progress.end_phase();
    progress.finish();

    // The bar is gone now, so the warnings and the final status read cleanly.
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    // The report is buffered and written once the pack has landed, so a
    // stdout failure (a `... | head` closing the pipe, for example) can never
    // abort the conversion with the pack unwritten. The verbose per-file
    // lines name the stored size next to the plain one and stay in processing
    // order, which is deterministic whatever the worker count.
    let mut report = Vec::new();
    if verbose {
        for (index, (rdts, cuts)) in stage_counts.iter().enumerate() {
            writeln!(report, "STAGE{}: {rdts} RDT(s), {cuts} cut(s)", index + 1)?;
        }
        writeln!(report, "room: {rdt_count} entries, {rdt_bytes} bytes")?;
        writeln!(report, "roomcut: {cut_count} entries, {cut_bytes} bytes")?;
        writeln!(report, "roommask: {mask_count} entries, {mask_bytes} bytes")?;
        writeln!(report, "bgm: {bgm_count} entries, {bgm_bytes} bytes")?;
        writeln!(report, "se: {se_count} entries, {se_bytes} bytes")?;
        writeln!(report, "door: {door_count} entries, {door_bytes} bytes")?;
        writeln!(
            report,
            "player: {player_count} entries, {player_bytes} bytes"
        )?;
        writeln!(
            report,
            "weapon: {weapon_count} entries, {weapon_bytes} bytes"
        )?;
        writeln!(report, "enemy: {npc_count} entries, {npc_bytes} bytes")?;
        writeln!(
            report,
            "monster: {enemy_count} entries, {enemy_bytes} bytes"
        )?;
        writeln!(
            report,
            "enemylua: {enemylua_count} entries, {enemylua_bytes} bytes"
        )?;
        writeln!(report, "uilua: {uilua_count} entries, {uilua_bytes} bytes")?;
        writeln!(
            report,
            "effspr: {effect_count} entries, {effect_bytes} bytes"
        )?;
        writeln!(report, "font: {font_count} entries, {font_bytes} bytes")?;
        writeln!(report, "ui: {ui_count} entries, {ui_bytes} bytes")?;
        writeln!(report, "item: {item_count} entries, {item_bytes} bytes")?;
        writeln!(report, "data: {data_count} entries, {data_bytes} bytes")?;
        writeln!(report, "demo: {demo_count} entries, {demo_bytes} bytes")?;
        writeln!(report, "core00: {core_count} entries, {core_bytes} bytes")?;
        writeln!(
            report,
            "shadow: {shadow_count} entries, {shadow_bytes} bytes"
        )?;
        writeln!(
            report,
            "language: {language_count} entries, {language_bytes} bytes"
        )?;
        writeln!(report, "ivm: {ivm_count} entries, {ivm_bytes} bytes")?;
        writeln!(report, "file: {file_count} entries, {file_bytes} bytes")?;
        writeln!(report, "map: {map_count} entries, {map_bytes} bytes")?;
        writeln!(
            report,
            "manifest: {manifest_count} entry, {manifest_bytes} bytes (base \"{}\")",
            base_manifest("ja-JP").id
        )?;
        writeln!(report, "voice: {voice_count} entries, {voice_bytes} bytes")?;
        if !voice_unreferenced.is_empty() {
            writeln!(
                report,
                "voice: {} unreferenced file(s) not packed: {}",
                voice_unreferenced.len(),
                voice_unreferenced.join(", ")
            )?;
        }
        writeln!(report, "movie: {movie_count} entries, {movie_bytes} bytes")?;
        writeln!(
            report,
            "compress: {compressed} of {entries} entries stored with zstd (level {})",
            crate::pack::ZSTD_LEVEL
        )?;
        write_entry_report(&writer, &mut report)?;
    }
    writeln!(
        report,
        "Created {} ({entries} entries, {}) in {}",
        out.display(),
        format_size(size as u64),
        format_duration(progress.elapsed())
    )?;
    let mut stdout = std::io::stdout().lock();
    flush_report(&mut stdout, &report)?;
    Ok(())
}

/// Decode one `RC*.pak` camera background into BMP bytes.
fn convert_camera(pak: &Path) -> Result<Vec<u8>> {
    let source = file_name(pak);
    let compressed = fs::read(pak).with_context(|| format!("failed to read {}", pak.display()))?;
    let decoded =
        lzw::decode(&compressed).with_context(|| format!("failed to LZW-decode {source}"))?;
    let image =
        tim::decode(&decoded).with_context(|| format!("failed to decode {source} as TIM"))?;
    // The retail data contains one 316x236 background (the screen-border
    // variant), so any non-empty image up to the 320x240 frame is accepted.
    if image.width == 0 || image.height == 0 || image.width > CUT_WIDTH || image.height > CUT_HEIGHT
    {
        bail!(
            "{source}: background {}x{} does not fit in {CUT_WIDTH}x{CUT_HEIGHT}",
            image.width,
            image.height
        );
    }

    let bmp_bytes =
        bmp::encode_to_vec(&image).with_context(|| format!("failed to encode {source} as BMP"))?;
    let bpp = bmp_bpp(&bmp_bytes)?;
    if bpp != 8 && bpp != 24 {
        bail!("{source}: encoded BMP has unsupported bit depth {bpp}");
    }
    Ok(bmp_bytes)
}

/// Decode one `OSP*.pak` room mask page into BMP bytes.
fn convert_roommask(pak: &Path) -> Result<Vec<u8>> {
    let source = file_name(pak);
    let compressed = fs::read(pak).with_context(|| format!("failed to read {}", pak.display()))?;
    let decoded = lzw::decode(&compressed)
        .with_context(|| format!("failed to LZW-decode room mask {source}"))?;
    let texture = tim::decode_8bpp(&decoded)
        .with_context(|| format!("failed to decode room mask {source} as an 8bpp TIM"))?;
    bmp::encode_texture8_to_vec(&texture)
        .with_context(|| format!("failed to encode room mask {source} as BMP"))
}

/// Add every `BGM_*.WAV` in the sound directory; returns entry count and bytes.
fn copy_music(
    sound: &Option<PathBuf>,
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let Some(sound) = sound else {
        return Ok((0, 0));
    };

    let mut files = Vec::new();
    for entry in read_dir_sorted(sound)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(path) = music::pack_path(name) else {
            continue;
        };
        files.push((path, entry.path()));
    }

    copy_raw_files(files, "bgm", writer, progress, jobs)
}

/// Add every sound effect named by the room sound tables.
///
/// The pack stores the canonical names lowercased (`se/ft_wda.wav`); the
/// install's file names are matched case-insensitively and copied raw.
fn copy_se(
    sound: &Option<PathBuf>,
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let Some(sound) = sound else {
        return Ok((0, 0));
    };

    let index = index_dir(sound)?;
    let mut files = Vec::new();
    let mut missing = Vec::new();
    // The named sound effects plus the non-`Bgm_*` group tracks (the muted
    // seeds, the lab cues and `V110_00`) the three-channel BGM engine reads
    // from `se/`.
    for name in sfx::SE_NAMES.iter().copied().chain(music::se_track_names()) {
        let file = format!("{}.wav", name.to_ascii_lowercase());
        match index.get(&file) {
            Some(path) => files.push((format!("se/{file}"), path.clone())),
            None => missing.push(format!("{}.WAV", name.to_ascii_uppercase())),
        }
    }
    if !missing.is_empty() {
        bail!(
            "missing {} sound effect file(s) in {}: {}",
            missing.len(),
            sound.display(),
            missing.join(", ")
        );
    }

    copy_raw_files(files, "se", writer, progress, jobs)
}

/// Add every resolved voice WAV to the pack.
///
/// The entries are the non-empty records of the per-stage name rows, named by
/// their `voice/{row}_{id}.wav` position; the shipped-but-unreferenced files
/// (`ANNOUNCE`, the `V111_*` group and the variants) are deliberately left out
/// and listed in the summary.
fn copy_voice(
    assets: &[VoiceAsset],
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let files = assets
        .iter()
        .map(|asset| (asset.entry.clone(), asset.source.clone()))
        .collect();
    copy_raw_files(files, "voice", writer, progress, jobs)
}

/// Add every resolved film AVI to the pack verbatim, under its canonical
/// `movie/{name}.avi` entry.
fn copy_movies(
    assets: &[MovieAsset],
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let files = assets
        .iter()
        .map(|asset| (asset.entry.clone(), asset.source.clone()))
        .collect();
    copy_raw_files(files, "movie", writer, progress, jobs)
}

/// Add every door animation named by the door type table.
///
/// The pack stores `door/{stem}.dor` (the stems are already lower-case); the
/// install's `ITEM_M1` file names are matched case-insensitively and copied
/// raw. Every type byte `0x00..=0x21` names a distinct `.dor`; higher bytes
/// reuse `door00`, so they add no extra file.
fn copy_doors(
    item_m1: &Option<PathBuf>,
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let Some(dir) = item_m1 else {
        return Ok((0, 0));
    };

    let index = index_dir(dir)?;
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for door_type in 0..=0x21u8 {
        let name = door::type_name(door_type);
        let file = format!("{name}.dor");
        match index.get(&file) {
            Some(path) => files.push((format!("door/{file}"), path.clone())),
            None => missing.push(format!("{}.DOR", name.to_ascii_uppercase())),
        }
    }
    if !missing.is_empty() {
        bail!(
            "missing {} door animation file(s) in {}: {}",
            missing.len(),
            dir.display(),
            missing.join(", ")
        );
    }

    copy_raw_files(files, "door", writer, progress, jobs)
}

/// Add every resolved player model and locomotion clip to the pack.
fn copy_players(
    players: &[PlayerAsset],
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let files = players
        .iter()
        .map(|asset| (asset.entry.clone(), asset.source.clone()))
        .collect();
    copy_raw_files(files, "player", writer, progress, jobs)
}

/// Add every resolved weapon animation and held-weapon model to the pack.
fn copy_weapon_assets(
    weapons: &[WeaponAsset],
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let files = weapons
        .iter()
        .map(|asset| (asset.entry.clone(), asset.source.clone()))
        .collect();
    copy_raw_files(files, "weapon", writer, progress, jobs)
}

/// The 33 shipped effect-sheet names: `esp000`, `esp001` and `esp200`..`esp230`.
///
/// The room→sheet map references 31 of them; `esp221` and `esp224` ship
/// unreferenced and are packed anyway.
const EFFECT_SHEET_FILES: [&str; 33] = [
    "esp000", "esp001", "esp200", "esp201", "esp202", "esp203", "esp204", "esp205", "esp206",
    "esp207", "esp208", "esp209", "esp210", "esp211", "esp212", "esp213", "esp214", "esp215",
    "esp216", "esp217", "esp218", "esp219", "esp220", "esp221", "esp222", "esp223", "esp224",
    "esp225", "esp226", "esp227", "esp228", "esp229", "esp230",
];

/// Add every resolved effect-sheet TIM to the pack raw.
fn copy_effect_sheets(
    assets: &[EffectSheet],
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let files = assets
        .iter()
        .map(|asset| (asset.entry.clone(), asset.source.clone()))
        .collect();
    copy_raw_files(files, "effspr", writer, progress, jobs)
}

/// Add every resolved scripted-character (NPC) model to the pack raw.
fn copy_npc_models(
    assets: &[NpcAsset],
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    progress.begin("enemy", assets.len() as u64);
    let results = parallel_map(assets.len(), jobs, progress, |index| {
        let asset = &assets[index];
        let data = fs::read(&asset.source).with_context(|| {
            format!(
                "failed to read NPC model {:#04x} ({})",
                asset.id,
                asset.source.display()
            )
        })?;
        Ok((asset.entry.clone(), 1, data))
    })?;
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (entry, data) in results {
        bytes += data.len();
        count += 1;
        writer
            .add(&entry, data)
            .with_context(|| format!("failed to add {entry}"))?;
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Add every resolved monster model of both scenario blocks to the pack raw.
fn copy_enemy_models(
    assets: &[EnemyAsset],
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    progress.begin("monster", assets.len() as u64);
    let results = parallel_map(assets.len(), jobs, progress, |index| {
        let asset = &assets[index];
        let data = fs::read(&asset.source).with_context(|| {
            format!(
                "failed to read monster model {:#04x} block {} ({})",
                asset.id,
                asset.player,
                asset.source.display()
            )
        })?;
        Ok((asset.entry.clone(), 1, data))
    })?;
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (entry, data) in results {
        bytes += data.len();
        count += 1;
        writer
            .add(&entry, data)
            .with_context(|| format!("failed to add {entry}"))?;
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Add checked-in scripts (the enemy or screen table) under their entries.
fn copy_scripts(
    phase: &str,
    scripts: &[(&str, &str)],
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    progress.begin(phase, scripts.len() as u64);
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (entry, source) in scripts {
        bytes += source.len();
        count += 1;
        writer
            .add(entry, source.as_bytes().to_vec())
            .with_context(|| format!("failed to add {entry}"))?;
        progress.advance(entry);
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Add the raw UI art and the converted title/select/options backgrounds.
///
/// The TIMs the renderer decodes itself are copied raw; the 16bpp direct
/// TIMs and headerless `.PIX` backgrounds are converted to BMP here.
fn copy_ui_art(
    data: &DataPlan,
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    progress.begin("ui", data.ui.len() as u64);
    let results = parallel_map(data.ui.len(), jobs, progress, |index| {
        let asset = &data.ui[index];
        let raw = fs::read(&asset.source)
            .with_context(|| format!("failed to read {}", asset.source.display()))?;
        let converted = convert_ui_asset(asset.kind, &raw)
            .with_context(|| format!("failed to convert {}", asset.source.display()))?;
        Ok((asset.entry.to_string(), 1, converted))
    })?;
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (entry, converted) in results {
        bytes += converted.len();
        count += 1;
        writer
            .add(&entry, converted)
            .with_context(|| format!("failed to add {entry}"))?;
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Bake the three item icon atlases through `STATUS.TIM`'s second CLUT row.
fn copy_item_art(
    data: &DataPlan,
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let Some(palette_path) = &data.palette else {
        return Ok((0, 0));
    };
    if data.items.is_empty() {
        return Ok((0, 0));
    }
    let palette_raw = fs::read(palette_path)
        .with_context(|| format!("failed to read {}", palette_path.display()))?;
    let texture = tim::decode_8bpp(&palette_raw)
        .with_context(|| format!("failed to decode {} as a CLUT TIM", palette_path.display()))?;

    progress.begin("item", data.items.len() as u64);
    let results = parallel_map(data.items.len(), jobs, progress, |index| {
        let asset = &data.items[index];
        let pix = fs::read(&asset.source)
            .with_context(|| format!("failed to read {}", asset.source.display()))?;
        let bmp_bytes = bake_item_atlas(&pix, asset.rows, &texture)
            .with_context(|| format!("failed to bake {}", asset.source.display()))?;
        Ok((asset.entry.to_string(), 1, bmp_bytes))
    })?;
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (entry, bmp_bytes) in results {
        bytes += bmp_bytes.len();
        count += 1;
        writer
            .add(&entry, bmp_bytes)
            .with_context(|| format!("failed to add {entry}"))?;
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Add the raw `data/bio_card.dat` save prefix.
fn copy_bio_card(
    data: &DataPlan,
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    let Some(source) = &data.bio_card else {
        return Ok((0, 0));
    };
    let raw = fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    progress.begin("data", 1);
    let bytes = raw.len();
    writer
        .add(BIO_CARD_ENTRY, raw)
        .with_context(|| format!("failed to add {BIO_CARD_ENTRY}"))?;
    progress.advance(BIO_CARD_ENTRY);
    progress.end_phase();
    Ok((1, bytes))
}

/// Add the raw attract-demo reels as `demo/<hex>.dat`, where the name is the
/// reel's hex number, so the title can enumerate them without a hard-coded
/// count.
fn copy_demos(
    data: &DataPlan,
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    let count_present = data.demos.iter().filter(|entry| entry.is_some()).count();
    if count_present == 0 {
        return Ok((0, 0));
    }
    progress.begin("demo", count_present as u64);
    let mut bytes = 0;
    let mut count = 0;
    for (index, source) in data.demos.iter().enumerate() {
        let Some(source) = source else {
            continue;
        };
        let raw =
            fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
        let entry = format!("demo/{index:02x}.dat");
        bytes += raw.len();
        count += 1;
        writer
            .add(&entry, raw)
            .with_context(|| format!("failed to add {entry}"))?;
        progress.advance(&entry);
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Add the raw `CORE00.ESP`/`CORE00.ETM` weapon-effect metadata.
///
/// The runtime decodes both files itself; they are copied raw.
fn copy_core_effects(
    data: &DataPlan,
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let mut files = Vec::new();
    if let Some(path) = &data.core_esp {
        files.push((
            crate::effects::room::CORE_ESP_ENTRY.to_string(),
            path.clone(),
        ));
    }
    if let Some(path) = &data.core_etm {
        files.push((
            crate::effects::room::CORE_ETM_ENTRY.to_string(),
            path.clone(),
        ));
    }
    copy_raw_files(files, "core00", writer, progress, jobs)
}

/// Add the raw `KAGE.TIM` player-shadow coverage page.
///
/// The page is copied raw: its palette's red channel is the coverage ramp the
/// renderer bakes into the texel alpha at load time.
fn copy_shadow(
    data: &DataPlan,
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    let Some(source) = &data.shadow else {
        return Ok((0, 0));
    };
    let raw = fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    let bytes = raw.len();
    progress.begin("shadow", 1);
    writer
        .add(crate::shadow::KAGE_ENTRY, raw)
        .with_context(|| format!("failed to add {}", crate::shadow::KAGE_ENTRY))?;
    progress.advance(crate::shadow::KAGE_ENTRY);
    progress.end_phase();
    Ok((1, bytes))
}

/// Convert one UI asset to its pack bytes.
fn convert_ui_asset(kind: UiKind, raw: &[u8]) -> Result<Vec<u8>> {
    match kind {
        UiKind::Raw => Ok(raw.to_vec()),
        UiKind::Tim16 => {
            let image = tim::decode(raw)?;
            bmp::encode_to_vec(&image)
        }
        UiKind::Pix16 { width, height } => {
            let image = decode_pix_16(raw, width, height)?;
            bmp::encode_to_vec(&image)
        }
    }
}

/// Decode a headerless 16bpp RGB555 `.PIX` image.
fn decode_pix_16(raw: &[u8], width: u32, height: u32) -> Result<Image> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .context("PIX dimensions overflow")?;
    let need = pixels.checked_mul(2).context("PIX dimensions overflow")?;
    if raw.len() < need {
        bail!(
            "PIX is {} bytes but {width}x{height} needs {need}",
            raw.len()
        );
    }
    let mut rgba = Vec::with_capacity(pixels * 4);
    for pixel in raw[..need].as_chunks::<2>().0 {
        rgba.extend_from_slice(&expand_rgb555(u16::from_le_bytes([pixel[0], pixel[1]])));
    }
    Ok(Image {
        width,
        height,
        rgba,
    })
}

/// Expand one RGB555 word to opaque RGBA, the shared PIX/TIM channel order.
fn expand_rgb555(value: u16) -> [u8; 4] {
    let r = value & 31;
    let g = (value >> 5) & 31;
    let b = (value >> 10) & 31;
    [
        (r * 255 / 31) as u8,
        (g * 255 / 31) as u8,
        (b * 255 / 31) as u8,
        255,
    ]
}

/// Bake a 40-pixel-wide icon sheet into one vertical BMP atlas.
///
/// `pix` is `rows` rows of 40x30 8bpp indices; the palette is `STATUS.TIM`'s
/// second CLUT row. Index 0 is forced to black so the runtime's mask decode
/// treats it as transparent.
fn bake_item_atlas(pix: &[u8], rows: usize, palette: &Texture8) -> Result<Vec<u8>> {
    let need = rows
        .checked_mul(items::ICON_ROW_BYTES)
        .context("item sheet is too large")?;
    if pix.len() < need {
        bail!(
            "item sheet is {} bytes but {rows} rows need {need}",
            pix.len()
        );
    }
    let width = items::ICON_WIDTH as usize;
    let height = rows * items::ICON_HEIGHT as usize;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for &index in &pix[..need] {
        let color = if index == 0 {
            [0, 0, 0, 255]
        } else {
            palette.palette(2, index)
        };
        rgba.extend_from_slice(&color);
    }
    bmp::encode_to_vec(&Image {
        width: items::ICON_WIDTH,
        height: (rows * items::ICON_HEIGHT as usize) as u32,
        rgba,
    })
}

/// Pack entry of the save prefix.
const BIO_CARD_ENTRY: &str = "data/bio_card.dat";
/// The attract-demo reels (raw), packed under `demo/<hex>.dat`.
const DEMO_FILES: [&str; 4] = ["PDEMO0.DAT", "PDEMO1.DAT", "PDEMO2.DAT", "PDEMO3.DAT"];
/// Shipped name of the save prefix.
const BIO_CARD_FILE: &str = "BIO_CARD.DAT";
/// Shipped name of the item-icon palette TIM.
const STATUS_FILE: &str = "STATUS.TIM";
/// Shipped name of the player-shadow coverage TIM.
const KAGE_FILE: &str = "KAGE.TIM";

/// How one `DATA` UI asset becomes its pack bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiKind {
    /// Copy the file raw; the renderer decodes the TIM itself.
    Raw,
    /// Decode a 16bpp direct-color TIM and encode it as BMP.
    Tim16,
    /// Decode a headerless 16bpp RGB555 PIX and encode it as BMP.
    Pix16 { width: u32, height: u32 },
}

/// One resolved UI asset.
#[derive(Debug)]
struct UiAsset {
    /// Pack entry, e.g. `ui/status.tim`.
    entry: &'static str,
    /// Source file in the installation.
    source: PathBuf,
    /// How to convert it.
    kind: UiKind,
}

/// One resolved item atlas.
#[derive(Debug)]
struct ItemAsset {
    /// Pack entry, e.g. `item/item_all.bmp`.
    entry: &'static str,
    /// Source `.PIX` in the installation.
    source: PathBuf,
    /// Number of 40x30 rows in the sheet.
    rows: usize,
}

/// The optional `DATA` inputs, resolved before decoding.
#[derive(Debug, Default)]
struct DataPlan {
    /// Raw UI TIMs and converted backgrounds, one per section-6 entry.
    ui: Vec<UiAsset>,
    /// The three item atlases.
    items: Vec<ItemAsset>,
    /// `STATUS.TIM`, the item-atlas palette.
    palette: Option<PathBuf>,
    /// `BIO_CARD.DAT`.
    bio_card: Option<PathBuf>,
    /// `PDEMO0..3.DAT`, the attract-demo reels, in index order. Missing
    /// entries are skipped (a pack may ship fewer reels).
    demos: Vec<Option<PathBuf>>,
    /// `KAGE.TIM`, the player shadow's coverage page.
    shadow: Option<PathBuf>,
    /// `CORE00.ESP`, the global weapon-effect sprite metadata.
    core_esp: Option<PathBuf>,
    /// `CORE00.ETM`, the global weapon-effect art.
    core_etm: Option<PathBuf>,
    /// Non-fatal problems found while resolving these inputs.
    warnings: Vec<String>,
}

/// The UI art table: pack entry, shipped file name and conversion kind.
const UI_ASSETS: &[(&str, &str, UiKind)] = &[
    ("ui/status.tim", "STATUS.TIM", UiKind::Raw),
    ("ui/statface.tim", "STATFACE.TIM", UiKind::Raw),
    ("ui/blue.tim", "BLUE.TIM", UiKind::Raw),
    ("ui/staitem.tim", "STAITEM.TIM", UiKind::Raw),
    ("ui/itemboxn.tim", "ITEMBOXN.TIM", UiKind::Raw),
    ("ui/died.tim", "DIED.TIM", UiKind::Raw),
    (
        "ui/title.bmp",
        "TITLE.PIX",
        UiKind::Pix16 {
            width: 320,
            height: 240,
        },
    ),
    ("ui/t_press.tim", "T_PRESS.TIM", UiKind::Raw),
    ("ui/t_start.tim", "T_START.TIM", UiKind::Raw),
    ("ui/type00.bmp", "TYPE00.TIM", UiKind::Tim16),
    (
        "ui/sel_back.bmp",
        "SEL_BACK.PIX",
        UiKind::Pix16 {
            width: 320,
            height: 240,
        },
    ),
    ("ui/select_b.bmp", "SELECT_B.TIM", UiKind::Tim16),
    ("ui/select_k.tim", "SELECT_K.TIM", UiKind::Raw),
    ("ui/optkey03.bmp", "OPTKEY03.TIM", UiKind::Tim16),
    ("ui/opt11.bmp", "OPT11.TIM", UiKind::Tim16),
    ("ui/jopt06.bmp", "JOPT06.TIM", UiKind::Tim16),
    ("ui/side06.bmp", "SIDE06.TIM", UiKind::Tim16),
    ("ui/sidekey3.bmp", "SIDEKEY3.TIM", UiKind::Tim16),
    // The RESULT screen's baked plates: 4bpp TIMs the ending result and the
    // rocket-launcher epilogue sample by V cursor. Raw, so the renderer
    // decodes them itself.
    ("ui/en05.tim", "EN05.TIM", UiKind::Raw),
    ("ui/en07.tim", "EN07.TIM", UiKind::Raw),
    // The RESULT screen's character backdrops (first-run and cleared-once
    // stills) and the epilogue's rocket-launcher backdrop, all 320x240 PIX16.
    (
        "ui/chris02.bmp",
        "CHRIS02.PIX",
        UiKind::Pix16 {
            width: 320,
            height: 240,
        },
    ),
    (
        "ui/jill02.bmp",
        "JILL02.PIX",
        UiKind::Pix16 {
            width: 320,
            height: 240,
        },
    ),
    (
        "ui/clis01.bmp",
        "CLIS01.PIX",
        UiKind::Pix16 {
            width: 320,
            height: 240,
        },
    ),
    (
        "ui/jill01.bmp",
        "JILL01.PIX",
        UiKind::Pix16 {
            width: 320,
            height: 240,
        },
    ),
    (
        "ui/rc1121.bmp",
        "RC1121.PIX",
        UiKind::Pix16 {
            width: 320,
            height: 240,
        },
    ),
];

/// The global weapon-effect metadata file names.
const CORE_ESP_FILE: &str = "CORE00.ESP";
/// The global weapon-effect art file name.
const CORE_ETM_FILE: &str = "CORE00.ETM";

/// The item atlas table: pack entry, shipped file name and row count.
const ITEM_ASSETS: &[(&str, &str, usize)] = &[
    (items::ITEM_ALL_ENTRY, "ITEM_ALL.PIX", items::ITEM_ALL_ROWS),
    (items::ITEM_MIX_ENTRY, "ITEM_MIX.PIX", items::ITEM_MIX_ROWS),
    (items::MEDAL_ENTRY, "MEDAL.PIX", items::MEDAL_ROWS),
];

/// Resolve the optional `DATA` inputs and aggregate missing-file warnings.
fn resolve_data_assets(data: Option<&Path>) -> Result<DataPlan> {
    let Some(dir) = data else {
        return Ok(DataPlan {
            warnings: vec![format!(
                "no data directory found; {} UI art file(s), {} item atlas(es), {}, {} and {BIO_CARD_ENTRY} will be missing",
                UI_ASSETS.len(),
                ITEM_ASSETS.len(),
                crate::shadow::KAGE_ENTRY,
                crate::effects::room::CORE_ESP_ENTRY
            )],
            ..DataPlan::default()
        });
    };

    let index = index_dir(dir)?;
    let mut plan = DataPlan::default();

    let mut missing_ui = Vec::new();
    for (entry, file, kind) in UI_ASSETS {
        match index.get(&file.to_ascii_lowercase()) {
            Some(source) => plan.ui.push(UiAsset {
                entry,
                source: source.clone(),
                kind: *kind,
            }),
            None => missing_ui.push(*file),
        }
    }

    let mut missing_items = Vec::new();
    for (entry, file, rows) in ITEM_ASSETS {
        match index.get(&file.to_ascii_lowercase()) {
            Some(source) => plan.items.push(ItemAsset {
                entry,
                source: source.clone(),
                rows: *rows,
            }),
            None => missing_items.push(*file),
        }
    }
    match index.get(&STATUS_FILE.to_ascii_lowercase()) {
        Some(source) => plan.palette = Some(source.clone()),
        None => {
            if !plan.items.is_empty() {
                plan.items.clear();
                plan.warnings.push(format!(
                    "missing {STATUS_FILE}; the item atlases cannot be baked"
                ));
            }
        }
    }

    plan.bio_card = index.get(&BIO_CARD_FILE.to_ascii_lowercase()).cloned();
    plan.demos = DEMO_FILES
        .iter()
        .map(|file| index.get(&file.to_ascii_lowercase()).cloned())
        .collect();
    plan.shadow = index.get(&KAGE_FILE.to_ascii_lowercase()).cloned();
    plan.core_esp = index.get(&CORE_ESP_FILE.to_ascii_lowercase()).cloned();
    plan.core_etm = index.get(&CORE_ETM_FILE.to_ascii_lowercase()).cloned();

    if plan.core_esp.is_none() {
        plan.warnings.push(format!(
            "missing {CORE_ESP_FILE}; no weapon effects will draw"
        ));
    }
    if plan.core_etm.is_none() {
        plan.warnings.push(format!(
            "missing {CORE_ETM_FILE}; weapon effect art will not decode"
        ));
    }
    if !missing_ui.is_empty() {
        plan.warnings.push(format!(
            "missing {} UI art file(s): {}",
            missing_ui.len(),
            missing_ui.join(", ")
        ));
    }
    if !missing_items.is_empty() {
        plan.warnings.push(format!(
            "missing {} item art file(s): {}",
            missing_items.len(),
            missing_items.join(", ")
        ));
    }
    if plan.bio_card.is_none() {
        plan.warnings.push(format!("missing {BIO_CARD_ENTRY}"));
    }
    if plan.shadow.is_none() {
        plan.warnings.push(format!(
            "missing {KAGE_FILE}; {} will be absent and no player shadow will draw",
            crate::shadow::KAGE_ENTRY
        ));
    }
    Ok(plan)
}

/// Shipped item-view model and document-art counts.
const IVM_COUNT: usize = 77;
const FILEI_COUNT: usize = 17;
const TEXTM_COUNT: usize = 43;

/// Add the checked-in `language/<tag>.txt` sources to the pack.
///
/// Every source is parsed before it is packed, so a malformed checked-in file
/// fails the conversion instead of shipping.
fn copy_language_files(writer: &mut PackWriter, progress: &mut Progress) -> Result<(usize, usize)> {
    if !crate::language::LANGUAGE_FILES
        .iter()
        .any(|(entry, _)| entry.eq_ignore_ascii_case("language/en-US.txt"))
    {
        bail!("the checked-in language files must include en-US.txt, the fallback language");
    }
    progress.begin("language", crate::language::LANGUAGE_FILES.len() as u64);
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (entry, source) in crate::language::LANGUAGE_FILES {
        crate::language::Language::parse(source)
            .with_context(|| format!("invalid language file {entry}"))?;
        bytes += source.len();
        count += 1;
        writer
            .add(entry, source.as_bytes().to_vec())
            .with_context(|| format!("failed to add {entry}"))?;
        progress.advance(entry);
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Add every `ITEM_M2/*.IVM` item-view model to the pack under `item/`.
///
/// The pack stores the canonical lower-case file name; the install's names are
/// matched case-insensitively. Every language tree is searched, the conversion
/// tree first, because a Steam root keeps one `ITEM_M2` per tree and the
/// Japanese tree also carries the `FILEI`/`TEXTM` document pages.
fn copy_item_models(
    item_m2: &[PathBuf],
    writer: &mut PackWriter,
    progress: &mut Progress,
    warnings: &mut Vec<String>,
    jobs: usize,
) -> Result<(usize, usize)> {
    if item_m2.is_empty() {
        warnings.push("no ITEM_M2 directory found; item models will be missing".to_string());
        return Ok((0, 0));
    }

    let index = index_dirs(item_m2, warnings)?;
    let mut files: Vec<(String, PathBuf)> = index
        .into_iter()
        .filter(|(name, _)| name.ends_with(".ivm"))
        .map(|(name, path)| (format!("item/{name}"), path))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    if files.len() != IVM_COUNT {
        warnings.push(format!(
            "found {} item model(s) in {}, expected {IVM_COUNT}",
            files.len(),
            searched_dirs(item_m2)
        ));
    }

    copy_raw_files(files, "item", writer, progress, jobs)
}

/// Add the document-reader art to the pack under `file/`: the `FILE` covers,
/// the `FILEI` backdrops and every `TEXTM_*.TIM` page, copied raw with
/// lower-case names.
fn copy_file_art(
    item_m2: &[PathBuf],
    writer: &mut PackWriter,
    progress: &mut Progress,
    warnings: &mut Vec<String>,
    jobs: usize,
) -> Result<(usize, usize)> {
    if item_m2.is_empty() {
        warnings.push("no ITEM_M2 directory found; file art will be missing".to_string());
        return Ok((0, 0));
    }

    let index = index_dirs(item_m2, warnings)?;
    let mut missing = Vec::new();
    let mut files = Vec::new();
    for name in ["file000.tim", "file001.tim"] {
        match index.get(name) {
            Some(path) => files.push((format!("file/{name}"), path.clone())),
            None => missing.push(name.to_string()),
        }
    }
    for number in 1..=FILEI_COUNT {
        let name = format!("filei{number:02}.tim");
        match index.get(&name) {
            Some(path) => files.push((format!("file/{name}"), path.clone())),
            None => missing.push(name),
        }
    }
    if !missing.is_empty() {
        warnings.push(format!(
            "missing {} document art file(s) in {}: {}",
            missing.len(),
            searched_dirs(item_m2),
            missing.join(", ")
        ));
    }

    let mut pages: Vec<&String> = index
        .keys()
        .filter(|name| name.starts_with("textm_") && name.ends_with(".tim"))
        .collect();
    pages.sort();
    if pages.len() != TEXTM_COUNT {
        warnings.push(format!(
            "found {} TEXTM page(s) in {}, expected {TEXTM_COUNT}",
            pages.len(),
            searched_dirs(item_m2)
        ));
    }
    for name in pages {
        files.push((format!("file/{name}"), index[name].clone()));
    }

    // The Western reader's document containers, one set per language tree,
    // and its page arrows. A Japanese install has neither, so their absence
    // is only reported when a Western tree is present but incomplete.
    let mut documents = Vec::new();
    for (language, extension) in DOCUMENT_LANGUAGES {
        let mut missing = Vec::new();
        let mut found = Vec::new();
        for stem in FILEM_STEMS {
            match index.get(&format!("{stem}.{extension}")) {
                Some(path) => found.push((language, stem, path.clone())),
                None => missing.push(format!("{stem}.{extension}")),
            }
        }
        if !found.is_empty() && !missing.is_empty() {
            warnings.push(format!(
                "missing {} document page file(s) in {}: {}",
                missing.len(),
                searched_dirs(item_m2),
                missing.join(", ")
            ));
        }
        documents.extend(found);
    }
    match index.get("arror.tim") {
        Some(path) => files.push(("file/arror.tim".to_string(), path.clone())),
        None if !documents.is_empty() => warnings.push(format!(
            "missing arror.tim in {}; the document reader draws no page arrows",
            searched_dirs(item_m2)
        )),
        None => {}
    }

    let (count, bytes) = copy_raw_files(files, "file", writer, progress, jobs)?;
    let (page_count, page_bytes) =
        copy_document_pages(documents, writer, progress, warnings, jobs)?;
    Ok((count + page_count, bytes + page_bytes))
}

/// The Western reader's per-document containers in document order
/// (`0x004d2350`): `filem_l0` is document 0 through `filem_00`, document 15.
const FILEM_STEMS: [&str; 16] = [
    "filem_l0", "filem_m0", "filem_n0", "filem_o0", "filem_p0", "filem_q0", "filem_r0", "filem_s0",
    "filem_t0", "filem_u0", "filem_v0", "filem_w0", "filem_x0", "filem_y0", "filem_z0", "filem_00",
];

/// The reader's page count of each document in [`FILEM_STEMS`] order
/// (`g_itemMaxCounts`); every shipped tree matches it.
const FILEM_PAGE_COUNTS: [usize; 16] = [4, 6, 11, 4, 2, 6, 7, 7, 10, 5, 5, 4, 3, 3, 3, 4];

/// The Western trees' document containers: the pack's language directory
/// and the container extension each release uses.
const DOCUMENT_LANGUAGES: [(&str, &str); 3] = [("en", "pix"), ("de", "gpx"), ("fr", "fpx")];

/// Unpack every `filem_*` container into its pages, packed as
/// `file/<language>/<stem>_<page>.tim` (two-digit page numbers). The pages
/// are optional art, so a container that fails to unpack is reported and
/// skipped, as is a page count the reader's table does not expect.
fn copy_document_pages(
    documents: Vec<(&str, &str, PathBuf)>,
    writer: &mut PackWriter,
    progress: &mut Progress,
    warnings: &mut Vec<String>,
    jobs: usize,
) -> Result<(usize, usize)> {
    progress.begin("documents", documents.len() as u64);
    let results = parallel_map(documents.len(), jobs, progress, |index| {
        let (language, stem, source) = &documents[index];
        let pages = fs::read(source)
            .with_context(|| format!("failed to read {}", source.display()))
            .and_then(|data| unpack_document_pages(&data))
            .map_err(|error| format!("skipped {}: {error:#}", source.display()));
        Ok((format!("file/{language}/{stem}"), 1, (*stem, source, pages)))
    })?;
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (prefix, (stem, source, pages)) in results {
        let pages = match pages {
            Ok(pages) => pages,
            Err(warning) => {
                warnings.push(warning);
                continue;
            }
        };
        let expected = FILEM_STEMS
            .iter()
            .position(|known| *known == stem)
            .map(|document| FILEM_PAGE_COUNTS[document]);
        if expected.is_some_and(|expected| expected != pages.len()) {
            warnings.push(format!(
                "{} holds {} page(s), the reader expects {}",
                source.display(),
                pages.len(),
                expected.unwrap_or_default()
            ));
        }
        for (number, page) in pages.into_iter().enumerate() {
            let entry = format!("{prefix}_{number:02}.tim");
            bytes += page.len();
            count += 1;
            writer
                .add(&entry, page)
                .with_context(|| format!("failed to add {entry}"))?;
        }
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Split a `filem_*.pix` container into its LZW-decoded TIM pages.
///
/// The container opens with a table of little-endian `u32` offsets, one per
/// page plus the end of the data; the first offset is the table's own size
/// and unused slots are zero. Each page is one `.pak` LZW stream
/// (`pickup_unpack_list`, 0x004827c0), keyed by [`key_document_page`].
fn unpack_document_pages(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let word = |index: usize| -> Option<usize> {
        let bytes = data.get(index * 4..index * 4 + 4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?) as usize)
    };
    let table = word(0).context("truncated page table")?;
    if table < 8 || table % 4 != 0 || table > data.len() {
        bail!("invalid page table size {table}");
    }
    let offsets: Vec<usize> = (0..table / 4)
        .map_while(|index| word(index).filter(|&offset| offset != 0))
        .collect();
    let mut pages = Vec::with_capacity(offsets.len().saturating_sub(1));
    for (number, pair) in offsets.windows(2).enumerate() {
        let (start, end) = (pair[0], pair[1]);
        let stream = data
            .get(start..end)
            .with_context(|| format!("page {number} spans {start}..{end} of {}", data.len()))?;
        let mut page =
            lzw::decode(stream).with_context(|| format!("failed to LZW-decode page {number}"))?;
        tim::mode(&page).with_context(|| format!("page {number} is not a TIM"))?;
        key_document_page(&mut page);
        pages.push(page);
    }
    if pages.is_empty() {
        bail!("the container holds no pages");
    }
    Ok(pages)
}

/// Move a document page's transparency key to index 0, where the renderer
/// keys every page. The pages put their white text at index 0 and the black
/// background at a later index (7 in every shipped page); the PC loader
/// keys the black entries of such a page instead (`TextureLoader.cpp`).
/// Swapping the two entries and their texels keeps every texel's colour, so
/// the page's HD hashes are unchanged. A page that is not a single-CLUT
/// 4bpp TIM, or already keys index 0, is left alone.
fn key_document_page(page: &mut [u8]) {
    const CLUT_AT: usize = 8;
    const ENTRIES_AT: usize = CLUT_AT + 12;
    if tim::decode_4bpp(page).is_err() {
        return;
    }
    let half = |page: &[u8], at: usize| u16::from_le_bytes([page[at], page[at + 1]]);
    let clut_len = u32::from_le_bytes(page[CLUT_AT..CLUT_AT + 4].try_into().unwrap()) as usize;
    let (width, height) = (half(page, CLUT_AT + 8), half(page, CLUT_AT + 10));
    if height != 1 || width < 16 {
        return;
    }
    let entry = |page: &[u8], index: usize| half(page, ENTRIES_AT + index * 2);
    if entry(page, 0) & 0x7FFF == 0 {
        return;
    }
    let Some(key) = (1..16).find(|&index| entry(page, index) & 0x7FFF == 0) else {
        return;
    };
    let (zero, black) = (entry(page, 0), entry(page, key));
    page[ENTRIES_AT..ENTRIES_AT + 2].copy_from_slice(&black.to_le_bytes());
    page[ENTRIES_AT + key * 2..ENTRIES_AT + key * 2 + 2].copy_from_slice(&zero.to_le_bytes());
    let key = key as u8;
    let swap = |nibble: u8| match nibble {
        0 => key,
        n if n == key => 0,
        n => n,
    };
    // `decode_4bpp` accepted the image block: its pixels follow a 12-byte
    // header inside the block's length.
    let image_at = CLUT_AT + clut_len;
    let image_len = u32::from_le_bytes(page[image_at..image_at + 4].try_into().unwrap()) as usize;
    let end = (image_at + image_len).min(page.len());
    for byte in page.get_mut(image_at + 12..end).unwrap_or_default() {
        *byte = swap(*byte & 0xF) | swap(*byte >> 4) << 4;
    }
}

/// Add the map floor-plan TIMs to the pack under `map/`: the fifteen
/// `MAP00`-`MAP0E` browser/display plans plus the `Map_blue` backdrop. The
/// engine decodes them itself, so no conversion is needed.
fn copy_map_art(
    item_m2: &[PathBuf],
    writer: &mut PackWriter,
    progress: &mut Progress,
    warnings: &mut Vec<String>,
    jobs: usize,
) -> Result<(usize, usize)> {
    if item_m2.is_empty() {
        warnings.push("no ITEM_M2 directory found; the map art will be missing".to_string());
        return Ok((0, 0));
    }

    let index = index_dirs(item_m2, warnings)?;
    let mut missing = Vec::new();
    let mut files = Vec::new();
    for number in 0..=0x0Eu32 {
        let name = format!("map{number:02x}.tim");
        match index.get(&name) {
            Some(path) => files.push((format!("map/{name}"), path.clone())),
            None => missing.push(name),
        }
    }
    match index.get("map_blue.tim") {
        Some(path) => files.push(("map/blue.tim".to_string(), path.clone())),
        None => missing.push("map_blue.tim".to_string()),
    }
    if !missing.is_empty() {
        warnings.push(format!(
            "missing {} map art file(s) in {}: {}; the map tab degrades to blank",
            missing.len(),
            searched_dirs(item_m2),
            missing.join(", ")
        ));
    }

    copy_raw_files(files, "map", writer, progress, jobs)
}

/// The id of the pack `convert-game` writes.
pub const BASE_PACK_ID: &str = "re1";
/// The base pack's self-description.
fn base_manifest(language: &str) -> manifest::Manifest {
    manifest::Manifest {
        format: manifest::FORMAT,
        id: BASE_PACK_ID.to_string(),
        name: Some(env!("CARGO_PKG_NAME").to_string()),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        kind: manifest::PackKind::Base,
        base: None,
        load_order: 0,
        rdt_version: manifest::DEFAULT_RDT_VERSION.to_string(),
        scd_version: manifest::DEFAULT_SCD_VERSION.to_string(),
        // The converted tree's own room text language (Japanese for the
        // Japanese releases, the Steam tree's language for the Western ones).
        language: Some(language.to_string()),
        engine: None,
        lua: Vec::new(),
    }
}

/// Add the base pack's rendered `manifest.toml`: its only self-description.
fn copy_manifest(writer: &mut PackWriter, language: &str) -> Result<(usize, usize)> {
    let text = base_manifest(language).render().into_bytes();
    let bytes = text.len();
    writer
        .add(manifest::ENTRY, text)
        .with_context(|| format!("failed to add {}", manifest::ENTRY))?;
    Ok((1, bytes))
}

/// Add the game-text sheets raw: the install's own as `font/font.tim` and the
/// other script's, when the install carries one, as `font/latin.tim` /
/// `font/jpn.tim`. Both are 4bpp TIMs the engine decodes, so no conversion is
/// needed.
fn copy_font(
    fonts: &Fonts,
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    let sheets = [
        ("font/font.tim", fonts.primary.as_deref()),
        ("font/latin.tim", fonts.latin.as_deref()),
        ("font/jpn.tim", fonts.japanese.as_deref()),
    ];
    let packed = sheets.iter().filter(|(_, path)| path.is_some()).count();
    if packed == 0 {
        return Ok((0, 0));
    }
    let mut count = 0;
    let mut bytes = 0;
    progress.begin("font", packed as u64);
    for (entry, path) in sheets {
        let Some(path) = path else {
            continue;
        };
        let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        bytes += data.len();
        count += 1;
        writer
            .add(entry, data)
            .with_context(|| format!("failed to add {entry}"))?;
        progress.advance(entry);
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Stage, sound, enemy-model, player, room-mask, door-art, data, item-view and
/// film directory roots discovered under the conversion root.
#[derive(Debug)]
struct Layout {
    stages: BTreeMap<u8, PathBuf>,
    sound: Option<PathBuf>,
    enemy: Option<PathBuf>,
    players: Option<PathBuf>,
    objspr: Option<PathBuf>,
    item_m1: Option<PathBuf>,
    data: Option<PathBuf>,
    item_m2: Option<PathBuf>,
    /// Every `ITEM_M2` directory under the root, the conversion tree first.
    /// The Japanese tree carries the document-reader pages the Western trees
    /// omit, so the item and file phases merge all of them.
    item_m2_dirs: Vec<PathBuf>,
    effspr: Option<PathBuf>,
    voice: Option<PathBuf>,
    movie: Option<PathBuf>,
    /// Every `movie` directory under the root, the conversion tree first.
    /// The Western and Japanese trees name the same films differently, so
    /// each table row is resolved across all of them.
    movie_dirs: Vec<PathBuf>,
    /// Every game-text sheet found under the root, in walk order: the first
    /// `font.tim` (the Japanese sheet) and the first `fontus.tim` (the
    /// Western one) of any install tree, so a Steam install converted from
    /// one tree can carry the other tree's sheet too.
    fonts: Vec<FontSheet>,
}

/// One `font.tim`/`fontus.tim` discovered under the conversion root.
#[derive(Debug, Clone)]
struct FontSheet {
    /// The sheet's file path.
    path: PathBuf,
    /// The script its width selects.
    script: crate::charset::Script,
}

/// Breadth-first, case-insensitive discovery of `STAGE1`..`STAGE7`, `sound`,
/// `enemy`, `players`, `objspr`, `ITEM_M1`, `ITEM_M2`, `data`, `effspr`,
/// `voice` and `movie`.
fn discover_layout(root: &Path) -> Result<Layout> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut stages: BTreeMap<u8, PathBuf> = BTreeMap::new();
    let mut sound = None;
    let mut enemy = None;
    let mut players = None;
    let mut objspr = None;
    let mut item_m1 = None;
    let mut data = None;
    let mut item_m2_dirs: Vec<PathBuf> = Vec::new();
    let mut effspr = None;
    let mut voice = None;
    let mut movie_dirs: Vec<PathBuf> = Vec::new();
    let mut fonts: Vec<FontSheet> = Vec::new();

    while let Some((dir, depth)) = queue.pop_front() {
        if let Some(name) = dir.file_name().and_then(|name| name.to_str()) {
            if let Some(digit) = stage_dir_digit(name) {
                stages.entry(digit).or_insert_with(|| dir.clone());
            } else if sound.is_none() && name.eq_ignore_ascii_case("sound") {
                sound = Some(dir.clone());
            } else if enemy.is_none() && name.eq_ignore_ascii_case("enemy") {
                enemy = Some(dir.clone());
            } else if players.is_none() && name.eq_ignore_ascii_case("players") {
                players = Some(dir.clone());
            } else if objspr.is_none() && name.eq_ignore_ascii_case("objspr") {
                objspr = Some(dir.clone());
            } else if item_m1.is_none() && name.eq_ignore_ascii_case("item_m1") {
                item_m1 = Some(dir.clone());
            } else if data.is_none() && name.eq_ignore_ascii_case("data") {
                data = Some(dir.clone());
            } else if name.eq_ignore_ascii_case("item_m2") && !item_m2_dirs.contains(&dir) {
                item_m2_dirs.push(dir.clone());
            } else if effspr.is_none() && name.eq_ignore_ascii_case("effspr") {
                effspr = Some(dir.clone());
            } else if voice.is_none() && name.eq_ignore_ascii_case("voice") {
                voice = Some(dir.clone());
            } else if name.eq_ignore_ascii_case("movie") && !movie_dirs.contains(&dir) {
                movie_dirs.push(dir.clone());
            }
            if name.eq_ignore_ascii_case("data") {
                collect_fonts(&dir, &mut fonts);
            }
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        for entry in read_dir_sorted(&dir)? {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }

    let missing: Vec<String> = (1..=RoomId::MAX_STAGE)
        .filter(|digit| !stages.contains_key(digit))
        .map(|digit| format!("STAGE{digit}"))
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing stage director{} within {MAX_DEPTH} levels of {}: {}",
            if missing.len() == 1 { "y" } else { "ies" },
            root.display(),
            missing.join(", ")
        );
    }

    Ok(Layout {
        stages,
        sound,
        enemy,
        players,
        objspr,
        item_m1,
        data,
        item_m2: item_m2_dirs.first().cloned(),
        item_m2_dirs,
        effspr,
        voice,
        movie: movie_dirs.first().cloned(),
        movie_dirs,
        fonts,
    })
}

/// The game-text sheets a conversion packs.
///
/// The install's own sheet is the pack's default (`font/font.tim`); a sheet
/// of the other script — found under the conversion root or named on the
/// command line — is packed as `font/<script>.tim`, so one pack can render
/// both the Japanese and the Western text.
#[derive(Debug, Default, Clone)]
struct Fonts {
    /// The install's own sheet, packed as `font/font.tim`.
    primary: Option<PathBuf>,
    /// The Western sheet (`fontus.tim`), packed as `font/latin.tim`.
    latin: Option<PathBuf>,
    /// The Japanese sheet (`FONT.TIM`), packed as `font/jpn.tim`.
    japanese: Option<PathBuf>,
}

/// Resolve the sheets to pack: the chosen data directory's own font is the
/// default, every other sheet the root carries becomes its script's override.
///
/// A sheet the root does not carry is a warning, not an error: a Japanese
/// install alone renders Latin languages with the Japanese sheet's tight
/// metrics, and a Steam tree alone renders Japanese with the Western sheet.
fn resolve_fonts(data: Option<&Path>, found: &[FontSheet]) -> (Fonts, Vec<String>) {
    let primary = data
        .and_then(|dir| found.iter().find(|sheet| sheet.path.parent() == Some(dir)))
        .or_else(|| found.first())
        .map(|sheet| sheet.path.clone());
    let mut fonts = Fonts {
        primary: primary.clone(),
        latin: None,
        japanese: None,
    };
    for sheet in found {
        if Some(&sheet.path) == primary.as_ref() {
            continue;
        }
        match sheet.script {
            crate::charset::Script::Latin if fonts.latin.is_none() => {
                fonts.latin = Some(sheet.path.clone());
            }
            crate::charset::Script::Japanese if fonts.japanese.is_none() => {
                fonts.japanese = Some(sheet.path.clone());
            }
            _ => {}
        }
    }
    // Only diagnose a missing sheet when the install actually has a DATA
    // directory; a missing DATA root is not reported so partial trees stay
    // warning-free. The default sheet counts as the script it carries, so a
    // Western default with no overrides is complete for Latin.
    let mut warnings = Vec::new();
    match (data, &fonts.primary) {
        (Some(dir), None) => warnings.push(format!(
            "missing DATA/FONT.TIM (or fontus.tim) in {}",
            dir.display()
        )),
        (_, Some(primary)) => {
            let primary = sheet_script_of(primary);
            if primary != crate::charset::Script::Latin && fonts.latin.is_none() {
                warnings.push(
                    "no Western font sheet (Data/fontus.tim) found; Latin languages draw with \
                     the Japanese sheet's tight metrics"
                        .to_string(),
                );
            }
            if primary != crate::charset::Script::Japanese && fonts.japanese.is_none() {
                warnings.push(
                    "no Japanese font sheet (DATA/FONT.TIM) found; Japanese text draws with \
                     the Western sheet"
                        .to_string(),
                );
            }
        }
        _ => {}
    }
    (fonts, warnings)
}

/// The pack's own room-text language: the tree the default sheet came from,
/// as the releases name it (`.../english/USA/Data/fontus.tim` is `en-US`).
fn install_language(fonts: &Fonts) -> String {
    let Some(primary) = fonts.primary.as_deref() else {
        return "ja-JP".to_string();
    };
    if sheet_script_of(primary) == crate::charset::Script::Japanese {
        return "ja-JP".to_string();
    }
    for part in primary.components().rev().skip(1).take(4) {
        let name = part.as_os_str().to_string_lossy();
        let tag = match name.to_ascii_lowercase().as_str() {
            "english" | "usa" => "en-US",
            "french" | "fra" => "fr-FR",
            "german" | "ger" => "de-DE",
            "italian" | "ita" => "it-IT",
            "spanish" | "spa" | "esp" => "es-ES",
            _ => continue,
        };
        return tag.to_string();
    }
    "en-US".to_string()
}

/// Collect the first `font.tim` (Japanese) and `fontus.tim` (Western) in
/// `dir`, naming each sheet's script by its width.
fn collect_fonts(dir: &Path, fonts: &mut Vec<FontSheet>) {
    let Ok(index) = index_dir(dir) else {
        return;
    };
    for name in ["font.tim", "fontus.tim"] {
        let Some(path) = index.get(name) else {
            continue;
        };
        let script = sheet_script_of(path);
        if fonts.iter().any(|sheet| sheet.script == script) {
            continue;
        }
        fonts.push(FontSheet {
            path: path.clone(),
            script,
        });
    }
}

/// The script a sheet file's width selects, or the Japanese sheet's when the
/// file cannot be read as a 4bpp TIM.
fn sheet_script_of(path: &Path) -> crate::charset::Script {
    let width = fs::read(path)
        .ok()
        .and_then(|bytes| crate::tim::image_width(&bytes).ok());
    match width {
        Some(width) if width < crate::font::JPN_SHEET_WIDTH => crate::charset::Script::Latin,
        _ => crate::charset::Script::Japanese,
    }
}

/// `stage1`..`stage7` (case-insensitive) -> `Some(1..=7)`.
fn stage_dir_digit(name: &str) -> Option<u8> {
    if name.len() != 6 || !name.get(..5)?.eq_ignore_ascii_case("stage") {
        return None;
    }
    let digit = *name.as_bytes().get(5)?;
    (b'1'..=b'7').contains(&digit).then_some(digit - b'0')
}

/// One RDT file and its parsed identity.
#[derive(Debug)]
struct Rdt {
    id: RoomId,
    bytes: Vec<u8>,
}

/// One player asset resolved to its pack entry.
#[derive(Debug)]
struct PlayerAsset {
    /// Pack entry, e.g. `player/01.emd`.
    entry: String,
    /// Source file in the installation.
    source: PathBuf,
}

/// One weapon asset (`W*.EMW` / `WS*.TMD`) resolved to its pack entry.
#[derive(Debug)]
struct WeaponAsset {
    /// Pack entry, e.g. `player/w02.emw`.
    entry: String,
    /// Source file in the installation.
    source: PathBuf,
}

/// One scripted-character (NPC) model resolved to its pack entry.
#[derive(Debug)]
struct NpcAsset {
    /// Entity id (`0x20..=0x2E`).
    id: u8,
    /// Pack entry, e.g. `enemy/em23.emd`.
    entry: String,
    /// Source file in the installation.
    source: PathBuf,
}

/// One monster model resolved to its pack entry, one per scenario block.
#[derive(Debug)]
struct EnemyAsset {
    /// Entity id (`0x00..=0x15`).
    id: u8,
    /// Scenario block: 0 Chris, 1 Jill.
    player: u8,
    /// Pack entry, e.g. `enemy/em130.emd`.
    entry: String,
    /// Source file in the installation.
    source: PathBuf,
}

/// One voice WAV resolved to its pack entry.
#[derive(Debug)]
struct VoiceAsset {
    /// Pack entry, e.g. `voice/01_09.wav` for the first table-row record.
    entry: String,
    /// Source file in the installation.
    source: PathBuf,
}

/// One film AVI resolved to its pack entry.
#[derive(Debug)]
struct MovieAsset {
    /// Pack entry, e.g. `movie/pj.avi`.
    entry: String,
    /// Source file in the installation.
    source: PathBuf,
}

/// One effect-sheet TIM resolved to its pack entry.
#[derive(Debug)]
struct EffectSheet {
    /// Pack entry, e.g. `effspr/esp000.tim`.
    entry: String,
    /// Source file in the installation.
    source: PathBuf,
}

/// One room mask page resolved to its pack entry.
#[derive(Debug)]
struct RoomMask {
    /// Pack entry, e.g. `roommask/100_000.bmp`.
    entry: String,
    /// Source `objspr` pak in the installation.
    source: PathBuf,
}

/// One distinct room (`stage` + `room`, player variants merged).
#[derive(Debug)]
struct Room {
    /// Player-variant-independent identity used for entry names.
    id: RoomId,
    /// Every RDT found for the room, one per player variant.
    rdts: Vec<Rdt>,
    /// Highest camera count across the variants.
    cameras: usize,
    /// Resolved background paks for cameras `0..cameras`.
    paks: Vec<PathBuf>,
    /// Highest mask group count per camera across the variants.
    mask_groups: Vec<u8>,
}

/// Everything conversion needs, resolved and validated before any decoding.
#[derive(Debug)]
struct Plan {
    rooms: Vec<Room>,
    sound: Option<PathBuf>,
    item_m1: Option<PathBuf>,
    /// Every `ITEM_M2` directory, the conversion tree first; the item, file
    /// and map phases resolve each shipped name across all of them.
    item_m2_dirs: Vec<PathBuf>,
    players: Vec<PlayerAsset>,
    /// The `W*.EMW` weapon animations and `WS*.TMD` held-weapon models,
    /// resolved by shipped file number.
    weapons: Vec<WeaponAsset>,
    /// The fifteen scripted-character models, resolved by entity id.
    npc: Vec<NpcAsset>,
    /// The monster models of both scenario blocks, resolved by entity id.
    enemy_models: Vec<EnemyAsset>,
    roommask: Vec<RoomMask>,
    /// The 33 effect-sheet TIMs, resolved by shipped name.
    effects: Vec<EffectSheet>,
    /// Resolved `DATA` UI, item and save-prefix assets.
    data: DataPlan,
    /// The game-text sheets to pack: the install's own `font/font.tim` and,
    /// when the root carries them, the other script's sheet as
    /// `font/<script>.tim`.
    fonts: Fonts,
    /// The pack's own room-text language, from the tree the fonts and data
    /// came from (the manifest's `language`).
    language: String,
    /// The referenced voice WAVs (the union of the name rows).
    voice: Vec<VoiceAsset>,
    /// Shipped voice files no row references; listed in the summary.
    voice_unreferenced: Vec<String>,
    /// Whether the install has a `voice` directory at all.
    voice_dir_found: bool,
    /// The named film AVIs (table ids 0..=28), resolved case-insensitively.
    movie: Vec<MovieAsset>,
    /// Whether the install has a `movie` directory at all.
    movie_dir_found: bool,
    /// Non-fatal problems found while resolving optional inputs.
    warnings: Vec<String>,
}

/// Discover, enumerate, and validate every conversion input.
#[cfg(test)]
fn build_plan(root: &Path) -> Result<Plan> {
    build_plan_with_progress(root, 1, &mut Progress::new("convert-game", 0))
}

/// The number of phases [`convert_game_with_options`] will report, matching its
/// `begin` calls so the overall bar's denominator is exact.
///
/// The conditional phases each resolve to a single boolean here; the map and
/// combat table phases are counted unconditionally because their `begin` is
/// unconditional, and the voice and film phases likewise.
fn conversion_phase_count(
    sound: &Option<PathBuf>,
    item_m1: &Option<PathBuf>,
    item_m2: &Option<PathBuf>,
    fonts: &Fonts,
    data: &DataPlan,
) -> u64 {
    // scan, room, roomcut, roommask, six model phases (player, weapon, npc,
    // monster, enemy scripts, effect sheets), ui, core00, language, voice,
    // film, compress, write.
    let mut phases: u64 = 17;
    phases += 2 * u64::from(sound.is_some()); // bgm, se
    phases += u64::from(item_m1.is_some()); // door
    phases += u64::from(fonts.primary.is_some()); // font
    phases += u64::from(data.palette.is_some() && !data.items.is_empty()); // item
    phases += u64::from(data.bio_card.is_some()); // data
    phases += u64::from(data.demos.iter().any(Option::is_some)); // demo
    phases += u64::from(data.shadow.is_some()); // shadow
    phases += 3 * u64::from(item_m2.is_some()); // item, file, map
    phases
}

/// Discover, enumerate, and validate every conversion input, reporting the RDT
/// read/parse scan on `progress` and using `jobs` workers for it.
fn build_plan_with_progress(root: &Path, jobs: usize, progress: &mut Progress) -> Result<Plan> {
    let layout = discover_layout(root)?;
    let (npc, npc_warnings) = resolve_npc_assets(&layout)?;
    let (enemy_models, enemy_warnings) = resolve_enemy_assets(&layout)?;
    let Layout {
        stages,
        sound,
        enemy,
        players,
        objspr,
        item_m1,
        data,
        item_m2,
        item_m2_dirs,
        effspr,
        voice: voice_dir,
        movie: movie_dir,
        movie_dirs,
        fonts: found_fonts,
    } = layout;
    // The game-text sheets: the chosen data directory's own font is the
    // pack's default, and every other sheet the root carries (the Steam
    // install's other language trees) rides along as its script's override.
    let (fonts, font_warnings) = resolve_fonts(data.as_deref(), &found_fonts);
    let language = install_language(&fonts);
    let data = resolve_data_assets(data.as_deref())?;

    // Enumerate every RDT first so reading and parsing them can be one
    // reported parallel phase, then merge the rooms in file order.
    let mut rdt_files: Vec<(RoomId, PathBuf)> = Vec::new();
    for (digit, dir) in &stages {
        for entry in read_dir_sorted(dir)? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(parsed) = rdt_id_from_file_name(name) else {
                continue;
            };
            let id =
                parsed.with_context(|| format!("invalid RDT file name in {}", dir.display()))?;
            if id.stage != *digit {
                bail!(
                    "RDT `{name}` in {} has stage digit {}, expected {digit}",
                    dir.display(),
                    id.stage
                );
            }
            rdt_files.push((id, entry.path()));
        }
    }

    progress.set_phases(conversion_phase_count(
        &sound, &item_m1, &item_m2, &fonts, &data,
    ));
    progress.begin("scan", rdt_files.len() as u64);
    let parsed = parallel_map(rdt_files.len(), jobs, progress, |index| {
        let (id, path) = &rdt_files[index];
        let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let state = rdt::parse(&bytes, *id)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok((id.rdt_entry(), 1, (bytes, state)))
    })?;
    progress.end_phase();

    let mut rooms: BTreeMap<(u8, u8), Room> = BTreeMap::new();
    for ((id, _), (_label, (bytes, state))) in rdt_files.into_iter().zip(parsed) {
        let room = rooms.entry((id.stage, id.room)).or_insert_with(|| Room {
            id,
            rdts: Vec::new(),
            cameras: 0,
            paks: Vec::new(),
            mask_groups: Vec::new(),
        });
        room.cameras = room.cameras.max(state.cuts.len());
        room.mask_groups.resize(room.cameras, 0);
        for (camera, cut) in state.cuts.iter().enumerate() {
            room.mask_groups[camera] = room.mask_groups[camera].max(cut.mask_group_count);
        }
        room.rdts.push(Rdt { id, bytes });
    }

    let indices: BTreeMap<u8, HashMap<String, PathBuf>> = stages
        .iter()
        .map(|(digit, dir)| Ok((*digit, index_dir(dir)?)))
        .collect::<Result<_>>()?;

    let mut missing = Vec::new();
    for room in rooms.values_mut() {
        let fold = room.id.fold_stage_digit();
        let dir = &stages[&fold];
        let index = &indices[&fold];
        for camera in 0..room.cameras {
            let name = camera_pak_name(room.id, camera);
            match index.get(&name.to_ascii_lowercase()) {
                Some(path) => room.paks.push(path.clone()),
                None => missing.push(format!("{name} in {}", dir.display())),
            }
        }
    }
    if !missing.is_empty() {
        bail!(
            "missing {} camera background file(s): {}",
            missing.len(),
            missing.join(", ")
        );
    }

    // Mask pages are optional: a missing page only drops that camera's
    // foreground layer, so unresolved files are collected as warnings.
    let objspr_index = objspr.as_deref().map(index_dir).transpose()?;
    let mut roommask = Vec::new();
    let mut missing_masks = Vec::new();
    for room in rooms.values() {
        for camera in 0..room.cameras {
            if room.mask_groups.get(camera).copied().unwrap_or(0) == 0 {
                continue;
            }
            let name = mask_pak_name(room.id, camera);
            match objspr_index
                .as_ref()
                .and_then(|index| index.get(&name.to_ascii_lowercase()))
            {
                Some(path) => roommask.push(RoomMask {
                    entry: room.id.roommask_entry(camera),
                    source: path.clone(),
                }),
                None => missing_masks.push(name),
            }
        }
    }
    // Effect sheets are optional like the mask pages: unresolved files are
    // aggregated into one warning per category and never fail the conversion.
    let effspr_index = effspr.as_deref().map(index_dir).transpose()?;
    let mut effects = Vec::new();
    let mut missing_effects = Vec::new();
    for name in EFFECT_SHEET_FILES {
        let file = format!("{name}.tim");
        match effspr_index.as_ref().and_then(|index| index.get(&file)) {
            Some(source) => effects.push(EffectSheet {
                entry: format!("effspr/{file}"),
                source: source.clone(),
            }),
            None => missing_effects.push(file.to_ascii_uppercase()),
        }
    }

    let mut warnings = Vec::new();
    if !missing_effects.is_empty() {
        if effspr.is_none() {
            warnings.push(format!(
                "no effspr directory found; {} effect sheet(s) will be missing",
                missing_effects.len()
            ));
        } else {
            warnings.push(format!(
                "missing {} effect sheet(s): {}",
                missing_effects.len(),
                missing_effects.join(", ")
            ));
        }
    }
    if !missing_masks.is_empty() {
        if objspr.is_none() {
            warnings.push(format!(
                "no objspr directory found; {} room mask page(s) will be missing",
                missing_masks.len()
            ));
        } else {
            warnings.push(format!(
                "missing {} room mask page(s): {}",
                missing_masks.len(),
                missing_masks.join(", ")
            ));
        }
    }

    // Voice is a second optional pack: every table record is resolved to its
    // `voice/{row}_{id}.wav` entry (empty records are silent slots and get no
    // entry), missing source files are aggregated into one warning, and the
    // shipped-but-unreferenced files are listed in the summary. A missing
    // `voice` directory is only reported by the conversion summary, so a
    // partial tree stays warning-free like the other optional categories.
    let voice_index = voice_dir.as_deref().map(index_dir).transpose()?;
    let mut voice_assets = Vec::new();
    let mut voice_unreferenced = Vec::new();
    if let (Some(dir), Some(index)) = (voice_dir.as_deref(), voice_index.as_ref()) {
        let referenced: HashSet<String> = voice_table::referenced_names()
            .into_iter()
            .map(|name| format!("{}.wav", name.to_ascii_lowercase()))
            .collect();
        let mut missing_voice = Vec::new();
        for name in voice_table::referenced_names() {
            let file = format!("{}.wav", name.to_ascii_lowercase());
            if !index.contains_key(&file) {
                missing_voice.push(format!("{}.WAV", name.to_ascii_uppercase()));
            }
        }
        for (row, id, name) in voice_table::entries() {
            let file = format!("{}.wav", name.to_ascii_lowercase());
            if let Some(source) = index.get(&file) {
                voice_assets.push(VoiceAsset {
                    entry: voice::entry_path(row, id),
                    source: source.clone(),
                });
            }
        }
        if !missing_voice.is_empty() {
            warnings.push(format!(
                "missing {} voice file(s) in {}: {}",
                missing_voice.len(),
                dir.display(),
                missing_voice.join(", ")
            ));
        }
        for entry in read_dir_sorted(dir)? {
            let file = entry.file_name();
            let Some(file) = file.to_str() else {
                continue;
            };
            if file.to_ascii_lowercase().ends_with(".wav")
                && !referenced.contains(&file.to_ascii_lowercase())
            {
                voice_unreferenced.push(file.to_owned());
            }
        }
    }

    // Film is the second optional pack: each table id 0..=28 is resolved
    // case-insensitively (the null id 10 is skipped) and the missing names are
    // aggregated into one warning. A missing `movie` directory is only
    // reported by the conversion summary, like the other optional categories.
    //
    // The Steam install has one movie directory per language tree. The
    // Western trees name the opening, endings and staff rolls differently
    // from the Japanese table (`OU` for `OJ`, `STAF_R` for `STAF_B`, ...), so
    // every row is searched in the conversion tree first (under either
    // spelling) and then in the other trees. The Virgin logo is absent from
    // every digital PC release, so it is not a conversion defect.
    let mut movie_indexes: Vec<HashMap<String, PathBuf>> = Vec::new();
    for (position, dir) in movie_dirs.iter().enumerate() {
        match index_dir(dir) {
            Ok(index) => movie_indexes.push(index),
            Err(error) if position > 0 => {
                warnings.push(format!(
                    "failed to read {}: {error}; its films are ignored",
                    dir.display()
                ));
            }
            Err(error) => return Err(error),
        }
    }
    let mut movie_assets = Vec::new();
    if movie_dir.is_some() {
        let mut missing_movies = Vec::new();
        for id in 0..=28u8 {
            let Some(name) = movie::name(id) else {
                continue;
            };
            let source = movie_indexes.iter().find_map(|index| {
                movie::release_names(name)
                    .find_map(|candidate| index.get(&format!("{candidate}.avi")).cloned())
            });
            match source {
                Some(source) => movie_assets.push(MovieAsset {
                    entry: movie::pack_path(name),
                    source,
                }),
                None if !movie::is_shipped(name) => {}
                None => missing_movies.push(format!("{}.AVI", name.to_ascii_uppercase())),
            }
        }
        if !missing_movies.is_empty() {
            warnings.push(format!(
                "missing {} film file(s) in {}: {}",
                missing_movies.len(),
                searched_dirs(&movie_dirs),
                missing_movies.join(", ")
            ));
        }
    }

    warnings.extend(data.warnings.iter().cloned());
    warnings.extend(npc_warnings);
    warnings.extend(enemy_warnings);
    warnings.extend(font_warnings);
    Ok(Plan {
        rooms: rooms.into_values().collect(),
        sound,
        item_m1,
        item_m2_dirs,
        players: resolve_players(enemy.as_deref(), players.as_deref())?,
        weapons: resolve_weapon_assets(players.as_deref())?,
        npc,
        enemy_models,
        roommask,
        effects,
        data,
        fonts,
        language,
        voice: voice_assets,
        voice_unreferenced,
        voice_dir_found: voice_dir.is_some(),
        movie: movie_assets,
        movie_dir_found: movie_dir.is_some(),
        warnings,
    })
}

/// Resolve the fifteen scripted-character models (`ENEMY/EM1020.EMD` ..
/// `ENEMY/EM102E.EMD`) by entity id.
///
/// Unlike the player models, a missing NPC model is an optional-category
/// warning: the affected characters stay present but invisible instead of
/// failing the conversion, so the unresolved files are reported together.
fn resolve_npc_assets(layout: &Layout) -> Result<(Vec<NpcAsset>, Vec<String>)> {
    let index = layout.enemy.as_deref().map(index_dir).transpose()?;

    let mut assets = Vec::new();
    let mut missing = Vec::new();
    for id in enemy::FIRST_ID..=enemy::LAST_ID {
        let file = format!("em10{id:02x}.emd");
        match index.as_ref().and_then(|index| index.get(&file)) {
            Some(source) => assets.push(NpcAsset {
                id,
                entry: enemy::model_path(id)
                    .expect("every character id maps to a pack path")
                    .to_string(),
                source: source.clone(),
            }),
            None => missing.push(format!("ENEMY/EM10{id:02X}.EMD")),
        }
    }

    let mut warnings = Vec::new();
    if !missing.is_empty() {
        if layout.enemy.is_none() {
            warnings.push(format!(
                "no enemy directory found; {} NPC model(s) will be missing",
                missing.len()
            ));
        } else {
            warnings.push(format!(
                "missing {} NPC model file(s): {}",
                missing.len(),
                missing.join(", ")
            ));
        }
    }
    Ok((assets, warnings))
}

/// Resolve the monster models of both scenario blocks by entity id:
/// `ENEMY/EM10xx.EMD` (Chris) to `enemy/em{id:02x}0.emd` and
/// `ENEMY/EM11xx.EMD` (Jill) to `enemy/em{id:02x}1.emd`, for ids `0x00..=0x15`.
///
/// Missing files are an optional-category warning, like the character models:
/// an id without a model renders nothing while its script still runs.
fn resolve_enemy_assets(layout: &Layout) -> Result<(Vec<EnemyAsset>, Vec<String>)> {
    let index = layout.enemy.as_deref().map(index_dir).transpose()?;

    let mut assets = Vec::new();
    let mut missing = Vec::new();
    for id in 0x00u8..=0x15 {
        for player in 0..=1u8 {
            let block = 10 + player;
            let file = format!("em{block}{id:02x}.emd");
            match index.as_ref().and_then(|index| index.get(&file)) {
                Some(source) => assets.push(EnemyAsset {
                    id,
                    player,
                    entry: format!("enemy/em{id:02x}{player}.emd"),
                    source: source.clone(),
                }),
                None => missing.push(format!("ENEMY/EM{block}{id:02X}.EMD")),
            }
        }
    }

    let mut warnings = Vec::new();
    if !missing.is_empty() {
        if layout.enemy.is_none() {
            warnings.push(format!(
                "no enemy directory found; {} monster model(s) will be missing",
                missing.len()
            ));
        } else {
            warnings.push(format!(
                "missing {} monster model file(s): {}",
                missing.len(),
                missing.join(", ")
            ));
        }
    }
    Ok((assets, warnings))
}

/// Resolve the four player models (`ENEMY/Char10..13.EMD`) and the two
/// no-weapon locomotion clips (`PLAYERS/W00.EMW`, `W10.EMW`) by character
/// index. Missing files are reported together.
fn resolve_players(enemy: Option<&Path>, players: Option<&Path>) -> Result<Vec<PlayerAsset>> {
    let enemy_index = enemy.map(index_dir).transpose()?;
    let players_index = players.map(index_dir).transpose()?;

    let mut assets = Vec::new();
    let mut missing = Vec::new();
    for id in 0..4usize {
        let file = format!("char1{id}.emd");
        match enemy_index.as_ref().and_then(|index| index.get(&file)) {
            Some(path) => assets.push(PlayerAsset {
                entry: format!("player/{id:02}.emd"),
                source: path.clone(),
            }),
            None => missing.push(format!("ENEMY/Char1{id}.EMD")),
        }
    }
    // The four alternate-outfit models (`ENEMY/EM1030..EM1033.EMD`): the
    // ref's `LoadEntityEMD` picks `em1030`/`em1032` for Chris and
    // `em1031`/`em1033` for Jill by the costume variant byte. They are packed
    // as `player/{character:02}_alt{variant}.emd`; a missing file is skipped
    // and the player keeps the base model.
    for (file, character, variant) in [
        ("em1030.emd", 0, 0),
        ("em1031.emd", 1, 0),
        ("em1032.emd", 0, 1),
        ("em1033.emd", 1, 1),
    ] {
        if let Some(path) = enemy_index.as_ref().and_then(|index| index.get(file)) {
            assets.push(PlayerAsset {
                entry: costume_entry(character, variant),
                source: path.clone(),
            });
        }
    }
    for (id, file) in [(0usize, "w00.emw"), (1, "w10.emw")] {
        match players_index.as_ref().and_then(|index| index.get(file)) {
            Some(path) => assets.push(PlayerAsset {
                entry: format!("player/{id:02}.emw"),
                source: path.clone(),
            }),
            None => missing.push(format!("PLAYERS/{}", file.to_ascii_uppercase())),
        }
    }

    if !missing.is_empty() {
        bail!(
            "missing {} player asset(s): {}",
            missing.len(),
            missing.join(", ")
        );
    }
    Ok(assets)
}

/// The pack entry of `character`'s alternate-outfit model `variant` (the
/// costume variant byte's low bit).
pub fn costume_entry(character: u8, variant: u8) -> String {
    format!("player/{:02}_alt{}.emd", character & 1, variant & 1)
}

/// Resolve the shipped weapon files under `PLAYERS/`: every `W*.EMW` weapon
/// animation and every `WS*.TMD` held-weapon model.
///
/// The two no-weapon locomotion clips (`W00.EMW` for Chris, `W10.EMW` for
/// Jill) stay under their existing `player/00.emw`/`player/01.emw` entries;
/// every other `W` file is packed as `player/w{file:02x}.emw`, and the TMDs as
/// `player/ws{file:03x}.tmd`. A missing directory or file is a warning, not a
/// failure: the weapon runtime falls back to the no-weapon pose.
fn resolve_weapon_assets(players: Option<&Path>) -> Result<Vec<WeaponAsset>> {
    let Some(dir) = players else {
        return Ok(Vec::new());
    };
    let index = index_dir(dir)?;
    let mut assets = Vec::new();
    for name in index.keys() {
        let lower = name.to_ascii_lowercase();
        if let Some(stem) = lower.strip_prefix('w').and_then(|s| s.strip_suffix(".emw")) {
            if stem.len() != 2 || !stem.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let id = u8::from_str_radix(stem, 16)
                .with_context(|| format!("invalid weapon animation name {name}"))?;
            // The no-weapon locomotion pair is packed by `resolve_players`.
            if id == 0x00 || id == 0x10 {
                continue;
            }
            let source = index[name].clone();
            assets.push(WeaponAsset {
                entry: format!("player/w{id:02x}.emw"),
                source,
            });
        } else if let Some(stem) = lower
            .strip_prefix("ws")
            .and_then(|s| s.strip_suffix(".tmd"))
        {
            if stem.len() != 3 || !stem.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            let id = u16::from_str_radix(stem, 16)
                .with_context(|| format!("invalid held-weapon name {name}"))?;
            let source = index[name].clone();
            assets.push(WeaponAsset {
                entry: format!("player/ws{id:03x}.tmd"),
                source,
            });
        }
    }
    assets.sort_by(|a, b| a.entry.cmp(&b.entry));
    Ok(assets)
}

/// Parse `ROOM####.RDT` (case-insensitive) into its identity.
///
/// Returns `None` when the name is not an RDT name at all and an error when it
/// looks like one but its hex digits are invalid.
fn rdt_id_from_file_name(name: &str) -> Option<Result<RoomId>> {
    let lower = name.to_ascii_lowercase();
    let digits = lower.strip_prefix("room")?.strip_suffix(".rdt")?;
    if digits.len() != 4 {
        return Some(Err(anyhow!(
            "RDT file name `{name}` must be `ROOM` plus four hex digits plus `.RDT`"
        )));
    }
    Some(RoomId::parse(digits).map_err(|err| anyhow!("RDT file name `{name}`: {err}")))
}

/// Camera background file name for room `id`, e.g. `RC100A.pak`.
///
/// Stages 6 and 7 use the STAGE1/STAGE2 directory and file-name digit.
fn camera_pak_name(id: RoomId, camera: usize) -> String {
    format!("RC{}{:02X}{camera:X}.pak", id.fold_stage_digit(), id.room)
}

/// Room mask file name for room `id`, e.g. `OSP00000.pak`.
///
/// Stages 6 and 7 reuse the STAGE1/STAGE2 digit and the room number is printed
/// as two decimal digits, matching the shipped `objspr` names.
fn mask_pak_name(id: RoomId, camera: usize) -> String {
    format!(
        "OSP0{}{:02}{}.pak",
        id.fold_stage_digit() - 1,
        id.room,
        camera
    )
}

/// Case-insensitive index of the file names directly inside `dir`.
fn index_dir(dir: &Path) -> Result<HashMap<String, PathBuf>> {
    let mut index = HashMap::new();
    for entry in read_dir_sorted(dir)? {
        index.insert(
            entry.file_name().to_string_lossy().to_ascii_lowercase(),
            entry.path(),
        );
    }
    Ok(index)
}

/// Merge the files of `dirs`, the conversion tree first.
///
/// A name found in an earlier directory wins, so a localized file is
/// preferred, while a file only another language tree ships (the Steam
/// release's Japanese `ITEM_M2` pages) is still found. A fallback directory
/// that cannot be read is skipped with a warning; the conversion tree's own
/// failure still fails the run.
fn index_dirs(dirs: &[PathBuf], warnings: &mut Vec<String>) -> Result<HashMap<String, PathBuf>> {
    let mut index = HashMap::new();
    for (position, dir) in dirs.iter().enumerate() {
        let files = match index_dir(dir) {
            Ok(files) => files,
            Err(error) if position > 0 => {
                warnings.push(format!(
                    "failed to read {}: {error}; its files are ignored",
                    dir.display()
                ));
                continue;
            }
            Err(error) => return Err(error),
        };
        for (name, path) in files {
            index.entry(name).or_insert(path);
        }
    }
    Ok(index)
}

/// The directories a merged lookup searched, for a warning message. One
/// directory reads exactly as before; a Steam root lists every language tree.
fn searched_dirs(dirs: &[PathBuf]) -> String {
    dirs.iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Read a directory and sort its entries by file name.
fn read_dir_sorted(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("failed to open directory {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to list directory {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

/// The file name of `path` as a display string.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Read the bit depth from a BMP header.
fn bmp_bpp(data: &[u8]) -> Result<u16> {
    let raw = data
        .get(BMP_BPP_OFFSET..BMP_BPP_OFFSET + 2)
        .context("encoded BMP is missing its bit depth field")?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RDT_HEADER_LEN: usize = 0x94;
    const RDT_CAMERA_LEN: usize = 44;

    #[test]
    fn precompressed_extensions_are_recognised_case_insensitively() {
        for entry in [
            "movie/oj.avi",
            "movie/OJ.mp4",
            "assets/logo.PNG",
            "bgm/track.ogg",
            "ui/photo.webp",
        ] {
            assert!(is_precompressed(entry), "{entry}");
        }
        for entry in [
            "room/1000.rdt",
            "bgm/013.wav",
            "ui/title.bmp",
            "manifest.toml",
            "no-extension",
        ] {
            assert!(!is_precompressed(entry), "{entry}");
        }
    }

    /// Self-deleting temporary directory unique to this process and label.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("arklay-convert-{}-{label}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// A minimal, decodable 4bpp TIM whose image block declares `width_units`
    /// 16-bit units (four pixels each): 192 is the 768-wide Japanese sheet,
    /// 64 the 256-wide Western one.
    fn font_tim_bytes(width_units: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(0x10u32.to_le_bytes());
        out.extend(0x08u32.to_le_bytes());
        out.extend((12u32 + 16 * 2).to_le_bytes());
        out.extend([0u8; 4]);
        out.extend(16u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend([0u8; 16 * 2]);
        let width = width_units as usize * 4;
        out.extend(((12 + width / 2) as u32).to_le_bytes());
        out.extend([0u8; 4]);
        out.extend(width_units.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(vec![0u8; width / 2]);
        out
    }

    fn make_stage_dirs(root: &Path) {
        for digit in 1..=RoomId::MAX_STAGE {
            fs::create_dir_all(root.join(format!("STAGE{digit}"))).unwrap();
        }
        make_player_dirs(root);
    }

    /// Player models and no-weapon clips, with mixed-case names like the
    /// shipped install.
    fn make_player_dirs(root: &Path) {
        let enemy = root.join("ENEMY");
        let players = root.join("PLAYERS");
        fs::create_dir_all(&enemy).unwrap();
        fs::create_dir_all(&players).unwrap();
        fs::write(enemy.join("Char10.emd"), b"emd0").unwrap();
        fs::write(enemy.join("CHAR11.EMD"), b"emd1").unwrap();
        fs::write(enemy.join("Char12.EMD"), b"emd2").unwrap();
        fs::write(enemy.join("char13.emd"), b"emd3").unwrap();
        // Two of the four alternate outfits: the others are optional.
        fs::write(enemy.join("EM1030.EMD"), b"alt-chris").unwrap();
        fs::write(enemy.join("Em1031.emd"), b"alt-jill").unwrap();
        fs::write(players.join("W00.EMW"), b"emw0").unwrap();
        fs::write(players.join("w10.emw"), b"emw1").unwrap();
    }

    /// The fifteen scripted-character models, with names like the shipped
    /// install.
    fn write_npc_files(root: &Path) {
        let enemy = root.join("ENEMY");
        fs::create_dir_all(&enemy).unwrap();
        for id in enemy::FIRST_ID..=enemy::LAST_ID {
            fs::write(
                enemy.join(format!("EM10{id:02X}.EMD")),
                format!("npc-{id:02x}"),
            )
            .unwrap();
        }
    }

    /// Every `.dor` the door type table names, with mixed-case names like the
    /// shipped install.
    fn write_door_files(root: &Path) {
        let item_m1 = root.join("ITEM_M1");
        fs::create_dir_all(&item_m1).unwrap();
        for door_type in 0..=0x21u8 {
            let name = door::type_name(door_type);
            fs::write(
                item_m1.join(format!("{}.DOR", name.to_ascii_uppercase())),
                format!("dor-{name}").as_bytes(),
            )
            .unwrap();
        }
    }

    /// Every sound effect the room tables name, as mixed-case files like the
    /// shipped install.
    fn write_se_files(root: &Path) {
        let sound = root.join("sound");
        fs::create_dir_all(&sound).unwrap();
        for name in sfx::SE_NAMES.iter().copied().chain(music::se_track_names()) {
            fs::write(
                sound.join(format!("{}.WAV", name.to_ascii_uppercase())),
                name.as_bytes(),
            )
            .unwrap();
        }
    }

    /// A minimal RDT with `cameras` zeroed camera records.
    fn rdt_bytes(cameras: u8) -> Vec<u8> {
        let mut data = vec![0u8; RDT_HEADER_LEN + RDT_CAMERA_LEN * usize::from(cameras)];
        data[0x01] = cameras;
        data
    }

    fn write_code(out: &mut Vec<u8>, acc: &mut u32, bits: &mut u32, code: u16, width: u8) {
        *acc = (*acc << width) | u32::from(code);
        *bits += u32::from(width);
        while *bits >= 8 {
            *bits -= 8;
            out.push(((*acc >> *bits) & 0xFF) as u8);
        }
        *acc &= (1 << *bits) - 1;
    }

    /// LZW-encode `data` as 9-bit literals with periodic dictionary resets,
    /// followed by the end code.
    fn lzw_literals(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc = 0u32;
        let mut bits = 0u32;
        let mut since_reset = 0usize;
        for &byte in data {
            if since_reset >= 30_000 {
                write_code(&mut out, &mut acc, &mut bits, 0x102, 9);
                since_reset = 0;
            }
            write_code(&mut out, &mut acc, &mut bits, u16::from(byte), 9);
            since_reset += 1;
        }
        write_code(&mut out, &mut acc, &mut bits, 0x100, 9);
        if bits > 0 {
            out.push((acc << (8 - bits)) as u8);
        }
        out
    }

    /// A valid 16bpp TIM of the given size wrapped in a real LZW stream.
    fn camera_pak_at(width: u16, height: u16) -> Vec<u8> {
        let mut tim = Vec::new();
        tim.extend_from_slice(&0x10u32.to_le_bytes());
        tim.extend_from_slice(&2u32.to_le_bytes());
        tim.extend_from_slice(&0u32.to_le_bytes());
        tim.extend_from_slice(&0i16.to_le_bytes());
        tim.extend_from_slice(&0i16.to_le_bytes());
        tim.extend_from_slice(&width.to_le_bytes());
        tim.extend_from_slice(&height.to_le_bytes());
        for _ in 0..usize::from(width) * usize::from(height) {
            tim.extend_from_slice(&0x7FFFu16.to_le_bytes());
        }
        lzw_literals(&tim)
    }

    /// A valid 320x240 16bpp TIM wrapped in a real LZW stream.
    fn camera_pak() -> Vec<u8> {
        camera_pak_at(320, 240)
    }

    /// A valid 8bpp TIM of the given size, one 256-colour CLUT row, wrapped in
    /// a real LZW stream like the shipped `objspr` pages.
    fn mask_pak_at(width: u16, height: u16) -> Vec<u8> {
        let palette: Vec<u16> = (0..256u16)
            .map(|index| index.wrapping_mul(0x0841))
            .collect();
        let pixels: Vec<u8> = (0..usize::from(width) * usize::from(height))
            .map(|index| (index % 251) as u8)
            .collect();
        let mut tim = Vec::new();
        tim.extend_from_slice(&0x10u32.to_le_bytes());
        tim.extend_from_slice(&9u32.to_le_bytes());
        tim.extend_from_slice(&(12u32 + palette.len() as u32 * 2).to_le_bytes());
        tim.extend_from_slice(&0i16.to_le_bytes());
        tim.extend_from_slice(&480i16.to_le_bytes());
        tim.extend_from_slice(&256u16.to_le_bytes());
        tim.extend_from_slice(&1u16.to_le_bytes());
        for entry in &palette {
            tim.extend_from_slice(&entry.to_le_bytes());
        }
        tim.extend_from_slice(&(12u32 + pixels.len() as u32).to_le_bytes());
        tim.extend_from_slice(&0i16.to_le_bytes());
        tim.extend_from_slice(&0i16.to_le_bytes());
        tim.extend_from_slice(&(width / 2).to_le_bytes());
        tim.extend_from_slice(&height.to_le_bytes());
        tim.extend_from_slice(&pixels);
        lzw_literals(&tim)
    }

    /// A one-camera RDT whose camera 0 carries a one-group mask table with a
    /// single 8x16 sprite.
    fn rdt_with_masks() -> Vec<u8> {
        let mut data = rdt_bytes(1);
        let offset = data.len();
        data[0x94..0x98].copy_from_slice(&(offset as i32).to_le_bytes());
        data.extend_from_slice(&1i32.to_le_bytes());
        for word in [1u16, 0, 0, 0] {
            data.extend_from_slice(&word.to_le_bytes());
        }
        for word in [0u16, 0, 380, 0x0800, 8, 16] {
            data.extend_from_slice(&word.to_le_bytes());
        }
        data
    }

    #[test]
    fn discovers_stages_case_insensitively_and_breadth_first() {
        let root = TempDir::new("discover");
        for digit in 1..=RoomId::MAX_STAGE {
            fs::create_dir_all(root.path.join(format!("install/sTaGe{digit}"))).unwrap();
        }
        fs::create_dir_all(root.path.join("STAGE1")).unwrap();
        fs::create_dir_all(root.path.join("install/sound")).unwrap();
        fs::create_dir_all(root.path.join("install/EnEmY")).unwrap();
        fs::create_dir_all(root.path.join("pLaYeRs")).unwrap();
        fs::create_dir_all(root.path.join("install/ObJsPr")).unwrap();
        fs::create_dir_all(root.path.join("install/ItEm_M1")).unwrap();
        fs::create_dir_all(root.path.join("install/DaTa")).unwrap();
        fs::create_dir_all(root.path.join("install/EffSpr")).unwrap();

        let layout = discover_layout(&root.path).unwrap();

        assert_eq!(layout.stages[&1], root.path.join("STAGE1"));
        assert_eq!(layout.stages[&2], root.path.join("install/sTaGe2"));
        assert_eq!(layout.stages[&7], root.path.join("install/sTaGe7"));
        assert_eq!(layout.sound.unwrap(), root.path.join("install/sound"));
        assert_eq!(layout.enemy.unwrap(), root.path.join("install/EnEmY"));
        assert_eq!(layout.players.unwrap(), root.path.join("pLaYeRs"));
        assert_eq!(layout.objspr.unwrap(), root.path.join("install/ObJsPr"));
        assert_eq!(layout.item_m1.unwrap(), root.path.join("install/ItEm_M1"));
        assert_eq!(layout.data.unwrap(), root.path.join("install/DaTa"));
        assert_eq!(layout.effspr.unwrap(), root.path.join("install/EffSpr"));
    }

    #[test]
    fn missing_stages_are_reported_together() {
        let root = TempDir::new("missing-stages");
        fs::create_dir_all(root.path.join("JPN/STAGE1")).unwrap();

        let message = discover_layout(&root.path).unwrap_err().to_string();

        for digit in 2..=7 {
            assert!(message.contains(&format!("STAGE{digit}")), "{message}");
        }
        assert!(!message.contains("STAGE1"), "{message}");
        assert!(
            message.contains(&root.path.display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn missing_player_assets_are_aggregated() {
        let root = TempDir::new("missing-players");
        for digit in 1..=RoomId::MAX_STAGE {
            fs::create_dir_all(root.path.join(format!("STAGE{digit}"))).unwrap();
        }
        fs::create_dir_all(root.path.join("ENEMY")).unwrap();
        fs::write(root.path.join("ENEMY/Char10.emd"), b"emd0").unwrap();
        fs::create_dir_all(root.path.join("PLAYERS")).unwrap();
        fs::write(root.path.join("PLAYERS/W00.EMW"), b"emw0").unwrap();

        let message = build_plan(&root.path).unwrap_err().to_string();

        for name in ["Char11.EMD", "Char12.EMD", "Char13.EMD", "W10.EMW"] {
            assert!(message.contains(name), "{message}");
        }
        assert!(!message.contains("Char10"), "{message}");
        assert!(!message.contains("W00"), "{message}");
    }

    #[test]
    fn resolves_and_packs_npc_models_case_insensitively() {
        let root = TempDir::new("npc-models");
        make_stage_dirs(&root.path);
        write_npc_files(&root.path);
        // Ship a mixed-case name; the resolver must match it.
        fs::rename(
            root.path.join("ENEMY/EM1023.EMD"),
            root.path.join("ENEMY/Em1023.emd"),
        )
        .unwrap();

        let plan = build_plan(&root.path).unwrap();

        assert_eq!(plan.npc.len(), 15);
        for (offset, asset) in plan.npc.iter().enumerate() {
            let id = enemy::FIRST_ID + offset as u8;
            assert_eq!(asset.id, id);
            assert_eq!(asset.entry, format!("enemy/em{id:02x}.emd"));
        }

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(
            pack.paths()
                .filter(|path| path.starts_with("enemy/") && path.ends_with(".emd"))
                .count(),
            15
        );
        assert_eq!(
            count("enemy/em13.lua"),
            1,
            "the checked-in script is packed"
        );
        assert_eq!(pack.read("enemy/em23.emd").unwrap().as_ref(), b"npc-23");
    }

    /// Write all 33 effect-sheet files under `EFFSPR` with mixed-case names.
    fn write_effect_sheets(root: &Path) {
        let dir = root.join("EFFSPR");
        fs::create_dir_all(&dir).unwrap();
        for name in EFFECT_SHEET_FILES {
            fs::write(
                dir.join(format!("{}.TIM", name.to_ascii_uppercase())),
                name.as_bytes(),
            )
            .unwrap();
        }
    }

    #[test]
    fn resolves_and_packs_effect_sheets_case_insensitively() {
        let root = TempDir::new("effect-sheets");
        make_stage_dirs(&root.path);
        write_effect_sheets(&root.path);

        let plan = build_plan(&root.path).unwrap();

        assert_eq!(plan.effects.len(), EFFECT_SHEET_FILES.len());
        assert!(
            !plan.warnings.iter().any(|w| w.contains("effect sheet")),
            "{:?}",
            plan.warnings
        );
        for asset in &plan.effects {
            assert!(asset.entry.starts_with("effspr/"), "{}", asset.entry);
        }

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let (count, bytes) =
            copy_effect_sheets(&plan.effects, &mut writer, &mut progress, 1).unwrap();
        assert_eq!(count, 33);
        assert_eq!(
            bytes,
            EFFECT_SHEET_FILES.iter().map(|n| n.len()).sum::<usize>()
        );
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        for name in EFFECT_SHEET_FILES {
            assert!(
                pack.contains(&format!("effspr/{name}.tim")),
                "missing {name}"
            );
        }
        assert_eq!(pack.read("effspr/esp224.tim").unwrap().as_ref(), b"esp224");
    }

    #[test]
    fn missing_effect_sheets_warn_once_without_failing() {
        let root = TempDir::new("missing-effects");
        make_stage_dirs(&root.path);

        let plan = build_plan(&root.path).unwrap();
        assert!(plan.effects.is_empty());
        let warnings: Vec<&String> = plan
            .warnings
            .iter()
            .filter(|warning| warning.contains("effect sheet"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", plan.warnings);
        assert!(warnings[0].contains("33 effect sheet"), "{:?}", warnings);

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();
        assert!(!pack.paths().any(|path| path.starts_with("effspr/")));
    }

    #[test]
    fn conversion_aggregates_missing_effect_sheets() {
        let root = TempDir::new("partial-effects");
        make_stage_dirs(&root.path);
        write_effect_sheets(&root.path);
        fs::remove_file(root.path.join("EFFSPR/ESP212.TIM")).unwrap();
        fs::remove_file(root.path.join("EFFSPR/ESP224.TIM")).unwrap();

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(plan.effects.len(), 31);
        let warning = plan
            .warnings
            .iter()
            .find(|warning| warning.contains("effect sheet"))
            .expect("aggregated warning");
        assert!(warning.contains("2 effect sheet"), "{warning}");
        assert!(warning.contains("ESP212.TIM"), "{warning}");
        assert!(warning.contains("ESP224.TIM"), "{warning}");
    }

    #[test]
    fn converts_core00_effect_metadata() {
        let root = TempDir::new("core00");
        make_stage_dirs(&root.path);
        let data = root.path.join("DATA");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("CORE00.ESP"), b"esp-bytes").unwrap();
        fs::write(data.join("CORE00.ETM"), b"etm-bytes").unwrap();

        let data_plan = resolve_data_assets(Some(&data)).unwrap();
        assert!(data_plan.core_esp.is_some());
        assert!(data_plan.core_etm.is_some());

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let (count, bytes) = copy_core_effects(&data_plan, &mut writer, &mut progress, 1).unwrap();
        assert_eq!(count, 2);
        assert_eq!(bytes, "esp-bytes".len() + "etm-bytes".len());
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(
            pack.read(crate::effects::room::CORE_ESP_ENTRY)
                .unwrap()
                .as_ref(),
            b"esp-bytes"
        );
        assert_eq!(
            pack.read(crate::effects::room::CORE_ETM_ENTRY)
                .unwrap()
                .as_ref(),
            b"etm-bytes"
        );
    }

    #[test]
    fn missing_core00_files_warn_without_failing() {
        let root = TempDir::new("core00-missing");
        make_stage_dirs(&root.path);
        let data = root.path.join("DATA");
        fs::create_dir_all(&data).unwrap();

        let plan = resolve_data_assets(Some(&data)).unwrap();
        assert!(plan.core_esp.is_none());
        assert!(plan.core_etm.is_none());
        assert!(
            plan.warnings.iter().any(|w| w.contains("CORE00.ESP")),
            "{:?}",
            plan.warnings
        );

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let (count, _) = copy_core_effects(&plan, &mut writer, &mut progress, 1).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn missing_npc_models_warn_once_without_failing() {
        let root = TempDir::new("missing-npc");
        make_stage_dirs(&root.path);

        // No NPC files at all: one aggregated warning, and the conversion
        // still succeeds with no `enemy/` entries.
        let plan = build_plan(&root.path).unwrap();
        assert!(plan.npc.is_empty());
        let warnings: Vec<&String> = plan
            .warnings
            .iter()
            .filter(|warning| warning.contains("NPC model"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", plan.warnings);
        assert!(warnings[0].contains("15 NPC model"), "{:?}", warnings);
        assert!(warnings[0].contains("ENEMY/EM1020.EMD"), "{:?}", warnings);
        assert!(warnings[0].contains("ENEMY/EM102E.EMD"), "{:?}", warnings);

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();
        assert!(
            !pack
                .paths()
                .any(|path| path.starts_with("enemy/") && path.ends_with(".emd")),
            "no character model entries"
        );
        assert_eq!(
            pack.paths()
                .filter(|path| path.starts_with("enemy/") && path.ends_with(".lua"))
                .count(),
            crate::enemy::ENEMY_SCRIPTS.len(),
            "the checked-in enemy scripts are independent of the install"
        );

        // Present files resolve; only the missing two are named.
        write_npc_files(&root.path);
        fs::remove_file(root.path.join("ENEMY/EM1022.EMD")).unwrap();
        fs::remove_file(root.path.join("ENEMY/EM102E.EMD")).unwrap();
        let plan = build_plan(&root.path).unwrap();
        assert_eq!(plan.npc.len(), 13);
        let warnings: Vec<&String> = plan
            .warnings
            .iter()
            .filter(|warning| warning.contains("NPC model"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", plan.warnings);
        assert!(warnings[0].contains("ENEMY/EM1022.EMD"), "{:?}", warnings);
        assert!(warnings[0].contains("ENEMY/EM102E.EMD"), "{:?}", warnings);
        assert!(!warnings[0].contains("EM1020.EMD"), "{:?}", warnings);
    }

    #[test]
    fn parses_rdt_ids_from_file_names() {
        let id = rdt_id_from_file_name("ROOM1001.RDT").unwrap().unwrap();
        assert_eq!(id, RoomId::parse("1001").unwrap());

        let id = rdt_id_from_file_name("room11C0.rdt").unwrap().unwrap();
        assert_eq!(id.room3(), "11C");
        assert_eq!(id.player_flag, 0);

        assert!(rdt_id_from_file_name("background.bin").is_none());
        assert!(rdt_id_from_file_name("room.txt").is_none());
        assert!(rdt_id_from_file_name("room.rdt").unwrap().is_err());
        assert!(rdt_id_from_file_name("ROOM10001.RDT").unwrap().is_err());
        assert!(rdt_id_from_file_name("ROOM100Z.RDT").unwrap().is_err());
    }

    #[test]
    fn camera_pak_names_fold_return_stages() {
        assert_eq!(
            camera_pak_name(RoomId::parse("1001").unwrap(), 0xA),
            "RC100A.pak"
        );
        assert_eq!(
            camera_pak_name(RoomId::parse("6100").unwrap(), 3),
            "RC1103.pak"
        );
        assert_eq!(
            camera_pak_name(RoomId::parse("71C1").unwrap(), 0),
            "RC21C0.pak"
        );
    }

    #[test]
    fn enumerates_rdt_files_case_insensitively() {
        let root = TempDir::new("rdt-case");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/RoOm1000.RdT"), rdt_bytes(0)).unwrap();

        let plan = build_plan(&root.path).unwrap();

        assert_eq!(plan.rooms.len(), 1);
        assert_eq!(plan.rooms[0].id, RoomId::parse("1000").unwrap());
    }

    #[test]
    fn rdt_stage_digit_must_match_its_directory() {
        let root = TempDir::new("stage-mismatch");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE2/ROOM1000.RDT"), rdt_bytes(0)).unwrap();

        let message = build_plan(&root.path).unwrap_err().to_string();

        assert!(message.contains("ROOM1000.RDT"), "{message}");
        assert!(message.contains("expected 2"), "{message}");
    }

    #[test]
    fn stub_rdts_need_no_paks() {
        let root = TempDir::new("stub");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1100.RDT"), [0u8; 4]).unwrap();

        let plan = build_plan(&root.path).unwrap();

        assert_eq!(plan.rooms.len(), 1);
        assert_eq!(plan.rooms[0].cameras, 0);
        assert!(plan.rooms[0].paks.is_empty());
        assert_eq!(plan.rooms[0].rdts[0].bytes.len(), 4);
    }

    #[test]
    fn plan_merges_player_variants_and_unions_cameras() {
        let root = TempDir::new("variants");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_bytes(1)).unwrap();
        fs::write(root.path.join("STAGE1/ROOM1001.RDT"), rdt_bytes(3)).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), b"0").unwrap();
        fs::write(root.path.join("STAGE1/RC1001.pak"), b"1").unwrap();
        fs::write(root.path.join("STAGE1/RC1002.pak"), b"2").unwrap();

        let plan = build_plan(&root.path).unwrap();

        assert_eq!(plan.rooms.len(), 1);
        let room = &plan.rooms[0];
        assert_eq!(room.id, RoomId::parse("1000").unwrap());
        assert_eq!(room.rdts.len(), 2);
        assert_eq!(room.cameras, 3);
        assert_eq!(room.paks.len(), 3);
    }

    #[test]
    fn missing_paks_require_the_union_of_variants() {
        let root = TempDir::new("union-missing");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_bytes(1)).unwrap();
        fs::write(root.path.join("STAGE1/ROOM1001.RDT"), rdt_bytes(3)).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), b"0").unwrap();
        fs::write(root.path.join("STAGE1/RC1001.pak"), b"1").unwrap();

        let message = build_plan(&root.path).unwrap_err().to_string();

        assert!(message.contains("RC1002.pak"), "{message}");
        assert!(!message.contains("RC1001.pak"), "{message}");
    }

    #[test]
    fn weapon_assets_resolve_to_their_entries() {
        let root = TempDir::new("weapons");
        make_stage_dirs(&root.path);
        let players = root.path.join("PLAYERS");
        fs::write(players.join("W02.EMW"), b"emw2").unwrap();
        fs::write(players.join("w11.emw"), b"emw11").unwrap();
        fs::write(players.join("WS202.TMD"), b"tmd202").unwrap();
        fs::write(players.join("ws236.tmd"), b"tmd236").unwrap();
        // The locomotion pair stays out of the weapon set.
        let assets = resolve_weapon_assets(Some(&players)).unwrap();
        let entries: Vec<&str> = assets.iter().map(|asset| asset.entry.as_str()).collect();
        assert_eq!(
            entries,
            vec![
                "player/w02.emw",
                "player/w11.emw",
                "player/ws202.tmd",
                "player/ws236.tmd"
            ]
        );
        assert_eq!(assets[0].source, players.join("W02.EMW"));

        // No PLAYERS directory is an empty weapon set, not an error.
        assert!(resolve_weapon_assets(None).unwrap().is_empty());
    }

    #[test]
    fn converts_synthetic_game_and_dedupes_variants() {
        let root = TempDir::new("synthetic-full");
        make_stage_dirs(&root.path);
        write_npc_files(&root.path);
        let pak = camera_pak();

        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_bytes(1)).unwrap();
        fs::write(root.path.join("STAGE1/room1001.rdt"), rdt_bytes(1)).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), &pak).unwrap();
        fs::write(root.path.join("STAGE6/ROOM6000.RDT"), rdt_bytes(1)).unwrap();

        fs::create_dir_all(root.path.join("sound")).unwrap();
        fs::write(root.path.join("sound/BGM_13.WAV"), b"wav13").unwrap();
        fs::write(root.path.join("sound/bgm_24a.wav"), b"wav24a").unwrap();
        fs::write(root.path.join("sound/BGM_02.WAV"), b"wav02").unwrap();
        fs::write(root.path.join("sound/not_bgm.wav"), b"other").unwrap();
        write_se_files(&root.path);
        write_door_files(&root.path);

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();

        assert!(pack.contains("room/1000.rdt"));
        assert!(pack.contains("room/1001.rdt"));
        assert!(pack.contains("room/6000.rdt"));
        assert!(pack.contains("roomcut/100_000.bmp"));
        assert!(pack.contains("roomcut/600_000.bmp"));
        assert!(pack.contains("bgm/013.wav"));
        assert!(pack.contains("bgm/024_00.wav"));
        assert!(pack.contains("bgm/002.wav"));
        assert!(!pack.contains("bgm/000.wav"));
        assert!(pack.contains("se/ft_wda.wav"));
        assert_eq!(pack.read("se/ft_wda.wav").unwrap().as_ref(), b"ft_wdA");

        assert_eq!(
            pack.read("door/door00.dor").unwrap().as_ref(),
            b"dor-door00"
        );
        assert_eq!(pack.read("door/kai03.dor").unwrap().as_ref(), b"dor-kai03");
        assert_eq!(pack.read("door/lad01.dor").unwrap().as_ref(), b"dor-lad01");

        assert_eq!(pack.read("player/01.emd").unwrap().as_ref(), b"emd1");
        assert_eq!(pack.read("player/01.emw").unwrap().as_ref(), b"emw1");
        assert_eq!(
            pack.read("player/00_alt0.emd").unwrap().as_ref(),
            b"alt-chris"
        );
        assert_eq!(
            pack.read("player/01_alt0.emd").unwrap().as_ref(),
            b"alt-jill"
        );
        assert!(
            !pack.contains("player/00_alt1.emd"),
            "missing outfits are skipped"
        );
        // The NPC phase copies the fifteen character models raw, lower-cased.
        assert_eq!(pack.read("enemy/em20.emd").unwrap().as_ref(), b"npc-20");
        assert_eq!(pack.read("enemy/em2e.emd").unwrap().as_ref(), b"npc-2e");

        let manifest = manifest::Manifest::parse(
            std::str::from_utf8(pack.read(manifest::ENTRY).unwrap().as_ref()).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.id, BASE_PACK_ID);
        assert_eq!(manifest.kind, manifest::PackKind::Base);
        assert_eq!(manifest.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));

        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("manifest"), 1);
        assert_eq!(count("room/"), 3);
        assert_eq!(count("roomcut/"), 2);
        assert_eq!(count("roommask/"), 0);
        assert_eq!(count("bgm/"), 3);
        assert_eq!(
            count("se/"),
            sfx::SE_NAMES.len() + music::se_track_names().len(),
            "one pack entry per named effect and per non-Bgm group track"
        );
        assert_eq!(count("door/"), 34);
        assert_eq!(
            count("player/"),
            8,
            "four models and clips plus two outfit models"
        );
        assert_eq!(
            pack.paths()
                .filter(|path| path.starts_with("enemy/") && path.ends_with(".emd"))
                .count(),
            15
        );
        assert_eq!(
            count("enemy/em13.lua"),
            1,
            "the checked-in script is packed"
        );
        assert_eq!(
            pack.paths()
                .filter(|path| path.starts_with("player/") && path.ends_with(".emd"))
                .count(),
            6,
            "four character models plus two outfit models"
        );
        assert_eq!(
            pack.paths()
                .filter(|path| path.starts_with("player/") && path.ends_with(".emw"))
                .count(),
            2
        );

        let image = bmp::decode(&pack.read("roomcut/100_000.bmp").unwrap()).unwrap();
        assert_eq!((image.width, image.height), (CUT_WIDTH, CUT_HEIGHT));
    }

    #[test]
    fn resolves_voice_files_and_lists_unreferenced() {
        let root = TempDir::new("voice-plan");
        make_stage_dirs(&root.path);
        // Case-insensitive directory and file names, like the shipped tree.
        let voice = root.path.join("VOICE");
        fs::create_dir_all(&voice).unwrap();
        fs::write(voice.join("v001_00.wav"), b"a").unwrap();
        fs::write(voice.join("V104_00.WAV"), b"bb").unwrap();
        fs::write(voice.join("VB00_31A.wav"), b"ccc").unwrap();
        fs::write(voice.join("ANNOUNCE.WAV"), b"dddd").unwrap();

        let layout = discover_layout(&root.path).unwrap();
        assert_eq!(layout.voice.as_deref(), Some(voice.as_path()));

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(plan.voice.len(), 3);
        // Row 0 id 0 (`V001_00`).
        assert!(
            plan.voice
                .iter()
                .any(|asset| asset.entry == "voice/01_00.wav")
        );
        // Row 1 id 0 (`V104_00`), lowercased source file.
        assert!(
            plan.voice
                .iter()
                .any(|asset| asset.entry == "voice/02_00.wav")
        );
        // The 8-character `VB00_31a` record is row 2 id 45.
        assert!(
            plan.voice
                .iter()
                .any(|asset| asset.entry == "voice/03_2d.wav")
        );
        assert_eq!(plan.voice_unreferenced, vec!["ANNOUNCE.WAV".to_string()]);
        assert!(plan.voice_dir_found);

        // A missing referenced file aggregates into one warning.
        fs::remove_file(voice.join("V104_00.WAV")).unwrap();
        let plan = build_plan(&root.path).unwrap();
        let warnings: Vec<&String> = plan
            .warnings
            .iter()
            .filter(|warning| warning.contains("voice file"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", plan.warnings);
        assert!(warnings[0].contains("V104_00.WAV"), "{:?}", warnings);
    }

    #[test]
    fn resolves_movie_files_and_warns_for_missing() {
        let root = TempDir::new("movie-plan");
        make_stage_dirs(&root.path);
        // Case-insensitive directory and file names, like the shipped tree.
        let movie = root.path.join("MOVIE");
        fs::create_dir_all(&movie).unwrap();
        fs::write(movie.join("PJ.AVI"), b"pj").unwrap();
        fs::write(movie.join("oj.avi"), b"oj").unwrap();

        let layout = discover_layout(&root.path).unwrap();
        assert_eq!(layout.movie.as_deref(), Some(movie.as_path()));

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(plan.movie.len(), 2);
        assert!(plan.movie.iter().any(|asset| asset.entry == "movie/pj.avi"));
        assert!(plan.movie.iter().any(|asset| asset.entry == "movie/oj.avi"));
        assert!(plan.movie_dir_found);
        let warning = plan
            .warnings
            .iter()
            .find(|warning| warning.contains("film file"))
            .expect("aggregated missing-film warning");
        assert!(warning.contains("25 film file"), "{warning}");
        assert!(!warning.contains("VLOGO.AVI"), "{warning}");

        // Without a movie directory no warning is raised here; the conversion
        // summary reports the whole missing category instead.
        fs::remove_dir_all(&movie).unwrap();
        let plan = build_plan(&root.path).unwrap();
        assert!(plan.movie.is_empty());
        assert!(!plan.movie_dir_found);
        assert!(!plan.warnings.iter().any(|w| w.contains("film file")));
    }

    #[test]
    fn resolves_western_film_names_to_the_canonical_entries() {
        // The Steam release's English tree carries OU/PU/EU4/EU5 and the
        // `_r` staff rolls, while the engine's table is the Japanese one.
        // Every row must pack under the canonical name and the absent Virgin
        // logo must not count as a missing film.
        let root = TempDir::new("movie-western");
        make_stage_dirs(&root.path);
        let movie = root.path.join("english/USA/Movie");
        fs::create_dir_all(&movie).unwrap();
        for id in 0..=28u8 {
            let Some(name) = movie::name(id) else {
                continue;
            };
            if name == "vlogo" {
                continue;
            }
            let western = movie::release_names(name).last().unwrap();
            fs::write(
                movie.join(format!("{}.AVI", western.to_ascii_uppercase())),
                western.as_bytes(),
            )
            .unwrap();
        }

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(plan.movie.len(), 27);
        assert!(
            !plan.warnings.iter().any(|w| w.contains("film file")),
            "{:?}",
            plan.warnings
        );
        let source = |entry: &str| {
            plan.movie
                .iter()
                .find(|asset| asset.entry == entry)
                .map(|asset| file_name(&asset.source))
                .unwrap_or_default()
        };
        assert_eq!(source("movie/oj.avi"), "OU.AVI");
        assert_eq!(source("movie/pj.avi"), "PU.AVI");
        assert_eq!(source("movie/ed4.avi"), "EU4.AVI");
        assert_eq!(source("movie/ed5.avi"), "EU5.AVI");
        assert_eq!(source("movie/staf_b.avi"), "STAF_R.AVI");
    }

    #[test]
    fn prefers_the_conversion_trees_film_spelling_before_other_trees() {
        // Each directory is searched under both spellings before the next
        // tree: the primary canonical name wins, then its Western alias, and
        // only then another tree's canonical file.
        let root = TempDir::new("movie-precedence");
        make_stage_dirs(&root.path);
        let english = root.path.join("english/USA/Movie");
        let japanese = root.path.join("japanese/JPN/Movie");
        fs::create_dir_all(&english).unwrap();
        fs::create_dir_all(&japanese).unwrap();
        fs::write(english.join("OJ.AVI"), b"primary-canonical").unwrap();
        fs::write(english.join("OU.AVI"), b"primary-western").unwrap();
        fs::write(japanese.join("OJ.AVI"), b"fallback-canonical").unwrap();
        fs::write(english.join("PU.AVI"), b"primary-western-pu").unwrap();
        fs::write(japanese.join("PJ.AVI"), b"fallback-canonical-pj").unwrap();
        fs::write(japanese.join("ED4.AVI"), b"fallback-ed4").unwrap();
        fs::write(english.join("STAF_R.AVI"), b"primary-staf-r").unwrap();
        fs::write(japanese.join("STAF_B.AVI"), b"fallback-staf-b").unwrap();

        let plan = build_plan(&root.path).unwrap();
        let source = |entry: &str| {
            plan.movie
                .iter()
                .find(|asset| asset.entry == entry)
                .map(|asset| file_name(&asset.source))
                .unwrap_or_default()
        };
        assert_eq!(source("movie/oj.avi"), "OJ.AVI", "primary canonical wins");
        assert_eq!(
            source("movie/pj.avi"),
            "PU.AVI",
            "the primary tree's Western spelling beats a fallback canonical"
        );
        assert_eq!(
            source("movie/ed4.avi"),
            "ED4.AVI",
            "a film only the fallback tree has is still packed"
        );
        assert_eq!(source("movie/staf_b.avi"), "STAF_R.AVI");
    }

    #[test]
    fn merges_the_steam_language_trees_for_item_and_file_art() {
        // The English tree has most of the shared assets; the Japanese tree
        // adds the FILEI backdrops, TEXTM pages, one item model and one map.
        // Every item phase must pack the merged set without a missing-file
        // warning.
        let root = TempDir::new("steam-item-art");
        make_stage_dirs(&root.path);
        let english = root.path.join("english/USA/Item_m2");
        let japanese = root.path.join("japanese/JPN/Item_m2");
        fs::create_dir_all(&english).unwrap();
        fs::create_dir_all(&japanese).unwrap();
        fs::write(english.join("MINI.IVM"), b"ivm1").unwrap();
        fs::write(japanese.join("I00V.IVM"), b"ivm0").unwrap();
        fs::write(english.join("FILE000.TIM"), b"cover0").unwrap();
        fs::write(english.join("FILE001.TIM"), b"cover1").unwrap();
        for number in 0..0x0Eu32 {
            fs::write(
                english.join(format!("MAP{number:02X}.TIM")),
                format!("plan{number}"),
            )
            .unwrap();
        }
        fs::write(japanese.join("MAP0E.TIM"), b"plan14").unwrap();
        fs::write(japanese.join("Map_blue.TIM"), b"blue").unwrap();
        for number in 1..=FILEI_COUNT {
            fs::write(
                japanese.join(format!("FILEI{number:02}.TIM")),
                format!("i{number}"),
            )
            .unwrap();
        }
        for number in 0..TEXTM_COUNT {
            fs::write(
                japanese.join(format!("TEXTM_{number:02}.TIM")),
                format!("page{number}"),
            )
            .unwrap();
        }

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(plan.item_m2_dirs.len(), 2);
        assert_eq!(plan.item_m2_dirs[0], english);

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let mut warnings = Vec::new();
        let (items, _bytes) = copy_item_models(
            &plan.item_m2_dirs,
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();
        let (files, _bytes) = copy_file_art(
            &plan.item_m2_dirs,
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();
        let (maps, _bytes) = copy_map_art(
            &plan.item_m2_dirs,
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();
        assert_eq!(items, 2);
        assert_eq!(files, 2 + FILEI_COUNT + TEXTM_COUNT);
        assert_eq!(maps, 16);
        // The two-IVM fixture legitimately warns about the count; the merged
        // file and map lookups must not warn at all.
        assert!(
            warnings
                .iter()
                .all(|warning| warning.contains("item model")),
            "{warnings:?}"
        );

        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.read("item/i00v.ivm").unwrap().as_ref(), b"ivm0");
        assert_eq!(pack.read("item/mini.ivm").unwrap().as_ref(), b"ivm1");
        assert_eq!(pack.read("file/filei17.tim").unwrap().as_ref(), b"i17");
        assert_eq!(pack.read("file/textm_42.tim").unwrap().as_ref(), b"page42");
        assert_eq!(pack.read("map/map0e.tim").unwrap().as_ref(), b"plan14");
        assert_eq!(pack.read("map/blue.tim").unwrap().as_ref(), b"blue");
    }

    #[test]
    fn discovers_an_item_m2_only_in_a_fallback_tree() {
        // A Steam root need not have an ITEM_M2 in every tree; the first one
        // found is the primary and still supplies every item phase.
        let root = TempDir::new("fallback-item-art");
        make_stage_dirs(&root.path);
        let japanese = root.path.join("japanese/JPN/Item_m2");
        fs::create_dir_all(&japanese).unwrap();
        fs::write(japanese.join("FILE000.TIM"), b"cover0").unwrap();
        fs::write(japanese.join("FILE001.TIM"), b"cover1").unwrap();
        for number in 1..=FILEI_COUNT {
            fs::write(
                japanese.join(format!("FILEI{number:02}.TIM")),
                format!("i{number}"),
            )
            .unwrap();
        }
        for number in 0..TEXTM_COUNT {
            fs::write(
                japanese.join(format!("TEXTM_{number:02}.TIM")),
                format!("page{number}"),
            )
            .unwrap();
        }

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(plan.item_m2_dirs, [japanese]);
        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let mut warnings = Vec::new();
        let (files, _bytes) = copy_file_art(
            &plan.item_m2_dirs,
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();
        assert_eq!(files, 2 + FILEI_COUNT + TEXTM_COUNT);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn converts_two_steam_language_trees_into_one_complete_pack() {
        // The end-to-end wiring: the English tree supplies the shared assets
        // and its own film spellings, the Japanese tree supplies the document
        // pages. The one pack must carry the canonical film entries and all
        // 62 document-art entries.
        let root = TempDir::new("steam-pack");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1100.RDT"), [0u8; 4]).unwrap();

        let english_item = root.path.join("english/USA/Item_m2");
        let japanese_item = root.path.join("japanese/JPN/Item_m2");
        fs::create_dir_all(&english_item).unwrap();
        fs::create_dir_all(&japanese_item).unwrap();
        fs::write(english_item.join("MINI.IVM"), b"ivm1").unwrap();
        fs::write(english_item.join("FILE000.TIM"), b"cover0").unwrap();
        fs::write(english_item.join("FILE001.TIM"), b"cover1").unwrap();
        for number in 1..=FILEI_COUNT {
            fs::write(
                japanese_item.join(format!("FILEI{number:02}.TIM")),
                format!("i{number}"),
            )
            .unwrap();
        }
        for number in 0..TEXTM_COUNT {
            fs::write(
                japanese_item.join(format!("TEXTM_{number:02}.TIM")),
                format!("page{number}"),
            )
            .unwrap();
        }

        let english_movie = root.path.join("english/USA/Movie");
        fs::create_dir_all(&english_movie).unwrap();
        let western = [
            ("oj", "ou"),
            ("pj", "pu"),
            ("ed4", "eu4"),
            ("ed5", "eu5"),
            ("stfc_b", "stfc_r"),
            ("stfj_b", "stfj_r"),
            ("stfz_b", "stfz_r"),
            ("staf_b", "staf_r"),
        ];
        for id in 0..=28u8 {
            let Some(name) = movie::name(id) else {
                continue;
            };
            if !movie::is_shipped(name) {
                continue;
            }
            let file = western
                .iter()
                .find_map(|(canonical, file)| (*canonical == name).then_some(*file))
                .unwrap_or(name);
            fs::write(
                english_movie.join(format!("{file}.AVI")),
                format!("film-{name}"),
            )
            .unwrap();
        }

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();
        assert_eq!(
            pack.paths()
                .filter(|path| path.starts_with("movie/"))
                .count(),
            27
        );
        assert_eq!(
            pack.paths()
                .filter(|path| path.starts_with("file/"))
                .count(),
            2 + FILEI_COUNT + TEXTM_COUNT
        );
        for id in 0..=28u8 {
            let Some(name) = movie::name(id) else {
                continue;
            };
            if !movie::is_shipped(name) {
                continue;
            }
            assert_eq!(
                pack.read(&movie::pack_path(name)).unwrap().as_ref(),
                format!("film-{name}").as_bytes(),
                "{name}"
            );
        }
        assert_eq!(pack.read("file/textm_42.tim").unwrap().as_ref(), b"page42");
        assert!(!pack.contains("movie/vlogo.avi"));
    }

    #[test]
    fn converts_movies_into_the_single_pack() {
        let root = TempDir::new("movie-pack");
        make_stage_dirs(&root.path);
        write_npc_files(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_bytes(1)).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), camera_pak()).unwrap();
        fs::create_dir_all(root.path.join("sound")).unwrap();
        write_se_files(&root.path);
        write_door_files(&root.path);
        let movie = root.path.join("movie");
        fs::create_dir_all(&movie).unwrap();
        fs::write(movie.join("oj.avi"), b"movie-oj").unwrap();
        fs::write(movie.join("capcom.AVI"), b"movie-capcom").unwrap();

        let out = root.path.join("out.akpak");
        convert_game_with_options(&root.path, &out, 1, false).unwrap();
        let main = crate::pack::Pack::open(&out).unwrap();
        assert_eq!(main.read("movie/oj.avi").unwrap().as_ref(), b"movie-oj");
        assert_eq!(
            main.read("movie/capcom.avi").unwrap().as_ref(),
            b"movie-capcom"
        );
        // The issue's policy end to end: an already-compressed film stays
        // borrowed (raw), a compressible RDT is owned (zstd).
        assert!(matches!(
            main.read("movie/oj.avi").unwrap(),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(
            main.read("room/1000.rdt").unwrap(),
            std::borrow::Cow::Owned(_)
        ));
        // No companion pack is written beside the main one.
        assert!(!root.path.join("out.movie.akpak").exists());
    }

    #[test]
    fn converts_voice_into_the_single_pack() {
        let root = TempDir::new("voice-pack");
        make_stage_dirs(&root.path);
        write_npc_files(&root.path);
        let pak = camera_pak();
        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_bytes(1)).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), &pak).unwrap();
        fs::create_dir_all(root.path.join("sound")).unwrap();
        write_se_files(&root.path);
        write_door_files(&root.path);
        let voice = root.path.join("voice");
        fs::create_dir_all(&voice).unwrap();
        fs::write(voice.join("v001_00.wav"), b"voice-a").unwrap();
        fs::write(voice.join("V104_00.WAV"), b"voice-bb").unwrap();
        fs::write(voice.join("V106_00.WAV"), b"voice-dup").unwrap();
        fs::write(voice.join("ANNOUNCE.WAV"), b"unused").unwrap();

        let out = root.path.join("out.akpak");
        convert_game_with_options(&root.path, &out, 1, false).unwrap();
        let main = crate::pack::Pack::open(&out).unwrap();
        assert_eq!(main.read("voice/01_00.wav").unwrap().as_ref(), b"voice-a");
        assert_eq!(main.read("voice/02_00.wav").unwrap().as_ref(), b"voice-bb");
        // `V106_00` is one source file referenced by two records: row 0 id
        // 130 and row 1 id 34 each get their own entry with the same bytes.
        assert_eq!(main.read("voice/01_82.wav").unwrap().as_ref(), b"voice-dup");
        assert_eq!(main.read("voice/02_22.wav").unwrap().as_ref(), b"voice-dup");
        // The unreferenced file stays out and no companion pack is written.
        assert!(!main.contains("voice/announce.wav"));
        assert!(!root.path.join("out.voice.akpak").exists());
    }

    #[test]
    fn parallel_map_reports_the_first_error_in_job_order() {
        let mut progress = Progress::new("test", 1);
        let result = parallel_map(16, 4, &mut progress, |index| {
            if index == 5 || index == 11 {
                bail!("job {index} failed");
            }
            Ok((format!("job {index}"), 1, index))
        });
        let message = result.unwrap_err().to_string();
        assert!(message.contains("job 5"), "{message}");
    }

    #[test]
    fn conversion_is_identical_for_any_worker_count() {
        let root = TempDir::new("jobs-parity");
        make_stage_dirs(&root.path);
        write_npc_files(&root.path);
        let pak = camera_pak();

        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_bytes(1)).unwrap();
        fs::write(root.path.join("STAGE1/room1001.rdt"), rdt_bytes(1)).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), &pak).unwrap();
        fs::write(root.path.join("STAGE6/ROOM6000.RDT"), rdt_bytes(1)).unwrap();

        fs::create_dir_all(root.path.join("sound")).unwrap();
        fs::write(root.path.join("sound/BGM_13.WAV"), b"wav13").unwrap();
        write_se_files(&root.path);
        write_door_files(&root.path);

        let serial = root.path.join("serial.akpak");
        let parallel = root.path.join("parallel.akpak");
        convert_game_with_options(&root.path, &serial, 1, false).unwrap();
        convert_game_with_options(&root.path, &parallel, 4, false).unwrap();
        assert_eq!(fs::read(&serial).unwrap(), fs::read(&parallel).unwrap());
    }

    #[test]
    fn entry_report_lines_name_both_sizes() {
        assert_eq!(
            entry_report_line("room/1000.rdt", 1000, 250, true),
            "room/1000.rdt: 1000 -> 250 bytes (75.0% smaller, zstd)"
        );
        assert_eq!(
            entry_report_line("movie/oj.avi", 1000, 1000, false),
            "movie/oj.avi: 1000 -> 1000 bytes (stored raw)"
        );
    }

    /// A writer that fails every write with a fixed [`std::io::ErrorKind`].
    struct FailingWriter(std::io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(self.0))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_drops_the_report_but_other_stdout_errors_fail() {
        let mut broken_pipe = FailingWriter(std::io::ErrorKind::BrokenPipe);
        assert!(flush_report(&mut broken_pipe, b"report").is_ok());

        let mut failed = FailingWriter(std::io::ErrorKind::StorageFull);
        let error = flush_report(&mut failed, b"report").unwrap_err();
        assert!(error.to_string().contains("conversion report"), "{error}");
    }

    #[test]
    fn the_verbose_report_lists_every_entry_in_processing_order() {
        let entries = [
            ("room/1000.rdt", vec![3u8; 9_000]),
            ("movie/oj.avi", vec![1u8, 2, 3, 4]),
            ("manifest.toml", b"format = 1\n".to_vec()),
        ];
        let mut one = PackWriter::new();
        let mut many = PackWriter::new();
        for (path, data) in &entries {
            one.add(path, data.clone()).unwrap();
            many.add(path, data.clone()).unwrap();
        }
        let raw = |path: &str| path.ends_with(".avi");
        assert_eq!(one.compress_entries(1, raw).unwrap(), 1);
        assert_eq!(many.compress_entries(4, raw).unwrap(), 1);

        let mut serial = Vec::new();
        write_entry_report(&one, &mut serial).unwrap();
        let mut parallel = Vec::new();
        write_entry_report(&many, &mut parallel).unwrap();
        assert_eq!(
            serial, parallel,
            "the report must not depend on the worker count or scheduling"
        );

        let text = String::from_utf8(serial).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), entries.len());
        assert!(lines[0].starts_with("room/1000.rdt: 9000 -> "), "{text}");
        assert!(lines[0].ends_with("% smaller, zstd)"), "{text}");
        assert_eq!(lines[1], "movie/oj.avi: 4 -> 4 bytes (stored raw)");
        assert_eq!(lines[2], "manifest.toml: 11 -> 11 bytes (stored raw)");
    }

    #[test]
    fn converts_room_masks_and_folds_return_stages() {
        let root = TempDir::new("masks");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_with_masks()).unwrap();
        fs::write(root.path.join("STAGE6/ROOM6000.RDT"), rdt_with_masks()).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), camera_pak()).unwrap();
        fs::create_dir_all(root.path.join("ObJsPr")).unwrap();
        let mask = mask_pak_at(16, 8);
        fs::write(root.path.join("ObJsPr/osp00000.PAK"), &mask).unwrap();

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();

        assert!(pack.contains("roommask/100_000.bmp"));
        assert!(pack.contains("roommask/600_000.bmp"));
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("roommask/"), 2);

        let decoded = tim::decode_8bpp(&lzw::decode(&mask).unwrap()).unwrap();
        let expected = bmp::encode_texture8_to_vec(&decoded).unwrap();
        assert_eq!(pack.read("roommask/100_000.bmp").unwrap(), expected);
        let image = bmp::decode(&expected).unwrap();
        assert_eq!((image.width, image.height), (16, 8));
        let direct = bmp::encode_texture8_to_vec(&decoded).unwrap();
        assert_eq!(image.rgba, bmp::decode(&direct).unwrap().rgba);
    }

    #[test]
    fn converts_door_art_from_item_m1_case_insensitively() {
        let root = TempDir::new("doors");
        make_stage_dirs(&root.path);
        write_door_files(&root.path);
        // Rename a couple of files to mixed case like the shipped install.
        fs::rename(
            root.path.join("ITEM_M1/ELE01.DOR"),
            root.path.join("ITEM_M1/Ele01.dor"),
        )
        .unwrap();
        fs::rename(
            root.path.join("ITEM_M1/MON.DOR"),
            root.path.join("ITEM_M1/mon.dor"),
        )
        .unwrap();

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();

        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("door/"), 34);
        assert_eq!(pack.read("door/ele01.dor").unwrap().as_ref(), b"dor-ele01");
        assert_eq!(pack.read("door/mon.dor").unwrap().as_ref(), b"dor-mon");
        // Every door type byte maps to a distinct pack entry.
        let mut entries: Vec<&str> = pack
            .paths()
            .filter(|path| path.starts_with("door/"))
            .collect();
        entries.sort_unstable();
        entries.dedup();
        assert_eq!(entries.len(), 34);
    }

    #[test]
    fn missing_door_files_are_aggregated() {
        let root = TempDir::new("missing-doors");
        make_stage_dirs(&root.path);
        write_door_files(&root.path);
        fs::remove_file(root.path.join("ITEM_M1/MON.DOR")).unwrap();
        fs::remove_file(root.path.join("ITEM_M1/KAI04.DOR")).unwrap();

        let message = convert_game(&root.path, &root.path.join("out.akpak"))
            .unwrap_err()
            .to_string();

        assert!(message.contains("MON.DOR"), "{message}");
        assert!(message.contains("KAI04.DOR"), "{message}");
        assert!(!message.contains("DOOR00.DOR"), "{message}");
        assert!(!root.path.join("out.akpak").exists());
    }

    #[test]
    fn a_missing_item_m1_directory_packs_no_doors() {
        let root = TempDir::new("no-doors");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1100.RDT"), [0u8; 4]).unwrap();

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();

        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("door/"), 0);
    }

    #[test]
    fn missing_mask_paks_are_warnings() {
        let root = TempDir::new("missing-masks");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_with_masks()).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), camera_pak()).unwrap();
        fs::create_dir_all(root.path.join("objspr")).unwrap();

        let plan = build_plan(&root.path).unwrap();

        assert!(plan.roommask.is_empty());
        assert_eq!(
            plan.warnings
                .iter()
                .filter(|warning| warning.contains("OSP00000.pak"))
                .count(),
            1,
            "{:?}",
            plan.warnings
        );

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("roommask/"), 0);
    }

    #[test]
    fn a_missing_objspr_directory_warns_once() {
        let root = TempDir::new("no-objspr");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_with_masks()).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), camera_pak()).unwrap();

        let plan = build_plan(&root.path).unwrap();

        assert!(plan.roommask.is_empty());
        assert_eq!(
            plan.warnings
                .iter()
                .filter(|warning| warning.contains("objspr"))
                .count(),
            1,
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn accepts_a_316x236_background() {
        let root = TempDir::new("small-cut");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE3/ROOM3000.RDT"), rdt_bytes(1)).unwrap();
        fs::write(root.path.join("STAGE3/RC3000.pak"), camera_pak_at(316, 236)).unwrap();

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();

        let pack = crate::pack::Pack::open(&out).unwrap();
        let image = bmp::decode(&pack.read("roomcut/300_000.bmp").unwrap()).unwrap();
        assert_eq!((image.width, image.height), (316, 236));
    }

    #[test]
    fn missing_paks_are_aggregated_and_no_pack_is_written() {
        let root = TempDir::new("missing-paks");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_bytes(2)).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), b"0").unwrap();
        fs::write(root.path.join("STAGE2/ROOM2000.RDT"), rdt_bytes(1)).unwrap();

        let out = root.path.join("out.akpak");
        let message = convert_game(&root.path, &out).unwrap_err().to_string();

        assert!(message.contains("RC1001.pak"), "{message}");
        assert!(message.contains("RC2000.pak"), "{message}");
        assert!(!out.exists(), "a failed conversion must not write a pack");
    }

    #[test]
    fn reads_bmp_bit_depth_from_header() {
        let mut data = vec![0u8; 30];
        data[28..30].copy_from_slice(&8u16.to_le_bytes());
        assert_eq!(bmp_bpp(&data).unwrap(), 8);
        data[28..30].copy_from_slice(&24u16.to_le_bytes());
        assert_eq!(bmp_bpp(&data).unwrap(), 24);
        assert!(bmp_bpp(&data[..28]).is_err());
    }

    /// A three-row palette with red at index 1 and green at index 2 of row 2.
    fn palette_texture() -> Texture8 {
        let mut palettes = vec![[0u8; 4]; 3 * 256];
        palettes[2 * 256 + 1] = [255, 0, 0, 255];
        palettes[2 * 256 + 2] = [0, 255, 0, 255];
        Texture8 {
            width: 1,
            height: 1,
            indices: vec![0],
            palettes,
            stp: Vec::new(),
        }
    }

    #[test]
    fn bakes_item_atlas_through_the_second_clut_row() {
        let mut pix = vec![0u8; items::ICON_ROW_BYTES * 2];
        pix[0] = 1;
        pix[1] = 0;
        pix[items::ICON_ROW_BYTES] = 2;

        let baked = bake_item_atlas(&pix, 2, &palette_texture()).unwrap();
        let image = bmp::decode(&baked).unwrap();
        assert_eq!((image.width, image.height), (40, 60));
        assert_eq!(&image.rgba[0..4], &[255, 0, 0, 255], "index 1 is red");
        assert_eq!(&image.rgba[4..8], &[0, 0, 0, 255], "index 0 is black");
        assert_eq!(
            &image.rgba[items::ICON_ROW_BYTES * 4..items::ICON_ROW_BYTES * 4 + 4],
            &[0, 255, 0, 255],
            "the second row starts a new icon row"
        );

        let mask = bmp::decode_mask(&baked).unwrap();
        assert_eq!(mask.rgba[3], 255);
        assert_eq!(mask.rgba[7], 0, "index 0 decodes transparent");
        assert_eq!(mask.rgba[items::ICON_ROW_BYTES * 4 + 3], 255);

        assert!(bake_item_atlas(&pix[..pix.len() - 1], 2, &palette_texture()).is_err());
    }

    #[test]
    fn decodes_headerless_rgb555_pix() {
        let raw = [0x1Fu8, 0x00, 0xE0, 0x03];
        let image = decode_pix_16(&raw, 2, 1).unwrap();
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(&image.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&image.rgba[4..8], &[0, 255, 0, 255]);
        assert!(decode_pix_16(&raw[..3], 2, 1).is_err());
    }

    #[test]
    fn converts_raw_tim_and_pix_ui_assets() {
        // Raw entries are copied byte for byte.
        let raw = vec![1u8, 2, 3];
        assert_eq!(convert_ui_asset(UiKind::Raw, &raw).unwrap(), raw);

        // A 1x1 16bpp TIM becomes a white 1x1 BMP.
        let bmp_bytes = convert_ui_asset(UiKind::Tim16, &tim16_1x1()).unwrap();
        let image = bmp::decode(&bmp_bytes).unwrap();
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.rgba, [255, 255, 255, 255]);
    }

    #[test]
    fn resolves_data_assets_and_warns_per_category() {
        let root = TempDir::new("data-plan");
        let data = root.path.join("install/DATA");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("STATUS.TIM"), b"tim").unwrap();
        fs::write(data.join("BIO_CARD.DAT"), b"card").unwrap();
        fs::write(data.join("ITEM_ALL.PIX"), b"pix").unwrap();

        let plan = resolve_data_assets(Some(&data)).unwrap();
        assert_eq!(plan.items.len(), 1, "only ITEM_ALL.PIX is present");
        assert!(plan.palette.is_some());
        assert!(plan.bio_card.is_some());
        assert!(plan.shadow.is_none());
        assert_eq!(plan.ui.len(), 1, "STATUS.TIM is also a raw UI entry");
        assert_eq!(
            plan.warnings
                .iter()
                .filter(|warning| warning.contains("UI art"))
                .count(),
            1,
            "{:?}",
            plan.warnings
        );
        assert_eq!(
            plan.warnings
                .iter()
                .filter(|warning| warning.contains("item art"))
                .count(),
            1,
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains(crate::shadow::KAGE_ENTRY)),
            "{:?}",
            plan.warnings
        );

        // Without STATUS.TIM the atlases are dropped with a warning.
        fs::remove_file(data.join("STATUS.TIM")).unwrap();
        let plan = resolve_data_assets(Some(&data)).unwrap();
        assert!(plan.items.is_empty());
        assert!(plan.palette.is_none());
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("STATUS.TIM")),
            "{:?}",
            plan.warnings
        );

        // A missing whole directory warns once without listing every file.
        let plan = resolve_data_assets(None).unwrap();
        assert_eq!(plan.warnings.len(), 1);
        assert!(
            plan.warnings[0].contains("data directory"),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn converts_synthetic_data_assets_into_the_pack() {
        let root = TempDir::new("synthetic-data");
        make_stage_dirs(&root.path);
        write_door_files(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1100.RDT"), [0u8; 4]).unwrap();

        let data = root.path.join("DATA");
        fs::create_dir_all(&data).unwrap();
        // A valid 8bpp palette TIM with three 256-entry CLUT rows.
        let mut palette = vec![0u16; 3 * 256];
        palette[2 * 256 + 1] = 0x001F;
        fs::write(data.join("STATUS.TIM"), tim_8bpp_palette(&palette)).unwrap();
        fs::write(
            data.join("ITEM_ALL.PIX"),
            vec![1u8; items::ICON_ROW_BYTES * items::ITEM_ALL_ROWS],
        )
        .unwrap();
        fs::write(data.join("BIO_CARD.DAT"), vec![0u8; 0x41C]).unwrap();
        fs::write(data.join("KAGE.TIM"), b"KAGE.TIM").unwrap();
        for (_, file, kind) in UI_ASSETS {
            if file.eq_ignore_ascii_case(STATUS_FILE) {
                continue;
            }
            let contents = match kind {
                UiKind::Raw => file.as_bytes().to_vec(),
                UiKind::Pix16 { width, height } => {
                    vec![0u8; (*width as usize) * (*height as usize) * 2]
                }
                UiKind::Tim16 => tim16_1x1(),
            };
            fs::write(data.join(file), contents).unwrap();
        }

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("ui/"), UI_ASSETS.len() + crate::ui::UI_SCRIPTS.len());
        assert_eq!(count("item/"), 1);
        assert_eq!(count("data/"), 1);
        assert_eq!(count("shadow/"), 1);
        assert_eq!(pack.read("data/bio_card.dat").unwrap().len(), 0x41C);
        assert_eq!(
            pack.read(crate::shadow::KAGE_ENTRY).unwrap(),
            b"KAGE.TIM".as_slice()
        );
        assert_eq!(
            pack.read("ui/statface.tim").unwrap(),
            b"STATFACE.TIM".as_slice()
        );
        let image = bmp::decode(&pack.read("item/item_all.bmp").unwrap()).unwrap();
        assert_eq!(
            (image.width, image.height),
            (
                items::ICON_WIDTH,
                items::ICON_HEIGHT * items::ITEM_ALL_ROWS as u32
            )
        );
        assert_eq!(&image.rgba[0..4], &[255, 0, 0, 255]);
    }

    /// A synthetic 1x1 white 16bpp TIM.
    fn tim16_1x1() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x10u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&0x7FFFu16.to_le_bytes());
        out
    }

    /// A synthetic 8bpp TIM carrying a flattened CLUT of `entries` colors.
    fn tim_8bpp_palette(entries: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x10u32.to_le_bytes());
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&(12u32 + entries.len() as u32 * 2).to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&480i16.to_le_bytes());
        out.extend_from_slice(&256u16.to_le_bytes());
        out.extend_from_slice(&3u16.to_le_bytes());
        for entry in entries {
            out.extend_from_slice(&entry.to_le_bytes());
        }
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&[0u8, 1]);
        out
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn real_data_assets_convert_and_match_a_direct_decode() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let data_dir = PathBuf::from(root).join("JPN/DATA");
        let plan = resolve_data_assets(Some(&data_dir)).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(plan.ui.len(), UI_ASSETS.len());
        assert_eq!(plan.items.len(), ITEM_ASSETS.len());
        assert!(plan.bio_card.is_some());
        assert!(plan.shadow.is_some());

        // The same conversion the pack writer performs, checked for counts.
        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let (ui_count, _) = copy_ui_art(&plan, &mut writer, &mut progress, 1).unwrap();
        let (item_count, _) = copy_item_art(&plan, &mut writer, &mut progress, 1).unwrap();
        let (data_count, _) = copy_bio_card(&plan, &mut writer, &mut progress).unwrap();
        let (shadow_count, _) = copy_shadow(&plan, &mut writer, &mut progress).unwrap();
        assert_eq!(ui_count, UI_ASSETS.len());
        assert_eq!(item_count, ITEM_ASSETS.len());
        assert_eq!(data_count, 1);
        assert_eq!(shadow_count, 1);
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.read(BIO_CARD_ENTRY).unwrap().len(), 0x41C);
        assert_eq!(
            pack.read(crate::shadow::KAGE_ENTRY).unwrap(),
            fs::read(plan.shadow.as_ref().unwrap()).unwrap()
        );
        for asset in &plan.ui {
            assert!(pack.contains(asset.entry), "{}", asset.entry);
        }

        let palette_raw = fs::read(plan.palette.as_ref().unwrap()).unwrap();
        let texture = tim::decode_8bpp(&palette_raw).unwrap();
        for asset in &plan.items {
            let pix = fs::read(&asset.source).unwrap();
            let baked = bake_item_atlas(&pix, asset.rows, &texture).unwrap();
            assert_eq!(
                pack.read(asset.entry).unwrap(),
                baked,
                "{} differs from the direct bake",
                asset.entry
            );
            let image = bmp::decode(&baked).unwrap();
            assert_eq!(image.width, items::ICON_WIDTH);
            assert_eq!(image.height, (asset.rows as u32) * items::ICON_HEIGHT);
            let expected: Vec<u8> = pix[..asset.rows * items::ICON_ROW_BYTES]
                .iter()
                .flat_map(|&index| {
                    if index == 0 {
                        [0, 0, 0, 255]
                    } else {
                        texture.palette(2, index)
                    }
                })
                .collect();
            assert_eq!(image.rgba, expected, "{}", asset.entry);

            let mask = bmp::decode_mask(&baked).unwrap();
            for (pixel, &index) in mask.rgba.as_chunks::<4>().0.iter().zip(pix.iter()) {
                let alpha = if index == 0 { 0 } else { 255 };
                assert_eq!(pixel[3], alpha, "{} pixel {index}", asset.entry);
            }
        }
    }

    #[test]
    fn discovers_item_m2() {
        let root = TempDir::new("item-m2-discover");
        make_stage_dirs(&root.path);
        fs::create_dir_all(root.path.join("install/ItEm_M2")).unwrap();

        let layout = discover_layout(&root.path).unwrap();
        assert_eq!(layout.item_m2.unwrap(), root.path.join("install/ItEm_M2"));
    }

    #[test]
    fn packs_synthetic_item_models_and_file_art() {
        let root = TempDir::new("item-art");
        let dir = root.path.join("ITEM_M2");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("I00V.IVM"), b"ivm0").unwrap();
        fs::write(dir.join("ING.ivm"), b"ivm1").unwrap();
        fs::write(dir.join("FILE000.TIM"), b"cover0").unwrap();
        fs::write(dir.join("FILE001.TIM"), b"cover1").unwrap();
        for number in 1..=FILEI_COUNT {
            fs::write(
                dir.join(format!("FILEI{number:02}.TIM")),
                format!("i{number}"),
            )
            .unwrap();
        }
        for number in 0..TEXTM_COUNT {
            fs::write(
                dir.join(format!("TEXTM_{number:02}.TIM")),
                format!("page{number}"),
            )
            .unwrap();
        }

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let mut warnings = Vec::new();
        let (items, item_bytes) = copy_item_models(
            std::slice::from_ref(&dir),
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();
        let (files, file_bytes) = copy_file_art(
            std::slice::from_ref(&dir),
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();

        assert_eq!(items, 2);
        assert_eq!(files, 2 + FILEI_COUNT + TEXTM_COUNT);
        assert!(item_bytes > 0 && file_bytes > 0);
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.read("item/i00v.ivm").unwrap().as_ref(), b"ivm0");
        assert_eq!(pack.read("item/ing.ivm").unwrap().as_ref(), b"ivm1");
        assert_eq!(pack.read("file/file000.tim").unwrap().as_ref(), b"cover0");
        assert_eq!(pack.read("file/filei17.tim").unwrap().as_ref(), b"i17");
        assert_eq!(pack.read("file/textm_42.tim").unwrap().as_ref(), b"page42");
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("item/"), 2);
        assert_eq!(count("file/"), 62);
    }

    /// A `filem_*` container over already-encoded page streams: a 16-slot
    /// offset table (unused slots zero) then the streams back to back.
    fn document_container(streams: &[Vec<u8>]) -> Vec<u8> {
        let mut offsets = vec![0x40u32];
        for stream in streams {
            offsets.push(offsets.last().unwrap() + stream.len() as u32);
        }
        offsets.resize(16, 0);
        let mut data: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        for stream in streams {
            data.extend_from_slice(stream);
        }
        data
    }

    #[test]
    fn unpacks_document_pages_from_the_offset_table() {
        let streams = [camera_pak_at(2, 1), camera_pak_at(3, 2)];
        let pages = unpack_document_pages(&document_container(&streams)).unwrap();
        assert_eq!(pages.len(), 2);
        for (page, stream) in pages.iter().zip(&streams) {
            assert_eq!(page, &lzw::decode(stream).unwrap());
        }

        // A table that is not a table, an empty one and a page that is not a
        // TIM are all rejected.
        assert!(unpack_document_pages(&[4, 0, 0, 0]).is_err());
        assert!(unpack_document_pages(&0x400u32.to_le_bytes()).is_err());
        assert!(unpack_document_pages(&document_container(&[])).is_err());
        let junk = lzw_literals(b"not a tim at all");
        assert!(unpack_document_pages(&document_container(&[junk])).is_err());
        let mut truncated = document_container(&streams);
        truncated.truncate(truncated.len() - 1);
        assert!(unpack_document_pages(&truncated).is_err());
    }

    #[test]
    fn packs_the_western_document_pages_and_arrows() {
        let root = TempDir::new("document-pages");
        let dir = root.path.join("Item_m2");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ARROR.TIM"), b"arrows").unwrap();
        let container = document_container(&[camera_pak_at(2, 1), camera_pak_at(2, 2)]);
        for stem in FILEM_STEMS {
            fs::write(dir.join(format!("{stem}.pix")), &container).unwrap();
        }
        // An incomplete French tree is packed and reported; a German
        // container that is not one is skipped.
        fs::write(dir.join("FILEM_L0.FPX"), &container).unwrap();
        fs::write(dir.join("FILEM_L0.GPX"), b"junk").unwrap();

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let mut warnings = Vec::new();
        copy_file_art(
            std::slice::from_ref(&dir),
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.read("file/arror.tim").unwrap().as_ref(), b"arrows");
        let page = lzw::decode(&camera_pak_at(2, 2)).unwrap();
        assert_eq!(
            pack.read("file/en/filem_00_01.tim").unwrap().as_ref(),
            page.as_slice()
        );
        assert!(pack.read("file/fr/filem_l0_01.tim").is_ok());
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("file/en/"), 32);
        assert_eq!(count("file/fr/"), 2);
        assert_eq!(count("file/de/"), 0);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("15 document page file(s)")
                    && warning.contains("filem_m0.fpx")),
            "{warnings:?}"
        );
        let warned = |needle: &str| {
            warnings
                .iter()
                .any(|warning| warning.to_lowercase().contains(needle))
        };
        assert!(warned("skipped") && warned("filem_l0.gpx"), "{warnings:?}");
        assert!(
            warned("filem_00.pix holds 2 page(s), the reader expects 4"),
            "{warnings:?}"
        );
    }

    /// A 4bpp 8 x 2 TIM over a 16-entry CLUT.
    fn document_page(clut: [u16; 16], nibbles: [u8; 16]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0x10u32.to_le_bytes());
        data.extend_from_slice(&8u32.to_le_bytes());
        data.extend_from_slice(&(12u32 + 32).to_le_bytes());
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        for colour in clut {
            data.extend_from_slice(&colour.to_le_bytes());
        }
        data.extend_from_slice(&(12u32 + 8).to_le_bytes());
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        for pair in nibbles.chunks(2) {
            data.push(pair[0] | pair[1] << 4);
        }
        data
    }

    #[test]
    fn document_pages_key_their_black_at_index_zero() {
        // The shipped pages' layout: white text at 0, the black key at 7.
        let mut clut: [u16; 16] = std::array::from_fn(|index| index as u16 | 0x0400);
        clut[0] = 0x7FFF;
        clut[7] = 0;
        let nibbles = [0, 7, 7, 0, 3, 7, 0, 15, 7, 7, 0, 0, 1, 2, 7, 0];
        let original = document_page(clut, nibbles);
        let mut page = original.clone();
        key_document_page(&mut page);

        let before = tim::decode_4bpp(&original).unwrap();
        let after = tim::decode_4bpp(&page).unwrap();
        assert_eq!(
            after.palette(0, 0)[..3],
            [0, 0, 0],
            "index 0 is the black key"
        );
        assert_eq!(after.palette(0, 7), before.palette(0, 0));
        let swapped: Vec<u8> = before
            .indices
            .iter()
            .map(|&index| match index {
                0 => 7,
                7 => 0,
                index => index,
            })
            .collect();
        assert_eq!(after.indices, swapped);
        // Every texel keeps its colour, so the HD lookup is unchanged.
        let colours = |texture: &crate::model::Texture8| -> Vec<[u8; 4]> {
            texture
                .indices
                .iter()
                .map(|&index| texture.palette(0, index))
                .collect()
        };
        assert_eq!(colours(&after), colours(&before));
        let hashes = |texture: &crate::model::Texture8| -> Vec<u32> {
            crate::htexture::page_candidates(texture, 0)
                .iter()
                .map(|candidate| candidate.hash)
                .collect()
        };
        assert_eq!(hashes(&after), hashes(&before));

        // A page whose index 0 is already black, or without a black entry,
        // is left alone.
        let mut keyed = page.clone();
        key_document_page(&mut keyed);
        assert_eq!(keyed, page);
        let mut no_black = clut;
        no_black[7] = 0x0007;
        let opaque = document_page(no_black, nibbles);
        let mut unchanged = opaque.clone();
        key_document_page(&mut unchanged);
        assert_eq!(unchanged, opaque);
    }

    #[test]
    fn converts_synthetic_text_item_and_file_entries() {
        let root = TempDir::new("text-item-file");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1100.RDT"), [0u8; 4]).unwrap();
        let dir = root.path.join("ITEM_M2");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("I00V.IVM"), b"ivm0").unwrap();
        fs::write(dir.join("MINI.ivm"), b"ivm1").unwrap();
        fs::write(dir.join("FILE000.TIM"), b"cover0").unwrap();
        fs::write(dir.join("FILE001.TIM"), b"cover1").unwrap();
        for number in 1..=FILEI_COUNT {
            fs::write(
                dir.join(format!("FILEI{number:02}.TIM")),
                format!("i{number}"),
            )
            .unwrap();
        }
        for number in 0..TEXTM_COUNT {
            fs::write(
                dir.join(format!("TEXTM_{number:02}.TIM")),
                format!("page{number}"),
            )
            .unwrap();
        }
        for number in 0..=0x0Eu32 {
            fs::write(
                dir.join(format!("MAP{number:02X}.TIM")),
                format!("plan{number}"),
            )
            .unwrap();
        }
        fs::write(dir.join("Map_blue.TIM"), b"blue").unwrap();

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();

        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("text/"), 0, "the text tables are gone for good");
        assert_eq!(count("language/"), crate::language::LANGUAGE_FILES.len());
        for (entry, source) in crate::language::LANGUAGE_FILES {
            assert_eq!(pack.read(entry).unwrap().as_ref(), source.as_bytes());
        }
        assert_eq!(count("item/"), 2);
        assert_eq!(count("file/"), 62);
        assert_eq!(count("map/"), 16, "15 plans and the backdrop");
        assert_eq!(pack.read("item/mini.ivm").unwrap().as_ref(), b"ivm1");
        assert_eq!(pack.read("file/textm_42.tim").unwrap().as_ref(), b"page42");
        assert_eq!(pack.read("map/map0e.tim").unwrap().as_ref(), b"plan14");
        assert_eq!(pack.read("map/blue.tim").unwrap().as_ref(), b"blue");
        assert!(!pack.contains("map/tables.bin"), "no map tables are packed");
        assert!(
            !pack.contains(crate::combat::COMBAT_ENTRY),
            "no combat tables are packed"
        );
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn packs_real_item_and_file_art() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let dir = PathBuf::from(&root).join("JPN/ITEM_M2");

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let mut warnings = Vec::new();
        let (items, items_bytes) = copy_item_models(
            std::slice::from_ref(&dir),
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();
        let (files, files_bytes) = copy_file_art(
            std::slice::from_ref(&dir),
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();

        let (maps, maps_bytes) = copy_map_art(
            std::slice::from_ref(&dir),
            &mut writer,
            &mut progress,
            &mut warnings,
            1,
        )
        .unwrap();

        assert_eq!(items, 77);
        assert_eq!(files, 2 + FILEI_COUNT + TEXTM_COUNT);
        assert_eq!(maps, 16);
        assert!(items_bytes > 0 && files_bytes > 0 && maps_bytes > 0);
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("item/"), 77);
        assert_eq!(count("file/"), 62);
        assert_eq!(count("map/"), 16);
        assert_eq!(
            pack.read("item/i00v.ivm").unwrap(),
            fs::read(dir.join("I00V.IVM")).unwrap()
        );
        assert_eq!(
            pack.read("file/textm_z0.tim").unwrap(),
            fs::read(dir.join("TEXTM_Z0.TIM")).unwrap()
        );
        assert_eq!(
            pack.read("map/map0b.tim").unwrap(),
            fs::read(dir.join("MAP0B.TIM")).unwrap()
        );
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn converts_real_install() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let temp = TempDir::new("real-install");
        let out = temp.path.join("re1.akpak");

        convert_game(&root, &out).unwrap();

        let pack = crate::pack::Pack::open(&out).unwrap();
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("room/"), 348);
        // One cut set per distinct room; stages 6/7 reuse the stage 1/2 pak
        // files (620 distinct paks) but still name their cuts with their own
        // room id, e.g. roomcut/600_000.bmp.
        assert_eq!(count("roomcut/"), 842);
        // One mask page per camera that carries sprite groups; stages 6/7
        // reuse the stage 1/2 pages.
        assert_eq!(count("roommask/"), 601);
        assert_eq!(count("bgm/"), 61);
        assert_eq!(
            count("se/"),
            sfx::SE_NAMES.len() + music::se_track_names().len(),
            "one pack entry per named effect and per non-Bgm group track"
        );
        assert_eq!(count("door/"), 34);
        assert_eq!(
            count("enemy/"),
            15 + 44 + crate::enemy::ENEMY_SCRIPTS.len(),
            "15 character models, 44 monster models and the checked-in scripts"
        );
        assert_eq!(count("effspr/"), 33);
        assert_eq!(count("map/"), 16, "15 plans and the backdrop");
        assert!(!pack.contains("map/tables.bin"), "no map tables are packed");
        assert!(
            !pack.contains(crate::combat::COMBAT_ENTRY),
            "no combat tables are packed"
        );
        assert!(pack.contains("enemy/em20.emd"));
        assert!(pack.contains("enemy/em2e.emd"));
        assert!(pack.contains("enemy/em000.emd"));
        assert!(pack.contains("enemy/em151.emd"));
        assert!(pack.contains("enemy/em13.lua"));
        assert!(pack.contains("effspr/esp000.tim"));
        assert!(pack.contains("effspr/esp224.tim"));
        assert!(pack.contains(crate::effects::room::CORE_ESP_ENTRY));
        assert!(pack.contains(crate::effects::room::CORE_ETM_ENTRY));

        // The global weapon effects parse into eight records with the shipped
        // art geometry (heights and CLUT rows drive the room page cursor).
        let weapon = crate::effects::WeaponEffects::load(&pack);
        assert!(weapon.warnings.is_empty(), "{:?}", weapon.warnings);
        assert_eq!(weapon.index, [5, 9, 12, 17, 0, 14, 8, 11]);
        assert_eq!(weapon.sprites.len(), 8);
        let geometry: Vec<(u16, u8)> = weapon
            .sprites
            .iter()
            .map(|sprite| (sprite.geometry.height, sprite.geometry.clut_rows))
            .collect();
        assert_eq!(
            geometry,
            [
                (64, 3),
                (112, 4),
                (64, 3),
                (16, 2),
                (24, 4),
                (72, 1),
                (24, 4),
                (24, 4),
            ]
        );
        assert!(weapon.sprites.iter().all(|sprite| sprite.tim.is_some()));

        // The weapon pass's shared cursor must reproduce the fixed sheet
        // regions the renderer indexes (page 0 V 0/64/176/240, page 1 V
        // 3/27/99/123).
        let mut cursor = crate::effects::pages::PackCursor::weapon_start();
        let regions: Vec<(u8, u8)> = weapon
            .sprites
            .iter()
            .map(|sprite| {
                let placement =
                    cursor.place(sprite.geometry.height, sprite.geometry.clut_rows.into());
                (placement.page_index(), placement.v)
            })
            .collect();
        assert_eq!(
            regions,
            [
                (0, 0),
                (0, 64),
                (0, 176),
                (0, 240),
                (1, 3),
                (1, 27),
                (1, 99),
                (1, 123)
            ]
        );
        assert!(pack.contains("room/1001.rdt"));
        assert!(pack.contains("roomcut/100_000.bmp"));
        assert!(pack.contains("roommask/100_000.bmp"));
        assert!(pack.contains("bgm/013.wav"));
        assert!(pack.contains("se/ft_wda.wav"));
        assert!(pack.contains("se/dr_wd01.wav"));
        assert!(pack.contains("door/door00.dor"));
        assert!(pack.contains("door/ele01a.dor"));
        assert!(pack.contains("door/kai02.dor"));
        assert!(pack.contains("door/door06k.dor"));

        // Door art is a raw copy of the matched, case-insensitive source.
        assert_eq!(
            pack.read("door/door00.dor").unwrap(),
            std::fs::read(root.join("JPN/ITEM_M1/door00.dor")).unwrap()
        );

        // The converted page must be exactly the `objspr` pak decode.
        let pak = std::fs::read(root.join("JPN/objspr/OSP00000.pak")).unwrap();
        let decoded = lzw::decode(&pak).unwrap();
        let texture = tim::decode_8bpp(&decoded).unwrap();
        let expected = bmp::encode_texture8_to_vec(&texture).unwrap();
        assert_eq!(pack.read("roommask/100_000.bmp").unwrap(), expected);

        // Four character models (Char10..Char13), the four alternate outfits
        // (EM1030..EM1033), the two no-weapon locomotion clips (W00, W10) and
        // the shipped weapon animations/models under `PLAYERS/` (every other
        // `W*.EMW` and `WS*.TMD`). player/01 is Jill, the character that owns
        // ROOM1001.
        assert_eq!(count("player/"), 4 + 4 + 2 + 37);
        for entry in [
            "player/00_alt0.emd",
            "player/01_alt0.emd",
            "player/00_alt1.emd",
            "player/01_alt1.emd",
        ] {
            assert!(pack.contains(entry), "{entry}");
        }
        assert!(pack.contains("player/01.emd"));
        assert!(pack.contains("player/01.emw"));

        let image = bmp::decode(&pack.read("roomcut/100_000.bmp").unwrap()).unwrap();
        assert_eq!((image.width, image.height), (CUT_WIDTH, CUT_HEIGHT));
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn real_rdts_declare_404_parsed_effect_sprites() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let root = PathBuf::from(root).join("JPN");
        let mut declared = 0usize;
        let mut rooms = 0usize;
        for digit in 1..=RoomId::MAX_STAGE {
            let dir = root.join(format!("STAGE{digit}"));
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("rdt"))
                })
                .collect();
            paths.sort();
            for path in paths {
                let name = path
                    .file_stem()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_ascii_uppercase();
                let room = name.trim_start_matches("ROOM");
                let id = RoomId::parse(room).unwrap();
                let data = fs::read(&path).unwrap();
                let state = crate::rdt::parse(&data, id).unwrap();
                if state.effects.sprites.is_empty() {
                    continue;
                }
                rooms += 1;
                declared += state.effects.sprites.len();
                assert!(
                    state.effects.warnings.is_empty(),
                    "{}: {:?}",
                    path.display(),
                    state.effects.warnings
                );
                for sprite in &state.effects.sprites {
                    assert_eq!(sprite.geometry.width, 256, "{}", path.display());
                    assert!(
                        (16..=256).contains(&sprite.geometry.height),
                        "{}: height {}",
                        path.display(),
                        sprite.geometry.height
                    );
                    assert!(
                        (1..=4).contains(&sprite.geometry.clut_rows),
                        "{}: {} CLUT rows",
                        path.display(),
                        sprite.geometry.clut_rows
                    );
                    assert!(sprite.tim.is_some(), "{}", path.display());
                }
            }
        }
        assert_eq!(declared, 404);
        assert_eq!(rooms, 186);
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn real_npc_models_convert_and_parse() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let layout = discover_layout(&root).unwrap();
        let (assets, warnings) = resolve_npc_assets(&layout).unwrap();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(assets.len(), 15);
        assert_eq!(
            assets.iter().map(|asset| asset.id).collect::<Vec<_>>(),
            (enemy::FIRST_ID..=enemy::LAST_ID).collect::<Vec<_>>()
        );

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let (count, bytes) = copy_npc_models(&assets, &mut writer, &mut progress, 1).unwrap();

        assert_eq!(count, 15);
        assert_eq!(bytes, 2_295_340, "total NPC model bytes");
        let size_of = |id: u8| {
            let asset = assets.iter().find(|asset| asset.id == id).unwrap();
            fs::metadata(&asset.source).unwrap().len() as usize
        };
        assert_eq!(size_of(0x29), 93_636, "the devoured corpse is the smallest");
        assert_eq!(size_of(0x22), 210_216, "Barry is the largest");

        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let entries: Vec<&str> = pack
            .paths()
            .filter(|path| path.starts_with("enemy/"))
            .collect();
        assert_eq!(entries.len(), 15, "{entries:?}");

        for asset in &assets {
            let data = pack.read(&asset.entry).unwrap();
            let emd =
                crate::emd::parse(&data).unwrap_or_else(|err| panic!("{}: {err:#}", asset.entry));
            let prims: usize = emd
                .mesh
                .objects
                .iter()
                .map(|object| object.prims.len())
                .sum();
            let expected_clips = match asset.id {
                0x25 | 0x26 | 0x29 => 1,
                0x27 => 3,
                0x28 => 4,
                0x20 => 53,
                0x21 => 49,
                0x22 => 59,
                0x23 => 65,
                0x24 => 64,
                0x2A => 55,
                0x2B => 49,
                0x2C => 52,
                0x2D => 59,
                0x2E => 63,
                other => panic!("unexpected character id {other:#04x}"),
            };
            println!(
                "{} (id {:#04x}): {} object(s), {} clip(s), {} keyframe(s), {prims} primitive(s), {} bytes",
                asset.entry,
                asset.id,
                emd.mesh.objects.len(),
                emd.clips.len(),
                emd.keyframes.len(),
                data.len()
            );
            assert_eq!(emd.clips.len(), expected_clips, "{} clips", asset.entry);
            assert!(
                (15..=16).contains(&emd.mesh.objects.len()),
                "{} objects",
                asset.entry
            );
            assert_eq!(emd.skeleton.relative.len(), 15, "{} joints", asset.entry);
            assert_eq!(
                emd.skeleton.children.len(),
                15,
                "{} child lists",
                asset.entry
            );
            assert!(
                (2..=1032).contains(&emd.keyframes.len()),
                "{} keyframes",
                asset.entry
            );
            assert_eq!(
                (emd.texture.width, emd.texture.height),
                (256, 256),
                "{} texture",
                asset.entry
            );
            assert!((670..=700).contains(&prims), "{} primitives", asset.entry);
        }
    }

    #[test]
    fn base_manifest_renders_and_parses_back() {
        let manifest = base_manifest("ja-JP");
        assert_eq!(
            manifest::Manifest::parse(&manifest.render()).unwrap(),
            manifest
        );

        let mut writer = PackWriter::new();
        let (count, bytes) = copy_manifest(&mut writer, "ja-JP").unwrap();
        assert_eq!(count, 1);
        assert_eq!(bytes, manifest.render().len());
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.manifest(), Some(&manifest));
        assert_eq!(pack.len(), 1);
    }

    #[test]
    fn packs_font_tim_raw() {
        let root = TempDir::new("font-pack");
        make_stage_dirs(&root.path);
        let data = root.path.join("DaTa");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("FONT.TIM"), b"raw-font-sheet").unwrap();

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(
            plan.fonts.primary.as_deref(),
            Some(data.join("FONT.TIM").as_path())
        );
        assert_eq!(plan.fonts.latin, None);
        assert_eq!(plan.fonts.japanese, None);
        assert_eq!(plan.language, "ja-JP");
        // The install carries no Western sheet: a warning, not an error.
        assert!(
            plan.warnings.iter().any(|w| w.contains("fontus.tim")),
            "warnings: {:?}",
            plan.warnings
        );

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let (count, bytes) = copy_font(&plan.fonts, &mut writer, &mut progress).unwrap();

        assert_eq!((count, bytes), (1, 14));
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(
            pack.read("font/font.tim").unwrap().as_ref(),
            b"raw-font-sheet"
        );
    }

    #[test]
    fn packs_the_other_scripts_sheet_beside_the_installs_own() {
        // A data directory carrying both sheets: the Japanese one is the
        // default (the install's own room text is Japanese) and the Western
        // one rides along as its override.
        let root = TempDir::new("font-both");
        make_stage_dirs(&root.path);
        let data = root.path.join("DATA");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("FONT.TIM"), font_tim_bytes(192)).unwrap();
        fs::write(data.join("fontus.tim"), font_tim_bytes(64)).unwrap();

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(
            plan.fonts.primary.as_deref(),
            Some(data.join("FONT.TIM").as_path())
        );
        assert_eq!(
            plan.fonts.latin.as_deref(),
            Some(data.join("fontus.tim").as_path())
        );
        assert_eq!(
            plan.fonts.japanese, None,
            "the primary is the Japanese sheet"
        );
        assert_eq!(plan.language, "ja-JP");

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let (count, _bytes) = copy_font(&plan.fonts, &mut writer, &mut progress).unwrap();
        assert_eq!(count, 2);
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert!(pack.contains("font/font.tim"));
        assert!(pack.contains("font/latin.tim"));
    }

    #[test]
    fn discovers_the_steam_install_root() {
        // The Steam layout: language trees one level down, each a complete
        // game. The walk picks the first tree (English) for the data and
        // packs every sheet the root carries, so the one pack serves both
        // scripts.
        let root = TempDir::new("steam-root");
        let english = root.path.join("english/USA");
        make_stage_dirs(&english);
        fs::create_dir_all(english.join("Data")).unwrap();
        fs::write(english.join("Data/fontus.tim"), font_tim_bytes(64)).unwrap();
        let japanese = root.path.join("japanese/JPN");
        make_stage_dirs(&japanese);
        fs::create_dir_all(japanese.join("DATA")).unwrap();
        fs::write(japanese.join("DATA/FONT.TIM"), font_tim_bytes(192)).unwrap();

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(
            plan.fonts.primary.as_deref(),
            Some(english.join("Data/fontus.tim").as_path())
        );
        assert_eq!(
            plan.fonts.japanese.as_deref(),
            Some(japanese.join("DATA/FONT.TIM").as_path())
        );
        assert_eq!(plan.fonts.latin, None, "the primary is the Western sheet");
        assert_eq!(plan.language, "en-US");

        let mut writer = PackWriter::new();
        let mut progress = Progress::new("test", 1);
        let (count, _bytes) = copy_font(&plan.fonts, &mut writer, &mut progress).unwrap();
        assert_eq!(count, 2);
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert!(pack.contains("font/font.tim"));
        assert!(pack.contains("font/jpn.tim"));
    }
}
