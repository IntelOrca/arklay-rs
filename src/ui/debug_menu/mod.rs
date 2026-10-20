//! The port-only F1 debug menu.
//!
//! This is a development aid the retail game never shipped. F1 while playing
//! freezes the room and opens a tool list: ROOM SELECT, INVENTORY, PLAYER,
//! FLAGS, CHEATS, SAVE, ITEM BOX, CLEAR GAME, CONTROLS (the input rebind
//! screen) and DISPLAY (the presented frame rate). Up/down move the cursor,
//! confirm enters a tool, cancel backs out one level (tool, main list, closed)
//! and F1 closes the menu from anywhere. The menu is always available
//! (`--debug-menu` is accepted for compatibility only).
//!
//! The engine drives the menu like the pause menu: one state machine owning
//! the current tool and every tool's state, ticked over the frozen room and
//! drawn last. F1 closes it from anywhere and reopening returns to the tool
//! and cursors it was left on; the room browser also follows a room change
//! made while the menu was closed. Edits that only touch [`GameState`]
//! (inventory, flags, cheats, the pending player selection) happen inside
//! [`DebugMenu::handle_input`]; everything that needs the room-load path, the
//! save directory or the app is returned as a [`DebugMenuEvent`].
//!
//! The reference has an F1 menu of its own (`ref/src/game/DebugMenu.cpp`);
//! its flag editor's views and names are mirrored, while the room columns, the
//! icon picker and the cheats are port-designed.

mod controls;
mod flags;
mod inventory;
mod rooms;

pub use controls::{CAPTURE_TICKS, ControlsEditor, Note};

pub use flags::{FlagEditor, FlagGroup, FlagName, GROUPS, flag_name};
pub use inventory::{InventoryEditor, Picker};
pub use rooms::{RoomSelect, RoomVariants, room_entries, room_variants};

use crate::charset;
use crate::controls::Bindings;
use crate::ending;
use crate::font::{self, Font};
use crate::game::{self, GameState, InfiniteAmmo};
use crate::pack::Pack;
use crate::render::{Framebuffer, Tint};
use crate::state::RoomId;
use crate::text::Text;
use crate::ui::UiInput;
use crate::ui::main_menu::{self, MenuAssets};
use crate::video;

/// One text row's height in pixels (the font's glyph height).
const ROW_HEIGHT: i32 = font::GLYPH_HEIGHT;
/// The dark panel's rectangle.
const PANEL: [i32; 4] = [24, 16, 272, 208];
/// The panel's inner right edge.
const PANEL_RIGHT: i32 = PANEL[0] + PANEL[2] - 8;
/// The title's baseline.
const TITLE_Y: i32 = PANEL[1] + 8;
/// The first body row.
const BODY_Y: i32 = 44;
/// Left edge of a row's text; the cursor sits 16 px to its left.
const TEXT_LEFT: i32 = 52;
/// The first of the two key-hint lines.
const HINT_Y: i32 = 196;
/// Cursor colour (orange).
const CURSOR_COLOR: [u8; 4] = [255, 160, 0, 255];
/// Tint of the selected row's text (orange, matching the cursor).
const SELECTED_TINT: Tint = Tint::Rgb(255, 160, 0);

/// What one tick of the menu decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugMenuEvent {
    /// The input was consumed inside the menu.
    None,
    /// Close the menu and resume the frozen room.
    Close,
    /// Load `RoomId` and place the player.
    Jump(RoomId),
    /// Open the save screen in save mode (no ink ribbon needed).
    SaveGame,
    /// Open the save screen in load mode.
    LoadGame,
    /// Open the item box.
    ItemBox,
    /// Start ending `id`'s film chain.
    ClearGame(u8),
    /// The CONTROLS tool changed the bindings; the menu stays open.
    BindingsChanged,
    /// The DISPLAY tool picked this presented frame rate; the menu stays
    /// open.
    FrameRateChanged(u32),
}

/// The main list's tools, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// The stage/room browser.
    Rooms,
    /// The inventory editor.
    Inventory,
    /// Character and outfit selection.
    Player,
    /// The flag editor.
    Flags,
    /// The cheat toggles.
    Cheats,
    /// The save/load chooser.
    Save,
    /// Open the item box.
    ItemBox,
    /// The ending chooser.
    ClearGame,
    /// The input rebind screen.
    Controls,
    /// The display options (the presented frame rate).
    Display,
}

/// The main list, in order.
pub const TOOLS: [Tool; 10] = [
    Tool::Rooms,
    Tool::Inventory,
    Tool::Player,
    Tool::Flags,
    Tool::Cheats,
    Tool::Save,
    Tool::ItemBox,
    Tool::ClearGame,
    Tool::Controls,
    Tool::Display,
];

