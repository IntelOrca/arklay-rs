//! `convert-game`: migrate a full game installation into an `.akpak` pack.
//!
//! Discovers `STAGE1`..`STAGE7`, `ENEMY`, `PLAYERS`, `sound`, `objspr`,
//! `ITEM_M1`, `effspr` and `DATA` (case-insensitively, up to two levels below
//! the root), stores every `ROOM####.RDT`, converts the camera backgrounds of
//! every distinct room once, converts the room mask pages of every camera that
//! has sprite groups, copies the door animations named by the door type table,
//! copies the `BGM_*.WAV` music files, the 68 named sound effects, the four
//! player models plus the two no-weapon locomotion clips, the fifteen scripted
//! character (NPC) models, the 33 effect-sheet TIMs, the `core00`
//! weapon-effect metadata and the `KAGE.TIM` player-shadow coverage page.
//! Stages 6 and 7 reuse the
//! backgrounds and mask pages of STAGE1/STAGE2 with the stage digit reduced
//! by 5.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};

use crate::items;
use crate::model::Texture8;
use crate::music;
use crate::npc;
use crate::pack::PackWriter;
use crate::progress::{Progress, format_duration};
use crate::sfx;
use crate::state::{Image, RoomId};
use crate::text;
use crate::voice;
use crate::{bmp, door, lzw, rdt, tim};

/// How many directory levels below the root are searched for the stage and
/// `sound` directories.
const MAX_DEPTH: usize = 2;

/// Expected camera background width.
const CUT_WIDTH: u32 = 320;
/// Expected camera background height.
const CUT_HEIGHT: u32 = 240;

/// Byte offset of the bit depth field in a BMP header.
const BMP_BPP_OFFSET: usize = 28;

/// Bytes buffered while streaming the finished pack to disk.
const WRITE_CHUNK: usize = 1 << 20;

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
    progress.begin(label, files.len() as u64, "files");
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

/// Convert the game installation under `root` into the `.akpak` pack `out`.
///
/// The text tables are extracted from a `Bio.exe` discovered under `root`; use
/// [`convert_game_with_exe`] to point at an executable elsewhere.
pub fn convert_game(root: &Path, out: &Path) -> Result<()> {
    convert_game_with_exe(root, out, None)
}

/// Convert the game installation under `root` into the `.akpak` pack `out`,
/// taking the text tables from `exe` instead of the discovered `Bio.exe`.
pub fn convert_game_with_exe(root: &Path, out: &Path, exe: Option<&Path>) -> Result<()> {
    convert_game_with_options(root, out, exe, 0)
}

/// How `convert-game` distributes the referenced voice WAVs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoicePackOptions {
    /// Write a second v1 pack (default: `<out stem>.voice.akpak`, or the
    /// given explicit path).
    Sibling(Option<PathBuf>),
    /// Embed the voice entries in the main pack for a single-file install.
    Embed,
    /// Do not pack voice at all.
    Skip,
}

impl Default for VoicePackOptions {
    fn default() -> Self {
        Self::Sibling(None)
    }
}

/// The default sibling voice-pack path for a main pack: `<stem>.voice.akpak`.
pub fn voice_pack_path(out: &Path) -> PathBuf {
    let stem = out
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("re1");
    out.with_file_name(format!("{stem}.voice.akpak"))
}

/// Like [`convert_game_with_exe`], but with an explicit worker count.
///
/// A `jobs` of `0` selects one worker per available CPU, and the output is
/// byte-identical whatever the worker count.
pub fn convert_game_with_options(
    root: &Path,
    out: &Path,
    exe: Option<&Path>,
    jobs: usize,
) -> Result<()> {
    convert_game_with_voice(root, out, exe, jobs, &VoicePackOptions::default())
}

