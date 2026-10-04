//! Menu frame/layout tables and inventory cursor geometry.
//!
//! The original packs every pause-menu frame, border, decoration and black
//! masking rectangle into one 712-byte block and reads 12-byte texture entries
//! (position, size, source UV) or 14-byte tiled records (a texture entry plus a
//! flag byte and a repeat/direction byte) backwards from labels inside the
//! block. [`MENU_BLOCK`] transcribes that block byte for byte; the accessors
//! below decode it into draw-order entries.
//!
//! The inventory slot table follows the rects: four tab positions, eight
//! two-column item-slot positions (four rows of 2) and a four-short tail. A
//! character with six slots (Chris) uses the bottom three rows; a character
//! with eight (Jill) uses all four. The cursor is the original's odd value:
//! tabs are `0..=6`, item slots are `8 + 2 * slot`, left/right toggles bit 1
//! (`cursor ^ 2`) and up/down moves four (`cursor ± 4`), wrapping past the
//! slot count.

use crate::render::Framebuffer;

/// Bytes of the contiguous menu frame/layout block.
pub const MENU_BLOCK_BYTES: usize = 712;

/// The original's contiguous menu frame/layout block.
///
/// Regions (all read backwards by the original):
/// - `0..264` 22 plain 12-byte frame parts (main panel, health/portrait and
///   equipped-weapon frames, bottom decorations);
/// - `264..312` 4 plain 12-byte top-tab parts;
/// - `312..368` 4 tiled 14-byte records (health-bar and common frame tiles);
/// - `368..494` 9 tiled 14-byte records: the six-slot inventory border;
/// - `496..622` 9 tiled 14-byte records: the eight-slot inventory border;
/// - `624..656` 4 black masking rects;
/// - `656..712` the inventory slot positions and trailing tab data.
pub const MENU_BLOCK: [u8; MENU_BLOCK_BYTES] = [
    0x08, 0x00, 0x0C, 0x00, 0x08, 0x00, 0x08, 0x00, 0x50, 0x00, 0x78, 0x00, 0x08, 0x00, 0x84, 0x00,
    0x08, 0x00, 0x08, 0x00, 0x58, 0x00, 0x78, 0x00, 0xC0, 0x00, 0x0C, 0x00, 0x08, 0x00, 0x08, 0x00,
    0x60, 0x00, 0x70, 0x00, 0xC0, 0x00, 0x84, 0x00, 0x08, 0x00, 0x08, 0x00, 0x60, 0x00, 0x78, 0x00,
    0x10, 0x00, 0x0C, 0x00, 0x10, 0x00, 0x08, 0x00, 0x30, 0x00, 0x78, 0x00, 0x10, 0x00, 0x84, 0x00,
    0x10, 0x00, 0x08, 0x00, 0x40, 0x00, 0x78, 0x00, 0x08, 0x00, 0x14, 0x00, 0x18, 0x00, 0x70, 0x00,
    0x98, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x14, 0x00, 0x08, 0x00, 0x70, 0x00, 0xB0, 0x00, 0x00, 0x00,
    0x20, 0x00, 0x0C, 0x00, 0xA0, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x98, 0x00, 0x08, 0x00, 0x8C, 0x00,
    0x80, 0x00, 0x28, 0x00, 0x00, 0x00, 0xB8, 0x00, 0x94, 0x00, 0x8E, 0x00, 0x3C, 0x00, 0x26, 0x00,
    0x84, 0x00, 0xBA, 0x00, 0xCC, 0x00, 0x0C, 0x00, 0x68, 0x00, 0x04, 0x00, 0x00, 0x00, 0x80, 0x00,
    0xCC, 0x00, 0x30, 0x00, 0x68, 0x00, 0x04, 0x00, 0x00, 0x00, 0x84, 0x00, 0xD0, 0x00, 0x92, 0x00,
    0x08, 0x00, 0x1E, 0x00, 0xB8, 0x00, 0x2E, 0x00, 0xD8, 0x00, 0xB0, 0x00, 0x04, 0x00, 0x08, 0x00,
    0x68, 0x00, 0x80, 0x00, 0x2C, 0x01, 0xB0, 0x00, 0x04, 0x00, 0x08, 0x00, 0x7C, 0x00, 0x80, 0x00,
    0xD0, 0x00, 0xB0, 0x00, 0x08, 0x00, 0x08, 0x00, 0x88, 0x00, 0x50, 0x00, 0x30, 0x01, 0xB0, 0x00,
    0x08, 0x00, 0x08, 0x00, 0x90, 0x00, 0x50, 0x00, 0x00, 0x00, 0xB6, 0x00, 0xC0, 0x00, 0x02, 0x00,
    0x00, 0x00, 0xA6, 0x00, 0xC0, 0x00, 0xB6, 0x00, 0x80, 0x00, 0x02, 0x00, 0x00, 0x00, 0xA6, 0x00,
    0x00, 0x00, 0xD8, 0x00, 0x78, 0x00, 0x10, 0x00, 0x00, 0x00, 0x88, 0x00, 0x78, 0x00, 0xD8, 0x00,
    0xC8, 0x00, 0x10, 0x00, 0x00, 0x00, 0xA8, 0x00, 0xD0, 0x00, 0x10, 0x00, 0x30, 0x00, 0x10, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x10, 0x00, 0x30, 0x00, 0x10, 0x00, 0x00, 0x00, 0x20, 0x00,
    0xD0, 0x00, 0x20, 0x00, 0x30, 0x00, 0x10, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x01, 0x20, 0x00,
    0x30, 0x00, 0x10, 0x00, 0x00, 0x00, 0x30, 0x00, 0x20, 0x00, 0x80, 0x00, 0xA0, 0x00, 0x0C, 0x00,
    0x00, 0x00, 0x98, 0x00, 0x40, 0x01, 0xCC, 0x00, 0x10, 0x00, 0x04, 0x00, 0x10, 0x00, 0x78, 0x00,
    0x88, 0x00, 0x00, 0x82, 0x30, 0x01, 0x10, 0x00, 0x04, 0x00, 0x10, 0x00, 0x7C, 0x00, 0x88, 0x00,
    0x00, 0x82, 0xDC, 0x00, 0xB0, 0x00, 0x10, 0x00, 0x08, 0x00, 0x6C, 0x00, 0x80, 0x00, 0x00, 0x05,
    0xD0, 0x00, 0x4E, 0x00, 0x08, 0x00, 0x08, 0x00, 0x60, 0x00, 0x58, 0x00, 0x00, 0x01, 0xD8, 0x00,
    0x4E, 0x00, 0x04, 0x00, 0x08, 0x00, 0x68, 0x00, 0x58, 0x00, 0x00, 0x01, 0xDC, 0x00, 0x4E, 0x00,
    0x10, 0x00, 0x08, 0x00, 0x6C, 0x00, 0x58, 0x00, 0x00, 0x05, 0x2C, 0x01, 0x4E, 0x00, 0x04, 0x00,
    0x08, 0x00, 0x7C, 0x00, 0x58, 0x00, 0x00, 0x01, 0x30, 0x01, 0x4E, 0x00, 0x08, 0x00, 0x08, 0x00,
    0x80, 0x00, 0x50, 0x00, 0x00, 0x01, 0xD0, 0x00, 0x56, 0x00, 0x08, 0x00, 0x0F, 0x00, 0xB8, 0x00,
    0x28, 0x00, 0x00, 0x84, 0xD8, 0x00, 0x56, 0x00, 0x04, 0x00, 0x1E, 0x00, 0x68, 0x00, 0x60, 0x00,
    0x00, 0x83, 0x2C, 0x01, 0x56, 0x00, 0x04, 0x00, 0x1E, 0x00, 0x7C, 0x00, 0x60, 0x00, 0x00, 0x83,
    0x30, 0x01, 0x56, 0x00, 0x08, 0x00, 0x0F, 0x00, 0x60, 0x00, 0x60, 0x00, 0x00, 0x86, 0x00, 0x00,
    0xD0, 0x00, 0x30, 0x00, 0x08, 0x00, 0x08, 0x00, 0x60, 0x00, 0x58, 0x00, 0x00, 0x01, 0xD8, 0x00,
    0x30, 0x00, 0x04, 0x00, 0x08, 0x00, 0x68, 0x00, 0x58, 0x00, 0x00, 0x01, 0xDC, 0x00, 0x30, 0x00,
    0x10, 0x00, 0x08, 0x00, 0x6C, 0x00, 0x58, 0x00, 0x00, 0x05, 0x2C, 0x01, 0x30, 0x00, 0x04, 0x00,
    0x08, 0x00, 0x7C, 0x00, 0x58, 0x00, 0x00, 0x01, 0x30, 0x01, 0x30, 0x00, 0x08, 0x00, 0x08, 0x00,
    0x80, 0x00, 0x50, 0x00, 0x00, 0x01, 0xD0, 0x00, 0x38, 0x00, 0x08, 0x00, 0x0F, 0x00, 0xB8, 0x00,
    0x28, 0x00, 0x00, 0x86, 0xD8, 0x00, 0x38, 0x00, 0x04, 0x00, 0x1E, 0x00, 0x68, 0x00, 0x60, 0x00,
    0x00, 0x84, 0x2C, 0x01, 0x38, 0x00, 0x04, 0x00, 0x1E, 0x00, 0x7C, 0x00, 0x60, 0x00, 0x00, 0x84,
    0x30, 0x01, 0x38, 0x00, 0x08, 0x00, 0x0F, 0x00, 0x60, 0x00, 0x60, 0x00, 0x00, 0x88, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x40, 0x01, 0x0C, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x09, 0x00, 0x80, 0x00,
    0xC7, 0x00, 0x0C, 0x00, 0x79, 0x00, 0x80, 0x00, 0x00, 0x00, 0x8C, 0x00, 0x40, 0x01, 0x64, 0x00,
    0xD0, 0x00, 0x10, 0x00, 0x00, 0x01, 0x10, 0x00, 0xD0, 0x00, 0x20, 0x00, 0x00, 0x01, 0x20, 0x00,
    0xDC, 0x00, 0x38, 0x00, 0x04, 0x01, 0x38, 0x00, 0xDC, 0x00, 0x56, 0x00, 0x04, 0x01, 0x56, 0x00,
    0xDC, 0x00, 0x74, 0x00, 0x04, 0x01, 0x74, 0x00, 0xDC, 0x00, 0x92, 0x00, 0x04, 0x01, 0x92, 0x00,
    0x01, 0x02, 0x03, 0x04, 0x04, 0x00, 0x00, 0x00,
];

