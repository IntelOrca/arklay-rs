//! Item-box storage screen (pause-menu mode 2).
//!
//! The box is opened by a room's `item_box` action: the room freezes, the
//! status/inventory panel is drawn behind it and this overlay draws the
//! `ui/itemboxn.tim` frame, the selected box slot's 40x30 icon with its
//! quantity, the three-row item name list and the page-position ticks. The
//! player cursor stays on the inventory grid: confirming an inventory stack
//! arms the swap, L1/R1 page the box cursor with a slide, and confirming again
//! performs it. Cancel (or confirming on a top-tab cursor) closes the screen.
//!
//! The frame tables preserve the original's memory layout: entries are stored
//! in reverse draw order and the accessors below return them in the order the
//! original's backwards walk draws them.

use anyhow::{Context, Result};

use crate::font::Tint;
use crate::game::GameState;
use crate::model::Texture8;
use crate::pack::Pack;
use crate::render::Framebuffer;
use crate::text::Text;
use crate::tim;

use super::main_menu::{
    MenuAssets, MenuInput, draw_item_icon, draw_item_quantity, item_name_bytes,
};

/// Pack entry of the item-box frame sheet.
pub const ITEMBOX_ENTRY: &str = "ui/itemboxn.tim";
/// Number of box slots (8bpp sheet and save block agree on 48).
pub const ITEMBOX_SLOTS_TOTAL: usize = 48;
/// Ticks one page-slide lasts.
pub const SLIDE_TICKS: u8 = 16;

/// One 12-byte frame texture entry of the item-box sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePart {
    /// Destination position.
    pub pos: [i16; 2],
    /// Destination and source size.
    pub size: [i16; 2],
    /// Source top-left in `ui/itemboxn.tim`.
    pub uv: [u8; 2],
}

/// One solid-colour rectangle of the item-box overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Top-left position.
    pub pos: [i16; 2],
    /// Width and height.
    pub size: [i16; 2],
}

const fn part(pos: [i16; 2], size: [i16; 2], uv: [u8; 2]) -> FramePart {
    FramePart { pos, size, uv }
}

const fn rect(pos: [i16; 2], size: [i16; 2]) -> Rect {
    Rect { pos, size }
}

/// Frame textures, in the original's draw order: the preview border, the two
/// box arrows, the item-list bottom and top edge lines (both sampling the
/// sheet's green edge rows).
pub const FRAME_PARTS_A: [FramePart; 5] = [
    part([0x5B, 0x57], [0x2A, 0x20], [0x10, 0x30]),
    part([0xAF, 0x53], [8, 7], [0x48, 0x40]),
    part([0xAF, 0x18], [8, 7], [0x48, 0x38]),
    part([41, 80], [128, 1], [0, 0x2F]),
    part([41, 33], [128, 1], [0, 0]),
];

/// Mask rectangles drawn over the background before the frame, in draw order.
pub const FRAME_MASKS: [Rect; 5] = [
    rect([0x5C, 0x77], [40, 9]),
    rect([0x85, 0x51], [0x23, 0x0F]),
    rect([0x5B, 0x51], [0x2A, 6]),
    rect([0x2A, 0x51], [0x31, 0x0F]),
    rect([0x2A, 0x18], [0x7E, 9]),
];

/// The trailing frame textures, in draw order: the list box interior (whose
/// blue fill and green side columns close the rectangle), the bottom
/// half-arrow and the top arrow.
pub const FRAME_PARTS_C: [FramePart; 3] = [
    part([0xAF, 0x40], [8, 0x12], [8, 0x30]),
    part([0xAF, 0x20], [8, 0x20], [0, 0x30]),
    part([41, 34], [128, 0x2E], [0, 1]),
];