/// [`convert_game_with_options`] with the voice-pack distribution selected.
pub fn convert_game_with_voice(
    root: &Path,
    out: &Path,
    exe: Option<&Path>,
    jobs: usize,
    voice_options: &VoicePackOptions,
) -> Result<()> {
    let jobs = worker_count(jobs);
    let mut progress = Progress::new();
    let Plan {
        rooms,
        sound,
        item_m1,
        item_m2,
        players,
        npc,
        roommask,
        effects,
        data,
        font,
        voice,
        voice_unreferenced,
        voice_dir_found,
        mut warnings,
    } = build_plan_with_progress(root, jobs, &mut progress)?;
    let exe = match exe {
        Some(path) if path.is_file() => Some(path.to_path_buf()),
        Some(path) => {
            warnings.push(format!(
                "executable {} does not exist; text tables will be missing",
                path.display()
            ));
            None
        }
        None => match discover_exe(root)? {
            Some(path) => Some(path),
            None => {
                warnings.push("no Bio.exe found; text tables will be missing".to_string());
                None
            }
        },
    };
    for warning in &warnings {
        println!("warning: {warning}");
    }
    let voice_enabled = !matches!(voice_options, VoicePackOptions::Skip);
    if voice_enabled && !voice_dir_found {
        println!(
            "warning: no voice directory found; {} voice line(s) will be missing",
            voice::referenced_names().len()
        );
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
    progress.begin("room", rdt_total, "files");
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

    progress.begin("roomcut", cut_total, "cuts");
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
    progress.begin("roommask", roommask.len() as u64, "files");
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
    // TODO(parity): (conversion) the original installs more than this pack
    // carries: the FMV AVIs and the held-weapon TMDs under
    // `players/ws*.tmd`. Those systems are unimplemented, so the conversion is
    // complete only for the modelled categories; add their copy phases when
    // the runtime grows them.
    let (door_count, door_bytes) = copy_doors(&item_m1, &mut writer, &mut progress, jobs)?;
    let (player_count, player_bytes) = copy_players(&players, &mut writer, &mut progress, jobs)?;
    let (npc_count, npc_bytes) = copy_npc_models(&npc, &mut writer, &mut progress, jobs)?;
    let (effect_count, effect_bytes) =
        copy_effect_sheets(&effects, &mut writer, &mut progress, jobs)?;
    let (font_count, font_bytes) = copy_font(font.as_deref(), &mut writer, &mut progress)?;
    let (ui_count, ui_bytes) = copy_ui_art(&data, &mut writer, &mut progress, jobs)?;
    let (item_count, item_bytes) = copy_item_art(&data, &mut writer, &mut progress, jobs)?;
    let (data_count, data_bytes) = copy_bio_card(&data, &mut writer, &mut progress)?;
    let (core_count, core_bytes) = copy_core_effects(&data, &mut writer, &mut progress, jobs)?;
    let (shadow_count, shadow_bytes) = copy_shadow(&data, &mut writer, &mut progress)?;
    let (text_count, text_bytes) = copy_text(exe.as_deref(), &mut writer, &mut progress)?;
    let (ivm_count, ivm_bytes) =
        copy_item_models(item_m2.as_deref(), &mut writer, &mut progress, jobs)?;
    let (file_count, file_bytes) =
        copy_file_art(item_m2.as_deref(), &mut writer, &mut progress, jobs)?;

    // Voice: `--with-voice` embeds the entries in the main pack; the default
    // writes a second plain v1 pack now, before the main pack streams.
    let embed_voice =
        voice_enabled && matches!(voice_options, VoicePackOptions::Embed) && !voice.is_empty();
    let (voice_count, voice_bytes, voice_note) = if !voice_enabled || voice.is_empty() {
        (0, 0, String::new())
    } else if embed_voice {
        let (count, bytes) = copy_voice(&voice, &mut writer, &mut progress, jobs)?;
        (count, bytes, " (embedded)".to_string())
    } else {
        let path = match voice_options {
            VoicePackOptions::Sibling(Some(path)) => path.clone(),
            _ => voice_pack_path(out),
        };
        let mut voice_writer = PackWriter::new();
        let (count, bytes) = copy_voice(&voice, &mut voice_writer, &mut progress, jobs)?;
        let voice_size = voice_writer.pack_size()?;
        voice_writer
            .write(&path)
            .with_context(|| format!("failed to write voice pack {}", path.display()))?;
        (
            count,
            bytes,
            format!(" -> {} ({voice_size} bytes)", path.display()),
        )
    };

    for (index, (rdts, cuts)) in stage_counts.iter().enumerate() {
        println!("STAGE{}: {rdts} RDT(s), {cuts} cut(s)", index + 1);
    }
    println!("room: {rdt_count} entries, {rdt_bytes} bytes");
    println!("roomcut: {cut_count} entries, {cut_bytes} bytes");
    println!("roommask: {mask_count} entries, {mask_bytes} bytes");
    println!("bgm: {bgm_count} entries, {bgm_bytes} bytes");
    println!("se: {se_count} entries, {se_bytes} bytes");
    println!("door: {door_count} entries, {door_bytes} bytes");
    println!("player: {player_count} entries, {player_bytes} bytes");
    println!("npc: {npc_count} entries, {npc_bytes} bytes");
    println!("effspr: {effect_count} entries, {effect_bytes} bytes");
    println!("font: {font_count} entries, {font_bytes} bytes");
    println!("ui: {ui_count} entries, {ui_bytes} bytes");
    println!("item: {item_count} entries, {item_bytes} bytes");
    println!("data: {data_count} entries, {data_bytes} bytes");
    println!("core00: {core_count} entries, {core_bytes} bytes");
    println!("shadow: {shadow_count} entries, {shadow_bytes} bytes");
    println!("text: {text_count} entries, {text_bytes} bytes");
    println!("ivm: {ivm_count} entries, {ivm_bytes} bytes");
    println!("file: {file_count} entries, {file_bytes} bytes");
    if voice_enabled {
        println!("voice: {voice_count} entries, {voice_bytes} bytes{voice_note}");
        if !voice_unreferenced.is_empty() {
            println!(
                "voice: {} unreferenced file(s) not packed: {}",
                voice_unreferenced.len(),
                voice_unreferenced.join(", ")
            );
        }
    }

    // Stream the pack straight to disk: the entry data is already in memory,
    // so materializing a second serialized copy would only cost memory and a
    // full memcpy.
    let size = writer.pack_size()?;
    progress.begin("write", size as u64, "bytes");
    let file =
        fs::File::create(out).with_context(|| format!("failed to create {}", out.display()))?;
    let mut file = ProgressWriter {
        inner: BufWriter::with_capacity(WRITE_CHUNK, file),
        progress: &mut progress,
    };
    writer
        .stream_to(&mut file)
        .with_context(|| format!("failed to write {}", out.display()))?;
    file.flush()
        .with_context(|| format!("failed to write {}", out.display()))?;
    drop(file);
    progress.end_phase();

    let entries = rdt_count
        + cut_count
        + mask_count
        + bgm_count
        + se_count
        + door_count
        + player_count
        + npc_count
        + effect_count
        + ui_count
        + item_count
        + data_count
        + core_count
        + shadow_count
        + font_count
        + text_count
        + ivm_count
        + file_count
        + if embed_voice { voice_count } else { 0 };
    println!(
        "wrote {} ({entries} entries, {size} bytes) in {}",
        out.display(),
        format_duration(progress.elapsed())
    );
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
/// The entries are the referenced union of the per-stage name rows; the
/// shipped-but-unreferenced files (`ANNOUNCE`, the `V111_*` group and the
/// variants) are deliberately left out and listed in the summary.
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
    progress.begin("npc", assets.len() as u64, "files");
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
    progress.begin("ui", data.ui.len() as u64, "files");
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

    progress.begin("item", data.items.len() as u64, "files");
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
    progress.begin("data", 1, "files");
    let bytes = raw.len();
    writer
        .add(BIO_CARD_ENTRY, raw)
        .with_context(|| format!("failed to add {BIO_CARD_ENTRY}"))?;
    progress.advance(BIO_CARD_ENTRY);
    progress.end_phase();
    Ok((1, bytes))
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
    progress.begin("shadow", 1, "files");
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

/// JPN virtual addresses and entry counts of the executable text tables.
const TEXT_MESSAGES_VA: u32 = 0x4CDE58;
const TEXT_MESSAGES_COUNT: usize = 64;
const TEXT_NAMES_VA: u32 = 0x4CD388;
const TEXT_NAMES_COUNT: usize = 128;
const TEXT_UNKNOWN_VA: u32 = 0x4CD548;
const TEXT_UNKNOWN_COUNT: usize = 16;
const TEXT_DESCRIPTIONS_VA: u32 = 0x4C9370;
const TEXT_DESCRIPTIONS_COUNT: usize = 79;

/// JPN virtual addresses of the save-screen string block.
const SAVE_HEADER_TABLE_VA: u32 = 0x4B0FF0;
const SAVE_HEADER_SUFFIX_VA: u32 = 0x4B0FD0;
const SAVE_EXIT_TABLE_VA: u32 = 0x4B1008;
const SAVE_EXIT_SUFFIX_VA: u32 = 0x4B0FF8;
const SAVE_CHAR_TABLE_VA: u32 = 0x4B1020;
const SAVE_FILLED_SLOT_VA: u32 = 0x4B0FA0;
const SAVE_EMPTY_SLOT_VA: u32 = 0x4B0FC0;
const SAVE_OVERWRITE_VA: u32 = 0x4B1130;
const SAVE_YES_NO_VA: u32 = 0x4B1148;
const SAVE_ERROR_1_VA: u32 = 0x4B1028;
const SAVE_ERROR_2_VA: u32 = 0x4B1040;
const SAVE_LOCATION_TABLE_VA: u32 = 0x4B1110;
const SAVE_LOCATION_COUNT: usize = 7;

/// Shipped item-view model and document-art counts.
const IVM_COUNT: usize = 77;
const FILEI_COUNT: usize = 17;
const TEXTM_COUNT: usize = 43;

/// How one extracted text stream is terminated.
#[derive(Debug, Clone, Copy)]
enum StreamKind {
    /// Message grammar: `0x01` terminator plus the trailing action byte.
    Message,
    /// Item name: `0x07` terminator.
    Name,
    /// Plain `0x01` terminator with no action byte (save strings).
    Plain,
}

/// One PE section: the virtual range and the file bytes backing it.
#[derive(Debug, Clone, Copy)]
struct PeSection {
    virtual_address: u32,
    virtual_size: u32,
    raw_offset: u32,
    raw_size: u32,
}

/// A parsed PE image: the image base and section table used to map the
/// executable's virtual addresses to file offsets.
struct PeImage<'a> {
    data: &'a [u8],
    image_base: u32,
    sections: Vec<PeSection>,
}

impl<'a> PeImage<'a> {
    /// Parse the DOS stub, PE signature, optional header and section table.
    fn parse(data: &'a [u8]) -> Result<Self> {
        let dos = data
            .get(0..0x40)
            .context("executable is too short for a DOS header")?;
        if &dos[0..2] != b"MZ" {
            bail!("executable has no MZ signature");
        }
        let pe = read_u32_at(data, 0x3C)? as usize;
        let signature = data
            .get(pe..pe + 4)
            .context("executable is too short for its PE signature")?;
        if signature != b"PE\0\0" {
            bail!("executable has no PE signature at 0x{pe:X}");
        }
        let coff = data
            .get(pe + 4..pe + 24)
            .context("executable COFF header is truncated")?;
        let section_count = usize::from(u16::from_le_bytes([coff[2], coff[3]]));
        let optional_size = usize::from(u16::from_le_bytes([coff[16], coff[17]]));
        let optional = data
            .get(pe + 24..pe + 24 + optional_size)
            .context("executable optional header is truncated")?;
        let magic = optional
            .get(0..2)
            .map(|raw| u16::from_le_bytes([raw[0], raw[1]]))
            .context("executable optional header is missing its magic")?;
        if magic != 0x10B {
            bail!("unsupported PE optional header magic 0x{magic:04X}; expected PE32 (0x010B)");
        }
        let image_base = optional
            .get(28..32)
            .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
            .context("executable optional header is missing its image base")?;

        let table_start = pe + 24 + optional_size;
        let table = data
            .get(table_start..table_start + section_count * 40)
            .with_context(|| {
                format!("section table for {section_count} section(s) is truncated")
            })?;
        let sections = table
            .as_chunks::<40>()
            .0
            .iter()
            .map(|entry| PeSection {
                virtual_size: u32::from_le_bytes(entry[8..12].try_into().unwrap()),
                virtual_address: u32::from_le_bytes(entry[12..16].try_into().unwrap()),
                raw_size: u32::from_le_bytes(entry[16..20].try_into().unwrap()),
                raw_offset: u32::from_le_bytes(entry[20..24].try_into().unwrap()),
            })
            .collect();
        Ok(Self {
            data,
            image_base,
            sections,
        })
    }

    /// Map a virtual address to a file offset. An address in a section's
    /// zero-filled tail is not backed by file bytes and maps to `None`.
    fn va_to_offset(&self, va: u32) -> Option<usize> {
        let rva = va.checked_sub(self.image_base)?;
        for section in &self.sections {
            let Some(delta) = rva.checked_sub(section.virtual_address) else {
                continue;
            };
            if delta >= section.virtual_size.max(section.raw_size) {
                continue;
            }
            if delta >= section.raw_size {
                return None;
            }
            let offset = section.raw_offset.checked_add(delta)? as usize;
            return (offset < self.data.len()).then_some(offset);
        }
        None
    }

    /// Read a little-endian `u32` at a virtual address.
    fn u32_at_va(&self, va: u32) -> Result<u32> {
        let offset = self
            .va_to_offset(va)
            .with_context(|| format!("virtual address 0x{va:08X} is not in the image"))?;
        read_u32_at(self.data, offset)
    }
}

/// Extract the five executable text tables, in pack-entry order.
fn extract_text_tables(data: &[u8]) -> Result<Vec<(&'static str, Vec<u8>)>> {
    let image = PeImage::parse(data)?;
    let messages = extract_pointer_table(
        &image,
        TEXT_MESSAGES_VA,
        TEXT_MESSAGES_COUNT,
        StreamKind::Message,
    )
    .context("failed to extract the global message table")?;
    let names = extract_pointer_table(&image, TEXT_NAMES_VA, TEXT_NAMES_COUNT, StreamKind::Name)
        .context("failed to extract the item-name table")?;
    let unknown = extract_pointer_table(
        &image,
        TEXT_UNKNOWN_VA,
        TEXT_UNKNOWN_COUNT,
        StreamKind::Name,
    )
    .context("failed to extract the generic-name table")?;
    let descriptions = extract_pointer_table(
        &image,
        TEXT_DESCRIPTIONS_VA,
        TEXT_DESCRIPTIONS_COUNT,
        StreamKind::Message,
    )
    .context("failed to extract the item-description table")?;
    let save = extract_save_strings(&image).context("failed to extract the save-screen strings")?;

    Ok(vec![
        (text::MESSAGES_ENTRY, text::encode_table(&messages)),
        (text::NAMES_ENTRY, text::encode_table(&names)),
        (text::UNKNOWN_ENTRY, text::encode_table(&unknown)),
        (text::DESCRIPTIONS_ENTRY, text::encode_table(&descriptions)),
        (text::SAVE_ENTRY, text::encode_table(&save)),
    ])
}

/// Follow a pointer table at `va` and extract every pointed-at stream.
///
/// A null or unmapped pointer, like the trailing slot of the global message
/// table, becomes a missing entry.
fn extract_pointer_table(
    image: &PeImage,
    va: u32,
    count: usize,
    kind: StreamKind,
) -> Result<Vec<Option<Vec<u8>>>> {
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let pointer_va = va
            .checked_add(index as u32 * 4)
            .context("text pointer table address overflows")?;
        let pointer = image.u32_at_va(pointer_va).with_context(|| {
            format!("text pointer table slot {index} at 0x{pointer_va:08X} is unreadable")
        })?;
        entries.push(read_stream(image, pointer, kind)?);
    }
    Ok(entries)
}