/// Offset of the 22 plain frame parts.
pub const FRAME_PARTS_AT: usize = 0;
/// Number of plain frame parts.
pub const FRAME_PARTS_LEN: usize = 22;
/// Offset of the four top-tab parts.
pub const TOP_TABS_AT: usize = 264;
/// Number of top-tab parts.
pub const TOP_TABS_LEN: usize = 4;
/// Offset of the four common tiled records.
pub const COMMON_TILES_AT: usize = 312;
/// Number of common tiled records.
pub const COMMON_TILES_LEN: usize = 4;
/// Offset of the six-slot inventory border records.
pub const BORDER_6_AT: usize = 368;
/// Offset of the eight-slot inventory border records.
pub const BORDER_8_AT: usize = 496;
/// Number of records in either border.
pub const BORDER_LEN: usize = 9;
/// Offset of the black masking rects.
pub const BLACK_RECTS_AT: usize = 624;
/// Number of black masking rects.
pub const BLACK_RECTS_LEN: usize = 4;
/// Offset of the inventory slot table.
pub const SLOT_TABLE_AT: usize = 656;

/// One plain 12-byte menu frame entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePart {
    /// Destination position.
    pub pos: [i16; 2],
    /// Destination and source size.
    pub size: [i16; 2],
    /// Source top-left in `ui/status.tim`.
    pub uv: [u8; 2],
}

