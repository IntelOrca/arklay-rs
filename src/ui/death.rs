//! The player death sequence and the game-over ("YOU DIED") screen.
//!
//! A player whose health goes below zero raises the dead flag; the original's
//! main loop then runs a small game-over machine that waits 90 frames (the
//! fall), fades the screen to white, and opens the DIED screen. The screen
//! shows the fallen body over the `died.tim` static backdrop with the
//! sine-wavy "YOU DIED" strip, then fades back out and returns to the title.
//!
//! # The game-over machine
//!
//! [`DeathSequence`] mirrors the original's death machine: state 0 triggers
//! (the Yawn rooms and the water tank skip the delay), state 1 counts the
//! 90-frame delay down, state 2 arms the white fade, state 3 waits for the
//! fade to wrap negative and then opens the screen. A room that is running
//! the attract demo or the scripted death variant skips the screen entirely.
//!
//! # The DIED screen
//!
//! [`DeathScreen`] is the original's display state machine, stepped once per
//! 30 Hz tick (the original stepped it once per game frame):
//!
//! - 0 arms the white flash (`counter 0xFE80`, accumulator `0x7FFF`);
//! - 1 waits for the flash to clear (~86 frames);
//! - 2 ramps the black overlay 3 per frame and, once it passes `0x3F`, draws
//!   and steps the wave amplitude down from 59;
//! - 3 holds the settled strip for 49 frames;
//! - 4 arms the black fade-out (`counter 0x180`);
//! - 5 re-waves the strip while the fade runs, then finishes.
//!
//! The body spins 8 angle units per frame in front of the fixed camera the
//! screen builds from the death position; the caller applies
//! [`DeathScreen::spin`] to the player's yaw each tick.
//!
//! # The strip math
//!
//! `died.tim` is an 8bpp page: rows 0..0x40 hold the red "YOU DIED" art and
//! rows 0x40..0xB8 the grey static backdrop. The backdrop is drawn as four
//! 160x120 quadrants of the same region, mirrored per quadrant so the static
//! tiles seamlessly. The strip is 256 one-pixel columns; each column is
//! displaced by `sin(phase) * param^2 >> 13` with the phase stepping `0x100`
//! per column, and the whole column sits `baseY` above the screen centre.
//! At the entry amplitude (59) the wave is ~1700px, so half the columns fly
//! off the top and half off the bottom; the two bands slam together and
//! settle into the 16-ripple wave.

use crate::model::Texture8;
use crate::render::Framebuffer;
use crate::state::RoomId;
use crate::transition::{Fade, Overlay};

/// Frames the game-over machine waits before the fade in an ordinary room.
pub const DEATH_DELAY: u8 = 0x5A;

/// The pre-screen fade counter (a 2-alpha-per-frame white-out).
const DEATH_FADE_OUT_COUNTER: i32 = 0x100;
/// The DIED screen's own white-flash counter (i16 -384, decays from 0x7FFF).
const SCREEN_FLASH_COUNTER: i32 = 0xFE80;
/// The DIED screen's black fade-out counter.
const SCREEN_FADE_COUNTER: i32 = 0x180;
/// Frames the settled strip is held (the original's `0x30` comparison).
const HOLD_FRAMES: u8 = 0x30;
/// The black overlay ramp step.
const RECT_RAMP: i16 = 3;
/// The overlay level past which the wave amplitude starts stepping down.
const RECT_STEP_THRESHOLD: i16 = 0x3F;
/// The overlay's ceiling: the last written colour stays when the ramp passes
/// it, exactly like the original's `< 0xFF` guard.
const RECT_MAX: i16 = 0xFF;
/// The wave amplitude at entry and the value the settled strip holds.
const WAVE_START: i16 = 0x3B;
/// The amplitude below which the hold begins.
const WAVE_SETTLED: i16 = 4;

