//! The door-transition timeline the engine drives each 30 Hz tick.
//!
//! A door transition is a fixed script over the `.dor` animation player
//! ([`crate::door::vm`]):
//!
//! * the first three animation frames are drawn fully black (the destination
//!   room loads behind them); the phase-2 hold freezes the frame counter, so
//!   the black covers the hold too, but a script that reaches `END` stops the
//!   timeline at the end of that pass: an immediate `END` exits after one
//!   black frame, without the hold;
//! * the animation's fade ops set a counter/state pair that the transition
//!   accumulates once per tick into the full-screen overlay;
//! * a fresh confirm (Cross) or run/cancel (Circle) press after frame 10
//!   skips the rest of the animation immediately; a button already held when
//!   the transition begins does not, because the original's skip gate reads
//!   its edge-detected pad word and so requires a release first;
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
//!   The gate also reads only a fresh press: the original's `g_PlayerPadHeld`
//!   is the edge-detected pad word, so the button that activated the door
//!   cannot skip the animation while it stays down.
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
/// First animation frame on which a skip press may end the transition.
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

    /// The overlay the original DRAWS this frame: the accumulator value before
    /// this tick's step. The original's main loop draws the fade and only then
    /// adds the counter, so a fade whose `state` is traced post-step draws one
    /// step behind it - and the tick that wraps the accumulator negative still
    /// draws the pre-wrap alpha.
    pub fn drawn_overlay(&self) -> Option<Overlay> {
        let previous = self.state.wrapping_sub(self.counter as i16);
        if previous < 0 {
            return None;
        }
        Some(Overlay {
            alpha: (i32::from(previous) >> 7).clamp(0, 255) as u8,
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
    /// A `FADE_IN`/`FADE_OUT` op ran during this step, so the shared fade
    /// takes the `fade_*` values.
    pub fade_written: bool,
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
    /// End the animation immediately (the skip press).
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
    /// `room_transition_load` placed the player at the record's entry point
    /// this frame (the snapshot, the room id and the placement, before the
    /// destination's `room_set`).
    pub place: bool,
    /// `room_transition_load` ran the destination's `room_set` this frame.
    pub load: bool,
    /// `room_transition_load` returned this frame: the engine runs its tail
    /// (camera, close SE, entry fade) now and gameplay resumes on the next
    /// tick.
    pub finished: bool,
    /// The animation's sound-busy wait state.
    pub sound_busy: bool,
}

/// What one frame's scheduler passes did.
#[derive(Debug, Default)]
struct FrameOut {
    overlay: Option<Overlay>,
    black: bool,
    drew: bool,
    place: bool,
    load: bool,
    finished: bool,
}

/// `game_loop`'s menu path opens a door with `set_fading(2, 0x600)` before it
/// calls `room_transition_load`.
const DOOR_FADE_TYPE: u8 = 2;
const DOOR_FADE_COUNTER: i32 = 0x600;

/// The gameplay task (task 0) while it runs a door: the end of `game_loop`'s
/// inner loop, its menu path and `room_transition_load`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainTask {
    /// The inner loop's last pass with the menu flag raised.
    Settle,
    /// The menu path: `set_fading(2, 0x600)`, then `room_transition_load`
    /// spawns the animation task and sleeps.
    Spawn,
    /// Woken after the spawn: place the player and load the destination.
    Place,
    /// A stage change's `init_room` yielded once before its `room_set`.
    Load,
    /// Polling the animation task's `MSF_ROOM_TRANSITION` bit.
    Wait,
    /// `room_transition_load` returned.
    Done,
}

/// The door-animation task (task 1, `DoorAnimTask`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnimTask {
    /// Not spawned yet.
    Idle,
    /// Spawned: its first run is `DoorAnimInit`.
    Start,
    /// `DoorAnimLoop`: one pass per run.
    Run,
    /// The loop ended: its next run is `DoorAnimTeardown`.
    Teardown,
    /// `Task_exit`.
    Exited,
}

