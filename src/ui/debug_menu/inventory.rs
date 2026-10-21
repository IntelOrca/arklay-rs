//! INVENTORY: the slot editor and its icon picker.
//!
//! One row per capacity slot shows the slot number, the item icon, its name
//! and the quantity. Left/right step the quantity inside the item's limit
//! (down to zero, which keeps the item) and START clears the slot. Confirm
//! opens the picker: a grid of every item icon, eight to a row, with an EMPTY
//! cell first. Every edit goes through [`GameState`] and its slot bookkeeping;
//! the equipped and selected items are dropped when an edit leaves them
//! dangling.

use super::{BODY_Y, Pen, Step, TEXT_LEFT, TITLE_Y};
use crate::game::{GameState, InventoryItem};
use crate::items;
use crate::render::Tint;
use crate::ui::UiInput;

/// Slot rows the panel shows before it scrolls.
pub const VISIBLE_SLOTS: usize = 4;
/// One slot row's height: an icon plus a little air.
const SLOT_HEIGHT: i32 = 34;
/// The picker's cells per row (40 px icons across the 320 px screen).
pub const PICKER_COLUMNS: usize = 8;
/// The picker's icon rows on screen.
pub const PICKER_VISIBLE_ROWS: usize = 5;
/// The last pickable id: the PC-only Minimi.
pub const PICKER_LAST_ITEM: u8 = items::ITEM_MINIMI;
/// The picker's cell count: EMPTY plus `0x01..=0x70`.
pub const PICKER_CELLS: usize = PICKER_LAST_ITEM as usize + 1;
/// One picker row's height.
const PICKER_ROW_HEIGHT: i32 = 32;
/// The picker grid's top edge.
const PICKER_Y: i32 = 28;

/// The highest quantity `item` can hold: the stack cap for the ammunition
/// range and the ribbon, the table maximum otherwise (0: no quantity).
pub fn quantity_limit(item: u8) -> u8 {
    if items::is_stackable(item) {
        items::ITEM_QUANTITY_CAP
    } else {
        items::max_quantity(item)
    }
}

/// The quantity a freshly picked item starts with: its table maximum, and at
/// least one.
pub fn default_quantity(item: u8) -> u8 {
    items::max_quantity(item).max(1)
}

/// Put `item` into `slot` (`0` clears it) and return the slot it landed in.
/// A slot past the held stacks appends, since the inventory is a compact
/// list, so the item lands in the first empty slot.
pub fn assign(game: &mut GameState, slot: usize, item: u8) -> usize {
    if slot >= game.inventory_capacity() {
        return slot;
    }
    let mut landed = slot;
    if item == 0 {
        if slot < game.inventory.len() {
            game.inventory.remove(slot);
        }
    } else {
        let stack = InventoryItem {
            id: item,
            quantity: default_quantity(item),
        };
        match game.inventory.get_mut(slot) {
            Some(existing) => *existing = stack,
            None => {
                landed = game.inventory.len();
                game.inventory.push(stack);
            }
        }
    }
    game.clear_dangling_items();
    game.rebuild_slots();
    landed
}

/// Step `slot`'s quantity by `delta` inside `0..=quantity_limit`; zero keeps
/// the item in its slot. An item without a quantity is left alone.
pub fn adjust(game: &mut GameState, slot: usize, delta: i32) {
    let Some(stack) = game.inventory.get(slot).copied() else {
        return;
    };
    let limit = i32::from(quantity_limit(stack.id));
    if limit == 0 {
        return;
    }
    let quantity = i32::from(stack.quantity).min(limit) + delta;
    game.inventory[slot].quantity = quantity.clamp(0, limit) as u8;
    game.rebuild_slots();
}

/// The icon picker's cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picker {
    cell: usize,
    scroll_row: usize,
}

impl Picker {
    /// Open on `item`'s cell.
    fn new(item: u8) -> Self {
        let mut picker = Self {
            cell: usize::from(item.min(PICKER_LAST_ITEM)),
            scroll_row: 0,
        };
        picker.scroll_into_view();
        picker
    }

    /// The highlighted item id (0 = EMPTY).
    pub fn item(&self) -> u8 {
        self.cell as u8
    }

    /// The first grid row drawn.
    pub fn scroll_row(&self) -> usize {
        self.scroll_row
    }

    fn scroll_into_view(&mut self) {
        let row = self.cell / PICKER_COLUMNS;
        if row < self.scroll_row {
            self.scroll_row = row;
        } else if row >= self.scroll_row + PICKER_VISIBLE_ROWS {
            self.scroll_row = row + 1 - PICKER_VISIBLE_ROWS;
        }
    }