impl Tool {
    /// The tool's label key (also its title).
    fn label(self) -> &'static str {
        match self {
            Tool::Rooms => "UI_DEBUG_TOOL_ROOMS",
            Tool::Inventory => "UI_DEBUG_TOOL_INVENTORY",
            Tool::Player => "UI_DEBUG_TOOL_PLAYER",
            Tool::Flags => "UI_DEBUG_TOOL_FLAGS",
            Tool::Cheats => "UI_DEBUG_TOOL_CHEATS",
            Tool::Save => "UI_DEBUG_TOOL_SAVE",
            Tool::ItemBox => "UI_DEBUG_TOOL_ITEM_BOX",
            Tool::ClearGame => "UI_DEBUG_TOOL_CLEAR_GAME",
            Tool::Controls => "UI_DEBUG_TOOL_CONTROLS",
            Tool::Display => "UI_DEBUG_TOOL_DISPLAY",
        }
    }

    /// The two key-hint keys shown while the tool is open.
    fn hints(self) -> [&'static str; 2] {
        match self {
            Tool::Rooms => ["UI_DEBUG_HINT_ROOMS_1", "UI_DEBUG_HINT_ROOMS_2"],
            Tool::Inventory => ["UI_DEBUG_HINT_INVENTORY_1", "UI_DEBUG_HINT_INVENTORY_2"],
            Tool::Player => ["UI_DEBUG_HINT_PLAYER_1", "UI_DEBUG_HINT_PLAYER_2"],
            Tool::Flags => ["UI_DEBUG_HINT_FLAGS_1", "UI_DEBUG_HINT_FLAGS_2"],
            Tool::Cheats => ["UI_DEBUG_HINT_CHEATS_1", "UI_DEBUG_HINT_CHEATS_2"],
            Tool::Controls => ["UI_DEBUG_HINT_CONTROLS_1", "UI_DEBUG_HINT_CONTROLS_2"],
            Tool::Display => ["UI_DEBUG_HINT_DISPLAY_1", "UI_DEBUG_HINT_DISPLAY_2"],
            Tool::Save | Tool::ItemBox | Tool::ClearGame => {
                ["UI_DEBUG_HINT_SELECT_1", "UI_DEBUG_HINT_SELECT_2"]
            }
        }
    }
}

/// The PLAYER tool's character names, by character.
const CHARACTER_NAMES: [&str; 2] = ["UI_DEBUG_CHRIS", "UI_DEBUG_JILL"];
/// The PLAYER tool's outfit names, by [`PlayerChoice::costume`].
const COSTUME_NAMES: [&str; 3] = [
    "UI_DEBUG_COSTUME_NORMAL",
    "UI_DEBUG_COSTUME_ALT_1",
    "UI_DEBUG_COSTUME_ALT_2",
];
/// The CHEATS rows.
const CHEAT_NAMES: [&str; 3] = [
    "UI_DEBUG_CHEAT_INVINCIBLE",
    "UI_DEBUG_CHEAT_GHOST",
    "UI_DEBUG_CHEAT_AMMO",
];
/// The SAVE rows.
const SAVE_ROWS: [&str; 2] = ["UI_DEBUG_SAVE_GAME", "UI_DEBUG_LOAD_GAME"];
/// The CLEAR GAME route hints, by ending id minus one.
const ENDING_HINTS: [&str; 7] = [
    "UI_DEBUG_ENDING_1",
    "UI_DEBUG_ENDING_2",
    "UI_DEBUG_ENDING_3",
    "UI_DEBUG_ENDING_4",
    "UI_DEBUG_ENDING_5",
    "UI_DEBUG_ENDING_6",
    "UI_DEBUG_ENDING_7",
];

/// What a tool's tick decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Stay in the tool.
    Stay,
    /// Back out to the main list.
    Back,
    /// Hand an event to the engine.
    Event(DebugMenuEvent),
}

/// The ending the live state would play: [`ending::select_id`] over bank 1's
/// partner and second-survivor bits and the current character.
pub fn default_ending(game: &GameState) -> u8 {
    ending::select_id(
        game.flag_test(
            game::BANK_SCENARIO2,
            ending::SCENARIO2_FLAG_PARTNER_ALIVE,
            false,
        ),
        game.flag_test(
            game::BANK_SCENARIO2,
            ending::SCENARIO2_FLAG_SECOND_SURVIVOR,
            false,
        ),
        game.player_choice().character,
    )
}

/// The character the room select jumps as: the pending selection, else the
/// current one.
pub fn jump_character(game: &GameState) -> u8 {
    game.debug
        .pending_player
        .unwrap_or_else(|| game.player_choice())
        .character
}

/// The menu's live state: the open tool and every tool's own state.
pub struct DebugMenu {
    tool: Option<Tool>,
    main_cursor: usize,
    rooms: RoomSelect,
    inventory: InventoryEditor,
    player_row: usize,
    flags: FlagEditor,
    cheat_row: usize,
    save_row: usize,
    ending_row: usize,
    controls: ControlsEditor,
    /// The presented frame rate the DISPLAY tool shows and steps.
    frame_rate: u32,
}

impl DebugMenu {
    /// Open the main list with every tool seeded from the live state and the
    /// window's `bindings`.
    pub fn open(pack: &Pack, current: RoomId, game: &GameState, bindings: &Bindings) -> Self {
        Self {
            tool: None,
            main_cursor: 0,
            rooms: RoomSelect::new(pack, current),
            inventory: InventoryEditor::default(),
            player_row: 0,
            flags: FlagEditor::default(),
            cheat_row: 0,
            save_row: 0,
            ending_row: usize::from(default_ending(game).clamp(1, 7) - 1),
            controls: ControlsEditor::new(bindings),
            frame_rate: video::TICK_RATE,
        }
    }

    /// Show the window's current presented frame rate in the DISPLAY tool.
    pub fn set_frame_rate(&mut self, frame_rate: u32) {
        self.frame_rate = frame_rate;
    }

    /// The DISPLAY tool's frame rate.
    pub fn frame_rate(&self) -> u32 {
        self.frame_rate
    }