/// Stage digit of the second mansion floor (the Yawn rooms).
const STAGE_MANSION_2F: u8 = 2;
/// The attic, where the first Yawn's death skips the delay.
const ROOM_ATTIC: u8 = 0x10;
/// Stage digit of the second mansion return.
const STAGE_MANSION_RETURN_2F: u8 = 7;
/// The lesson room, where the second Yawn's death skips the delay.
const ROOM_LESSON: u8 = 0x0C;
/// Stage digit of the guardhouse.
const STAGE_GUARDHOUSE: u8 = 4;
/// The water tank, where the Neptune death skips the delay.
const ROOM_WATER_TANK: u8 = 0x0E;
/// The armor room, whose scripted death skips the blood pool.
const ROOM_ARMOR: u8 = 0x05;
/// The drug storehouse, whose scripted death skips the blood pool.
const ROOM_DRUG_STOREHOUSE: u8 = 0x09;
/// Stage digit of the laboratory.
const STAGE_LABORATORY: u8 = 5;
/// The morgue, whose scripted death skips the blood pool.
const ROOM_MORGUE: u8 = 0x07;

/// Whether this room's death fades immediately instead of running the
/// 90-frame delay (the Yawn rooms and the Neptune water tank).
pub fn immediate_fade(id: RoomId) -> bool {
    (id.stage == STAGE_MANSION_2F && id.room == ROOM_ATTIC)
        || (id.stage == STAGE_MANSION_RETURN_2F && id.room == ROOM_LESSON)
        || (id.stage == STAGE_GUARDHOUSE && id.room == ROOM_WATER_TANK)
}

/// Whether this room's scripted death skips the blood pool and hands the
/// player straight to the blocked state (the armor room, the drug storehouse
/// and the morgue).
pub fn skips_blood_pool(id: RoomId) -> bool {
    (id.stage == STAGE_MANSION_2F && id.room == ROOM_ARMOR)
        || (id.stage == STAGE_GUARDHOUSE && id.room == ROOM_DRUG_STOREHOUSE)
        || (id.stage == STAGE_LABORATORY && id.room == ROOM_MORGUE)
}

/// What one game-over tick produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeathTick {
    /// Keep running the delay or the fade.
    None,
    /// The fade completed; the DIED screen should open.
    OpenScreen,
    /// The variant path skipped the screen; the title should return.
    Finished,
}

/// The game-over machine (the original's death-state counter, delay and fade).
#[derive(Debug, Clone)]
pub struct DeathSequence {
    /// 0 idle, 1 delay, 2 arm the fade, 3 fading, 4 screen, 5 done.
    state: u8,
    /// The delay countdown.
    delay: u8,
    /// The attract/demo or scripted death variant: no DIED screen.
    variant: bool,
    /// The death fade (white to the screen, black out of it).
    fade: Fade,
    /// The open DIED screen, once the fade completed.
    pub screen: Option<DeathScreen>,
    /// The sequence ended; the caller should return to the title.
    pub finished: bool,
}

impl Default for DeathSequence {
    fn default() -> Self {
        Self::idle()
    }
}

impl DeathSequence {
    /// A sequence that has not been triggered.
    pub const fn idle() -> Self {
        Self {
            state: 0,
            delay: 0,
            variant: false,
            fade: Fade::inactive(),
            screen: None,
            finished: false,
        }
    }

    /// Whether the sequence has not been triggered yet.
    pub fn is_idle(&self) -> bool {
        self.state == 0
    }

    /// Whether the DIED screen owns the tick.
    pub fn screen_open(&self) -> bool {
        self.screen.is_some()
    }

    /// Trigger the sequence: an immediate-fade room arms the white fade at
    /// once, every other room starts the 90-frame delay. `variant` is the
    /// attract-demo/scripted-death flag, which skips the screen.
    pub fn start(&mut self, immediate: bool, variant: bool) {
        self.variant = variant;
        if immediate || variant {
            self.arm_fade(1, DEATH_FADE_OUT_COUNTER);
            self.state = 3;
        } else {
            self.delay = DEATH_DELAY;
            self.state = 1;
        }
    }