/// Extract one `0x01`/`0x07`-terminated stream at `pointer`, if it is mapped.
fn read_stream(image: &PeImage, pointer: u32, kind: StreamKind) -> Result<Option<Vec<u8>>> {
    if pointer == 0 {
        return Ok(None);
    }
    let Some(offset) = image.va_to_offset(pointer) else {
        return Ok(None);
    };
    let stream = &image.data[offset..];
    let end = match kind {
        StreamKind::Name => text::scan_terminator(stream, 0x07),
        StreamKind::Plain => text::scan_terminator(stream, 0x01),
        StreamKind::Message => text::scan_message(stream),
    }
    .with_context(|| format!("unterminated text stream at VA 0x{pointer:08X}"))?;
    let end = if matches!(kind, StreamKind::Message) {
        end.checked_add(1)
            .filter(|&end| end <= stream.len())
            .with_context(|| format!("text stream at VA 0x{pointer:08X} has no action byte"))?
    } else {
        end
    };
    Ok(Some(stream[..end].to_vec()))
}

/// Extract the save-screen strings in their documented order: both headers,
/// the header suffix, both character names, both exit verbs, the exit suffix,
/// the filled and empty slot rows, the confirmation lines and the seven
/// location names.
fn extract_save_strings(image: &PeImage) -> Result<Vec<Option<Vec<u8>>>> {
    let headers = extract_pointer_table(image, SAVE_HEADER_TABLE_VA, 2, StreamKind::Plain)?;
    let chars = extract_pointer_table(image, SAVE_CHAR_TABLE_VA, 2, StreamKind::Plain)?;
    let exits = extract_pointer_table(image, SAVE_EXIT_TABLE_VA, 2, StreamKind::Plain)?;
    let locations = extract_pointer_table(
        image,
        SAVE_LOCATION_TABLE_VA,
        SAVE_LOCATION_COUNT,
        StreamKind::Plain,
    )?;

    let mut entries = vec![
        headers[0].clone(),
        headers[1].clone(),
        read_stream(image, SAVE_HEADER_SUFFIX_VA, StreamKind::Plain)?,
        chars[0].clone(),
        chars[1].clone(),
        exits[0].clone(),
        exits[1].clone(),
        read_stream(image, SAVE_EXIT_SUFFIX_VA, StreamKind::Plain)?,
        read_stream(image, SAVE_FILLED_SLOT_VA, StreamKind::Plain)?,
        read_stream(image, SAVE_EMPTY_SLOT_VA, StreamKind::Plain)?,
        read_stream(image, SAVE_OVERWRITE_VA, StreamKind::Plain)?,
        read_stream(image, SAVE_YES_NO_VA, StreamKind::Plain)?,
        read_stream(image, SAVE_ERROR_1_VA, StreamKind::Plain)?,
        read_stream(image, SAVE_ERROR_2_VA, StreamKind::Plain)?,
    ];
    entries.extend(locations);
    Ok(entries)
}