    /// Reattach the menu to the live session after it was closed: the open
    /// tool and every cursor are kept, while the room browser follows
    /// `current` if the room changed, the inventory cursor is clamped to the
    /// live capacity and the CONTROLS editor picks up the window's
    /// `bindings`.
    pub fn reopen(&mut self, current: RoomId, game: &GameState, bindings: &Bindings) {
        self.rooms.refresh(current);
        self.inventory.refresh(game);
        self.controls.refresh(bindings);
    }

    /// The CONTROLS tool's state (its bindings are the edited map).
    pub fn controls(&self) -> &ControlsEditor {
        &self.controls
    }

    /// The open tool (`None` on the main list).
    pub fn tool(&self) -> Option<Tool> {
        self.tool
    }

    /// The main list's cursor.
    pub fn main_cursor(&self) -> usize {
        self.main_cursor
    }

    /// The room browser's state.
    pub fn rooms(&self) -> &RoomSelect {
        &self.rooms
    }

    /// The inventory editor's state.
    pub fn inventory(&self) -> &InventoryEditor {
        &self.inventory
    }

    /// The flag editor's state.
    pub fn flags(&self) -> &FlagEditor {
        &self.flags
    }

    /// The CLEAR GAME cursor's ending id.
    pub fn ending(&self) -> u8 {
        self.ending_row as u8 + 1
    }

    /// One tick of edge-triggered menu input against the live state.
    pub fn handle_input(&mut self, input: UiInput, game: &mut GameState) -> DebugMenuEvent {
        if input.debug_menu {
            return DebugMenuEvent::Close;
        }
        let Some(tool) = self.tool else {
            return self.handle_main(input);
        };
        let step = match tool {
            Tool::Rooms => self.rooms.handle_input(input, jump_character(game)),
            Tool::Inventory => self.inventory.handle_input(input, game),
            Tool::Player => self.handle_player(input, game),
            Tool::Flags => self.flags.handle_input(input, game),
            Tool::Cheats => self.handle_cheats(input, game),
            Tool::Save => self.handle_save(input),
            Tool::ClearGame => self.handle_clear(input),
            Tool::ItemBox => Step::Back,
            Tool::Controls => match self.controls.handle_input(input) {
                (step, false) => step,
                (_, true) => Step::Event(DebugMenuEvent::BindingsChanged),
            },
            Tool::Display => self.handle_display(input),
        };
        match step {
            Step::Stay => DebugMenuEvent::None,
            Step::Back => {
                self.tool = None;
                DebugMenuEvent::None
            }
            Step::Event(event) => event,
        }
    }

    /// The main list: move, enter a tool, or close.
    fn handle_main(&mut self, input: UiInput) -> DebugMenuEvent {
        let count = TOOLS.len();
        if input.cancel {
            return DebugMenuEvent::Close;
        }
        if input.up {
            self.main_cursor = (self.main_cursor + count - 1) % count;
        } else if input.down {
            self.main_cursor = (self.main_cursor + 1) % count;
        } else if input.confirm {
            match TOOLS[self.main_cursor] {
                Tool::ItemBox => return DebugMenuEvent::ItemBox,
                tool => self.tool = Some(tool),
            }
        }
        DebugMenuEvent::None
    }

    /// PLAYER: up/down pick the field, left/right/confirm change it. The
    /// change stays pending until the next room load; picking the current
    /// character and outfit again drops it.
    fn handle_player(&mut self, input: UiInput, game: &mut GameState) -> Step {
        if input.cancel {
            return Step::Back;
        }
        if input.up || input.down {
            self.player_row ^= 1;
            return Step::Stay;
        }
        let delta: i32 = if input.left {
            -1
        } else if input.right || input.confirm {
            1
        } else {
            return Step::Stay;
        };
        let current = game.player_choice();
        let mut choice = game.debug.pending_player.unwrap_or(current);
        if self.player_row == 0 {
            choice.character ^= 1;
        } else {
            choice.costume = (i32::from(choice.costume) + delta).rem_euclid(3) as u8;
        }
        game.debug.pending_player = (choice != current).then_some(choice);
        Step::Stay
    }

    /// DISPLAY: left/right step the frame rate through
    /// [`video::FRAME_RATE_PRESETS`] (confirm steps up); each change applies
    /// at once and is saved to `arklay.toml` by the engine.
    fn handle_display(&mut self, input: UiInput) -> Step {
        if input.cancel {
            return Step::Back;
        }
        let delta = if input.left {
            -1
        } else if input.right || input.confirm {
            1
        } else {
            return Step::Stay;
        };
        self.frame_rate = video::step_frame_rate(self.frame_rate, delta);
        Step::Event(DebugMenuEvent::FrameRateChanged(self.frame_rate))
    }

    /// CHEATS: up/down pick the row, left/right/confirm toggle a boolean row
    /// or step INFINITE AMMO through off/reload/infinite. Turning INFINITE on
    /// fills the equipped clip at once.
    fn handle_cheats(&mut self, input: UiInput, game: &mut GameState) -> Step {
        if input.cancel {
            return Step::Back;
        }
        let count = CHEAT_NAMES.len();
        if input.up {
            self.cheat_row = (self.cheat_row + count - 1) % count;
        } else if input.down {
            self.cheat_row = (self.cheat_row + 1) % count;
        } else if input.left || input.right || input.confirm {
            let delta = if input.left { -1 } else { 1 };
            match self.cheat_row {
                0 => game.debug.invincible = !game.debug.invincible,
                1 => game.debug.ghost = !game.debug.ghost,
                2 => {
                    game.debug.infinite_ammo = game.debug.infinite_ammo.step(delta);
                    if game.debug.infinite_ammo == InfiniteAmmo::Infinite {
                        crate::weapons::refill_clip(game);
                    }
                }
                _ => {}
            }
        }
        Step::Stay
    }