    /// Advance the machine one tick (before the screen opens).
    pub fn tick(&mut self) -> DeathTick {
        match self.state {
            1 => {
                if self.delay == 0 {
                    self.arm_fade(1, DEATH_FADE_OUT_COUNTER);
                    self.state = 3;
                } else {
                    self.delay -= 1;
                    return DeathTick::None;
                }
            }
            2 => {
                self.arm_fade(1, DEATH_FADE_OUT_COUNTER);
                self.state = 3;
            }
            3 => {
                self.fade.tick();
                if self.fade.state() < 0 {
                    self.state = 4;
                    if self.variant {
                        self.finished = true;
                        return DeathTick::Finished;
                    }
                    return DeathTick::OpenScreen;
                }
            }
            _ => {}
        }
        DeathTick::None
    }

    /// Arm a fade the way the original's `fade_update` does: a positive
    /// counter ramps up from zero, a negative one decays from `0x7FFF`.
    fn arm_fade(&mut self, fade_type: u8, counter: i32) {
        let state = if counter <= 0 { 0x7FFF } else { 0 };
        self.fade.set(fade_type, counter, state);
    }

    /// The full-screen overlay the death fade draws, if any.
    pub fn overlay(&self) -> Option<Overlay> {
        self.fade.overlay()
    }
}

/// The DIED screen state machine and its fixed camera.
#[derive(Debug, Clone)]
pub struct DeathScreen {
    /// 0..=6, the original's display state.
    state: u8,
    /// The wave amplitude index.
    image_idx: i16,
    /// The hold-state frame counter.
    hold: u8,
    /// The last overlay colour written (the original keeps it once the ramp
    /// passes `0xFF`).
    rect: i16,
    /// The ramp counter.
    rect_color: i16,
    /// The amplitude the strip is drawn with this frame, if any.
    strip: Option<i16>,
    /// The screen's own fade (the white flash and the final fade-out).
    fade: Fade,
    /// The corpse's yaw; the caller adds 8 per tick.
    pub spin: u16,
    /// The fixed camera's eye.
    pub camera_from: [i32; 3],
    /// The fixed camera's look-at point.
    pub camera_to: [i32; 3],
    /// The focal length the screen renders with (the room's camera).
    pub fov: i32,
    /// The screen finished; the title should return.
    pub finished: bool,
}

impl DeathScreen {
    /// Open the screen over a corpse at `anchor` facing `angle`, with the
    /// room camera's focal length. The camera looks down at the body from
    /// 7000 units above, 1000 units short of its height. The first frame
    /// arms the white flash, exactly like the original's entry into state 0.
    pub fn open(anchor: [i32; 3], angle: u16, fov: i32) -> Self {
        let mut screen = Self {
            state: 1,
            image_idx: WAVE_START,
            hold: 0,
            rect: 0,
            rect_color: 0,
            strip: None,
            fade: Fade::inactive(),
            spin: angle,
            camera_from: [
                anchor[0].wrapping_add(5000),
                anchor[1].wrapping_sub(7000),
                anchor[2],
            ],
            camera_to: [anchor[0], anchor[1].wrapping_sub(1000), anchor[2]],
            fov,
            finished: false,
        };
        screen.fade.set(1, SCREEN_FLASH_COUNTER, 0x7FFF);
        screen
    }