/// Extract the executable's text tables into the pack.
///
/// A missing or unreadable executable is a warning: room messages still work
/// and menu strings read empty.
fn copy_text(
    exe: Option<&Path>,
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    let Some(path) = exe else {
        return Ok((0, 0));
    };
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            println!(
                "warning: failed to read executable {}: {error}; text tables will be missing",
                path.display()
            );
            return Ok((0, 0));
        }
    };
    let tables = match extract_text_tables(&data) {
        Ok(tables) => tables,
        Err(error) => {
            println!(
                "warning: failed to extract text from {}: {error:#}; text tables will be missing",
                path.display()
            );
            return Ok((0, 0));
        }
    };

    progress.begin("text", tables.len() as u64, "files");
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (entry, data) in tables {
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

/// Add every `ITEM_M2/*.IVM` item-view model to the pack under `item/`.
///
/// The pack stores the canonical lower-case file name; the install's names are
/// matched case-insensitively.
fn copy_item_models(
    item_m2: Option<&Path>,
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let Some(dir) = item_m2 else {
        println!("warning: no ITEM_M2 directory found; item models will be missing");
        return Ok((0, 0));
    };

    let mut files = Vec::new();
    for entry in read_dir_sorted(dir)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.to_ascii_lowercase().ends_with(".ivm") {
            continue;
        }
        files.push((format!("item/{}", name.to_ascii_lowercase()), entry.path()));
    }
    if files.len() != IVM_COUNT {
        println!(
            "warning: found {} item model(s) in {}, expected {IVM_COUNT}",
            files.len(),
            dir.display()
        );
    }

    copy_raw_files(files, "item", writer, progress, jobs)
}

/// Add the document-reader art to the pack under `file/`: the `FILE` covers,
/// the `FILEI` backdrops and every `TEXTM_*.TIM` page, copied raw with
/// lower-case names.
fn copy_file_art(
    item_m2: Option<&Path>,
    writer: &mut PackWriter,
    progress: &mut Progress,
    jobs: usize,
) -> Result<(usize, usize)> {
    let Some(dir) = item_m2 else {
        println!("warning: no ITEM_M2 directory found; file art will be missing");
        return Ok((0, 0));
    };

    let index = index_dir(dir)?;
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
        println!(
            "warning: missing {} document art file(s) in {}: {}",
            missing.len(),
            dir.display(),
            missing.join(", ")
        );
    }

    let mut pages: Vec<&String> = index
        .keys()
        .filter(|name| name.starts_with("textm_") && name.ends_with(".tim"))
        .collect();
    pages.sort();
    if pages.len() != TEXTM_COUNT {
        println!(
            "warning: found {} TEXTM page(s) in {}, expected {TEXTM_COUNT}",
            pages.len(),
            dir.display()
        );
    }
    for name in pages {
        files.push((format!("file/{name}"), index[name].clone()));
    }

    copy_raw_files(files, "file", writer, progress, jobs)
}

/// Add `DATA/FONT.TIM` raw as `font/font.tim`; the JPN sheet is 4bpp and
/// decoded by the engine, so no conversion is needed.
fn copy_font(
    font: Option<&Path>,
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    let Some(font) = font else {
        return Ok((0, 0));
    };
    let data = fs::read(font).with_context(|| format!("failed to read {}", font.display()))?;
    let bytes = data.len();
    writer
        .add("font/font.tim", data)
        .context("failed to add font/font.tim")?;
    progress.begin("font", 1, "files");
    progress.advance("font/font.tim");
    progress.end_phase();
    Ok((1, bytes))
}

/// Stage, sound, enemy-model, player, room-mask, door-art, data and item-view
/// directory roots discovered under the conversion root.
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
    effspr: Option<PathBuf>,
    voice: Option<PathBuf>,
}

/// Breadth-first, case-insensitive discovery of `STAGE1`..`STAGE7`, `sound`,
/// `enemy`, `players`, `objspr`, `ITEM_M1`, `ITEM_M2`, `data`, `effspr` and
/// `voice`.
fn discover_layout(root: &Path) -> Result<Layout> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut stages: BTreeMap<u8, PathBuf> = BTreeMap::new();
    let mut sound = None;
    let mut enemy = None;
    let mut players = None;
    let mut objspr = None;
    let mut item_m1 = None;
    let mut data = None;
    let mut item_m2 = None;
    let mut effspr = None;
    let mut voice = None;

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
            } else if item_m2.is_none() && name.eq_ignore_ascii_case("item_m2") {
                item_m2 = Some(dir.clone());
            } else if effspr.is_none() && name.eq_ignore_ascii_case("effspr") {
                effspr = Some(dir.clone());
            } else if voice.is_none() && name.eq_ignore_ascii_case("voice") {
                voice = Some(dir.clone());
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
        item_m2,
        effspr,
        voice,
    })
}

/// Find the game executable `Bio.exe` (case-insensitive) up to `MAX_DEPTH`
/// directory levels below `root`.
fn discover_exe(root: &Path) -> Result<Option<PathBuf>> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        for entry in read_dir_sorted(&dir)? {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                if depth < MAX_DEPTH {
                    queue.push_back((entry.path(), depth + 1));
                }
            } else if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("bio.exe"))
            {
                return Ok(Some(entry.path()));
            }
        }
    }
    Ok(None)
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