    /// SAVE: pick SAVE GAME or LOAD GAME.
    fn handle_save(&mut self, input: UiInput) -> Step {
        if input.cancel {
            return Step::Back;
        }
        if input.up || input.down {
            self.save_row ^= 1;
        } else if input.confirm {
            return Step::Event(if self.save_row == 0 {
                DebugMenuEvent::SaveGame
            } else {
                DebugMenuEvent::LoadGame
            });
        }
        Step::Stay
    }

    /// CLEAR GAME: pick an ending id and start it.
    fn handle_clear(&mut self, input: UiInput) -> Step {
        let count = ENDING_HINTS.len();
        if input.cancel {
            return Step::Back;
        }
        if input.up {
            self.ending_row = (self.ending_row + count - 1) % count;
        } else if input.down {
            self.ending_row = (self.ending_row + 1) % count;
        } else if input.confirm {
            return Step::Event(DebugMenuEvent::ClearGame(self.ending()));
        }
        Step::Stay
    }

    /// Draw the dark panel, the open tool (or the main list) and its key
    /// hints. Without a font the panel still dims the frozen frame.
    pub fn draw(
        &mut self,
        framebuffer: &mut Framebuffer,
        font: Option<&Font>,
        text: &Text,
        assets: Option<&MenuAssets>,
        game: &GameState,
    ) {
        framebuffer.blend_black_rect(PANEL, 200);
        let Some(font) = font else {
            return;
        };
        let mut pen = Pen {
            framebuffer,
            font,
            text,
            assets,
            tight: text.uses_tight_metrics(),
        };
        let (title, hints) = match self.tool {
            None => (
                "UI_DEBUG_TITLE",
                ["UI_DEBUG_HINT_MAIN_1", "UI_DEBUG_HINT_MAIN_2"],
            ),
            Some(tool) => (tool.label(), tool.hints()),
        };
        // The item picker draws full screen with its own title and hint.
        if self.tool == Some(Tool::Inventory) && self.inventory.picker().is_some() {
            self.inventory.draw(&mut pen, game);
            return;
        }
        pen.bytes(PANEL[0] + 12, TITLE_Y, Tint::White, text.ui(title));
        match self.tool {
            None => self.draw_main(&mut pen),
            Some(Tool::Rooms) => self.rooms.draw(&mut pen),
            Some(Tool::Inventory) => self.inventory.draw(&mut pen, game),
            Some(Tool::Player) => self.draw_player(&mut pen, game),
            Some(Tool::Flags) => self.flags.draw(&mut pen, game),
            Some(Tool::Cheats) => self.draw_cheats(&mut pen, game),
            Some(Tool::Save) => draw_list(&mut pen, &SAVE_ROWS, self.save_row),
            Some(Tool::ClearGame) => self.draw_clear(&mut pen, game),
            Some(Tool::Controls) => self.controls.draw(&mut pen),
            Some(Tool::Display) => self.draw_display(&mut pen),
            Some(Tool::ItemBox) => {}
        }
        for (line, key) in hints.iter().enumerate() {
            let hint = pen.text.ui(key);
            let x = PANEL[0] + 12;
            pen.fit(
                x,
                HINT_Y + line as i32 * ROW_HEIGHT,
                Tint::Grey,
                hint,
                PANEL_RIGHT - x,
            );
        }
    }

    fn draw_main(&self, pen: &mut Pen) {
        let keys: Vec<&str> = TOOLS.iter().map(|tool| tool.label()).collect();
        draw_list(pen, &keys, self.main_cursor);
    }

    fn draw_player(&self, pen: &mut Pen, game: &GameState) {
        let current = game.player_choice();
        let choice = game.debug.pending_player.unwrap_or(current);
        let rows = [
            (
                "UI_DEBUG_PLAYER_CHARACTER",
                CHARACTER_NAMES[usize::from(choice.character & 1)],
            ),
            (
                "UI_DEBUG_PLAYER_COSTUME",
                COSTUME_NAMES[usize::from(choice.costume.min(2))],
            ),
        ];
        let value_x = TEXT_LEFT + 12 * pen.font.metrics.glyph_w;
        for (index, (label, value)) in rows.iter().enumerate() {
            let y = BODY_Y + index as i32 * ROW_HEIGHT;
            let mut tint = Tint::White;
            if index == self.player_row {
                pen.cursor(TEXT_LEFT - 16, y);
                tint = SELECTED_TINT;
            }
            pen.bytes(TEXT_LEFT, y, tint, pen.text.ui(label));
            pen.bytes(value_x, y, Tint::Yellow, pen.text.ui(value));
        }
        let note = if game.debug.pending_player.is_some() {
            "UI_DEBUG_PLAYER_PENDING"
        } else {
            "UI_DEBUG_PLAYER_CURRENT"
        };
        let note = pen.text.ui(note);
        pen.fit(
            TEXT_LEFT,
            BODY_Y + 3 * ROW_HEIGHT,
            Tint::Grey,
            note,
            PANEL_RIGHT - TEXT_LEFT,
        );
    }

