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
pub mod map;
pub mod save_load;
pub mod status;
pub mod title;

use std::path::Path;

use anyhow::Result;

use crate::font::Font;
use crate::pack::Pack;
use crate::render::Framebuffer;
use crate::text::Text;

/// One frame of UI input.
///
/// `up`/`down`/`left`/`right` are edges: true only on the tick the key went
/// down, so a screen never has to debounce. The `held_*` flags are the keys'
/// held level, which the item viewer reads to spin its model continuously;
/// captures and synthetic tests leave them false. `any` covers every other key
/// the platform reported this tick and drives the title's "press any button"
/// gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UiInput {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    /// Held level of the direction keys, for continuous spin.
    pub held_up: bool,
    /// Held level of the direction keys, for continuous spin.
    pub held_down: bool,
    /// Held level of the direction keys, for continuous spin.
    pub held_left: bool,
    /// Held level of the direction keys, for continuous spin.
    pub held_right: bool,
    /// Confirm (Space or Return).
    pub confirm: bool,
    /// Cancel (X, Backspace or Escape).
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

/// One UI cue a screen asks the app to play after its tick.
///
/// Every port screen is silent on its own: it queues a cue through
/// [`UiContext::play_cue`], and the app resolves and mixes it when an audio
/// device is open. Captures leave the queue drained and unplayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCue {
    /// The title's opening cue (`Evil01`, global bank 12).
    Title,
    /// A cursor move (character-table slot 4).
    Cursor,
    /// A cancel (character-table slot 5).
    Cancel,
    /// A confirm/decide (character-table slot 6).
    Decide,
}

impl UiCue {
    /// The pack `se/` name the cue plays, or `None` when the bank slot is
    /// empty.
    pub fn name(self) -> Option<&'static str> {
        match self {
            UiCue::Title => crate::sfx::global_sfx(12, 0),
            UiCue::Cursor => Some(crate::sfx::UI_CURSOR),
            UiCue::Cancel => Some(crate::sfx::UI_CANCEL),
            UiCue::Decide => Some(crate::sfx::UI_DECIDE),
        }
    }
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
    /// Cues the screen requested this tick; the app drains them after
    /// [`Screen::update`]. Interior mutability keeps `update`'s `&UiContext`
    /// borrow (screens only ever push).
    pub cues: std::cell::RefCell<Vec<UiCue>>,
}

impl UiContext<'_> {
    /// Queue one cue for the app to play after this tick.
    pub fn play_cue(&self, cue: UiCue) {
        self.cues.borrow_mut().push(cue);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cue_names_resolve_to_pack_sounds() {
        assert_eq!(UiCue::Cursor.name(), Some("cursor"));
        assert_eq!(UiCue::Cancel.name(), Some("cancel"));
        assert_eq!(UiCue::Decide.name(), Some("decide"));
        assert_eq!(UiCue::Title.name(), Some("Evil01"));
    }
}
