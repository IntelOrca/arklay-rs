//! The title screen.
//!
//! The background is `ui/title.bmp`. The prompt rows come from the shipped
//! 4bpp `ui/t_press.tim` (or `ui/t_start.tim` when that is the only sheet
//! present): row `0` is PRESS ANY BUTTON, row `83` is the NEW GAME/LOAD GAME
//! band with NEW GAME highlighted and row `175` the band with LOAD GAME
//! highlighted. A band is a full 256-pixel-wide, 70-row sprite drawn at the
//! original descriptor position: x `-130`, y `38`, with the screen origin
//! `(160, 120)` added, exactly as the original's sprite path does.
//!
//! The screen fades in from black while the PRESS text ramps up, then pulses
//! bright/dim. Any key opens the option menu, where left/right picks NEW GAME
//! or LOAD GAME. LOAD is refused when no `savedat*.dat` exists: the cursor
//! never leaves NEW GAME and confirming it is a no-op. An attract-mode timeout
//! is not implemented; the screen waits on PRESS forever.

use anyhow::Result;

use crate::bmp;
use crate::render::Framebuffer;
use crate::save;
use crate::state::Image;
use crate::tim;
use crate::transition::{Fade, FadeColor};

use super::{Screen, ScreenAction, ScreenResult, UiContext, UiCue, UiInput};

/// Screen origin the original's sprite path adds to every descriptor.
const SCREEN_ORIGIN_X: i32 = 160;
/// Screen origin the original's sprite path adds to every descriptor.
const SCREEN_ORIGIN_Y: i32 = 120;
/// The descriptor x of every title prompt row.
const PROMPT_X: i32 = -130;
/// The descriptor y the row's own screen-y is offset from.
const PROMPT_Y: i32 = 38;
/// Frames the PRESS ramp takes to reach full brightness.
const PRESS_RAMP_STEP: u8 = 4;
/// Brightness byte the ramp ends at (the original's `0x80`).
const PRESS_FULL: u8 = 0x80;
/// Ticks per PRESS blink phase.
const BLINK_TICKS: u32 = 32;
/// Fade step per tick for the screen fades.
const FADE_STEP: u8 = 8;

/// One prompt row's UV: sheet row, sprite height and descriptor screen-y.
struct PromptRow {
    v: i32,
    height: i32,
    screen_y: i32,
}

/// PRESS ANY BUTTON.
const ROW_PRESS: PromptRow = PromptRow {
    v: 0,
    height: 54,
    screen_y: 24,
};
/// NEW GAME highlighted, LOAD GAME dim.
const ROW_NEW_GAME: PromptRow = PromptRow {
    v: 83,
    height: 70,
    screen_y: 8,
};
/// LOAD GAME highlighted, NEW GAME dim.
const ROW_LOAD_GAME: PromptRow = PromptRow {
    v: 175,
    height: 70,
    screen_y: 8,
};

/// Which part of the screen is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// PRESS ANY BUTTON fades in and blinks.
    Press,
    /// NEW GAME / LOAD GAME selection.
    Menu,
    /// Fade to black, then report the stored action.
    FadeOut,
}

/// The title screen state.
pub struct TitleScreen {
    background: Option<Image>,
    prompt: Option<crate::model::Texture8>,
    stage: Stage,
    /// `1` NEW GAME, `2` LOAD GAME.
    selection: u8,
    /// Whether any `savedat*.dat` exists in the save directory.
    any_saves: bool,
    /// PRESS brightness byte, `0..=0x80`.
    press_brightness: u8,
    /// The full-screen overlay accumulator: the opening black fade-in, then
    /// the confirm flash chain (white flash, white decay, black out).
    fade: Fade,
    /// Which leg of the confirm flash chain is running (the original's
    /// `g_titleOptionsFading` 6..9 then 4).
    fade_phase: u8,
    ticks: u32,
    /// Action reported once the fade-out finishes.
    exit: Option<ScreenAction>,
}

impl TitleScreen {
    /// An empty title screen; call [`Screen::open`] to load its art.
    pub fn new() -> Self {
        Self {
            background: None,
            prompt: None,
            stage: Stage::Press,
            selection: 1,
            any_saves: false,
            press_brightness: 0,
            fade: Fade::fade_in(FADE_STEP.into(), 0x7FFF, 2),
            fade_phase: 0,
            ticks: 0,
            exit: None,
        }
    }

    /// Whether LOAD GAME can be chosen: at least one save file exists.
    pub fn load_enabled(&self) -> bool {
        self.any_saves
    }

    /// The option menu's current selection (`1` NEW GAME, `2` LOAD GAME).
    pub fn selection(&self) -> u8 {
        self.selection
    }