    fn draw_display(&self, pen: &mut Pen) {
        let value_x = TEXT_LEFT + 12 * pen.font.metrics.glyph_w;
        pen.cursor(TEXT_LEFT - 16, BODY_Y);
        pen.bytes(
            TEXT_LEFT,
            BODY_Y,
            SELECTED_TINT,
            pen.text.ui("UI_DEBUG_DISPLAY_FRAME_RATE"),
        );
        pen.ascii(
            value_x,
            BODY_Y,
            Tint::Yellow,
            &format!("{}", self.frame_rate),
        );
        let note = if self.frame_rate > video::TICK_RATE {
            "UI_DEBUG_DISPLAY_TWEENED"
        } else {
            "UI_DEBUG_DISPLAY_NATIVE"
        };
        let note = pen.text.ui(note);
        pen.fit(
            TEXT_LEFT,
            BODY_Y + 2 * ROW_HEIGHT,
            Tint::Grey,
            note,
            PANEL_RIGHT - TEXT_LEFT,
        );
    }

    fn draw_cheats(&self, pen: &mut Pen, game: &GameState) {
        let on_off = |on: bool| {
            if on {
                ("UI_DEBUG_ON", Tint::Green)
            } else {
                ("UI_DEBUG_OFF", Tint::Grey)
            }
        };
        let ammo = match game.debug.infinite_ammo {
            InfiniteAmmo::Off => ("UI_DEBUG_OFF", Tint::Grey),
            InfiniteAmmo::Reload => ("UI_DEBUG_AMMO_RELOAD", Tint::Yellow),
            InfiniteAmmo::Infinite => ("UI_DEBUG_AMMO_INFINITE", Tint::Green),
        };
        let states = [
            on_off(game.debug.invincible),
            on_off(game.debug.ghost),
            ammo,
        ];
        for (index, (label, (value, tint))) in CHEAT_NAMES.iter().zip(states).enumerate() {
            let y = BODY_Y + index as i32 * ROW_HEIGHT;
            let mut label_tint = Tint::White;
            if index == self.cheat_row {
                pen.cursor(TEXT_LEFT - 16, y);
                label_tint = SELECTED_TINT;
            }
            pen.bytes(TEXT_LEFT, y, label_tint, pen.text.ui(label));
            // Place each value after its own label: the tri-state values are
            // much wider than ON/OFF, and a fixed column overflows the panel
            // with the wide Japanese glyphs.
            let value_x = TEXT_LEFT + (label.len() as i32 + 1) * pen.font.metrics.glyph_w;
            pen.fit(value_x, y, tint, pen.text.ui(value), PANEL_RIGHT - value_x);
        }
    }

    fn draw_clear(&self, pen: &mut Pen, game: &GameState) {
        let default = default_ending(game);
        for (index, key) in ENDING_HINTS.iter().enumerate() {
            let y = BODY_Y + index as i32 * ROW_HEIGHT;
            let selected = index == self.ending_row;
            if selected {
                pen.cursor(TEXT_LEFT - 16, y);
            }
            let id = index as u8 + 1;
            let tint = if selected {
                SELECTED_TINT
            } else if id == default {
                Tint::Green
            } else {
                Tint::White
            };
            pen.ascii(TEXT_LEFT, y, tint, &format!("{id}"));
            let x = TEXT_LEFT + 2 * pen.font.metrics.glyph_w;
            pen.fit(x, y, tint, pen.text.ui(key), PANEL_RIGHT - x);
        }
    }
}

/// Draw a plain list of language keys with the cursor on `cursor`.
fn draw_list(pen: &mut Pen, keys: &[&str], cursor: usize) {
    for (index, key) in keys.iter().enumerate() {
        let y = BODY_Y + index as i32 * ROW_HEIGHT;
        let mut tint = Tint::White;
        if index == cursor {
            pen.cursor(TEXT_LEFT - 16, y);
            tint = SELECTED_TINT;
        }
        pen.bytes(TEXT_LEFT, y, tint, pen.text.ui(key));
    }
}

/// The drawing context the tools share.
struct Pen<'a> {
    framebuffer: &'a mut Framebuffer,
    font: &'a Font,
    text: &'a Text,
    assets: Option<&'a MenuAssets>,
    tight: bool,
}

