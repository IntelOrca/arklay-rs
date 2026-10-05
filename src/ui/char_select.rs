//! The character-selection screen.
//!
//! `ui/sel_back.bmp` is the desk background. The two police cards come from
//! `ui/select_b.bmp`: each card is a left 80x128 half and a right 112x128 half,
//! with Chris on row 0 and Jill on row `0x80`. Only the end poses are drawn
//! (the original slides and rescales the cards while the choice changes): the
//! selected card sits at its front position at full scale and brightness, the
//! other card at its back position scaled to `0.8125` and dimmed. The blinking
//! pick arrows are the small sprites at `(0xC0, 0x30)`/`(0xC0, 0x50)`.
//! `ui/select_k.tim` is loaded the way the shipped screen does, but the end
//! poses do not sample it.
//!
//! Left/right switches the pick, confirm starts a new game with that character
//! and cancel fades back to the title.

use anyhow::Result;

use crate::bmp;
use crate::model::Texture8;
use crate::render::Framebuffer;
use crate::state::Image;
use crate::tim;

use super::{Screen, ScreenAction, ScreenResult, UiContext, UiCue, UiInput};

/// Screen origin the original's sprite path adds to every descriptor.
const SCREEN_ORIGIN_X: i32 = 160;
/// Screen origin the original's sprite path adds to every descriptor.
const SCREEN_ORIGIN_Y: i32 = 120;
/// Width of a card's left (portrait) half.
const CARD_LEFT_W: i32 = 0x50;
/// Width of a card's right (information) half.
const CARD_RIGHT_W: i32 = 0x70;
/// Height of both card halves.
const CARD_H: i32 = 0x80;
/// The selected card's screen position (the original's `g_char0PosX/Y`).
const FRONT_POS: (i32, i32) = (0x88, 0x48);
/// The unselected card's screen position (the original's `g_char1PosX/Y`).
const BACK_POS: (i32, i32) = (200, 0x68);
/// 1.0 in the original's 4.12 fixed point.
const FRONT_SCALE: i32 = 0x1000;
/// `ComputeScale(0x110)`: the unselected card's `0.8125` scale.
const BACK_SCALE: i32 = 0xD00;
/// Full colour multiplier.
const FRONT_BRIGHTNESS: u8 = 255;
/// The original's `0x50` colour multiplier, `0x50 * 2 / 255 * 255`.
const BACK_BRIGHTNESS: u8 = 160;
/// The card shadow's black blend, `100 / 255`.
const SHADOW_ALPHA: u8 = 100;
/// Pick-arrow source region.
const ARROW_W: i32 = 0x0C;
/// Pick-arrow source region.
const ARROW_H: i32 = 0x0B;
/// Pick-arrow row when the blink is bright.
const ARROW_V_BRIGHT: i32 = 0x30;
/// Pick-arrow row when the blink is dim.
const ARROW_V_DIM: i32 = 0x50;
/// Ticks per arrow blink phase.
const BLINK_MASK: u32 = 0x30;

/// Which part of the screen is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    FadeOut,
}

/// The character-select screen state.
pub struct CharSelectScreen {
    background: Option<Image>,
    cards: Option<Image>,
    /// The shipped screen's second selection sheet; kept loaded for parity.
    extras: Option<Texture8>,
    /// `0` Chris, `1` Jill.
    selected: u8,
    fade: u8,
    ticks: u32,
    stage: Stage,
    exit: Option<ScreenAction>,
}

impl CharSelectScreen {
    /// An empty screen; call [`Screen::open`] to load its art.
    pub fn new() -> Self {
        Self {
            background: None,
            cards: None,
            extras: None,
            selected: 0,
            fade: 255,
            ticks: 0,
            stage: Stage::Idle,
            exit: None,
        }
    }

    /// The currently picked character (`0` Chris, `1` Jill).
    pub fn selected(&self) -> u8 {
        self.selected
    }

    /// Swap the pick.
    pub fn toggle(&mut self) {
        self.selected ^= 1;
    }

    /// Scale a source dimension by a 4.12 fixed-point factor.
    fn scaled(value: i32, scale: i32) -> i32 {
        (value * scale) >> 12
    }

