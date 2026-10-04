//! The door-transition timeline the engine drives each 30 Hz tick.
//!
//! A door transition is a fixed script over the `.dor` animation player
//! ([`crate::door::vm`]):
//!
//! * the first three animation frames are drawn fully black (the destination
//!   room loads behind them); the phase-2 hold freezes the frame counter, so
//!   the black covers the hold too;
//! * phase 2 holds for five extra frames (the "door fully open" beat);
//! * the animation's fade ops set a counter/state pair that the transition
//!   accumulates once per tick into the full-screen overlay;
//! * a d-pad direction or action button held after frame 10 skips the rest of
//!   the animation immediately;
//! * gameplay input stays locked until the animation finishes.
//!
//! [`DoorStepper`] is the seam between this timeline and the `.dor`
//! interpreter: the engine adapts [`crate::door::vm::Vm`] to it. All timeline
//! bookkeeping (frame/phase/hold counters, skip, fade accumulation,
//! completion) lives here, so the machine is unit-testable with a fake
//! animation.
//!
//! # Drive loop
//!
//! ```
//! use arklay::transition::{DoorFrame, DoorStepper, Transition};
//!
//! struct Animation {
//!     ticks: u32,
//! }
//!
//! impl DoorStepper for Animation {
//!     fn step(&mut self) -> DoorFrame {
//!         self.ticks += 1;
//!         DoorFrame { done: self.ticks >= 20, ..DoorFrame::default() }
//!     }
//!     fn finish(&mut self) {}
//!     fn set_sound_busy(&mut self, _busy: bool) {}
//! }
//!
//! let mut transition = Transition::new(Animation { ticks: 0 });
//! while !transition.is_finished() {
//!     // Gameplay is locked while the transition runs; only the skip flag is
//!     // meaningful input.
//!     let frame = transition.tick(false);
//!     if frame.black {
//!         // Draw a full-screen black rect.
//!     } else {
//!         // Draw the door scene with frame.camera; blend frame.overlay on top.
//!     }
//! }
//! // Teardown: swap in the destination room, play the door SE, start the BGM.
//! ```
//!
//! # Faithfulness notes
//!
//! * The skip gate is `frame > 10` on the animation frame counter, which is
//!   frozen during the phase-2 hold, exactly like the original's frame count.
//! * The fade accumulator is a 16-bit signed counter: it only advances while
//!   non-negative and wraps when it overflows. The overlay is drawn before the
//!   tick's accumulation (the original queues the rect in the same frame the
//!   script runs the fade op).
//! * `CLR_FLAGS2` can clear the sound-busy wait inside the animation, so the
//!   busy flag reported by the stepper is authoritative over the value last
//!   passed to [`Transition::set_sound_busy`].

/// Leading animation frames the original draws fully black.
pub const BLACK_FRAMES: u32 = 3;
/// Extra frames phase 2 is held before its phase counter advances.
pub const PHASE2_HOLD_FRAMES: u8 = 5;
/// First animation frame on which held input may skip the transition.
pub const SKIP_AFTER_FRAME: u32 = 10;

/// Overlay colour selected by the animation's fade type id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FadeColor {
    /// Fade type 1: an opaque-as-`alpha` white flash.
    White,
    /// Fade type 2: an opaque-as-`alpha` black veil.
    #[default]
    Black,
}

impl FadeColor {
    /// Map a fade type id to its overlay colour.
    ///
    /// Type 1 is the white flash and type 2 the black veil; every shipped door
    /// transition uses 2. Other ids fall back to the white variant, matching
    /// the original's greyscale tint path.
    pub fn from_id(fade_type: u8) -> Self {
        if fade_type == 2 {
            Self::Black
        } else {
            Self::White
        }
    }
}

/// The full-screen overlay the renderer blends over the door scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overlay {
    /// Blend weight, `0..=255`.
    pub alpha: u8,
    /// Overlay colour.
    pub color: FadeColor,
}

/// The script-driven fade accumulator.
///
/// The `.dor` `FADE_IN`/`FADE_OUT` ops set `fade_type`, `counter` and an
/// initial `state`; the transition adds `counter` to `state` once per tick and
/// exposes `alpha = state >> 7`. A negative accumulator means "no overlay",
/// matching the original's non-negative accumulator gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fade {
    fade_type: u8,
    counter: i32,
    state: i16,
}