/// One 14-byte tiled menu frame record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTile {
    /// The first tile's geometry.
    pub part: FramePart,
    /// Sprite flags (`0x40`/`0x80`); `0x80` marks an opaque-run flag byte.
    pub flags: u8,
    /// Low seven bits: number of tiles. Bit `0x80`: step vertically by the
    /// tile height instead of horizontally by the width.
    pub count: u8,
}

impl FrameTile {
    /// Number of tiles to draw (at least one).
    pub fn tiles(&self) -> u8 {
        self.count & 0x7F
    }

    /// Whether the tiles step down the screen instead of across.
    pub fn vertical(&self) -> bool {
        self.count & 0x80 != 0
    }
}

/// One solid-colour rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Top-left position.
    pub pos: [i16; 2],
    /// Width and height.
    pub size: [i16; 2],
}

/// Read a little-endian `u16` from the block.
fn u16_at(offset: usize) -> u16 {
    u16::from_le_bytes([MENU_BLOCK[offset], MENU_BLOCK[offset + 1]])
}

/// Decode the 12-byte frame entry at `offset`.
fn frame_part_at(offset: usize) -> FramePart {
    FramePart {
        pos: [u16_at(offset) as i16, u16_at(offset + 2) as i16],
        size: [u16_at(offset + 4) as i16, u16_at(offset + 6) as i16],
        uv: [MENU_BLOCK[offset + 8], MENU_BLOCK[offset + 10]],
    }
}