/// Half-strength shadow over the list's first and third name rows.
pub const SHADOW_TOP: Rect = rect([42, 34], [126, 15]);
/// Half-strength shadow over the list's fourth name row.
pub const SHADOW_BOTTOM: Rect = rect([42, 0x40], [126, 0x10]);
/// The browse-mode shadow over the whole list box.
pub const SHADOW_LIST: Rect = rect([42, 34], [126, 0x2E]);
/// The browse-mode shadow over the preview slot.
pub const SHADOW_SLOT: Rect = rect([0x5C, 0x58], [40, 0x1E]);
/// The dark scrollbar track above the list.
pub const TRACK_TOP: Rect = rect([0xD2, 0x10], [0x5E, 0x10]);
/// The dark scrollbar track beside the list.
pub const TRACK_SIDE: Rect = rect([0xD2, 0x20], [0x2F, 0x10]);
/// The item-name list's left edge.
pub const NAME_X: i16 = 0x2A;
/// The first item-name list row's y.
pub const NAME_Y: i16 = 0x23;
/// Vertical step between name rows.
pub const NAME_STEP: i16 = 0x0F;
/// The box preview slot position.
pub const SLOT_POS: [i16; 2] = [0x5C, 0x58];
/// The preview icon size.
pub const SLOT_SIZE: [i16; 2] = [40, 30];
/// Page-position ticks left edge.
pub const TICK_X: i16 = 0xB0;
/// Page-position ticks first y offset.
pub const TICK_Y: i16 = 0x21;

/// The item-box screen's drawable art.
pub struct ItemBoxAssets {
    /// `ui/itemboxn.tim` frame sheet.
    pub itemboxn: Texture8,
}

impl ItemBoxAssets {
    /// Load the item-box sheet from `pack`.
    pub fn load(pack: &Pack) -> Result<Self> {
        let bytes = pack
            .read(ITEMBOX_ENTRY)
            .with_context(|| format!("missing {ITEMBOX_ENTRY}"))?;
        let itemboxn =
            tim::decode_8bpp(bytes).with_context(|| format!("invalid {ITEMBOX_ENTRY}"))?;
        Ok(Self { itemboxn })
    }
}

/// Which part of the interaction owns the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemBoxMode {
    /// The inventory cursor is being moved.
    Browse,
    /// An inventory stack is armed; confirms swap, L1/R1 page the box.
    Armed,
}

/// What a handled item-box input asks the engine to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemBoxEvent {
    /// The input was consumed.
    None,
    /// The game state changed (a swap happened).
    Changed,
    /// Close the box and the pause menu.
    Close,
}

/// The item-box screen state.
#[derive(Debug, Clone)]
pub struct ItemBox {
    /// Which cursor owns the input.
    pub mode: ItemBoxMode,
    /// The box cursor (`0..48`), matching the original's slot byte.
    pub box_cursor: u8,
    /// The inventory cursor value (`8 + 2 * slot`).
    pub player_cursor: u8,
    /// Inventory slots of the current character.
    pub slots: usize,
    /// Page-slide direction: `0` idle, `+1` next, `-1` previous.
    pub slide_dir: i8,
    /// Page-slide progress in `0..=SLIDE_TICKS`.
    pub slide_step: u8,
    /// Blink counter driving the cursor and page ticks.
    pub blink: u8,
}

impl Default for ItemBox {
    fn default() -> Self {
        Self {
            mode: ItemBoxMode::Browse,
            box_cursor: 0,
            player_cursor: 8,
            slots: 6,
            slide_dir: 0,
            slide_step: 0,
            blink: 0,
        }
    }
}

impl ItemBox {
    /// Open the box for the current character: the original resets the box
    /// cursor and starts on the first inventory slot.
    pub fn open(&mut self, game: &GameState) {
        self.mode = ItemBoxMode::Browse;
        self.box_cursor = 0;
        self.player_cursor = 8;
        self.slots = game.inventory_capacity();
        self.slide_dir = 0;
        self.slide_step = 0;
        self.blink = 0;
    }

    /// The inventory slot under the player cursor.
    pub fn player_slot(&self) -> Option<usize> {
        let slot = super::layout::slot_of_cursor(self.player_cursor)?;
        (slot < self.slots).then_some(slot)
    }

    /// The item id under the player cursor, or `0`.
    pub fn selected_item(&self, game: &GameState) -> u8 {
        self.player_slot()
            .and_then(|slot| game.inventory.get(slot))
            .map(|stack| stack.id)
            .unwrap_or(0)
    }

