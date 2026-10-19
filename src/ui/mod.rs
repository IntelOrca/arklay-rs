//! UI screens: the title, character select and the save-load picker.
//!
//! Every screen implements [`Screen`], the shared contract the engine's app
//! loop drives. A screen loads its art in [`Screen::open`], advances one fixed
//! 30 Hz tick in [`Screen::update`], draws itself in [`Screen::draw`] and
//! reports a completed interaction through [`ScreenResult`]. The app reacts to
//! [`ScreenAction`] values by switching modes.
//!
//! The same contract also serves gameplay modals: `crate::engine::GameSession`
//! owns an optional boxed screen and freezes the room tick while one is
//! installed, so screens that own all of their state (the item viewer) plug in
//! without changing the mode machine. The message window and pause menu are
//! not [`Screen`]s: they share [`crate::game::GameState`] and are driven
//! explicitly by the session.

pub mod computer_lab;
pub mod death;
pub mod debug_menu;
pub mod file;
pub mod item_box;
pub mod item_view;
pub mod layout;
pub mod lua_screen;
pub mod main_menu;
pub mod map;
pub mod return_title;
pub mod save_load;
pub mod status;

use std::path::Path;

mod scripts {
    include!(concat!(env!("OUT_DIR"), "/ui_scripts.rs"));
}
/// The checked-in screen scripts (`games/re1/ui/**.lua`) as `(entry, source)`
/// pairs. `convert-game` packs them; a pack that predates them (or a test
/// pack) falls back to these copies, so every pack has its screens.
pub use scripts::UI_SCRIPTS;

use anyhow::{Context, Result};

use crate::assets::{self, Artwork, Geometry};
use crate::font::Font;
use crate::pack::{AssetClass, Pack};
use crate::render::Framebuffer;
use crate::state::Image;
use crate::text::Text;
use crate::transition::FadeColor;

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
    /// Confirm (Space or Return by default).
    pub confirm: bool,
    /// Cancel (X, Backspace or Escape by default).
    pub cancel: bool,
    /// L1 (`[` by default): pages the item box back.
    pub page_left: bool,
    /// R1 (`]` by default): pages the item box forward.
    pub page_right: bool,
    /// START (Tab by default): opens the gameplay pause menu.
    pub start: bool,
    /// F1: the port-only debug room overlay's edge. The overlay is always
    /// available; `--debug-menu` is accepted for compatibility only.
    pub debug_menu: bool,
    /// F9: the in-game return-to-title prompt's edge. A second F9 confirms
    /// the return and any other key cancels.
    pub return_title: bool,
    /// Any key went down this tick.
    pub any: bool,
    /// The first bindable input pressed this tick, whatever it is bound to:
    /// the rebind screen's capture.
    pub captured: Option<crate::controls::Source>,
}

/// What a screen reports after one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenResult {
    /// Keep this screen and tick again.
    Continue,
    /// The screen finished; the app should apply [`ScreenAction`].
    Done(ScreenAction),
}

/// A completed screen interaction, interpreted by the app's mode machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenAction {
    /// Start a new game as the character (`0` Chris, `1` Jill).
    NewGame { character: u8 },
    /// Continue from save slot `index` (zero-based, `savedat{index+1}.dat`).
    LoadGame { slot: usize },
    /// Continue from an explicit save file, the file a Lua screen chose.
    LoadFile(std::path::PathBuf),
    /// Play the next attract-demo reel (`demo/<hex>.dat`), then return to the
    /// title.
    Attract,
    /// Return to the title screen.
    Title,
    /// Open the character-selection screen.
    CharSelect,
    /// Open the load screen.
    SaveLoad,
    /// The ending films finished: open the RESULT screen.
    EndingResult,
    /// The RESULT screen finished: run the rocket-launcher epilogue (endings
    /// 6/7 without the special key) or go straight to the next-cycle save.
    EndingEpilogue,
    /// The epilogue finished (or was skipped): build the next-cycle card and
    /// open the save screen.
    EndingSave,
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
    /// The title's confirm voice line (`Evil01`, global bank 12 slot 0).
    Title,
    /// A cursor move (character-table slot 4).
    Cursor,
    /// A cancel (character-table slot 5).
    Cancel,
    /// A confirm/decide (character-table slot 6).
    Decide,
    /// The character-select cursor move (`Select06`, select bank slot 0).
    SelectCursor,
    /// The character-select confirm (`Select05`, select bank slot 1).
    SelectConfirm,
    /// The save/load cursor move (`type01`, title bank slot 14).
    SaveCursor,
    /// The save/load confirm (`type02`, title bank slot 15). The typed
    /// reveal's "typewriter" tick plays the same slot per revealed character.
    SaveConfirm,
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
            UiCue::SelectCursor => crate::sfx::global_sfx(13, 0),
            UiCue::SelectConfirm => crate::sfx::global_sfx(13, 1),
            UiCue::SaveCursor => crate::sfx::global_sfx(12, 14),
            UiCue::SaveConfirm => crate::sfx::global_sfx(12, 15),
        }
    }
}