/// The door-transition state machine.
///
/// Owns the animation timeline described in the [module docs](self) and drives
/// a [`DoorStepper`] once per tick. While [`Transition::input_locked`] is true
/// the engine must not run gameplay ticks; only the skip flag passed to
/// [`Transition::tick`] is meaningful input.
///
/// Each tick is one `main_loop` frame of the original, which runs the task
/// scheduler over the gameplay task (`room_transition_load`) and the
/// door-animation task, around the shared `g_fading_counter`/`g_fading_state`
/// fade. The menu path's `set_fading` raises `MSF_FADE_ACTIVE`, whose branch
/// suspends the gameplay task until the accumulator runs negative; on that
/// frame the branch resumes it and falls through into a second scheduler
/// pass, so the animation steps twice and the destination loads behind it.
#[derive(Debug, Clone)]
pub struct Transition<S> {
    stepper: S,
    frame: u32,
    phase: i16,
    hold: u8,
    fade: Fade,
    /// `MSF_FADE_ACTIVE`.
    fade_active: bool,
    /// The fade branch suspended the gameplay task and owns its resume.
    main_suspended: bool,
    main: MainTask,
    anim: AnimTask,
    /// The animation task holds `MSF_ROOM_TRANSITION`.
    anim_running: bool,
    /// The destination is a stage change (`init_room` yields once).
    load_yields: bool,
    /// Camera-only record: `room_transition_load` loads no room.
    camera_only: bool,
    last: DoorFrame,
    /// The counters of the last drawn door pass.
    drawn: (u32, i16, u8),
    last_black: bool,
    sound_busy: bool,
    /// The skip button has been released at least once since the transition
    /// began. The original's skip gate reads its edge-detected pad word, so a
    /// button already down when the transition starts cannot skip until it
    /// comes up again.
    skip_released: bool,
}

impl<S: DoorStepper> Transition<S> {
    /// Start a transition over `stepper` on the tick after the door fired.
    ///
    /// The shared fade starts inactive; [`Transition::with_fade`] seeds it
    /// with the room's live values.
    pub fn new(stepper: S) -> Self {
        Self {
            stepper,
            frame: 0,
            phase: 0,
            hold: 0,
            fade: Fade::inactive(),
            fade_active: false,
            main_suspended: false,
            main: MainTask::Settle,
            anim: AnimTask::Idle,
            anim_running: false,
            load_yields: false,
            camera_only: false,
            last: DoorFrame::default(),
            drawn: (0, 0, 0),
            last_black: true,
            sound_busy: false,
            skip_released: false,
        }
    }

    /// Seed the shared fade with the values the room left in
    /// `g_fading_counter`/`g_fading_state`.
    pub fn with_fade(mut self, fade_type: u8, counter: i32, state: i16) -> Self {
        self.fade.set(fade_type, counter, state);
        self
    }

    /// Describe the load `room_transition_load` runs: a camera-only record
    /// loads nothing, and a stage change's `init_room` yields one frame
    /// before its `room_set`.
    pub fn with_load(mut self, camera_only: bool, stage_change: bool) -> Self {
        self.camera_only = camera_only;
        self.load_yields = stage_change && !camera_only;
        self
    }

    /// Advance one `main_loop` frame.
    ///
    /// `skip_pressed` is the engine's confirm/cancel press edge; the animation
    /// only acts on it once it is past [`SKIP_AFTER_FRAME`] and the button has
    /// been released at least once since the transition began. The original's
    /// skip gate reads its edge-detected pad word, so the press that activated
    /// the door cannot skip the animation while it stays down. The returned
    /// frame is the one to render this tick; when `finished` is set, render
    /// it once and then tear down.
    pub fn tick(&mut self, skip_pressed: bool) -> TransitionFrame {
        self.skip_released |= !skip_pressed;
        self.tick_frame(skip_pressed && self.skip_released)
    }

    /// Advance one frame with the skip gate forced.
    ///
    /// The engine's cutscene fast-forward skips a door the scene walks through
    /// under the letterbox. That is not a player press, so it does not wait
    /// for a release, and it must not arm the gate for a later normal tick;
    /// everything else matches [`Transition::tick`].
    pub fn tick_forced(&mut self) -> TransitionFrame {
        self.tick_frame(true)
    }

