//! The character-selection screen.
//!
//! `ui/sel_back.bmp` is the desk background. The two police cards come from
//! `ui/select_b.bmp`: the sheet holds one shared blue S.T.A.R.S. portrait half
//! at `(0, 0)`, Chris's information half at `(0x50, 0)` and Jill's at
//! `(0, 0x80)`, so both cards draw the shared portrait half next to their own
//! information half. The selected card sits at its
//! front position at full scale and brightness, the other at its back position
//! scaled to `0.8125` and dimmed. While the pick changes the cards slide,
//! overshoot and settle on the opposite poses over 34 ticks, exactly like the
//! original's acceleration state machine: the card on the way out accelerates
//! toward the other slot, its velocity reverses, the scale and brightness ramp
//! by ±2 and ±3 per frame, and the tpage (draw order) flips at the velocity
//! zero crossing. The blinking pick arrows are the small sprites at
//! `(0xC0, 0x30)`/`(0xC0, 0x50)`. `ui/select_k.tim` is loaded the way the
//! shipped screen does, but the end poses do not sample it.
//!
//! Left/right switches the pick, confirm starts a new game with that character
//! and cancel fades back to the title.

use anyhow::Result;

use crate::bmp;
use crate::model::Texture8;
use crate::render::Framebuffer;
use crate::state::Image;
use crate::tim;
use crate::transition::Fade;

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
/// The front card's panel scale field (`ComputeScale` maps it to 1.0).
const FRONT_PANEL_SCALE: i32 = 0xF0;
/// The back card's panel scale field (`ComputeScale` maps it to `0.8125`).
const BACK_PANEL_SCALE: i32 = 0x110;
/// The front card's panel brightness (`0x80`).
const FRONT_PANEL_BRIGHT: u8 = 0x80;
/// The back card's panel brightness (`0x50`).
const BACK_PANEL_BRIGHT: u8 = 0x50;
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
/// Ticks one card swap takes to reach its end poses.
pub const SWAP_TICKS: u32 = 34;

/// Which part of the screen is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Swapping,
    /// The white flash-in after the pick is confirmed (the original's state 3).
    ConfirmFadeIn,
    /// The white fade-out from full (the original's state 4).
    ConfirmFadeOut,
    /// The post-flash hold before the game starts (the original's state 5).
    ConfirmHold,
    /// The black fade-out back to the title (the original's state 7).
    CancelFadeOut,
}

/// One card's live pose: the position, the panel scale/brightness fields, the
/// draw priority (tpage) and the swap state machine's velocity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CardPose {
    pos: [i32; 2],
    scale: i32,
    bright: u8,
    tpage: u8,
    vel: [i32; 2],
    acc: [i32; 2],
}

impl CardPose {
    /// Whether this card draws in front of `other` (the original's lower
    /// tpage wins the later draw, i.e. the lower tpage is the front panel).
    fn in_front_of(&self, other: &CardPose) -> bool {
        self.tpage < other.tpage
    }
}

