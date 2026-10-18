//! Pause-menu status/inventory screen and the USE/CHECK/MOVE item submenu.
//!
//! This is menu mode 0: the status panel (portrait, health face, EKG) plus the
//! inventory grid, its cursor and the item action submenu. The screen is
//! deliberately stateless about the engine: [`MainMenu::handle_input`] maps one
//! discrete [`MenuInput`] to a [`MenuEvent`] and mutates [`GameState`], and
//! [`MainMenu::draw`] paints the frozen gameplay frame's replacement into the
//! framebuffer. The integration layer owns the pad mapping, the message window
//! round trips and the fades.
//!
//! # Engine hook
//!
//! `crate::engine::GameSession` handles this screen explicitly (it is not a
//! [`crate::ui::Screen`] modal, because it mutates the session's
//! [`GameState`] and shares its message window):
//!
//! 1. [`MenuAssets`] load lazily on the first START, once per session;
//! 2. START freezes the room tick and opens [`MainMenu`] over the last
//!    gameplay frame, with the game's message window switched to the menu
//!    line;
//! 3. each frozen frame maps one pad edge to [`MenuInput`], calls
//!    [`MainMenu::handle_input`] and consumes the [`MenuEvent`]: `Close`
//!    unfreezes, `Message` starts a menu-paused window, `ViewItem` is a
//!    stub until the viewer slice, and `Tab`/`Changed` just redraw;
//! 4. the session draws the frozen frame, then the menu, then the message
//!    window over both.
//!
//! `--ui menu` boots straight into this state over room 1001 with a known
//! inventory; the screen has no SDL dependency.

use anyhow::{Context, Result};

use crate::bmp;
use crate::font::{Font, Tint};
use crate::game::{CombineResult, GameState, InventoryItem, STATE_BYTE_CHARACTER, UseResult};
use crate::items;
use crate::model::Texture8;
use crate::pack::Pack;
use crate::render::Framebuffer;
use crate::state::Image;
use crate::text::Text;
use crate::tim;

use super::layout;
use super::status::HealthBar;

/// Pack entry of the status frame sheet.
pub const STATUS_ENTRY: &str = "ui/status.tim";
/// Pack entry of the empty-slot sheet.
pub const BLUE_ENTRY: &str = "ui/blue.tim";
/// Pack entry of the character portrait sheet.
pub const STATFACE_ENTRY: &str = "ui/statface.tim";
/// Pack entry of the special-item sheet.
pub const STAITEM_ENTRY: &str = "ui/staitem.tim";
/// Pack entry of the item-icon atlas.
pub const ITEM_ALL_ENTRY: &str = items::ITEM_ALL_ENTRY;
/// Pack entry of the font sheet.
pub const FONT_ENTRY: &str = "font/font.tim";

/// The message shown when a combine needs the guardhouse drug store.
pub const MESSAGE_NEEDS_DRUG_STORE: u16 = 0xF2;
/// The message shown after a herb recipe is applied.
pub const MESSAGE_HERB_MIX: u16 = 0xF5;
/// The message shown when two herbs have no recipe between them.
pub const MESSAGE_NO_RECIPE: u16 = 0xF6;
/// First refusal message; add the use-category index.
pub const MESSAGE_USE_REFUSED: u16 = 0xF7;
/// The menu model's dim ambient light: the original's `setBackColor(0x199,
/// 0x199, 0x199)`, a 12-bit 10% grey. It lights the character model in the
/// viewport; it is not a screen clear and never paints the framebuffer.
pub const MENU_AMBIENT: u16 = 0x199;

/// The screen clear behind the menu panels: pure black, the same clear the
/// original uses before the menu frame is drawn. The centre viewport (the
/// inventory, item-view and item-box background) is this black.
const MENU_CLEAR: [u8; 4] = [0, 0, 0, 255];

/// One discrete menu input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuInput {
    /// Move the cursor up a row / previous action.
    Up,
    /// Move the cursor down a row / next action.
    Down,
    /// Move the cursor one column left.
    Left,
    /// Move the cursor one column right.
    Right,
    /// Confirm / action key.
    Confirm,
    /// Cancel / start key.
    Cancel,
    /// L1 / previous page; only the item box uses it.
    PageLeft,
    /// R1 / next page; only the item box uses it.
    PageRight,
}

/// What a handled input asks the integration layer to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuEvent {
    /// The input was consumed inside the menu.
    None,
    /// Close the menu and unfreeze gameplay.
    Close,
    /// Show a menu message through the message window.
    Message(u16),
    /// CHECK: hand the item to the item viewer (slice 7).
    ViewItem(u8),
    /// A top tab was chosen: the raw cursor (`0` map, `2` file, `4` radio).
    Tab(u8),
    /// The game state changed (use, equip, combine).
    Changed,
}

/// Which submenu owns the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuMode {
    /// The status/inventory screen's cursor.
    Navigation,
    /// The USE/CHECK/MOVE action submenu.
    Actions,
    /// The MOVE combine cursor.
    Move,
}

/// The pause menu's drawable assets.
pub struct MenuAssets {
    /// `ui/status.tim` frames, faces, digits and cursor art.
    pub status: Texture8,
    /// `ui/blue.tim` empty-slot plate.
    pub blue: Texture8,
    /// `ui/statface.tim` character portraits.
    pub statface: Texture8,
    /// `ui/staitem.tim` Ingram/Minimi icons.
    pub staitem: Texture8,
    /// `item/item_all.bmp` 40x30 icon atlas (black keyed transparent).
    pub item_all: Image,
    /// The text font.
    pub font: Font,
}

impl MenuAssets {
    /// Load every asset the menu draws from `pack`.
    /// Load the menu art. `font` is the session's glyph sheet — the script
    /// the language files encode in — falling back to the pack's default
    /// sheet when the caller has none (tests and capture-less helpers).
    pub fn load(pack: &Pack, font: Option<&Font>) -> Result<Self> {
        let status = decode_texture(pack, STATUS_ENTRY)?;
        let blue = decode_texture(pack, BLUE_ENTRY)?;
        let statface = decode_texture(pack, STATFACE_ENTRY)?;
        let staitem = decode_texture(pack, STAITEM_ENTRY)?;
        let item_all = bmp::decode_mask(
            &pack
                .read(ITEM_ALL_ENTRY)
                .with_context(|| format!("missing {ITEM_ALL_ENTRY}"))?,
        )
        .with_context(|| format!("invalid {ITEM_ALL_ENTRY}"))?;
        let font = match font {
            Some(font) => font.clone(),
            None => Font::new(tim::decode_4bpp(
                &pack
                    .read(FONT_ENTRY)
                    .with_context(|| format!("missing {FONT_ENTRY}"))?,
            )?),
        };
        Ok(Self {
            status,
            blue,
            statface,
            staitem,
            item_all,
            font,
        })
    }
}

fn decode_texture(pack: &Pack, entry: &str) -> Result<Texture8> {
    let bytes = pack
        .read(entry)
        .with_context(|| format!("missing {entry}"))?;
    tim::decode_8bpp(&bytes).with_context(|| format!("invalid {entry}"))
}

/// The pause menu's screen state.
#[derive(Debug, Clone)]
pub struct MainMenu {
    /// Current cursor (`0..=6` tabs, `8 + 2 * slot` item slots).
    pub cursor: u8,
    /// Which submenu owns the cursor.
    pub mode: MenuMode,
    /// Selected action in [`MenuMode::Actions`] (0 use/equip, 1 check, 2 move).
    pub action: u8,
    /// The MOVE cursor (`cursor` when the move starts).
    pub move_cursor: u8,
    /// The item under the cursor, or `0` when the cursor is on an empty slot
    /// or a tab.
    pub selected_item: u8,
    /// Whether the selected item supports the equip action.
    pub equippable: bool,
    /// Inventory slots of the current character (6 or 8).
    pub slots: usize,
    /// The animated health monitor.
    pub health_bar: HealthBar,
    /// The action submenu's open animation frame (`DAT_00ae9f27` low 7 bits):
    /// `0` on the confirm frame, then `1..=8` as the box grows from nothing to
    /// its full 48x24 over eight frames. Once it reaches `SUBMENU_OPEN_FRAMES`
    /// the box is fully open and accepts input.
    pub submenu_frames: u8,
    /// An action confirmed on the previous frame, run at the top of the next
    /// `tick`. The original's submenu confirm with the use/view actions
    /// switches to state 4 with `DAT_00ae9f22 = 0` and the action function
    /// runs on the following frame (equip toggles immediately).
    pending_action: Option<PendingAction>,
    /// The submenu's close animation is running (the open sequence in
    /// reverse, `DAT_00ae9f27` `0x88..=0x80`): input is gated until it
    /// finishes and the cursor returns to the inventory. The original always
    /// runs it after a successful use; an equip toggle keeps the submenu up.
    pub submenu_closing: bool,
    /// Whether the whole menu exits when the close animation finishes (the
    /// original's `DAT_00ae9f13 & 0x80` key/special/book close bit).
    close_after_submenu: bool,
}