    /// One `main_loop` frame with the skip gate already resolved.
    fn tick_frame(&mut self, skip: bool) -> TransitionFrame {
        let mut out = FrameOut::default();
        if self.main == MainTask::Done {
            return self.output();
        }
        if self.fade_active {
            // The fade branch: a fresh fade starts its accumulator at 0 (at
            // 0x7FFF when it counts down) and suspends the gameplay task.
            if self.fade.state() < 0 {
                let start = if self.fade.counter() < 1 { i16::MAX } else { 0 };
                self.fade
                    .set(self.fade.fade_type(), self.fade.counter(), start);
                self.main_suspended = true;
            }
            out.overlay = self.fade.overlay();
            self.pass(skip, &mut out);
            self.fade_step();
            if self.fade.state() < 0 {
                self.fade.set(self.fade.fade_type(), 0, 0);
                self.fade_active = false;
                self.main_suspended = false;
                self.idle_branch(skip, &mut out);
            }
        } else {
            self.idle_branch(skip, &mut out);
        }
        self.output_into(out)
    }

    /// `main_loop`'s non-fade branch: a scheduler pass, then the accumulator
    /// draws and steps while it is non-negative.
    fn idle_branch(&mut self, skip: bool, out: &mut FrameOut) {
        self.pass(skip, out);
        if self.fade.is_active() {
            if out.overlay.is_none() {
                out.overlay = self.fade.overlay();
            }
            self.fade_step();
        }
    }

    /// Add the counter to the 16-bit accumulator, like both of `main_loop`'s
    /// branches.
    fn fade_step(&mut self) {
        let state = self.fade.state().wrapping_add(self.fade.counter() as i16);
        self.fade
            .set(self.fade.fade_type(), self.fade.counter(), state);
    }

    /// One `TaskScheduler_Update`: the gameplay task, then the animation
    /// task. Every sleep is one pass long, so a task that yielded in the
    /// previous pass runs again (even within the same frame).
    fn pass(&mut self, skip: bool, out: &mut FrameOut) {
        if !self.main_suspended {
            self.run_main(out);
        }
        self.run_anim(skip, out);
    }

    fn run_main(&mut self, out: &mut FrameOut) {
        match self.main {
            MainTask::Settle => self.main = MainTask::Spawn,
            MainTask::Spawn => {
                // `set_fading` only latches while the accumulator is idle.
                if self.fade.state() < 1 {
                    self.fade_active = true;
                    self.fade
                        .set(DOOR_FADE_TYPE, DOOR_FADE_COUNTER, self.fade.state());
                }
                self.anim = AnimTask::Start;
                self.main = MainTask::Place;
            }
            MainTask::Place => {
                out.place = !self.camera_only;
                if self.load_yields {
                    self.main = MainTask::Load;
                } else {
                    out.load = !self.camera_only;
                    self.wait(out);
                }
            }
            MainTask::Load => {
                out.load = true;
                self.wait(out);
            }
            MainTask::Wait => self.wait(out),
            MainTask::Done => {}
        }
    }

    /// `room_transition_load`'s wait for the animation task.
    fn wait(&mut self, out: &mut FrameOut) {
        if self.anim_running {
            self.main = MainTask::Wait;
        } else {
            self.main = MainTask::Done;
            out.finished = true;
        }
    }

    fn run_anim(&mut self, skip: bool, out: &mut FrameOut) {
        match self.anim {
            AnimTask::Idle | AnimTask::Exited => {}
            AnimTask::Start => {
                self.anim_running = true;
                self.anim = AnimTask::Run;
            }
            AnimTask::Run => self.anim_pass(skip, out),
            AnimTask::Teardown => {
                self.fade
                    .set(self.fade.fade_type(), self.fade.counter(), -1);
                self.anim_running = false;
                self.anim = AnimTask::Exited;
            }
        }
    }

