//! The character-selection screen.
//!
//! `ui/sel_back.bmp` is the desk background. The two police cards come from
//! `ui/select_b.bmp`: each card is a left 80x128 half and a right 112x128 half,
//! with Chris on row 0 and Jill on row `0x80`. The selected card sits at its
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
    FadeOut,
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
            poses: Self::initial_poses(),
            swap_sub: 0,
            swap_timer: 0,
            swap_dir: 0,
            fade: 255,
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
        (pose.pos[0], pose.pos[1], pose.scale, pose.bright, pose.tpage)
    }

    /// Start the swap animation toward `dir` (`0` right, `1` left, the
    /// original's `g_selSwapDir`). The pick itself flips when the slide lands.
    pub fn start_swap(&mut self, dir: u8) {
        let dir = dir & 1;
        let (ax, ay) = if dir == self.selected { (2, 1) } else { (-2, -1) };
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

    /// Draw one card: its left portrait half then its information half.
    fn draw_card(&self, framebuffer: &mut Framebuffer, card: usize) {
        let Some(cards) = &self.cards else {
            return;
        };
        let pose = self.poses[card];
        let x = pose.pos[0] - 0x100 + SCREEN_ORIGIN_X;
        let y = pose.pos[1] - 0x98 + SCREEN_ORIGIN_Y;
        let scale = Self::compute_scale(pose.scale);
        let brightness = Self::panel_brightness(pose.bright);
        let row = card as i32 * CARD_H;
        let left_w = (CARD_LEFT_W * scale) >> 12;
        let right_w = (CARD_RIGHT_W * scale) >> 12;
        let height = (CARD_H * scale) >> 12;
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
        self.poses = Self::initial_poses();
        self.swap_sub = 0;
        self.swap_timer = 0;
        self.swap_dir = 0;
        self.fade = 255;
        self.ticks = 0;
        self.stage = Stage::Idle;
        self.exit = None;
        Ok(())
    }

    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
        self.ticks = self.ticks.saturating_add(1);
        match self.stage {
            Stage::Idle => {
                self.fade = self.fade.saturating_sub(8);
                if input.left || input.right {
                    cx.play_cue(UiCue::Cursor);
                    // The original: left (`g_selSwapDir` 1) or right (0). It
                    // enters the animation in the same tick (`goto case_2`).
                    self.start_swap(u8::from(input.left));
                    self.step_swap();
                } else if input.confirm {
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
            Stage::Swapping => self.step_swap(),
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
        // The back card draws first, then its shadow, then the front card. The
        // tpage owns the order, so the flip mid-slide changes it.
        let (first, second) = if self.poses[0].in_front_of(&self.poses[1]) {
            (1, 0)
        } else {
            (0, 1)
        };
        self.draw_card(framebuffer, first);
        self.draw_shadow(framebuffer, first);
        self.draw_card(framebuffer, second);
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
        settle(&mut screen);
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
