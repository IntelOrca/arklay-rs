//! `convert-game`: migrate a full game installation into an `.akpak` pack.
//!
//! Discovers `STAGE1`..`STAGE7`, `ENEMY`, `PLAYERS`, `sound` and `objspr`
//! (case-insensitively, up to two levels below the root), stores every
//! `ROOM####.RDT`, converts the camera backgrounds of every distinct room
//! once, converts the room mask pages of every camera that has sprite groups,
//! copies the `BGM_*.WAV` music files and the four player models plus
//! the two no-weapon locomotion clips. Stages 6 and 7 reuse the backgrounds
//! and mask pages of STAGE1/STAGE2 with the stage digit reduced by 5.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::music;
use crate::pack::PackWriter;
use crate::progress::{Progress, format_duration};
use crate::sfx;
use crate::state::RoomId;
use crate::{bmp, lzw, rdt, tim};

/// How many directory levels below the root are searched for the stage and
/// `sound` directories.
const MAX_DEPTH: usize = 2;

/// Expected camera background width.
const CUT_WIDTH: u32 = 320;
/// Expected camera background height.
const CUT_HEIGHT: u32 = 240;

/// Byte offset of the bit depth field in a BMP header.
const BMP_BPP_OFFSET: usize = 28;

/// Bytes written per chunk while streaming the finished pack to disk.
const WRITE_CHUNK: usize = 1 << 20;