    /// Advance the screen one tick; returns whether it finished.
    pub fn tick(&mut self, confirm: bool) -> bool {
        // Only the states that draw the strip set it; every other state
        // leaves the frame without one.
        self.strip = None;
        let mut armed = false;
        match self.state {
            0 => {
                self.fade.set(1, SCREEN_FLASH_COUNTER, 0x7FFF);
                armed = true;
                self.state = 1;
            }
            1 => {
                if self.fade.state() < 0 {
                    self.state = 2;
                }
            }
            2 => {
                if self.rect_color < RECT_MAX {
                    self.rect = self.rect_color;
                }
                self.rect_color = self.rect_color.wrapping_add(RECT_RAMP);
                if self.rect_color > RECT_STEP_THRESHOLD {
                    // The strip draws with the pre-step amplitude, then the
                    // amplitude steps down.
                    self.strip = Some(self.image_idx);
                    self.image_idx -= 1;
                }
                if self.image_idx < WAVE_SETTLED {
                    self.state = 3;
                }
            }
            3 => {
                self.strip = Some(self.image_idx);
                self.hold += 1;
                if self.hold > HOLD_FRAMES {
                    self.state = 4;
                    self.hold = 0;
                }
            }
            4 => {
                self.fade.set(2, SCREEN_FADE_COUNTER, 0);
                armed = true;
                self.image_idx = WAVE_SETTLED;
                self.state = 5;
            }
            5 => {
                self.strip = Some(self.image_idx);
                self.image_idx += 1;
                if self.fade.state() < 0 {
                    self.finished = true;
                }
            }
            6 if self.fade.state() < 0 => {
                self.finished = true;
            }
            _ => {}
        }
        if !armed && self.fade.state() >= 0 {
            self.fade.tick();
        }
        // The confirm button skips the rest of the screen at any state but 0.
        if confirm && self.state != 0 {
            self.fade.set(2, 0x800, 0);
            self.finished = true;
        }
        self.finished
    }

    /// The strip amplitude to draw this frame, if any.
    pub fn strip_param(&self) -> Option<i16> {
        self.strip
    }

    /// The black overlay's alpha.
    pub fn rect_alpha(&self) -> u8 {
        self.rect.clamp(0, 255) as u8
    }

    /// The screen's own fade overlay.
    pub fn overlay(&self) -> Option<Overlay> {
        self.fade.overlay()
    }
}

/// The original's fix12 sine: `(int)(sin(angle * 2^-12 * 2pi) * 4096)`,
/// truncated toward zero.
pub fn fsin(angle: i32) -> i32 {
    ((f64::from(angle) * (1.0 / 4096.0) * 6.283_185_482_025_146_5).sin() * 4096.0) as i32
}

/// The strip's baseline for amplitude `param`: the negative screen-space Y
/// (centre-origin) of the column top before the wave.
pub fn strip_base(param: i16) -> i16 {
    let param = i32::from(param);
    let squared = param * 8 * param * 8;
    let u_var1 = ((squared >> 4) + 0x1600) as u16;
    let base = (u32::from(u_var1) * 0x40) >> 0xD;
    (base as u16).wrapping_neg() as i16
}

/// One column's wave displacement for amplitude `param`; the column's phase
/// is `column * 0x100`, sixteen ripples across the 256 columns.
pub fn strip_wave(param: i16, column: usize) -> i16 {
    let sin = fsin(column as i32 * 0x100);
    let product = sin.wrapping_mul(i32::from(param) * i32::from(param)) as u32;
    (product >> 0xD) as u16 as i16
}

/// One `died.tim` texel, or `None` for the colour-keyed black.
fn texel(texture: &Texture8, u: u32, v: u32) -> Option<[u8; 4]> {
    if u >= texture.width || v >= texture.height {
        return None;
    }
    let index = texture.indices[(v * texture.width + u) as usize];
    let colour = texture
        .palettes
        .get(usize::from(index))
        .copied()
        .unwrap_or([0, 0, 0, 0]);
    if colour[0] == 0 && colour[1] == 0 && colour[2] == 0 {
        return None;
    }
    Some(colour)
}

/// Write one opaque texel into the framebuffer.
fn put(framebuffer: &mut Framebuffer, x: i32, y: i32, colour: [u8; 4]) {
    if x < 0 || y < 0 || x >= framebuffer.width as i32 || y >= framebuffer.height as i32 {
        return;
    }
    let offset = ((y as u32 * framebuffer.width + x as u32) * 4) as usize;
    framebuffer.rgba[offset..offset + 4].copy_from_slice(&colour);
}

/// The backdrop's four quadrants: the same 160x120 region at (0, 0x40) of the
/// page, mirrored per quadrant and drawn at the four screen quarters.
const QUADRANTS: [(i32, i32, bool, bool); 4] = [
    (0, 0, false, false),
    (0, 120, false, true),
    (160, 0, true, false),
    (160, 120, true, true),
];