/// A submenu action deferred to the next frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    /// Use the selected item (action 0, non-equippable).
    Use,
    /// Open the item viewer for the selected item (action 1).
    View(u8),
}

impl MainMenu {
    /// A menu for a character with `slots` inventory slots.
    pub fn new(slots: usize) -> Self {
        Self {
            cursor: layout::FIRST_SLOT_CURSOR,
            mode: MenuMode::Navigation,
            action: 0,
            move_cursor: layout::FIRST_SLOT_CURSOR,
            selected_item: 0,
            equippable: false,
            slots,
            health_bar: HealthBar::new(),
            submenu_frames: layout::SUBMENU_OPEN_FRAMES,
            pending_action: None,
            submenu_closing: false,
            close_after_submenu: false,
        }
    }

    /// Open the menu on the current inventory: cursor on the first slot,
    /// navigation mode, health monitor reset.
    pub fn open(&mut self, game: &mut GameState) {
        self.slots = game.inventory_capacity();
        self.cursor = layout::FIRST_SLOT_CURSOR;
        self.mode = MenuMode::Navigation;
        self.action = 0;
        self.move_cursor = self.cursor;
        self.health_bar = HealthBar::new();
        self.submenu_frames = layout::SUBMENU_OPEN_FRAMES;
        self.pending_action = None;
        self.submenu_closing = false;
        self.close_after_submenu = false;
        self.refresh_selection(game);
    }

    /// Whether the action submenu has finished its open animation and accepts
    /// input. The engine consumes no menu input while it is still opening.
    pub fn submenu_ready(&self) -> bool {
        self.submenu_frames >= layout::SUBMENU_OPEN_FRAMES
    }

    /// Advance one frame: run a deferred action first (the original's input
    /// runs before `menu_draw_health_bar`), then the health monitor, then the
    /// cursor mirror. Returns the event a deferred action produced.
    pub fn tick(&mut self, game: &mut GameState) -> Option<MenuEvent> {
        let mut event = match self.pending_action.take() {
            Some(PendingAction::Use) => Some(self.use_selected(game)),
            Some(PendingAction::View(item)) => {
                self.mode = MenuMode::Navigation;
                Some(MenuEvent::ViewItem(item))
            }
            None => None,
        };
        if self.submenu_closing {
            // The close walks the open sequence backwards; the cursor only
            // returns to the inventory when the box is gone.
            self.submenu_frames = self.submenu_frames.saturating_sub(1);
            if self.submenu_frames == 0 {
                self.submenu_closing = false;
                self.mode = MenuMode::Navigation;
                self.refresh_selection(game);
                if self.close_after_submenu {
                    self.close_after_submenu = false;
                    event = Some(MenuEvent::Close);
                }
            }
        } else if self.mode == MenuMode::Navigation {
            self.refresh_selection(game);
        } else if self.submenu_frames < layout::SUBMENU_OPEN_FRAMES {
            self.submenu_frames += 1;
        }
        let health = game.entities[0].health;
        let max_health = game.max_health;
        let status = game.health_status;
        self.health_bar
            .update(health, max_health, status, &mut game.rand_state);
        event
    }

    /// Move the cursor to `cursor` and re-read the item under it.
    ///
    /// The item-box overlay drives its own copy of the inventory cursor; the
    /// engine mirrors it here so the slot cursor, the item name and the
    /// selected item stay on the same slot.
    pub fn move_cursor_to(&mut self, game: &mut GameState, cursor: u8) {
        self.cursor = cursor;
        if self.mode == MenuMode::Navigation {
            self.refresh_selection(game);
        }
    }

    /// Move the cursor to the next top tab and return its cursor value. The
    /// cycle is map (0) -> file (2) -> radio (4) -> exit (6) -> map; the radio
    /// tab is skipped while the player does not carry the radio. Landing on a
    /// tab clears the item selection, exactly like walking there with the pad.
    pub fn cycle_tab(&mut self, game: &mut GameState, has_radio: bool) -> u8 {
        let mut next = if layout::is_tab(self.cursor) {
            (self.cursor + 2) & 7
        } else {
            0
        };
        if next == 4 && !has_radio {
            next = 6;
        }
        self.cursor = next;
        self.mode = MenuMode::Navigation;
        self.refresh_selection(game);
        next
    }

    /// Handle one input, mutating `game` where the action takes effect.
    pub fn handle_input(&mut self, game: &mut GameState, input: MenuInput) -> MenuEvent {
        if self.submenu_closing {
            return MenuEvent::None;
        }
        match self.mode {
            MenuMode::Navigation => self.navigation_input(game, input),
            MenuMode::Actions => self.action_input(game, input),
            MenuMode::Move => self.move_input(game, input),
        }
    }

    fn navigation_input(&mut self, game: &mut GameState, input: MenuInput) -> MenuEvent {
        match input {
            MenuInput::Left | MenuInput::Right => {
                self.cursor = layout::move_sideways(self.cursor);
                self.refresh_selection(game);
                MenuEvent::None
            }
            MenuInput::Up => {
                self.cursor = layout::move_up(self.cursor, self.slots);
                self.refresh_selection(game);
                MenuEvent::None
            }
            MenuInput::Down => {
                self.cursor = layout::move_down(self.cursor, self.slots);
                self.refresh_selection(game);
                MenuEvent::None
            }
            MenuInput::Cancel => MenuEvent::Close,
            MenuInput::PageLeft | MenuInput::PageRight => MenuEvent::None,
            MenuInput::Confirm => {
                if layout::is_tab(self.cursor) {
                    return if self.cursor == 6 {
                        MenuEvent::Close
                    } else {
                        MenuEvent::Tab(self.cursor)
                    };
                }
                if self.selected_item == 0 {
                    return MenuEvent::None;
                }
                self.mode = MenuMode::Actions;
                self.action = 0;
                self.move_cursor = self.cursor;
                self.equippable = is_equippable(self.selected_item);
                // Start the submenu box's open animation from nothing.
                self.submenu_frames = 0;
                MenuEvent::None
            }
        }
    }

    fn action_input(&mut self, game: &mut GameState, input: MenuInput) -> MenuEvent {
        match input {
            MenuInput::Cancel => {
                self.mode = MenuMode::Navigation;
                MenuEvent::None
            }
            MenuInput::Up => {
                self.action = (self.action + 2) % 3;
                MenuEvent::None
            }
            MenuInput::Down => {
                self.action = (self.action + 1) % 3;
                MenuEvent::None
            }
            MenuInput::Left | MenuInput::Right | MenuInput::PageLeft | MenuInput::PageRight => {
                MenuEvent::None
            }
            MenuInput::Confirm => self.apply_action(game),
        }
    }

    fn apply_action(&mut self, game: &mut GameState) -> MenuEvent {
        let item = self.selected_item;
        match self.action {
            0 if self.equippable => {
                let equipped = game.equipped == Some(item);
                game.set_equipped(if equipped { None } else { Some(item) });
                // The original's equip toggle also closes the submenu through
                // its state-3 animation (`DAT_00ae9f21 = 3`).
                self.submenu_closing = true;
                MenuEvent::Changed
            }
            0 => {
                self.pending_action = Some(PendingAction::Use);
                MenuEvent::None
            }
            1 => {
                self.pending_action = Some(PendingAction::View(item));
                MenuEvent::None
            }
            2 => {
                self.mode = MenuMode::Move;
                self.move_cursor = self.cursor;
                MenuEvent::None
            }
            _ => MenuEvent::None,
        }
    }