impl Default for Fade {
    fn default() -> Self {
        Self::inactive()
    }
}

impl Fade {
    /// No fade yet: the accumulator starts negative, so [`Fade::overlay`]
    /// returns `None`.
    pub const fn inactive() -> Self {
        Self {
            fade_type: 0,
            counter: 0,
            state: -1,
        }
    }

    /// A fade to black/white: `counter = value << 7`, so `state` grows and the
    /// overlay darkens (or whitens) by `value` alpha steps per tick.
    pub fn fade_out(value: i16, state: i16, fade_type: u8) -> Self {
        Self {
            fade_type,
            counter: i32::from(value) << 7,
            state,
        }
    }

    /// A fade from black/white: `counter = -value << 7`, so `state` shrinks by
    /// `value` alpha steps per tick.
    pub fn fade_in(value: i16, state: i16, fade_type: u8) -> Self {
        Self {
            fade_type,
            counter: -(i32::from(value) << 7),
            state,
        }
    }

    /// Latch a script's fade values verbatim.
    ///
    /// `counter` is kept as-is (the `.dor` interpreter computes it with the
    /// full-width shift); the 16-bit accumulator truncates it on each add,
    /// matching the original's `short` counter.
    pub fn set(&mut self, fade_type: u8, counter: i32, state: i16) {
        self.fade_type = fade_type;
        self.counter = counter;
        self.state = state;
    }

    /// Advance the accumulator by one 30 Hz tick.
    ///
    /// The original stops updating a negative accumulator, so a fade that has
    /// fully cleared stays cleared until the next scripted fade.
    pub fn tick(&mut self) {
        if self.state >= 0 {
            self.state = self.state.wrapping_add(self.counter as i16);
        }
    }

    /// The overlay to blend this frame, or `None` once the accumulator has run
    /// negative.
    pub fn overlay(&self) -> Option<Overlay> {
        if self.state < 0 {
            return None;
        }
        Some(Overlay {
            alpha: (i32::from(self.state) >> 7).clamp(0, 255) as u8,
            color: FadeColor::from_id(self.fade_type),
        })
    }

    /// The scripted fade type id.
    pub fn fade_type(&self) -> u8 {
        self.fade_type
    }

    /// The per-tick accumulator step.
    pub fn counter(&self) -> i32 {
        self.counter
    }

    /// The current 16-bit accumulator.
    pub fn state(&self) -> i16 {
        self.state
    }

    /// Whether the accumulator is non-negative (the original draws then).
    pub fn is_active(&self) -> bool {
        self.state >= 0
    }
}

/// The door scene's camera.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Camera {
    /// Camera position.
    pub from: [i32; 3],
    /// Camera look-at point.
    pub to: [i32; 3],
    /// Focal length in pixels.
    pub focal: i32,
}

/// One raw frame produced by a [`DoorStepper`].
///
/// This is the plain snapshot the `.dor` interpreter exposes: the scene
/// camera, the values of the most recent `FADE_IN`/`FADE_OUT` op, whether the
/// script has ended, the animation's own phase/hold counters and black flag,
/// and whether the stepper is currently waiting for the sound system.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DoorFrame {
    /// Door scene camera.
    pub camera: Camera,
    /// Scripted fade type id.
    pub fade_type: u8,
    /// Scripted per-tick fade step.
    pub fade_counter: i32,
    /// Scripted fade start/current accumulator.
    pub fade_state: i16,
    /// The animation's script has reached `END`.
    pub done: bool,
    /// The animation's phase counter.
    pub phase: i16,
    /// The animation's hold counter.
    pub hold: u8,
    /// The original draws the screen black while its frame counter is below 3.
    pub black: bool,
    /// The stepper is paused waiting for the sound system.
    pub sound_busy: bool,
}

/// The animation player behind a transition.
///
/// The engine adapts [`crate::door::vm::Vm`] to this trait:
/// [`DoorStepper::step`] returns the VM's per-frame snapshot,
/// [`DoorStepper::finish`] maps to `Vm::finish` and
/// [`DoorStepper::set_sound_busy`] to `Vm::set_sound_busy`.
pub trait DoorStepper {
    /// Advance one 30 Hz frame and return its snapshot.
    fn step(&mut self) -> DoorFrame;
    /// End the animation immediately (the held-input skip).
    fn finish(&mut self);
    /// Mark the sound system busy, pausing the script's `SFX` op.
    fn set_sound_busy(&mut self, busy: bool);
}