/// The character-select screen state.
pub struct CharSelectScreen {
    background: Option<Image>,
    cards: Option<Image>,
    /// The shipped screen's second selection sheet; kept loaded for parity.
    extras: Option<Texture8>,
    /// `0` Chris, `1` Jill: which card owns the front pose once a swap settles.
    selected: u8,
    /// The two cards' live poses, indexed by character.
    poses: [CardPose; 2],
    /// The swap state machine (the original's `g_selSubState`/`g_selTimer`).
    swap_sub: u8,
    swap_timer: i8,
    swap_dir: u8,
    /// The full-screen overlay accumulator: black while the screen fades in,
    /// then the original's white flash chain once the pick is confirmed.
    fade: Fade,
    /// Ticks left of the post-flash hold (the original's `g_selTimer`). The
    /// original stores `-16` in a signed byte and decrements to zero, so the
    /// hold is 240 ticks, not 16.
    hold_timer: u16,
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
            poses: Self::initial_poses(),
            swap_sub: 0,
            swap_timer: 0,
            swap_dir: 0,
            fade: Fade::fade_in(8, 0x7FFF, 2),
            hold_timer: 0,
            ticks: 0,
            stage: Stage::Idle,
            exit: None,
        }
    }

    /// The boot poses: card 0 (Chris) at the front, card 1 (Jill) at the back.
    fn initial_poses() -> [CardPose; 2] {
        [
            CardPose {
                pos: [FRONT_POS.0, FRONT_POS.1],
                scale: FRONT_PANEL_SCALE,
                bright: FRONT_PANEL_BRIGHT,
                tpage: 2,
                vel: [0; 2],
                acc: [0; 2],
            },
            CardPose {
                pos: [BACK_POS.0, BACK_POS.1],
                scale: BACK_PANEL_SCALE,
                bright: BACK_PANEL_BRIGHT,
                tpage: 3,
                vel: [0; 2],
                acc: [0; 2],
            },
        ]
    }

    /// The currently picked character (`0` Chris, `1` Jill).
    pub fn selected(&self) -> u8 {
        self.selected
    }

    /// Whether the slide animation is running.
    pub fn swapping(&self) -> bool {
        self.stage == Stage::Swapping
    }

    /// The live pose of `card` (0 Chris, 1 Jill), for tests.
    pub fn pose(&self, card: usize) -> (i32, i32, i32, u8, u8) {
        let pose = self.poses[card];
        (
            pose.pos[0],
            pose.pos[1],
            pose.scale,
            pose.bright,
            pose.tpage,
        )
    }

    /// Start the swap animation toward `dir` (`0` right, `1` left, the
    /// original's `g_selSwapDir`). The pick itself flips when the slide lands.
    pub fn start_swap(&mut self, dir: u8) {
        let dir = dir & 1;
        let (ax, ay) = if dir == self.selected {
            (2, 1)
        } else {
            (-2, -1)
        };
        self.poses[0].acc = [ax, ay];
        self.poses[1].acc = [-ax, -ay];
        for pose in &mut self.poses {
            pose.vel = [0; 2];
        }
        self.swap_dir = dir;
        self.swap_sub = 0;
        self.swap_timer = 8;
        self.stage = Stage::Swapping;
    }

    /// The original's `ComputeScale`: the panel scale field to 4.12.
    fn compute_scale(panel: i32) -> i32 {
        panel * -24 + 0x2680
    }

    /// The sprite brightness the panel field maps to (`0x80` -> full,
    /// `0x50` -> 160).
    fn panel_brightness(panel: u8) -> u8 {
        (u16::from(panel) * 2).min(255) as u8
    }

    /// One acceleration step for both cards.
    fn add_velocity(&mut self) {
        for pose in &mut self.poses {
            pose.vel[0] += pose.acc[0];
            pose.vel[1] += pose.acc[1];
        }
    }

    /// Reverse both cards' acceleration.
    fn reverse_accel(&mut self) {
        for pose in &mut self.poses {
            pose.acc = [-pose.acc[0], -pose.acc[1]];
        }
    }

    /// One position step for both cards.
    fn apply_position(&mut self) {
        for pose in &mut self.poses {
            pose.pos[0] += pose.vel[0];
            pose.pos[1] += pose.vel[1];
        }
    }

    /// Phase 3's scale/brightness ramp: the selected card grows and brightens,
    /// the other shrinks and dims, by ±2/±3 per frame.
    fn ramp_scale(&mut self) {
        let (front, back) = if self.selected == 0 { (0, 1) } else { (1, 0) };
        self.poses[front].scale += 2;
        self.poses[back].scale -= 2;
        self.poses[front].bright = self.poses[front].bright.saturating_sub(3);
        self.poses[back].bright = self.poses[back].bright.saturating_add(3);
    }

    /// The shared sub-2/3 entry: flip the draw order at the velocity zero
    /// crossing, ramp the scale, then either coast or enter the next phase.
    fn swap_sub_23(&mut self) {
        if self.poses[0].vel[0] == 0 {
            self.swap_sub = 3;
            self.poses[0].tpage ^= 1;
            self.poses[1].tpage ^= 1;
        }
        self.ramp_scale();
        if self.swap_timer == 0 {
            self.swap_sub_4_entry();
        } else {
            self.add_velocity();
        }
    }

    /// Enter the second movement phase and, when the direction needs no coast
    /// window, fall straight through into the final phase in this same tick
    /// (the original's case 3 -> case 4 fallthrough).
    fn swap_sub_4_entry(&mut self) {
        self.swap_sub = 4;
        self.add_velocity();
        self.swap_timer = (self.swap_dir << 2) as i8;
        if self.swap_timer == 0 {
            self.swap_sub = 5;
            self.swap_timer = 7;
            self.reverse_accel();
            self.add_velocity();
        }
    }

    /// One tick of the original's swap state machine.
    fn step_swap(&mut self) {
        self.swap_timer -= 1;
        match self.swap_sub {
            0 => {
                if self.swap_timer == 0 {
                    self.swap_sub = 1;
                    self.add_velocity();
                    self.swap_timer = ((self.swap_dir ^ 1) << 2) as i8;
                    if self.swap_timer == 0 {
                        self.swap_sub = 2;
                        self.swap_timer = 0x0F;
                        self.reverse_accel();
                        self.swap_sub_23();
                    }
                } else {
                    self.add_velocity();
                }
            }
            1 => {
                if self.swap_timer == 0 {
                    self.swap_sub = 2;
                    self.swap_timer = 0x0F;
                    self.reverse_accel();
                    self.swap_sub_23();
                }
            }
            2 => self.swap_sub_23(),
            3 => {
                self.ramp_scale();
                if self.swap_timer == 0 {
                    self.swap_sub_4_entry();
                } else {
                    self.add_velocity();
                }
            }
            4 => {
                if self.swap_timer == 0 {
                    self.swap_sub = 5;
                    self.swap_timer = 7;
                    self.reverse_accel();
                    self.add_velocity();
                }
            }
            5 => {
                if self.swap_timer == 0 {
                    self.selected ^= 1;
                    self.add_velocity();
                    self.apply_position();
                    self.stage = Stage::Idle;
                    return;
                }
                self.add_velocity();
            }
            _ => {}
        }
        self.apply_position();
    }

    /// Draw one card: the shared blue S.T.A.R.S. portrait half then this
    /// character's information half.
    ///
    /// The left half is always the sheet's row-0 portrait: both cards share
    /// it. Only the information half selects the character, at `(0x50, 0)`
    /// for Chris and `(0, 0x80)` for Jill; the sheet's other columns are
    /// padding and must not be sampled.
    fn draw_card(&self, framebuffer: &mut Framebuffer, card: usize) {
        let Some(cards) = &self.cards else {
            return;
        };
        let pose = self.poses[card];
        let x = pose.pos[0] - 0x100 + SCREEN_ORIGIN_X;
        let y = pose.pos[1] - 0x98 + SCREEN_ORIGIN_Y;
        let scale = Self::compute_scale(pose.scale);
        let brightness = Self::panel_brightness(pose.bright);
        let [info_x, info_y] = if card == 0 {
            [CARD_LEFT_W, 0]
        } else {
            [0, CARD_H]
        };
        let left_w = (CARD_LEFT_W * scale) >> 12;
        let right_w = (CARD_RIGHT_W * scale) >> 12;
        let height = (CARD_H * scale) >> 12;
        framebuffer.draw_rgba_sprite_scaled(
            cards,
            [0, 0, CARD_LEFT_W, CARD_H],
            [x, y, left_w, height],
            brightness,
        );
        framebuffer.draw_rgba_sprite_scaled(
            cards,
            [info_x, info_y, CARD_RIGHT_W, CARD_H],
            [x + left_w, y, right_w, height],
            brightness,
        );
    }

    /// Draw the black shadow the original lays over the back card.
    fn draw_shadow(&self, framebuffer: &mut Framebuffer, card: usize) {
        let pose = self.poses[card];
        let x = pose.pos[0] - 0x100 + SCREEN_ORIGIN_X;
        let y = pose.pos[1] - 0x98 + SCREEN_ORIGIN_Y;
        let scale = Self::compute_scale(pose.scale);
        framebuffer.blend_black_rect(
            [
                x,
                y,
                ((CARD_LEFT_W + CARD_RIGHT_W) * scale) >> 12,
                (CARD_H * scale) >> 12,
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
        // The card sheet's transparency is stored as exact black (the
        // original's BGR555 0x8000 cut-outs around the rounded corners and
        // the arrows); the converted 24-bit BMP cannot carry alpha, so key
        // black out here. The background sheet above keeps its black art.
        self.cards = match cx.pack.read("ui/select_b.bmp") {
            Ok(bytes) => match bmp::decode_mask(bytes) {
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
        self.poses = Self::initial_poses();
        self.swap_sub = 0;
        self.swap_timer = 0;
        self.swap_dir = 0;
        self.fade = Fade::fade_in(8, 0x7FFF, 2);
        self.hold_timer = 0;
        self.ticks = 0;
        self.stage = Stage::Idle;
        self.exit = None;
        Ok(())
    }

    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
        self.ticks = self.ticks.saturating_add(1);
        match self.stage {
            Stage::Idle => {
                self.fade.tick();
                if input.left || input.right {
                    cx.play_cue(UiCue::SelectCursor);
                    // The original: left (`g_selSwapDir` 1) or right (0). It
                    // enters the animation in the same tick (`goto case_2`).
                    self.start_swap(u8::from(input.left));
                    self.step_swap();
                } else if input.confirm {
                    cx.play_cue(UiCue::SelectConfirm);
                    self.exit = Some(ScreenAction::NewGame {
                        character: self.selected,
                    });
                    // The original's state 3: the white flash rises two alpha
                    // steps a tick for 128 ticks (`set_fading(1, 0x100)`).
                    self.fade = Fade::fade_out(2, 0, 1);
                    self.stage = Stage::ConfirmFadeIn;
                } else if input.cancel {
                    // The original's cancel is silent and black-fades
                    // (`set_fading(2, 0xC00)`), ~11 ticks.
                    self.exit = Some(ScreenAction::Title);
                    self.fade = Fade::fade_out(24, 0, 2);
                    self.stage = Stage::CancelFadeOut;
                }
            }
            Stage::Swapping => self.step_swap(),
            Stage::ConfirmFadeIn => {
                self.fade.tick();
                if !self.fade.is_active() {
                    // The original's state 4: the full white decays from
                    // `0x7FFF` at two alpha steps a tick.
                    self.fade = Fade::fade_in(2, 0x7FFF, 1);
                    self.stage = Stage::ConfirmFadeOut;
                }
            }
            Stage::ConfirmFadeOut => {
                self.fade.tick();
                if !self.fade.is_active() {
                    self.stage = Stage::ConfirmHold;
                    self.hold_timer = 240;
                }
            }
            Stage::ConfirmHold => {
                self.hold_timer = self.hold_timer.saturating_sub(1);
                if self.hold_timer == 0 {
                    return ScreenResult::Done(self.exit.take().unwrap_or(ScreenAction::Title));
                }
            }
            Stage::CancelFadeOut => {
                self.fade.tick();
                if !self.fade.is_active() {
                    return ScreenResult::Done(self.exit.take().unwrap_or(ScreenAction::Title));
                }
            }
        }
        ScreenResult::Continue
    }

    fn draw(&mut self, _cx: &UiContext<'_>, framebuffer: &mut Framebuffer) {
        // The original blanks the background and both cards the instant the
        // confirm flash fills the screen: state 4 decays the white over flat
        // black, and the standalone cancel fade runs over black too. The
        // caller's overlay supplies the fading colour.
        if matches!(
            self.stage,
            Stage::ConfirmFadeOut | Stage::ConfirmHold | Stage::CancelFadeOut
        ) {
            framebuffer.clear();
            framebuffer.fill_rect(
                [0, 0, framebuffer.width as i32, framebuffer.height as i32],
                [0, 0, 0, 255],
            );
            return;
        }
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
        // The cards keep drawing while the white flash rises; they stop as
        // soon as it completes, and the arrows stop the moment confirm is
        // pressed (the original only draws them in its idle state).
        if matches!(
            self.stage,
            Stage::Idle | Stage::Swapping | Stage::ConfirmFadeIn
        ) {
            // The back card draws first, then its shadow, then the front card.
            // The tpage owns the order, so the flip mid-slide changes it.
            let (first, second) = if self.poses[0].in_front_of(&self.poses[1]) {
                (1, 0)
            } else {
                (0, 1)
            };
            self.draw_card(framebuffer, first);
            self.draw_shadow(framebuffer, first);
            self.draw_card(framebuffer, second);
        }
        if self.stage == Stage::Idle {
            self.draw_cursors(framebuffer);
        }
    }

    fn overlay(&self) -> Option<(crate::transition::FadeColor, u8)> {
        self.fade
            .overlay()
            .map(|overlay| (overlay.color, overlay.alpha))
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

    /// Run the active slide to its end (the machine takes `SWAP_TICKS`).
    fn settle(screen: &mut CharSelectScreen) {
        let cx = context();
        for _ in 0..SWAP_TICKS {
            screen.update(&cx, neutral());
        }
    }

    #[test]
    fn left_and_right_swap_the_pick_when_the_slide_lands() {
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
        assert!(screen.swapping(), "right starts the slide");
        assert_eq!(screen.selected(), 0, "the pick flips only at the end");
        settle(&mut screen);
        assert!(!screen.swapping());
        assert_eq!(screen.selected(), 1);
        screen.update(
            &cx,
            UiInput {
                left: true,
                ..neutral()
            },
        );
        settle(&mut screen);
        assert_eq!(screen.selected(), 0);
    }

    #[test]
    fn the_slide_follows_the_original_frame_trace() {
        let mut screen = CharSelectScreen::new();
        let cx = context();
        screen.update(
            &cx,
            UiInput {
                right: true,
                ..neutral()
            },
        );
        // Frame 1: card 0 left the front pose by one acceleration step.
        assert_eq!(screen.pose(0), (138, 73, 0xF0, 0x80, 2));
        assert_eq!(screen.pose(1), (198, 103, 0x110, 0x50, 3));
        // Frames 2..: the overshoot builds toward x = 312.
        screen.update(&cx, neutral());
        assert_eq!(screen.pose(0), (142, 75, 0xF0, 0x80, 2));
        for _ in 0..8 {
            screen.update(&cx, neutral());
        }
        // Frame 10.
        assert_eq!(screen.pose(0), (240, 124, 0xF0, 0x80, 2));
        for _ in 0..5 {
            screen.update(&cx, neutral());
        }
        // Frame 15: the scale/brightness ramp is running.
        assert_eq!(screen.pose(0), (300, 154, 0xF8, 0x74, 2));
        for _ in 0..5 {
            screen.update(&cx, neutral());
        }
        // Frame 20: the velocity crosses zero and the tpage flips.
        assert_eq!(screen.pose(0), (310, 159, 0x102, 0x65, 3));
        assert_eq!(screen.pose(1), (26, 17, 0xFE, 0x6B, 2));
        for _ in 0..13 {
            screen.update(&cx, neutral());
        }
        // Frame 33: the cards sit on their final poses.
        assert_eq!(screen.pose(0), (200, 104, 0x110, 0x50, 3));
        assert_eq!(screen.pose(1), (136, 72, 0xF0, 0x80, 2));
        // Frame 34 flips the pick and holds the same poses.
        screen.update(&cx, neutral());
        assert!(!screen.swapping());
        assert_eq!(screen.selected(), 1);
        assert_eq!(screen.pose(0), (200, 104, 0x110, 0x50, 3));
        assert_eq!(screen.pose(1), (136, 72, 0xF0, 0x80, 2));
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
        settle(&mut screen);
        assert_eq!(screen.selected(), 1);
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        let mut result = ScreenResult::Continue;
        let mut ticks = 0usize;
        loop {
            result = screen.update(&cx, neutral());
            ticks += 1;
            if result != ScreenResult::Continue {
                break;
            }
            assert!(ticks < 700, "the confirm chain never finished");
        }
        // 128 fade-in + 128 fade-out + the 240-tick `g_selTimer` hold, matching
        // the reference's case 6 at frame 496.
        assert_eq!(ticks, 496, "the confirm chain's fixed length");
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
        assert_eq!(cx.cues.borrow().as_slice(), &[UiCue::SelectCursor]);
        cx.cues.borrow_mut().clear();
        settle(&mut screen);
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(cx.cues.borrow().as_slice(), &[UiCue::SelectConfirm]);

        // Cancel is silent in the original.
        cx.cues.borrow_mut().clear();
        screen.update(
            &cx,
            UiInput {
                cancel: true,
                ..neutral()
            },
        );
        assert!(cx.cues.borrow().is_empty(), "cancel must not queue a cue");
    }

    /// The white overlay alpha at the current tick, or `None` when clear.
    fn white(screen: &CharSelectScreen) -> Option<u8> {
        match screen.overlay() {
            Some((crate::transition::FadeColor::White, alpha)) => Some(alpha),
            Some((crate::transition::FadeColor::Black, _)) => panic!("expected a white overlay"),
            None => None,
        }
    }

    #[test]
    fn confirm_runs_the_white_flash_chain_before_the_new_game() {
        let mut screen = CharSelectScreen::new();
        let cx = context();
        // Skip the opening black fade.
        for _ in 0..40 {
            screen.update(&cx, neutral());
        }
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..neutral()
            },
        );
        assert_eq!(white(&screen), Some(0), "the confirm tick starts clear");
        assert_eq!(screen.stage, Stage::ConfirmFadeIn);

        // The flash rises two alpha steps a tick; the 128th tick wraps the
        // 16-bit accumulator and hands over to the fade-out.
        screen.update(&cx, neutral());
        assert_eq!(white(&screen), Some(2));
        for _ in 0..126 {
            screen.update(&cx, neutral());
        }
        assert_eq!(white(&screen), Some(254));
        assert_eq!(screen.stage, Stage::ConfirmFadeIn);
        screen.update(&cx, neutral());
        assert_eq!(screen.stage, Stage::ConfirmFadeOut);
        assert_eq!(white(&screen), Some(255), "the fade-out starts from full");

        // The white decays two alpha steps a tick for 128 ticks.
        screen.update(&cx, neutral());
        assert_eq!(white(&screen), Some(253));
        for _ in 0..126 {
            screen.update(&cx, neutral());
        }
        assert_eq!(white(&screen), Some(1));
        assert_eq!(screen.stage, Stage::ConfirmFadeOut);

        // The 240-tick hold runs with the frame clear: `g_selTimer` starts at
        // -16 and decrements to zero through the signed-byte wrap.
        screen.update(&cx, neutral());
        assert_eq!(screen.stage, Stage::ConfirmHold);
        assert_eq!(screen.overlay(), None);
        let mut result = ScreenResult::Continue;
        for _ in 0..240 {
            result = screen.update(&cx, neutral());
            if result != ScreenResult::Continue {
                break;
            }
        }
        assert_eq!(
            result,
            ScreenResult::Done(ScreenAction::NewGame { character: 0 })
        );
    }

    #[test]
    fn cancel_black_fades_in_eleven_ticks_without_a_cue() {
        let mut screen = CharSelectScreen::new();
        let cx = context();
        for _ in 0..40 {
            screen.update(&cx, neutral());
        }
        screen.update(
            &cx,
            UiInput {
                cancel: true,
                ..neutral()
            },
        );
        assert_eq!(screen.stage, Stage::CancelFadeOut);
        assert_eq!(
            screen.overlay(),
            Some((crate::transition::FadeColor::Black, 0))
        );
        screen.update(&cx, neutral());
        assert_eq!(
            screen.overlay(),
            Some((crate::transition::FadeColor::Black, 24))
        );
        for _ in 0..9 {
            screen.update(&cx, neutral());
        }
        assert_eq!(
            screen.overlay(),
            Some((crate::transition::FadeColor::Black, 240))
        );
        let result = screen.update(&cx, neutral());
        assert_eq!(result, ScreenResult::Done(ScreenAction::Title));
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

    /// A card sheet with a distinct colour per usable region and opaque black
    /// padding everywhere else, matching the converted 24-bit BMP.
    fn card_sheet() -> Image {
        let mut rgba = vec![0u8; 256 * 256 * 4];
        for pixel in rgba.as_chunks_mut::<4>().0.iter_mut() {
            *pixel = [0, 0, 0, 255];
        }
        let mut fill = |x: usize, y: usize, w: usize, h: usize, color: [u8; 4]| {
            for row in y..y + h {
                for column in x..x + w {
                    let offset = (row * 256 + column) * 4;
                    rgba[offset..offset + 4].copy_from_slice(&color);
                }
            }
        };
        fill(
            0,
            0,
            CARD_LEFT_W as usize,
            CARD_H as usize,
            [10, 10, 200, 255],
        );
        fill(
            CARD_LEFT_W as usize,
            0,
            CARD_RIGHT_W as usize,
            CARD_H as usize,
            [30, 200, 30, 255],
        );
        fill(
            0,
            CARD_H as usize,
            CARD_RIGHT_W as usize,
            CARD_H as usize,
            [200, 30, 30, 255],
        );
        Image {
            width: 256,
            height: 256,
            rgba,
        }
    }

    #[test]
    fn each_card_draws_the_shared_portrait_half_with_its_own_information_half() {
        let mut screen = CharSelectScreen::new();
        screen.cards = Some(card_sheet());
        // Force both cards onto the front pose so the sample points are fixed;
        // the slide machine is not part of this test.
        screen.poses[1] = CardPose {
            pos: [FRONT_POS.0, FRONT_POS.1],
            scale: FRONT_PANEL_SCALE,
            bright: FRONT_PANEL_BRIGHT,
            tpage: 2,
            vel: [0; 2],
            acc: [0; 2],
        };
        let x = FRONT_POS.0 - 0x100 + SCREEN_ORIGIN_X;
        let y = FRONT_POS.1 - 0x98 + SCREEN_ORIGIN_Y;
        let sample = |framebuffer: &Framebuffer, dx: i32, dy: i32| -> [u8; 4] {
            let offset = ((y + dy) as usize * framebuffer.width as usize + (x + dx) as usize) * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        };

        // Both cards sample the sheet's row-0 shared portrait half.
        let mut chris = Framebuffer::new();
        screen.draw_card(&mut chris, 0);
        assert_eq!(sample(&chris, 8, 8), [10, 10, 200, 255]);
        assert_eq!(sample(&chris, 0x50 + 8, 8), [30, 200, 30, 255]);
        assert_eq!(
            sample(&chris, 0x50 + 0x70 - 1, 8),
            [30, 200, 30, 255],
            "the last column is information, not padding"
        );

        let mut jill = Framebuffer::new();
        screen.draw_card(&mut jill, 1);
        assert_eq!(sample(&jill, 8, 8), [10, 10, 200, 255]);
        assert_eq!(sample(&jill, 0x50 + 8, 8), [200, 30, 30, 255]);
        assert_eq!(
            sample(&jill, 0x50 + 0x70 - 1, 8),
            [200, 30, 30, 255],
            "Jill's card must not sample the sheet padding"
        );
    }

    /// The confirm fade-out/hold and the standalone cancel fade blank the
    /// background and both cards, so only the caller's overlay shows.
    #[test]
    fn the_confirm_and_cancel_fades_draw_over_black() {
        let mut screen = CharSelectScreen::new();
        let cx = context();
        screen.background = Some(card_sheet());
        screen.cards = Some(card_sheet());
        for stage in [
            Stage::ConfirmFadeOut,
            Stage::ConfirmHold,
            Stage::CancelFadeOut,
        ] {
            screen.stage = stage;
            let mut framebuffer = Framebuffer::new();
            screen.draw(&cx, &mut framebuffer);
            assert!(
                framebuffer
                    .rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| pixel[..3] == [0, 0, 0]),
                "{stage:?} must blank the background and cards"
            );
        }
        // The idle stage still draws the desk and the cards.
        screen.stage = Stage::Idle;
        let mut framebuffer = Framebuffer::new();
        screen.draw(&cx, &mut framebuffer);
        assert!(
            framebuffer
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[..3] != [0, 0, 0]),
            "the idle stage keeps its art"
        );
    }
}