    /// Use the selected item: apply the effect, consume one, record the used
    /// item and kick the EKG flush.
    ///
    /// A successful key/special/chemical/book use sets the original's
    /// `DAT_00ae9f13 & 0x80` close bit (`menu_item_use_if_flag` /
    /// `menu_item_use_red_book`), so once the submenu's close animation has run
    /// the whole menu exits (`main_menu` case 1's `DAT_00ae9f13 & 0x41` test
    /// with `menu_item_submenu`'s state-3 `| 1`). The port reports the same
    /// outcome as [`MenuEvent::Close`].
    fn use_selected(&mut self, game: &mut GameState) -> MenuEvent {
        let item = self.selected_item;
        if item == 0 {
            return MenuEvent::None;
        }
        match game.use_item(item) {
            UseResult::Used { healed, cured } => {
                game.record_used_item(item);
                let slot = layout::slot_of_cursor(self.cursor).unwrap_or(0);
                consume_slot(game, slot);
                self.health_bar.start_flush(healed, cured);
                self.refresh_selection(game);
                // A successful use always runs the submenu's close animation
                // (state 3) before the cursor returns to the inventory; a
                // key/special/book item also arms the menu's close bit, so the
                // whole menu exits when the animation finishes.
                self.submenu_closing = true;
                self.close_after_submenu = closes_inventory(item);
                MenuEvent::Changed
            }
            UseResult::Unusable => MenuEvent::Message(MESSAGE_USE_REFUSED + category_index(item)),
        }
    }

    fn move_input(&mut self, game: &mut GameState, input: MenuInput) -> MenuEvent {
        match input {
            MenuInput::Cancel => {
                // Walking back from MOVE returns to the already-open submenu:
                // the original does not replay the open animation.
                self.mode = MenuMode::Actions;
                self.submenu_frames = layout::SUBMENU_OPEN_FRAMES;
                MenuEvent::None
            }
            MenuInput::Left | MenuInput::Right => {
                self.move_cursor = layout::move_sideways(self.move_cursor);
                MenuEvent::None
            }
            MenuInput::PageLeft | MenuInput::PageRight => MenuEvent::None,
            MenuInput::Up => {
                self.move_cursor = layout::move_up(self.move_cursor, self.slots);
                MenuEvent::None
            }
            MenuInput::Down => {
                self.move_cursor = layout::move_down(self.move_cursor, self.slots);
                MenuEvent::None
            }
            MenuInput::Confirm => {
                if self.move_cursor == self.cursor {
                    return MenuEvent::None;
                }
                let Some(cursor_slot) = layout::slot_of_cursor(self.cursor) else {
                    return MenuEvent::None;
                };
                let Some(target_slot) = layout::slot_of_cursor(self.move_cursor) else {
                    return MenuEvent::None;
                };
                if target_slot >= game.inventory.len() {
                    return MenuEvent::None;
                }
                match game.combine_slots(cursor_slot, target_slot) {
                    CombineResult::Applied { herb, .. } => {
                        self.refresh_selection(game);
                        self.mode = MenuMode::Navigation;
                        if herb {
                            MenuEvent::Message(MESSAGE_HERB_MIX)
                        } else {
                            MenuEvent::Changed
                        }
                    }
                    CombineResult::NeedsDrugStore => MenuEvent::Message(MESSAGE_NEEDS_DRUG_STORE),
                    // Two herbs with no recipe: the original's distinct
                    // "cannot combine" result shows `0xF6`; every other pair
                    // is refused silently.
                    CombineResult::HerbNoRecipe => MenuEvent::Message(MESSAGE_NO_RECIPE),
                    CombineResult::NoRecipe => MenuEvent::None,
                }
            }
        }
    }

    /// Re-read the item under the cursor and mirror it into the state byte the
    /// scripts observe with `cmpb` 6.
    fn refresh_selection(&mut self, game: &mut GameState) {
        let item = layout::slot_of_cursor(self.cursor)
            .and_then(|slot| game.inventory.get(slot))
            .map(|stack| stack.id)
            .filter(|id| *id != 0);
        self.selected_item = item.unwrap_or(0);
        self.equippable = item.is_some_and(is_equippable);
        game.select_item(item);
    }

    /// Draw the whole screen over the framebuffer's current contents.
    ///
    /// The layer order follows the original's depth sort (a lower depth draws
    /// later, i.e. nearer the viewer): the background and its border masks,
    /// then the tab highlight (drawn *behind* the tab art so the bar shows
    /// through the tab's transparent pixels), then the frames, the inventory,
    /// the portraits and the tabs. The item-slot cursor is the topmost layer.
    pub fn draw(
        &self,
        framebuffer: &mut Framebuffer,
        assets: &MenuAssets,
        text: &Text,
        game: &GameState,
    ) {
        self.draw_with_name(framebuffer, assets, text, game, true, true);
    }