impl<T: DoorStepper + ?Sized> DoorStepper for &mut T {
    fn step(&mut self) -> DoorFrame {
        (**self).step()
    }

    fn finish(&mut self) {
        (**self).finish();
    }

    fn set_sound_busy(&mut self, busy: bool) {
        (**self).set_sound_busy(busy);
    }
}

impl<T: DoorStepper + ?Sized> DoorStepper for Box<T> {
    fn step(&mut self) -> DoorFrame {
        (**self).step()
    }

    fn finish(&mut self) {
        (**self).finish();
    }

    fn set_sound_busy(&mut self, busy: bool) {
        (**self).set_sound_busy(busy);
    }
}

/// One frame of the transition timeline, ready to render.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransitionFrame {
    /// Door scene camera.
    pub camera: Camera,
    /// Full-screen overlay to blend over the scene.
    pub overlay: Option<Overlay>,
    /// Draw the screen fully black instead of the door scene.
    pub black: bool,
    /// Animation frame counter (frozen during the phase-2 hold).
    pub frame: u32,
    /// Animation phase counter.
    pub phase: i16,
    /// Animation hold counter.
    pub hold: u8,
    /// The animation is over; the engine may tear the transition down.
    pub finished: bool,
    /// The animation's sound-busy wait state.
    pub sound_busy: bool,
}

/// The door-transition state machine.
///
/// Owns the animation timeline described in the [module docs](self) and drives
/// a [`DoorStepper`] once per tick. While [`Transition::input_locked`] is true
/// the engine must not run gameplay ticks; only the skip flag passed to
/// [`Transition::tick`] is meaningful input.
#[derive(Debug, Clone)]
pub struct Transition<S> {
    stepper: S,
    frame: u32,
    phase: i16,
    hold: u8,
    fade: Fade,
    latched_fade: Option<(u8, i32, i16)>,
    last: DoorFrame,
    sound_busy: bool,
    finished: bool,
}

impl<S: DoorStepper> Transition<S> {
    /// Start a transition over `stepper`.
    pub fn new(stepper: S) -> Self {
        Self {
            stepper,
            frame: 0,
            phase: 0,
            hold: 0,
            fade: Fade::inactive(),
            latched_fade: None,
            last: DoorFrame::default(),
            sound_busy: false,
            finished: false,
        }
    }

    /// Advance one 30 Hz tick.
    ///
    /// `skip_held` is the engine's d-pad-direction/action-button state; the
    /// transition only acts on it once the animation is past
    /// [`SKIP_AFTER_FRAME`]. The returned frame is the one to render this
    /// tick; when `finished` is set, render it once and then tear down.
    pub fn tick(&mut self, skip_held: bool) -> TransitionFrame {
        if self.finished {
            return self.output();
        }

        let raw = self.stepper.step();
        self.last = raw;
        self.sound_busy = raw.sound_busy;
        self.latch_fade(&raw);

        let frame = self.frame;
        let phase = self.phase;
        let hold = self.hold;
        // The original queues the fade rect after the animation task ran, so
        // this frame shows the accumulator before the tick's addition.
        let overlay = self.fade.overlay();
        let black = frame < BLACK_FRAMES || raw.black;

        if skip_held && frame > SKIP_AFTER_FRAME {
            self.stepper.finish();
            self.finished = true;
        }

        // Phase 2 holds for five extra frames with the frame counter frozen.
        if self.phase == 2 && self.hold < PHASE2_HOLD_FRAMES {
            self.hold += 1;
        } else {
            self.hold = 0;
            self.phase += 1;
            self.frame += 1;
        }

        if !self.finished
            && raw.done
            && self.frame >= BLACK_FRAMES
            && !(self.phase == 2 && self.hold < PHASE2_HOLD_FRAMES)
        {
            self.finished = true;
        }

        self.fade.tick();

        TransitionFrame {
            camera: raw.camera,
            overlay,
            black,
            frame,
            phase,
            hold,
            finished: self.finished,
            sound_busy: self.sound_busy,
        }
    }