    /// Draw one card: its left portrait half then its information half.
    fn draw_card(&self, framebuffer: &mut Framebuffer, character: u8, front: bool) {
        let Some(cards) = &self.cards else {
            return;
        };
        let (pos_x, pos_y, scale, brightness) = if front {
            (FRONT_POS.0, FRONT_POS.1, FRONT_SCALE, FRONT_BRIGHTNESS)
        } else {
            (BACK_POS.0, BACK_POS.1, BACK_SCALE, BACK_BRIGHTNESS)
        };
        let x = pos_x - 0x100 + SCREEN_ORIGIN_X;
        let y = pos_y - 0x98 + SCREEN_ORIGIN_Y;
        let row = i32::from(character) * CARD_H;
        let left_w = Self::scaled(CARD_LEFT_W, scale);
        let right_w = Self::scaled(CARD_RIGHT_W, scale);
        let height = Self::scaled(CARD_H, scale);
        framebuffer.draw_rgba_sprite_scaled(
            cards,
            [0, row, CARD_LEFT_W, CARD_H],
            [x, y, left_w, height],
            brightness,
        );
        framebuffer.draw_rgba_sprite_scaled(
            cards,
            [CARD_LEFT_W, row, CARD_RIGHT_W, CARD_H],
            [x + left_w, y, right_w, height],
            brightness,
        );
    }

    /// Draw the black shadow the original lays over the back card.
    fn draw_shadow(&self, framebuffer: &mut Framebuffer, front: bool) {
        let (pos_x, pos_y, scale) = if front {
            (FRONT_POS.0, FRONT_POS.1, FRONT_SCALE)
        } else {
            (BACK_POS.0, BACK_POS.1, BACK_SCALE)
        };
        let x = pos_x - 0x100 + SCREEN_ORIGIN_X;
        let y = pos_y - 0x98 + SCREEN_ORIGIN_Y;
        framebuffer.blend_black_rect(
            [
                x,
                y,
                Self::scaled(CARD_LEFT_W + CARD_RIGHT_W, scale),
                Self::scaled(CARD_H, scale),
            ],
            SHADOW_ALPHA,
        );
    }

    /// Draw the two blinking pick arrows.
    fn draw_cursors(&self, framebuffer: &mut Framebuffer) {
        let Some(cards) = &self.cards else {
            return;
        };
        let v = if (self.ticks & BLINK_MASK) == 0 {
            ARROW_V_DIM
        } else {
            ARROW_V_BRIGHT
        };
        let y = -0x16 + SCREEN_ORIGIN_Y;
        framebuffer.draw_rgba_sprite_scaled(
            cards,
            [0xC0, v, ARROW_W, ARROW_H],
            [-0x88 + SCREEN_ORIGIN_X, y, ARROW_W, ARROW_H],
            255,
        );
        framebuffer.draw_rgba_sprite_scaled(
            cards,
            [0xC0, v + 0x10, ARROW_W, ARROW_H],
            [0x4C + SCREEN_ORIGIN_X, y, ARROW_W, ARROW_H],
            255,
        );
    }
}

