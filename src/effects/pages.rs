//! Effect-sheet page packing and the executable's static effect tables.
//!
//! The original packs every declared effect sprite into 256-tall texture
//! pages in two passes: the eight global weapon-FX sheets first (page cursor
//! starting at `0x18`, V cursor at 4), then the room's declared sprites on the
//! same shared cursor. Each sprite's TIM starts at the current V and the
//! cursors advance by the TIM's height and CLUT row count; overflows reset V
//! to 3 and step the page, or reset the CLUT cursor to 4 and step the page
//! row. The assigned page id and V offset are written into the sprite-info
//! record, and room sprites additionally get their UV records' V bytes biased
//! by the assigned offset (their art is blitted into a shared page; weapon
//! sheets keep local UVs because each has its own texture).
//!
//! The static tables below are the executable's numeric data: the
//! `(stage, room) -> four sheet names` map, the blend bands and colour-tint
//! records, the camera light-record index and the weapon-sheet regions. The
//! renderer reads them for the room→sheet lookup, the blend mode and the
//! per-spawn tint.

use crate::effects::room::{EFFECT_SPRITE_SLOTS, RoomEffects, WeaponEffects};

/// The original's `texY` bias: page 0 is stored as `0x18`.
pub const PAGE_BIAS: u8 = 0x18;

/// V cursor reset after a page overflow.
const PAGE_RESET_V: u16 = 3;
/// CLUT V cursor reset after a page-row overflow.
const CLUT_RESET_V: u16 = 4;
/// V cursor page height.
const PAGE_HEIGHT: u16 = 0x100;
/// CLUT V cursor page-row height.
const CLUT_PAGE_ROWS: u16 = 0x1F;
/// Constant added to the packed page/CLUT word.
const PAGE_CLUT_BIAS: u16 = 0x7810;

/// One colour-tint record: how many tint levels it carries and their RGB
/// triplets.
#[derive(Debug, Clone, Copy)]
pub struct EffectColorRecord {
    /// Number of levels (entries in `rgb`).
    pub count: usize,
    /// The RGB triplets, level 0 first.
    pub rgb: &'static [[u8; 3]],
}

impl EffectColorRecord {
    /// The RGB level `level`, clamped to the record's last entry.
    pub fn level(&self, level: usize) -> [u8; 3] {
        if self.rgb.is_empty() {
            return [0xFF, 0xFF, 0xFF];
        }
        self.rgb[level.min(self.count.saturating_sub(1))]
    }
}

/// The texture page and V offset assigned to one sheet by the packing cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Placement {
    /// The original's `texY`, biased by [`PAGE_BIAS`].
    pub page: u8,
    /// V offset down the page.
    pub v: u8,
    /// The page/CLUT descriptor word written into the sprite record.
    pub page_clut_word: u16,
}

impl Placement {
    /// The zero-based effspr page index (`page - PAGE_BIAS`).
    pub fn page_index(&self) -> u8 {
        self.page.wrapping_sub(PAGE_BIAS)
    }
}

/// The shared two-pass packing cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackCursor {
    /// Page id (the original's `texY`).
    pub page: u8,
    /// Texture X within the page (the original's `texX`).
    pub tex_x: u16,
    /// CLUT V cursor (the original's `curV`).
    pub clut_v: u16,
    /// CLUT page row (the original's `pageRow`).
    pub page_row: u8,
    /// V cursor down the page (the original's `curU`).
    pub v: u16,
}

impl PackCursor {
    /// The weapon pass's starting cursor.
    pub fn weapon_start() -> Self {
        Self {
            page: PAGE_BIAS,
            tex_x: 0,
            clut_v: CLUT_RESET_V,
            page_row: 0,
            v: 0,
        }
    }

    /// Place one sprite whose TIM is `height` pixels tall with `clut_rows`
    /// CLUT rows, applying the original's overflow resets, then advance the
    /// cursors past it.
    pub fn place(&mut self, height: u16, clut_rows: u16) -> Placement {
        if u32::from(height) + u32::from(self.v) > u32::from(PAGE_HEIGHT) {
            self.v = PAGE_RESET_V;
            self.page = self.page.wrapping_add(1);
            self.tex_x = self.tex_x.wrapping_add(0x40);
        }
        if u32::from(clut_rows) + u32::from(self.clut_v) > u32::from(CLUT_PAGE_ROWS) {
            self.clut_v = CLUT_RESET_V;
            self.page_row = self.page_row.wrapping_add(1);
        }
        let placement = Placement {
            page: self.page,
            v: self.v as u8,
            page_clut_word: self
                .clut_v
                .wrapping_mul(0x40)
                .wrapping_add(u16::from(self.page_row))
                .wrapping_add(PAGE_CLUT_BIAS),
        };
        self.v = self.v.wrapping_add(height);
        self.clut_v = self.clut_v.wrapping_add(clut_rows);
        placement
    }
}