    /// [`MainMenu::draw`] with the item cursor and the item-name line optional.
    ///
    /// The original draws the cursor and the 0xBA name line from
    /// `menu_handle_input` (state 1) and the submenu states that still own the
    /// screen. While the menu fade is still running (state 0) neither is drawn;
    /// while a message, the examine viewer or a pick-up owns the line the name
    /// must not paint over them (`menu_item_submenu` case 4's jump past the
    /// name draw).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_with_name(
        &self,
        framebuffer: &mut Framebuffer,
        assets: &MenuAssets,
        text: &Text,
        game: &GameState,
        show_cursor: bool,
        show_item_name: bool,
    ) {
        self.draw_background(framebuffer);
        if show_cursor {
            self.draw_tab_cursor(framebuffer, assets);
        }
        self.draw_frames(framebuffer, assets);
        self.draw_inventory(framebuffer, assets, game);
        self.draw_portrait(framebuffer, assets, game);
        self.health_bar.draw(framebuffer, &assets.status);
        self.draw_tabs(framebuffer, assets, game.has_radio());
        if self.mode != MenuMode::Navigation || self.submenu_closing {
            self.draw_submenu(framebuffer, assets);
        }
        if show_cursor {
            self.draw_slot_cursor(framebuffer, assets);
        }
        if show_item_name {
            self.draw_item_name(framebuffer, assets, text, game);
        }
    }

    /// Clear the whole screen to black and lay the four black border masks
    /// over it.
    ///
    /// The centre viewport rect stays black (the original renders the
    /// character model over the same screen clear), so the frozen room never
    /// shows through the menu.
    fn draw_background(&self, framebuffer: &mut Framebuffer) {
        layout::fill_rect(
            framebuffer,
            layout::Rect {
                pos: [0, 0],
                size: [framebuffer.width as i16, framebuffer.height as i16],
            },
            MENU_CLEAR,
        );
        for rect in layout::black_rects() {
            layout::fill_rect(framebuffer, rect, [0, 0, 0, 255]);
        }
    }

    /// Re-draw the frame decorations that overlap `rect` over a sub-screen's
    /// content.
    ///
    /// The original queues the status screen's frame decorations and the map
    /// tab's sprites into one ordering table, and the decorations' depth is
    /// the nearer one: the panel's right-hand edge (the 8x112 part at
    /// (192,20)) is drawn over the map's composite, and its dithered left
    /// column lets the map show through every other row. Everything else the
    /// status screen draws - the health monitor, the portrait, the tabs - is
    /// under the map tab's sprites, so only the overlapping decorations are
    /// re-drawn.
    pub fn draw_frames_over(
        &self,
        framebuffer: &mut Framebuffer,
        assets: &MenuAssets,
        rect: layout::Rect,
    ) {
        let overlaps = |pos: [i16; 2], size: [i16; 2]| {
            pos[0] < rect.pos[0] + rect.size[0]
                && pos[0] + size[0] > rect.pos[0]
                && pos[1] < rect.pos[1] + rect.size[1]
                && pos[1] + size[1] > rect.pos[1]
        };
        for part in layout::frame_parts() {
            if overlaps(part.pos, part.size) {
                draw_part(framebuffer, &assets.status, part);
            }
        }
        for tile in layout::common_tiles() {
            if overlaps(tile.part.pos, tile.part.size) {
                draw_tile(framebuffer, &assets.status, tile);
            }
        }
        for tile in layout::border_tiles(self.slots) {
            if overlaps(tile.part.pos, tile.part.size) {
                draw_tile(framebuffer, &assets.status, tile);
            }
        }
    }

    fn draw_frames(&self, framebuffer: &mut Framebuffer, assets: &MenuAssets) {
        for part in layout::frame_parts() {
            draw_part(framebuffer, &assets.status, part);
        }
        for tile in layout::common_tiles() {
            draw_tile(framebuffer, &assets.status, tile);
        }
        for tile in layout::border_tiles(self.slots) {
            draw_tile(framebuffer, &assets.status, tile);
        }
    }

    fn draw_inventory(&self, framebuffer: &mut Framebuffer, assets: &MenuAssets, game: &GameState) {
        for slot in 0..self.slots {
            let Some(position) = layout::slot_position(self.slots, slot) else {
                continue;
            };
            let stack = game.inventory.get(slot).copied().unwrap_or_default();
            if stack.id == 0 {
                draw_empty_slot(framebuffer, assets, position);
            } else {
                draw_item_icon(framebuffer, assets, stack.id, position);
                draw_item_quantity(framebuffer, assets, game, stack, position);
            }
        }
        let [x, y] = layout::EQUIPPED_POS;
        match game.equipped {
            Some(item) if item != 0 => {
                draw_item_icon(framebuffer, assets, item, [x, y]);
                draw_item_quantity(
                    framebuffer,
                    assets,
                    game,
                    InventoryItem {
                        id: item,
                        quantity: game.item_count(item).min(0xFF) as u8,
                    },
                    [x, y],
                );
            }
            _ => draw_empty_slot(framebuffer, assets, [x, y]),
        }
    }

    fn draw_portrait(&self, framebuffer: &mut Framebuffer, assets: &MenuAssets, game: &GameState) {
        let character = game.state_bytes[usize::from(STATE_BYTE_CHARACTER)] & 3;
        let [x, y] = layout::PORTRAIT_POS;
        let [w, h] = layout::PORTRAIT_SIZE;
        framebuffer.draw_indexed_sprite(
            &assets.statface,
            [
                i32::from(character & 1) * 32,
                i32::from(character & 2) * 16,
                32,
                32,
            ],
            [i32::from(x), i32::from(y), i32::from(w), i32::from(h)],
            0,
            0,
            Tint::White,
        );
    }

    fn draw_tabs(&self, framebuffer: &mut Framebuffer, assets: &MenuAssets, has_radio: bool) {
        for tab in 0..layout::TOP_TABS_LEN {
            let mut part = layout::tab_part(tab);
            // The original swaps the RADIO tab art for a blank dash (source V
            // 0x40) while `DAT_00ae9f1f` is clear: `MainMenu.cpp:925`,
            // `if (bVar6 == 4 && DAT_00ae9f1f == 0) g_TextureDesc.texV = 0x40`.
            // Tab index 2 is the radio tab (cursor value 4, bottom-left).
            if tab == 2 && !has_radio {
                part.uv[1] = 0x40;
            }
            draw_part(framebuffer, &assets.status, part);
        }
    }

    fn draw_submenu(&self, framebuffer: &mut Framebuffer, assets: &MenuAssets) {
        let labels: [u8; layout::SUBMENU_OPTION_COUNT] = if self.equippable {
            [0x00, 0x30, 0x48]
        } else {
            [0x18, 0x30, 0x48]
        };
        let [x, y] = layout::SUBMENU_POS;
        let [fw, fh] = layout::SUBMENU_OPTION_SIZE;
        // The original grows the box from nothing at (0x90, 0x39): each open
        // frame samples a 6px-wider, 3px-taller region of the option art, so
        // the full 48x24 is revealed over eight frames. The options' screen
        // rows are at the full 0x18 pitch throughout.
        let frame = self.submenu_frames.min(layout::SUBMENU_OPEN_FRAMES);
        let w = i32::from(fw).min(i32::from(frame) * 6);
        let h = i32::from(fh).min(i32::from(frame) * 3);
        if w <= 0 || h <= 0 {
            return;
        }
        if self.submenu_ready() {
            // Selected action highlight behind the option list.
            framebuffer.draw_indexed_sprite(
                &assets.status,
                [0x30, 0x60, i32::from(fw), i32::from(fh)],
                [
                    i32::from(x),
                    i32::from(y) + i32::from(self.action) * i32::from(fh),
                    i32::from(fw),
                    i32::from(fh),
                ],
                0,
                0,
                Tint::White,
            );
        }
        for (option, v) in labels.into_iter().enumerate() {
            framebuffer.draw_indexed_sprite(
                &assets.status,
                [0x30, i32::from(v), w, h],
                [
                    i32::from(x),
                    i32::from(y) + option as i32 * i32::from(fh),
                    w,
                    h,
                ],
                0,
                0,
                Tint::White,
            );
        }
    }

    /// The top-tab highlight: a solid red 48x16 bar at the cursor's tab.
    ///
    /// It is drawn behind the tab art (see [`Self::draw`]), so the tab's
    /// transparent background reveals the bar while its glyph keeps the
    /// button readable.
    fn draw_tab_cursor(&self, framebuffer: &mut Framebuffer, assets: &MenuAssets) {
        if self.mode != MenuMode::Navigation {
            return;
        }
        let cursor = self.cursor;
        if !layout::is_tab(cursor) {
            return;
        }
        let Some(position) = layout::cursor_position(cursor, self.slots) else {
            return;
        };
        framebuffer.draw_indexed_sprite(
            &assets.status,
            [
                0,
                0x50,
                i32::from(layout::TAB_CURSOR_SIZE[0]),
                i32::from(layout::TAB_CURSOR_SIZE[1]),
            ],
            [
                i32::from(position[0]),
                i32::from(position[1]),
                i32::from(layout::TAB_CURSOR_SIZE[0]),
                i32::from(layout::TAB_CURSOR_SIZE[1]),
            ],
            0,
            0,
            Tint::White,
        );
    }

    /// The item-slot cursor, the topmost layer of the screen.
    fn draw_slot_cursor(&self, framebuffer: &mut Framebuffer, assets: &MenuAssets) {
        let cursor = match self.mode {
            MenuMode::Move => self.move_cursor,
            MenuMode::Navigation => self.cursor,
            MenuMode::Actions => return,
        };
        let Some(position) = layout::cursor_position(cursor, self.slots) else {
            return;
        };
        if layout::is_tab(cursor) {
            return;
        }
        framebuffer.draw_indexed_sprite(
            &assets.status,
            [0x80, 0xE0, 40, 30],
            [i32::from(position[0]), i32::from(position[1]), 40, 30],
            0,
            0,
            Tint::White,
        );
    }

    fn draw_item_name(
        &self,
        framebuffer: &mut Framebuffer,
        assets: &MenuAssets,
        text: &Text,
        game: &GameState,
    ) {
        if self.selected_item == 0 {
            return;
        }
        let Some(name) = item_name_bytes(text, self.selected_item, &game.examined_flags()) else {
            return;
        };
        assets.font.draw_text(
            framebuffer,
            assets.font.metrics.left_margin,
            layout::ITEM_NAME_Y,
            Tint::White,
            0,
            name,
            text.uses_tight_metrics(),
        );
    }
}

/// Whether an item takes the equip action instead of use (`item < CLIP` or
/// above the non-infinite maximum).
pub fn is_equippable(item: u8) -> bool {
    !(0x0B..=0x6E).contains(&item)
}

/// Whether a successful use of `item` closes the inventory afterwards: the
/// key, special, chemical and book dispatch slots set the original's
/// `DAT_00ae9f13` bit 0x80, which the main menu's case-1 test turns into a
/// full menu exit. (Books only reach here as the red book, the only usable
/// one.)
pub fn closes_inventory(item: u8) -> bool {
    matches!(
        items::use_category(item),
        items::UseCategory::Chemicals
            | items::UseCategory::Special
            | items::UseCategory::Keys
            | items::UseCategory::Books
    )
}