    /// Advance one fixed tick: the slide and blink animations.
    pub fn tick(&mut self) {
        self.blink = self.blink.wrapping_add(1);
        if self.slide_dir != 0 {
            self.slide_step = self.slide_step.saturating_add(1);
            if self.slide_step >= SLIDE_TICKS {
                self.slide_step = 0;
                self.slide_dir = 0;
            }
        }
    }

    /// Whether the inventory cursor is on a top tab (the original closes the
    /// box when confirm lands there).
    pub fn cursor_on_tab(&self) -> bool {
        self.player_cursor < super::layout::FIRST_SLOT_CURSOR
    }

    /// Handle one discrete menu input, mutating `game` on a confirmed swap.
    pub fn handle_input(&mut self, game: &mut GameState, input: MenuInput) -> ItemBoxEvent {
        match self.mode {
            ItemBoxMode::Browse => self.browse_input(game, input),
            ItemBoxMode::Armed => self.armed_input(game, input),
        }
    }

    fn browse_input(&mut self, _game: &mut GameState, input: MenuInput) -> ItemBoxEvent {
        match input {
            MenuInput::Cancel => ItemBoxEvent::Close,
            MenuInput::Confirm => {
                if self.cursor_on_tab() {
                    return ItemBoxEvent::Close;
                }
                self.mode = ItemBoxMode::Armed;
                ItemBoxEvent::None
            }
            MenuInput::Left | MenuInput::Right => {
                self.player_cursor = super::layout::move_sideways(self.player_cursor);
                ItemBoxEvent::None
            }
            MenuInput::Up => {
                self.player_cursor = super::layout::move_up(self.player_cursor, self.slots);
                ItemBoxEvent::None
            }
            MenuInput::Down => {
                self.player_cursor = super::layout::move_down(self.player_cursor, self.slots);
                ItemBoxEvent::None
            }
            MenuInput::PageLeft | MenuInput::PageRight => ItemBoxEvent::None,
        }
    }

    fn armed_input(&mut self, game: &mut GameState, input: MenuInput) -> ItemBoxEvent {
        match input {
            MenuInput::Cancel => {
                self.mode = ItemBoxMode::Browse;
                ItemBoxEvent::None
            }
            MenuInput::PageLeft => {
                self.page(-1);
                ItemBoxEvent::None
            }
            MenuInput::PageRight => {
                self.page(1);
                ItemBoxEvent::None
            }
            MenuInput::Confirm => {
                let box_occupied = game.item_box[usize::from(self.box_cursor)].id != 0;
                if !box_occupied && self.selected_item(game) == 0 {
                    return ItemBoxEvent::None;
                }
                let Some(player_slot) = self.player_slot() else {
                    return ItemBoxEvent::None;
                };
                let changed = game.item_box_swap(usize::from(self.box_cursor), player_slot);
                self.mode = ItemBoxMode::Browse;
                if changed {
                    ItemBoxEvent::Changed
                } else {
                    ItemBoxEvent::None
                }
            }
            MenuInput::Up | MenuInput::Down | MenuInput::Left | MenuInput::Right => {
                ItemBoxEvent::None
            }
        }
    }

    /// Step the box cursor by one slot with the slide animation, wrapping at
    /// the 48-slot ends exactly like the original's L1/R1 walk.
    pub fn page(&mut self, direction: i8) {
        if direction > 0 {
            self.box_cursor = if self.box_cursor >= ITEMBOX_SLOTS_TOTAL as u8 - 1 {
                0
            } else {
                self.box_cursor + 1
            };
        } else if direction < 0 {
            self.box_cursor = if self.box_cursor == 0 {
                ITEMBOX_SLOTS_TOTAL as u8 - 1
            } else {
                self.box_cursor - 1
            };
        } else {
            return;
        }
        self.slide_dir = direction.signum();
        self.slide_step = 0;
    }