/// Run both packing passes: the weapon sheets first, then the room's declared
/// sprites, writing the page id, V offset and page/CLUT word into the room
/// sprite records and biasing the room sprites' UV V bytes.
///
/// Returns the cursor left by the room pass. Sprites with no TIM geometry
/// (a pack converted without `core00.etm`, or a malformed embedded TIM) are
/// skipped so the pass cannot corrupt the cursor.
pub fn pack(weapon: &WeaponEffects, room: &mut RoomEffects) -> PackCursor {
    let mut cursor = PackCursor::weapon_start();
    for slot in 0..EFFECT_SPRITE_SLOTS {
        let Some(&index) = weapon.index.get(slot) else {
            break;
        };
        if index == 0xFF {
            break;
        }
        let Some(sprite) = weapon.sprite(index) else {
            continue;
        };
        if sprite.geometry.height == 0 && sprite.geometry.clut_rows == 0 {
            continue;
        }
        cursor.place(sprite.geometry.height, u16::from(sprite.geometry.clut_rows));
    }

    for slot in 0..EFFECT_SPRITE_SLOTS {
        let Some(&index) = room.index.get(slot) else {
            break;
        };
        if index == 0xFF {
            break;
        }
        let Some(sprite) = room.sprite_mut(index) else {
            continue;
        };
        if sprite.geometry.height == 0 && sprite.geometry.clut_rows == 0 {
            continue;
        }
        let placement = cursor.place(sprite.geometry.height, u16::from(sprite.geometry.clut_rows));
        sprite.info.page_clut_word = placement.page_clut_word;
        sprite.info.page_id = placement.page;
        sprite.info.page_v = placement.v;
        for uv in &mut sprite.info.uvs {
            uv.v = uv.v.wrapping_add(placement.v);
        }
    }
    cursor
}

/// The effect-sheet name for `(stage, room, page)`, or `None` when the slot is
/// free (`0xFF`) or the indices are out of range.
///
/// `stage` is the zero-based stage id and `room` the room byte; `page` is the
/// zero-based effspr page index (0-3).
pub fn room_effect_sheet(stage: usize, room: usize, page: usize) -> Option<&'static str> {
    let row = ROOM_EFFECT_SHEETS.get(stage.checked_mul(32)?.checked_add(room)?)?;
    let name = row.get(page)?;
    if *name == 0xFF {
        return None;
    }
    EFFECT_SHEET_NAMES.get(usize::from(*name)).copied()
}

/// The fixed `(page, V)` region of weapon sheet `slot` (`0..8`).
pub fn weapon_sheet_region(slot: usize) -> Option<(u8, u16)> {
    Some((
        *WEAPON_SHEET_PAGE.get(slot)?,
        *WEAPON_SHEET_PAGE_V.get(slot)?,
    ))
}

/// The first blend record of `depth_slot` whose band end is above `band_v`, as
/// `(blend_mode, color_idx, start_v, len)`.
///
/// `depth_slot` is the packed page id minus [`PAGE_BIAS`]; the record scan
/// reproduces the original's one-sided `band_v < start_v + len` test in table
/// order, so a V below a record's start still selects it.
pub fn blend_record(depth_slot: usize, band_v: u8) -> Option<(u8, u8, u8, u8)> {
    let start = usize::from(*EFFECT_BLEND_START.get(depth_slot)?);
    let count = usize::from(*EFFECT_BLEND_COUNT.get(depth_slot)?);
    for row in EFFECT_BLEND_TABLE.iter().skip(start).take(count) {
        let (start_v, len, mode, color) = (row[0], row[1], row[2], row[3]);
        if u16::from(band_v) < u16::from(start_v) + u16::from(len) {
            return Some((mode, color, start_v, len));
        }
    }
    None
}