/// Draw the `died.tim` static backdrop: four mirrored 160x120 quadrants of
/// the page's lower region, colour-keyed on black.
pub fn draw_backdrop(framebuffer: &mut Framebuffer, texture: &Texture8) {
    for (origin_x, origin_y, mirror_u, mirror_v) in QUADRANTS {
        for y in 0..120u32 {
            let v = if mirror_v { 119 - y } else { y } + 0x40;
            for x in 0..160u32 {
                let u = if mirror_u { 159 - x } else { x };
                if let Some(colour) = texel(texture, u, v) {
                    put(
                        framebuffer,
                        origin_x + x as i32,
                        origin_y + y as i32,
                        colour,
                    );
                }
            }
        }
    }
}

/// Draw the wavy strip: 256 one-pixel columns of the page's top region, each
/// displaced by its sine phase.
pub fn draw_strip(framebuffer: &mut Framebuffer, texture: &Texture8, param: i16) {
    let base = strip_base(param);
    for column in 0..256usize {
        let top = base.wrapping_sub(strip_wave(param, column)) as i32 + 120;
        let x = column as i32 + 32;
        for row in 0..0x40u32 {
            if let Some(colour) = texel(texture, column as u32, row) {
                put(framebuffer, x, top + row as i32, colour);
            }
        }
    }
}

/// Blend the black overlay the state-2 ramp builds over everything beneath.
pub fn draw_rect(framebuffer: &mut Framebuffer, alpha: u8) {
    if alpha == 0 {
        return;
    }
    framebuffer.blend_rect(
        [0, 0, framebuffer.width as i32, framebuffer.height as i32],
        [0, 0, 0],
        alpha,
    );
}