/// One scripted-character (NPC) model resolved to its pack entry.
#[derive(Debug)]
struct NpcAsset {
    /// Entity id (`0x20..=0x2E`).
    id: u8,
    /// Pack entry, e.g. `npc/23.emd`.
    entry: String,
    /// Source file in the installation.
    source: PathBuf,
}

/// One voice WAV resolved to its pack entry.
#[derive(Debug)]
struct VoiceAsset {
    /// Pack entry, e.g. `voice/v004_00.wav`.
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
    item_m2: Option<PathBuf>,
    players: Vec<PlayerAsset>,
    /// The fifteen scripted-character models, resolved by entity id.
    npc: Vec<NpcAsset>,
    roommask: Vec<RoomMask>,
    /// The 33 effect-sheet TIMs, resolved by shipped name.
    effects: Vec<EffectSheet>,
    /// Resolved `DATA` UI, item and save-prefix assets.
    data: DataPlan,
    /// `DATA/FONT.TIM` to pack raw as `font/font.tim`.
    font: Option<PathBuf>,
    /// The referenced voice WAVs (the union of the name rows).
    voice: Vec<VoiceAsset>,
    /// Shipped voice files no row references; listed in the summary.
    voice_unreferenced: Vec<String>,
    /// Whether the install has a `voice` directory at all.
    voice_dir_found: bool,
    /// Non-fatal problems found while resolving optional inputs.
    warnings: Vec<String>,
}

/// Discover, enumerate, and validate every conversion input.
#[cfg(test)]
fn build_plan(root: &Path) -> Result<Plan> {
    build_plan_with_progress(root, 1, &mut Progress::new())
}