/// Convert the game installation under `root` into the `.akpak` pack `out`.
pub fn convert_game(root: &Path, out: &Path) -> Result<()> {
    let mut progress = Progress::new();
    let Plan {
        rooms,
        sound,
        players,
        roommask,
        warnings,
    } = build_plan(root)?;
    for warning in &warnings {
        println!("warning: {warning}");
    }

    let mut writer = PackWriter::new();
    let mut stage_counts = [(0usize, 0usize); RoomId::MAX_STAGE as usize];
    let mut rdt_count = 0usize;
    let mut rdt_bytes = 0usize;
    let mut cut_count = 0usize;
    let mut cut_bytes = 0usize;
    let mut mask_count = 0usize;
    let mut mask_bytes = 0usize;

    // Resolve the cut jobs first so RDTs and backgrounds can be reported as
    // two clean phases instead of alternating per room.
    let mut cut_jobs: Vec<(RoomId, usize, PathBuf)> = Vec::new();
    for room in &rooms {
        for (camera, pak) in room.paks.iter().enumerate() {
            cut_jobs.push((room.id, camera, pak.clone()));
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

    let cut_total = cut_jobs.len() as u64;
    progress.begin("roomcut", cut_total, "cuts");
    for (id, camera, pak) in cut_jobs {
        let bmp_bytes = convert_camera(&pak)?;
        let entry = id.cut_entry(camera);
        cut_bytes += bmp_bytes.len();
        cut_count += 1;
        stage_counts[id.stage_index() as usize].1 += 1;
        writer
            .add(&entry, bmp_bytes)
            .with_context(|| format!("failed to add {entry}"))?;
        progress.advance(&entry);
    }
    progress.end_phase();

    let mask_total = roommask.len() as u64;
    progress.begin("roommask", mask_total, "files");
    for asset in &roommask {
        let bmp_bytes = convert_roommask(&asset.source)?;
        mask_bytes += bmp_bytes.len();
        mask_count += 1;
        writer
            .add(&asset.entry, bmp_bytes)
            .with_context(|| format!("failed to add {}", asset.entry))?;
        progress.advance(&asset.entry);
    }
    progress.end_phase();

    let (bgm_count, bgm_bytes) = copy_music(&sound, &mut writer, &mut progress)?;
    let (se_count, se_bytes) = copy_se(&sound, &mut writer, &mut progress)?;
    let (player_count, player_bytes) = copy_players(&players, &mut writer, &mut progress)?;

    for (index, (rdts, cuts)) in stage_counts.iter().enumerate() {
        println!("STAGE{}: {rdts} RDT(s), {cuts} cut(s)", index + 1);
    }
    println!("room: {rdt_count} entries, {rdt_bytes} bytes");
    println!("roomcut: {cut_count} entries, {cut_bytes} bytes");
    println!("roommask: {mask_count} entries, {mask_bytes} bytes");
    println!("bgm: {bgm_count} entries, {bgm_bytes} bytes");
    println!("se: {se_count} entries, {se_bytes} bytes");
    println!("player: {player_count} entries, {player_bytes} bytes");

    let pack_bytes = writer.to_bytes()?;
    let size = pack_bytes.len();
    progress.begin("write", size as u64, "bytes");
    let mut file =
        fs::File::create(out).with_context(|| format!("failed to create {}", out.display()))?;
    for chunk in pack_bytes.chunks(WRITE_CHUNK) {
        file.write_all(chunk)
            .with_context(|| format!("failed to write {}", out.display()))?;
        progress.advance_by(chunk.len() as u64);
    }
    drop(file);
    progress.end_phase();

    let entries = rdt_count + cut_count + mask_count + bgm_count + se_count + player_count;
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

    progress.begin("bgm", files.len() as u64, "files");
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (path, source) in files {
        let data =
            fs::read(&source).with_context(|| format!("failed to read {}", source.display()))?;
        bytes += data.len();
        count += 1;
        writer
            .add(&path, data)
            .with_context(|| format!("failed to add {path}"))?;
        progress.advance(&path);
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Add every sound effect named by the room sound tables.
///
/// The pack stores the canonical names lowercased (`se/ft_wda.wav`); the
/// install's file names are matched case-insensitively and copied raw.
fn copy_se(
    sound: &Option<PathBuf>,
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    let Some(sound) = sound else {
        return Ok((0, 0));
    };

    let index = index_dir(sound)?;
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for name in sfx::SE_NAMES {
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

    progress.begin("se", files.len() as u64, "files");
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (path, source) in files {
        let data =
            fs::read(&source).with_context(|| format!("failed to read {}", source.display()))?;
        bytes += data.len();
        count += 1;
        writer
            .add(&path, data)
            .with_context(|| format!("failed to add {path}"))?;
        progress.advance(&path);
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Add every resolved player model and locomotion clip to the pack.
fn copy_players(
    players: &[PlayerAsset],
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    progress.begin("player", players.len() as u64, "files");
    let mut count = 0usize;
    let mut bytes = 0usize;
    for asset in players {
        let data = fs::read(&asset.source)
            .with_context(|| format!("failed to read {}", asset.source.display()))?;
        bytes += data.len();
        count += 1;
        writer
            .add(&asset.entry, data)
            .with_context(|| format!("failed to add {}", asset.entry))?;
        progress.advance(&asset.entry);
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Stage, sound, enemy-model, player and room-mask directory roots discovered
/// under the conversion root.
#[derive(Debug)]
struct Layout {
    stages: BTreeMap<u8, PathBuf>,
    sound: Option<PathBuf>,
    enemy: Option<PathBuf>,
    players: Option<PathBuf>,
    objspr: Option<PathBuf>,
}

/// Breadth-first, case-insensitive discovery of `STAGE1`..`STAGE7`, `sound`,
/// `enemy`, `players` and `objspr`.
fn discover_layout(root: &Path) -> Result<Layout> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut stages: BTreeMap<u8, PathBuf> = BTreeMap::new();
    let mut sound = None;
    let mut enemy = None;
    let mut players = None;
    let mut objspr = None;

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
    })
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
    players: Vec<PlayerAsset>,
    roommask: Vec<RoomMask>,
    /// Non-fatal problems found while resolving optional inputs.
    warnings: Vec<String>,
}

/// Discover, enumerate, and validate every conversion input.
fn build_plan(root: &Path) -> Result<Plan> {
    let Layout {
        stages,
        sound,
        enemy,
        players,
        objspr,
    } = discover_layout(root)?;
    let mut rooms: BTreeMap<(u8, u8), Room> = BTreeMap::new();

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

            let path = entry.path();
            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            let state = rdt::parse(&bytes, id)
                .with_context(|| format!("failed to parse {}", path.display()))?;

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
    let mut warnings = Vec::new();
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

    Ok(Plan {
        rooms: rooms.into_values().collect(),
        sound,
        players: resolve_players(enemy.as_deref(), players.as_deref())?,
        roommask,
        warnings,
    })
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

    /// Every sound effect the room tables name, as mixed-case files like the
    /// shipped install.
    fn write_se_files(root: &Path) {
        let sound = root.join("sound");
        fs::create_dir_all(&sound).unwrap();
        for name in sfx::SE_NAMES {
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

        let layout = discover_layout(&root.path).unwrap();

        assert_eq!(layout.stages[&1], root.path.join("STAGE1"));
        assert_eq!(layout.stages[&2], root.path.join("install/sTaGe2"));
        assert_eq!(layout.stages[&7], root.path.join("install/sTaGe7"));
        assert_eq!(layout.sound.unwrap(), root.path.join("install/sound"));
        assert_eq!(layout.enemy.unwrap(), root.path.join("install/EnEmY"));
        assert_eq!(layout.players.unwrap(), root.path.join("pLaYeRs"));
        assert_eq!(layout.objspr.unwrap(), root.path.join("install/ObJsPr"));
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

        assert_eq!(pack.read("player/01.emd").unwrap(), b"emd1");
        assert_eq!(pack.read("player/01.emw").unwrap(), b"emw1");

        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("room/"), 3);
        assert_eq!(count("roomcut/"), 2);
        assert_eq!(count("roommask/"), 0);
        assert_eq!(count("bgm/"), 3);
        assert_eq!(count("se/"), 68);
        assert_eq!(count("player/"), 6);
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
    fn missing_mask_paks_are_warnings() {
        let root = TempDir::new("missing-masks");
        make_stage_dirs(&root.path);
        fs::write(root.path.join("STAGE1/ROOM1000.RDT"), rdt_with_masks()).unwrap();
        fs::write(root.path.join("STAGE1/RC1000.pak"), camera_pak()).unwrap();
        fs::create_dir_all(root.path.join("objspr")).unwrap();

        let plan = build_plan(&root.path).unwrap();

        assert!(plan.roommask.is_empty());
        assert_eq!(plan.warnings.len(), 1);
        assert!(
            plan.warnings[0].contains("OSP00000.pak"),
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
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].contains("objspr"), "{:?}", plan.warnings);
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
        assert_eq!(count("se/"), 68);
        assert!(pack.contains("room/1001.rdt"));
        assert!(pack.contains("roomcut/100_000.bmp"));
        assert!(pack.contains("roommask/100_000.bmp"));
        assert!(pack.contains("bgm/013.wav"));
        assert!(pack.contains("se/ft_wda.wav"));
        assert!(pack.contains("se/dr_wd01.wav"));

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
}