/// Decode the 14-byte tiled record at `offset`.
fn frame_tile_at(offset: usize) -> FrameTile {
    FrameTile {
        part: frame_part_at(offset),
        flags: MENU_BLOCK[offset + 12],
        count: MENU_BLOCK[offset + 13],
    }
}

/// The 22 plain frame parts in the original's draw order: the last file entry
/// first, down to the first.
pub fn frame_parts() -> [FramePart; FRAME_PARTS_LEN] {
    std::array::from_fn(|index| frame_part_at((FRAME_PARTS_LEN - 1 - index) * 12))
}

/// The four top-tab parts in the original's draw order (last first).
pub fn top_tabs() -> [FramePart; TOP_TABS_LEN] {
    std::array::from_fn(|index| frame_part_at(TOP_TABS_AT + (TOP_TABS_LEN - 1 - index) * 12))
}

/// The tab part for cursor/tab `tab` (`0..4`) in cursor order.
pub fn tab_part(tab: usize) -> FramePart {
    frame_part_at(TOP_TABS_AT + tab.min(TOP_TABS_LEN - 1) * 12)
}

/// The four common tiled records in the original's draw order (last first).
pub fn common_tiles() -> [FrameTile; COMMON_TILES_LEN] {
    std::array::from_fn(|index| {
        frame_tile_at(COMMON_TILES_AT + (COMMON_TILES_LEN - 1 - index) * 14)
    })
}

/// The nine inventory border records in the original's draw order (last
/// record first) for a character with `slots` inventory slots.
pub fn border_tiles(slots: usize) -> [FrameTile; BORDER_LEN] {
    let base = if slots >= 8 { BORDER_8_AT } else { BORDER_6_AT };
    std::array::from_fn(|index| frame_tile_at(base + (BORDER_LEN - 1 - index) * 14))
}

/// The four black masking rects in the original's draw order (last first).
pub fn black_rects() -> [Rect; BLACK_RECTS_LEN] {
    std::array::from_fn(|index| {
        let offset = BLACK_RECTS_AT + (BLACK_RECTS_LEN - 1 - index) * 8;
        Rect {
            pos: [u16_at(offset) as i16, u16_at(offset + 2) as i16],
            size: [u16_at(offset + 4) as i16, u16_at(offset + 6) as i16],
        }
    })
}

/// The four top-tab positions, indexed by tab (`cursor / 2`).
pub const TAB_POSITIONS: [[i16; 2]; 4] = [[208, 16], [256, 16], [208, 32], [256, 32]];

/// The eight item-slot positions in cursor order. A six-slot character uses
/// entries `2..8`; an eight-slot character uses all of them.
pub const SLOT_POSITIONS: [[i16; 2]; 8] = [
    [220, 56],
    [260, 56],
    [220, 86],
    [260, 86],
    [220, 116],
    [260, 116],
    [220, 146],
    [260, 146],
];

/// The position of tab `tab` (`0..4`).
pub fn tab_position(tab: usize) -> [i16; 2] {
    TAB_POSITIONS[tab.min(TAB_POSITIONS.len() - 1)]
}

/// The position of item slot `slot` for a character with `slots` inventory
/// slots. Six-slot characters use the bottom three rows; eight-slot ones use
/// all four.
pub fn slot_position(slots: usize, slot: usize) -> Option<[i16; 2]> {
    let offset = SLOT_POSITIONS
        .len()
        .saturating_sub(slots.min(SLOT_POSITIONS.len()));
    SLOT_POSITIONS.get(slot.checked_add(offset)?).copied()
}

/// The position of cursor value `cursor`, whether it names a tab or a slot.
pub fn cursor_position(cursor: u8, slots: usize) -> Option<[i16; 2]> {
    if cursor & 1 != 0 {
        return None;
    }
    if cursor < FIRST_SLOT_CURSOR {
        return Some(tab_position(usize::from(cursor / 2)));
    }
    slot_position(slots, slot_of_cursor(cursor)?)
}