    /// Whether confirming has handed the screen back to gameplay pacing.
    ///
    /// The original sets its game-active flag when NEW GAME or LOAD GAME is
    /// confirmed, so the fade-out chain runs on the 33 ms gameplay limiter
    /// while the PRESS and menu phases run on the 16 ms one.
    pub fn game_active(&self) -> bool {
        self.stage == Stage::FadeOut
    }

    /// Move the menu cursor by `delta`. LOAD GAME is unreachable while no save
    /// exists, so the selection stays on NEW GAME.
    fn move_selection(&mut self, delta: i32) {
        if !self.any_saves {
            self.selection = 1;
            return;
        }
        let moved = i32::from(self.selection) + delta;
        self.selection = if moved <= 1 { 1 } else { 2 };
    }

    /// The PRESS text's colour scale: ramp up from black, then pulse.
    fn press_scale(&self) -> u8 {
        if self.press_brightness < PRESS_FULL {
            u16::from(self.press_brightness).saturating_mul(2).min(255) as u8
        } else if (self.ticks / BLINK_TICKS).is_multiple_of(2) {
            255
        } else {
            140
        }
    }

    /// Latch the next leg of the confirm flash chain (the original's
    /// `g_titleOptionsFading` 7, 8, 9 then 4; leg 6 is the initial white
    /// flash, set on confirm). The white legs decay from full white; the last
    /// is the black-out. Returns `true` once the black-out completes.
    fn advance_fade_phase(&mut self) -> bool {
        self.fade_phase = self.fade_phase.saturating_add(1);
        self.fade = match self.fade_phase {
            // 7: white decay at `-0x4000`.
            1 => Fade::fade_in(0x4000 >> 7, 0x7FFF, 1),
            // 8: white decay at `-0x8000`.
            2 => Fade::fade_in(256, 0x7FFF, 1),
            // 9: white decay at `-0x800`.
            3 => Fade::fade_in(0x800 >> 7, 0x7FFF, 1),
            // 4: black-out at `+0x270`.
            4 => {
                let mut fade = Fade::inactive();
                fade.set(2, 0x270, 0);
                fade
            }
            _ => return true,
        };
        false
    }
}