/// The original use-category dispatch index (weapons through unusable).
fn category_index(item: u8) -> u16 {
    match items::use_category(item) {
        items::UseCategory::Weapons => 0,
        items::UseCategory::Ammo => 1,
        items::UseCategory::Bottle => 2,
        items::UseCategory::Chemicals => 3,
        items::UseCategory::Special => 4,
        items::UseCategory::Keys => 5,
        items::UseCategory::Books => 6,
        items::UseCategory::Heal => 7,
        items::UseCategory::Always => 8,
        items::UseCategory::Unusable => 9,
    }
}

/// Remove one item from `slot`, compacting the inventory.
fn consume_slot(game: &mut GameState, slot: usize) {
    if let Some(stack) = game.inventory.get_mut(slot) {
        if stack.quantity > 0 {
            stack.quantity -= 1;
        }
        if stack.quantity == 0 {
            stack.id = 0;
        }
    }
    game.rebuild_slots();
    if game.equipped.is_some_and(|item| !game.has_item(item)) {
        game.set_equipped(None);
    }
}

/// The item's encoded name: the real name when the record has one, the
/// generic-name class otherwise. An item the player has not examined yet (its
/// class bit clear in `examined`) shows the generic name, exactly like the
/// original's `message_item_name_lookup`.
pub fn item_name_bytes<'a>(text: &'a Text, item: u8, examined: &[u8; 4]) -> Option<&'a [u8]> {
    match items::record(item) {
        Some(record)
            if !record.name_valid()
                && !crate::message::examined_bit(examined, record.name_class) =>
        {
            text.item_name_unchecked(u16::from(item))
                .or_else(|| text.item_name(u16::from(item)))
        }
        _ => text.item_name(u16::from(item)),
    }
}