    /// Draw the box overlay over the already-drawn inventory panel.
    pub fn draw(
        &self,
        framebuffer: &mut Framebuffer,
        assets: &ItemBoxAssets,
        menu: &MenuAssets,
        text: &Text,
        game: &GameState,
    ) {
        // Mask the box area out of the dark background, then lay the shadows.
        for rect in FRAME_MASKS {
            fill_rect(framebuffer, rect, [0, 0, 0, 255]);
        }
        framebuffer.blend_black_rect(rect_dst(SHADOW_TOP), 128);
        framebuffer.blend_black_rect(rect_dst(SHADOW_BOTTOM), 128);
        if self.mode == ItemBoxMode::Browse {
            framebuffer.blend_black_rect(rect_dst(SHADOW_LIST), 128);
            framebuffer.blend_black_rect(rect_dst(SHADOW_SLOT), 128);
        }
        fill_rect(framebuffer, TRACK_TOP, [0, 0, 0, 255]);
        fill_rect(framebuffer, TRACK_SIDE, [0, 0, 0, 255]);

        for part in FRAME_PARTS_A {
            draw_part(framebuffer, &assets.itemboxn, part);
        }
        for part in FRAME_PARTS_C {
            draw_part(framebuffer, &assets.itemboxn, part);
        }

        self.draw_slot(framebuffer, menu, game);
        self.draw_names(framebuffer, menu, text, game);
        self.draw_ticks(framebuffer, assets);
    }

    /// The preview slot: the box cursor's 40x30 icon with its quantity, plus
    /// the incoming slot during a page slide.
    fn draw_slot(&self, framebuffer: &mut Framebuffer, menu: &MenuAssets, game: &GameState) {
        let slide = i32::from(self.slide_step).min(i32::from(SLIDE_TICKS));
        let reveal = if self.slide_dir == 0 {
            30
        } else {
            30 - (i32::from(SLIDE_TICKS) - slide) * 2
        };
        let second = self.slide_dir != 0;
        for index in 0..if second { 2 } else { 1 } {
            let cursor = if index == 0 {
                self.box_cursor
            } else if self.slide_dir > 0 {
                if self.box_cursor >= ITEMBOX_SLOTS_TOTAL as u8 - 1 {
                    0
                } else {
                    self.box_cursor + 1
                }
            } else if self.box_cursor == 0 {
                ITEMBOX_SLOTS_TOTAL as u8 - 1
            } else {
                self.box_cursor - 1
            };
            let stack = game.item_box[usize::from(cursor)];
            let y = i32::from(SLOT_POS[1]) + index * 30;
            let height = if index == 0 { reveal.max(0) } else { 30 };
            if height == 0 {
                continue;
            }
            let dst = [i32::from(SLOT_POS[0]), y, i32::from(SLOT_SIZE[0]), height];
            if stack.id == 0 {
                framebuffer.draw_indexed_sprite(
                    &menu.blue,
                    [0, 0, SLOT_SIZE[0].into(), SLOT_SIZE[1].into()],
                    dst,
                    0,
                    0,
                    Tint::White,
                );
            } else {
                draw_item_icon(
                    framebuffer,
                    menu,
                    stack.id,
                    [SLOT_POS[0], SLOT_POS[1] + index as i16 * 30],
                );
                if index == 0 {
                    draw_item_quantity(framebuffer, menu, game, stack, SLOT_POS);
                }
            }
        }
    }

    /// The three-row item name list ending at the box cursor.
    fn draw_names(
        &self,
        framebuffer: &mut Framebuffer,
        menu: &MenuAssets,
        text: &Text,
        game: &GameState,
    ) {
        let mut slot = if self.box_cursor != 0 {
            self.box_cursor - 1
        } else {
            ITEMBOX_SLOTS_TOTAL as u8 - 1
        };
        let shift = if self.slide_dir == 0 {
            0
        } else {
            i32::from(self.slide_dir) * (i32::from(SLIDE_TICKS) - i32::from(self.slide_step))
        };
        for row in 0..3 {
            let stack = game.item_box[usize::from(slot)];
            if stack.id != 0
                && let Some(name) = item_name_bytes(text, stack.id)
            {
                menu.font.draw_text(
                    framebuffer,
                    i32::from(NAME_X),
                    i32::from(NAME_Y) + row * i32::from(NAME_STEP) + shift,
                    Tint::White,
                    0,
                    name,
                );
            }
            slot = if slot >= ITEMBOX_SLOTS_TOTAL as u8 - 1 {
                0
            } else {
                slot + 1
            };
        }
    }

