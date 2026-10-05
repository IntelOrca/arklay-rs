//! Per-room and global effect sprite metadata.
//!
//! The RDT header stores three direct file offsets (pointer slots 13/14/15):
//! an eight-entry sprite index table (`0xFF` = free), a backward-read table of
//! sprite-info offsets, and a backward-read table of embedded 4bpp TIM
//! offsets. A sprite-info record holds the frame table, the UV records and an
//! animation blob of eight depth rows, each an array of frames, each frame an
//! array of 24-byte behaviour blocks.
//!
//! `data/core00.esp` uses the same sprite-info/animation layout for the eight
//! global weapon-FX types, with the TIMs in `data/core00.etm` addressed by a
//! trailing backward-read offset table. Both parsers are total: malformed
//! records become warnings, never failures.

use anyhow::{Context, Result, bail};

use crate::effects::pool::{EFFECT_BLOCK_LEN, EffectBlock};
use crate::model::Texture8;
use crate::pack::Pack;
use crate::tim;

/// Number of sprite slots an RDT or `core00.esp` declares.
pub const EFFECT_SPRITE_SLOTS: usize = 8;

/// The RDT effect sprite section is absent.
const FREE_SPRITE: u8 = 0xFF;

/// One entry of a sprite's frame table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameEntry {
    /// Index into the sprite's UV records.
    pub uv_index: u8,
    /// Frames to hold this entry, or `0xFF` for a jump.
    pub delay: u8,
}

/// One UV record: the texture rectangle origin plus its pivot within the
/// 128-pixel billboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UvRecord {
    pub u: u8,
    pub v: u8,
    pub pivot_x: u8,
    pub pivot_y: u8,
}

/// A parsed sprite-info record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpriteInfo {
    /// Number of UV records.
    pub uv_count: u8,
    /// The sprite frame table.
    pub frames: Vec<FrameEntry>,
    /// The UV records; the frame table indexes this list.
    pub uvs: Vec<UvRecord>,
    /// Page/CLUT descriptor filled by the page packing pass.
    pub page_clut_word: u16,
    /// Texture page id filled by the page packing pass (`0` until packed).
    /// This is the original's biased `texY`; see [`SpriteInfo::page_index`].
    pub page_id: u8,
    /// V offset within the page filled by the page packing pass.
    pub page_v: u8,
}

impl SpriteInfo {
    /// The zero-based effspr page index once packed (`page_id - PAGE_BIAS`).
    pub fn page_index(&self) -> u8 {
        self.page_id.wrapping_sub(crate::effects::pages::PAGE_BIAS)
    }
}

/// One animation frame: the behaviour blocks spawned as one particle burst.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnimFrame {
    /// The frame's 24-byte blocks.
    pub blocks: Vec<EffectBlock>,
}

/// A sprite's animation blob: the eight depth-table entries plus the frames
/// each row resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpriteAnimation {
    /// The raw depth table; values are dword offsets from the blob base.
    pub depth_table: [u8; 8],
    /// Frames per depth row. Rows whose table entry points into the table
    /// itself (an unused row in the shipped data) stay empty.
    pub depth_frames: [Vec<AnimFrame>; 8],
}

impl SpriteAnimation {
    /// The frames of depth row `row` (`depth_group & 7`).
    pub fn depth(&self, row: usize) -> Option<&Vec<AnimFrame>> {
        self.depth_frames.get(row)
    }
}

/// The geometry the page-packing cursor needs from an embedded TIM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimGeometry {
    /// Pixel width (the TIM image width in 16-bit units times four).
    pub width: u16,
    /// Pixel height (the TIM image height).
    pub height: u16,
    /// Number of 16-entry CLUT rows.
    pub clut_rows: u8,
}