/// Discover, enumerate, and validate every conversion input, reporting the RDT
/// read/parse scan on `progress` and using `jobs` workers for it.
fn build_plan_with_progress(root: &Path, jobs: usize, progress: &mut Progress) -> Result<Plan> {
    let layout = discover_layout(root)?;
    let (npc, npc_warnings) = resolve_npc_assets(&layout)?;
    let Layout {
        stages,
        sound,
        enemy,
        players,
        objspr,
        item_m1,
        data,
        item_m2,
        effspr,
        voice: voice_dir,
    } = layout;
    // The font is optional and only diagnosable when the install actually has
    // a DATA directory; a missing DATA root is not reported so partial trees
    // stay warning-free.
    let (font, font_warning) = match data.as_deref() {
        Some(dir) => match index_dir(dir)?.get("font.tim") {
            Some(path) => (Some(path.clone()), None),
            None => (
                None,
                Some(format!("missing DATA/FONT.TIM in {}", dir.display())),
            ),
        },
        None => (None, None),
    };
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

    progress.begin("scan", rdt_files.len() as u64, "files");
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

    // Voice is a second optional pack: every referenced name is resolved
    // case-insensitively, missing files are aggregated into one warning, and
    // the shipped-but-unreferenced files are listed in the summary. A missing
    // `voice` directory is only reported by the conversion summary, so a
    // partial tree stays warning-free like the other optional categories.
    let voice_index = voice_dir.as_deref().map(index_dir).transpose()?;
    let mut voice_assets = Vec::new();
    let mut voice_unreferenced = Vec::new();
    if let (Some(dir), Some(index)) = (voice_dir.as_deref(), voice_index.as_ref()) {
        let referenced: HashSet<String> = voice::referenced_names()
            .into_iter()
            .map(|name| format!("{}.wav", name.to_ascii_lowercase()))
            .collect();
        let mut missing_voice = Vec::new();
        for name in voice::referenced_names() {
            let file = format!("{}.wav", name.to_ascii_lowercase());
            match index.get(&file) {
                Some(source) => voice_assets.push(VoiceAsset {
                    entry: format!("voice/{file}"),
                    source: source.clone(),
                }),
                None => missing_voice.push(format!("{}.WAV", name.to_ascii_uppercase())),
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

    warnings.extend(data.warnings.iter().cloned());
    warnings.extend(npc_warnings);
    if let Some(font_warning) = font_warning {
        warnings.push(font_warning);
    }
    Ok(Plan {
        rooms: rooms.into_values().collect(),
        sound,
        item_m1,
        item_m2,
        players: resolve_players(enemy.as_deref(), players.as_deref())?,
        npc,
        roommask,
        effects,
        data,
        font,
        voice: voice_assets,
        voice_unreferenced,
        voice_dir_found: voice_dir.is_some(),
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
    for id in npc::FIRST_ID..=npc::LAST_ID {
        let file = format!("em10{id:02x}.emd");
        match index.as_ref().and_then(|index| index.get(&file)) {
            Some(source) => assets.push(NpcAsset {
                id,
                entry: npc::model_path(id)
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

/// Read a little-endian `u32` from `data`.
fn read_u32_at(data: &[u8], offset: usize) -> Result<u32> {
    let raw = data
        .get(offset..offset + 4)
        .with_context(|| format!("read of 4 byte(s) at offset 0x{offset:X} is out of bounds"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RDT_HEADER_LEN: usize = 0x94;
    const RDT_CAMERA_LEN: usize = 44;

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
        fs::write(players.join("W00.EMW"), b"emw0").unwrap();
        fs::write(players.join("w10.emw"), b"emw1").unwrap();
    }

    /// The fifteen scripted-character models, with names like the shipped
    /// install.
    fn write_npc_files(root: &Path) {
        let enemy = root.join("ENEMY");
        fs::create_dir_all(&enemy).unwrap();
        for id in npc::FIRST_ID..=npc::LAST_ID {
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
            let id = npc::FIRST_ID + offset as u8;
            assert_eq!(asset.id, id);
            assert_eq!(asset.entry, format!("npc/{id:02x}.emd"));
        }

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("npc/"), 15);
        assert_eq!(pack.read("npc/23.emd").unwrap(), b"npc-23");
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
        let mut progress = Progress::new();
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
        assert_eq!(pack.read("effspr/esp224.tim").unwrap(), b"esp224");
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
        let mut progress = Progress::new();
        let (count, bytes) = copy_core_effects(&data_plan, &mut writer, &mut progress, 1).unwrap();
        assert_eq!(count, 2);
        assert_eq!(bytes, "esp-bytes".len() + "etm-bytes".len());
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(
            pack.read(crate::effects::room::CORE_ESP_ENTRY).unwrap(),
            b"esp-bytes"
        );
        assert_eq!(
            pack.read(crate::effects::room::CORE_ETM_ENTRY).unwrap(),
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
        let mut progress = Progress::new();
        let (count, _) = copy_core_effects(&plan, &mut writer, &mut progress, 1).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn missing_npc_models_warn_once_without_failing() {
        let root = TempDir::new("missing-npc");
        make_stage_dirs(&root.path);

        // No NPC files at all: one aggregated warning, and the conversion
        // still succeeds with no `npc/` entries.
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
        assert!(!pack.paths().any(|path| path.starts_with("npc/")));

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
        assert_eq!(pack.read("se/ft_wda.wav").unwrap(), b"ft_wdA");

        assert_eq!(pack.read("door/door00.dor").unwrap(), b"dor-door00");
        assert_eq!(pack.read("door/kai03.dor").unwrap(), b"dor-kai03");
        assert_eq!(pack.read("door/lad01.dor").unwrap(), b"dor-lad01");

        assert_eq!(pack.read("player/01.emd").unwrap(), b"emd1");
        assert_eq!(pack.read("player/01.emw").unwrap(), b"emw1");
        // The NPC phase copies the fifteen character models raw, lower-cased.
        assert_eq!(pack.read("npc/20.emd").unwrap(), b"npc-20");
        assert_eq!(pack.read("npc/2e.emd").unwrap(), b"npc-2e");

        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
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
        assert_eq!(count("player/"), 6);
        assert_eq!(count("npc/"), 15);
        assert_eq!(
            pack.paths()
                .filter(|path| path.starts_with("player/") && path.ends_with(".emd"))
                .count(),
            4
        );
        assert_eq!(
            pack.paths()
                .filter(|path| path.starts_with("player/") && path.ends_with(".emw"))
                .count(),
            2
        );

        let image = bmp::decode(pack.read("roomcut/100_000.bmp").unwrap()).unwrap();
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
        assert!(
            plan.voice
                .iter()
                .any(|asset| asset.entry == "voice/v001_00.wav")
        );
        assert!(
            plan.voice
                .iter()
                .any(|asset| asset.entry == "voice/v104_00.wav")
        );
        // The 8-character `VB00_31a` record resolves to its lowercased file.
        assert!(
            plan.voice
                .iter()
                .any(|asset| asset.entry == "voice/vb00_31a.wav")
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
    fn converts_voice_into_a_sibling_pack_and_embedded() {
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
        fs::write(voice.join("ANNOUNCE.WAV"), b"unused").unwrap();

        // Default: a second plain v1 pack beside the main one.
        let out = root.path.join("out.akpak");
        let voice_out = root.path.join("out.voice.akpak");
        convert_game_with_voice(&root.path, &out, None, 1, &VoicePackOptions::Sibling(None))
            .unwrap();
        let voice_pack = crate::pack::Pack::open(&voice_out).unwrap();
        assert_eq!(voice_pack.len(), 2);
        assert_eq!(voice_pack.read("voice/v001_00.wav").unwrap(), b"voice-a");
        assert_eq!(voice_pack.read("voice/v104_00.wav").unwrap(), b"voice-bb");
        let main = crate::pack::Pack::open(&out).unwrap();
        assert!(!main.paths().any(|path| path.starts_with("voice/")));

        // Embed: the entries move into the main pack, no sibling is written.
        let out = root.path.join("embed.akpak");
        convert_game_with_voice(&root.path, &out, None, 1, &VoicePackOptions::Embed).unwrap();
        let main = crate::pack::Pack::open(&out).unwrap();
        assert_eq!(main.read("voice/v001_00.wav").unwrap(), b"voice-a");
        assert_eq!(main.read("voice/v104_00.wav").unwrap(), b"voice-bb");
        assert!(!root.path.join("embed.voice.akpak").exists());
    }

    #[test]
    fn parallel_map_reports_the_first_error_in_job_order() {
        let mut progress = Progress::new();
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
        convert_game_with_options(&root.path, &serial, None, 1).unwrap();
        convert_game_with_options(&root.path, &parallel, None, 4).unwrap();
        assert_eq!(fs::read(&serial).unwrap(), fs::read(&parallel).unwrap());
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
        assert_eq!(pack.read("door/ele01.dor").unwrap(), b"dor-ele01");
        assert_eq!(pack.read("door/mon.dor").unwrap(), b"dor-mon");
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
        let image = bmp::decode(pack.read("roomcut/300_000.bmp").unwrap()).unwrap();
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
        assert_eq!(count("ui/"), UI_ASSETS.len());
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
        let image = bmp::decode(pack.read("item/item_all.bmp").unwrap()).unwrap();
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
        let mut progress = Progress::new();
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

    const SYNTHETIC_SECTION_VA: u32 = 0xB0000;
    const SYNTHETIC_SECTION_SIZE: usize = 0x20000;
    const SYNTHETIC_RAW_OFFSET: usize = 0x400;

    /// A minimal PE32 image with one section covering RVAs `0xB0000..0xD0000`.
    fn synthetic_pe() -> Vec<u8> {
        let mut data = vec![0u8; SYNTHETIC_RAW_OFFSET + SYNTHETIC_SECTION_SIZE];
        data[0] = b'M';
        data[1] = b'Z';
        data[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        data[0x80..0x84].copy_from_slice(b"PE\0\0");
        data[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        data[0x94..0x96].copy_from_slice(&0xE0u16.to_le_bytes());
        let optional = 0x80 + 24;
        data[optional..optional + 2].copy_from_slice(&0x10Bu16.to_le_bytes());
        data[optional + 28..optional + 32].copy_from_slice(&0x400000u32.to_le_bytes());
        let section = optional + 0xE0;
        data[section..section + 5].copy_from_slice(b".data");
        data[section + 8..section + 12]
            .copy_from_slice(&((SYNTHETIC_SECTION_SIZE + 0x8000) as u32).to_le_bytes());
        data[section + 12..section + 16].copy_from_slice(&SYNTHETIC_SECTION_VA.to_le_bytes());
        data[section + 16..section + 20]
            .copy_from_slice(&(SYNTHETIC_SECTION_SIZE as u32).to_le_bytes());
        data[section + 20..section + 24]
            .copy_from_slice(&(SYNTHETIC_RAW_OFFSET as u32).to_le_bytes());
        data
    }

    /// Write `bytes` at virtual address `va` of a synthetic image.
    fn pe_place(pe: &mut [u8], va: u32, bytes: &[u8]) {
        let offset = SYNTHETIC_RAW_OFFSET + (va - 0x400000 - SYNTHETIC_SECTION_VA) as usize;
        pe[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// Write a pointer table at `table_va`.
    fn pe_pointer_table(pe: &mut [u8], table_va: u32, count: usize, entries: &[(usize, u32)]) {
        let mut table = vec![0u8; count * 4];
        for &(index, va) in entries {
            table[index * 4..index * 4 + 4].copy_from_slice(&va.to_le_bytes());
        }
        pe_place(pe, table_va, &table);
    }

    /// A synthetic executable holding all five text tables at the JPN virtual
    /// addresses.
    fn synthetic_text_exe() -> Vec<u8> {
        let mut pe = synthetic_pe();

        pe_place(&mut pe, 0x4CDA00, &[0x0C, 0x0D, 0x01, 0x00]);
        pe_place(
            &mut pe,
            0x4CDA10,
            &[0x05, 0x01, 0x06, 0x00, 0x05, 0x00, 0x0C, 0x01, 0x2A],
        );
        pe_pointer_table(
            &mut pe,
            TEXT_MESSAGES_VA,
            TEXT_MESSAGES_COUNT,
            &[(0, 0x4CDA00), (1, 0x4CDA10)],
        );

        pe_place(&mut pe, 0x4CDA20, &[0xB0, 0x07]);
        pe_pointer_table(&mut pe, TEXT_NAMES_VA, TEXT_NAMES_COUNT, &[(0, 0x4CDA20)]);
        pe_place(&mut pe, 0x4CDA30, &[0xC0, 0x07]);
        pe_pointer_table(
            &mut pe,
            TEXT_UNKNOWN_VA,
            TEXT_UNKNOWN_COUNT,
            &[(0, 0x4CDA30)],
        );

        pe_place(&mut pe, 0x4CDA40, &[0xDD, 0x01, 0x00]);
        pe_pointer_table(
            &mut pe,
            TEXT_DESCRIPTIONS_VA,
            TEXT_DESCRIPTIONS_COUNT,
            &[(0, 0x4CDA40)],
        );

        pe_place(&mut pe, 0x4B0FA0, &[0x00, 0xFB, 0x01]);
        pe_place(&mut pe, 0x4B0FC0, &[0x3B, 0x01]);
        pe_place(
            &mut pe,
            0x4B0FD0,
            &[0x00, 0x00, 0x00, 0x00, 0x00, 0x23, 0x1D, 0x29, 0x21, 0x01],
        );
        pe_place(&mut pe, 0x4B0FE0, &[0x28, 0x2B, 0x1D, 0x20, 0x01]);
        pe_place(&mut pe, 0x4B0FE8, &[0x2F, 0x1D, 0x32, 0x21, 0x01]);
        pe_pointer_table(
            &mut pe,
            SAVE_HEADER_TABLE_VA,
            2,
            &[(0, 0x4B0FE8), (1, 0x4B0FE0)],
        );
        pe_place(&mut pe, 0x4B0FF8, &[0x00, 0x00, 0x00, 0x01]);
        pe_place(&mut pe, 0x4B1000, &[0x01]);
        pe_place(&mut pe, 0x4B1004, &[0x01]);
        pe_pointer_table(
            &mut pe,
            SAVE_EXIT_TABLE_VA,
            2,
            &[(0, 0x4B1004), (1, 0x4B1000)],
        );

        pe_place(&mut pe, 0x4B1010, &[0x41, 0x01]);
        pe_place(&mut pe, 0x4B1018, &[0x42, 0x01]);
        pe_pointer_table(
            &mut pe,
            SAVE_CHAR_TABLE_VA,
            2,
            &[(0, 0x4B1010), (1, 0x4B1018)],
        );
        pe_place(&mut pe, 0x4B1028, &[0x01]);
        pe_place(&mut pe, 0x4B1040, &[0x01]);
        for index in 0..SAVE_LOCATION_COUNT {
            let va = 0x4B1068 + index as u32 * 0x18;
            pe_place(&mut pe, va, &[0x50 + index as u8, 0x01]);
        }
        let locations: Vec<(usize, u32)> = (0..SAVE_LOCATION_COUNT)
            .map(|index| (index, 0x4B1068 + index as u32 * 0x18))
            .collect();
        pe_pointer_table(
            &mut pe,
            SAVE_LOCATION_TABLE_VA,
            SAVE_LOCATION_COUNT,
            &locations,
        );
        pe_place(&mut pe, SAVE_OVERWRITE_VA, &[0x01]);
        pe_place(&mut pe, SAVE_YES_NO_VA, &[0x01]);
        pe
    }

    #[test]
    fn maps_virtual_addresses_through_the_section_table() {
        let data = synthetic_pe();
        let image = PeImage::parse(&data).unwrap();

        assert_eq!(image.image_base, 0x400000);
        assert_eq!(image.va_to_offset(0x4B0000), Some(SYNTHETIC_RAW_OFFSET));
        assert_eq!(image.va_to_offset(0x4B01FF), Some(0x5FF));
        // The section's zero-filled tail has no file bytes.
        assert_eq!(image.va_to_offset(0x4D0000), None);
        assert_eq!(image.va_to_offset(0x4AFFFF), None);
        assert_eq!(image.va_to_offset(0x500000), None);
        assert!(PeImage::parse(&data[..0x20]).is_err());
        assert!(PeImage::parse(&[0u8; 0x100]).is_err());
    }

    #[test]
    fn extracts_the_five_text_tables_from_a_synthetic_executable() {
        let data = synthetic_text_exe();
        let tables = extract_text_tables(&data).unwrap();
        assert_eq!(tables.len(), 5);
        let table = |name: &str| {
            tables
                .iter()
                .find(|(path, _)| *path == name)
                .map(|(_, data)| data.as_slice())
                .unwrap()
        };

        let messages = crate::text::Table::parse_message(table(text::MESSAGES_ENTRY)).unwrap();
        assert_eq!(messages.len(), 64);
        assert_eq!(messages.get(0), Some(&[0x0C, 0x0D, 0x01, 0x00][..]));
        assert_eq!(
            messages.get(1),
            Some(&[0x05, 0x01, 0x06, 0x00, 0x05, 0x00, 0x0C, 0x01, 0x2A][..])
        );
        // A null pointer is a missing entry, like the shipped table's slot 63.
        assert_eq!(messages.get(63), None);

        let names = crate::text::Table::parse_name(table(text::NAMES_ENTRY)).unwrap();
        assert_eq!(names.len(), 128);
        assert_eq!(names.get(0), Some(&[0xB0, 0x07][..]));

        let unknown = crate::text::Table::parse_name(table(text::UNKNOWN_ENTRY)).unwrap();
        assert_eq!(unknown.len(), 16);
        // The unknown table is the tail of the name table.
        assert_eq!(unknown.get(0), Some(&[0xC0, 0x07][..]));

        let descriptions =
            crate::text::Table::parse_message(table(text::DESCRIPTIONS_ENTRY)).unwrap();
        assert_eq!(descriptions.len(), 79);
        assert_eq!(descriptions.get(0), Some(&[0xDD, 0x01, 0x00][..]));

        let save = crate::text::Table::parse_plain(table(text::SAVE_ENTRY)).unwrap();
        assert_eq!(save.len(), 21);
        assert_eq!(save.get(0), Some(&[0x2F, 0x1D, 0x32, 0x21, 0x01][..]));
        assert_eq!(save.get(8), Some(&[0x00, 0xFB, 0x01][..]));
        assert_eq!(save.get(14), Some(&[0x50, 0x01][..]));
        assert_eq!(save.get(20), Some(&[0x56, 0x01][..]));
    }

    #[test]
    fn discovers_item_m2_and_the_executable() {
        let root = TempDir::new("item-m2-discover");
        make_stage_dirs(&root.path);
        fs::create_dir_all(root.path.join("install/ItEm_M2")).unwrap();
        fs::write(root.path.join("install/Bio.exe"), b"MZ").unwrap();

        let layout = discover_layout(&root.path).unwrap();
        assert_eq!(layout.item_m2.unwrap(), root.path.join("install/ItEm_M2"));
        assert_eq!(
            discover_exe(&root.path).unwrap().unwrap(),
            root.path.join("install/Bio.exe")
        );
        assert_eq!(discover_exe(&root.path.join("STAGE1")).unwrap(), None);
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
        let mut progress = Progress::new();
        let (items, item_bytes) =
            copy_item_models(Some(&dir), &mut writer, &mut progress, 1).unwrap();
        let (files, file_bytes) = copy_file_art(Some(&dir), &mut writer, &mut progress, 1).unwrap();

        assert_eq!(items, 2);
        assert_eq!(files, 2 + FILEI_COUNT + TEXTM_COUNT);
        assert!(item_bytes > 0 && file_bytes > 0);
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.read("item/i00v.ivm").unwrap(), b"ivm0");
        assert_eq!(pack.read("item/ing.ivm").unwrap(), b"ivm1");
        assert_eq!(pack.read("file/file000.tim").unwrap(), b"cover0");
        assert_eq!(pack.read("file/filei17.tim").unwrap(), b"i17");
        assert_eq!(pack.read("file/textm_42.tim").unwrap(), b"page42");
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("item/"), 2);
        assert_eq!(count("file/"), 62);
    }

    #[test]
    fn converts_synthetic_text_item_and_file_entries() {
        let root = TempDir::new("text-item-file");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1100.RDT"), [0u8; 4]).unwrap();
        fs::write(root.path.join("Bio.exe"), synthetic_text_exe()).unwrap();
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

        let out = root.path.join("out.akpak");
        convert_game(&root.path, &out).unwrap();
        let pack = crate::pack::Pack::open(&out).unwrap();

        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("text/"), 5);
        assert_eq!(count("item/"), 2);
        assert_eq!(count("file/"), 62);
        let messages =
            crate::text::Table::parse_message(pack.read("text/messages.bin").unwrap()).unwrap();
        assert_eq!(messages.len(), 64);
        assert_eq!(messages.get(0), Some(&[0x0C, 0x0D, 0x01, 0x00][..]));
        assert_eq!(pack.read("item/mini.ivm").unwrap(), b"ivm1");
        assert_eq!(pack.read("file/textm_42.tim").unwrap(), b"page42");
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn extracts_real_text_tables() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let path = PathBuf::from(root).join("Bio.exe");
        let data = fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));

        let tables = extract_text_tables(&data).unwrap();
        assert_eq!(tables.len(), 5);
        let table = |name: &str| {
            tables
                .iter()
                .find(|(path, _)| *path == name)
                .map(|(_, data)| data.as_slice())
                .unwrap()
        };

        let messages = crate::text::Table::parse_message(table(text::MESSAGES_ENTRY)).unwrap();
        assert_eq!(messages.len(), 64);
        assert_eq!(
            messages.get(0).unwrap(),
            &[
                0x05, 0x01, 0x06, 0x00, 0x05, 0x00, 0x83, 0xF9, 0x52, 0x7E, 0x75, 0x63, 0x5C, 0x1B,
                0x08, 0x02, 0x0A, 0x00, 0x01, 0x00,
            ][..]
        );
        assert_eq!(messages.get(63), None);

        let names = crate::text::Table::parse_name(table(text::NAMES_ENTRY)).unwrap();
        assert_eq!(names.len(), 128);
        assert_eq!(
            names.get(0).unwrap(),
            &[0xB0, 0xD4, 0xE4, 0xF6, 0xBA, 0xBB, 0xA8, 0xC2, 0x07][..]
        );

        let unknown = crate::text::Table::parse_name(table(text::UNKNOWN_ENTRY)).unwrap();
        assert_eq!(unknown.len(), 16);
        assert_eq!(unknown.get(0).unwrap(), names.get(112).unwrap());

        let descriptions =
            crate::text::Table::parse_message(table(text::DESCRIPTIONS_ENTRY)).unwrap();
        assert_eq!(descriptions.len(), 79);
        assert_eq!(&descriptions.get(0).unwrap()[..2], &[0x60, 0x6F]);

        let save = crate::text::Table::parse_plain(table(text::SAVE_ENTRY)).unwrap();
        assert_eq!(save.len(), 21);
        assert_eq!(
            save.get(0).unwrap(),
            &[0x2F, 0x1D, 0x32, 0x21, 0x01][..],
            "header 0 must be SAVE"
        );
        assert_eq!(
            save.get(1).unwrap(),
            &[0x28, 0x2B, 0x1D, 0x20, 0x01][..],
            "header 1 must be LOAD"
        );
        assert!(save.get(8).unwrap().ends_with(&[0x01]));
        // Location names are eight two-byte cells plus the terminator.
        assert_eq!(save.get(14).unwrap().len(), 17);
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn packs_real_item_and_file_art() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let dir = PathBuf::from(&root).join("JPN/ITEM_M2");

        let mut writer = PackWriter::new();
        let mut progress = Progress::new();
        let (items, items_bytes) =
            copy_item_models(Some(&dir), &mut writer, &mut progress, 1).unwrap();
        let (files, files_bytes) =
            copy_file_art(Some(&dir), &mut writer, &mut progress, 1).unwrap();

        assert_eq!(items, 77);
        assert_eq!(files, 2 + FILEI_COUNT + TEXTM_COUNT);
        assert!(items_bytes > 0 && files_bytes > 0);
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("item/"), 77);
        assert_eq!(count("file/"), 62);
        assert_eq!(
            pack.read("item/i00v.ivm").unwrap(),
            fs::read(dir.join("I00V.IVM")).unwrap()
        );
        assert_eq!(
            pack.read("file/textm_z0.tim").unwrap(),
            fs::read(dir.join("TEXTM_Z0.TIM")).unwrap()
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
        assert_eq!(count("npc/"), 15);
        assert_eq!(count("effspr/"), 33);
        assert!(pack.contains("npc/20.emd"));
        assert!(pack.contains("npc/2e.emd"));
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

        // Four character models (Char10..Char13) plus the two no-weapon
        // locomotion clips (W00, W10). player/01 is Jill, the character that
        // owns ROOM1001.
        assert_eq!(count("player/"), 6);
        assert!(pack.contains("player/01.emd"));
        assert!(pack.contains("player/01.emw"));

        let image = bmp::decode(pack.read("roomcut/100_000.bmp").unwrap()).unwrap();
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
            (npc::FIRST_ID..=npc::LAST_ID).collect::<Vec<_>>()
        );

        let mut writer = PackWriter::new();
        let mut progress = Progress::new();
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
            .filter(|path| path.starts_with("npc/"))
            .collect();
        assert_eq!(entries.len(), 15, "{entries:?}");

        for asset in &assets {
            let data = pack.read(&asset.entry).unwrap();
            let emd =
                crate::emd::parse(data).unwrap_or_else(|err| panic!("{}: {err:#}", asset.entry));
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
    fn packs_font_tim_raw() {
        let root = TempDir::new("font-pack");
        make_stage_dirs(&root.path);
        let data = root.path.join("DaTa");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("FONT.TIM"), b"raw-font-sheet").unwrap();

        let plan = build_plan(&root.path).unwrap();
        assert_eq!(plan.font.as_deref(), Some(data.join("FONT.TIM").as_path()));

        let mut writer = PackWriter::new();
        let mut progress = Progress::new();
        let (count, bytes) = copy_font(plan.font.as_deref(), &mut writer, &mut progress).unwrap();

        assert_eq!((count, bytes), (1, 14));
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.read("font/font.tim").unwrap(), b"raw-font-sheet");
    }
}