fn draw_part(framebuffer: &mut Framebuffer, texture: &Texture8, part: layout::FramePart) {
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

fn draw_tile(framebuffer: &mut Framebuffer, texture: &Texture8, tile: layout::FrameTile) {
    let [x, y] = tile.part.pos;
    let [w, h] = tile.part.size;
    // The tiled record's flag byte maps straight onto the sprite mirror bits
    // the original sets from `(flags & 0xc0) << 16` (SpriteRenderer.h:
    // TEXDESC_MIRROR_U 0x80, TEXDESC_MIRROR_V 0x40). The bottom decoration
    // record (0x40) is the top one flipped with V.
    let mirror = [tile.flags & 0x80 != 0, tile.flags & 0x40 != 0];
    for index in 0..tile.tiles() {
        let (dx, dy) = if tile.vertical() {
            (0, i32::from(index) * i32::from(h))
        } else {
            (i32::from(index) * i32::from(w), 0)
        };
        framebuffer.draw_indexed_sprite_mirrored(
            texture,
            [
                i32::from(tile.part.uv[0]),
                i32::from(tile.part.uv[1]),
                i32::from(w),
                i32::from(h),
            ],
            [
                i32::from(x) + dx,
                i32::from(y) + dy,
                i32::from(w),
                i32::from(h),
            ],
            0,
            0,
            Tint::White,
            mirror,
        );
    }
}

/// Draw an empty inventory slot from `ui/blue.tim`.
fn draw_empty_slot(framebuffer: &mut Framebuffer, assets: &MenuAssets, position: [i16; 2]) {
    framebuffer.draw_indexed_sprite(
        &assets.blue,
        [0, 0, 40, 30],
        [i32::from(position[0]), i32::from(position[1]), 40, 30],
        0,
        0,
        Tint::White,
    );
}

/// Draw one 40x30 item icon: `item/item_all.bmp` for ids below `0x6F`, the
/// `ui/staitem.tim` special rows for the Ingram and Minimi.
pub fn draw_item_icon(
    framebuffer: &mut Framebuffer,
    assets: &MenuAssets,
    item: u8,
    position: [i16; 2],
) {
    draw_item_icon_slice(framebuffer, assets, item, position, 0, 30);
}

/// [`draw_item_icon`] with a vertical slice of the icon: `source_v` skips the
/// first rows of the 40x30 cell and `height` draws only that many.
///
/// The item box's page slide samples its VRAM icon two pixels further down per
/// step inside a window that shrinks from the bottom (`texV = base + state * 2`
/// with `height = 0x1E - state * 2`), which this reproduces.
pub fn draw_item_icon_slice(
    framebuffer: &mut Framebuffer,
    assets: &MenuAssets,
    item: u8,
    position: [i16; 2],
    source_v: i32,
    height: i32,
) {
    if height <= 0 {
        return;
    }
    let dst = [i32::from(position[0]), i32::from(position[1]), 40, height];
    if item < 0x6F {
        let row = usize::from(items::image_class(item).wrapping_sub(1));
        if items::image_class(item) == 0 || row >= items::ITEM_ALL_ROWS {
            return;
        }
        framebuffer.draw_rgba_sprite(
            &assets.item_all,
            [
                0,
                (row * items::ICON_HEIGHT as usize) as i32 + source_v,
                40,
                height,
            ],
            dst,
            0,
        );
    } else {
        let row = i32::from(item - 0x6F);
        framebuffer.draw_indexed_sprite(
            &assets.staitem,
            [0, row * 30 + source_v, 40, height],
            dst,
            0,
            0,
            Tint::White,
        );
    }
}

/// Draw the item's three-digit quantity or the infinity glyph.
pub fn draw_item_quantity(
    framebuffer: &mut Framebuffer,
    assets: &MenuAssets,
    game: &GameState,
    stack: InventoryItem,
    position: [i16; 2],
) {
    draw_item_quantity_clipped(framebuffer, assets, game, stack, position, i32::MAX);
}

/// [`draw_item_quantity`] with every glyph clipped to rows above `clip_bottom`
/// (exclusive). The item box's preview column uses it to stop a sliding slot's
/// quantity where the panel's lower mask ends.
pub fn draw_item_quantity_clipped(
    framebuffer: &mut Framebuffer,
    assets: &MenuAssets,
    game: &GameState,
    stack: InventoryItem,
    position: [i16; 2],
    clip_bottom: i32,
) {
    let item = stack.id;
    let quantifiable = (item < 0x13 && item != 0x01) || item == 0x2F || item > items::MAX_ITEM;
    if !quantifiable {
        return;
    }
    let infinite = item > items::MAX_ITEM || (item == 0x0A && game.flag_test(0, 0x7E, false));
    let x = i32::from(position[0]);
    let y = i32::from(position[1]) + 0x14;
    let height = (clip_bottom - y).clamp(0, 8);
    if height <= 0 {
        return;
    }
    if infinite {
        framebuffer.draw_indexed_sprite(
            &assets.status,
            [152, 112, 10, height],
            [x + 6, y, 10, height],
            0,
            0,
            Tint::White,
        );
        return;
    }
    let mut quantity = stack.quantity;
    if item != 0x06 && item < 0x0B {
        quantity &= 0x7F;
    }
    let digits = [quantity / 100, (quantity / 10) % 10, quantity % 10];
    let column = match item {
        4 | 8 | 0x0D | 0x11 => 0x88,
        7 | 0x12 => 0xC0,
        _ => 0x80,
    };
    let mut cursor = x + if item < 0x0B { 4 } else { 0x0E };
    let mut drawn = false;
    for (index, digit) in digits.into_iter().enumerate() {
        let last = index == 2;
        if digit != 0 || drawn || last {
            framebuffer.draw_indexed_sprite(
                &assets.status,
                [column, i32::from(digit) * 8, 8, height],
                [cursor, y, 8, height],
                0,
                0,
                Tint::White,
            );
            drawn = true;
        }
        if drawn || item > 0x0A {
            cursor += 8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{STATE_BYTE_EQUIPPED, STATE_BYTE_SELECTED_ITEM, STATE_BYTE_USED_ITEM};
    use crate::model::PALETTE_ROW_LEN;
    use crate::state::{RoomId, RoomState};

    fn new_game() -> GameState {
        let mut game = GameState::new(RoomId::parse("1001").unwrap(), &RoomState::default());
        game.max_health = 96;
        game.entities[0].health = 96;
        game
    }

    fn pixel(framebuffer: &Framebuffer, x: usize, y: usize) -> [u8; 4] {
        let offset = (y * framebuffer.width as usize + x) * 4;
        framebuffer.rgba[offset..offset + 4].try_into().unwrap()
    }

    /// Confirm the selected action and run its one-frame hand-off plus the
    /// submenu's close animation, returning the last event the animation
    /// produced (a key/book use closes the inventory when it finishes).
    fn confirm_action(menu: &mut MainMenu, game: &mut GameState) -> Option<MenuEvent> {
        menu.handle_input(game, MenuInput::Confirm);
        let mut event = menu.tick(game);
        while menu.submenu_closing {
            if let Some(next) = menu.tick(game) {
                event = Some(next);
            }
        }
        event
    }

    fn solid_texture(width: u32, height: u32, patches: &[([u32; 4], u8, [u8; 4])]) -> Texture8 {
        let mut indices = vec![0u8; (width * height) as usize];
        for &([x, y, w, h], index, _) in patches {
            for row in y..y + h {
                for column in x..x + w {
                    indices[(row * width + column) as usize] = index;
                }
            }
        }
        let mut palettes = vec![[0u8; 4]; PALETTE_ROW_LEN];
        for &(_, index, color) in patches {
            if index != 0 {
                palettes[usize::from(index)] = color;
            }
        }
        Texture8 {
            width,
            height,
            indices,
            palettes,
            stp: Vec::new(),
        }
    }

    fn image(width: u32, height: u32, patch: [u8; 4]) -> Image {
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for pixel in rgba.as_chunks_mut::<4>().0.iter_mut() {
            *pixel = patch;
        }
        Image {
            width,
            height,
            rgba,
        }
    }

    fn assets() -> MenuAssets {
        MenuAssets {
            // Tabs, frames, cursor and quantity glyphs all sample from here.
            status: solid_texture(
                256,
                256,
                &[
                    ([0x80, 0xE0, 40, 30], 1, [255, 0, 255, 255]),
                    ([0, 0x50, 48, 16], 2, [0, 255, 255, 255]),
                    ([0, 0, 40, 30], 3, [10, 20, 30, 255]),
                    ([0x80, 0, 8, 8], 4, [0, 0, 255, 255]),
                    ([0x88, 0, 8, 8], 5, [255, 128, 0, 255]),
                    ([0xC0, 0, 8, 8], 6, [255, 0, 0, 255]),
                    ([152, 112, 10, 8], 7, [255, 255, 255, 255]),
                ],
            ),
            blue: solid_texture(64, 64, &[([0, 0, 40, 30], 1, [0, 0, 255, 255])]),
            statface: solid_texture(64, 64, &[([0, 0, 32, 32], 1, [255, 255, 0, 255])]),
            staitem: solid_texture(64, 64, &[([0, 0, 40, 30], 1, [1, 2, 3, 255])]),
            item_all: image(40, 2160, [0, 255, 0, 255]),
            font: Font::new(Texture8 {
                width: 768,
                height: 256,
                indices: vec![0; 768 * 256],
                palettes: vec![[0u8; 4]; PALETTE_ROW_LEN],
                stp: Vec::new(),
            }),
        }
    }

    #[test]
    fn cursor_navigation_uses_the_layout_rules() {
        let mut game = new_game();
        game.add_item(0x44, 1);
        game.add_item(0x43, 1);
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        assert_eq!(menu.cursor, 8);
        assert_eq!(menu.selected_item, 0x44);
        assert_eq!(game.selected_item, Some(0x44));
        assert_eq!(
            game.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)],
            0x44
        );

        menu.handle_input(&mut game, MenuInput::Right);
        assert_eq!(menu.cursor, 10);
        assert_eq!(menu.selected_item, 0x43);
        menu.handle_input(&mut game, MenuInput::Up);
        assert_eq!(menu.cursor, 6, "up from the top slot row reaches the tabs");
        assert_eq!(menu.selected_item, 0);
        assert_eq!(game.selected_item, None);
        menu.handle_input(&mut game, MenuInput::Up);
        assert_eq!(menu.cursor, 2);
        menu.handle_input(&mut game, MenuInput::Up);
        assert_eq!(
            menu.cursor, 22,
            "up from the top tab row wraps to the bottom"
        );

        game.id.player_flag = 0;
        let mut chris = MainMenu::new(6);
        chris.open(&mut game);
        assert_eq!(chris.slots, 6);
        chris.handle_input(&mut game, MenuInput::Down);
        chris.handle_input(&mut game, MenuInput::Down);
        assert_eq!(chris.cursor, 16);
        chris.handle_input(&mut game, MenuInput::Down);
        assert_eq!(chris.cursor, 0, "past the sixth slot wraps to the tab row");
    }

    #[test]
    fn submenu_transitions_and_check_return() {
        let mut game = new_game();
        game.add_item(0x44, 1);
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);

        // Confirm on the item opens the action submenu.
        assert_eq!(
            menu.handle_input(&mut game, MenuInput::Confirm),
            MenuEvent::None
        );
        assert_eq!(menu.mode, MenuMode::Actions);
        assert!(!menu.equippable);
        // Down twice selects MOVE, then cancel walks back.
        menu.handle_input(&mut game, MenuInput::Down);
        menu.handle_input(&mut game, MenuInput::Down);
        assert_eq!(menu.action, 2);
        menu.handle_input(&mut game, MenuInput::Cancel);
        assert_eq!(menu.mode, MenuMode::Navigation);

        // CHECK returns the viewer action.
        menu.handle_input(&mut game, MenuInput::Confirm);
        menu.handle_input(&mut game, MenuInput::Down);
        assert_eq!(menu.action, 1);
        assert_eq!(
            confirm_action(&mut menu, &mut game),
            Some(MenuEvent::ViewItem(0x44))
        );

        // An empty slot refuses to open the submenu.
        let mut empty = new_game();
        let mut menu = MainMenu::new(8);
        menu.open(&mut empty);
        assert_eq!(
            menu.handle_input(&mut empty, MenuInput::Confirm),
            MenuEvent::None
        );
        assert_eq!(menu.mode, MenuMode::Navigation);
    }

    #[test]
    fn tab_and_exit_confirmations() {
        let mut game = new_game();
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.cursor = 4;
        assert_eq!(
            menu.handle_input(&mut game, MenuInput::Confirm),
            MenuEvent::Tab(4)
        );
        menu.cursor = 6;
        assert_eq!(
            menu.handle_input(&mut game, MenuInput::Confirm),
            MenuEvent::Close
        );
        assert_eq!(
            menu.handle_input(&mut game, MenuInput::Cancel),
            MenuEvent::Close
        );
    }

    #[test]
    fn a_herb_heal_through_the_menu_updates_health_and_state() {
        let mut game = new_game();
        game.entities[0].health = 20;
        game.add_item(0x44, 1);
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);

        menu.handle_input(&mut game, MenuInput::Confirm);
        // The action runs one frame after the confirm; assert the flush's
        // first frame before the submenu's close animation advances it.
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert_eq!(menu.tick(&mut game), Some(MenuEvent::Changed));
        // A green herb restores exactly one third of the maximum.
        assert_eq!(game.entities[0].health, 20 + 96 / 3);
        assert!(game.inventory.is_empty(), "the herb was consumed");
        assert_eq!(game.last_used_item, Some(0x44));
        assert_eq!(game.state_bytes[usize::from(STATE_BYTE_USED_ITEM)], 0x44);
        assert_eq!(game.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)], 0);
        assert_eq!(menu.health_bar.flush, Some(crate::ui::status::Flush::Green));
        // The heal's draw is the same frame's health-bar update, so the flush
        // has already advanced its first step.
        assert_eq!(menu.health_bar.state, 3);
        while menu.submenu_closing {
            menu.tick(&mut game);
        }
        assert_eq!(menu.mode, MenuMode::Navigation);
    }

    #[test]
    fn heal_refusals_do_not_consume() {
        let mut game = new_game();
        game.add_item(0x44, 1);
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        let event = confirm_action(&mut menu, &mut game);
        assert_eq!(event, Some(MenuEvent::Message(MESSAGE_USE_REFUSED + 7)));
        assert_eq!(game.inventory.len(), 1, "the herb was not consumed");
        assert_eq!(game.last_used_item, None);
    }

    #[test]
    fn poison_cures_clear_the_status_bits() {
        let mut game = new_game();
        game.entities[0].health = 96;
        game.set_health_status(0x22);
        game.add_item(0x45, 1); // blue herb cures poison 0x02
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert_eq!(
            confirm_action(&mut menu, &mut game),
            Some(MenuEvent::Changed)
        );
        assert_eq!(game.health_status, 0x20);
        assert_eq!(
            game.state_bytes[usize::from(crate::game::STATE_BYTE_HEALTH_STATUS)],
            0x20
        );

        // Serum cures the 0x20 poison and clears the scenario-2 marker.
        game.set_health_status(0x22);
        game.apply_flag(1, crate::game::SCENARIO2_FLAG_YAWN_POISONED, 0);
        game.add_item(0x42, 1);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert_eq!(
            confirm_action(&mut menu, &mut game),
            Some(MenuEvent::Changed)
        );
        assert_eq!(game.health_status, 0x02);
        assert!(!game.flag_test(1, crate::game::SCENARIO2_FLAG_YAWN_POISONED, false));
    }

    #[test]
    fn keys_need_their_use_flag_and_are_consumed() {
        let mut game = new_game();
        game.add_item(0x33, 1); // sword key
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert_eq!(
            confirm_action(&mut menu, &mut game),
            Some(MenuEvent::Message(MESSAGE_USE_REFUSED + 5))
        );
        assert_eq!(game.inventory.len(), 1);

        game.set_item_use_flag(0x33, true);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert_eq!(
            confirm_action(&mut menu, &mut game),
            Some(MenuEvent::Close),
            "a key use closes the inventory"
        );
        assert!(game.inventory.is_empty());
        assert_eq!(game.last_used_item, Some(0x33));
    }

    #[test]
    fn the_red_book_needs_its_own_flag() {
        let mut game = new_game();
        game.add_item(0x3E, 1);
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert_eq!(
            confirm_action(&mut menu, &mut game),
            Some(MenuEvent::Message(MESSAGE_USE_REFUSED + 6))
        );
        game.set_item_use_flag(0x3E, true);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert_eq!(
            confirm_action(&mut menu, &mut game),
            Some(MenuEvent::Close),
            "a book use closes the inventory"
        );
        assert!(game.inventory.is_empty());
    }

    #[test]
    fn closes_inventory_follows_the_use_dispatch() {
        // Key, special, chemical and book slots set the close bit; heal,
        // ammo, weapons and always-usable items do not.
        assert!(closes_inventory(0x33)); // sword key
        assert!(closes_inventory(0x1D)); // crank (special)
        assert!(closes_inventory(0x15)); // UMB No.2 (chemical)
        assert!(closes_inventory(0x3E)); // red book
        assert!(!closes_inventory(0x44)); // green herb
        assert!(!closes_inventory(0x02)); // beretta
        assert!(!closes_inventory(0x0B)); // handgun ammo
    }

    #[test]
    fn equipping_a_weapon_toggles_the_state_byte() {
        let mut game = new_game();
        game.add_item(0x02, 15);
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert!(menu.equippable);
        assert_eq!(
            menu.handle_input(&mut game, MenuInput::Confirm),
            MenuEvent::Changed
        );
        assert_eq!(game.equipped, Some(0x02));
        assert_eq!(game.state_bytes[usize::from(STATE_BYTE_EQUIPPED)], 0x02);
        // The equip runs the submenu's close animation; a second confirm once
        // it is back on the inventory removes it again. The ammo is not
        // consumed by an equip.
        while menu.submenu_closing {
            menu.tick(&mut game);
        }
        assert_eq!(menu.mode, MenuMode::Navigation);
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert_eq!(
            menu.handle_input(&mut game, MenuInput::Confirm),
            MenuEvent::Changed
        );
        assert_eq!(game.equipped, None);
        assert_eq!(game.inventory[0].quantity, 15);
    }

    #[test]
    fn two_herbs_combine_through_the_move_action() {
        let mut game = new_game();
        game.add_item(0x44, 1); // green
        game.add_item(0x43, 1); // red
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        menu.handle_input(&mut game, MenuInput::Down);
        menu.handle_input(&mut game, MenuInput::Down);
        assert_eq!(
            menu.handle_input(&mut game, MenuInput::Confirm),
            MenuEvent::None
        );
        assert_eq!(menu.mode, MenuMode::Move);
        menu.handle_input(&mut game, MenuInput::Right);
        assert_eq!(menu.move_cursor, 10);
        assert_eq!(
            menu.handle_input(&mut game, MenuInput::Confirm),
            MenuEvent::Message(MESSAGE_HERB_MIX)
        );
        assert_eq!(game.inventory.len(), 1);
        assert_eq!(game.inventory[0].id, 0x46, "green+red mixed herb");
        assert_eq!(menu.mode, MenuMode::Navigation);
    }

    #[test]
    fn ammo_transfer_and_quantity_merge_go_through_the_combine_tables() {
        // Beretta + clip: effect 1 moves the clip's rounds into the gun.
        let mut game = new_game();
        game.add_item(0x02, 1);
        game.add_item(0x0B, 30);
        assert_eq!(
            game.combine_slots(0, 1),
            CombineResult::Applied {
                herb: false,
                chemical: false
            }
        );
        assert_eq!(game.inventory[0].id, 0x02);
        assert_eq!(game.inventory[0].quantity, 0x0F);
        // The gun's own round is part of the transfer: 30 + 1 - 15.
        assert_eq!(game.inventory[1].quantity, 30 + 1 - 0x0F);

        // Two ink-ribbon slots merge up to the 0xFA cap. They cannot be built
        // through `add_item` (that already merges stacks), so push both.
        let mut game = new_game();
        game.inventory.push(InventoryItem {
            id: 0x2F,
            quantity: 3,
        });
        game.inventory.push(InventoryItem {
            id: 0x2F,
            quantity: 3,
        });
        let result = game.combine_slots(0, 1);
        assert!(matches!(result, CombineResult::Applied { .. }));
        assert_eq!(game.inventory.len(), 1);
        assert_eq!(game.inventory[0].quantity, 3 + 3);
    }

    #[test]
    fn chemicals_need_the_drug_store() {
        let mut game = new_game();
        game.add_item(0x15, 1); // UMB No.2
        game.add_item(0x18, 1); // UMB No.7
        assert_eq!(game.combine_slots(0, 1), CombineResult::NeedsDrugStore);
        assert_eq!(game.inventory.len(), 2, "nothing changed");

        game.id = RoomId::parse("309").unwrap();
        assert!(game.is_drug_store());
        let result = game.combine_slots(0, 1);
        assert!(matches!(
            result,
            CombineResult::Applied { chemical: true, .. }
        ));
        assert!(game.flag_test(0, crate::game::SCENARIO_FLAG_CHEMICAL_COMBINE, false));
    }

    #[test]
    fn no_recipe_between_unrelated_items() {
        let mut game = new_game();
        game.add_item(0x44, 1);
        game.add_item(0x41, 1);
        assert_eq!(game.combine_slots(0, 1), CombineResult::NoRecipe);
        assert_eq!(game.inventory.len(), 2);
    }

    #[test]
    fn two_herbs_without_a_recipe_show_the_cannot_combine_message() {
        // Red + blue has no recipe, but both are herbs: the original's
        // distinct result 3, which shows `0xF6` rather than refusing silently.
        let mut game = new_game();
        game.add_item(0x43, 1); // red herb
        game.add_item(0x45, 1); // blue herb
        assert_eq!(game.combine_slots(0, 1), CombineResult::HerbNoRecipe);

        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        menu.handle_input(&mut game, MenuInput::Down);
        menu.handle_input(&mut game, MenuInput::Down);
        menu.handle_input(&mut game, MenuInput::Confirm);
        assert_eq!(menu.mode, MenuMode::Move);
        menu.handle_input(&mut game, MenuInput::Right);
        assert_eq!(
            menu.handle_input(&mut game, MenuInput::Confirm),
            MenuEvent::Message(MESSAGE_NO_RECIPE)
        );
        assert_eq!(game.inventory.len(), 2, "nothing was consumed");
    }

    #[test]
    fn the_screen_draws_slots_icons_cursor_and_portrait() {
        let mut game = new_game();
        game.state_bytes[usize::from(STATE_BYTE_CHARACTER)] = 0;
        game.add_item(0x44, 1); // green herb -> item_all row 64
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        let assets = assets();
        let text = Text::default();
        let mut framebuffer = Framebuffer::new();
        menu.draw(&mut framebuffer, &assets, &text, &game);

        // The top black strip is always painted.
        assert_eq!(pixel(&framebuffer, 4, 4), [0, 0, 0, 255]);
        // The cursor over slot 0 at (220, 56) draws over the icon.
        assert_eq!(pixel(&framebuffer, 240, 70), [255, 0, 255, 255]);
        // Slot 1 is empty and draws the blue plate.
        assert_eq!(pixel(&framebuffer, 280, 70), [0, 0, 255, 255]);
        // The portrait at (22, 146) comes from statface.
        assert_eq!(pixel(&framebuffer, 30, 150), [255, 255, 0, 255]);

        // Moving the cursor to the empty slot paints the slot cursor there.
        menu.handle_input(&mut game, MenuInput::Right);
        let mut framebuffer = Framebuffer::new();
        menu.draw(&mut framebuffer, &assets, &text, &game);
        assert_eq!(pixel(&framebuffer, 280, 70), [255, 0, 255, 255]);
    }

    #[test]
    fn the_item_name_line_can_be_suppressed_for_a_message() {
        use crate::pack::{Pack, PackWriter};

        // A language file with one real name for the green herb, so the name
        // draw has something to paint.
        let dir = std::env::temp_dir().join(format!("arklay-menu-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("game.akpak");
        let mut writer = PackWriter::new();
        writer
            .add("language/en-US.txt", b"ITEM_44_NAME = \"\\xAA\"\n".to_vec())
            .unwrap();
        writer.write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();
        let text = Text::load(&pack, Some("en-US"));

        let mut game = new_game();
        game.add_item(0x44, 1);
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        let mut assets = assets();
        assets.font = Font::new(solid_texture(
            768,
            256,
            &[([0, 0, 768, 256], 1, [1, 2, 3, 255])],
        ));
        let name_pixels = |show_name: bool| {
            let mut framebuffer = Framebuffer::new();
            menu.draw_with_name(&mut framebuffer, &assets, &text, &game, true, show_name);
            let mut count = 0;
            for y in 0..framebuffer.height {
                for x in 0..framebuffer.width {
                    if pixel(&framebuffer, x as usize, y as usize) == [1, 2, 3, 255] {
                        count += 1;
                    }
                }
            }
            count
        };
        assert!(
            name_pixels(true) > 0,
            "the name draws on the menu when the menu owns the line"
        );
        assert_eq!(
            name_pixels(false),
            0,
            "the name is suppressed while a message owns the line"
        );
    }

    #[test]
    fn menu_actions_are_visible_to_cmpb() {
        let mut game = new_game();
        game.entities[0].health = 20;
        game.add_item(0x44, 1);
        game.add_item(0x0B, 30);
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        assert!(game.compare_byte(6, 0, 0x44), "selected item byte");
        assert!(game.compare_byte(7, 0, 2), "two held slots");
        assert!(game.compare_byte(18, 0, 0));

        menu.handle_input(&mut game, MenuInput::Confirm);
        confirm_action(&mut menu, &mut game);
        assert!(game.compare_byte(18, 0, 0x44), "used item byte");
        assert!(game.compare_byte(7, 0, 1), "the green herb was consumed");
        assert!(game.compare_byte(6, 0, 0x0B), "cursor follows the pack");
    }

    #[test]
    fn quantities_draw_digits_and_the_infinity_glyph() {
        let assets = assets();
        let mut game = new_game();
        let mut framebuffer = Framebuffer::new();
        // Ammo draws right-aligned: a zero quantity shows one digit at
        // x + 0x0E + 16 in the green column.
        draw_item_quantity(
            &mut framebuffer,
            &assets,
            &game,
            InventoryItem {
                id: 0x0B,
                quantity: 0,
            },
            [10, 10],
        );
        assert_eq!(pixel(&framebuffer, 10 + 0x0E + 16, 30), [0, 0, 255, 255]);

        // DumDum rounds use the orange column.
        draw_item_quantity(
            &mut framebuffer,
            &assets,
            &game,
            InventoryItem {
                id: 0x0D,
                quantity: 0,
            },
            [10, 10],
        );
        assert_eq!(pixel(&framebuffer, 10 + 0x0E + 16, 30), [255, 128, 0, 255]);

        // Above the non-infinite maximum the quantity becomes the infinity
        // glyph at (x + 6, y + 20).
        draw_item_quantity(
            &mut framebuffer,
            &assets,
            &game,
            InventoryItem {
                id: 0x6F,
                quantity: 1,
            },
            [10, 10],
        );
        assert_eq!(pixel(&framebuffer, 16, 30), [255, 255, 255, 255]);

        // The rocket launcher is infinite only with scenario flag 0x7E.
        game.apply_flag(0, 0x7E, 0);
        let mut framebuffer = Framebuffer::new();
        draw_item_quantity(
            &mut framebuffer,
            &assets,
            &game,
            InventoryItem {
                id: 0x0A,
                quantity: 1,
            },
            [10, 10],
        );
        assert_eq!(pixel(&framebuffer, 16, 30), [255, 255, 255, 255]);
    }

    #[test]
    fn the_submenu_box_draws_at_its_screen_position() {
        let mut game = new_game();
        game.add_item(0x44, 1);
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.handle_input(&mut game, MenuInput::Confirm);
        let assets = assets();
        let text = Text::default();
        let mut framebuffer = Framebuffer::new();
        menu.draw(&mut framebuffer, &assets, &text, &game);
        // The action box lives at (0x90, 0x39) and samples status (0x30, ..),
        // which is black in the synthetic sheet; the surrounding pixel is the
        // black screen clear the background pass laid down.
        assert_eq!(pixel(&framebuffer, 0x90 + 5, 0x39 + 5), [0, 0, 0, 255]);
    }

    #[test]
    fn the_menu_background_is_opaque_over_the_frozen_frame() {
        let mut game = new_game();
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        let assets = assets();
        let text = Text::default();
        let mut framebuffer = Framebuffer::new();
        for pixel in framebuffer.rgba.as_chunks_mut::<4>().0.iter_mut() {
            *pixel = [99, 98, 97, 255];
        }
        menu.draw(&mut framebuffer, &assets, &text, &game);
        // The centre viewport is the original's pure-black screen clear; the
        // frozen gameplay frame must never bleed through.
        assert_eq!(pixel(&framebuffer, 100, 100), [0, 0, 0, 255]);
    }

    #[test]
    fn the_tab_highlight_draws_behind_the_tab_art() {
        let mut game = new_game();
        let mut menu = MainMenu::new(8);
        menu.open(&mut game);
        menu.cursor = 0;
        let tab = layout::tab_part(0);
        // A sheet with an opaque white tab and the solid red highlight bar;
        // the original's tab art is partly transparent so the bar shows only
        // where the glyph does not cover it.
        let mut assets = assets();
        assets.status = solid_texture(
            256,
            256,
            &[
                (
                    [
                        u32::from(tab.uv[0]),
                        u32::from(tab.uv[1]),
                        u32::from(tab.size[0].max(0) as u16),
                        u32::from(tab.size[1].max(0) as u16),
                    ],
                    1,
                    [255, 255, 255, 255],
                ),
                ([0, 0x50, 48, 16], 2, [255, 0, 0, 255]),
            ],
        );
        let text = Text::default();
        let mut framebuffer = Framebuffer::new();
        menu.draw(&mut framebuffer, &assets, &text, &game);
        let [x, y] = layout::cursor_position(0, menu.slots).unwrap();
        assert_eq!(tab.pos, [x, y], "the tab art sits on the highlight");
        assert_eq!(
            pixel(&framebuffer, x as usize + 4, y as usize + 4),
            [255, 255, 255, 255],
            "the tab art must stay on top of its highlight"
        );
    }
}