/// The camera light record used to shade effects in `(stage, room, camera)`.
pub fn camera_light_record(stage: usize, room: usize, camera: usize) -> [i16; 3] {
    let index = (stage * 32 + room) * 8 + camera;
    EFFECT_CAMERA_LIGHT_INDEX
        .get(index)
        .copied()
        .map(|record| EFFECT_LIGHT_RECORDS[usize::from(record)])
        .unwrap_or([0, 0, 0])
}

/// The 7x32x4 room to effect-sheet map: `(stage * 32 + room) * 4 + page`.
pub const ROOM_EFFECT_SHEETS: [[u8; 4]; 7 * 32] = [
    [0x00, 0x02, 0xFF, 0xFF],
    [0x00, 0x03, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x05, 0x06, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x03, 0xFF, 0xFF],
    [0x00, 0x07, 0x08, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x09, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0x0A, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x09, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0xFF, 0xFF, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x03, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x02, 0xFF, 0xFF],
    [0x00, 0x03, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x0B, 0xFF, 0xFF],
    [0x00, 0x04, 0x0C, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x03, 0xFF, 0xFF],
    [0x00, 0x0D, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0x0A, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x09, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x0E, 0x0F, 0x10],
    [0x00, 0x04, 0x11, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0x12, 0xFF],
    [0x00, 0x13, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x14, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x13, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x15, 0x16, 0xFF],
    [0x00, 0x18, 0xFF, 0xFF],
    [0x00, 0x18, 0xFF, 0xFF],
    [0x00, 0x18, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x19, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x19, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x03, 0x1A, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x03, 0x1A, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x19, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x19, 0xFF, 0xFF],
    [0x00, 0x1B, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0x1C, 0xFF],
    [0x00, 0x04, 0x1C, 0xFF],
    [0x00, 0x04, 0x1C, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0x1D, 0x1E],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x13, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x07, 0x08, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0x0A, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x03, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x03, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x0B, 0xFF, 0xFF],
    [0x00, 0x04, 0x0C, 0xFF],
    [0x00, 0x01, 0x1F, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x03, 0xFF, 0xFF],
    [0x00, 0x0D, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0x0A, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x04, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
    [0x00, 0x01, 0xFF, 0xFF],
];

/// The 32 effect-sheet names indexed by the room map.
pub const EFFECT_SHEET_NAMES: [&str; 32] = [
    "esp000", "esp001", "esp200", "esp201", "esp202", "esp203", "esp204", "esp205", "esp206",
    "esp207", "esp208", "esp209", "esp210", "esp211", "esp212", "esp213", "esp214", "esp215",
    "esp216", "esp217", "esp218", "esp219", "esp220", "esp221", "esp222", "esp223", "esp225",
    "esp226", "esp227", "esp228", "esp229", "esp230",
];

/// Weapon-FX sheets are fixed regions of two effspr pages.
pub const WEAPON_SHEET_PAGE: [u8; 8] = [0, 0, 0, 0, 1, 1, 1, 1];
pub const WEAPON_SHEET_PAGE_V: [u16; 8] = [0, 64, 176, 240, 3, 27, 99, 123];