    /// Move the cell: left/right by one, up/down by a row, all wrapping.
    fn step(&mut self, input: UiInput) {
        let cells = PICKER_CELLS as isize;
        let delta = if input.left {
            -1
        } else if input.right {
            1
        } else if input.up {
            -(PICKER_COLUMNS as isize)
        } else if input.down {
            PICKER_COLUMNS as isize
        } else {
            0
        };
        self.cell = (self.cell as isize + delta).rem_euclid(cells) as usize;
        self.scroll_into_view();
    }
}

/// The INVENTORY tool's state.
#[derive(Debug, Clone, Default)]
pub struct InventoryEditor {
    slot: usize,
    scroll: usize,
    picker: Option<Picker>,
}

impl InventoryEditor {
    /// The slot under the cursor.
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The open picker, if any.
    pub fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }

    /// Clamp the remembered slot to the live capacity after the menu was
    /// closed: a character change can shrink the inventory while it is
    /// hidden, leaving the cursor past the last row.
    pub(super) fn refresh(&mut self, game: &GameState) {
        let capacity = game.inventory_capacity();
        self.slot = self.slot.min(capacity.saturating_sub(1));
        self.scroll = self.scroll.min(capacity.saturating_sub(VISIBLE_SLOTS));
        if self.slot < self.scroll {
            self.scroll = self.slot;
        } else if self.slot >= self.scroll + VISIBLE_SLOTS {
            self.scroll = self.slot + 1 - VISIBLE_SLOTS;
        }
    }

    /// One tick of input against the live inventory.
    pub(super) fn handle_input(&mut self, input: UiInput, game: &mut GameState) -> Step {
        let capacity = game.inventory_capacity();
        self.slot = self.slot.min(capacity.saturating_sub(1));
        if let Some(picker) = self.picker.as_mut() {
            if input.cancel {
                self.picker = None;
            } else if input.confirm {
                let item = picker.item();
                self.picker = None;
                // The cursor follows the item to the slot it landed in.
                self.slot = assign(game, self.slot, item);
            } else {
                picker.step(input);
            }
            return Step::Stay;
        }
        if input.cancel {
            return Step::Back;
        }
        if input.up {
            self.slot = (self.slot + capacity - 1) % capacity;
        } else if input.down {
            self.slot = (self.slot + 1) % capacity;
        } else if input.left {
            adjust(game, self.slot, -1);
        } else if input.right {
            adjust(game, self.slot, 1);
        } else if input.start {
            self.slot = assign(game, self.slot, 0);
        } else if input.confirm {
            let item = game.inventory.get(self.slot).map_or(0, |stack| stack.id);
            self.picker = Some(Picker::new(item));
        }
        if self.slot < self.scroll {
            self.scroll = self.slot;
        } else if self.slot >= self.scroll + VISIBLE_SLOTS {
            self.scroll = self.slot + 1 - VISIBLE_SLOTS;
        }
        Step::Stay
    }

    /// Draw the slot rows, or the full-screen picker over them.
    pub(super) fn draw(&self, pen: &mut Pen, game: &GameState) {
        if let Some(picker) = self.picker.as_ref() {
            draw_picker(pen, picker);
            return;
        }
        let capacity = game.inventory_capacity();
        for slot in (self.scroll..capacity).take(VISIBLE_SLOTS) {
            let y = BODY_Y + (slot - self.scroll) as i32 * SLOT_HEIGHT;
            let mut name_tint = Tint::White;
            if slot == self.slot {
                pen.cursor(TEXT_LEFT - 16, y + 8);
                name_tint = super::SELECTED_TINT;
            }
            pen.ascii(TEXT_LEFT, y + 8, Tint::Grey, &format!("{}", slot + 1));
            let name_x = TEXT_LEFT + 62;
            match game.inventory.get(slot) {
                Some(stack) => {
                    pen.icon(stack.id, [TEXT_LEFT + 14, y]);
                    pen.item_name(name_x, y + 1, name_tint, stack.id);
                    let limit = quantity_limit(stack.id);
                    let quantity = if limit == 0 {
                        format!("{:02X}", stack.id)
                    } else {
                        format!("{:02X}  {}/{}", stack.id, stack.quantity, limit)
                    };
                    pen.ascii(name_x, y + 15, Tint::Grey, &quantity);
                }
                None => {
                    let empty = pen.text.ui("UI_DEBUG_EMPTY");
                    pen.bytes(name_x, y + 8, Tint::Grey, empty);
                }
            }
        }
    }
}