    /// One `DoorAnimLoop` pass.
    fn anim_pass(&mut self, skip: bool, out: &mut FrameOut) {
        let raw = self.stepper.step();
        self.last = raw;
        self.sound_busy = raw.sound_busy;
        if raw.fade_written {
            self.fade
                .set(raw.fade_type, raw.fade_counter, raw.fade_state);
        }
        let frame = self.frame;
        self.drawn = (self.frame, self.phase, self.hold);
        self.last_black = frame < BLACK_FRAMES || raw.black;
        out.black |= self.last_black;
        out.drew = true;

        // The pass that reaches `END` is the last one the original runs: it is
        // drawn, then the loop exits, skipping the phase-2 hold and the
        // frame-counter increment. A skip press ends the loop the same way.
        let mut ended = raw.done;
        if !ended && skip && frame > SKIP_AFTER_FRAME {
            self.stepper.finish();
            ended = true;
        }
        if ended {
            self.anim = AnimTask::Teardown;
            return;
        }
        // Phase 2 holds for five extra frames with the frame counter frozen.
        if self.phase == 2 && self.hold < PHASE2_HOLD_FRAMES {
            self.hold += 1;
        } else {
            self.hold = 0;
            self.phase += 1;
            self.frame += 1;
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

    /// The shared `g_fading_counter`/`g_fading_state` fade.
    pub fn fade(&self) -> &Fade {
        &self.fade
    }

    /// Whether the transition is over and the engine may tear it down.
    pub fn is_finished(&self) -> bool {
        self.main == MainTask::Done
    }

    /// Whether gameplay input must stay locked.
    pub fn input_locked(&self) -> bool {
        !self.is_finished()
    }

    /// Whether the screen is black this frame: the animation is not drawing
    /// a door pass (before its loop, after its teardown) or the pass is one
    /// of the black frames.
    pub fn black(&self) -> bool {
        self.anim != AnimTask::Run || self.last_black
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

    fn output_into(&self, out: FrameOut) -> TransitionFrame {
        TransitionFrame {
            overlay: out.overlay,
            black: out.black || !out.drew,
            place: out.place,
            load: out.load,
            finished: out.finished,
            ..self.output_with()
        }
    }

    fn output_with(&self) -> TransitionFrame {
        let (frame, phase, hold) = self.drawn;
        TransitionFrame {
            camera: self.last.camera,
            frame,
            phase,
            hold,
            sound_busy: self.sound_busy,
            ..TransitionFrame::default()
        }
    }

    fn output(&self) -> TransitionFrame {
        TransitionFrame {
            overlay: self.fade.overlay(),
            black: self.black(),
            finished: self.is_finished(),
            ..self.output_with()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct FakeStepper {
        next: DoorFrame,
        /// A fade op the next step runs (once).
        fade_op: Option<(u8, i32, i16)>,
        steps: u32,
        finish_calls: u32,
        busy_calls: Vec<bool>,
    }

    impl DoorStepper for FakeStepper {
        fn step(&mut self) -> DoorFrame {
            self.steps += 1;
            let mut frame = self.next;
            if let Some((fade_type, counter, state)) = self.fade_op.take() {
                frame.fade_type = fade_type;
                frame.fade_counter = counter;
                frame.fade_state = state;
                frame.fade_written = true;
            }
            frame
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

    /// The shipped doors open on `FADE_IN 2, 8, 0x2200`.
    const DOOR_FADE_IN: (u8, i32, i16) = (2, -(8 << 7), 0x2200);

    /// A door over the shipped opening fade, with the room's settled fade
    /// (`-6144:-4097`) in the shared pair.
    fn transition() -> Transition<FakeStepper> {
        let stepper = FakeStepper {
            fade_op: Some(DOOR_FADE_IN),
            ..FakeStepper::default()
        };
        Transition::new(stepper).with_fade(2, -6144, -4097)
    }

    /// Run the two frames before the animation's first pass.
    fn lead_in(transition: &mut Transition<FakeStepper>) {
        for _ in 0..2 {
            let frame = transition.tick(false);
            assert!(frame.black && !frame.place && !frame.load && !frame.finished);
        }
        assert_eq!(transition.stepper().steps, 0);
    }

    fn fade(transition: &Transition<FakeStepper>) -> (i32, i16) {
        (transition.fade().counter(), transition.fade().state())
    }

    #[test]
    fn the_menu_path_runs_two_frames_before_the_animation() {
        let mut transition = transition();
        // The inner loop's last pass leaves the room's fade alone.
        transition.tick(false);
        assert_eq!(fade(&transition), (-6144, -4097));
        // The menu path's `set_fading(2, 0x600)` latches only the counter.
        transition.tick(false);
        assert_eq!(fade(&transition), (0x600, -4097));
        assert_eq!(transition.stepper().steps, 0);
        // The fade branch starts the accumulator at 0, the first pass's
        // `FADE_IN` replaces it and the frame steps it once.
        transition.tick(false);
        assert_eq!(transition.stepper().steps, 1);
        assert_eq!(fade(&transition), (-(8 << 7), 0x2200 - (8 << 7)));
    }

    #[test]
    fn the_load_waits_for_the_fade_and_shares_its_frame_with_a_second_pass() {
        let mut transition = transition();
        lead_in(&mut transition);
        // 0x2200 counts down by 0x400 for eight frames with the gameplay task
        // suspended.
        for _ in 0..8 {
            let frame = transition.tick(false);
            assert!(!frame.place && !frame.load);
        }
        assert_eq!(fade(&transition), (-(8 << 7), 512));
        assert_eq!(transition.stepper().steps, 8);
        // The accumulator runs out: the branch clears the pair, resumes the
        // gameplay task and runs a second pass in the same frame.
        let frame = transition.tick(false);
        assert!(frame.place && frame.load);
        assert_eq!(fade(&transition), (0, 0));
        assert_eq!(transition.stepper().steps, 10);
        assert!(!frame.finished);
    }

    #[test]
    fn a_stage_change_loads_one_frame_after_the_placement() {
        let mut transition = transition().with_load(false, true);
        lead_in(&mut transition);
        for _ in 0..8 {
            transition.tick(false);
        }
        let frame = transition.tick(false);
        assert!(frame.place && !frame.load);
        let frame = transition.tick(false);
        assert!(!frame.place && frame.load);
    }

    #[test]
    fn a_camera_only_record_loads_nothing() {
        let mut transition = transition().with_load(true, true);
        lead_in(&mut transition);
        for _ in 0..30 {
            let frame = transition.tick(false);
            assert!(!frame.place && !frame.load);
        }
    }

    #[test]
    fn black_covers_the_first_three_frames_and_the_phase_two_hold() {
        let mut transition = transition();
        lead_in(&mut transition);

        let black: Vec<bool> = (0..10).map(|_| transition.tick(false).black).collect();

        // The phase-2 hold freezes the frame counter at 2, so it stays black.
        assert_eq!(&black[..8], [true; 8]);
        assert!(!black[8]);
        assert!(!black[9]);
    }

    #[test]
    fn phase_two_holds_for_five_extra_frames() {
        let mut transition = transition();
        lead_in(&mut transition);

        let states: Vec<(i16, u8, u32)> = (0..8)
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
            ]
        );
        // The ninth frame runs two passes (frames 3 and 4) and reports the
        // last one drawn.
        let frame = transition.tick(false);
        assert_eq!((frame.phase, frame.hold, frame.frame), (4, 0, 4));
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
    fn a_fade_op_after_the_load_draws_then_steps() {
        let mut transition = transition();
        lead_in(&mut transition);
        for _ in 0..12 {
            transition.tick(false);
        }
        assert_eq!(fade(&transition), (0, 0));

        // A `FADE_OUT 2, 14, 0x200` pass: the frame draws the written value,
        // then the accumulator steps.
        transition.stepper_mut().fade_op = Some((2, 14 << 7, 0x200));
        let frame = transition.tick(false);
        assert_eq!(frame.overlay.expect("fade overlay").alpha, 4);
        assert_eq!(fade(&transition), (14 << 7, 0x200 + (14 << 7)));
        // The same op again restarts the accumulator rather than continuing.
        transition.stepper_mut().fade_op = Some((2, 14 << 7, 0x200));
        transition.tick(false);
        assert_eq!(fade(&transition), (14 << 7, 0x200 + (14 << 7)));
    }

    #[test]
    fn a_skip_press_ends_the_animation_only_after_frame_ten() {
        let mut transition = transition();
        lead_in(&mut transition);

        // Ticks return frames 0, 1, 2 (held five times), 3+4, 5..=10.
        for _ in 0..15 {
            let frame = transition.tick(true);
            assert!(frame.frame <= SKIP_AFTER_FRAME);
            assert!(!frame.finished);
        }
        assert_eq!(transition.stepper().finish_calls, 0);

        // Frame 11 is skippable: the loop ends on that pass, the teardown
        // runs on the next frame and `room_transition_load` returns on the
        // one after.
        let frame = transition.tick(true);
        assert_eq!(frame.frame, 11);
        assert!(!frame.finished);
        assert_eq!(transition.stepper().finish_calls, 1);
        assert!(!transition.tick(true).finished);
        assert!(transition.input_locked());
        assert!(transition.tick(true).finished);
        assert!(!transition.input_locked());
    }

    /// The action button that activated the door is still down when the
    /// transition starts; it must not be read as a skip (issue #107).
    #[test]
    fn a_button_held_from_the_door_press_never_skips() {
        let mut transition = transition();
        for _ in 0..SKIP_AFTER_FRAME as usize + 8 {
            let frame = transition.tick(true);
            assert!(!frame.finished);
        }
        assert_eq!(
            transition.stepper().finish_calls,
            0,
            "a held button must be released before it can skip"
        );
        assert!(transition.input_locked());
    }

    /// Once the held button is released and pressed again, the fresh press
    /// skips exactly like the original's edge-detected pad word.
    #[test]
    fn a_release_then_press_skips_the_transition() {
        let mut transition = transition();
        for _ in 0..SKIP_AFTER_FRAME as usize + 8 {
            transition.tick(true);
        }
        assert_eq!(transition.stepper().finish_calls, 0);
        transition.tick(false);
        let frame = transition.tick(true);
        assert!(frame.frame > SKIP_AFTER_FRAME);
        assert_eq!(transition.stepper().finish_calls, 1);
    }

    /// The cutscene fast-forward skips a door the scene walks through, and is
    /// not a player press, so it does not wait for a release.
    #[test]
    fn the_cutscene_fast_forward_skips_a_held_button() {
        let mut transition = transition();
        for _ in 0..SKIP_AFTER_FRAME as usize + 8 {
            transition.tick_forced();
        }
        assert_eq!(transition.stepper().finish_calls, 1);
    }

    #[test]
    fn completion_runs_the_teardown_frame_then_returns() {
        let mut transition = transition();
        lead_in(&mut transition);
        for _ in 0..20 {
            assert!(!transition.tick(false).finished);
        }

        transition.stepper_mut().next.done = true;
        let frame = transition.tick(false);
        assert!(!frame.finished && !frame.black, "the END pass is drawn");
        let steps = transition.stepper().steps;

        // `DoorAnimTeardown`: the accumulator goes inactive, nothing is drawn.
        let frame = transition.tick(false);
        assert!(!frame.finished && frame.black);
        assert_eq!(transition.fade().state(), -1);

        let frame = transition.tick(false);
        assert!(frame.finished && frame.black);
        assert_eq!(transition.stepper().steps, steps);
        assert_eq!(
            transition.stepper().finish_calls,
            0,
            "completion must not call the skip path"
        );
    }

    #[test]
    fn an_immediate_end_still_waits_for_the_fade() {
        let mut transition = transition();
        transition.stepper_mut().next.done = true;
        lead_in(&mut transition);

        let frame = transition.tick(false);
        assert!(frame.black, "pass 0 is one of the black frames");
        assert_eq!(frame.frame, 0);
        let mut ticks = 1;
        while !transition.tick(false).finished {
            ticks += 1;
            assert!(ticks < 64, "the transition never returned");
        }
        assert!(!transition.input_locked());
        assert_eq!(transition.stepper().steps, 1);

        // A finished timeline stops stepping the animation.
        assert!(transition.tick(false).finished);
        assert_eq!(transition.stepper().steps, 1);
    }

    #[test]
    fn sound_busy_is_forwarded_and_keeps_the_timeline_running() {
        let mut transition = transition();
        lead_in(&mut transition);
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
        lead_in(&mut transition);
        for _ in 0..12 {
            transition.tick(false);
        }
        assert!(!transition.tick(false).black);
        transition.stepper_mut().next.black = true;
        assert!(transition.tick(false).black);
    }
}