/// The 124 packed blend records `[startV, len, blendMode, colorIdx]`.
pub const EFFECT_BLEND_TABLE: [[u8; 4]; 124] = [
    [0x00, 0x40, 0x00, 0x00],
    [0x40, 0x70, 0x80, 0x01],
    [0xB0, 0x40, 0x80, 0x02],
    [0xF0, 0x10, 0x80, 0x03],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x18, 0x00, 0x08],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x28, 0x00, 0x09],
    [0xBB, 0x18, 0x00, 0x0A],
    [0xD3, 0x20, 0x00, 0x0B],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x28, 0x00, 0x0C],
    [0xBB, 0x18, 0x00, 0x0D],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x18, 0x00, 0x0E],
    [0xAB, 0x18, 0x00, 0x0F],
    [0xC3, 0x20, 0x00, 0x10],
    [0x03, 0x20, 0x00, 0x11],
    [0x23, 0x20, 0x00, 0x12],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x48, 0x80, 0x13],
    [0x03, 0x50, 0x80, 0x14],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x18, 0x00, 0x15],
    [0x03, 0x78, 0x00, 0x16],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x10, 0x80, 0x17],
    [0x03, 0x40, 0x80, 0x18],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x28, 0x00, 0x19],
    [0xBB, 0x18, 0x00, 0x1A],
    [0xD3, 0x10, 0x80, 0x1B],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x18, 0x00, 0x1C],
    [0xAB, 0x40, 0x00, 0x1D],
    [0x03, 0xE8, 0x80, 0x1E],
    [0x03, 0x40, 0x00, 0x1F],
    [0x03, 0x40, 0x00, 0x20],
    [0x43, 0x20, 0x00, 0x21],
    [0x63, 0x20, 0x00, 0x22],
    [0x03, 0x40, 0x00, 0x23],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x50, 0x00, 0x24],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x40, 0x00, 0x25],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x40, 0x00, 0x26],
    [0x03, 0x50, 0x00, 0x27],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x40, 0x00, 0x28],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x40, 0x00, 0x29],
    [0xD3, 0x10, 0x80, 0x2A],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x18, 0x00, 0x2B],
    [0xAB, 0x40, 0x00, 0x2C],
    [0x03, 0x18, 0x00, 0x2D],
    [0x1B, 0x48, 0x00, 0x2E],
    [0x03, 0x18, 0x00, 0x04],
    [0x1B, 0x48, 0x80, 0x05],
    [0x63, 0x18, 0x80, 0x06],
    [0x7B, 0x18, 0x80, 0x07],
    [0x93, 0x28, 0x00, 0x2F],
    [0xBB, 0x18, 0x00, 0x30],
    [0xD3, 0x18, 0x00, 0x31],
    [0x03, 0x48, 0x00, 0x32],
    [0x4B, 0x18, 0x80, 0x33],
    [0x03, 0xFD, 0x00, 0x34],
    [0x03, 0x18, 0x00, 0x35],
    [0x1B, 0x20, 0x00, 0x36],
    [0x3B, 0x20, 0x00, 0x37],
    [0x5B, 0x20, 0x00, 0x38],
    [0x7B, 0x10, 0x80, 0x39],
    [0x8B, 0x18, 0x00, 0x3A],
    [0x03, 0xF0, 0x00, 0x3B],
];

pub const EFFECT_BLEND_COUNT: [u8; 32] = [
    4, 4, 5, 7, 6, 7, 2, 5, 1, 5, 1, 5, 1, 7, 6, 1, 1, 3, 1, 5, 5, 5, 1, 5, 6, 6, 2, 7, 2, 1, 6, 1,
];
pub const EFFECT_BLEND_START: [u8; 32] = [
    0, 4, 8, 13, 20, 26, 33, 35, 40, 41, 46, 47, 52, 53, 60, 66, 67, 68, 71, 72, 77, 82, 87, 88,
    93, 99, 105, 107, 114, 116, 117, 123,
];