/// Blend the screen's own fade (the white flash or the final fade-out).
pub fn draw_overlay(framebuffer: &mut Framebuffer, overlay: Option<Overlay>) {
    if let Some(overlay) = overlay {
        framebuffer.fade_to_color(overlay.color, overlay.alpha);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transition::FadeColor;

    fn room(text: &str) -> RoomId {
        RoomId::parse(text).unwrap()
    }

    #[test]
    fn the_room_variants_dispatch_by_room() {
        // The Yawn rooms and the water tank fade immediately.
        assert!(immediate_fade(room("210")));
        assert!(immediate_fade(room("70C")));
        assert!(immediate_fade(room("40E")));
        assert!(!immediate_fade(room("100")));
        assert!(!immediate_fade(room("205")));
        // The scripted deaths skip the blood pool.
        assert!(skips_blood_pool(room("205")));
        assert!(skips_blood_pool(room("409")));
        assert!(skips_blood_pool(room("507")));
        assert!(!skips_blood_pool(room("210")));
        assert!(!skips_blood_pool(room("100")));
    }

    #[test]
    fn an_ordinary_death_waits_ninety_frames_then_fades() {
        let mut sequence = DeathSequence::idle();
        sequence.start(false, false);
        assert_eq!(sequence.delay, DEATH_DELAY);
        // The 90 delay ticks produce nothing and leave the fade inactive.
        for _ in 0..DEATH_DELAY {
            assert_eq!(sequence.tick(), DeathTick::None);
            assert!(sequence.overlay().is_none());
        }
        // The next tick arms the white fade at alpha zero.
        assert_eq!(sequence.tick(), DeathTick::None);
        let overlay = sequence.overlay().unwrap();
        assert_eq!(overlay.color, FadeColor::White);
        assert_eq!(overlay.alpha, 0);
        // 128 adds of 0x100 reach the alpha ceiling, then the accumulator
        // wraps negative and the screen opens.
        let mut opened = None;
        for tick in 0..200 {
            if sequence.tick() == DeathTick::OpenScreen {
                opened = Some(tick);
                break;
            }
        }
        let opened = opened.expect("the fade completes");
        // The first 127 ticks ramp (the last drawn alpha is 254); the 128th
        // wraps negative.
        assert_eq!(opened, 127);
    }

    #[test]
    fn an_immediate_room_arms_the_fade_without_the_delay() {
        let mut sequence = DeathSequence::idle();
        sequence.start(true, false);
        let overlay = sequence.overlay().unwrap();
        assert_eq!(overlay.color, FadeColor::White);
        assert_eq!(overlay.alpha, 0);
    }

    #[test]
    fn the_variant_path_skips_the_screen() {
        let mut sequence = DeathSequence::idle();
        sequence.start(false, true);
        for _ in 0..300 {
            if sequence.tick() == DeathTick::Finished {
                break;
            }
        }
        assert!(sequence.finished);
        assert!(sequence.screen.is_none());
    }

    #[test]
    fn the_screen_machine_runs_its_states_in_order() {
        let mut screen = DeathScreen::open([1000, 0, 2000], 0x100, 221);
        assert_eq!(screen.camera_from, [6000, -7000, 2000]);
        assert_eq!(screen.camera_to, [1000, -1000, 2000]);

        // The entry arms the white flash: the first frame is fully white.
        let overlay = screen.overlay().unwrap();
        assert_eq!(overlay.color, FadeColor::White);
        assert_eq!(overlay.alpha, 255);
        assert_eq!(screen.state, 1);

        // The flash decays over 86 ticks (the last drawn alpha is 0), then
        // the reveal starts on the 87th.
        assert_eq!(screen.rect_alpha(), 0);
        let mut reveal = 0;
        while screen.state == 1 {
            screen.tick(false);
            reveal += 1;
            assert!(reveal < 200, "the flash must clear");
        }
        assert_eq!(reveal, 87);
        assert_eq!(screen.state, 2);

        // The black ramp starts at zero; the strip starts stepping once the
        // ramp passes the threshold, 21 ticks later.
        let mut first_strip = None;
        let mut stepped = 0;
        for tick in 0..200 {
            screen.tick(false);
            if screen.strip_param().is_some() {
                first_strip.get_or_insert((tick, screen.rect_alpha()));
                stepped += 1;
            }
            if screen.state == 3 {
                break;
            }
        }
        let (tick, alpha) = first_strip.expect("the strip draws");
        assert_eq!(alpha, 63, "the first strip frame is past the threshold");
        assert_eq!(tick, 21);
        assert_eq!(stepped, 56, "59 down to 4 is 56 stepped frames");
        assert_eq!(screen.state, 3);

        // The hold runs 49 frames at the settled amplitude.
        for _ in 0..=HOLD_FRAMES {
            assert!(!screen.tick(false));
        }
        assert_eq!(screen.state, 4);
        // The black fade-out arms at alpha zero and re-waves the strip.
        screen.tick(false);
        assert_eq!(
            screen.rect_alpha(),
            228,
            "the overlay holds the last ramped value"
        );
        assert_eq!(screen.overlay().unwrap().color, FadeColor::Black);
        let mut rewave = Vec::new();
        for _ in 0..200 {
            if screen.tick(false) {
                break;
            }
            if let Some(param) = screen.strip_param() {
                rewave.push(param);
            }
        }
        assert!(screen.finished);
        assert_eq!(rewave[0], 4, "the re-wave starts at the settled value");
        assert_eq!(rewave[1], 5);
    }

    #[test]
    fn the_confirm_button_skips_the_screen() {
        let mut screen = DeathScreen::open([0, 0, 0], 0, 221);
        screen.tick(false);
        assert!(screen.tick(true));
        assert!(screen.finished);
    }

    #[test]
    fn the_strip_math_matches_the_original() {
        assert_eq!(fsin(0x100), 1567);
        assert_eq!(fsin(0x800), 0);
        assert_eq!(strip_base(59), -152);
        assert_eq!(strip_base(3), -44);
        assert_eq!(strip_wave(59, 1), 665);
        assert_eq!(strip_wave(59, 2), 1230);
        assert_eq!(strip_wave(3, 1), 1);
        // The unsigned shift wraps negative sine products below the baseline.
        assert_eq!(strip_wave(59, 255), -666);
        assert_eq!(strip_wave(3, 255), -2);
    }
}