/// Load one UI page by stem: the shipped TIM or a mod's replacement.
///
/// A page is addressed by fixed UVs or sprite rectangles, so a replacement
/// must keep the shipped page's dimensions; see [`Geometry::Shipped`].
pub fn load_page(pack: &Pack, stem: &str) -> Result<Artwork> {
    let asset = pack
        .read_asset(stem, AssetClass::Image)
        .with_context(|| format!("failed to read {stem}"))?
        .with_context(|| format!("missing {stem}"))?;
    assets::decode_page(&asset, Geometry::Shipped)
        .with_context(|| format!("invalid {}", asset.path()))
}

/// Load one full-frame picture by stem, with an optional transparency key.
///
/// [`Geometry::Free`] accepts any size (the caller stretches the picture over
/// its frame); [`Geometry::Shipped`] keeps a sheet that is sampled by fixed
/// rectangles at the shipped dimensions.
pub fn load_picture(
    pack: &Pack,
    stem: &str,
    geometry: Geometry,
    key: Option<[u8; 3]>,
) -> Result<Image> {
    let asset = pack
        .read_asset(stem, AssetClass::Image)
        .with_context(|| format!("failed to read {stem}"))?
        .with_context(|| format!("missing {stem}"))?;
    assets::decode_picture(&asset, geometry, key)
        .with_context(|| format!("invalid {}", asset.path()))
}

/// Everything a screen may read while it runs.
///
/// The context is built fresh each tick by the app, so screens never own (or
/// borrow beyond a call) the pack.
pub struct UiContext<'a> {
    /// The open game pack.
    pub pack: &'a Pack,
    /// Save directory (`--save-dir`, `arklay.toml`'s `save_dir`, default
    /// `<config dir>/save`).
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

    /// The full-screen overlay to blend after the screen draws, or `None`
    /// while the frame is clear.
    ///
    /// The default maps [`Screen::fade`] to a black overlay, so a screen that
    /// only ever fades to black keeps implementing `fade`; screens with the
    /// original's white flash override this to select the colour.
    fn overlay(&self) -> Option<(FadeColor, u8)> {
        match self.fade() {
            0 => None,
            alpha => Some((FadeColor::Black, alpha)),
        }
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
        assert_eq!(UiCue::SelectCursor.name(), Some("Select06"));
        assert_eq!(UiCue::SelectConfirm.name(), Some("Select05"));
        assert_eq!(UiCue::SaveCursor.name(), Some("type01"));
        assert_eq!(UiCue::SaveConfirm.name(), Some("type02"));
    }

    #[test]
    fn the_default_overlay_is_the_black_fade() {
        struct Black {
            alpha: u8,
        }
        impl Screen for Black {
            fn open(&mut self, _cx: &mut UiContext<'_>) -> Result<()> {
                Ok(())
            }
            fn update(&mut self, _cx: &UiContext<'_>, _input: UiInput) -> ScreenResult {
                ScreenResult::Continue
            }
            fn draw(&mut self, _cx: &UiContext<'_>, _framebuffer: &mut Framebuffer) {}
            fn fade(&self) -> u8 {
                self.alpha
            }
        }

        assert_eq!(Black { alpha: 0 }.overlay(), None);
        assert_eq!(
            Black { alpha: 128 }.overlay(),
            Some((FadeColor::Black, 128))
        );
    }
}