/// The first item-slot cursor value; anything below it is a top tab.
pub const FIRST_SLOT_CURSOR: u8 = 8;

/// Whether `cursor` names a top tab.
pub fn is_tab(cursor: u8) -> bool {
    cursor < FIRST_SLOT_CURSOR
}

/// The inventory slot of a cursor value, or `None` for a tab or odd value.
pub fn slot_of_cursor(cursor: u8) -> Option<usize> {
    if cursor & 1 != 0 || cursor < FIRST_SLOT_CURSOR {
        return None;
    }
    Some(usize::from((cursor - FIRST_SLOT_CURSOR) / 2))
}

/// The cursor value of inventory slot `slot`.
pub fn cursor_of_slot(slot: usize) -> u8 {
    FIRST_SLOT_CURSOR + (slot as u8) * 2
}

/// Left/right on the 2-wide grid: `cursor ^ 2`.
pub fn move_sideways(cursor: u8) -> u8 {
    cursor ^ 2
}

/// Down one row: `cursor + 4`, wrapping past the last slot to the top tab in
/// the same column.
pub fn move_down(cursor: u8, slots: usize) -> u8 {
    let next = cursor.wrapping_add(4);
    if usize::from(next) > slots * 2 + 6 {
        next & 2
    } else {
        next
    }
}

/// Up one row: `cursor - 4`, wrapping from the tab row to the last slot row.
pub fn move_up(cursor: u8, slots: usize) -> u8 {
    if cursor & 0xFC == 0 {
        (cursor & 2) + (slots as u8) * 2 + 4
    } else {
        cursor - 4
    }
}

/// The bottom-row cursor of a character with `slots` slots (`cursor + 4`
/// wrap target).
pub fn bottom_cursor(slots: usize) -> u8 {
    (slots as u8) * 2 + 4
}

/// The last valid item-slot cursor of a character with `slots` slots.
pub fn last_slot_cursor(slots: usize) -> u8 {
    FIRST_SLOT_CURSOR + (slots as u8 - 1) * 2
}

/// The equipped-item icon position.
pub const EQUIPPED_POS: [i16; 2] = [0xA0, 0x92];
/// The character portrait position.
pub const PORTRAIT_POS: [i16; 2] = [0x16, 0x92];
/// The item icon size.
pub const ICON_SIZE: [i16; 2] = [40, 30];
/// The character portrait size.
pub const PORTRAIT_SIZE: [i16; 2] = [30, 30];
/// The health-face icon position.
pub const HEALTH_FACE_POS: [i16; 2] = [100, 0xA8];
/// The health-face icon size.
pub const HEALTH_FACE_SIZE: [i16; 2] = [32, 8];
/// Left edge of the EKG sweep window.
pub const EKG_MIN_X: i32 = 0x54;
/// Right edge of the EKG sweep window.
pub const EKG_MAX_X: i32 = 0x83;
/// EKG baseline y.
pub const EKG_BASE_Y: i32 = 0xA2;
/// The item-name line's y (menu text baseline).
pub const ITEM_NAME_Y: i32 = 0xBA;
/// The item action submenu box position.
pub const SUBMENU_POS: [i16; 2] = [0x90, 0x39];
/// One item action option's size.
pub const SUBMENU_OPTION_SIZE: [i16; 2] = [0x30, 0x18];
/// Number of item action options (use/check/move).
pub const SUBMENU_OPTION_COUNT: usize = 3;
/// Top-tab icon size.
pub const TAB_SIZE: [i16; 2] = [0x30, 0x10];
/// Top-tab cursor icon size.
pub const TAB_CURSOR_SIZE: [i16; 2] = [0x30, 0x10];