impl Default for TitleScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen for TitleScreen {
    fn open(&mut self, cx: &mut UiContext<'_>) -> Result<()> {
        self.background = match cx.pack.read("ui/title.bmp") {
            Ok(bytes) => match bmp::decode(bytes) {
                Ok(image) => Some(image),
                Err(err) => {
                    eprintln!("warning: invalid title background: {err:#}");
                    None
                }
            },
            Err(err) => {
                eprintln!("warning: missing title background: {err:#}");
                None
            }
        };
        let prompt = ["ui/t_press.tim", "ui/t_start.tim"]
            .into_iter()
            .find_map(|entry| cx.pack.read(entry).ok());
        self.prompt = match prompt {
            Some(bytes) => match tim::decode_4bpp(bytes) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    eprintln!("warning: invalid title prompt sheet: {err:#}");
                    None
                }
            },
            None => {
                eprintln!("warning: missing title prompt sheet (t_press/t_start)");
                None
            }
        };
        self.any_saves = save::scan_slots(cx.save_dir).iter().any(Option::is_some);
        self.selection = if self.any_saves { 2 } else { 1 };
        self.press_brightness = 0;
        self.fade = Fade::fade_in(FADE_STEP.into(), 0x7FFF, 2);
        self.fade_phase = 0;
        self.ticks = 0;
        self.stage = Stage::Press;
        self.exit = None;
        Ok(())
    }

    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
        self.ticks = self.ticks.saturating_add(1);
        // TODO(parity): (UI) the original title runs an attract/demo timer
        // (`g_titleDemoTime`): idling on PRESS or on the option menu eventually
        // fades into the attract demo and back, the selection id cycles
        // NEW/LOAD (and the DC STANDARD/TRAINING/ADVANCED submenu). The port
        // waits on PRESS forever with only the two entries. The PRESS phase
        // and the option-menu moves are silent in the original; confirming
        // NEW GAME or LOAD GAME plays EVIL01 and runs the white flash chain.
        match self.stage {
            Stage::Press => {
                self.fade.tick();
                self.press_brightness = self
                    .press_brightness
                    .saturating_add(PRESS_RAMP_STEP)
                    .min(PRESS_FULL);
                if input.any || input.confirm || input.cancel {
                    self.stage = Stage::Menu;
                    self.ticks = 0;
                }
            }
            Stage::Menu => {
                self.fade.tick();
                if input.left {
                    self.move_selection(-1);
                }
                if input.right {
                    self.move_selection(1);
                }
                if input.up {
                    self.move_selection(-1);
                }
                if input.down {
                    self.move_selection(1);
                }
                if input.confirm {
                    let action = match self.selection {
                        2 if self.any_saves => Some(ScreenAction::SaveLoad),
                        2 => None,
                        _ => Some(ScreenAction::CharSelect),
                    };
                    if let Some(action) = action {
                        cx.play_cue(UiCue::Title);
                        self.exit = Some(action);
                        // The original's state 6: a white flash at `+0x7F00`
                        // that wraps the accumulator in two ticks.
                        self.fade = Fade::fade_out(0x7F00 >> 7, 0, 1);
                        self.fade_phase = 0;
                        self.stage = Stage::FadeOut;
                    }
                }
            }
            Stage::FadeOut => {
                self.fade.tick();
                if !self.fade.is_active() && self.advance_fade_phase() {
                    return ScreenResult::Done(self.exit.take().unwrap_or(ScreenAction::Title));
                }
            }
        }
        ScreenResult::Continue
    }

    fn draw(&mut self, _cx: &UiContext<'_>, framebuffer: &mut Framebuffer) {
        match &self.background {
            Some(background) => framebuffer.blit(background),
            None => {
                framebuffer.clear();
                framebuffer.fill_rect(
                    [0, 0, framebuffer.width as i32, framebuffer.height as i32],
                    [0, 0, 0, 255],
                );
            }
        }
        let Some(prompt) = &self.prompt else {
            return;
        };
        let (row, scale) = match self.stage {
            Stage::Press => (&ROW_PRESS, self.press_scale()),
            Stage::Menu | Stage::FadeOut => {
                let row = if self.selection == 2 {
                    &ROW_LOAD_GAME
                } else {
                    &ROW_NEW_GAME
                };
                (row, 255)
            }
        };
        framebuffer.draw_indexed_sprite_scaled(
            prompt,
            [0, row.v, 256, row.height],
            [
                PROMPT_X + SCREEN_ORIGIN_X,
                row.screen_y + PROMPT_Y + SCREEN_ORIGIN_Y,
                256,
                row.height,
            ],
            0,
            scale,
            crate::font::Tint::White,
        );
    }

    fn fade(&self) -> u8 {
        match self.fade.overlay() {
            Some(overlay) if overlay.color == FadeColor::Black => overlay.alpha,
            _ => 0,
        }
    }

    fn overlay(&self) -> Option<(FadeColor, u8)> {
        self.fade
            .overlay()
            .map(|overlay| (overlay.color, overlay.alpha))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A title screen in the option menu without touching the pack.
    fn menu(any_saves: bool) -> TitleScreen {
        TitleScreen {
            stage: Stage::Menu,
            any_saves,
            selection: if any_saves { 2 } else { 1 },
            fade: Fade::inactive(),
            ..TitleScreen::new()
        }
    }

    fn neutral() -> UiInput {
        UiInput::default()
    }

    #[test]
    fn any_button_opens_the_option_menu() {
        let mut screen = TitleScreen::new();
        let cx = test_context();
        screen.update(
            &cx,
            UiInput {
                any: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::Menu);
    }

    #[test]
    fn selection_starts_on_load_only_with_saves() {
        let mut cx = test_context();
        let mut screen = TitleScreen::new();
        screen.open(&mut cx).unwrap();
        // The context's save directory is empty.
        assert!(!screen.load_enabled());
        assert_eq!(screen.selection(), 1);
    }

    #[test]
    fn load_is_unreachable_without_saves() {
        let mut screen = menu(false);
        let cx = test_context();
        screen.update(
            &cx,
            UiInput {
                right: true,
                ..neutral()
            },
        );
        assert_eq!(screen.selection(), 1, "LOAD must stay unreachable");
        screen.update(
            &cx,
            UiInput {
                left: true,
                ..neutral()
            },
        );
        assert_eq!(screen.selection(), 1);

        // Even forced onto the row, confirming LOAD does nothing.
        screen.selection = 2;
        let result = screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(result, ScreenResult::Continue);
        assert_eq!(screen.stage, Stage::Menu);
    }

    #[test]
    fn cursor_selects_load_and_confirms_when_saves_exist() {
        let mut screen = menu(true);
        let mut cx = test_context();
        screen.update(
            &cx,
            UiInput {
                left: true,
                ..neutral()
            },
        );
        assert_eq!(screen.selection(), 1);
        screen.update(
            &cx,
            UiInput {
                right: true,
                ..neutral()
            },
        );
        assert_eq!(screen.selection(), 2);

        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::FadeOut);
        let action = run_to_result(&mut screen, &mut cx);
        assert_eq!(action, ScreenAction::SaveLoad);
    }

    #[test]
    fn confirm_new_game_fades_then_reports_char_select() {
        let mut screen = menu(false);
        let mut cx = test_context();
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::FadeOut);
        let action = run_to_result(&mut screen, &mut cx);
        assert_eq!(action, ScreenAction::CharSelect);
    }

    #[test]
    fn only_the_confirm_plays_the_title_voice() {
        let mut screen = TitleScreen::new();
        let cx = test_context();
        // The PRESS dismissal is silent.
        screen.update(
            &cx,
            UiInput {
                any: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::Menu);
        assert!(cx.cues.borrow().is_empty(), "PRESS must not play a cue");

        // The option-menu moves are silent in the original.
        for input in [
            UiInput {
                left: true,
                ..neutral()
            },
            UiInput {
                right: true,
                ..neutral()
            },
            UiInput {
                up: true,
                ..neutral()
            },
            UiInput {
                down: true,
                ..neutral()
            },
        ] {
            screen.update(&cx, input);
            assert!(cx.cues.borrow().is_empty(), "a menu move must be silent");
        }

        // Confirming NEW GAME or LOAD GAME plays the RE voice line.
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(cx.cues.borrow().as_slice(), &[UiCue::Title]);
    }

    #[test]
    fn confirm_runs_the_white_flash_then_black_out() {
        let mut screen = menu(false);
        let cx = test_context();
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(screen.overlay(), Some((FadeColor::White, 0)));

        // Leg 6: the flash reaches white in two ticks.
        screen.update(&cx, neutral());
        assert_eq!(screen.overlay(), Some((FadeColor::White, 254)));
        screen.update(&cx, neutral());
        assert_eq!(
            screen.overlay(),
            Some((FadeColor::White, 255)),
            "leg 7 starts from full white"
        );
        screen.update(&cx, neutral());
        assert_eq!(screen.overlay(), Some((FadeColor::White, 127)));
        screen.update(&cx, neutral());
        assert_eq!(
            screen.overlay(),
            Some((FadeColor::White, 255)),
            "leg 8 starts from full white"
        );
        screen.update(&cx, neutral());
        assert_eq!(
            screen.overlay(),
            Some((FadeColor::White, 255)),
            "leg 9 starts from full white"
        );

        // Leg 9 decays 16 alpha steps a tick over 15 ticks.
        screen.update(&cx, neutral());
        assert_eq!(screen.overlay(), Some((FadeColor::White, 239)));
        for _ in 0..14 {
            screen.update(&cx, neutral());
        }
        assert_eq!(screen.overlay(), Some((FadeColor::White, 15)));

        // Leg 4: the black-out rises to full.
        screen.update(&cx, neutral());
        assert_eq!(screen.overlay(), Some((FadeColor::Black, 0)));
        screen.update(&cx, neutral());
        assert_eq!(screen.overlay(), Some((FadeColor::Black, 4)));
        let mut ticks = 1;
        loop {
            if let ScreenResult::Done(action) = screen.update(&cx, neutral()) {
                assert_eq!(action, ScreenAction::CharSelect);
                break;
            }
            ticks += 1;
            assert!(ticks < 128, "the black-out never finished");
        }
        assert_eq!(ticks, 52, "the black-out holds for 52 more drawn ticks");
    }

    #[test]
    fn the_fade_in_reaches_clear_and_lifts_the_press_text() {
        let mut screen = TitleScreen::new();
        let cx = test_context();
        for _ in 0..64 {
            screen.update(&cx, neutral());
        }
        assert_eq!(screen.fade(), 0);
        assert_eq!(screen.press_brightness, PRESS_FULL);
        // The blink alternates between two non-black scales.
        let scales: Vec<u8> = (0..BLINK_TICKS * 2)
            .map(|_| {
                screen.update(&cx, neutral());
                screen.press_scale()
            })
            .collect();
        assert!(scales.contains(&255));
        assert!(scales.iter().any(|&scale| scale < 255));
    }

    fn run_to_result(screen: &mut TitleScreen, cx: &mut UiContext<'_>) -> ScreenAction {
        for _ in 0..256 {
            if let ScreenResult::Done(action) = screen.update(cx, neutral()) {
                return action;
            }
        }
        panic!("the screen never finished");
    }

    /// A context over an empty pack and save directory.
    fn test_context() -> UiContext<'static> {
        use std::path::Path;
        use std::sync::OnceLock;

        static PACK: OnceLock<crate::pack::Pack> = OnceLock::new();
        static SAVE_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
        let dir = SAVE_DIR.get_or_init(|| {
            let path =
                std::env::temp_dir().join(format!("arklay-title-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            path
        });
        let pack = PACK.get_or_init(|| {
            let path = std::env::temp_dir()
                .join(format!("arklay-title-test-{}.akpak", std::process::id()));
            let mut writer = crate::pack::PackWriter::new();
            writer.add("unused", Vec::new()).unwrap();
            writer.write(&path).unwrap();
            let pack = crate::pack::Pack::open(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            pack
        });
        UiContext {
            pack,
            save_dir: Path::new(dir),
            font: None,
            text: None,
            ticks: 0,
            cues: Default::default(),
        }
    }
}
