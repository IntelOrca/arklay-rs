//! `convert-game`: migrate a full game installation into an `.akpak` pack.
//!
//! Discovers `STAGE1`..`STAGE7`, `ENEMY`, `PLAYERS`, `sound`, `objspr` and
//! `ITEM_M1` (case-insensitively, up to two levels below the root), stores
//! every `ROOM####.RDT`, converts the camera backgrounds of every distinct
//! room once, converts the room mask pages of every camera that has sprite
//! groups, copies the door animations named by the door type table, copies the
//! `BGM_*.WAV` music files, the 68 named sound effects and the four player
//! models plus the two no-weapon locomotion clips. Stages 6 and 7 reuse the
//! backgrounds and mask pages of STAGE1/STAGE2 with the stage digit reduced
//! by 5.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::items;
use crate::model::Texture8;
use crate::music;
use crate::pack::PackWriter;
use crate::progress::{Progress, format_duration};
use crate::sfx;
use crate::state::{Image, RoomId};
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

/// Bytes written per chunk while streaming the finished pack to disk.
const WRITE_CHUNK: usize = 1 << 20;

/// Convert the game installation under `root` into the `.akpak` pack `out`.
pub fn convert_game(root: &Path, out: &Path) -> Result<()> {
    let mut progress = Progress::new();
    let Plan {
        rooms,
        sound,
        item_m1,
        players,
        roommask,
        data,
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
    let (door_count, door_bytes) = copy_doors(&item_m1, &mut writer, &mut progress)?;
    let (player_count, player_bytes) = copy_players(&players, &mut writer, &mut progress)?;
    let (ui_count, ui_bytes) = copy_ui_art(&data, &mut writer, &mut progress)?;
    let (item_count, item_bytes) = copy_item_art(&data, &mut writer, &mut progress)?;
    let (data_count, data_bytes) = copy_bio_card(&data, &mut writer, &mut progress)?;

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
    println!("ui: {ui_count} entries, {ui_bytes} bytes");
    println!("item: {item_count} entries, {item_bytes} bytes");
    println!("data: {data_count} entries, {data_bytes} bytes");

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

    let entries = rdt_count
        + cut_count
        + mask_count
        + bgm_count
        + se_count
        + door_count
        + player_count
        + ui_count
        + item_count
        + data_count;
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

    progress.begin("door", files.len() as u64, "files");
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

/// Add the raw UI art and the converted title/select/options backgrounds.
///
/// The TIMs the renderer decodes itself are copied raw; the 16bpp direct
/// TIMs and headerless `.PIX` backgrounds are converted to BMP here.
fn copy_ui_art(
    data: &DataPlan,
    writer: &mut PackWriter,
    progress: &mut Progress,
) -> Result<(usize, usize)> {
    progress.begin("ui", data.ui.len() as u64, "files");
    let mut count = 0usize;
    let mut bytes = 0usize;
    for asset in &data.ui {
        let raw = fs::read(&asset.source)
            .with_context(|| format!("failed to read {}", asset.source.display()))?;
        let converted = convert_ui_asset(asset.kind, &raw)
            .with_context(|| format!("failed to convert {}", asset.source.display()))?;
        bytes += converted.len();
        count += 1;
        writer
            .add(asset.entry, converted)
            .with_context(|| format!("failed to add {}", asset.entry))?;
        progress.advance(asset.entry);
    }
    progress.end_phase();
    Ok((count, bytes))
}

/// Bake the three item icon atlases through `STATUS.TIM`'s second CLUT row.
fn copy_item_art(
    data: &DataPlan,
    writer: &mut PackWriter,
    progress: &mut Progress,
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
    let mut count = 0usize;
    let mut bytes = 0usize;
    for asset in &data.items {
        let pix = fs::read(&asset.source)
            .with_context(|| format!("failed to read {}", asset.source.display()))?;
        let bmp_bytes = bake_item_atlas(&pix, asset.rows, &texture)
            .with_context(|| format!("failed to bake {}", asset.source.display()))?;
        bytes += bmp_bytes.len();
        count += 1;
        writer
            .add(asset.entry, bmp_bytes)
            .with_context(|| format!("failed to add {}", asset.entry))?;
        progress.advance(asset.entry);
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
                "no data directory found; {} UI art file(s), {} item atlas(es) and {BIO_CARD_ENTRY} will be missing",
                UI_ASSETS.len(),
                ITEM_ASSETS.len()
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
    Ok(plan)
}

/// Stage, sound, enemy-model, player, room-mask, door-art and data directory
/// roots discovered under the conversion root.
#[derive(Debug)]
struct Layout {
    stages: BTreeMap<u8, PathBuf>,
    sound: Option<PathBuf>,
    enemy: Option<PathBuf>,
    players: Option<PathBuf>,
    objspr: Option<PathBuf>,
    item_m1: Option<PathBuf>,
    data: Option<PathBuf>,
}

/// Breadth-first, case-insensitive discovery of `STAGE1`..`STAGE7`, `sound`,
/// `enemy`, `players`, `objspr`, `ITEM_M1` and `data`.
fn discover_layout(root: &Path) -> Result<Layout> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut stages: BTreeMap<u8, PathBuf> = BTreeMap::new();
    let mut sound = None;
    let mut enemy = None;
    let mut players = None;
    let mut objspr = None;
    let mut item_m1 = None;
    let mut data = None;

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
    item_m1: Option<PathBuf>,
    players: Vec<PlayerAsset>,
    roommask: Vec<RoomMask>,
    /// Resolved `DATA` UI, item and save-prefix assets.
    data: DataPlan,
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
        item_m1,
        data,
    } = discover_layout(root)?;
    let data = resolve_data_assets(data.as_deref())?;
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

    warnings.extend(data.warnings.iter().cloned());
    Ok(Plan {
        rooms: rooms.into_values().collect(),
        sound,
        item_m1,
        players: resolve_players(enemy.as_deref(), players.as_deref())?,
        roommask,
        data,
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
        fs::create_dir_all(root.path.join("install/ItEm_M1")).unwrap();
        fs::create_dir_all(root.path.join("install/DaTa")).unwrap();

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

        let count = |prefix: &str| pack.paths().filter(|path| path.starts_with(prefix)).count();
        assert_eq!(count("room/"), 3);
        assert_eq!(count("roomcut/"), 2);
        assert_eq!(count("roommask/"), 0);
        assert_eq!(count("bgm/"), 3);
        assert_eq!(count("se/"), 68);
        assert_eq!(count("door/"), 34);
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
        assert_eq!(pack.read("data/bio_card.dat").unwrap().len(), 0x41C);
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

        // The same conversion the pack writer performs, checked for counts.
        let mut writer = PackWriter::new();
        let mut progress = Progress::new();
        let (ui_count, _) = copy_ui_art(&plan, &mut writer, &mut progress).unwrap();
        let (item_count, _) = copy_item_art(&plan, &mut writer, &mut progress).unwrap();
        let (data_count, _) = copy_bio_card(&plan, &mut writer, &mut progress).unwrap();
        assert_eq!(ui_count, UI_ASSETS.len());
        assert_eq!(item_count, ITEM_ASSETS.len());
        assert_eq!(data_count, 1);
        let pack = crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.read(BIO_CARD_ENTRY).unwrap().len(), 0x41C);
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
        assert_eq!(count("door/"), 34);
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
}