    /// The three page-position ticks following the box cursor.
    fn draw_ticks(&self, framebuffer: &mut Framebuffer, assets: &ItemBoxAssets) {
        let mut tick = if self.box_cursor != 0 {
            self.box_cursor - 1
        } else {
            ITEMBOX_SLOTS_TOTAL as u8 - 1
        };
        for _ in 0..3 {
            if tick >= ITEMBOX_SLOTS_TOTAL as u8 {
                tick = 0;
            }
            for x in 0..6 {
                let y = i32::from(TICK_Y) + i32::from(tick);
                framebuffer.draw_indexed_sprite(
                    &assets.itemboxn,
                    [0x40 + x, 0x38, 1, 1],
                    [i32::from(TICK_X) + x, y, 1, 1],
                    0,
                    0,
                    Tint::White,
                );
            }
            tick += 1;
        }
    }
}

fn rect_dst(rect: Rect) -> [i32; 4] {
    [
        i32::from(rect.pos[0]),
        i32::from(rect.pos[1]),
        i32::from(rect.size[0]),
        i32::from(rect.size[1]),
    ]
}

fn draw_part(framebuffer: &mut Framebuffer, texture: &Texture8, part: FramePart) {
    let [x, y] = part.pos;
    let [w, h] = part.size;
    framebuffer.draw_indexed_sprite(
        texture,
        [
            i32::from(part.uv[0]),
            i32::from(part.uv[1]),
            i32::from(w),
            i32::from(h),
        ],
        [i32::from(x), i32::from(y), i32::from(w), i32::from(h)],
        0,
        0,
        Tint::White,
    );
}