/// One declared effect sprite: its index plus the parsed metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectSprite {
    /// The sprite index scripts spawn this sprite by.
    pub index: u8,
    /// Frame table, UV records and the packed page fields.
    pub info: SpriteInfo,
    /// The parsed depth rows.
    pub anim: SpriteAnimation,
    /// The decoded 4bpp art; `None` when the embedded TIM is malformed.
    pub tim: Option<Texture8>,
    /// TIM geometry for the page-packing cursor.
    pub geometry: TimGeometry,
}

/// The effect sprites one room declares, parsed from its RDT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomEffects {
    /// The eight sprite index declarations (`0xFF` = free).
    pub index: [u8; EFFECT_SPRITE_SLOTS],
    /// One parsed record per non-free declaration, in declaration order.
    pub sprites: Vec<EffectSprite>,
    /// Non-fatal problems found while parsing.
    pub warnings: Vec<String>,
}

impl Default for RoomEffects {
    fn default() -> Self {
        Self {
            index: [FREE_SPRITE; EFFECT_SPRITE_SLOTS],
            sprites: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl RoomEffects {
    /// Parse the room's three effect tables.
    ///
    /// `index_ptr`, `info_ptr` and `tim_ptr` are RDT header pointer slots
    /// 13/14/15. A zero pointer means the room declares no effects. Slot `i`'s
    /// sprite-info and TIM offsets are the `i32` dwords at `ptr - 4 * i`, so
    /// the tables are stored backwards from their pointers.
    pub fn parse(data: &[u8], index_ptr: u32, info_ptr: u32, tim_ptr: u32) -> Self {
        let mut effects = Self::default();
        if index_ptr == 0 || info_ptr == 0 || tim_ptr == 0 {
            return effects;
        }
        for slot in 0..EFFECT_SPRITE_SLOTS {
            let index = match usize::try_from(index_ptr)
                .ok()
                .and_then(|base| base.checked_add(slot))
                .and_then(|at| data.get(at))
                .copied()
            {
                Some(index) => index,
                None => {
                    effects.warnings.push(format!(
                        "effect index pointer 0x{index_ptr:x} is out of bounds for the {}-byte RDT",
                        data.len()
                    ));
                    break;
                }
            };
            effects.index[slot] = index;
            if index == FREE_SPRITE {
                break;
            }
            let sprite = (|| -> Result<EffectSprite> {
                let info_offset = read_backward(data, info_ptr as usize, slot, "sprite-info")?;
                let tim_offset = read_backward(data, tim_ptr as usize, slot, "sprite TIM")?;
                let tim = data.get(tim_offset..).with_context(|| {
                    format!("effect TIM offset 0x{tim_offset:x} is out of bounds")
                })?;
                parse_sprite(data, index, info_offset, tim)
            })();
            match sprite {
                Ok(sprite) => effects.sprites.push(sprite),
                Err(error) => effects
                    .warnings
                    .push(format!("effect sprite {index}: {error:#}")),
            }
        }
        effects
    }

    /// Whether the room declares no effect sprites.
    pub fn is_empty(&self) -> bool {
        self.sprites.is_empty()
    }

    /// The declared sprite with index `index`.
    pub fn sprite(&self, index: u8) -> Option<&EffectSprite> {
        self.sprites.iter().find(|sprite| sprite.index == index)
    }

    /// The declared sprite with index `index`, mutably.
    pub fn sprite_mut(&mut self, index: u8) -> Option<&mut EffectSprite> {
        self.sprites.iter_mut().find(|sprite| sprite.index == index)
    }
}

/// The global weapon-FX sprite metadata (`data/core00.esp`/`.etm`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeaponEffects {
    /// The eight sprite index declarations (`0xFF` = free).
    pub index: [u8; EFFECT_SPRITE_SLOTS],
    /// One parsed record per non-free declaration, in slot order.
    pub sprites: Vec<EffectSprite>,
    /// Non-fatal problems found while loading.
    pub warnings: Vec<String>,
}

impl Default for WeaponEffects {
    fn default() -> Self {
        Self {
            index: [FREE_SPRITE; EFFECT_SPRITE_SLOTS],
            sprites: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl WeaponEffects {
    /// Load both `core00` metadata files from a pack. A pack converted without
    /// effects yields an empty table plus a warning; the runtime still loads.
    pub fn load(pack: &Pack) -> Self {
        let esp = pack.read(CORE_ESP_ENTRY).ok();
        let etm = pack.read(CORE_ETM_ENTRY).ok();
        match (esp, etm) {
            (Some(esp), Some(etm)) => Self::parse(esp, etm),
            (None, _) => Self {
                warnings: vec![format!(
                    "missing {CORE_ESP_ENTRY}; no weapon effects will draw"
                )],
                ..Self::default()
            },
            (Some(_), None) => Self {
                warnings: vec![format!(
                    "missing {CORE_ETM_ENTRY}; weapon effect art will not decode"
                )],
                ..Self::default()
            },
        }
    }

    /// Parse `core00.esp` (sprite info and animation) plus `core00.etm` (art).
    ///
    /// The `.esp` index table is its first eight bytes and its sprite-info
    /// offsets read backwards from the last aligned dword. The `.etm` TIM
    /// offsets read backwards from the very end of the file.
    pub fn parse(esp: &[u8], etm: &[u8]) -> Self {
        let mut effects = Self::default();
        let index = match esp.get(..EFFECT_SPRITE_SLOTS) {
            Some(index) => index,
            None => {
                effects
                    .warnings
                    .push("core00.esp is shorter than its 8-byte index table".to_string());
                return effects;
            }
        };
        effects.index.copy_from_slice(index);

        let info_end = align4(esp.len()).saturating_sub(4);
        let tim_end = align4(etm.len());
        for slot in 0..EFFECT_SPRITE_SLOTS {
            let index = effects.index[slot];
            if index == FREE_SPRITE {
                break;
            }
            let sprite = (|| -> Result<EffectSprite> {
                let info_offset = read_backward(esp, info_end, slot, "core00 sprite-info")?;
                let tim_offset = read_backward(etm, tim_end.saturating_sub(4), slot, "core00 TIM")?;
                let tim = etm.get(tim_offset..).with_context(|| {
                    format!("core00 TIM offset 0x{tim_offset:x} is out of bounds")
                })?;
                parse_sprite(esp, index, info_offset, tim)
            })();
            match sprite {
                Ok(sprite) => effects.sprites.push(sprite),
                Err(error) => effects
                    .warnings
                    .push(format!("core00 sprite {index}: {error:#}")),
            }
        }
        effects
    }

    /// The weapon sprite with index `index`.
    pub fn sprite(&self, index: u8) -> Option<&EffectSprite> {
        self.sprites.iter().find(|sprite| sprite.index == index)
    }

    /// The slot (`0..8`) the weapon sprite with `index` occupies.
    pub fn slot_of(&self, index: u8) -> Option<usize> {
        self.index.iter().position(|&declared| declared == index)
    }
}

/// Pack entry of the weapon sprite metadata.
pub const CORE_ESP_ENTRY: &str = "data/core00.esp";
/// Pack entry of the weapon sprite art.
pub const CORE_ETM_ENTRY: &str = "data/core00.etm";

/// Parse one sprite-info record and its animation blob at `offset`.
///
/// `tim` is the embedded TIM's bytes (the rest of the containing file from the
/// TIM offset). The animation blob starts after the frame table and the UV
/// records: `offset + 8 + (frame_count + uv_count) * 4`.
fn parse_sprite(data: &[u8], index: u8, offset: usize, tim: &[u8]) -> Result<EffectSprite> {
    let head = data
        .get(offset..offset + 8)
        .with_context(|| format!("sprite-info record at 0x{offset:x} is truncated"))?;
    let uv_count = head[0];
    let frame_count = u16::from_le_bytes([head[2], head[3]]);
    let page_clut_word = u16::from_le_bytes([head[4], head[5]]);
    let page_id = head[6];

    let mut frames = Vec::with_capacity(usize::from(frame_count));
    let mut cursor = offset + 8;
    for entry in 0..frame_count {
        let raw = data
            .get(cursor..cursor + 4)
            .with_context(|| format!("frame table entry {entry} is truncated"))?;
        frames.push(FrameEntry {
            uv_index: raw[0],
            delay: raw[1],
        });
        cursor += 4;
    }

    let mut uvs = Vec::with_capacity(usize::from(uv_count));
    for record in 0..uv_count {
        let raw = data
            .get(cursor..cursor + 4)
            .with_context(|| format!("UV record {record} is truncated"))?;
        uvs.push(UvRecord {
            u: raw[0],
            v: raw[1],
            pivot_x: raw[2],
            pivot_y: raw[3],
        });
        cursor += 4;
    }

    let animation = parse_animation(data, cursor)
        .with_context(|| format!("animation blob at 0x{cursor:x} is malformed"))?;
    let (geometry, decoded) = parse_tim(tim);
    Ok(EffectSprite {
        index,
        info: SpriteInfo {
            uv_count,
            frames,
            uvs,
            page_clut_word,
            page_id,
            page_v: 0,
        },
        anim: animation,
        tim: decoded,
        geometry,
    })
}

/// Parse the animation blob at `base`.
///
/// Each depth-table entry is a dword offset from `base`; rows that point back
/// inside the eight-byte table are unused and stay empty, matching the shipped
/// data (the original would read the table bytes as a frame count).
fn parse_animation(data: &[u8], base: usize) -> Result<SpriteAnimation> {
    let table = data
        .get(base..base + 8)
        .context("depth table is truncated")?;
    let mut animation = SpriteAnimation::default();
    animation.depth_table.copy_from_slice(table);

    for (row, &entry) in animation.depth_table.iter().enumerate() {
        let row_offset = usize::from(entry) * 4;
        if row_offset < 8 {
            continue;
        }
        let row_at = base + row_offset;
        let raw = data
            .get(row_at..row_at + 4)
            .with_context(|| format!("depth row {row} frame count is truncated"))?;
        let frame_count = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        // One frame is at least its 4-byte block count.
        if frame_count as usize > data.len() / 4 {
            bail!("depth row {row} claims {frame_count} frames, past the file end");
        }
        let mut cursor = row_at + 4;
        let mut frames = Vec::new();
        for frame in 0..frame_count {
            let raw = data
                .get(cursor..cursor + 4)
                .with_context(|| format!("depth row {row} frame {frame} is truncated"))?;
            let block_count = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
            cursor += 4;
            let bytes = block_count
                .checked_mul(EFFECT_BLOCK_LEN)
                .context("frame block count overflows")?;
            let end = cursor
                .checked_add(bytes)
                .context("frame block data overflows")?;
            let blocks = data
                .get(cursor..end)
                .with_context(|| format!("depth row {row} frame {frame} blocks are truncated"))?;
            frames.push(AnimFrame {
                blocks: blocks
                    .as_chunks::<EFFECT_BLOCK_LEN>()
                    .0
                    .iter()
                    .map(|block| EffectBlock(*block))
                    .collect(),
            });
            cursor = end;
        }
        animation.depth_frames[row] = frames;
    }
    Ok(animation)
}

/// Parse an embedded 4bpp TIM's geometry and decode its art.
///
/// A malformed TIM yields a zero geometry and no decoded art; the sprite still
/// parses so the rest of the table stays usable.
fn parse_tim(data: &[u8]) -> (TimGeometry, Option<Texture8>) {
    let geometry = tim_geometry(data).unwrap_or_default();
    (geometry, tim::decode_4bpp(data).ok())
}

/// Read a 4bpp TIM's image geometry without decoding the pixels.
fn tim_geometry(data: &[u8]) -> Result<TimGeometry> {
    let header = data.get(..20).context("TIM header is truncated")?;
    if u32::from_le_bytes(header[0..4].try_into().unwrap()) != 0x10 {
        bail!("not a TIM: bad magic");
    }
    let flags = u32::from_le_bytes(header[4..8].try_into().unwrap());
    if flags & 7 != 0 {
        bail!("not a 4bpp TIM: flags 0x{flags:02X}");
    }
    if flags & 8 == 0 {
        bail!("4bpp TIM has no CLUT block");
    }
    let clut_length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    let clut_rows = u16::from_le_bytes([header[18], header[19]]);
    let image = 8usize
        .checked_add(clut_length)
        .context("TIM CLUT length overflows")?;
    let block = data
        .get(image..image + 12)
        .context("TIM image block is truncated")?;
    let width_units = u16::from_le_bytes([block[8], block[9]]);
    let height = u16::from_le_bytes([block[10], block[11]]);
    Ok(TimGeometry {
        width: width_units.saturating_mul(4),
        height,
        clut_rows: clut_rows.min(u16::from(u8::MAX)) as u8,
    })
}

/// Read slot `slot`'s offset from the backward table ending at `ptr`.
///
/// Slot 0's dword is at `ptr`, slot `i`'s at `ptr - 4 * i`; negative addresses
/// and reads past the file end are refused.
fn read_backward(data: &[u8], ptr: usize, slot: usize, what: &str) -> Result<usize> {
    let address = ptr as i64 - 4 * slot as i64;
    if address < 0 {
        bail!("{what} table pointer 0x{ptr:x} underflows at slot {slot}");
    }
    let address = address as usize;
    let end = address
        .checked_add(4)
        .with_context(|| format!("{what} offset for slot {slot} overflows"))?;
    let raw = data
        .get(address..end)
        .with_context(|| format!("{what} offset for slot {slot} is out of bounds"))?;
    let offset = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    usize::try_from(offset).with_context(|| format!("{what} offset {offset} is negative"))
}

/// Round `value` up to the next multiple of four.
fn align4(value: usize) -> usize {
    value.saturating_add(3) & !3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::fixtures;

    const INDEX_PTR: u32 = 0x100;
    const INFO_PTR: u32 = 0x130;
    const TIM_PTR: u32 = 0x140;
    const TIM_OFFSET: usize = 0x200;

    fn block(anim_id: u8, update_id: u8, yaw: i16) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[0] = anim_id;
        bytes[1] = update_id;
        bytes[18..20].copy_from_slice(&yaw.to_le_bytes());
        bytes
    }

    /// A synthetic RDT carrying one effect sprite (index 7).
    fn synthetic_rdt() -> Vec<u8> {
        let mut data = vec![0u8; TIM_OFFSET + 0x100];
        // Sprite-info record at 0x48: uv_count 2, frame_count 2.
        data[0x48] = 2;
        data[0x4A..0x4C].copy_from_slice(&2u16.to_le_bytes());
        data[0x4C..0x4E].copy_from_slice(&0x7840u16.to_le_bytes());
        // Frame table at 0x50.
        data[0x50..0x54].copy_from_slice(&[0, 3, 0, 0]);
        data[0x54..0x58].copy_from_slice(&[1, 0xFF, 0, 0]);
        // UV records at 0x58.
        data[0x58..0x5C].copy_from_slice(&[10, 20, 30, 40]);
        data[0x5C..0x60].copy_from_slice(&[50, 60, 70, 80]);
        // Animation blob at 0x60: rows 0/1/3 share offset 8 (0x68), row 2 is
        // unused (points into the table itself) and rows 4-7 are unused too.
        data[0x60..0x68].copy_from_slice(&[2, 2, 0, 2, 0, 0, 0, 0]);
        data[0x68..0x6C].copy_from_slice(&1u32.to_le_bytes());
        data[0x6C..0x70].copy_from_slice(&2u32.to_le_bytes());
        data[0x70..0x88].copy_from_slice(&block(4, 0, 0));
        data[0x88..0xA0].copy_from_slice(&block(5, 1, 90));
        // Index table at 0x100, info and TIM pointers read backwards.
        data[0x100] = 7;
        data[0x101..0x108].fill(0xFF);
        data[0x110..0x130].fill(0xFF);
        data[0x120..0x140].fill(0xFF);
        data[0x130..0x134].copy_from_slice(&0x48i32.to_le_bytes());
        data[0x140..0x144].copy_from_slice(&(TIM_OFFSET as i32).to_le_bytes());
        data[TIM_OFFSET..TIM_OFFSET + fixtures::tim_bytes(8, 4, 2).len()]
            .copy_from_slice(&fixtures::tim_bytes(8, 4, 2));
        data
    }

    #[test]
    fn parses_sprite_info_uv_records_and_blocks() {
        let data = synthetic_rdt();
        let effects = RoomEffects::parse(&data, INDEX_PTR, INFO_PTR, TIM_PTR);

        assert!(effects.warnings.is_empty(), "{:?}", effects.warnings);
        assert_eq!(effects.index[0], 7);
        assert_eq!(&effects.index[1..], &[0xFF; 7]);
        assert_eq!(effects.sprites.len(), 1);
        let sprite = effects.sprite(7).unwrap();
        assert_eq!(sprite.info.uv_count, 2);
        assert_eq!(
            sprite.info.frames,
            [
                FrameEntry {
                    uv_index: 0,
                    delay: 3
                },
                FrameEntry {
                    uv_index: 1,
                    delay: 0xFF
                },
            ]
        );
        assert_eq!(
            sprite.info.uvs,
            [
                UvRecord {
                    u: 10,
                    v: 20,
                    pivot_x: 30,
                    pivot_y: 40
                },
                UvRecord {
                    u: 50,
                    v: 60,
                    pivot_x: 70,
                    pivot_y: 80
                },
            ]
        );
        assert_eq!(sprite.info.page_clut_word, 0x7840);
        assert_eq!(sprite.info.page_id, 0);
        assert_eq!(sprite.anim.depth_table, [2, 2, 0, 2, 0, 0, 0, 0]);
        let row = sprite.anim.depth(0).unwrap();
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].blocks.len(), 2);
        assert_eq!(row[0].blocks[0].anim_id(), 4);
        assert_eq!(row[0].blocks[1].anim_id(), 5);
        assert_eq!(row[0].blocks[1].update_id(), 1);
        assert_eq!(row[0].blocks[1].yaw(), 90);
        assert_eq!(sprite.anim.depth(1).unwrap().len(), 1);
        assert!(sprite.anim.depth(2).unwrap().is_empty());
        assert!(sprite.anim.depth(7).unwrap().is_empty());
        assert_eq!(
            sprite.geometry,
            TimGeometry {
                width: 8,
                height: 4,
                clut_rows: 2
            }
        );
        let texture = sprite.tim.as_ref().expect("TIM decodes");
        assert_eq!((texture.width, texture.height), (8, 4));
    }

    #[test]
    fn a_malformed_animation_row_becomes_a_warning() {
        let mut data = synthetic_rdt();
        data[0x60] = 0xFF; // row 0 points past the end of the file
        let effects = RoomEffects::parse(&data, INDEX_PTR, INFO_PTR, TIM_PTR);
        assert!(effects.sprites.is_empty());
        assert_eq!(effects.warnings.len(), 1, "{:?}", effects.warnings);
        assert!(effects.warnings[0].contains("effect sprite 7"));
    }

    #[test]
    fn zero_pointers_declare_no_effects() {
        let effects = RoomEffects::parse(&[], 0, 0, 0);
        assert!(effects.is_empty());
        assert!(effects.warnings.is_empty());
    }

    #[test]
    fn a_malformed_tim_keeps_the_sprite_without_art() {
        let mut data = synthetic_rdt();
        data[TIM_OFFSET..TIM_OFFSET + 4].fill(0);
        let effects = RoomEffects::parse(&data, INDEX_PTR, INFO_PTR, TIM_PTR);
        assert_eq!(effects.sprites.len(), 1);
        let sprite = effects.sprite(7).unwrap();
        assert!(sprite.tim.is_none());
        assert_eq!(sprite.geometry, TimGeometry::default());
    }

    /// Build a synthetic `core00.esp` with `entries` of `(index, info_offset)`.
    fn synthetic_esp(entries: &[(u8, usize)]) -> Vec<u8> {
        let mut data = vec![0u8; 0x200];
        data[..8].fill(0xFF);
        for (slot, &(index, _)) in entries.iter().enumerate() {
            data[slot] = index;
        }
        for &(_, offset) in entries {
            // Info record, frame table, one UV record, then the animation blob
            // at `offset + 8 + (uv_count + frame_count) * 4` = offset + 16.
            data[offset] = 1;
            data[offset + 2..offset + 4].copy_from_slice(&1u16.to_le_bytes());
            data[offset + 8..offset + 12].copy_from_slice(&[0, 9, 0, 0]);
            data[offset + 12..offset + 16].copy_from_slice(&[1, 2, 3, 4]);
            data[offset + 16..offset + 24].copy_from_slice(&[2, 2, 2, 2, 2, 2, 2, 2]);
            data[offset + 24..offset + 28].copy_from_slice(&1u32.to_le_bytes());
            data[offset + 28..offset + 32].copy_from_slice(&1u32.to_le_bytes());
            data[offset + 32..offset + 56].copy_from_slice(&block(6, 0, 0));
        }
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }
        // Pointers read backwards from the last dword: slot i at end - 4 - 4i.
        for &(_, offset) in entries.iter().rev() {
            data.extend_from_slice(&(offset as i32).to_le_bytes());
        }
        data
    }

    #[test]
    fn parses_core00_sprite_info_and_art() {
        let esp = synthetic_esp(&[(5, 0x80), (9, 0x100)]);
        let mut etm = fixtures::tim_bytes(256, 16, 1);
        let tim0 = 0usize;
        let tim1 = etm.len();
        etm.extend_from_slice(&fixtures::tim_bytes(256, 24, 3));
        while !etm.len().is_multiple_of(4) {
            etm.push(0);
        }
        // TIM pointers: slot 0 at the last dword, slot 1 before it.
        etm.extend_from_slice(&(tim1 as i32).to_le_bytes());
        etm.extend_from_slice(&(tim0 as i32).to_le_bytes());

        let effects = WeaponEffects::parse(&esp, &etm);

        assert!(effects.warnings.is_empty(), "{:?}", effects.warnings);
        assert_eq!(&effects.index[..2], &[5, 9]);
        assert_eq!(effects.sprites.len(), 2);
        assert_eq!(effects.slot_of(9), Some(1));
        let sprite = effects.sprite(9).unwrap();
        assert_eq!(sprite.anim.depth_table, [2; 8]);
        assert_eq!(sprite.geometry.height, 24);
        assert_eq!(sprite.geometry.clut_rows, 3);
        assert!(sprite.tim.is_some());
        assert_eq!(effects.sprite(5).unwrap().geometry.height, 16);
    }

    #[test]
    fn core00_load_reports_a_missing_file_without_failing() {
        let pack =
            crate::pack::Pack::from_bytes(crate::pack::PackWriter::new().to_bytes().unwrap())
                .unwrap();
        let effects = WeaponEffects::load(&pack);
        assert!(effects.sprites.is_empty());
        assert_eq!(effects.warnings.len(), 1);
        assert!(effects.warnings[0].contains(CORE_ESP_ENTRY));
    }
}