    /// Set the sound system's busy state for the stepper's `SFX` wait.
    ///
    /// The stepper's reported state is authoritative on the next tick because
    /// the animation's `CLR_FLAGS2` op can clear the wait itself.
    pub fn set_sound_busy(&mut self, busy: bool) {
        self.sound_busy = busy;
        self.stepper.set_sound_busy(busy);
    }

    /// The animation frame counter (frozen during the phase-2 hold).
    pub fn frame(&self) -> u32 {
        self.frame
    }

    /// The animation phase counter.
    pub fn phase(&self) -> i16 {
        self.phase
    }

    /// The animation hold counter.
    pub fn hold(&self) -> u8 {
        self.hold
    }

    /// The fade accumulator.
    pub fn fade(&self) -> &Fade {
        &self.fade
    }

    /// Whether the transition is over and the engine may tear it down.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Whether gameplay input must stay locked.
    pub fn input_locked(&self) -> bool {
        !self.finished
    }

    /// Whether the screen is black this frame.
    pub fn black(&self) -> bool {
        self.frame < BLACK_FRAMES || self.last.black
    }

    /// The last sound-busy state reported by the stepper.
    pub fn sound_busy(&self) -> bool {
        self.sound_busy
    }

    /// The animation player.
    ///
    /// Read-only: stepping it directly would desynchronise the timeline.
    pub fn stepper(&self) -> &S {
        &self.stepper
    }

    /// The animation player, mutably.
    ///
    /// Do not call [`DoorStepper::step`] through this handle; the transition
    /// drives the stepper in [`Transition::tick`].
    pub fn stepper_mut(&mut self) -> &mut S {
        &mut self.stepper
    }

    fn latch_fade(&mut self, raw: &DoorFrame) {
        let spec = (raw.fade_type, raw.fade_counter, raw.fade_state);
        if self.latched_fade != Some(spec) {
            self.latched_fade = Some(spec);
            self.fade
                .set(raw.fade_type, raw.fade_counter, raw.fade_state);
        }
    }