impl Pen<'_> {
    /// Draw an encoded stream.
    fn bytes(&mut self, x: i32, y: i32, tint: Tint, bytes: &[u8]) {
        self.font
            .draw_text(self.framebuffer, x, y, tint, 2, bytes, self.tight);
    }

    /// Draw an encoded stream clipped to `max_width` pixels.
    fn fit(&mut self, x: i32, y: i32, tint: Tint, bytes: &[u8], max_width: i32) {
        let clipped = font::fit_pixels(bytes, max_width, self.font, self.tight);
        self.bytes(x, y, tint, clipped);
    }

    /// Draw an ASCII identifier or number.
    fn ascii(&mut self, x: i32, y: i32, tint: Tint, text: &str) {
        let mut bytes = Vec::new();
        charset::encode(text, self.text.script(), &mut bytes);
        self.bytes(x, y, tint, &bytes);
    }

    /// [`Pen::ascii`] clipped to `max_width` pixels.
    fn ascii_fit(&mut self, x: i32, y: i32, tint: Tint, text: &str, max_width: i32) {
        let mut bytes = Vec::new();
        charset::encode(text, self.text.script(), &mut bytes);
        self.fit(x, y, tint, &bytes, max_width);
    }

    /// Draw the list cursor: an orange right-pointing arrow (the font sheet
    /// only has a down-pointing one).
    fn cursor(&mut self, x: i32, y: i32) {
        for column in 0..6 {
            let height = 11 - column * 2;
            self.framebuffer
                .fill_rect([x + 4 + column, y + 1 + column, 1, height], CURSOR_COLOR);
        }
    }

    /// Draw `item`'s 40x30 icon at `[x, y]` (nothing without the menu art).
    fn icon(&mut self, item: u8, [x, y]: [i32; 2]) {
        if let Some(assets) = self.assets {
            main_menu::draw_item_icon(self.framebuffer, assets, item, [x as i16, y as i16]);
        }
    }

    /// Draw `item`'s real name, or its hex id when it has none.
    fn item_name(&mut self, x: i32, y: i32, tint: Tint, item: u8) {
        self.item_name_fit(x, y, tint, item, PANEL_RIGHT - x);
    }

    /// [`Pen::item_name`] clipped to `max_width` pixels.
    fn item_name_fit(&mut self, x: i32, y: i32, tint: Tint, item: u8, max_width: i32) {
        match self.text.item_name(u16::from(item)) {
            Some(name) => self.fit(x, y, tint, name, max_width),
            None => self.ascii_fit(x, y, tint, &format!("ITEM {item:02X}"), max_width),
        }
    }

    /// A one-pixel yellow frame around `rect`.
    fn outline(&mut self, [x, y, width, height]: [i32; 4]) {
        let color = [255, 224, 64, 255];
        self.framebuffer.fill_rect([x, y, width, 1], color);
        self.framebuffer
            .fill_rect([x, y + height - 1, width, 1], color);
        self.framebuffer.fill_rect([x, y, 1, height], color);
        self.framebuffer
            .fill_rect([x + width - 1, y, 1, height], color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::PlayerChoice;
    use crate::game::{MSF2_COSTUME_VARIANT, STATE_BYTE_CHARACTER};
    use crate::pack::PackWriter;

    fn pack_with(paths: &[&str]) -> Pack {
        let mut writer = PackWriter::new();
        for path in paths {
            writer.add(path, vec![0u8; 4]).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    fn press(f: impl FnOnce(&mut UiInput)) -> UiInput {
        let mut input = UiInput::default();
        f(&mut input);
        input
    }

    fn up() -> UiInput {
        press(|i| i.up = true)
    }
    fn down() -> UiInput {
        press(|i| i.down = true)
    }
    fn left() -> UiInput {
        press(|i| i.left = true)
    }
    fn right() -> UiInput {
        press(|i| i.right = true)
    }
    fn confirm() -> UiInput {
        press(|i| i.confirm = true)
    }
    fn cancel() -> UiInput {
        press(|i| i.cancel = true)
    }
    fn f1() -> UiInput {
        press(|i| i.debug_menu = true)
    }

    fn menu(game: &GameState) -> DebugMenu {
        let pack = pack_with(&["room/1000.rdt", "room/1001.rdt", "room/1010.rdt"]);
        DebugMenu::open(
            &pack,
            RoomId::parse("1000").unwrap(),
            game,
            &Bindings::default(),
        )
    }

    /// Walk the main list to `tool` and enter it.
    fn enter(menu: &mut DebugMenu, game: &mut GameState, tool: Tool) -> DebugMenuEvent {
        while TOOLS[menu.main_cursor()] != tool {
            menu.handle_input(down(), game);
        }
        menu.handle_input(confirm(), game)
    }

    #[test]
    fn the_main_list_wraps_enters_and_backs_out() {
        let mut game = GameState::default();
        let mut menu = menu(&game);
        assert_eq!(menu.tool(), None);
        menu.handle_input(up(), &mut game);
        assert_eq!(TOOLS[menu.main_cursor()], Tool::Display, "up wraps");
        menu.handle_input(down(), &mut game);
        assert_eq!(menu.main_cursor(), 0);
        for tool in TOOLS {
            if tool == Tool::ItemBox {
                assert_eq!(enter(&mut menu, &mut game, tool), DebugMenuEvent::ItemBox);
                assert_eq!(menu.tool(), None, "ITEM BOX has no sub-screen");
                continue;
            }
            assert_eq!(enter(&mut menu, &mut game, tool), DebugMenuEvent::None);
            assert_eq!(menu.tool(), Some(tool));
            // Cancel backs out one level, to the main list on the same entry.
            assert_eq!(menu.handle_input(cancel(), &mut game), DebugMenuEvent::None);
            assert_eq!(menu.tool(), None);
            assert_eq!(TOOLS[menu.main_cursor()], tool);
        }
        assert_eq!(
            menu.handle_input(cancel(), &mut game),
            DebugMenuEvent::Close
        );
    }

    #[test]
    fn display_steps_the_frame_rate_and_stays_open() {
        let mut game = GameState::default();
        let mut menu = menu(&game);
        menu.set_frame_rate(60);
        assert_eq!(
            enter(&mut menu, &mut game, Tool::Display),
            DebugMenuEvent::None
        );
        assert_eq!(
            menu.handle_input(right(), &mut game),
            DebugMenuEvent::FrameRateChanged(120)
        );
        assert_eq!(
            menu.handle_input(confirm(), &mut game),
            DebugMenuEvent::FrameRateChanged(144)
        );
        assert_eq!(
            menu.handle_input(left(), &mut game),
            DebugMenuEvent::FrameRateChanged(120)
        );
        assert_eq!(menu.handle_input(up(), &mut game), DebugMenuEvent::None);
        assert_eq!(menu.frame_rate(), 120);
        assert_eq!(menu.tool(), Some(Tool::Display));
        assert_eq!(menu.handle_input(cancel(), &mut game), DebugMenuEvent::None);
        assert_eq!(menu.tool(), None);
    }

    #[test]
    fn debug_edits_survive_a_save_round_trip_and_cheats_do_not() {
        let mut game = GameState::default();
        game.id = RoomId::parse("1000").unwrap();
        let mut menu = menu(&game);
        // INVENTORY: pick item 0x01 into the first slot.
        enter(&mut menu, &mut game, Tool::Inventory);
        menu.handle_input(confirm(), &mut game);
        menu.handle_input(right(), &mut game);
        menu.handle_input(confirm(), &mut game);
        menu.handle_input(cancel(), &mut game);
        // FLAGS: flip the first scenario flag.
        enter(&mut menu, &mut game, Tool::Flags);
        let (group, sel) = menu.flags().selected();
        menu.handle_input(confirm(), &mut game);
        menu.handle_input(cancel(), &mut game);
        // CHEATS: INVINCIBLE on.
        enter(&mut menu, &mut game, Tool::Cheats);
        menu.handle_input(confirm(), &mut game);
        assert!(game.debug.invincible);

        let bytes = crate::save::SaveFile::from_state(&game).to_bytes();
        let mut loaded = GameState::default();
        crate::save::SaveFile::from_bytes(&bytes)
            .unwrap()
            .apply_to(&mut loaded);
        assert_eq!(loaded.inventory[0].id, 0x01);
        assert!(loaded.flag_test(group.bank, sel, false));
        assert!(!loaded.debug.invincible, "cheats are never saved");
        assert_eq!(
            loaded.debug.infinite_ammo,
            InfiniteAmmo::Off,
            "the ammo cheat is never saved"
        );
    }

    #[test]
    fn f1_closes_from_anywhere() {
        let mut game = GameState::default();
        let mut menu = menu(&game);
        assert_eq!(menu.handle_input(f1(), &mut game), DebugMenuEvent::Close);
        enter(&mut menu, &mut game, Tool::Inventory);
        menu.handle_input(confirm(), &mut game);
        assert!(menu.inventory().picker().is_some());
        assert_eq!(menu.handle_input(f1(), &mut game), DebugMenuEvent::Close);
    }

    #[test]
    fn reopening_keeps_the_tool_and_moves_the_room_cursor_with_the_session() {
        let mut game = GameState::default();
        let mut menu = menu(&game);
        enter(&mut menu, &mut game, Tool::Rooms);
        assert_eq!(
            menu.rooms().selected().map(|room| (room.stage, room.room)),
            Some((1, 0))
        );
        menu.handle_input(down(), &mut game);
        assert_eq!(
            menu.rooms().selected().map(|room| (room.stage, room.room)),
            Some((1, 1))
        );

        // Staying in the same room keeps the cursor where the player left it.
        menu.reopen(RoomId::parse("1000").unwrap(), &game, &Bindings::default());
        assert_eq!(menu.tool(), Some(Tool::Rooms), "the tool is remembered");
        assert_eq!(
            menu.rooms().selected().map(|room| (room.stage, room.room)),
            Some((1, 1))
        );

        // A room change while the menu was closed moves the cursor onto it.
        menu.handle_input(up(), &mut game);
        menu.reopen(RoomId::parse("1010").unwrap(), &game, &Bindings::default());
        assert_eq!(menu.tool(), Some(Tool::Rooms));
        assert_eq!(
            menu.rooms().selected().map(|room| (room.stage, room.room)),
            Some((1, 1))
        );
    }

    #[test]
    fn room_select_jumps_in_the_pending_characters_variant() {
        let mut game = GameState::default();
        let mut menu = menu(&game);
        enter(&mut menu, &mut game, Tool::Rooms);
        assert_eq!(
            menu.handle_input(confirm(), &mut game),
            DebugMenuEvent::Jump(RoomId::parse("1000").unwrap())
        );
        game.debug.pending_player = Some(PlayerChoice {
            character: 1,
            costume: 0,
        });
        assert_eq!(
            menu.handle_input(confirm(), &mut game),
            DebugMenuEvent::Jump(RoomId::parse("1001").unwrap())
        );
    }

    #[test]
    fn player_choices_stay_pending_and_drop_when_reverted() {
        let mut game = GameState::default();
        let mut menu = menu(&game);
        enter(&mut menu, &mut game, Tool::Player);
        menu.handle_input(right(), &mut game);
        assert_eq!(
            game.debug.pending_player,
            Some(PlayerChoice {
                character: 1,
                costume: 0
            })
        );
        assert_eq!(
            game.state_bytes[usize::from(STATE_BYTE_CHARACTER)],
            0,
            "not applied yet"
        );
        menu.handle_input(down(), &mut game);
        menu.handle_input(left(), &mut game);
        assert_eq!(
            game.debug.pending_player.map(|c| c.costume),
            Some(2),
            "wraps to ALT 2"
        );
        menu.handle_input(confirm(), &mut game);
        assert_eq!(game.debug.pending_player.map(|c| c.costume), Some(0));
        menu.handle_input(up(), &mut game);
        menu.handle_input(confirm(), &mut game);
        assert_eq!(
            game.debug.pending_player, None,
            "back to the current player"
        );
    }

    #[test]
    fn an_outfit_only_choice_leaves_the_character_bytes_alone() {
        let mut game = GameState::default();
        game.id = RoomId::parse("1000").unwrap();
        game.max_health = 120;
        game.entities[0].id = 0x40;
        game.apply_player_choice(PlayerChoice {
            character: 0,
            costume: 1,
        });
        assert!(game.flag_test(5, MSF2_COSTUME_VARIANT, false));
        assert_eq!(game.costume_variant, 0);
        assert_eq!(game.max_health, 120, "the same character keeps its maximum");
        assert_eq!(game.entities[0].id, 0x40, "and its entity id");
    }

    #[test]
    fn applying_a_player_choice_switches_character_and_outfit() {
        let mut game = GameState::default();
        game.id = RoomId::parse("1001").unwrap();
        game.apply_player_choice(PlayerChoice {
            character: 1,
            costume: 2,
        });
        for item in 1..=8 {
            game.inventory.push(game::InventoryItem {
                id: item,
                quantity: 1,
            });
        }
        game.set_equipped(Some(8));
        game.entities[0].health = 96;
        assert!(game.flag_test(5, game::MSF_CHAR_VARIANT, false));
        assert!(game.flag_test(5, MSF2_COSTUME_VARIANT, false));
        assert_eq!(game.costume_variant, 1);
        assert_eq!(game.max_health, 96);
        assert_eq!(
            game.player_choice(),
            PlayerChoice {
                character: 1,
                costume: 2
            }
        );
        // Jill to Chris in the normal outfit: six slots, more health, no
        // outfit bit; the dropped slot's equipped weapon goes with it.
        game.id = RoomId::parse("1000").unwrap();
        game.apply_player_choice(PlayerChoice::default());
        assert!(!game.flag_test(5, game::MSF_CHAR_VARIANT, false));
        assert!(!game.flag_test(5, MSF2_COSTUME_VARIANT, false));
        assert_eq!(game.max_health, 140);
        assert_eq!(
            game.entities[0].health, 96,
            "health is clamped, not refilled"
        );
        assert_eq!(game.inventory.len(), 6);
        assert_eq!(game.equipped, None);
        assert_eq!(game.player_choice(), PlayerChoice::default());
    }

    #[test]
    fn cheats_toggle() {
        let mut game = GameState::default();
        assert!(
            !game.debug.invincible && !game.debug.ghost,
            "off in a fresh session"
        );
        let mut menu = menu(&game);
        enter(&mut menu, &mut game, Tool::Cheats);
        menu.handle_input(confirm(), &mut game);
        assert!(game.debug.invincible);
        menu.handle_input(down(), &mut game);
        menu.handle_input(right(), &mut game);
        assert!(game.debug.ghost);
        menu.handle_input(left(), &mut game);
        assert!(!game.debug.ghost);
        assert!(game.debug.invincible);
    }

    #[test]
    fn infinite_ammo_cycles_and_fills_the_clip() {
        let mut game = GameState::default();
        game.inventory
            .push(game::InventoryItem { id: 2, quantity: 4 });
        game.set_equipped(Some(2));
        let mut menu = menu(&game);
        enter(&mut menu, &mut game, Tool::Cheats);
        menu.handle_input(down(), &mut game);
        menu.handle_input(down(), &mut game);
        assert_eq!(game.debug.infinite_ammo, InfiniteAmmo::Off);
        menu.handle_input(confirm(), &mut game);
        assert_eq!(game.debug.infinite_ammo, InfiniteAmmo::Reload);
        assert_eq!(game.inventory[0].quantity, 4, "reload does not prefill");
        menu.handle_input(confirm(), &mut game);
        assert_eq!(game.debug.infinite_ammo, InfiniteAmmo::Infinite);
        assert_eq!(game.inventory[0].quantity, 15, "infinite fills at once");
        menu.handle_input(right(), &mut game);
        assert_eq!(game.debug.infinite_ammo, InfiniteAmmo::Off, "wraps around");
        game.inventory[0].quantity = 2;
        menu.handle_input(left(), &mut game);
        assert_eq!(
            game.debug.infinite_ammo,
            InfiniteAmmo::Infinite,
            "left steps back"
        );
        assert_eq!(game.inventory[0].quantity, 15, "entering infinite refills");
    }

    #[test]
    fn save_rows_raise_save_and_load() {
        let mut game = GameState::default();
        let mut menu = menu(&game);
        enter(&mut menu, &mut game, Tool::Save);
        assert_eq!(
            menu.handle_input(confirm(), &mut game),
            DebugMenuEvent::SaveGame
        );
        menu.handle_input(down(), &mut game);
        assert_eq!(
            menu.handle_input(confirm(), &mut game),
            DebugMenuEvent::LoadGame
        );
    }

    #[test]
    fn clear_game_defaults_to_the_live_ending() {
        let mut game = GameState::default();
        // Chris with the partner alive: ending 1.
        game.apply_flag(
            game::BANK_SCENARIO2,
            ending::SCENARIO2_FLAG_PARTNER_ALIVE,
            0,
        );
        let mut menu = menu(&game);
        assert_eq!(menu.ending(), 1);
        // Jill with neither survivor: ending 5.
        let mut jill = GameState::default();
        jill.state_bytes[usize::from(STATE_BYTE_CHARACTER)] = 1;
        assert_eq!(default_ending(&jill), 5);
        enter(&mut menu, &mut game, Tool::ClearGame);
        menu.handle_input(up(), &mut game);
        assert_eq!(
            menu.handle_input(confirm(), &mut game),
            DebugMenuEvent::ClearGame(7)
        );
        menu.handle_input(down(), &mut game);
        assert_eq!(
            menu.handle_input(confirm(), &mut game),
            DebugMenuEvent::ClearGame(1)
        );
    }
}