impl Default for CharSelectScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen for CharSelectScreen {
    fn open(&mut self, cx: &mut UiContext<'_>) -> Result<()> {
        self.background = match cx.pack.read("ui/sel_back.bmp") {
            Ok(bytes) => match bmp::decode(bytes) {
                Ok(image) => Some(image),
                Err(err) => {
                    eprintln!("warning: invalid character-select background: {err:#}");
                    None
                }
            },
            Err(err) => {
                eprintln!("warning: missing character-select background: {err:#}");
                None
            }
        };
        self.cards = match cx.pack.read("ui/select_b.bmp") {
            Ok(bytes) => match bmp::decode(bytes) {
                Ok(image) => Some(image),
                Err(err) => {
                    eprintln!("warning: invalid character-select cards: {err:#}");
                    None
                }
            },
            Err(err) => {
                eprintln!("warning: missing character-select cards: {err:#}");
                None
            }
        };
        self.extras = match cx.pack.read("ui/select_k.tim") {
            Ok(bytes) => match tim::decode_8bpp(bytes) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    eprintln!("warning: invalid character-select sheet: {err:#}");
                    None
                }
            },
            Err(err) => {
                eprintln!("warning: missing character-select sheet: {err:#}");
                None
            }
        };
        self.selected = 0;
        self.fade = 255;
        self.ticks = 0;
        self.stage = Stage::Idle;
        self.exit = None;
        Ok(())
    }

    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
        self.ticks = self.ticks.saturating_add(1);
        // TODO(parity): (UI) the original slides and rescales the two cards
        // between their front/back positions while the pick changes; the port
        // snaps straight to the end poses but plays the select cue slots.
        match self.stage {
            Stage::Idle => {
                self.fade = self.fade.saturating_sub(8);
                if input.left || input.right {
                    cx.play_cue(UiCue::Cursor);
                    self.toggle();
                }
                if input.confirm {
                    cx.play_cue(UiCue::Decide);
                    self.exit = Some(ScreenAction::NewGame {
                        character: self.selected,
                    });
                    self.stage = Stage::FadeOut;
                } else if input.cancel {
                    cx.play_cue(UiCue::Cancel);
                    self.exit = Some(ScreenAction::Title);
                    self.stage = Stage::FadeOut;
                }
            }
            Stage::FadeOut => {
                self.fade = self.fade.saturating_add(16);
                if self.fade == 255 {
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
        let back = self.selected ^ 1;
        self.draw_card(framebuffer, back, false);
        self.draw_shadow(framebuffer, false);
        self.draw_card(framebuffer, self.selected, true);
        self.draw_cursors(framebuffer);
    }

    fn fade(&self) -> u8 {
        self.fade
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn neutral() -> UiInput {
        UiInput::default()
    }

    fn context() -> UiContext<'static> {
        use std::path::Path;
        use std::sync::OnceLock;

        static PACK: OnceLock<crate::pack::Pack> = OnceLock::new();
        let pack = PACK.get_or_init(|| {
            let path = std::env::temp_dir()
                .join(format!("arklay-select-test-{}.akpak", std::process::id()));
            let mut writer = crate::pack::PackWriter::new();
            writer.add("unused", Vec::new()).unwrap();
            writer.write(&path).unwrap();
            let pack = crate::pack::Pack::open(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            pack
        });
        UiContext {
            pack,
            save_dir: Path::new("."),
            font: None,
            text: None,
            ticks: 0,
            cues: Default::default(),
        }
    }

    #[test]
    fn left_and_right_swap_the_pick() {
        let mut screen = CharSelectScreen::new();
        let cx = context();
        assert_eq!(screen.selected(), 0);
        screen.update(
            &cx,
            UiInput {
                right: true,
                ..neutral()
            },
        );
        assert_eq!(screen.selected(), 1);
        screen.update(
            &cx,
            UiInput {
                left: true,
                ..neutral()
            },
        );
        assert_eq!(screen.selected(), 0);
    }

    #[test]
    fn confirm_starts_a_new_game_for_the_picked_character() {
        let mut screen = CharSelectScreen::new();
        let cx = context();
        screen.update(
            &cx,
            UiInput {
                right: true,
                ..neutral()
            },
        );
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        let mut result = ScreenResult::Continue;
        for _ in 0..64 {
            result = screen.update(&cx, neutral());
            if result != ScreenResult::Continue {
                break;
            }
        }
        assert_eq!(
            result,
            ScreenResult::Done(ScreenAction::NewGame { character: 1 })
        );
    }

    #[test]
    fn cue_queue_follows_the_input_edges() {
        let mut screen = CharSelectScreen::new();
        let cx = context();
        screen.update(
            &cx,
            UiInput {
                right: true,
                ..neutral()
            },
        );
        assert_eq!(cx.cues.borrow().as_slice(), &[UiCue::Cursor]);
        cx.cues.borrow_mut().clear();
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(cx.cues.borrow().as_slice(), &[UiCue::Decide]);
    }

    #[test]
    fn cancel_returns_to_the_title() {
        let mut screen = CharSelectScreen::new();
        let cx = context();
        screen.update(
            &cx,
            UiInput {
                cancel: true,
                ..neutral()
            },
        );
        let mut result = ScreenResult::Continue;
        for _ in 0..64 {
            result = screen.update(&cx, neutral());
            if result != ScreenResult::Continue {
                break;
            }
        }
        assert_eq!(result, ScreenResult::Done(ScreenAction::Title));
    }
}