fn fill_rect(framebuffer: &mut Framebuffer, rect: Rect, rgba: [u8; 4]) {
    super::layout::fill_rect(
        framebuffer,
        super::layout::Rect {
            pos: rect.pos,
            size: rect.size,
        },
        rgba,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{InventoryItem, STATE_BYTE_EQUIPPED};
    use crate::state::{RoomId, RoomState};

    fn game() -> GameState {
        let mut state = GameState::new(RoomId::parse("1001").unwrap(), &RoomState::default());
        state.max_health = 96;
        state
    }

    #[test]
    fn the_frame_tables_are_in_draw_order() {
        // The first submitted piece is the preview border; the list interior is
        // drawn last but is the largest piece.
        assert_eq!(
            FRAME_PARTS_A[0],
            FramePart {
                pos: [0x5B, 0x57],
                size: [0x2A, 0x20],
                uv: [0x10, 0x30],
            }
        );
        assert_eq!(FRAME_PARTS_A[4].pos, [41, 33]);
        assert_eq!(FRAME_PARTS_A[4].size, [128, 1]);
        assert_eq!(FRAME_PARTS_C[0].pos, [0xAF, 0x40]);
        assert_eq!(FRAME_PARTS_C[2].pos, [41, 34]);
        assert_eq!(FRAME_PARTS_C[2].size, [128, 0x2E]);
        assert_eq!(FRAME_MASKS[0].pos, [0x5C, 0x77]);
        assert_eq!(FRAME_MASKS[4].size, [0x7E, 9]);
        // All table UVs stay inside the decoded 128x208 sheet.
        for part in FRAME_PARTS_A.into_iter().chain(FRAME_PARTS_C) {
            assert!(i32::from(part.uv[0]) + i32::from(part.size[0]) <= 128);
            assert!(i32::from(part.uv[1]) + i32::from(part.size[1]) <= 208);
        }
    }

    #[test]
    fn paging_walks_one_slot_and_wraps_at_48() {
        let mut item_box = ItemBox::default();
        assert_eq!(item_box.box_cursor, 0);
        item_box.page(-1);
        assert_eq!(item_box.box_cursor, 47);
        assert_eq!(item_box.slide_dir, -1);
        item_box.page(1);
        assert_eq!(item_box.box_cursor, 0);
        assert_eq!(item_box.slide_dir, 1);
        for _ in 0..SLIDE_TICKS {
            item_box.tick();
        }
        assert_eq!(item_box.slide_dir, 0);
        assert_eq!(item_box.slide_step, 0);
        for _ in 0..47 {
            item_box.page(1);
        }
        assert_eq!(item_box.box_cursor, 47);
        item_box.page(1);
        assert_eq!(item_box.box_cursor, 0, "the last slot wraps to the first");
    }

    #[test]
    fn browse_confirm_arms_and_confirm_again_swaps() {
        let mut state = game();
        state.add_item(0x41, 1); // spray in slot 0
        state.item_box[2] = InventoryItem {
            id: 0x0B,
            quantity: 12,
        };
        let mut item_box = ItemBox::default();
        item_box.open(&state);

        assert_eq!(
            item_box.handle_input(&mut state, MenuInput::Confirm),
            ItemBoxEvent::None
        );
        assert_eq!(item_box.mode, ItemBoxMode::Armed);
        // Cancel disarms without moving anything.
        assert_eq!(
            item_box.handle_input(&mut state, MenuInput::Cancel),
            ItemBoxEvent::None
        );
        assert_eq!(item_box.mode, ItemBoxMode::Browse);
        assert_eq!(state.inventory[0].id, 0x41);

        // Arm, page to the ammo slot and confirm: deposit the spray and take
        // the ammo.
        item_box.handle_input(&mut state, MenuInput::Confirm);
        item_box.page(1);
        item_box.page(1);
        assert_eq!(item_box.box_cursor, 2);
        assert_eq!(
            item_box.handle_input(&mut state, MenuInput::Confirm),
            ItemBoxEvent::Changed
        );
        assert_eq!(item_box.mode, ItemBoxMode::Browse);
        assert_eq!(state.item_box[2].id, 0x41);
        assert_eq!(state.inventory[0].id, 0x0B);
        assert_eq!(state.inventory[0].quantity, 12);
    }

    #[test]
    fn a_swap_clears_the_equipped_marker_when_the_item_leaves() {
        let mut state = game();
        state.add_item(0x02, 15); // Beretta
        state.add_item(0x41, 1);
        state.set_equipped(Some(0x02));
        let mut item_box = ItemBox::default();
        item_box.open(&state);

        item_box.handle_input(&mut state, MenuInput::Confirm);
        assert_eq!(
            item_box.handle_input(&mut state, MenuInput::Confirm),
            ItemBoxEvent::Changed
        );
        assert!(state.item_box[0].id == 0x02);
        assert_eq!(state.inventory[0].id, 0x41);
        assert_eq!(state.equipped, None);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_EQUIPPED)], 0);
    }

    #[test]
    fn cancel_and_tab_confirm_close_the_box() {
        let mut state = game();
        state.add_item(0x41, 1);
        let mut item_box = ItemBox::default();
        item_box.open(&state);
        assert_eq!(
            item_box.handle_input(&mut state, MenuInput::Cancel),
            ItemBoxEvent::Close
        );

        // A cursor that reached the tab row closes on confirm, as the
        // original's `(cursor & 0xf8) == 0` check does.
        item_box.player_cursor = 6;
        item_box.mode = ItemBoxMode::Browse;
        assert_eq!(
            item_box.handle_input(&mut state, MenuInput::Confirm),
            ItemBoxEvent::Close
        );
    }

    #[test]
    fn an_empty_armed_slot_does_not_change_the_state() {
        let mut state = game();
        let mut item_box = ItemBox::default();
        item_box.open(&state);
        item_box.handle_input(&mut state, MenuInput::Confirm);
        assert_eq!(
            item_box.handle_input(&mut state, MenuInput::Confirm),
            ItemBoxEvent::None
        );
        assert!(state.item_box.iter().all(|slot| slot.id == 0));
    }
}