/// The 60 colour-tint records (level count plus RGB triplets).
pub const EFFECT_COLOR_RECORDS: [EffectColorRecord; 60] = [
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xFF, 0xFF, 0xFF],
            [0xB2, 0xB2, 0xB2],
            [0xFF, 0xCC, 0x66],
            [0xCC, 0xCC, 0x33],
        ],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xFF, 0x99, 0x99], [0x99, 0x99, 0xFF]],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0x69, 0x1E, 0x0A],
            [0x37, 0x5A, 0x14],
            [0x91, 0x5A, 0x14],
            [0xFF, 0xFF, 0xFF],
        ],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0xFF, 0xFF, 0xFF], [0xFF, 0xFF, 0xFF], [0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xFF, 0xFF, 0xFF],
            [0x7F, 0x7F, 0x7F],
            [0xCC, 0xFF, 0xCC],
            [0xFF, 0xFF, 0x99],
        ],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xB2, 0xB2, 0xFF],
            [0xFF, 0xAF, 0x69],
            [0xFF, 0xFF, 0xFF],
            [0xFF, 0xFF, 0x7F],
        ],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xB2, 0xB2, 0xFF],
            [0xFF, 0xB2, 0xB2],
            [0xFF, 0xFF, 0xFF],
            [0xFF, 0xFF, 0xB2],
        ],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xD2, 0xA0, 0x28], [0x66, 0xB2, 0x66]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xB4, 0xBE, 0xFF], [0xFF, 0x73, 0x23]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xB4, 0xBE, 0xFF], [0xFF, 0x73, 0x23]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xFF, 0xFF, 0xFF], [0xB4, 0xC8, 0xB4]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xFF, 0xFF, 0xFF], [0x6E, 0x78, 0xAE]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xFF, 0xFF, 0xFF], [0xB4, 0xC8, 0xB4]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xFF, 0xFF, 0xFF], [0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0xFF, 0xFF, 0xFF], [0xCC, 0xFF, 0xCC], [0xFF, 0xB2, 0xB2]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xB2, 0xB2, 0xFF],
            [0xB2, 0xB2, 0xFF],
            [0xCC, 0xCC, 0xFF],
            [0xFF, 0xFF, 0xFF],
        ],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0xFF, 0xFF, 0xFF], [0xCC, 0xFF, 0xCC], [0xFF, 0xB2, 0xB2]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xFF, 0xFF, 0xFF], [0xB4, 0xC8, 0xB4]],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xFF, 0xFF, 0xFF], [0xB4, 0xC8, 0xB4]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xFF, 0xB2, 0xB2],
            [0xB2, 0xB2, 0xFF],
            [0xFF, 0x66, 0x66],
            [0x93, 0x66, 0xFF],
        ],
    },
    EffectColorRecord {
        count: 2,
        rgb: &[[0xFF, 0xFF, 0xFF], [0xB4, 0xC8, 0xB4]],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xFF, 0xB2, 0xB2],
            [0xB2, 0xB2, 0xFF],
            [0xFF, 0x66, 0x66],
            [0xA0, 0xB4, 0xFF],
        ],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 3,
        rgb: &[[0x99, 0x33, 0x33], [0x31, 0x8C, 0x2D], [0xC1, 0xA0, 0x33]],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xFF, 0xB2, 0xB2],
            [0xB2, 0xB2, 0xFF],
            [0xFF, 0x66, 0x66],
            [0x93, 0x66, 0xFF],
        ],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xFF, 0xFF, 0xFF],
            [0x64, 0x64, 0x64],
            [0xD2, 0xA0, 0x8C],
            [0xD2, 0xAA, 0x46],
        ],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
    EffectColorRecord {
        count: 4,
        rgb: &[
            [0xFF, 0xB2, 0xB2],
            [0xB2, 0xB2, 0xFF],
            [0xFF, 0x66, 0x66],
            [0x93, 0x66, 0xFF],
        ],
    },
    EffectColorRecord {
        count: 1,
        rgb: &[[0xFF, 0xFF, 0xFF]],
    },
];

/// The 5x32x8 camera to light-record index.
pub const EFFECT_CAMERA_LIGHT_INDEX: [u8; 5 * 32 * 8] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    6, 6, 6, 6, 6, 0, 6, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0,
    3, 3, 3, 3, 3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    5, 5, 5, 5, 5, 5, 5, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// The seven camera light records `[scale_x_add, scale_y_add, brightness]`.