/// Fill `rect` with one opaque colour, clipped to the framebuffer.
pub fn fill_rect(framebuffer: &mut Framebuffer, rect: Rect, rgba: [u8; 4]) {
    let [x, y] = rect.pos;
    let [w, h] = rect.size;
    let start_x = i32::from(x).max(0);
    let start_y = i32::from(y).max(0);
    let end_x = (i32::from(x) + i32::from(w)).min(framebuffer.width as i32);
    let end_y = (i32::from(y) + i32::from(h)).min(framebuffer.height as i32);
    for row in start_y..end_y {
        for column in start_x..end_x {
            let offset = (row as usize * framebuffer.width as usize + column as usize) * 4;
            if let Some(pixel) = framebuffer.rgba.get_mut(offset..offset + 4) {
                pixel.copy_from_slice(&rgba);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_has_every_region_back_to_back() {
        assert_eq!(MENU_BLOCK.len(), MENU_BLOCK_BYTES);
        assert_eq!(FRAME_PARTS_AT + FRAME_PARTS_LEN * 12, TOP_TABS_AT);
        assert_eq!(TOP_TABS_AT + TOP_TABS_LEN * 12, COMMON_TILES_AT);
        assert_eq!(COMMON_TILES_AT + COMMON_TILES_LEN * 14, BORDER_6_AT);
        assert_eq!(BORDER_6_AT + BORDER_LEN * 14, 494);
        assert_eq!(BORDER_8_AT + BORDER_LEN * 14, 622);
        assert_eq!(BLACK_RECTS_AT + BLACK_RECTS_LEN * 8, SLOT_TABLE_AT);
        assert_eq!(SLOT_TABLE_AT + 24 * 2 + 8, MENU_BLOCK_BYTES);
    }

    #[test]
    fn frame_parts_decode_in_draw_order() {
        let parts = frame_parts();
        // The original draws the last file entry first: the bottom strip.
        assert_eq!(
            parts[0],
            FramePart {
                pos: [120, 216],
                size: [200, 16],
                uv: [0, 0xA8],
            }
        );
        // The last drawn part is the top-left main-frame corner.
        assert_eq!(
            parts[FRAME_PARTS_LEN - 1],
            FramePart {
                pos: [8, 12],
                size: [8, 8],
                uv: [0x50, 0x78],
            }
        );
        // The health-bar base strip sits in the middle.
        assert_eq!(
            parts[13],
            FramePart {
                pos: [32, 12],
                size: [160, 12],
                uv: [0, 0x98],
            }
        );
    }

    #[test]
    fn top_tabs_decode_to_their_four_positions() {
        // Cursor order: top-left, top-right, bottom-left, bottom-right.
        for (tab, expected, v) in [
            (0, [208, 16], 0x00),
            (1, [256, 16], 0x20),
            (2, [208, 32], 0x10),
            (3, [256, 32], 0x30),
        ] {
            let part = tab_part(tab);
            assert_eq!(part.pos, expected);
            assert_eq!(part.size, TAB_SIZE);
            assert_eq!(part.uv, [0, v]);
        }
        assert_eq!(top_tabs()[0].pos, [256, 32], "draw order is reversed");
    }

    #[test]
    fn border_records_select_the_character_grid() {
        let chris = border_tiles(6);
        let jill = border_tiles(8);
        // Draw order is the file order reversed: the right border first.
        assert_eq!(chris[0].part.pos, [304, 86]);
        assert_eq!(jill[0].part.pos, [304, 56]);
        assert_eq!(chris[8].part.pos, [208, 78]);
        assert_eq!(jill[8].part.pos, [208, 48]);
        // Six-slot right border is 6 tiles, eight-slot is 8.
        assert_eq!(chris[0].tiles(), 6);
        assert!(chris[0].vertical());
        assert_eq!(jill[0].tiles(), 8);
        assert!(jill[0].vertical());
    }

    #[test]
    fn common_tiles_are_tiled_records() {
        let tiles = common_tiles();
        assert_eq!(tiles.len(), COMMON_TILES_LEN);
        // The last file record is the item submenu frame, drawn first.
        assert_eq!(
            tiles[0],
            FrameTile {
                part: FramePart {
                    pos: [220, 176],
                    size: [16, 8],
                    uv: [108, 128],
                },
                flags: 0,
                count: 5,
            }
        );
        assert_eq!(tiles[0].tiles(), 5);
        assert!(!tiles[0].vertical());
        assert_eq!(tiles[1].part.pos, [304, 16]);
        assert_eq!(tiles[1].part.uv, [124, 136]);
        assert_eq!(tiles[1].tiles(), 2);
        assert!(tiles[1].vertical());
        // The wide health-bar base tile is the first file record, drawn last.
        assert_eq!(
            tiles[3],
            FrameTile {
                part: FramePart {
                    pos: [32, 128],
                    size: [160, 12],
                    uv: [0, 152],
                },
                flags: 0x40,
                count: 1,
            }
        );
    }

    #[test]
    fn black_rects_cover_the_screen_edges() {
        let rects = black_rects();
        assert_eq!(
            rects[0],
            Rect {
                pos: [0, 140],
                size: [320, 100],
            }
        );
        assert_eq!(
            rects[3],
            Rect {
                pos: [0, 0],
                size: [320, 12],
            }
        );
        let total: i32 = rects
            .iter()
            .map(|rect| i32::from(rect.size[0]) * i32::from(rect.size[1]))
            .sum();
        assert_eq!(total, 320 * 12 + 9 * 128 + 121 * 128 + 320 * 100);
    }

    #[test]
    fn slot_positions_follow_the_character_grid() {
        assert_eq!(slot_position(8, 0), Some([220, 56]));
        assert_eq!(slot_position(8, 7), Some([260, 146]));
        assert_eq!(slot_position(6, 0), Some([220, 86]));
        assert_eq!(slot_position(6, 5), Some([260, 146]));
        assert_eq!(slot_position(6, 6), None);
        assert_eq!(slot_position(8, 8), None);
        assert_eq!(cursor_position(0, 6), Some([208, 16]));
        assert_eq!(cursor_position(6, 8), Some([256, 32]));
        assert_eq!(cursor_position(8, 6), Some([220, 86]));
        assert_eq!(cursor_position(18, 6), Some([260, 146]));
        assert_eq!(cursor_position(20, 8), Some([220, 146]));
        assert_eq!(cursor_position(3, 8), None);
    }

    #[test]
    fn cursor_math_matches_the_original_walk() {
        assert_eq!(cursor_of_slot(0), 8);
        assert_eq!(cursor_of_slot(7), 22);
        assert_eq!(slot_of_cursor(8), Some(0));
        assert_eq!(slot_of_cursor(18), Some(5));
        assert_eq!(slot_of_cursor(22), Some(7));
        assert_eq!(slot_of_cursor(0), None);
        assert_eq!(slot_of_cursor(9), None);
        assert!(is_tab(6));
        assert!(!is_tab(8));

        for slots in [6usize, 8] {
            for slot in 0..slots {
                let cursor = cursor_of_slot(slot);
                // Sideways toggles the column and toggling twice is identity.
                assert_eq!(move_sideways(move_sideways(cursor)), cursor);
                // A row is two slots; down then up is a no-op while both rows
                // are valid.
                if slot + 2 < slots {
                    assert_eq!(slot_of_cursor(move_down(cursor, slots)), Some(slot + 2));
                    assert_eq!(move_up(move_down(cursor, slots), slots), cursor);
                }
            }
        }
        // Down from the bottom row wraps to the top tab of the same column.
        assert_eq!(move_down(16, 6), 0);
        assert_eq!(move_down(18, 6), 2);
        assert_eq!(move_down(20, 8), 0);
        assert_eq!(move_down(22, 8), 2);
        // Up from the top tab wraps to the bottom slot row.
        assert_eq!(move_up(0, 6), 16);
        assert_eq!(move_up(2, 6), 18);
        assert_eq!(move_up(0, 8), 20);
        assert_eq!(move_up(2, 8), 22);
        // Otherwise a plain four-step walk.
        assert_eq!(move_down(8, 8), 12);
        assert_eq!(move_up(12, 8), 8);
        assert_eq!(move_up(4, 8), 0);
        assert_eq!(move_down(4, 8), 8);
        assert_eq!(last_slot_cursor(6), 18);
        assert_eq!(last_slot_cursor(8), 22);
        assert_eq!(bottom_cursor(6), 16);
        assert_eq!(bottom_cursor(8), 20);
    }

    #[test]
    fn fill_rect_clips_to_the_framebuffer() {
        let mut framebuffer = Framebuffer::new();
        fill_rect(
            &mut framebuffer,
            Rect {
                pos: [-4, -4],
                size: [8, 8],
            },
            [1, 2, 3, 255],
        );
        let pixel = |framebuffer: &Framebuffer, x: usize, y: usize| {
            let offset = (y * framebuffer.width as usize + x) * 4;
            framebuffer.rgba[offset..offset + 4].to_vec()
        };
        assert_eq!(pixel(&framebuffer, 0, 0), [1, 2, 3, 255]);
        assert_eq!(pixel(&framebuffer, 3, 3), [1, 2, 3, 255]);
        assert_eq!(pixel(&framebuffer, 4, 4), [0, 0, 0, 0]);
    }
}