    fn output(&self) -> TransitionFrame {
        TransitionFrame {
            camera: self.last.camera,
            overlay: self.fade.overlay(),
            black: self.black(),
            frame: self.frame,
            phase: self.phase,
            hold: self.hold,
            finished: self.finished,
            sound_busy: self.sound_busy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct FakeStepper {
        next: DoorFrame,
        steps: u32,
        finish_calls: u32,
        busy_calls: Vec<bool>,
    }

    impl DoorStepper for FakeStepper {
        fn step(&mut self) -> DoorFrame {
            self.steps += 1;
            self.next
        }

        fn finish(&mut self) {
            self.finish_calls += 1;
            self.next.done = true;
        }

        fn set_sound_busy(&mut self, busy: bool) {
            self.next.sound_busy = busy;
            self.busy_calls.push(busy);
        }
    }

    fn transition() -> Transition<FakeStepper> {
        Transition::new(FakeStepper::default())
    }

    #[test]
    fn black_covers_the_first_three_frames_and_the_phase_two_hold() {
        let mut transition = transition();

        let black: Vec<bool> = (0..10).map(|_| transition.tick(false).black).collect();

        // The phase-2 hold freezes the frame counter at 2, so it stays black.
        assert_eq!(&black[..8], [true; 8]);
        assert!(!black[8]);
        assert!(!black[9]);
    }

    #[test]
    fn phase_two_holds_for_five_extra_frames() {
        let mut transition = transition();

        let states: Vec<(i16, u8, u32)> = (0..9)
            .map(|_| {
                let frame = transition.tick(false);
                (frame.phase, frame.hold, frame.frame)
            })
            .collect();

        assert_eq!(
            states,
            [
                (0, 0, 0),
                (1, 0, 1),
                (2, 0, 2),
                (2, 1, 2),
                (2, 2, 2),
                (2, 3, 2),
                (2, 4, 2),
                (2, 5, 2),
                (3, 0, 3),
            ]
        );
    }

    #[test]
    fn fade_out_and_in_accumulate_and_clamp() {
        let mut out = Fade::fade_out(8, 0, 2);
        assert_eq!(out.counter(), 8 << 7);
        assert_eq!(out.overlay().unwrap().alpha, 0);
        for alpha in [8, 16, 24] {
            out.tick();
            assert_eq!(out.overlay().unwrap().alpha, alpha);
        }
        assert_eq!(out.overlay().unwrap().color, FadeColor::Black);

        let mut fade_in = Fade::fade_in(8, 0x2200, 2);
        assert_eq!(fade_in.counter(), -(8 << 7));
        assert_eq!(fade_in.overlay().unwrap().alpha, (0x2200i32 >> 7) as u8);
        for _ in 0..8 {
            fade_in.tick();
        }
        assert_eq!(fade_in.overlay().unwrap().alpha, 4);
        fade_in.tick();
        assert!(
            fade_in.overlay().is_none(),
            "a cleared fade must stop drawing"
        );

        let clamped = Fade::fade_out(1, i16::MAX, 1);
        assert_eq!(clamped.overlay().unwrap().alpha, 255);
        assert_eq!(clamped.overlay().unwrap().color, FadeColor::White);
    }

    #[test]
    fn fade_specs_from_the_animation_are_latched_and_accumulated() {
        let mut transition = transition();
        transition.stepper_mut().next.fade_type = 2;
        transition.stepper_mut().next.fade_counter = 10 << 7;
        transition.stepper_mut().next.fade_state = 2;

        let alpha = |transition: &mut Transition<FakeStepper>| {
            transition.tick(false).overlay.expect("fade overlay").alpha
        };

        assert_eq!(alpha(&mut transition), 0);
        assert_eq!(alpha(&mut transition), 10);
        assert_eq!(alpha(&mut transition), 20);

        // A later fade op replaces the accumulator instead of continuing it.
        transition.stepper_mut().next.fade_state = 0x2200;
        transition.stepper_mut().next.fade_counter = -(8 << 7);
        assert_eq!(alpha(&mut transition), (0x2200i32 >> 7) as u8);
        assert_eq!(alpha(&mut transition), ((0x2200 - (8 << 7)) >> 7) as u8);
    }

    #[test]
    fn held_input_skips_only_after_frame_ten() {
        let mut transition = transition();

        for _ in 0..16 {
            let frame = transition.tick(true);
            assert!(
                !frame.finished,
                "frame {} must not be skippable",
                frame.frame
            );
            assert!(transition.input_locked());
        }

        let frame = transition.tick(true);
        assert!(frame.finished);
        assert!(!transition.input_locked());
        assert_eq!(transition.stepper().finish_calls, 1);
    }

    #[test]
    fn completion_waits_for_the_animation_to_report_done() {
        let mut transition = transition();
        for _ in 0..20 {
            assert!(!transition.tick(false).finished);
        }

        transition.stepper_mut().next.done = true;
        let frame = transition.tick(false);
        assert!(frame.finished);
        assert_eq!(
            transition.stepper().finish_calls,
            0,
            "completion must not call the skip path"
        );
    }

    #[test]
    fn done_animation_waits_out_the_black_frames_and_hold() {
        let mut transition = transition();
        transition.stepper_mut().next.done = true;

        let frames: Vec<TransitionFrame> = (0..8).map(|_| transition.tick(false)).collect();

        assert!(frames[..7].iter().all(|frame| !frame.finished));
        assert!(frames[7].finished);
        assert!(frames.iter().all(|frame| frame.black));
    }

    #[test]
    fn sound_busy_is_forwarded_and_keeps_the_timeline_running() {
        let mut transition = transition();
        transition.set_sound_busy(true);
        assert_eq!(transition.stepper().busy_calls, [true]);

        let frame = transition.tick(false);
        assert!(frame.sound_busy);
        assert_eq!(frame.frame, 0);
        assert_eq!(
            transition.tick(false).frame,
            1,
            "the timeline advances while the script waits"
        );

        transition.set_sound_busy(false);
        assert!(!transition.tick(false).sound_busy);
        assert_eq!(transition.stepper().busy_calls, [true, false]);
    }

    #[test]
    fn a_stepper_black_flag_extends_the_black_screen() {
        let mut transition = transition();
        transition.stepper_mut().next.black = true;
        assert!(transition.tick(false).black);
    }

    #[test]
    fn a_finished_transition_stops_stepping() {
        let mut transition = transition();
        transition.stepper_mut().next.done = true;
        for _ in 0..8 {
            transition.tick(false);
        }
        assert!(transition.is_finished());

        let steps = transition.stepper().steps;
        let frame = transition.tick(false);
        assert!(frame.finished);
        assert_eq!(transition.stepper().steps, steps);
    }
}