pub const EFFECT_LIGHT_RECORDS: [[i16; 3]; 7] = [
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [-1000, 0, 1000],
    [-4000, 0, 0],
    [-2500, 0, 2500],
    [0, 0, 60],
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::fixtures;
    use crate::effects::room::TimGeometry;

    fn empty_rows() -> [Vec<Vec<[u8; 24]>>; 8] {
        std::array::from_fn(|_| Vec::new())
    }

    fn weapon_sprite(index: u8, height: u16, clut_rows: u8) -> crate::effects::room::EffectSprite {
        let mut sprite = fixtures::sprite(index, empty_rows());
        sprite.geometry = TimGeometry {
            width: 256,
            height,
            clut_rows,
        };
        sprite
    }

    #[test]
    fn room_map_rows_carry_the_stage_variant() {
        // Every non-empty row starts on esp000 (index 0).
        for row in ROOM_EFFECT_SHEETS {
            if row[0] != 0xFF {
                assert_eq!(row[0], 0, "page 0 must be esp000");
            }
        }
        assert_eq!(ROOM_EFFECT_SHEETS[32], [0, 1, 0xFF, 0xFF]); // stage 1 room 0
        assert_eq!(ROOM_EFFECT_SHEETS[33], [0, 3, 0xFF, 0xFF]); // ROOM1010
        assert_eq!(ROOM_EFFECT_SHEETS[66], [0, 14, 15, 16]); // stage 2 room 2
        assert_eq!(room_effect_sheet(1, 0, 0), Some("esp000"));
        assert_eq!(room_effect_sheet(1, 0, 1), Some("esp001"));
        assert_eq!(room_effect_sheet(1, 1, 1), Some("esp201"));
        assert_eq!(room_effect_sheet(2, 2, 3), Some("esp214"));
        assert_eq!(room_effect_sheet(1, 0, 2), None);
    }

    #[test]
    fn room_map_has_the_shipped_shape() {
        let distinct: std::collections::BTreeSet<[u8; 4]> =
            ROOM_EFFECT_SHEETS.iter().copied().collect();
        assert_eq!(distinct.len(), 25);
        assert_eq!(ROOM_EFFECT_SHEETS.len(), 7 * 32);
        for name in distinct
            .iter()
            .flat_map(|row| row.iter())
            .filter(|&&name| name != 0xFF)
        {
            assert!(usize::from(*name) < EFFECT_SHEET_NAMES.len());
        }
        assert!(EFFECT_SHEET_NAMES.contains(&"esp000"));
        assert!(EFFECT_SHEET_NAMES.contains(&"esp230"));
        assert!(!EFFECT_SHEET_NAMES.contains(&"esp224"));
    }

    #[test]
    fn blend_start_and_count_tile_the_table() {
        assert_eq!(
            EFFECT_BLEND_COUNT
                .iter()
                .map(|&c| usize::from(c))
                .sum::<usize>(),
            124
        );
        assert_eq!(EFFECT_BLEND_START[0], 0);
        for index in 0..31 {
            assert_eq!(
                usize::from(EFFECT_BLEND_START[index]) + usize::from(EFFECT_BLEND_COUNT[index]),
                usize::from(EFFECT_BLEND_START[index + 1]),
            );
        }
        assert_eq!(
            usize::from(EFFECT_BLEND_START[31]) + usize::from(EFFECT_BLEND_COUNT[31]),
            124
        );
    }

    #[test]
    fn blend_record_scan_uses_the_band_end() {
        // Slot 0: (0,0x40,opaque,0), (0x40,0x70,half,1), ...
        assert_eq!(blend_record(0, 0), Some((0x00, 0, 0x00, 0x40)));
        assert_eq!(blend_record(0, 0x3F), Some((0x00, 0, 0x00, 0x40)));
        assert_eq!(blend_record(0, 0x40), Some((0x80, 1, 0x40, 0x70)));
        assert_eq!(blend_record(0, 0xFF), Some((0x80, 3, 0xF0, 0x10)));
        // The scan tests `band_v < start_v + len` only, so V below the start
        // still matches the row; the band end is the cull.
        assert_eq!(blend_record(31, 0x02), Some((0x00, 0x3B, 0x03, 0xF0)));
        assert_eq!(blend_record(31, 0xF2), Some((0x00, 0x3B, 0x03, 0xF0)));
        assert_eq!(blend_record(31, 0xF3), None);
        assert_eq!(blend_record(32, 0), None);
    }

    #[test]
    fn tint_records_are_clamped() {
        assert_eq!(EFFECT_COLOR_RECORDS.len(), 60);
        assert_eq!(EFFECT_COLOR_RECORDS[0].level(0), [0xFF, 0xFF, 0xFF]);
        assert_eq!(EFFECT_COLOR_RECORDS[0].level(9), [0xFF, 0xFF, 0xFF]);
        assert_eq!(EFFECT_COLOR_RECORDS[1].level(0), [0xFF, 0xFF, 0xFF]);
        assert_eq!(EFFECT_COLOR_RECORDS[1].level(1), [0xB2, 0xB2, 0xB2]);
        assert_eq!(EFFECT_COLOR_RECORDS[1].level(3), [0xCC, 0xCC, 0x33]);
        assert_eq!(EFFECT_COLOR_RECORDS[1].level(99), [0xCC, 0xCC, 0x33]);
        assert_eq!(EFFECT_COLOR_RECORDS[8].level(2), [0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn camera_light_records_index_by_stage_room_camera() {
        assert_eq!(camera_light_record(1, 12, 0), [-1000, 0, 1000]);
        assert_eq!(camera_light_record(0, 12, 0), [0, 0, 60]);
        assert_eq!(camera_light_record(3, 12, 0), [-2500, 0, 2500]);
        assert_eq!(camera_light_record(4, 19, 5), [0, 0, 0]);
        // Out-of-range indices fall back to the unlit record (0, 0, 0).
        assert_eq!(camera_light_record(9, 0, 0), [0, 0, 0]);
    }

    #[test]
    fn weapon_regions_match_the_page_layout() {
        assert_eq!(weapon_sheet_region(0), Some((0, 0)));
        assert_eq!(weapon_sheet_region(1), Some((0, 64)));
        assert_eq!(weapon_sheet_region(2), Some((0, 176)));
        assert_eq!(weapon_sheet_region(3), Some((0, 240)));
        assert_eq!(weapon_sheet_region(4), Some((1, 3)));
        assert_eq!(weapon_sheet_region(5), Some((1, 27)));
        assert_eq!(weapon_sheet_region(6), Some((1, 99)));
        assert_eq!(weapon_sheet_region(7), Some((1, 123)));
        assert_eq!(weapon_sheet_region(8), None);
    }

    #[test]
    fn cursor_applies_both_overflow_resets() {
        let mut cursor = PackCursor::weapon_start();
        let first = cursor.place(200, 4);
        assert_eq!((first.page, first.v), (0x18, 0));
        assert_eq!(first.page_clut_word, 4 * 0x40 + 0x7810);
        let second = cursor.place(100, 28);
        assert_eq!((second.page, second.v), (0x19, 3));
        assert_eq!(second.page_clut_word, 4 * 0x40 + 1 + 0x7810);
        assert_eq!(second.page_index(), 1);
        assert_eq!(cursor.v, 103);
        assert_eq!(cursor.clut_v, 32);
        assert_eq!(cursor.page_row, 1);
        assert_eq!(cursor.tex_x, 0x40);
    }

    #[test]
    fn weapon_pass_skips_sheets_without_geometry() {
        let mut weapon = WeaponEffects::default();
        weapon.index[0] = 5;
        weapon.index[1] = 9;
        weapon.sprites.push(weapon_sprite(5, 0, 0));
        weapon.sprites.push(weapon_sprite(9, 16, 2));
        let mut room = RoomEffects::default();
        let cursor = pack(&weapon, &mut room);
        assert_eq!(cursor.v, 16);
        assert_eq!(cursor.clut_v, 6);
    }

    #[test]
    fn room_pass_resumes_the_weapon_cursor_and_patches_uvs() {
        let mut weapon = WeaponEffects::default();
        weapon.index[0] = 5;
        weapon.sprites.push(weapon_sprite(5, 16, 4));
        let mut room = RoomEffects::default();
        room.index[0] = 3;
        let mut sprite = weapon_sprite(3, 64, 1);
        sprite.info.uvs = vec![
            crate::effects::room::UvRecord {
                u: 0,
                v: 32,
                pivot_x: 1,
                pivot_y: 2,
            },
            crate::effects::room::UvRecord {
                u: 8,
                v: 200,
                pivot_x: 3,
                pivot_y: 4,
            },
        ];
        room.sprites.push(sprite);

        pack(&weapon, &mut room);

        let sprite = room.sprite(3).unwrap();
        // The weapon pass leaves V at 16, so the room sprite starts there and
        // its UV records gain the same offset.
        assert_eq!(sprite.info.page_id, 0x18);
        assert_eq!(sprite.info.page_v, 16);
        assert_eq!(sprite.info.page_clut_word, 8 * 0x40 + 0x7810);
        assert_eq!(sprite.info.uvs[0].v, 48);
        assert_eq!(sprite.info.uvs[1].v, 216);
    }

    #[test]
    fn room_pass_wraps_onto_the_next_page() {
        let mut weapon = WeaponEffects::default();
        weapon.index[0] = 5;
        weapon.sprites.push(weapon_sprite(5, 200, 4));
        let mut room = RoomEffects::default();
        room.index[0] = 3;
        room.index[1] = 4;
        room.sprites.push(weapon_sprite(3, 100, 28));
        let mut second = weapon_sprite(4, 200, 1);
        second.info.uvs = vec![crate::effects::room::UvRecord {
            u: 0,
            v: 5,
            pivot_x: 0,
            pivot_y: 0,
        }];
        room.sprites.push(second);

        pack(&weapon, &mut room);

        let first = room.sprite(3).unwrap();
        assert_eq!((first.info.page_id, first.info.page_v), (0x19, 3));
        assert_eq!(first.info.page_clut_word, 4 * 0x40 + 1 + 0x7810);
        let second = room.sprite(4).unwrap();
        assert_eq!((second.info.page_id, second.info.page_v), (0x1A, 3));
        // The first room sprite's 28 CLUT rows push `curV` to 32, so the
        // second sprite's CLUT overflow resets the row to the next one.
        assert_eq!(second.info.page_clut_word, 4 * 0x40 + 2 + 0x7810);
        assert_eq!(second.info.uvs[0].v, 8);
    }
}