/// The picker: a full-screen grid of icons with the highlighted id and name
/// in a footer.
fn draw_picker(pen: &mut Pen, picker: &Picker) {
    pen.framebuffer.blend_black_rect([0, 0, 320, 240], 200);
    let title = pen.text.ui("UI_DEBUG_PICKER_TITLE");
    pen.bytes(12, TITLE_Y - 16, Tint::White, title);
    let first = picker.scroll_row * PICKER_COLUMNS;
    let last = (first + PICKER_VISIBLE_ROWS * PICKER_COLUMNS).min(PICKER_CELLS);
    for cell in first..last {
        let column = (cell % PICKER_COLUMNS) as i32;
        let row = (cell / PICKER_COLUMNS - picker.scroll_row) as i32;
        let x = column * 40;
        let y = PICKER_Y + row * PICKER_ROW_HEIGHT;
        if cell == 0 {
            let empty = pen.text.ui("UI_DEBUG_EMPTY");
            pen.fit(x + 2, y + 8, Tint::Grey, empty, 36);
        } else {
            pen.icon(cell as u8, [x, y]);
        }
        if cell == picker.cell {
            pen.outline([x, y, 40, 30]);
        }
    }
    let footer_y = PICKER_Y + PICKER_VISIBLE_ROWS as i32 * PICKER_ROW_HEIGHT + 4;
    let item = picker.item();
    if item == 0 {
        let empty = pen.text.ui("UI_DEBUG_EMPTY");
        pen.bytes(12, footer_y, Tint::Yellow, empty);
    } else {
        pen.ascii(12, footer_y, Tint::Yellow, &format!("{item:02X}"));
        pen.item_name(
            12 + 4 * pen.font.metrics.glyph_w,
            footer_y,
            Tint::White,
            item,
        );
    }
    let hint = pen.text.ui("UI_DEBUG_HINT_PICKER");
    pen.bytes(12, footer_y + 18, Tint::Grey, hint);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RoomId;

    fn game(player_flag: u8) -> GameState {
        let mut game = GameState::default();
        game.id = RoomId {
            stage: 1,
            room: 0,
            player_flag,
        };
        game
    }

    fn press(f: impl FnOnce(&mut UiInput)) -> UiInput {
        let mut input = UiInput::default();
        f(&mut input);
        input
    }

    #[test]
    fn quantity_steps_clamp_and_keep_zero() {
        let mut game = game(0);
        game.add_item(0x0B, 249);
        game.add_item(0x02, 15);
        // A stackable item caps at the stack limit, not its table maximum.
        adjust(&mut game, 0, 1);
        assert_eq!(game.inventory[0].quantity, 250);
        adjust(&mut game, 0, 1);
        assert_eq!(game.inventory[0].quantity, 250, "capped");
        // A weapon caps at its own maximum.
        let limit = items::max_quantity(0x02);
        assert!(limit > 0);
        adjust(&mut game, 1, 1);
        assert_eq!(game.inventory[1].quantity, limit);
        // Stepping down to zero keeps the stack in its slot.
        game.inventory[0].quantity = 1;
        adjust(&mut game, 0, -1);
        assert_eq!(
            game.inventory[0],
            InventoryItem {
                id: 0x0B,
                quantity: 0
            }
        );
        adjust(&mut game, 0, -1);
        assert_eq!(game.inventory[0].quantity, 0, "floor is zero");
        assert_eq!(game.inventory.len(), 2);
        assert_eq!(
            game.state_bytes[usize::from(crate::game::STATE_BYTE_TOTAL_HELD)],
            2
        );
    }

    #[test]
    fn start_clears_the_slot_and_drops_dangling_items() {
        let mut game = game(0);
        game.add_item(0x0B, 1);
        game.set_equipped(Some(0x0B));
        let mut editor = InventoryEditor::default();
        // Zero keeps the item (and the equipped copy) in place.
        editor.handle_input(press(|i| i.left = true), &mut game);
        assert_eq!(
            game.inventory[0],
            InventoryItem {
                id: 0x0B,
                quantity: 0
            }
        );
        assert_eq!(game.equipped, Some(0x0B));
        // START removes the item instead.
        editor.handle_input(press(|i| i.start = true), &mut game);
        assert!(game.inventory.is_empty());
        assert_eq!(game.equipped, None);
        assert_eq!(
            game.state_bytes[usize::from(crate::game::STATE_BYTE_TOTAL_HELD)],
            0
        );
    }

    #[test]
    fn items_without_a_quantity_ignore_the_steps() {
        let mut game = game(0);
        // The knife has no quantity.
        game.add_item(0x01, 0);
        assert_eq!(quantity_limit(0x01), 0);
        adjust(&mut game, 0, -1);
        adjust(&mut game, 0, 1);
        assert_eq!(
            game.inventory,
            vec![InventoryItem {
                id: 0x01,
                quantity: 0
            }]
        );
    }

    #[test]
    fn assign_replaces_appends_and_clears_dangling_state() {
        let mut game = game(0);
        game.add_item(0x02, 15);
        game.set_equipped(Some(0x02));
        game.select_item(Some(0x02));
        assign(&mut game, 0, 0x0C);
        assert_eq!(game.inventory[0].id, 0x0C);
        assert_eq!(game.equipped, None, "the equipped Beretta is gone");
        assert_eq!(game.selected_item, None);
        assert_eq!(
            assign(&mut game, 5, 0x6F),
            1,
            "lands in the first empty slot"
        );
        assert_eq!(game.inventory[1].id, 0x6F, "past the held stacks appends");
        assign(&mut game, 6, 0x03);
        assert_eq!(game.inventory.len(), 2, "Chris has no seventh slot");
        assign(&mut game, 0, 0);
        assert_eq!(
            game.inventory,
            vec![InventoryItem {
                id: 0x6F,
                quantity: 1
            }]
        );
    }

    #[test]
    fn the_editor_walks_capacity_slots_and_picks_items() {
        let mut game = game(1);
        let mut editor = InventoryEditor::default();
        // Jill has eight slots; up wraps to the last.
        editor.handle_input(press(|i| i.up = true), &mut game);
        assert_eq!(editor.slot(), 7);
        editor.handle_input(press(|i| i.down = true), &mut game);
        assert_eq!(editor.slot(), 0);
        // Confirm opens the picker on EMPTY; walk to item 0x09 and pick it.
        editor.handle_input(press(|i| i.confirm = true), &mut game);
        assert_eq!(editor.picker().map(Picker::item), Some(0));
        editor.handle_input(press(|i| i.down = true), &mut game);
        editor.handle_input(press(|i| i.right = true), &mut game);
        assert_eq!(editor.picker().map(Picker::item), Some(9));
        assert_eq!(
            editor.handle_input(press(|i| i.confirm = true), &mut game),
            Step::Stay
        );
        assert!(editor.picker().is_none());
        assert_eq!(game.inventory[0].id, 0x09);
        // Cancel closes a picker first, then backs out of the tool.
        editor.handle_input(press(|i| i.confirm = true), &mut game);
        assert_eq!(
            editor.picker().map(Picker::item),
            Some(9),
            "opens on the held item"
        );
        editor.handle_input(press(|i| i.cancel = true), &mut game);
        assert!(editor.picker().is_none());
        assert_eq!(game.inventory[0].id, 0x09, "cancel keeps the slot");
        // Picking for an empty slot past the stacks moves the cursor to the
        // slot the item landed in.
        editor.handle_input(press(|i| i.down = true), &mut game);
        editor.handle_input(press(|i| i.down = true), &mut game);
        assert_eq!(editor.slot(), 2);
        editor.handle_input(press(|i| i.confirm = true), &mut game);
        editor.handle_input(press(|i| i.right = true), &mut game);
        editor.handle_input(press(|i| i.confirm = true), &mut game);
        assert_eq!(game.inventory[1].id, 0x01);
        assert_eq!(editor.slot(), 1);
        editor.handle_input(press(|i| i.up = true), &mut game);
        assert_eq!(
            editor.handle_input(press(|i| i.cancel = true), &mut game),
            Step::Back
        );
    }

    #[test]
    fn refresh_clamps_the_remembered_slot_to_the_new_capacity() {
        let mut jill = game(1);
        let mut editor = InventoryEditor::default();
        // Jill has eight slots: up wraps to the last one and scrolls.
        editor.handle_input(press(|i| i.up = true), &mut jill);
        assert_eq!((editor.slot, editor.scroll), (7, 4));
        // Chris has six: the remembered cursor lands on the last one and the
        // window keeps four rows visible.
        editor.refresh(&game(0));
        assert_eq!(editor.slot, 5);
        assert_eq!(editor.scroll, 2);
    }

    #[test]
    fn the_picker_covers_empty_through_the_minimi_and_wraps() {
        let mut picker = Picker::new(0);
        picker.step(press(|i| i.left = true));
        assert_eq!(picker.item(), PICKER_LAST_ITEM);
        assert_eq!(
            picker.scroll_row(),
            (PICKER_CELLS - 1) / PICKER_COLUMNS + 1 - PICKER_VISIBLE_ROWS
        );
        picker.step(press(|i| i.right = true));
        assert_eq!((picker.item(), picker.scroll_row()), (0, 0));
        picker.step(press(|i| i.up = true));
        assert_eq!(usize::from(picker.item()), PICKER_CELLS - PICKER_COLUMNS);
    }
}
