//! UI screens: the title, character select and the save-load picker.
//!
//! Every screen implements [`Screen`], the shared contract the engine's app
//! loop drives. A screen loads its art in [`Screen::open`], advances one fixed
//! 30 Hz tick in [`Screen::update`], draws itself in [`Screen::draw`] and
//! reports a completed interaction through [`ScreenResult`]. The app reacts to
//! [`ScreenAction`] values by switching modes.
//!
//! The same contract also serves gameplay modals: [`crate::engine::GameSession`]
//! owns an optional boxed screen and freezes the room tick while one is
//! installed, so screens that own all of their state (the item viewer) plug in
//! without changing the mode machine. The message window and pause menu are
//! not [`Screen`]s: they share [`crate::game::GameState`] and are driven
//! explicitly by the session.

pub mod char_select;
pub mod file;
pub mod item_box;
pub mod item_view;
pub mod layout;
pub mod main_menu;
pub mod save_load;
pub mod status;
pub mod title;

use std::path::Path;

use anyhow::Result;

use crate::font::Font;
use crate::pack::Pack;
use crate::render::Framebuffer;
use crate::text::Text;

/// One frame of edge-triggered UI input.
///
/// Every flag is an edge: it is true only on the tick the key went down, so a
/// screen never has to debounce. `any` covers every other key the platform
/// reported this tick and drives the title's "press any button" gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UiInput {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    /// Confirm (Space or Return).
    pub confirm: bool,
    /// Cancel (X or Backspace).
    pub cancel: bool,
    /// L1 (`[`): pages the item box back.
    pub page_left: bool,
    /// R1 (`]`): pages the item box forward.
    pub page_right: bool,
    /// START (Tab): opens the gameplay pause menu.
    pub start: bool,
    /// Any key went down this tick.
    pub any: bool,
}

/// What a screen reports after one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenResult {
    /// Keep this screen and tick again.
    Continue,
    /// The screen finished; the app should apply [`ScreenAction`].
    Done(ScreenAction),
}

/// A completed screen interaction, interpreted by the app's mode machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenAction {
    /// Start a new game as the character (`0` Chris, `1` Jill).
    NewGame { character: u8 },
    /// Continue from save slot `index` (zero-based, `savedat{index+1}.dat`).
    LoadGame { slot: usize },
    /// Return to the title screen.
    Title,
    /// Open the character-selection screen.
    CharSelect,
    /// Open the load screen.
    SaveLoad,
    /// Close a gameplay modal and resume the frozen room.
    Resume,
    /// Quit the app.
    Quit,
}

/// Everything a screen may read while it runs.
///
/// The context is built fresh each tick by the app, so screens never own (or
/// borrow beyond a call) the pack.
pub struct UiContext<'a> {
    /// The open game pack.
    pub pack: &'a Pack,
    /// Save directory (`--save-dir`, default `saves/` beside the pack).
    pub save_dir: &'a Path,
    /// The decoded font, when the pack carries one.
    pub font: Option<&'a Font>,
    /// The decoded text tables, when the pack carries them.
    pub text: Option<&'a Text>,
    /// Fixed ticks the app has run, for deterministic animations.
    pub ticks: u64,
}

/// The shared screen contract.
pub trait Screen {
    /// Load the screen's art and initial state. Called once before the first
    /// update; missing optional art is logged and leaves that layer empty.
    fn open(&mut self, cx: &mut UiContext<'_>) -> Result<()>;

    /// Advance one fixed tick from edge-triggered input.
    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult;

    /// Draw one frame into `framebuffer`. The framebuffer already holds the
    /// frozen gameplay frame when the screen runs as a modal.
    fn draw(&mut self, cx: &UiContext<'_>, framebuffer: &mut Framebuffer);

    /// Black-overlay opacity in `0..=255` drawn over the screen after it
    /// draws; `0` means fully visible and `255` fully black.
    fn fade(&self) -> u8 {
        0
    }
}
