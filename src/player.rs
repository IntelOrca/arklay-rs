//! Player locomotion: input mapping, movement, room collision and camera cuts.
//!
//! This mirrors the original engine's fixed 30 Hz player step using the EMD/EMW
//! body clips: idle settle and breathe, walk, turn in place, backward walk and
//! run. The room collision path never changes Y; the only height changes come
//! from the stair state the room scripts drive ([`StairState`]), which either
//! holds the ramp height or suspends collision while a stair/ladder climb runs.
//!
//! The original's slow-motion modifier (`MSF2_EFFECT_ZONE`) halves the walk
//! and run speeds and holds each locomotion frame for an extra tick; the
//! engine raises [`Input::slow_motion`] from the room-effect flag and the
//! locomotion machine applies both halves.
//!
//! Footsteps follow the clip each behavior plays. The walk and the in-place
//! turn share the no-weapon walk cycle and fire on its contact frames `0x08`
//! and `0x16` with the "A" footstep; the forward run plays the no-weapon run
//! cycle whose contacts are the first and tenth frames and uses the "B"
//! footstep; the backward run plays the body model's run cycle and fires on
//! `0x08`/`0x16` with the "A" footstep. The run cycle is both shorter and has
//! its contacts closer together, so running produces more footfalls per second
//! than walking.
//!
//! Three locked action behaviours sit on top of the locomotion machine:
//! [`LockedAction::Push`] plays the room animation pair's `0x30` wind-up and
//! `0x31` push loop while the object pass keeps raising the push bit,
//! [`LockedAction::Vault`] runs the `0x33`/`0x35` climb-over clip and the
//! `0x73A`/`0x708` warp, and [`LockedAction::Ladder`] runs the eight-state
//! `set_stairs_zone` climb (approach, turn, the room's `0x33`/`0x35` climb
//! clip with its step SEs, descent and walk-away). All are selected outside
//! this module (the action press and the push bit are read by
//! [`crate::game::GameState::tick_objects`]) and their one-shot sounds are
//! drained through [`PlayerState::take_sounds`]. A room without the embedded
//! animation pair falls back to a short wind-up and the warp (documented on
//! the behaviour functions).

use crate::anim::AnimPlayer;
use crate::game::ReachRequest;
use crate::model::Clip;
use crate::state::{Collision, CollisionRect, RoomId, RoomState};

/// Collision radius of the Chris model.
pub const CHRIS_RADIUS: i32 = 422;
/// Collision radius of the Jill model.
pub const JILL_RADIUS: i32 = 372;

/// The player's collision callback flags as spawned. The original's player
/// entity aliases this byte with `healthStatusFlags` and raises `0x10` on every
/// player init; the collision pass skips shape-5 records while it is set
/// ([`PlayerState::collision_flags`]).
pub const PLAYER_COLLISION_FLAGS: u8 = 0x10;

/// EMD clip 0: the three-frame idle settle pose.
const SETTLE_CLIP: usize = 0;
/// EMW clip 0: the transition from the settle pose into the breathe loop.
const BREATHE_IN_CLIP: usize = 0;
/// EMW clip 1: the looping breathe animation.
const BREATHE_CLIP: usize = 1;
/// EMW clip 2: the walk cycle, also used for turning in place.
const WALK_CLIP: usize = 2;
/// EMW clip 3: the forward run.
const RUN_CLIP: usize = 3;
// TODO(parity): (gameplay) the original falls back to EMD clip 2 for the
// backward walk while an enemy is in view; this engine has no enemy-visibility
// test, so the fallback is never selected.
/// EMD clip 3: the backward run. The original plays it from the body model,
/// not the no-weapon EMW.
const BACK_CLIP: usize = 3;
/// EMW clip 4: the no-weapon reach animation of the 0x0c interaction
/// (`player_behavior_0c_interact`, `attackAnim 4`).
const INTERACT_CLIP: usize = 4;

/// Ticks the settle pose is held before the breathe transition begins.
const IDLE_SETTLE_TICKS: u32 = 100;

/// Walk speed before the per-character footfall modulation.
const WALK_SPEED: i32 = 0x5D;
/// Backward walk speed.
const BACK_SPEED: i32 = 0x40;
/// Run speed.
const RUN_SPEED: i32 = 0xD2;

/// Turn added per tick when walking or backing with a direction held.
const DIAGONAL_TURN: u16 = 0x28;
/// Turn added per tick when turning in place.
const TURN_STEP: u16 = 0x60;
/// Turn added per tick when steering a run.
const RUN_TURN: u16 = 0x30;
/// Yaw offset that makes the backward walk face away from the movement.
const BACK_OFFSET: u16 = 0x800;

/// Distance of the room-action reach probe in front of the player.
pub const REACH_DISTANCE: i32 = 600;

/// 18-unit skin added to the entity radius for rectangle pushes.
const COLLISION_SKIN: i16 = 0x12;
/// The single-axis push test accepts a correction of at most 400 units, which
/// is `(push + 0x190) as u16 <= 0x320`.
const MAX_PUSH_TEST: u16 = 0x320;
/// Offset of the 400-unit bound inside the push test.
const PUSH_TEST_BIAS: i16 = 0x190;

/// No direction held selects the idle behavior.
const BEHAVIOR_IDLE: u8 = 0;
/// `Input.up` selects the walk.
const BEHAVIOR_WALK: u8 = 1;
/// A direction without `up`/`down` selects the in-place turn.
const BEHAVIOR_TURN: u8 = 2;
/// `Input.down` selects the backward walk.
const BEHAVIOR_BACK: u8 = 3;
/// `Input.up + Input.run` selects the run.
const BEHAVIOR_RUN: u8 = 4;
/// Transient value that forces the next tick through the entry path, so a
/// released stop re-arms its blend instead of continuing the old animation.
const BEHAVIOR_ENTRY: u8 = 0xFF;

/// Ticks of the run's decelerating stop segment.
const RUN_STOP_TICKS: u8 = 4;
/// Speed shed by each tick of the run's stop segment.
const RUN_STOP_DECEL: u16 = 0x1e;
/// Frame the run clip starts on when there is no walk stride to continue.
const RUN_ENTRY_FRAME: usize = 1;
/// Run entry frame that continues a walk whose pending frame is mid-stride.
const RUN_ENTRY_FRAME_STRIDE: usize = 0x0C;
/// Walk re-entry frame for a run at the start of its stride.
const WALK_REENTRY_FRAME: usize = 10;
/// Walk re-entry frame that continues a run late in its stride.
const WALK_REENTRY_FRAME_LATE: usize = 0x19;

/// Per-character footfall speed table: two frame windows and two reductions.
const CHRIS_FOOTFALL: [u8; 4] = [0x15, 0x17, 0x0D, 0x0E];
/// Jill's footfall speed table.
const JILL_FOOTFALL: [u8; 4] = [0x14, 0x16, 0x0F, 0x0F];

/// Angular step of the PS1 trig tables: `2*pi / 4096`.
const ANGLE_STEP: f64 = 0.0015339807880859375;

/// Keyboard state for one tick.
///
/// `action_held` and `action_pressed` mirror the original's action button
/// (D-pad `0x80`): the press edge starts the climb scan and the action-key
/// room probe, while the held level is what the message window sees. The
/// engine already computes both around the message window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub run: bool,
    pub action_held: bool,
    pub action_pressed: bool,
    /// `MSF2_EFFECT_ZONE` this tick: walk/run speeds halve and the locomotion
    /// animation advances only every other tick (the original's slow-motion
    /// variant).
    pub slow_motion: bool,
}

/// A footstep sound request emitted when a locomotion clip is about to apply
/// a contact frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Footstep {
    /// Player position when the footstep is emitted, before the tick's
    /// movement.
    pub pos: [i32; 3],
    /// The contact frame: `0x08`/`0x16` for the walk, turn and backward run,
    /// `0x00`/`0x0A` for the forward run.
    pub frame: u8,
    /// Entity sound type: 0 (the A footstep) for the walk, turn and backward
    /// run, 1 (the B footstep) for the forward run.
    pub sound_type: u8,
}

/// The footstep rule of one locomotion behavior: which clip's frames apply a
/// footfall and which entity sound type each contact plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FootstepRule {
    /// File the clip plays from.
    source: ClipSource,
    /// Clip index inside that file.
    clip: usize,
    /// Frames that apply a footfall.
    frames: &'static [usize],
    /// Entity sound type passed to the room's footstep lookup.
    sound_type: u8,
}

/// The original's contact frames and footstep variant per behavior.
///
/// The walk and in-place turn share the no-weapon walk cycle; the forward run
/// uses its own run cycle and the B footstep; the backward run uses the body
/// model's run cycle with the walk contact frames.
fn footstep_rule(behavior: u8) -> Option<FootstepRule> {
    match behavior {
        BEHAVIOR_WALK | BEHAVIOR_TURN => Some(FootstepRule {
            source: ClipSource::Emw,
            clip: WALK_CLIP,
            frames: &[0x08, 0x16],
            sound_type: 0,
        }),
        BEHAVIOR_RUN => Some(FootstepRule {
            source: ClipSource::Emw,
            clip: RUN_CLIP,
            frames: &[0x00, 0x0A],
            sound_type: 1,
        }),
        BEHAVIOR_BACK => Some(FootstepRule {
            source: ClipSource::Emd,
            clip: BACK_CLIP,
            frames: &[0x08, 0x16],
            sound_type: 0,
        }),
        _ => None,
    }
}

/// Stair/ladder movement state driven by the room scripts.
///
/// `stairs_height_update` supplies the ground height for the tick;
/// `set_stairs_zone` and stair doors latch an entry whose climb behaviour
/// suspends the room collision pass and holds the facing towards the target
/// until the player reaches it or the room changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StairState {
    /// Height the active ramp holds at the player's position, applied after
    /// this tick's movement. `None` when no ramp contains the player.
    pub height: Option<i32>,
    /// A `set_stairs_zone` entry is latched.
    pub in_zone: bool,
    /// The entry's ladder variant (`zoneFlags` bit `0x10`).
    pub ladder: bool,
    /// Ladder base latched by the entry.
    pub base: [u16; 2],
    /// The stair/ladder climb behaviour owns the tick: the collision boundary
    /// pass is suspended and the facing is held towards the climb target.
    pub climbing: bool,
    /// Facing held while `climbing`.
    pub locked_angle: u16,
}

/// Which model file's keyframes drive the current clip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipSource {
    /// The body EMD (idle settle and scripted poses).
    #[default]
    Emd,
    /// The no-weapon EMW (breathe, walk, turn, back, run).
    Emw,
    /// The RDT-embedded room player-animation pair (push, vault, ladders).
    Room,
}

/// Room-animation clip of the push wind-up (the original's `attackAnim 0x30`).
pub const PUSH_CLIP: usize = 0x30;
/// Room-animation clip of the push loop (`attackAnim 0x31`, one past the
/// wind-up: the behaviour increments the id when the wind-up completes).
pub const PUSH_LOOP_CLIP: usize = 0x31;
/// Room-animation clip of the vault's plain approach (`attackAnim 0x33`).
pub const VAULT_CLIP_PLAIN: usize = 0x33;
/// Room-animation clip of the vault's return side (`attackAnim 0x35`).
pub const VAULT_CLIP_RETURN: usize = 0x35;
/// Forward speed of the push behaviour while the clip is below frame `0x10`.
pub const PUSH_SPEED: i32 = 0x32;
/// Vault displacement along the approach side (X is negated for the vault).
pub const VAULT_SIDE: i32 = 0x73A;
/// Vault displacement on the Y axis (negated on the approach side).
pub const VAULT_ACROSS: i32 = 0x708;
/// Grunt SE of a push against a model without bit `0x40`.
pub const SE_PUSH_GRUNT: u16 = 0x16;
/// Grunt SE of a push against a model with bit `0x40` (the heavy grunt).
pub const SE_PUSH_GRUNT_HEAVY: u16 = 0x17;
/// SE played at the vault clip's cue frames.
pub const SE_VAULT_STEP: u16 = 0x23;
/// Sentinel sound id for the entity footstep path (the original's
/// `PlayEntitySnd(0)`).
pub const SE_FOOTSTEP: u16 = 0;

/// Room-animation clip of the plain ladder climb (`attackAnim 0x33`).
pub const LADDER_CLIP_PLAIN: usize = 0x33;
/// Room-animation clip of the variant ladder climb (`attackAnim 0x35`).
pub const LADDER_CLIP_VARIANT: usize = 0x35;
/// Approach speed of ladder states 0/1.
pub const LADDER_APPROACH_SPEED: i32 = 0x5D;
/// Per-tick turn step of the approach rotate-toward-target.
pub const LADDER_APPROACH_TURN: u16 = 0x40;
/// Distance at which the approach hands over to the turn state.
pub const LADDER_ARRIVE_DISTANCE: i32 = 900;
/// Walk-away speed of ladder state 7.
pub const LADDER_WALK_AWAY_SPEED: i32 = 0x3C;
/// Ticks the walk-away runs (the original's `attackDirection` countdown).
pub const LADDER_WALK_AWAY_TICKS: u8 = 0x0F;
/// Walk-away frame that plays the entity footstep.
pub const LADDER_WALK_AWAY_FOOTSTEP_FRAME: usize = 8;
/// Descend step-off distance on the plain ladder.
pub const LADDER_STEP_OFF: i32 = 1000;
/// Descend step-off distance on the variant ladder.
pub const LADDER_STEP_OFF_VARIANT: i32 = 2000;
/// Height the variant climb sets on its shift frame.
pub const LADDER_VARIANT_HEIGHT: i32 = 0xA8C;
/// Z displacement the variant clip applies on its shift frame.
pub const LADDER_VARIANT_SLIDE: i32 = 0x708;
/// Variant-clip frame that applies the Z displacement and the height.
pub const LADDER_VARIANT_SHIFT_FRAME: usize = 0x0F;
/// Variant-clip frame that plays the climb grunt.
pub const LADDER_VARIANT_GRUNT_FRAME: usize = 0x1A;
/// Plain-climb frame that plays the climb-end SE.
pub const LADDER_CLIMB_END_FRAME: usize = 0x32;
/// Step SE of the plain climb.
pub const SE_LADDER_STEP: u16 = 0x23;
/// SE played when the plain climb reaches its end frame.
pub const SE_LADDER_END: u16 = 0x2D;
/// Grunt SE of the variant climb.
pub const SE_LADDER_GRUNT: u16 = 0x17;

/// The transcribed ladder step-frame table: `move_speed_current` indexes the
/// frame that plays [`SE_LADDER_STEP`], and the counter advances on each match.
/// The first three entries are the shipped climb's 12/29/39; the 80/100/130
/// tail is kept for the longer stair clips the same handler serves (a zero
/// entry matches no displayed frame once the counter reaches it).
#[rustfmt::skip]
pub const LADDER_STEP_FRAMES: [u8; 64] = [
    0x0C, 0x1D, 0x27, 0x00, 0x50, 0x64, 0x82, 0x64,
    0x6B, 0x68, 0x00, 0x64, 0x64, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x64, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// A camera screen-effect rectangle written by the climb behaviours (the
/// original's `BillboardSetRect`). The camera-scroll consumer is not ported,
/// so the values are recorded for tests and a later slice; the eight words the
/// original writes map `right` to `+0x60`/`+0x70`, `-left` to
/// `+0x58`/`+0x68`, `front` to `+0x5C`/`+0x64` and `-back` to
/// `+0x6C`/`+0x74`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenEffect {
    /// The rectangle's `right` argument.
    pub right: u16,
    /// The rectangle's `left` argument.
    pub left: u16,
    /// The rectangle's `front` argument.
    pub front: u16,
    /// The rectangle's `back` argument.
    pub back: u16,
}

/// A one-shot player sound request (the original's `Play3DSnd` and
/// `PlayEntitySnd` calls in the action behaviours). The engine resolves
/// [`SE_FOOTSTEP`] through the room's footstep zones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerSound {
    /// Global SE id, or [`SE_FOOTSTEP`].
    pub id: u16,
    /// World position the sound plays at.
    pub pos: [i32; 3],
}

/// A locked action behaviour selected outside the locomotion machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LockedAction {
    /// Normal control.
    #[default]
    None,
    /// `action_behavior 0x10`: the push animation (`attackAnim 0x30`).
    Push,
    /// `action_behavior 0x0a` with msf bit 7 raised: the vault into a
    /// climbable object (clips `0x33`/`0x35` and the `0x73A`/`0x708` warp).
    Vault,
    /// `action_behavior 0x0b`: the eight-state ladder climb selected by an
    /// action press inside a marked `set_stairs_zone` zone.
    Ladder,
    /// `action_behavior 0x0c`: the object reach animation a gated action-key
    /// press selects before its item viewer opens.
    Interact,
}

/// The moving player: position, facing, collision radius and animation.
#[derive(Clone, Debug)]
pub struct PlayerState {
    /// Room position; Y is the floor height (0 in room 1001).
    pub pos: [i32; 3],
    /// 12-bit yaw.
    pub angle: u16,
    /// Chris 422, Jill 372.
    pub radius: i32,
    /// The original's per-entity collision callback flags (`Entity.collisionFlags`)
    /// as the room collision pass reads them for the player. The player's byte
    /// aliases the health status flags, so its `0x10` bit is raised by the
    /// player init on every room spawn; while it is set the collision pass
    /// skips shape-5 records (the large stair/corridor floor volumes).
    ///
    /// TODO(parity): the original reads the live health byte here, so a script
    /// that clears `0x10` stops skipping shape 5. The port keeps the spawn
    /// value until the health byte's collision-flag aliasing is modelled.
    pub collision_flags: u8,
    /// Clip playback state, interpreted against `clip_source`.
    pub anim: AnimPlayer,
    /// Which file's clips `anim` indexes.
    pub clip_source: ClipSource,
    /// 0 idle, 1 walk, 2 turn, 3 back, 4 run (internal).
    pub behavior: u8,
    /// Idle sequence phase: 0 settle, 1 breathe transition, 2 breathe loop.
    pub idle_phase: u8,
    /// Ticks spent in the idle behavior.
    pub idle_ticks: u32,
    /// Stair/ladder state set by the room's action probes.
    pub stairs: StairState,
    /// The locked action behaviour owning the tick, if any.
    pub locked: LockedAction,
    /// Action state byte of the locked behaviour.
    pub action_state: u8,
    /// The push bit (msf `0x40`) raised by `tick_objects` last tick. The
    /// behaviour starts the tick after the bit is raised, the original's
    /// one-frame hand-off.
    pub object_push: bool,
    /// The climb/vault transition bit (msf `0x80`): raised by the climb scan,
    /// cleared when the vault settles.
    pub vault_bit: bool,
    /// `zoneFlags` bit `0x10` for the current vault: the return side.
    pub vault_return: bool,
    /// Slot of the object the push probe last held, for the grunt SE.
    pub push_object: Option<u8>,
    /// Slot of the climb candidate latched by `check_climb_object`.
    pub climb_object: Option<u8>,
    /// Whether the pushed object's model byte carries the heavy-grunt `0x40`.
    pub push_heavy: bool,
    /// `attackDirection` of the latched climb candidate (`-1`/`1`).
    pub attack_direction: i8,
    /// `move_speed_current`, used by the push, vault and ladder state machines.
    pub move_speed_current: u16,
    /// The ladder climb's state 8 ran: the room probe clears zone flag `0x10`
    /// and `MSF_LADDER_DOWN` and returns the message flag next tick.
    pub ladder_release: bool,
    /// The reach request [`PlayerState::begin_interact`] armed, consumed when
    /// the EMW reach clip finishes.
    interact_request: Option<ReachRequest>,
    /// The reach request handed to the engine when the reach clip finished.
    interact_finished: Option<ReachRequest>,
    /// The last tick's input; `tick_objects` reads the action edges from here.
    pub input: Input,
    /// Slow-motion animation cadence counter (the original's `attackDirection`
    /// reuse): 0 applies the frame and re-arms to 1, 1 skips the frame.
    pub slow_counter: i16,
    /// Remaining ticks of the run's four-tick decelerating stop; 0 outside a
    /// stop. The first stop tick switches the animation to the body idle clip
    /// with a fresh blend and sheds [`RUN_STOP_DECEL`] from the run speed.
    pub run_stop_ticks: u8,
    /// The next tick selects its behavior through the normal entry path even
    /// when the requested behavior matches the current one; a finished run
    /// stop sets it so idle re-arms a fresh blend.
    pub fresh_entry: bool,
    /// One-shot sound requests emitted since the last
    /// [`PlayerState::take_sounds`].
    sounds: Vec<PlayerSound>,
    /// Footstep events emitted since the last [`PlayerState::take_footsteps`].
    footsteps: Vec<Footstep>,
    /// Camera screen-effect rectangles recorded since the last
    /// [`PlayerState::take_screen_effects`].
    screen_effects: Vec<ScreenEffect>,
}

/// Spawn in the middle of the first walkable zone (or the origin when none).
pub fn spawn(id: RoomId, room: &RoomState) -> PlayerState {
    let pos = room.walk_zones.first().map_or([0, 0, 0], |zone| {
        [
            (i32::from(zone.x1) + i32::from(zone.x2)) / 2,
            0,
            (i32::from(zone.z1) + i32::from(zone.z2)) / 2,
        ]
    });

    PlayerState {
        pos,
        angle: 0,
        radius: if id.player_flag & 1 == 0 {
            CHRIS_RADIUS
        } else {
            JILL_RADIUS
        },
        collision_flags: PLAYER_COLLISION_FLAGS,
        anim: AnimPlayer::new(SETTLE_CLIP),
        clip_source: ClipSource::Emd,
        behavior: BEHAVIOR_IDLE,
        idle_phase: 0,
        idle_ticks: 0,
        stairs: StairState::default(),
        locked: LockedAction::None,
        action_state: 0,
        object_push: false,
        vault_bit: false,
        vault_return: false,
        push_object: None,
        climb_object: None,
        push_heavy: false,
        attack_direction: 0,
        move_speed_current: 0,
        ladder_release: false,
        interact_request: None,
        interact_finished: None,
        input: Input::default(),
        slow_counter: 0,
        run_stop_ticks: 0,
        fresh_entry: false,
        sounds: Vec::new(),
        footsteps: Vec::new(),
        screen_effects: Vec::new(),
    }
}

/// One fixed 30 Hz tick: choose behavior, move, collide, advance animation.
///
/// `emd_clips` are the body model's clips (idle settle); `emw_clips` are the
/// no-weapon clips (breathe, walk, turn, back, run).
pub fn update(
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    input: Input,
) {
    update_with_room(player, room, emd_clips, emw_clips, &[], input);
}

/// [`update`] with the room's own player-animation clips (RDT pointer slots
/// 9/10) available to the locked action behaviours. A room without the pair
/// falls back: the push still winds up and releases, the vault skips straight
/// to its warp.
pub fn update_with_room(
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
    input: Input,
) {
    player.input = input;
    // The push bit raised by `tick_objects` last tick forces the push
    // behaviour this tick, the original's one-frame hand-off.
    if player.object_push && player.locked == LockedAction::None {
        player.locked = LockedAction::Push;
        player.action_state = 0;
    }
    if player.locked != LockedAction::None {
        update_locked(player, room, emd_clips, emw_clips, room_clips);
        return;
    }

    // The run's stop segment owns the tick until its four decelerating ticks
    // have played; no input cancels it (the original's action_state 3).
    if player.run_stop_ticks != 0 {
        run_stop_tick(player, room, emd_clips, emw_clips, room_clips);
        return;
    }
    // A finished stop hands control back to idle on the next tick, which must
    // re-arm the clip blend even when the requested behavior is unchanged.
    if player.fresh_entry {
        player.fresh_entry = false;
        player.behavior = BEHAVIOR_ENTRY;
    }

    let behavior = behavior_for(input);

    // Walk release: the original's walk handler keeps the walk speed and the
    // walk pose for one tick (no frame is consumed), then idle takes over.
    if player.behavior == BEHAVIOR_WALK && behavior != BEHAVIOR_WALK && !input.up {
        player.locomotion_tick(room, BEHAVIOR_WALK, i32::from(player.move_speed_current));
        player.enter_behavior(BEHAVIOR_IDLE);
        player.set_clip(ClipSource::Emd, SETTLE_CLIP);
        return;
    }
    // Turn release: the original's turn handler also releases through
    // behavior 0 without consuming a frame, so the turn pose stays one tick.
    if player.behavior == BEHAVIOR_TURN && behavior != BEHAVIOR_TURN && !input.left && !input.right
    {
        player.enter_behavior(BEHAVIOR_IDLE);
        player.set_clip(ClipSource::Emd, SETTLE_CLIP);
        return;
    }
    // Run release: the original's run handler starts its four-tick
    // decelerating stop towards the body idle clip.
    if player.behavior == BEHAVIOR_RUN && behavior != BEHAVIOR_RUN && !input.up {
        player.run_stop_ticks = RUN_STOP_TICKS;
        run_stop_tick(player, room, emd_clips, emw_clips, room_clips);
        return;
    }

    if behavior != player.behavior {
        // A walk/run flip keeps the stride: the entry frame is the one that
        // continues the phase the abandoned clip was about to reach.
        let stride = player.entry_stride(behavior, emw_clips);
        player.enter_behavior(behavior);
        player.set_clip(entry_clip_source(behavior), entry_clip(behavior));
        if let Some(frame) = stride {
            player.anim.frame = frame;
        } else if behavior == BEHAVIOR_RUN {
            player.anim.frame = RUN_ENTRY_FRAME;
        }
    }

    // The stair/ladder climb behaviour locks the facing towards its target;
    // otherwise the input steers the walk as usual.
    player.angle = if player.stairs.climbing {
        player.stairs.locked_angle & 0x0FFF
    } else {
        turned_angle(player.angle, behavior, input)
    };

    let mut speed = match behavior {
        BEHAVIOR_WALK => walk_speed(player.radius, player.anim.frame),
        BEHAVIOR_TURN => 0,
        BEHAVIOR_BACK => BACK_SPEED,
        BEHAVIOR_RUN => RUN_SPEED,
        _ => 0,
    };
    // `MSF2_EFFECT_ZONE` halves the walk and run speeds; the animation
    // cadence below also drops to every other tick. The turn and backward
    // behaviours have no slow variant in the original.
    let slow_locomotion = input.slow_motion && matches!(behavior, BEHAVIOR_WALK | BEHAVIOR_RUN);
    if slow_locomotion {
        speed /= 2;
    }
    // The released walk keeps the last walk tick's speed (the original's
    // `move_speed_current`), so the release tick and the stop segment can read
    // it back.
    if matches!(behavior, BEHAVIOR_WALK | BEHAVIOR_RUN) {
        player.move_speed_current = speed.max(0) as u16;
    }
    // The original checks the pending animation frame and plays the footstep
    // before `Joint_move` applies it and before the tick's movement, so the
    // sound uses the pre-move position.
    player.emit_footsteps(behavior, emd_clips, emw_clips, room_clips);

    player.locomotion_tick(room, behavior, speed);

    if behavior == BEHAVIOR_IDLE {
        player.slow_counter = 0;
        player.idle_ticks = player.idle_ticks.saturating_add(1);
        match player.idle_phase {
            0 => {
                player.advance(emd_clips, emw_clips, room_clips);
                if player.idle_ticks >= IDLE_SETTLE_TICKS {
                    player.idle_phase = 1;
                    player.set_clip(ClipSource::Emw, BREATHE_IN_CLIP);
                }
            }
            1 => {
                if player.advance(emd_clips, emw_clips, room_clips) {
                    player.idle_phase = 2;
                    player.set_clip(ClipSource::Emw, BREATHE_CLIP);
                }
            }
            _ => {
                player.advance(emd_clips, emw_clips, room_clips);
            }
        }
    } else if slow_locomotion {
        // The original keeps a counter in `attackDirection`: a frame applies
        // when it was 0, then the counter re-arms to 1 and the next tick skips.
        let previous = player.slow_counter;
        player.slow_counter -= 1;
        if previous == 0 {
            player.advance(emd_clips, emw_clips, room_clips);
            player.slow_counter = 1;
        }
    } else {
        player.slow_counter = 0;
        player.advance(emd_clips, emw_clips, room_clips);
    }
}

/// One tick of the run's stop segment.
///
/// The original's run handler switches to the body idle clip on the first
/// stop tick and sheds [`RUN_STOP_DECEL`] from `move_speed_current` on each of
/// the four ticks (`0xD2` eases out as 180/150/120/90), steering with the run
/// step. The fourth tick hands control back to idle through [`BEHAVIOR_ENTRY`],
/// so the next tick re-arms a fresh three-step blend into the settle pose.
fn run_stop_tick(
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    if player.run_stop_ticks == RUN_STOP_TICKS {
        player.set_clip(ClipSource::Emd, SETTLE_CLIP);
    }
    if !player.stairs.climbing {
        player.angle = turned_angle(player.angle, BEHAVIOR_RUN, player.input);
    }
    player.move_speed_current = player.move_speed_current.saturating_sub(RUN_STOP_DECEL);
    player.locomotion_tick(room, BEHAVIOR_RUN, i32::from(player.move_speed_current));
    player.advance(emd_clips, emw_clips, room_clips);
    player.run_stop_ticks -= 1;
    if player.run_stop_ticks == 0 {
        player.enter_behavior(BEHAVIOR_IDLE);
        player.fresh_entry = true;
    }
}

impl PlayerState {
    fn set_clip(&mut self, source: ClipSource, clip: usize) {
        self.clip_source = source;
        self.anim.set_clip(clip);
    }

    fn advance(&mut self, emd_clips: &[Clip], emw_clips: &[Clip], room_clips: &[Clip]) -> bool {
        match self.clip_source {
            ClipSource::Emd => self.anim.update(emd_clips),
            ClipSource::Emw => self.anim.update(emw_clips),
            ClipSource::Room => self.anim.update(room_clips),
        }
    }

    /// Select a locomotion behavior with its entry state: the idle sequence
    /// and the slow-motion cadence restart, exactly like the original's
    /// action state 0.
    fn enter_behavior(&mut self, behavior: u8) {
        self.behavior = behavior;
        self.idle_ticks = 0;
        self.idle_phase = 0;
        self.slow_counter = 0;
    }

    /// The run or walk entry frame that continues the current stride, when the
    /// behavior change crosses the walk and run clips.
    ///
    /// The original reads the frame after the tick the handler would have
    /// played: a consuming tick advances the pending frame (wrapping at the
    /// clip end), a held tick leaves it. A walk headed into the stride's
    /// middle (10..=0x19) enters the run at `0x0C`, everything else at frame
    /// 1; a run whose next frame is early (1..=0x0B) re-enters the walk at
    /// `0x19`, everything else at frame 10.
    fn entry_stride(&self, behavior: u8, emw_clips: &[Clip]) -> Option<usize> {
        if self.clip_source != ClipSource::Emw {
            return None;
        }
        let next = if self.anim.timing > 1 {
            self.anim.frame
        } else {
            match emw_clips.get(self.anim.clip) {
                Some(clip) if self.anim.frame + 1 >= clip.frames.len() => 0,
                _ => self.anim.frame + 1,
            }
        };
        if behavior == BEHAVIOR_RUN && self.anim.clip == WALK_CLIP {
            Some(if (10..=0x19).contains(&next) {
                RUN_ENTRY_FRAME_STRIDE
            } else {
                RUN_ENTRY_FRAME
            })
        } else if behavior == BEHAVIOR_WALK && self.anim.clip == RUN_CLIP {
            Some(if next != 0 && next < 0x0C {
                WALK_REENTRY_FRAME_LATE
            } else {
                WALK_REENTRY_FRAME
            })
        } else {
            None
        }
    }

    /// Move one tick for a locomotion behavior, with the room collision pass
    /// and the ramp height the room scripts own.
    fn locomotion_tick(&mut self, room: &RoomState, behavior: u8, speed: i32) {
        let offset = if behavior == BEHAVIOR_BACK {
            BACK_OFFSET
        } else {
            0
        };
        let prev = self.pos;
        let (dx, dz) = rotate_speed(self.angle, offset, speed);
        let proposed = [prev[0] + dx, prev[1], prev[2] + dz];
        // The climb behaviour suspends the collision boundary pass, exactly
        // like the original's `update_player_anim` does for 0x11.
        self.pos = if self.stairs.climbing {
            proposed
        } else {
            resolve_collision(
                &room.collision,
                prev,
                proposed,
                self.radius,
                self.collision_flags,
            )
        };
        // `stairs_height_update` owns the player's height on the ramp.
        if let Some(height) = self.stairs.height {
            self.pos[1] = height;
        }
    }

    /// Emit a footstep when the behavior's locomotion clip is about to apply
    /// one of its contact frames.
    ///
    /// The original tests `animation_frame_id` before `Joint_move` applies the
    /// frame, so the pending frame is the one checked. A contact frame whose
    /// own timing holds it therefore fires exactly once, while a contact
    /// reached with the previous frame still held fires on each held tick
    /// until it is applied - both behaviours fall out of checking the pending
    /// frame here.
    fn emit_footsteps(
        &mut self,
        behavior: u8,
        emd_clips: &[Clip],
        emw_clips: &[Clip],
        room_clips: &[Clip],
    ) {
        let Some(rule) = footstep_rule(behavior) else {
            return;
        };
        if self.clip_source != rule.source || self.anim.clip != rule.clip {
            return;
        }

        let clips = match rule.source {
            ClipSource::Emd => emd_clips,
            ClipSource::Emw => emw_clips,
            ClipSource::Room => room_clips,
        };
        let Some(clip) = clips.get(rule.clip) else {
            return;
        };
        let frame = self.anim.frame;
        if frame < clip.frames.len() && rule.frames.contains(&frame) {
            self.footsteps.push(Footstep {
                pos: self.pos,
                frame: frame as u8,
                sound_type: rule.sound_type,
            });
        }
    }

    /// Drain the footstep events emitted since the last call.
    pub fn take_footsteps(&mut self) -> Vec<Footstep> {
        std::mem::take(&mut self.footsteps)
    }

    /// Drain the one-shot sound requests emitted since the last call.
    pub fn take_sounds(&mut self) -> Vec<PlayerSound> {
        std::mem::take(&mut self.sounds)
    }

    /// Select the gated object reach (`action_behavior 0x0c`) for `request`.
    /// The original raises the health lock and message bit in the room action
    /// layer and starts the behaviour with `animFrameId = 1`; the port starts
    /// the locked action and reports the request back when the clip finishes.
    pub fn begin_interact(&mut self, request: ReachRequest) {
        self.locked = LockedAction::Interact;
        self.action_state = 0;
        self.interact_request = Some(request);
    }

    /// Take the reach request whose clip just finished, if any. The engine
    /// raises the viewer flag through [`crate::game::GameState::complete_reach`].
    pub fn take_interact_finished(&mut self) -> Option<ReachRequest> {
        self.interact_finished.take()
    }

    /// Queue a one-shot sound at the player's current position.
    fn queue_sound(&mut self, id: u16) {
        self.sounds.push(PlayerSound { id, pos: self.pos });
    }

    /// Move the locked behaviour's forward step with the room collision pass.
    fn locked_step(&mut self, room: &RoomState, speed: i32) {
        let (dx, dz) = rotate_speed(self.angle, 0, speed);
        let prev = self.pos;
        let proposed = [prev[0] + dx, prev[1], prev[2] + dz];
        self.pos = resolve_collision(
            &room.collision,
            prev,
            proposed,
            self.radius,
            self.collision_flags,
        );
    }
}

/// Run the locked action behaviour that owns this tick.
///
/// The push keeps the room collision pass (the original only suspends it for
/// the ladder/stairs behaviour); the vault suspends it, because its warp moves
/// the player across the object's volume.
fn update_locked(
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    match player.locked {
        LockedAction::Push => update_push(player, room, emd_clips, emw_clips, room_clips),
        LockedAction::Vault => update_vault(player, emd_clips, emw_clips, room_clips),
        LockedAction::Ladder => update_ladder(player, room, emd_clips, emw_clips, room_clips),
        LockedAction::Interact => update_interact(player, emd_clips, emw_clips, room_clips),
        LockedAction::None => {}
    }
}

/// `player_behavior_0c_interact` (0x00495e00): the object reach a gated
/// action-key press selects.
///
/// State 0 selects the no-weapon EMW reach clip (`attackAnim 4`, blend 3);
/// state 1 plays it and, when it completes, hands the reach request to the
/// engine (which raises the viewer flag and clears the health lock) and arms
/// state 2; state 2 plays one EMD clip-0 tick and returns control.
fn update_interact(
    player: &mut PlayerState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    match player.action_state {
        0 => {
            player.set_clip(ClipSource::Emw, INTERACT_CLIP);
            player.anim.frame = 0;
            player.anim.display_frame = 0;
            player.anim.timing = 0;
            player.action_state = 1;
        }
        1 => {
            if player.advance(emd_clips, emw_clips, room_clips) {
                player.interact_finished = player.interact_request;
                player.action_state = 2;
                // The original switches to the body clip with a snap
                // (`unk_8c = 0`) and frame 0.
                player.set_clip(ClipSource::Emd, SETTLE_CLIP);
                player.anim.blend_counter = 0;
                player.anim.frame = 0;
                player.anim.display_frame = 0;
                player.anim.timing = 0;
            }
        }
        2 => {
            // One body clip-0 tick, then back to the pad-driven control.
            player.advance(emd_clips, emw_clips, room_clips);
            player.locked = LockedAction::None;
            player.action_state = 0;
            player.enter_idle();
        }
        _ => {}
    }
}

/// `player_behavior_10_push` (0x00457230): wind-up, the forward creep while the
/// push bit stays raised, then release.
///
/// The clip's frame 1 plays the grunt whose id depends on the pushed object's
/// model byte. A room without the clip still winds up (one tick) and releases
/// the moment the push bit drops.
#[allow(clippy::collapsible_match)]
fn update_push(
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    let windup = room_clips
        .get(PUSH_CLIP)
        .is_some_and(|clip| !clip.frames.is_empty());
    let looping = room_clips
        .get(PUSH_LOOP_CLIP)
        .is_some_and(|clip| !clip.frames.is_empty());
    match player.action_state {
        0 => {
            player.set_clip(ClipSource::Room, PUSH_CLIP);
            player.move_speed_current = 0;
            player.action_state = 1;
        }
        1 => {
            if !windup || player.advance(emd_clips, emw_clips, room_clips) {
                player.set_clip(ClipSource::Room, PUSH_LOOP_CLIP);
                player.action_state = 2;
                player.move_speed_current = 0;
            }
        }
        2 => {
            if looping {
                player.advance(emd_clips, emw_clips, room_clips);
            }
            // The locomotion runs on every frame below 0x10, even the frame
            // the push bit drops: the original tests the bit only afterwards.
            if player.anim.display_frame < 0x10 {
                player.move_speed_current = PUSH_SPEED as u16;
                player.locked_step(room, PUSH_SPEED);
            }
            if !player.object_push {
                player.anim.frame = 0;
                player.anim.display_frame = 0;
                player.anim.timing = 0;
                player.move_speed_current = 0;
                player.action_state = 3;
            } else if player.anim.display_frame == 1 {
                let id = if player.push_heavy {
                    SE_PUSH_GRUNT_HEAVY
                } else {
                    SE_PUSH_GRUNT
                };
                player.queue_sound(id);
            }
        }
        3 => {
            if !looping || player.advance(emd_clips, emw_clips, room_clips) {
                // Back to normal control; the message flag the original
                // returns is represented by the port's own message state.
                player.locked = LockedAction::None;
                player.action_state = 0;
                player.push_object = None;
                player.enter_idle();
            }
        }
        _ => {}
    }
}

/// `player_door_open_sequence` (0x00457390) with msf bit 7 raised: the vault
/// into a climbable object.
///
/// State 0 turns the player square with `(angle & 0x3FC) >> 2` per tick; state
/// 1 picks clip `0x33`/`0x35`; state 2 runs it with the SE schedule; state 3
/// warps `0x73A` along the approach side and `0x708` vertically.
fn update_vault(
    player: &mut PlayerState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    match player.action_state {
        0 => {
            // The turn runs over the walk pose: the original selects
            // `attackAnim` 2 (the unarmed walk clip) and resets the frame each
            // tick, so the pose is the walk's first frame while the player
            // squares up.
            player.set_clip(ClipSource::Emw, WALK_CLIP);
            player.move_speed_current = 0;
            let angle = player.angle & 0x0FFF;
            let step = (angle & 0x3FC) >> 2;
            player.angle = if angle & 0x200 != 0 {
                angle.wrapping_add(step)
            } else {
                angle.wrapping_sub(step)
            } & 0x0FFF;
            if player.angle & 0x3E0 != 0 {
                return;
            }
            player.action_state = 1;
        }
        1 => {
            let clip = if player.vault_return {
                VAULT_CLIP_RETURN
            } else {
                VAULT_CLIP_PLAIN
            };
            player.set_clip(ClipSource::Room, clip);
            player.move_speed_current = 0;
            player.action_state = 2;
        }
        _ => {}
    }

    if player.action_state == 2 {
        if !player.vault_clip_present(room_clips) {
            // A room without the climb clips cannot play the animation; the
            // warp still runs so the vault lands (documented fallback).
            player.action_state = 3;
        } else {
            let frame = player.anim.display_frame as i32;
            let mut speed = i32::from(player.move_speed_current as i16);
            if speed * 15 - frame == -0xC {
                player.queue_sound(SE_VAULT_STEP);
                speed += 1;
            }
            if speed == 3 {
                speed = 7;
            }
            if speed == 7 && frame == 0x35 {
                player.queue_sound(SE_VAULT_STEP);
            }
            if player.vault_return && speed == 2 {
                speed = 5;
            }
            if speed > 4 && speed * 9 - frame == 1 {
                speed = 6;
                player.queue_sound(SE_FOOTSTEP);
            }
            player.move_speed_current = speed as u16;
            if player.advance(emd_clips, emw_clips, room_clips) {
                player.action_state = 3;
            }
        }
    }

    if player.action_state == 3 {
        player.warp_through_object();
    }
}

/// `player_behavior_0b_ladder` (0x00496480): the eight-state ladder climb.
///
/// States 0/1 walk to the latched base at speed `0x5D`, rotating toward it in
/// `0x40` steps; state 2 eases the remaining angle onto a quadrant; state 3
/// picks the room clip `0x33`/`0x35` and writes the first screen-effect
/// rectangle; state 4 runs the climb with its step SEs, the variant frame
/// `0x0F` displacement/height and the frame `0x1A` grunt; state 5 steps off;
/// states 6/7 walk away; state 8 releases the zone and control.
///
/// The room collision pass stays suspended through the approach (the latched
/// base can sit inside the shaft's collision volume); the walk-away uses the
/// normal resolved step.
#[allow(clippy::too_many_lines)]
fn update_ladder(
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    if player.action_state == 0 {
        player.action_state = 1;
        player.move_speed_current = LADDER_APPROACH_SPEED as u16;
        player.set_clip(ClipSource::Emw, WALK_CLIP);
    }
    match player.action_state {
        1 => {
            let base = [
                i32::from(player.stairs.base[0]),
                i32::from(player.stairs.base[1]),
            ];
            rotate_toward(player, base[0], base[1], LADDER_APPROACH_TURN);
            player.advance(emd_clips, emw_clips, room_clips);
            let speed = i32::from(player.move_speed_current as i16);
            let (dx, dz) = rotate_speed(player.angle, 0, speed);
            player.pos[0] += dx;
            player.pos[2] += dz;
            let dx = i64::from(player.pos[0] - base[0]);
            let dz = i64::from(player.pos[2] - base[1]);
            if dx * dx + dz * dz
                < i64::from(LADDER_ARRIVE_DISTANCE) * i64::from(LADDER_ARRIVE_DISTANCE)
            {
                player.action_state = 2;
            }
        }
        2 => {
            player.advance(emd_clips, emw_clips, room_clips);
            let angle = player.angle & 0x0FFF;
            let step = (angle & 0x3FC) >> 2;
            let turn = if angle & 0x400 == 0 {
                step
            } else {
                step.wrapping_neg()
            };
            player.angle = angle.wrapping_add(turn) & 0x0FFF;
            if player.angle & 0x3E0 == 0 {
                player.action_state = 3;
            }
        }
        3 => {
            let clip = if player.stairs.ladder {
                LADDER_CLIP_VARIANT
            } else {
                LADDER_CLIP_PLAIN
            };
            player.set_clip(ClipSource::Room, clip);
            player.move_speed_current = 0;
            player.action_state = 4;
            player.screen_effects.push(ScreenEffect {
                right: 800,
                left: 700,
                front: 700,
                back: 700,
            });
            ladder_climb_tick(player, emd_clips, emw_clips, room_clips);
        }
        4 => ladder_climb_tick(player, emd_clips, emw_clips, room_clips),
        5 => {
            ladder_descend(player);
        }
        6 => ladder_walk_away_start(player),
        7 => {
            if player.anim.display_frame == LADDER_WALK_AWAY_FOOTSTEP_FRAME {
                player.queue_sound(SE_FOOTSTEP);
            }
            player.advance(emd_clips, emw_clips, room_clips);
            let speed = i32::from(player.move_speed_current as i16);
            player.locked_step(room, speed);
            if player.attack_direction == 0 {
                player.action_state = 8;
                player.queue_sound(SE_FOOTSTEP);
            } else {
                player.attack_direction -= 1;
            }
        }
        8 => {
            // Release: the port's room probe clears zone flag 0x10 and
            // `MSF_LADDER_DOWN` and returns the message flag next tick.
            player.stairs.ladder = false;
            player.ladder_release = true;
            player.locked = LockedAction::None;
            player.action_state = 0;
            player.move_speed_current = 0;
            player.attack_direction = 0;
            player.enter_idle();
        }
        _ => {}
    }
}

/// State 4 of the ladder climb: the SE schedule, the variant shift and the
/// per-tick clip advance that ends the state on completion.
fn ladder_climb_tick(
    player: &mut PlayerState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    let frame = player.anim.display_frame;
    if player.stairs.ladder {
        if frame == LADDER_VARIANT_SHIFT_FRAME {
            let slide = if player.angle > 0x800 {
                LADDER_VARIANT_SLIDE
            } else {
                -LADDER_VARIANT_SLIDE
            };
            player.pos[2] += slide;
            player.pos[1] = LADDER_VARIANT_HEIGHT;
        }
        if frame == LADDER_VARIANT_GRUNT_FRAME {
            player.queue_sound(SE_LADDER_GRUNT);
        }
    } else {
        let step = LADDER_STEP_FRAMES
            .get(usize::from(player.move_speed_current))
            .copied();
        if step == Some(frame as u8) {
            player.queue_sound(SE_LADDER_STEP);
            player.move_speed_current = player.move_speed_current.wrapping_add(1);
        }
        if frame == LADDER_CLIMB_END_FRAME {
            player.queue_sound(SE_LADDER_END);
        }
    }
    if player.advance(emd_clips, emw_clips, room_clips) {
        player.action_state += 1;
    }
}

/// State 5 of the ladder climb: step off the ladder and either release
/// (variant) or set up the walk-away.
fn ladder_descend(player: &mut PlayerState) {
    player.move_speed_current = 0;
    player.anim.frame = 0;
    player.anim.display_frame = 0;
    player.anim.timing = 0;
    let direction = if player.angle > 0x800 { 1 } else { -1 };
    let distance = if player.stairs.ladder {
        LADDER_STEP_OFF_VARIANT
    } else {
        LADDER_STEP_OFF
    };
    player.pos[2] += direction * distance;
    player.pos[1] = if player.stairs.ladder {
        LADDER_VARIANT_HEIGHT
    } else {
        0
    };
    player.screen_effects.push(ScreenEffect {
        right: 500,
        left: 500,
        front: 700,
        back: 700,
    });
    if player.stairs.ladder {
        player.action_state = 8;
    } else {
        ladder_walk_away_start(player);
    }
}

/// State 6: switch to the walk-away clip and start the 15-tick countdown.
fn ladder_walk_away_start(player: &mut PlayerState) {
    player.action_state = 7;
    player.move_speed_current = LADDER_WALK_AWAY_SPEED as u16;
    player.attack_direction = LADDER_WALK_AWAY_TICKS as i8;
    player.set_clip(ClipSource::Emw, WALK_CLIP);
}

/// The original's `entity_rotate_toward_target` for the ladder approach: turn
/// the facing toward `(target_x, target_z)` by `step`, snapping when the
/// remaining angle is inside `2*step`.
fn rotate_toward(player: &mut PlayerState, target_x: i32, target_z: i32, step: u16) {
    let target =
        crate::sfx::angle_between_xz(player.pos[0], player.pos[2], target_x, target_z) & 0x0FFF;
    let delta = step.wrapping_sub(player.angle).wrapping_add(target) & 0x0FFF;
    if i32::from(delta) < i32::from(step as i16) * 2 {
        player.angle = target;
        return;
    }
    player.angle = player.angle.wrapping_sub(step) & 0x0FFF;
    if delta < 0x801 {
        player.angle = player.angle.wrapping_add(step.wrapping_mul(2)) & 0x0FFF;
    }
}

impl PlayerState {
    /// Drain the camera screen-effect rectangles recorded since the last call.
    pub fn take_screen_effects(&mut self) -> Vec<ScreenEffect> {
        std::mem::take(&mut self.screen_effects)
    }

    /// Whether the vault's selected room clip exists and has frames.
    fn vault_clip_present(&self, room_clips: &[Clip]) -> bool {
        room_clips
            .get(self.anim.clip)
            .is_some_and(|clip| !clip.frames.is_empty())
    }

    /// The vault's warp: `0x73A` sideways (X for an X-facing approach, Z
    /// otherwise, negated for the vault), `0x708` vertically (positive on the
    /// return side). The transition bit and the return-side flag are consumed
    /// and control returns to the locomotion machine.
    fn warp_through_object(&mut self) {
        let dir = i32::from(self.attack_direction);
        let sideways = self.angle & 0x400 != 0;
        let (x, z) = if sideways {
            (0, dir * VAULT_SIDE)
        } else {
            (-(dir * VAULT_SIDE), 0)
        };
        let y = if self.vault_return {
            VAULT_ACROSS
        } else {
            -VAULT_ACROSS
        };
        self.pos = [self.pos[0] + x, self.pos[1] + y, self.pos[2] + z];
        self.vault_bit = false;
        self.vault_return = false;
        self.locked = LockedAction::None;
        self.action_state = 0;
        self.move_speed_current = 0;
        self.enter_idle();
    }

    /// Return the visible player to the idle stance after a locked behaviour.
    fn enter_idle(&mut self) {
        self.behavior = BEHAVIOR_IDLE;
        self.idle_phase = 0;
        self.idle_ticks = 0;
        // A locked behaviour interrupts a stop (or a stop hand-off) and owns
        // the next selection: neither survives the return to idle.
        self.run_stop_ticks = 0;
        self.fresh_entry = false;
        self.set_clip(ClipSource::Emd, SETTLE_CLIP);
    }
}

/// Whether the collision resolver would push a body of `radius` at `pos`.
///
/// Shape 4 records are soft zones and shapes 0/2 have no handler, so neither
/// blocks. A shape 1 rectangle blocks whenever the grown box contains the
/// point; a shape 3 circle only when it actually overlaps (its grown box
/// corner does not push). A shape 5 rectangle blocks the same way as shape 1
/// unless `collision_flags` has bit `0x10` set, the original's per-entity skip
/// ([`PlayerState::collision_flags`]).
pub fn position_blocked(room: &RoomState, pos: [i32; 3], radius: i32, collision_flags: u8) -> bool {
    room.collision
        .records(pos[0], pos[2])
        .iter()
        .any(|rect| match rect.kind & 0xFF {
            1 => classify(pos[0], pos[2], rect, radius).is_some(),
            5 => collision_flags & 0x10 == 0 && classify(pos[0], pos[2], rect, radius).is_some(),
            3 => circle_overlap(rect, pos[0], pos[2], radius).is_some(),
            _ => false,
        })
}

/// Pick the camera cut for the player position: find the zone group whose
/// header `cam_from` equals `current`, then the first following zone with the
/// same `cam_from` whose quad contains the player; return its `cam_to` when it
/// is a valid cut index, otherwise `current`.
pub fn camera_for_position(room: &RoomState, current: usize, pos: [i32; 3]) -> usize {
    let Some(header) = room
        .zones
        .iter()
        .position(|zone| zone.cam_from >= 0 && zone.cam_from as usize == current)
    else {
        return current;
    };

    for zone in &room.zones[header + 1..] {
        if zone.cam_from < 0 || zone.cam_from as usize != current {
            break;
        }
        if zone.contains(pos[0], pos[2]) {
            let cam_to = zone.cam_to as i64;
            if (0..room.cuts.len() as i64).contains(&cam_to) {
                return cam_to as usize;
            }
            return current;
        }
    }
    current
}

/// Map one tick's held keys to the internal behavior code.
fn behavior_for(input: Input) -> u8 {
    if input.up && input.run {
        BEHAVIOR_RUN
    } else if input.up {
        BEHAVIOR_WALK
    } else if input.down {
        BEHAVIOR_BACK
    } else if input.left || input.right {
        BEHAVIOR_TURN
    } else {
        BEHAVIOR_IDLE
    }
}

/// The clip a behavior starts on when it is selected.
fn entry_clip(behavior: u8) -> usize {
    match behavior {
        BEHAVIOR_WALK | BEHAVIOR_TURN => WALK_CLIP,
        BEHAVIOR_BACK => BACK_CLIP,
        BEHAVIOR_RUN => RUN_CLIP,
        _ => SETTLE_CLIP,
    }
}

/// Which file the entry clip of a behavior lives in.
fn entry_clip_source(behavior: u8) -> ClipSource {
    match behavior {
        BEHAVIOR_WALK | BEHAVIOR_TURN | BEHAVIOR_RUN => ClipSource::Emw,
        _ => ClipSource::Emd,
    }
}

/// Apply the behavior's per-tick yaw step; right is positive, left negative.
fn turned_angle(angle: u16, behavior: u8, input: Input) -> u16 {
    let step = match behavior {
        BEHAVIOR_WALK | BEHAVIOR_BACK => DIAGONAL_TURN,
        BEHAVIOR_TURN => TURN_STEP,
        BEHAVIOR_RUN => RUN_TURN,
        _ => return angle & 0x0FFF,
    };

    let mut angle = angle & 0x0FFF;
    if input.right {
        angle = angle.wrapping_add(step) & 0x0FFF;
    }
    if input.left {
        angle = angle.wrapping_sub(step) & 0x0FFF;
    }
    angle
}

/// The walk speed for the current clip frame, with the footfall modulation.
fn walk_speed(radius: i32, frame: usize) -> i32 {
    let table = if radius == CHRIS_RADIUS {
        &CHRIS_FOOTFALL
    } else {
        &JILL_FOOTFALL
    };
    let frame = frame as u8;
    let mut speed = WALK_SPEED;

    if frame.wrapping_sub(table[0]) < 7 {
        speed = WALK_SPEED - i32::from(table[2]);
    }
    if frame.wrapping_sub(7) < 7 {
        speed -= i32::from(table[2]);
    }
    if frame.wrapping_sub(table[1]) < 3 {
        speed -= i32::from(table[3]);
    }
    if frame.wrapping_sub(9) < 3 {
        speed -= i32::from(table[3]);
    }
    speed
}

/// The room-action reach probe offset: 600 units along the facing direction.
///
/// The original tests interaction zones against a point this far in front of
/// the player (`update_player_position`), so a door or item triggers while the
/// player is still short of its box.
pub fn reach_offset(angle: u16) -> (i32, i32) {
    rotate_speed(angle, 0, REACH_DISTANCE)
}

/// Rotate `(speed, 0, 0)` by `angle + offset` about Y and return `(dx, dz)`.
///
/// Convention: angle 0 walks along +X; increasing the yaw turns the movement
/// toward -Z. The fixed-point pipeline is the original's: the 14-bit trig
/// values build a 4.12 matrix element, and the matrix-vector product is
/// truncated back down by 12.
pub(crate) fn rotate_speed(angle: u16, offset: u16, speed: i32) -> (i32, i32) {
    let angle = angle.wrapping_add(offset) & 0x0FFF;
    let m00 = cos14(angle) >> 2;
    let m20 = (-sin14(angle)) >> 2;
    (fixed_mul_12(m00, speed), fixed_mul_12(m20, speed))
}

/// Rotate an arbitrary local XZ point by a 12-bit yaw, the transform the SCA
/// collision records run their part offsets through (the original's
/// `SetEntityScaHitData` applies the entity's `RotMatrixY` to each part).
pub(crate) fn rotate_xz(angle: u16, x: i32, z: i32) -> (i32, i32) {
    let angle = angle & 0x0FFF;
    let m00 = cos14(angle) >> 2;
    let m02 = sin14(angle) >> 2;
    let m20 = (-sin14(angle)) >> 2;
    let m22 = cos14(angle) >> 2;
    (
        fixed_mul_12(m00, x) + fixed_mul_12(m02, z),
        fixed_mul_12(m20, x) + fixed_mul_12(m22, z),
    )
}

/// 4.12 matrix element times the speed, truncated back to an integer.
fn fixed_mul_12(element: i32, speed: i32) -> i32 {
    let product = element * speed;
    (product + ((product >> 31) & 0xFFF)) >> 12
}

/// 14-bit amplitude sine of a 12-bit angle.
fn sin14(angle: u16) -> i32 {
    saturate14(((f64::from(angle & 0x0FFF) * ANGLE_STEP).sin() * 16384.0) as i32)
}

/// 14-bit amplitude cosine of a 12-bit angle.
fn cos14(angle: u16) -> i32 {
    saturate14(((f64::from(angle & 0x0FFF) * ANGLE_STEP).cos() * 16384.0) as i32)
}

/// Clamp a table value so +1.0 never reads back as exactly 14-bit signed min.
fn saturate14(value: i32) -> i32 {
    value.clamp(-0x3FFF, 0x3FFF)
}

/// Apply the two-pass room collision resolution to one tick's movement.
///
/// Pass 1 classifies the incoming (already moved) position against the
/// quadrant's records and lets each hit shape push the proposed position.
/// Pass 2 re-classifies the pushed position and rolls the tick back to `prev`
/// if it is still inside a blocking record. While `collision_flags` has bit
/// `0x10` set, shape-5 records are skipped in both passes, the original's
/// per-entity floor-volume skip ([`PlayerState::collision_flags`]).
pub(crate) fn resolve_collision(
    collision: &Collision,
    prev: [i32; 3],
    proposed: [i32; 3],
    radius: i32,
    collision_flags: u8,
) -> [i32; 3] {
    let records = collision.records(proposed[0], proposed[2]);
    let mut pos = proposed;
    let mut hit = 0u16;

    for rect in records {
        if rect.kind & 0xFF == 5 && collision_flags & 0x10 != 0 {
            continue;
        }
        if let Some(shape) = classify(proposed[0], proposed[2], rect, radius) {
            match shape {
                1 | 5 => push_rect(rect, prev, &mut pos, radius),
                3 => push_circle(rect, &mut pos, radius),
                _ => {}
            }
            hit |= (rect.flags & 0x300) >> 8;
        }
    }

    if hit == 0 {
        return pos;
    }

    let mut still_hit = 0u16;
    let mut wedged = false;
    for rect in records {
        let shape = rect.kind & 0xFF;
        if shape == 4 || (shape == 5 && collision_flags & 0x10 != 0) {
            continue;
        }
        if let Some(inside) = classify(pos[0], pos[2], rect, radius) {
            let bits = rect.flags & 0x300;
            still_hit |= bits >> 8;
            if inside == 3 && bits != 0 {
                wedged = true;
            }
        }
    }

    if wedged || still_hit == 0 {
        return pos;
    }
    [prev[0], proposed[1], prev[2]]
}

/// The original's `check_room_collision_two_point` as the room-object pass
/// uses it: push the two body-local floor-probe endpoints of a room object
/// against the room.
///
/// Both endpoints rotate twice: by `yaw` around `pos` for the tested centre,
/// and by `mirror` around `committed` (the last committed 16-bit position) for
/// the value the shape pushes roll the centre back to. The original walks the
/// second endpoint first, pushes accumulate on the entity's live position, and
/// shapes 4 and 5 are skipped. Every record that classifies ORs its
/// `(flags & 0x300) >> 8` bits into the result; the caller treats a nonzero
/// result as blocked.
pub(crate) fn object_two_point_probe(
    collision: &Collision,
    pos: &mut [i32; 3],
    committed: [i32; 3],
    yaw: u16,
    mirror: u16,
    probes: [[u16; 2]; 2],
    radius: i32,
) -> u16 {
    let mut bits = 0u16;
    // The original starts with end B (probe slot 1); a push moves the live
    // position, so the next endpoint's centre is computed from the displacement.
    for which in [1usize, 0] {
        let x = i32::from(probes[which][0]);
        let z = i32::from(probes[which][1]);
        let (test_x, test_z) = rotate_xz(yaw, x, z);
        let point = [pos[0] + test_x, pos[1], pos[2] + test_z];
        let (world_x, world_z) = rotate_xz(mirror, x, z);
        let prev = [committed[0] + world_x, committed[1], committed[2] + world_z];
        // The point every record of this endpoint classifies against is fixed
        // for the whole walk; only the push handlers' scratch `centre` drifts.
        let mut centre = point;

        for rect in collision.records(point[0], point[2]) {
            let shape = rect.kind & 0xFF;
            if shape == 4 || shape == 5 {
                continue;
            }
            if classify(point[0], point[2], rect, radius).is_none() {
                continue;
            }
            match shape {
                1 => {
                    if let Some((dx, dz)) = push_rect_probe(rect, prev, &mut centre, radius) {
                        pos[0] += dx;
                        pos[2] += dz;
                    }
                }
                3 => {
                    let (dx, dz) = circle_push_delta(rect, centre[0], centre[2], radius);
                    pos[0] += dx;
                    pos[2] += dz;
                }
                _ => {}
            }
            bits |= (rect.flags & 0x300) >> 8;
        }
    }
    bits
}

/// Is the circle at `(x, z)` inside the record's grown bounds?
///
/// Returns the record's shape index, or `None` when the point is outside. The
/// unsigned 16-bit coordinates are zero-extended and grown by the radius with
/// signed 32-bit arithmetic; the inclusive comparison matches the original's
/// sign-fold.
fn classify(x: i32, z: i32, rect: &CollisionRect, radius: i32) -> Option<u16> {
    let r = radius & 0xFFFF;
    let x_hi = i32::from(rect.x_max) + r;
    let z_hi = i32::from(rect.z_max) + r;
    let x_lo = i32::from(rect.x_min) - r;
    let z_lo = i32::from(rect.z_min) - r;

    if point_outside(x, z, x_hi, z_hi, x_lo, z_lo) {
        None
    } else {
        Some(rect.kind & 0xFF)
    }
}

/// Signed 32-bit box test: true when the point lies outside the grown bounds.
pub(crate) fn point_outside(x: i32, z: i32, x_hi: i32, z_hi: i32, x_lo: i32, z_lo: i32) -> bool {
    let a = x.wrapping_sub(x_lo);
    let b = x_hi.wrapping_sub(x);
    let c = z.wrapping_sub(z_lo);
    let d = z_hi.wrapping_sub(z);
    (a | b | c | d) < 0
}

/// The rectangle push's four exit depths and selector.
///
/// The four exit depths are computed in 16-bit modular arithmetic with the
/// 18-unit skin. The axis whose push opposes this tick's movement is the face
/// the entity entered through; when both or neither do, or when a single-axis
/// push exceeds 400 units, the shallower correction wins. The selector is bit
/// 0: movement and push disagree in sign on X; bit 1: on Z.
fn rect_push(rect: &CollisionRect, prev: [i32; 3], pos: &[i32; 3], radius: i32) -> (i16, i16, u8) {
    let r = radius as i16;
    let pos_x = pos[0] as i16;
    let pos_z = pos[2] as i16;

    let push_x_hi = r
        .wrapping_sub(pos_x)
        .wrapping_add(rect.x_max as i16)
        .wrapping_add(COLLISION_SKIN);
    let push_x_lo = (rect.x_min as i16)
        .wrapping_sub(r)
        .wrapping_sub(pos_x)
        .wrapping_sub(COLLISION_SKIN);
    let push_x = if -(i32::from(push_x_lo)) < i32::from(push_x_hi) {
        push_x_lo
    } else {
        push_x_hi
    };

    let push_z_hi = r
        .wrapping_add(rect.z_max as i16)
        .wrapping_sub(pos_z)
        .wrapping_add(COLLISION_SKIN);
    let push_z_lo = (rect.z_min as i16)
        .wrapping_sub(r)
        .wrapping_sub(pos_z)
        .wrapping_sub(COLLISION_SKIN);
    let push_z = if -(i32::from(push_z_lo)) < i32::from(push_z_hi) {
        push_z_lo
    } else {
        push_z_hi
    };

    let movement_x = pos[0] - prev[0];
    let movement_z = pos[2] - prev[2];
    let selector = ((((movement_z >> 14) ^ (i32::from(push_z) >> 14)) & 2)
        | (((movement_x >> 15) ^ (i32::from(push_x) >> 15)) & 1)) as u8;

    (push_x, push_z, selector)
}

/// Shapes 1 and 5: push the entity out of a rectangular obstacle.
fn push_rect(rect: &CollisionRect, prev: [i32; 3], pos: &mut [i32; 3], radius: i32) {
    let (push_x, push_z, selector) = rect_push(rect, prev, pos, radius);

    match selector {
        0 => {
            pos[0] = prev[0];
            pos[2] = prev[2];
        }
        1 if push_within_limit(push_x) => pos[0] += i32::from(push_x),
        2 if push_within_limit(push_z) => pos[2] += i32::from(push_z),
        _ => push_shallow(push_x, push_z, pos),
    }
}

/// Shapes 1 and 5 for [`object_two_point_probe`].
///
/// The probe point is the same scratch the handlers receive in the original's
/// two-point path: only selector 0 rolls it back to `prev`, while the accepted
/// corrections are returned for the caller to apply to the entity's live
/// position (the original's handler writes the entity matrix, not the point).
fn push_rect_probe(
    rect: &CollisionRect,
    prev: [i32; 3],
    point: &mut [i32; 3],
    radius: i32,
) -> Option<(i32, i32)> {
    let (push_x, push_z, selector) = rect_push(rect, prev, point, radius);

    match selector {
        0 => {
            point[0] = prev[0];
            point[2] = prev[2];
            None
        }
        1 if push_within_limit(push_x) => Some((i32::from(push_x), 0)),
        2 if push_within_limit(push_z) => Some((0, i32::from(push_z))),
        _ => {
            if i32::from(push_z).abs() > i32::from(push_x).abs() {
                Some((i32::from(push_x), 0))
            } else {
                Some((0, i32::from(push_z)))
            }
        }
    }
}

/// A shape 3 circle's overlap with a point: `(penetration, dx, dz, dist)`.
///
/// The circle's radius is the record's X half-width plus the entity radius and
/// its centre is the box centre. `None` when the point does not overlap, i.e.
/// when the push would be smaller than one unit. `dist` may be zero at the
/// exact centre.
fn circle_overlap(
    rect: &CollisionRect,
    x: i32,
    z: i32,
    radius: i32,
) -> Option<(i32, i32, i32, i32)> {
    let extent = (u32::from(rect.x_max))
        .wrapping_sub(u32::from(rect.x_min))
        .wrapping_add((radius as u32).wrapping_mul(2));
    let reach = (extent as i32) / 2;

    let dz = (z - i32::from(rect.z_min) - reach) + radius;
    let dx = (x - i32::from(rect.x_min) - reach) + radius;
    let dist = integer_sqrt(dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz)));

    let penetration = reach - dist;
    (penetration >= 1).then_some((penetration, dx, dz, dist))
}

/// Shapes 3: the circle push correction for the point `(x, z)`, or `(0, 0)`
/// when it does not overlap. The original's two-point path writes the
/// correction straight to the entity's live position without moving the tested
/// point, so [`object_two_point_probe`] consumes this delta directly.
fn circle_push_delta(rect: &CollisionRect, x: i32, z: i32, radius: i32) -> (i32, i32) {
    let Some((penetration, dx, dz, dist)) = circle_overlap(rect, x, z, radius) else {
        return (0, 0);
    };
    if dist == 0 {
        return (penetration, 0);
    }

    (
        penetration.wrapping_mul(dx) / dist,
        penetration.wrapping_mul(dz) / dist,
    )
}

/// Shapes 3: push the entity out of a circular obstacle whose radius is the
/// record's X half-width plus the entity radius, centred on the box centre.
fn push_circle(rect: &CollisionRect, pos: &mut [i32; 3], radius: i32) {
    let (dx, dz) = circle_push_delta(rect, pos[0], pos[2], radius);
    pos[2] += dz;
    pos[0] += dx;
}

/// The original's `SquareRoot0`: integer square root, zero for non-positive.
fn integer_sqrt(value: i32) -> i32 {
    if value <= 0 {
        0
    } else {
        (f64::from(value)).sqrt() as i32
    }
}

/// Whether a single-axis push correction is within the accepted 400 units.
fn push_within_limit(push: i16) -> bool {
    (push.wrapping_add(PUSH_TEST_BIAS) as u16) <= MAX_PUSH_TEST
}

/// Ambiguous push: move out along whichever axis needs the smaller correction.
fn push_shallow(push_x: i16, push_z: i16, pos: &mut [i32; 3]) {
    if i32::from(push_z).abs() > i32::from(push_x).abs() {
        pos[0] += i32::from(push_x);
    } else {
        pos[2] += i32::from(push_z);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anim::blend_keyframes;
    use crate::model::{ClipFrame, Keyframe};
    use crate::state::{Cut, WalkZone, Zone};

    fn clip(frames: usize) -> Clip {
        Clip {
            frames: vec![
                ClipFrame {
                    keyframe: 0,
                    timing: 1,
                };
                frames
            ],
        }
    }

    fn clips() -> Vec<Clip> {
        vec![clip(3), clip(2), clip(35), clip(28)]
    }

    fn rect(x_max: u16, z_max: u16, x_min: u16, z_min: u16, kind: u16) -> CollisionRect {
        CollisionRect {
            x_max,
            z_max,
            x_min,
            z_min,
            kind,
            flags: 0x300,
        }
    }

    fn room_with_quadrant(rect_record: CollisionRect) -> RoomState {
        let mut room = RoomState::default();
        room.collision.quadrants[0].push(rect_record);
        room
    }

    fn quad(x1: i16, z1: i16, x2: i16, z2: i16) -> [[i16; 2]; 4] {
        [[x1, z1], [x1, z2], [x2, z2], [x2, z1]]
    }

    fn player_at(x: i32, z: i32) -> PlayerState {
        PlayerState {
            pos: [x, 0, z],
            angle: 0,
            radius: CHRIS_RADIUS,
            collision_flags: PLAYER_COLLISION_FLAGS,
            anim: AnimPlayer::new(SETTLE_CLIP),
            clip_source: ClipSource::Emd,
            behavior: BEHAVIOR_IDLE,
            idle_phase: 0,
            idle_ticks: 0,
            stairs: StairState::default(),
            locked: LockedAction::None,
            action_state: 0,
            object_push: false,
            vault_bit: false,
            vault_return: false,
            push_object: None,
            climb_object: None,
            push_heavy: false,
            attack_direction: 0,
            move_speed_current: 0,
            ladder_release: false,
            interact_request: None,
            interact_finished: None,
            input: Input::default(),
            slow_counter: 0,
            run_stop_ticks: 0,
            fresh_entry: false,
            sounds: Vec::new(),
            footsteps: Vec::new(),
            screen_effects: Vec::new(),
        }
    }

    /// Drive one tick with the same clip set standing in for EMD and EMW.
    fn step(player: &mut PlayerState, room: &RoomState, clips: &[Clip], input: Input) {
        update(player, room, clips, clips, input);
    }

    /// Distinct poses per clip source for the blend and stride tests: the body
    /// settle (EMD 0) is keyframe 0, the no-weapon walk (EMW 2) keyframe 1 and
    /// the no-weapon run (EMW 3) keyframe 2.
    fn blend_fixture() -> (Vec<Clip>, Vec<Clip>, Vec<Keyframe>) {
        fn looping(keyframe: u16, frames: usize) -> Clip {
            Clip {
                frames: vec![
                    ClipFrame {
                        keyframe,
                        timing: 1,
                    };
                    frames
                ],
            }
        }
        let keyframes = vec![
            Keyframe {
                offset: [0, 0, 0],
                rotations: vec![[0, 0, 0]],
            },
            Keyframe {
                offset: [0, 200, 0],
                rotations: vec![[0x200, 0, 0]],
            },
            Keyframe {
                offset: [0, 400, 0],
                rotations: vec![[0x400, 0, 0]],
            },
        ];
        let emd = vec![looping(0, 3)];
        let emw = vec![looping(1, 2), looping(1, 2), looping(1, 35), looping(2, 28)];
        (emd, emw, keyframes)
    }

    /// One tick against the split EMD/EMW fixture.
    fn blend_step(
        player: &mut PlayerState,
        room: &RoomState,
        emd: &[Clip],
        emw: &[Clip],
        input: Input,
    ) {
        update_with_room(player, room, emd, emw, &[], input);
    }

    /// The pose the player currently shows, read against the fixture's clips.
    fn shown_pose(
        player: &PlayerState,
        emd: &[Clip],
        emw: &[Clip],
        keyframes: &[Keyframe],
    ) -> Keyframe {
        let clips = match player.clip_source {
            ClipSource::Emd => emd,
            ClipSource::Emw => emw,
            ClipSource::Room => &[],
        };
        player.anim.pose_keyframe(clips, keyframes).unwrap()
    }

    #[test]
    fn walk_release_holds_the_walk_pose_then_eases_into_settle() {
        let room = RoomState::default();
        let (emd, emw, keyframes) = blend_fixture();
        let mut player = player_at(1000, 0);
        let walk = Input {
            up: true,
            ..Input::default()
        };
        for _ in 0..5 {
            blend_step(&mut player, &room, &emd, &emw, walk);
        }
        let walk_pose = shown_pose(&player, &emd, &emw, &keyframes);
        assert_eq!(walk_pose, keyframes[1]);
        let speed = i32::from(player.move_speed_current);
        assert!(speed > 0, "the walk set move_speed_current");

        // Release tick: no frame is consumed, so the walk pose stays on screen
        // and the player still moves at the last walk speed.
        let before = player.pos;
        blend_step(&mut player, &room, &emd, &emw, Input::default());
        assert_eq!(player.behavior, BEHAVIOR_IDLE);
        assert_eq!(player.clip_source, ClipSource::Emd);
        assert_eq!(
            shown_pose(&player, &emd, &emw, &keyframes),
            walk_pose,
            "release tick keeps the walk pose"
        );
        let (dx, dz) = rotate_speed(player.angle, 0, speed);
        assert_eq!(player.pos, [before[0] + dx, before[1], before[2] + dz]);

        // The four following ticks ease the walk pose into the settle keyframe
        // with the fresh three-step blend: 3/4, 1/2, 1/4, then direct.
        let settle = keyframes[0].clone();
        let mut expected = walk_pose.clone();
        for counter in (1..=3).rev() {
            blend_step(&mut player, &room, &emd, &emw, Input::default());
            assert_eq!(player.anim.blend_used, counter);
            expected = blend_keyframes(&expected, &settle, counter, 0x400);
            assert_eq!(shown_pose(&player, &emd, &emw, &keyframes), expected);
        }
        blend_step(&mut player, &room, &emd, &emw, Input::default());
        assert_eq!(player.anim.blend_used, 0);
        assert_eq!(shown_pose(&player, &emd, &emw, &keyframes), settle);
    }

    #[test]
    fn run_release_decelerates_and_rearms_the_idle_blend() {
        let room = RoomState::default();
        let (emd, emw, keyframes) = blend_fixture();
        let mut player = player_at(1000, 0);
        let run = Input {
            up: true,
            run: true,
            ..Input::default()
        };

        // Enter the run and read its pose.
        blend_step(&mut player, &room, &emd, &emw, run);
        assert_eq!(player.behavior, BEHAVIOR_RUN);
        assert_eq!(player.move_speed_current, RUN_SPEED as u16);
        let run_pose = shown_pose(&player, &emd, &emw, &keyframes);
        assert_eq!(run_pose, keyframes[2]);
        blend_step(&mut player, &room, &emd, &emw, run);

        // Release: four decelerating ticks on body clip 0, moving at
        // 180/150/120/90.
        let settle = keyframes[0].clone();
        let mut expected = run_pose;
        for (tick, (speed, counter)) in [(180u16, 3u16), (150, 2), (120, 1), (90, 0)]
            .into_iter()
            .enumerate()
        {
            let before = player.pos;
            blend_step(&mut player, &room, &emd, &emw, Input::default());
            assert_eq!(player.move_speed_current, speed, "stop tick {tick}");
            assert_eq!(player.anim.blend_used, counter, "stop tick {tick}");
            assert_eq!(player.clip_source, ClipSource::Emd);
            assert_eq!(player.anim.clip, SETTLE_CLIP);
            let (dx, dz) = rotate_speed(player.angle, 0, i32::from(speed));
            assert_eq!(
                player.pos,
                [before[0] + dx, before[1], before[2] + dz],
                "stop tick {tick} step"
            );
            expected = if counter == 0 {
                settle.clone()
            } else {
                blend_keyframes(&expected, &settle, counter, 0x400)
            };
            assert_eq!(shown_pose(&player, &emd, &emw, &keyframes), expected);
        }
        assert_eq!(player.behavior, BEHAVIOR_IDLE);
        assert!(player.fresh_entry);

        // Idle re-arms a fresh three-step blend into the settle pose.
        blend_step(&mut player, &room, &emd, &emw, Input::default());
        assert_eq!(player.anim.blend_used, 3);
        assert_eq!(player.anim.blend_counter, 2);
        assert_eq!(shown_pose(&player, &emd, &emw, &keyframes), settle);
        blend_step(&mut player, &room, &emd, &emw, Input::default());
        assert_eq!(player.anim.blend_used, 2);
    }

    #[test]
    fn walk_run_flips_keep_the_stride() {
        let room = RoomState::default();
        let (emd, emw, keyframes) = blend_fixture();
        let walk = Input {
            up: true,
            ..Input::default()
        };
        let run = Input {
            up: true,
            run: true,
            ..Input::default()
        };

        // Walk headed into the stride's middle enters the run at 0x0C.
        let mut player = player_at(1000, 0);
        for _ in 0..10 {
            blend_step(&mut player, &room, &emd, &emw, walk);
        }
        assert_eq!(player.anim.frame, 10);
        let walk_pose = shown_pose(&player, &emd, &emw, &keyframes);
        blend_step(&mut player, &room, &emd, &emw, run);
        assert_eq!(player.behavior, BEHAVIOR_RUN);
        assert_eq!(player.anim.display_frame, RUN_ENTRY_FRAME_STRIDE);
        assert_eq!(player.anim.blend_used, 3);
        assert_eq!(
            shown_pose(&player, &emd, &emw, &keyframes),
            blend_keyframes(&walk_pose, &keyframes[2], 3, 0x400)
        );

        // A walk early in its cycle enters the run on frame 1.
        let mut player = player_at(1000, 0);
        for _ in 0..2 {
            blend_step(&mut player, &room, &emd, &emw, walk);
        }
        blend_step(&mut player, &room, &emd, &emw, run);
        assert_eq!(player.anim.display_frame, RUN_ENTRY_FRAME);
        assert_eq!(player.anim.blend_used, 3);

        // A run early in its stride re-enters the walk at 0x19.
        let mut player = player_at(1000, 0);
        blend_step(&mut player, &room, &emd, &emw, run);
        assert_eq!(player.anim.frame, 2);
        let run_pose = shown_pose(&player, &emd, &emw, &keyframes);
        blend_step(&mut player, &room, &emd, &emw, walk);
        assert_eq!(player.behavior, BEHAVIOR_WALK);
        assert_eq!(player.anim.display_frame, WALK_REENTRY_FRAME_LATE);
        assert_eq!(player.anim.blend_used, 3);
        assert_eq!(
            shown_pose(&player, &emd, &emw, &keyframes),
            blend_keyframes(&run_pose, &keyframes[1], 3, 0x400)
        );

        // A run late in its stride re-enters the walk at frame 10.
        let mut player = player_at(1000, 0);
        for _ in 0..10 {
            blend_step(&mut player, &room, &emd, &emw, run);
        }
        assert_eq!(player.anim.frame, 11);
        blend_step(&mut player, &room, &emd, &emw, walk);
        assert_eq!(player.anim.display_frame, WALK_REENTRY_FRAME);
        assert_eq!(player.anim.blend_used, 3);
    }

    #[test]
    fn turn_release_holds_the_pose_then_eases_into_settle() {
        let room = RoomState::default();
        let (emd, emw, keyframes) = blend_fixture();
        let mut player = player_at(1000, 0);
        blend_step(
            &mut player,
            &room,
            &emd,
            &emw,
            Input {
                left: true,
                ..Input::default()
            },
        );
        let turn_pose = shown_pose(&player, &emd, &emw, &keyframes);
        assert_eq!(player.behavior, BEHAVIOR_TURN);

        // Release holds the turn pose for one tick, then the idle entry eases
        // it into the settle pose with a fresh three-step blend.
        blend_step(&mut player, &room, &emd, &emw, Input::default());
        assert_eq!(player.behavior, BEHAVIOR_IDLE);
        assert_eq!(shown_pose(&player, &emd, &emw, &keyframes), turn_pose);
        blend_step(&mut player, &room, &emd, &emw, Input::default());
        assert_eq!(player.anim.blend_used, 3);
        assert_eq!(
            shown_pose(&player, &emd, &emw, &keyframes),
            blend_keyframes(&turn_pose, &keyframes[0], 3, 0x400)
        );
    }

    #[test]
    fn walk_speed_matches_the_chris_footfall_table() {
        let expected = [
            93, 93, 93, 93, 93, 93, 93, 80, 80, 66, 66, 66, 80, 80, 93, 93, 93, 93, 93, 93, 93, 80,
            80, 66, 66, 66, 80, 80, 93, 93, 93, 93, 93, 93, 93,
        ];
        for (frame, speed) in expected.iter().enumerate() {
            assert_eq!(walk_speed(CHRIS_RADIUS, frame), *speed, "frame {frame}");
        }
    }

    #[test]
    fn walk_speed_matches_the_jill_footfall_table() {
        let expected = [
            93, 93, 93, 93, 93, 93, 93, 78, 78, 63, 63, 63, 78, 78, 93, 93, 93, 93, 93, 93, 78, 78,
            63, 63, 63, 78, 78, 93, 93, 93, 93, 93, 93, 93, 93,
        ];
        for (frame, speed) in expected.iter().enumerate() {
            assert_eq!(walk_speed(JILL_RADIUS, frame), *speed, "frame {frame}");
        }
    }

    #[test]
    fn walking_forward_moves_along_plus_x() {
        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(1000, 1000);
        let input = Input {
            up: true,
            ..Input::default()
        };

        for _ in 0..10 {
            step(&mut player, &room, &clips, input);
        }

        assert!(player.pos[0] > 1000, "walked {}", player.pos[0]);
        assert_eq!(player.pos[2], 1000);
        assert_eq!(player.pos[1], 0);
        assert_eq!(player.behavior, BEHAVIOR_WALK);
        assert_eq!(player.anim.clip, WALK_CLIP);
    }

    #[test]
    fn walking_at_quarter_turn_moves_along_minus_z() {
        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(1000, 1000);
        player.angle = 0x400;

        step(
            &mut player,
            &room,
            &clips,
            Input {
                up: true,
                ..Input::default()
            },
        );

        assert_eq!(player.pos[0], 1000);
        assert!(player.pos[2] < 1000, "walked z {}", player.pos[2]);
    }

    #[test]
    fn slow_motion_halves_walk_speed_and_cadence() {
        let room = RoomState::default();
        let clips = clips();
        let mut fast = player_at(1000, 1000);
        let mut slow = player_at(1000, 1000);
        let fast_input = Input {
            up: true,
            ..Input::default()
        };
        let slow_input = Input {
            up: true,
            slow_motion: true,
            ..Input::default()
        };

        for _ in 0..10 {
            step(&mut fast, &room, &clips, fast_input);
            step(&mut slow, &room, &clips, slow_input);
        }

        // The animation advances on every tick at full speed and every other
        // tick under slow motion (`slow_counter` 0 applied / 1 skipped).
        assert_eq!(fast.anim.frame, 10, "full-speed cadence");
        assert_eq!(slow.anim.frame, 5, "slow-motion cadence");

        // The speeds halve per tick, so the slow walk covers roughly half the
        // ground (the frame-driven footfall modulation makes it approximate).
        let fast_dx = fast.pos[0] - 1000;
        let slow_dx = slow.pos[0] - 1000;
        assert!(slow_dx > 0, "slow motion still walks");
        assert!(slow_dx * 3 < fast_dx * 2, "slow < 2/3 of full speed");
        assert!(slow_dx * 4 > fast_dx, "slow > 1/4 of full speed");

        // Dropping the flag returns to the full cadence and speed.
        step(
            &mut slow,
            &room,
            &clips,
            Input {
                up: true,
                ..Input::default()
            },
        );
        assert_eq!(slow.anim.frame, 6);
    }

    #[test]
    fn slow_motion_does_not_touch_turning_or_backing() {
        let room = RoomState::default();
        let clips = clips();
        let mut fast = player_at(1000, 1000);
        fast.angle = 0;
        let mut slow = fast.clone();
        let fast_input = Input {
            down: true,
            ..Input::default()
        };
        let slow_input = Input {
            down: true,
            slow_motion: true,
            ..Input::default()
        };
        for _ in 0..6 {
            step(&mut fast, &room, &clips, fast_input);
            step(&mut slow, &room, &clips, slow_input);
        }
        assert_eq!(fast.pos[0], slow.pos[0], "backing is not slowed");
        assert_eq!(fast.anim.frame, slow.anim.frame);
    }

    #[test]
    fn walking_into_a_wall_is_blocked_without_passing_through() {
        // Wall face at x = 1180; the 422 radius plus the 18-unit skin stops the
        // player centre at 740.
        let room = room_with_quadrant(rect(2000, 2000, 1180, 0, 1));
        let clips = clips();
        let mut player = player_at(500, 1000);

        for _ in 0..10 {
            step(
                &mut player,
                &room,
                &clips,
                Input {
                    up: true,
                    ..Input::default()
                },
            );
        }

        assert_eq!(player.pos, [740, 0, 1000]);
        assert!(player.pos[0] < 1180 - CHRIS_RADIUS);
    }

    #[test]
    fn a_record_containing_the_player_reverts_the_tick() {
        let room = room_with_quadrant(rect(10000, 10000, 0, 0, 1));
        let clips = clips();
        let mut player = player_at(5000, 5000);

        for _ in 0..10 {
            step(
                &mut player,
                &room,
                &clips,
                Input {
                    up: true,
                    ..Input::default()
                },
            );
        }

        assert_eq!(player.pos, [5000, 0, 5000]);
    }

    #[test]
    fn walking_diagonally_into_a_wall_slides_along_it() {
        // The wall blocks +x only, so the diagonal walk keeps its -z movement
        // while being pushed back to the wall's skin line.
        let room = room_with_quadrant(rect(4000, 2000, 600, 0, 1));
        let clips = clips();
        let mut player = player_at(100, 1000);
        player.angle = 0x200;

        for _ in 0..10 {
            step(
                &mut player,
                &room,
                &clips,
                Input {
                    up: true,
                    ..Input::default()
                },
            );
        }

        assert_eq!(player.pos[0], 160);
        assert!(player.pos[2] < 1000, "slid z {}", player.pos[2]);
    }

    #[test]
    fn camera_picks_the_first_matching_zone() {
        let room = RoomState {
            cuts: vec![Cut::default(); 3],
            zones: vec![
                Zone {
                    cam_to: 9,
                    cam_from: 0,
                    corners: quad(0, 0, 200, 200),
                },
                Zone {
                    cam_to: 2,
                    cam_from: 0,
                    corners: quad(10, 10, 190, 190),
                },
                Zone {
                    cam_to: 1,
                    cam_from: 0,
                    corners: quad(20, 20, 180, 180),
                },
            ],
            ..RoomState::default()
        };

        assert_eq!(camera_for_position(&room, 0, [50, 0, 50]), 2);
    }

    #[test]
    fn camera_keeps_the_current_cut_when_cam_to_is_invalid() {
        let room = RoomState {
            cuts: vec![Cut::default(); 3],
            zones: vec![
                Zone {
                    cam_to: 9,
                    cam_from: 0,
                    corners: quad(0, 0, 200, 200),
                },
                Zone {
                    cam_to: 9,
                    cam_from: 0,
                    corners: quad(10, 10, 190, 190),
                },
            ],
            ..RoomState::default()
        };

        assert_eq!(camera_for_position(&room, 0, [50, 0, 50]), 0);
    }

    #[test]
    fn camera_skips_group_headers() {
        let room = RoomState {
            cuts: vec![Cut::default(); 3],
            zones: vec![Zone {
                cam_to: 2,
                cam_from: 0,
                corners: quad(0, 0, 200, 200),
            }],
            ..RoomState::default()
        };

        assert_eq!(camera_for_position(&room, 0, [50, 0, 50]), 0);
    }

    #[test]
    fn camera_scans_only_the_current_group() {
        let room = RoomState {
            cuts: vec![Cut::default(); 3],
            zones: vec![
                Zone {
                    cam_to: 9,
                    cam_from: 0,
                    corners: quad(0, 0, 200, 200),
                },
                Zone {
                    cam_to: 9,
                    cam_from: 1,
                    corners: quad(0, 0, 200, 200),
                },
                Zone {
                    cam_to: 2,
                    cam_from: 1,
                    corners: quad(10, 10, 190, 190),
                },
                Zone {
                    cam_to: 1,
                    cam_from: 1,
                    corners: quad(20, 20, 180, 180),
                },
            ],
            ..RoomState::default()
        };

        assert_eq!(camera_for_position(&room, 1, [50, 0, 50]), 2);
        // No group for cut 2 at all: the current cut is kept.
        assert_eq!(camera_for_position(&room, 2, [50, 0, 50]), 2);
    }

    #[test]
    fn position_blocked_sees_shape_rects_and_ignores_soft_zones() {
        let mut room = RoomState::default();
        room.collision.quadrants[0].push(rect(2000, 2000, 1180, 0, 1));
        room.collision.quadrants[0].push(CollisionRect {
            kind: 4,
            ..rect(10000, 10000, 0, 0, 4)
        });

        assert!(position_blocked(
            &room,
            [1500, 0, 1000],
            CHRIS_RADIUS,
            PLAYER_COLLISION_FLAGS
        ));
        // The soft zone covers the point but never pushes.
        assert!(!position_blocked(
            &room,
            [5000, 0, 5000],
            CHRIS_RADIUS,
            PLAYER_COLLISION_FLAGS
        ));
        // Outside the grown rectangle.
        assert!(!position_blocked(
            &room,
            [500, 0, 1000],
            CHRIS_RADIUS,
            PLAYER_COLLISION_FLAGS
        ));
    }

    #[test]
    fn position_blocked_ignores_a_circle_box_corner() {
        let room = room_with_quadrant(rect(2000, 2000, 1180, 0, 3));

        // Inside the grown box but far outside the circle: no push.
        assert!(!position_blocked(
            &room,
            [800, 0, 2000],
            CHRIS_RADIUS,
            PLAYER_COLLISION_FLAGS
        ));
        // The circle centre: pushed.
        assert!(position_blocked(
            &room,
            [1590, 0, 410],
            CHRIS_RADIUS,
            PLAYER_COLLISION_FLAGS
        ));
    }

    #[test]
    fn player_resolution_skips_shape_five_in_both_passes() {
        let mut room = RoomState::default();
        // A shape-1 wall whose +x push lands inside a shape-5 floor volume.
        room.collision.quadrants[0].push(rect(2000, 3000, 1000, 0, 1));
        room.collision.quadrants[0].push(rect(4000, 3000, 2000, 0, 5));

        // position_blocked: the floor volume blocks only without the player bit.
        assert!(position_blocked(&room, [2440, 0, 1000], CHRIS_RADIUS, 0));
        assert!(!position_blocked(
            &room,
            [2440, 0, 1000],
            CHRIS_RADIUS,
            PLAYER_COLLISION_FLAGS
        ));

        // Pass 1: a proposed point inside the shape-5 volume is left alone.
        assert_eq!(
            resolve_collision(
                &room.collision,
                [4000, 0, 1000],
                [2440, 0, 1000],
                CHRIS_RADIUS,
                PLAYER_COLLISION_FLAGS
            ),
            [2440, 0, 1000]
        );

        // Pass 2: the shape-1 push lands inside the shape-5 volume; the player
        // bit accepts the pushed point, clearing it rolls the tick back.
        let prev = [1800, 0, 1000];
        let proposed = [1900, 0, 1000];
        assert_eq!(
            resolve_collision(
                &room.collision,
                prev,
                proposed,
                CHRIS_RADIUS,
                PLAYER_COLLISION_FLAGS
            ),
            [2440, 0, 1000]
        );
        assert_eq!(
            resolve_collision(&room.collision, prev, proposed, CHRIS_RADIUS, 0),
            prev
        );
    }

    #[test]
    fn stair_height_is_applied_after_movement() {
        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(0, 0);
        player.stairs.height = Some(777);
        step(
            &mut player,
            &room,
            &clips,
            Input {
                up: true,
                ..Input::default()
            },
        );
        assert_eq!(player.pos[1], 777, "the ramp owns Y");
        assert!(player.pos[0] > 0, "the walk still advances on the XZ plane");
    }

    #[test]
    fn climb_suspends_collision_and_locks_the_facing() {
        // A solid rectangle whose grown bounds start at x = 1578.
        let room = room_with_quadrant(rect(4000, 2000, 2000, 0, 1));
        let clips = clips();
        let mut player = player_at(1500, 1000);
        assert!(!position_blocked(
            &room,
            player.pos,
            player.radius,
            player.collision_flags,
        ));

        player.stairs.climbing = true;
        player.stairs.locked_angle = 0;
        for _ in 0..10 {
            step(
                &mut player,
                &room,
                &clips,
                Input {
                    up: true,
                    left: true,
                    ..Input::default()
                },
            );
        }
        assert!(
            player.pos[0] > 2000,
            "collision was not suspended: {:?}",
            player.pos
        );
        assert!(
            position_blocked(&room, player.pos, player.radius, player.collision_flags),
            "the climb should have carried the player into the wall volume"
        );
        assert_eq!(player.angle, 0, "the climb holds the locked facing");
    }

    #[test]
    fn releasing_the_climb_restores_collision() {
        let room = room_with_quadrant(rect(4000, 2000, 2000, 0, 1));
        let clips = clips();
        let mut player = player_at(1500, 1000);
        player.stairs.climbing = true;
        player.stairs.locked_angle = 0;
        for _ in 0..8 {
            step(
                &mut player,
                &room,
                &clips,
                Input {
                    up: true,
                    ..Input::default()
                },
            );
        }
        let inside = player.pos;
        assert!(position_blocked(
            &room,
            inside,
            player.radius,
            player.collision_flags,
        ));

        player.stairs.climbing = false;
        for _ in 0..5 {
            step(
                &mut player,
                &room,
                &clips,
                Input {
                    up: true,
                    ..Input::default()
                },
            );
        }
        assert!(
            player.pos[0] <= inside[0],
            "collision should stop the player once the climb releases: {:?}",
            player.pos
        );
        assert!(!position_blocked(
            &room,
            player.pos,
            player.radius,
            player.collision_flags,
        ));
    }

    #[test]
    fn input_selects_behaviors_and_clips() {
        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(0, 0);
        assert_eq!(player.behavior, BEHAVIOR_IDLE);
        assert_eq!(player.anim.clip, SETTLE_CLIP);

        step(
            &mut player,
            &room,
            &clips,
            Input {
                up: true,
                ..Input::default()
            },
        );
        assert_eq!(player.behavior, BEHAVIOR_WALK);
        assert_eq!(player.anim.clip, WALK_CLIP);

        step(
            &mut player,
            &room,
            &clips,
            Input {
                up: true,
                run: true,
                ..Input::default()
            },
        );
        assert_eq!(player.behavior, BEHAVIOR_RUN);
        assert_eq!(player.anim.clip, RUN_CLIP);
        assert_eq!(player.clip_source, ClipSource::Emw);

        // Forward release begins the run's four-tick stop; it plays out before
        // the next direction is selected.
        for _ in 0..RUN_STOP_TICKS {
            step(&mut player, &room, &clips, Input::default());
        }
        assert_eq!(player.behavior, BEHAVIOR_IDLE);
        step(
            &mut player,
            &room,
            &clips,
            Input {
                down: true,
                ..Input::default()
            },
        );
        assert_eq!(player.behavior, BEHAVIOR_BACK);
        assert_eq!(player.anim.clip, BACK_CLIP);
        assert_eq!(player.clip_source, ClipSource::Emd);

        step(
            &mut player,
            &room,
            &clips,
            Input {
                left: true,
                ..Input::default()
            },
        );
        assert_eq!(player.behavior, BEHAVIOR_TURN);
        assert_eq!(player.anim.clip, WALK_CLIP);

        step(&mut player, &room, &clips, Input::default());
        assert_eq!(player.behavior, BEHAVIOR_IDLE);
        assert_eq!(player.anim.clip, SETTLE_CLIP);
    }

    #[test]
    fn idle_settle_holds_then_breathes() {
        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(0, 0);

        assert_eq!(player.anim.clip, SETTLE_CLIP);
        for _ in 0..IDLE_SETTLE_TICKS {
            step(&mut player, &room, &clips, Input::default());
        }
        assert_eq!(player.idle_phase, 1);
        assert_eq!(player.clip_source, ClipSource::Emw);
        assert_eq!(player.anim.clip, BREATHE_IN_CLIP);

        let mut wrapped = false;
        for _ in 0..10 {
            step(&mut player, &room, &clips, Input::default());
            if player.anim.clip == BREATHE_CLIP {
                wrapped = true;
                break;
            }
        }
        assert!(wrapped, "breathe transition never wrapped into the loop");

        // Once breathing, later idle ticks stay on the breathe loop.
        for _ in 0..3 {
            step(&mut player, &room, &clips, Input::default());
        }
        assert_eq!(player.anim.clip, BREATHE_CLIP);
        assert_eq!(player.behavior, BEHAVIOR_IDLE);
    }

    #[test]
    fn turning_uses_the_expected_steps_and_signs() {
        let room = RoomState::default();
        let clips = clips();

        let mut player = player_at(0, 0);
        step(
            &mut player,
            &room,
            &clips,
            Input {
                left: true,
                ..Input::default()
            },
        );
        assert_eq!(player.angle, 0x0FA0);

        player.angle = 0;
        step(
            &mut player,
            &room,
            &clips,
            Input {
                right: true,
                ..Input::default()
            },
        );
        assert_eq!(player.angle, 0x0060);

        player.angle = 0;
        step(
            &mut player,
            &room,
            &clips,
            Input {
                up: true,
                right: true,
                ..Input::default()
            },
        );
        assert_eq!(player.angle, 0x0028);

        // Release the walk first: the original holds the walk pose for a tick
        // before the next direction is selected.
        step(&mut player, &room, &clips, Input::default());
        player.angle = 0;
        step(
            &mut player,
            &room,
            &clips,
            Input {
                down: true,
                left: true,
                ..Input::default()
            },
        );
        assert_eq!(player.angle, 0x0FD8);

        player.angle = 0;
        step(
            &mut player,
            &room,
            &clips,
            Input {
                up: true,
                run: true,
                left: true,
                ..Input::default()
            },
        );
        assert_eq!(player.angle, 0x0FD0);
    }

    #[test]
    fn walking_with_a_direction_keeps_the_same_behavior_and_clip() {
        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(0, 0);

        for _ in 0..2 {
            step(
                &mut player,
                &room,
                &clips,
                Input {
                    up: true,
                    ..Input::default()
                },
            );
        }
        let frame = player.anim.display_frame;

        step(
            &mut player,
            &room,
            &clips,
            Input {
                up: true,
                right: true,
                ..Input::default()
            },
        );

        assert_eq!(player.behavior, BEHAVIOR_WALK);
        assert_eq!(player.anim.clip, WALK_CLIP);
        assert_eq!(player.anim.display_frame, frame + 1);
    }

    #[test]
    fn spawn_uses_the_first_walk_zone_center() {
        let mut room = RoomState::default();
        room.walk_zones.push(WalkZone {
            x1: 100,
            z1: 200,
            x2: 300,
            z2: 400,
            field_08: 0,
            flags: 0,
        });

        let chris = spawn(
            RoomId {
                stage: 1,
                room: 0,
                player_flag: 0,
            },
            &room,
        );
        assert_eq!(chris.pos, [200, 0, 300]);
        assert_eq!(chris.radius, CHRIS_RADIUS);
        assert_eq!(chris.angle, 0);
        assert_eq!(chris.behavior, BEHAVIOR_IDLE);
        assert_eq!(chris.anim.clip, SETTLE_CLIP);

        let jill = spawn(
            RoomId {
                stage: 1,
                room: 0,
                player_flag: 1,
            },
            &RoomState::default(),
        );
        assert_eq!(jill.pos, [0, 0, 0]);
        assert_eq!(jill.radius, JILL_RADIUS);
    }

    fn step_events(
        player: &mut PlayerState,
        room: &RoomState,
        clips: &[Clip],
        input: Input,
        ticks: usize,
    ) -> Vec<Footstep> {
        let mut events = Vec::new();
        for _ in 0..ticks {
            step(player, room, clips, input);
            events.extend(player.take_footsteps());
        }
        events
    }

    #[test]
    fn walking_emits_footsteps_on_contact_frames() {
        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(1000, 1000);
        let input = Input {
            up: true,
            ..Input::default()
        };

        let events = step_events(&mut player, &room, &clips, input, 30);

        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].frame, 0x08);
        assert_eq!(events[1].frame, 0x16);
        assert!(events.iter().all(|event| event.sound_type == 0));
        assert!(events.iter().all(|event| event.pos[1] == 0));
    }

    #[test]
    fn turning_and_running_emit_footsteps() {
        let room = RoomState::default();
        let clips = clips();

        let mut turning = player_at(1000, 1000);
        let events = step_events(
            &mut turning,
            &room,
            &clips,
            Input {
                left: true,
                ..Input::default()
            },
            30,
        );
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].frame, 0x08);
        assert_eq!(events[1].frame, 0x16);

        let mut running = player_at(1000, 1000);
        let events = step_events(
            &mut running,
            &room,
            &clips,
            Input {
                up: true,
                run: true,
                ..Input::default()
            },
            30,
        );
        // The run starts on frame 1 (the original's run state 0 skips frame
        // 0), so the fixture's 28-frame clip contacts on 0x0A and, after the
        // wrap, on frame 0 inside the 30 ticks.
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].frame, 0x0A);
        assert_eq!(events[1].frame, 0x00);
        assert!(events.iter().all(|event| event.sound_type == 1));
    }

    #[test]
    fn running_footsteps_restart_the_bank_voice() {
        use crate::audio::MixState;

        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(1000, 1000);
        let input = Input {
            up: true,
            run: true,
            ..Input::default()
        };

        let mut mixer = MixState::new();
        // Longer than the 10-tick gap between the run's contact frames, so a
        // buffer-per-cue mixer would overlap the second copy. The original's
        // one buffer per entity sound bank restarts that voice instead.
        let step_pcm = vec![1000i16; 2000];
        let mut out = Vec::new();
        let mut triggers = Vec::new();
        let mut sounded = 0usize;

        for tick in 0..60 {
            step(&mut player, &room, &clips, input);
            let events = player.take_footsteps();
            if !events.is_empty() {
                triggers.push(tick);
            }
            for _ in &events {
                // The run's bank-2 column key; both contacts resolve the same
                // column, so the second restarts the first.
                mixer.play_sfx_on_bank(0x0201, step_pcm.clone(), 1.0, 0.0);
            }

            let active = mixer.active_sfx();
            assert!(active <= 1, "tick {tick}: {active} footstep voices");
            mixer.render(1, &mut out);
            if active == 1 {
                sounded += 1;
                // Center-panned: full sample in both channels.
                let left = i16::from_le_bytes([out[out.len() - 4], out[out.len() - 3]]);
                let right = i16::from_le_bytes([out[out.len() - 2], out[out.len() - 1]]);
                assert_eq!((left, right), (1000, 1000), "tick {tick}");
            }
        }

        assert!(
            triggers.len() >= 2,
            "the run triggered {} footfalls in 60 ticks",
            triggers.len()
        );
        assert!(sounded >= 2, "the footstep bank never sounded");
    }

    #[test]
    fn backward_run_emits_footsteps_from_the_body_clip() {
        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(1000, 1000);
        let input = Input {
            down: true,
            ..Input::default()
        };

        let events = step_events(&mut player, &room, &clips, input, 30);

        // The body EMD's run clip carries the walk contact frames and the A
        // footstep.
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].frame, 0x08);
        assert_eq!(events[1].frame, 0x16);
        assert!(events.iter().all(|event| event.sound_type == 0));
        assert_eq!(player.clip_source, ClipSource::Emd);
    }

    #[test]
    fn a_contact_frame_held_by_its_timing_fires_once() {
        let room = RoomState::default();
        let mut clips = clips();
        clips[WALK_CLIP].frames[0x08].timing = 2;
        let mut player = player_at(1000, 1000);
        let input = Input {
            up: true,
            ..Input::default()
        };

        // Advance to the tick that first applies contact frame 0x08.
        let mut fired = false;
        for _ in 0..30 {
            step(&mut player, &room, &clips, input);
            let taken = player.take_footsteps();
            if !taken.is_empty() {
                assert_eq!(taken.len(), 1, "{taken:?}");
                assert_eq!(taken[0].frame, 0x08);
                fired = true;
                break;
            }
        }
        assert!(fired, "the 0x08 contact frame was never applied");

        // The contact is still displayed while its timing runs down, but the
        // pending frame has already moved past it: no re-fire.
        step(&mut player, &room, &clips, input);
        assert_eq!(player.anim.display_frame, 0x08);
        assert!(
            player.take_footsteps().is_empty(),
            "a held contact frame re-fired"
        );
    }

    #[test]
    fn a_contact_reached_while_the_previous_frame_is_held_fires_each_held_tick() {
        // The original tests the pending frame before Joint_move decides
        // whether it can apply it, so a contact reached with the preceding
        // frame still held fires on every held tick until it is applied.
        let room = RoomState::default();
        let mut clips = clips();
        clips[WALK_CLIP].frames[0x07].timing = 2;
        let mut player = player_at(1000, 1000);

        let log = footfall_log(
            &mut player,
            &room,
            &clips,
            &clips,
            Input {
                up: true,
                ..Input::default()
            },
            30,
        );

        let fires: Vec<(usize, u8, u8)> = log
            .iter()
            .copied()
            .filter(|entry| entry.1 == 0x08)
            .collect();
        assert_eq!(fires, [(8, 0x08, 0), (9, 0x08, 0)], "not exactly two fires");
    }

    /// Clip sets shaped like the shipped models: the body EMD's run clip 3 has
    /// 28 frames and the no-weapon EMW carries the 28-frame walk cycle (clip 2)
    /// and the 20-frame forward run (clip 3). Every frame lasts one tick.
    fn shipped_clips() -> (Vec<Clip>, Vec<Clip>) {
        (
            vec![clip(3), clip(20), clip(35), clip(28), clip(30)],
            vec![clip(35), clip(16), clip(28), clip(20), clip(25)],
        )
    }

    /// Drive `ticks` ticks and collect `(tick, contact frame, sound type)` for
    /// every footfall.
    fn footfall_log(
        player: &mut PlayerState,
        room: &RoomState,
        emd_clips: &[Clip],
        emw_clips: &[Clip],
        input: Input,
        ticks: usize,
    ) -> Vec<(usize, u8, u8)> {
        let mut log = Vec::new();
        for tick in 0..ticks {
            update(player, room, emd_clips, emw_clips, input);
            for event in player.take_footsteps() {
                log.push((tick, event.frame, event.sound_type));
            }
        }
        log
    }

    /// A room clip set with the given clips placed at their indices.
    fn room_clips(entries: &[(usize, usize)]) -> Vec<Clip> {
        let count = entries
            .iter()
            .map(|&(index, _)| index + 1)
            .max()
            .unwrap_or(0);
        let mut clips = vec![clip(0); count];
        for &(index, frames) in entries {
            clips[index] = clip(frames);
        }
        clips
    }

    #[test]
    fn push_winds_up_grunts_moves_and_releases() {
        let room = RoomState::default();
        let emd = clips();
        let emw = clips();
        let room_clips = room_clips(&[(PUSH_CLIP, 2), (PUSH_LOOP_CLIP, 4)]);
        let mut player = player_at(1000, 1000);
        player.locked = LockedAction::Push;
        player.object_push = true;
        player.push_heavy = false;

        // Wind up through clip 0x30, then run clip 0x31; the grunt lands on
        // the loop's frame 1.
        let mut sounds = Vec::new();
        for _ in 0..20 {
            update_with_room(
                &mut player,
                &room,
                &emd,
                &emw,
                &room_clips,
                Input::default(),
            );
            sounds.extend(player.take_sounds().into_iter().map(|sound| sound.id));
            if sounds.contains(&SE_PUSH_GRUNT) {
                break;
            }
        }
        assert!(
            sounds.contains(&SE_PUSH_GRUNT),
            "grunt not queued: {sounds:?}"
        );
        assert_eq!(player.clip_source, ClipSource::Room);
        assert_eq!(player.anim.clip, PUSH_LOOP_CLIP);
        assert!(player.pos[0] > 1000, "push did not move the player");

        // The bit drops: the release state runs, then control returns.
        player.object_push = false;
        let mut released = false;
        for _ in 0..12 {
            update_with_room(
                &mut player,
                &room,
                &emd,
                &emw,
                &room_clips,
                Input::default(),
            );
            if player.locked == LockedAction::None {
                released = true;
                break;
            }
        }
        assert!(released, "the push never released");
        assert_eq!(player.clip_source, ClipSource::Emd);
        assert_eq!(player.anim.clip, SETTLE_CLIP);
    }

    #[test]
    fn push_without_the_room_clip_still_releases() {
        let room = RoomState::default();
        let emd = clips();
        let mut player = player_at(0, 0);
        player.locked = LockedAction::Push;
        player.object_push = true;
        update_with_room(&mut player, &room, &emd, &emd, &[], Input::default());
        assert_eq!(player.action_state, 1);
        update_with_room(&mut player, &room, &emd, &emd, &[], Input::default());
        assert_eq!(player.action_state, 2, "the missing clip skips the wind-up");
        player.object_push = false;
        update_with_room(&mut player, &room, &emd, &emd, &[], Input::default());
        assert_eq!(player.action_state, 3);
        update_with_room(&mut player, &room, &emd, &emd, &[], Input::default());
        assert_eq!(player.locked, LockedAction::None);
    }

    #[test]
    fn push_heavy_uses_the_other_grunt() {
        let room = RoomState::default();
        let emd = clips();
        let room_clips = room_clips(&[(PUSH_CLIP, 1), (PUSH_LOOP_CLIP, 3)]);
        let mut player = player_at(0, 0);
        player.locked = LockedAction::Push;
        player.object_push = true;
        player.push_heavy = true;
        let mut grunts = Vec::new();
        for _ in 0..20 {
            update_with_room(
                &mut player,
                &room,
                &emd,
                &emd,
                &room_clips,
                Input::default(),
            );
            grunts.extend(player.take_sounds().into_iter().map(|sound| sound.id));
            if grunts.contains(&SE_PUSH_GRUNT_HEAVY) {
                break;
            }
        }
        assert!(grunts.contains(&SE_PUSH_GRUNT_HEAVY), "{grunts:?}");
        assert!(!grunts.contains(&SE_PUSH_GRUNT), "{grunts:?}");
    }

    #[test]
    fn interact_plays_the_reach_then_returns_control() {
        let room = RoomState::default();
        let emd = clips();
        let mut emw = clips();
        emw.push(clip(2));
        let request = ReachRequest {
            slot: 3,
            handler: 4,
        };
        let mut player = player_at(0, 0);
        player.begin_interact(request);
        assert_eq!(player.locked, LockedAction::Interact);
        assert_eq!(player.action_state, 0);

        // State 0 selects the no-weapon reach clip.
        update_with_room(&mut player, &room, &emd, &emw, &[], Input::default());
        assert_eq!(player.clip_source, ClipSource::Emw);
        assert_eq!(player.anim.clip, INTERACT_CLIP);
        assert_eq!(player.action_state, 1);
        assert_eq!(player.locked, LockedAction::Interact, "still locked");

        // The two-frame clip finishes: the request is handed back and state 2
        // arms the body clip with a snap.
        let mut finished = None;
        for _ in 0..8 {
            update_with_room(&mut player, &room, &emd, &emw, &[], Input::default());
            if let Some(report) = player.take_interact_finished() {
                finished = Some(report);
                break;
            }
        }
        assert_eq!(finished, Some(request), "the reach reported its request");
        assert_eq!(
            player.locked,
            LockedAction::Interact,
            "state 2 still owns the tick"
        );
        assert_eq!(
            player.clip_source,
            ClipSource::Emd,
            "the body clip is armed"
        );
        assert_eq!(player.anim.blend_counter, 0, "the body clip snaps");

        // State 2 plays one body tick and returns control.
        update_with_room(&mut player, &room, &emd, &emw, &[], Input::default());
        assert_eq!(player.locked, LockedAction::None);
        assert_eq!(player.action_state, 0);
        assert!(player.take_interact_finished().is_none());
    }

    #[test]
    fn vault_turns_runs_its_cues_and_warps() {
        let room = RoomState::default();
        let emd = clips();
        let room_clips = room_clips(&[(VAULT_CLIP_PLAIN, 54)]);
        let mut player = player_at(1000, 1000);
        player.angle = 0x123;
        player.locked = LockedAction::Vault;
        player.vault_bit = true;
        player.attack_direction = -1;

        let mut sounds = Vec::new();
        let mut warped = None;
        for tick in 0..200 {
            update_with_room(
                &mut player,
                &room,
                &emd,
                &emd,
                &room_clips,
                Input::default(),
            );
            sounds.extend(player.take_sounds().into_iter().map(|sound| sound.id));
            if player.locked == LockedAction::None {
                warped = Some(tick);
                break;
            }
        }

        assert!(warped.is_some(), "the vault never completed");
        // The warp: dir -1, X not sideways -> X = -(-0x73A), Y = -0x708.
        assert_eq!(player.pos, [1000 + VAULT_SIDE, -VAULT_ACROSS, 1000]);
        assert!(
            sounds.iter().filter(|id| **id == SE_VAULT_STEP).count() >= 2,
            "vault cues missing: {sounds:?}"
        );
        assert!(!player.vault_bit, "the vault must clear the transition bit");
        assert_eq!(player.clip_source, ClipSource::Emd);
    }

    #[test]
    fn vault_state_zero_turns_over_the_walk_clip() {
        let room = RoomState::default();
        let emd = clips();
        let emw = clips();
        let room_clips = room_clips(&[(VAULT_CLIP_PLAIN, 4)]);
        let mut player = player_at(0, 0);
        player.angle = 0x123;
        player.locked = LockedAction::Vault;
        player.vault_bit = true;

        update_with_room(
            &mut player,
            &room,
            &emd,
            &emw,
            &room_clips,
            Input::default(),
        );
        assert_eq!(player.action_state, 0, "still squaring up");
        assert_eq!(player.clip_source, ClipSource::Emw);
        assert_eq!(player.anim.clip, WALK_CLIP);
        assert_eq!(player.anim.display_frame, 0);
    }

    #[test]
    fn vault_return_side_uses_the_other_clip_and_mirrors_z() {
        let room = RoomState::default();
        let emd = clips();
        let room_clips = room_clips(&[(VAULT_CLIP_RETURN, 4)]);
        let mut player = player_at(0, 0);
        player.angle = 0x400;
        player.locked = LockedAction::Vault;
        player.vault_bit = true;
        player.vault_return = true;
        player.attack_direction = 1;
        for _ in 0..30 {
            update_with_room(
                &mut player,
                &room,
                &emd,
                &emd,
                &room_clips,
                Input::default(),
            );
            if player.locked == LockedAction::None {
                break;
            }
        }
        // Sideways (angle bit 0x400): Z = dir * 0x73A; the return side flips Y.
        assert_eq!(player.pos, [0, VAULT_ACROSS, VAULT_SIDE]);
    }

    #[test]
    fn vault_without_the_room_clip_still_warps() {
        let room = RoomState::default();
        let emd = clips();
        let mut player = player_at(0, 0);
        player.locked = LockedAction::Vault;
        player.vault_bit = true;
        player.attack_direction = -1;
        for _ in 0..10 {
            update_with_room(&mut player, &room, &emd, &emd, &[], Input::default());
            if player.locked == LockedAction::None {
                break;
            }
        }
        assert_eq!(player.locked, LockedAction::None);
        assert_eq!(player.pos, [VAULT_SIDE, -VAULT_ACROSS, 0]);
    }

    #[test]
    fn a_locked_push_does_not_read_locomotion_input() {
        let room = RoomState::default();
        let emd = clips();
        let room_clips = room_clips(&[(PUSH_CLIP, 10)]);
        let mut player = player_at(0, 0);
        player.locked = LockedAction::Push;
        player.object_push = true;
        // Walking input must not switch the clip back to the EMW walk.
        update_with_room(
            &mut player,
            &room,
            &emd,
            &emd,
            &room_clips,
            Input {
                up: true,
                ..Input::default()
            },
        );
        assert_eq!(player.clip_source, ClipSource::Room);
        assert_eq!(player.anim.clip, PUSH_CLIP);
    }

    #[test]
    fn running_emits_footfalls_faster_than_walking() {
        const TICKS: usize = 300;
        let room = RoomState::default();
        let (emd_clips, emw_clips) = shipped_clips();

        let mut walking = player_at(1000, 1000);
        let walk = footfall_log(
            &mut walking,
            &room,
            &emd_clips,
            &emw_clips,
            Input {
                up: true,
                ..Input::default()
            },
            TICKS,
        );

        let mut running = player_at(1000, 1000);
        let run = footfall_log(
            &mut running,
            &room,
            &emd_clips,
            &emw_clips,
            Input {
                up: true,
                run: true,
                ..Input::default()
            },
            TICKS,
        );

        let walk_ticks: Vec<usize> = walk.iter().map(|&(tick, ..)| tick).collect();
        let run_ticks: Vec<usize> = run.iter().map(|&(tick, ..)| tick).collect();

        // The 28-frame walk cycle contacts on frames 8 and 0x16: a footfall
        // every 14 ticks (about 2.14/s).
        assert_eq!(walk_ticks.len(), 21, "{walk_ticks:?}");
        assert!(walk_ticks.windows(2).all(|pair| pair[1] - pair[0] == 14));
        assert!(
            walk.iter()
                .all(|&(_, frame, sound)| { sound == 0 && (frame == 0x08 || frame == 0x16) })
        );

        // The 20-frame run cycle contacts on frames 0 and 0x0A: a footfall
        // every 10 ticks (3/s).
        assert_eq!(run_ticks.len(), 30, "{run_ticks:?}");
        assert!(run_ticks.windows(2).all(|pair| pair[1] - pair[0] == 10));
        assert!(
            run.iter()
                .all(|&(_, frame, sound)| { sound == 1 && (frame == 0x00 || frame == 0x0A) })
        );

        assert!(
            run_ticks.len() > walk_ticks.len(),
            "run {} footfalls, walk {}",
            run_ticks.len(),
            walk_ticks.len()
        );
    }

    /// Drive a locked ladder climb, collecting every queued sound id and the
    /// screen-effect records, until control returns.
    fn ladder_drive(
        player: &mut PlayerState,
        room: &RoomState,
        emd: &[Clip],
        emw: &[Clip],
        room_clips: &[Clip],
        ticks: usize,
    ) -> (Vec<u16>, Vec<ScreenEffect>, Vec<(usize, u16)>) {
        let mut sounds = Vec::new();
        let mut effects = Vec::new();
        let mut frames = Vec::new();
        for _ in 0..ticks {
            let before = player.anim.display_frame;
            update_with_room(player, room, emd, emw, room_clips, Input::default());
            frames.push((player.anim.clip, before as u16));
            sounds.extend(player.take_sounds().into_iter().map(|sound| sound.id));
            effects.extend(player.take_screen_effects());
            if player.locked == LockedAction::None {
                break;
            }
        }
        (sounds, effects, frames)
    }

    #[test]
    fn ladder_plain_climb_plays_the_transcribed_step_frames() {
        let room = RoomState::default();
        let emd = clips();
        let emw = clips();
        // The shipped ROOM301 clip 0x33 has 59 frames; frame 0x32 is the sound
        // cue while the clip's own completion advances the state.
        let room_clips = room_clips(&[(LADDER_CLIP_PLAIN, 59)]);
        let mut player = player_at(0, 0);
        player.locked = LockedAction::Ladder;
        player.action_state = 0;
        player.stairs.base = [100, 0];
        player.stairs.ladder = false;

        let (sounds, effects, frames) =
            ladder_drive(&mut player, &room, &emd, &emw, &room_clips, 300);

        assert_eq!(effects.len(), 2, "the climb writes two screen effects");
        assert_eq!(
            effects[0],
            ScreenEffect {
                right: 800,
                left: 700,
                front: 700,
                back: 700,
            }
        );
        assert_eq!(
            effects[1],
            ScreenEffect {
                right: 500,
                left: 500,
                front: 700,
                back: 700,
            }
        );
        // The step SE fires on the transcribed frames; the end SE on frame 0x32.
        let step_frames = [12u16, 29, 39];
        for frame in step_frames {
            assert!(
                frames
                    .iter()
                    .any(|&(clip, at)| clip == LADDER_CLIP_PLAIN && at == frame),
                "frame {frame} never displayed"
            );
        }
        let step_count = sounds.iter().filter(|&&id| id == SE_LADDER_STEP).count();
        assert_eq!(step_count, 3, "step SEs: {sounds:?}");
        assert!(sounds.contains(&SE_LADDER_END), "end SE: {sounds:?}");
        assert!(!sounds.contains(&SE_LADDER_GRUNT));

        // The plain step-off is -1000 Z (facing +X) and the walk-away runs
        // with the entity footstep at frame 8.
        assert_eq!(player.pos[1], 0, "the plain climb returns to the floor");
        assert_eq!(player.pos[2], -LADDER_STEP_OFF);
        assert_eq!(
            sounds.iter().filter(|&&id| id == SE_FOOTSTEP).count(),
            2,
            "walk-away footsteps: {sounds:?}"
        );
    }

    #[test]
    fn ladder_variant_shift_sets_the_height_and_slide() {
        let room = RoomState::default();
        let emd = clips();
        let emw = clips();
        let room_clips = room_clips(&[(LADDER_CLIP_VARIANT, 40)]);
        let mut player = player_at(0, 0);
        player.locked = LockedAction::Ladder;
        player.action_state = 4;
        player.stairs.ladder = true;
        player.angle = 0xC00;
        player.set_clip(ClipSource::Room, LADDER_CLIP_VARIANT);
        player.anim.display_frame = LADDER_VARIANT_SHIFT_FRAME;

        update_with_room(
            &mut player,
            &room,
            &emd,
            &emw,
            &room_clips,
            Input::default(),
        );
        assert_eq!(player.pos[2], LADDER_VARIANT_SLIDE, "the +Z shift");
        assert_eq!(player.pos[1], LADDER_VARIANT_HEIGHT);
        assert!(player.take_sounds().is_empty());

        // Frame 0x1A plays the grunt instead of a step SE.
        player.anim.display_frame = LADDER_VARIANT_GRUNT_FRAME;
        update_with_room(
            &mut player,
            &room,
            &emd,
            &emw,
            &room_clips,
            Input::default(),
        );
        let sounds: Vec<u16> = player.take_sounds().iter().map(|s| s.id).collect();
        assert_eq!(sounds, [SE_LADDER_GRUNT]);
    }

    #[test]
    fn ladder_variant_rides_up_and_releases() {
        let room = RoomState::default();
        let emd = clips();
        let emw = clips();
        let room_clips = room_clips(&[(LADDER_CLIP_VARIANT, 40)]);
        let mut player = player_at(0, 0);
        player.locked = LockedAction::Ladder;
        player.action_state = 0;
        // Base straight ahead in +Z so the approach keeps the 0xC00 facing.
        player.stairs.base = [0, 100];
        player.stairs.ladder = true;
        player.angle = 0xC00;

        let (sounds, effects, _) = ladder_drive(&mut player, &room, &emd, &emw, &room_clips, 300);

        assert!(sounds.contains(&SE_LADDER_GRUNT), "{sounds:?}");
        assert!(!sounds.contains(&SE_LADDER_STEP), "variant has no step SE");
        assert!(!sounds.contains(&SE_LADDER_END), "variant has no end SE");
        assert_eq!(
            player.pos[1], LADDER_VARIANT_HEIGHT,
            "the variant holds the upper height"
        );
        // The variant step-off is +2000 Z after the +0x708 shift.
        assert!(
            player.pos[2] >= LADDER_VARIANT_SLIDE + LADDER_STEP_OFF_VARIANT,
            "variant travel {}",
            player.pos[2]
        );
        assert_eq!(effects.len(), 2);
        assert!(
            player.ladder_release,
            "state 8 must raise the release for the room probe"
        );
    }

    #[test]
    fn ladder_state_8_clears_the_variant_and_releases_control() {
        let room = RoomState::default();
        let emd = clips();
        let emw = clips();
        let room_clips = room_clips(&[(LADDER_CLIP_VARIANT, 40)]);
        let mut player = player_at(0, 0);
        player.locked = LockedAction::Ladder;
        player.action_state = 0;
        player.stairs.base = [100, 0];
        player.stairs.ladder = true;
        player.angle = 0xC00;
        for _ in 0..300 {
            update_with_room(
                &mut player,
                &room,
                &emd,
                &emw,
                &room_clips,
                Input::default(),
            );
            if player.locked == LockedAction::None {
                break;
            }
        }
        assert_eq!(player.locked, LockedAction::None);
        assert!(player.ladder_release);
        assert!(!player.stairs.ladder, "state 8 clears zone flag 0x10");
        assert_eq!(player.anim.clip, SETTLE_CLIP);
        assert_eq!(player.clip_source, ClipSource::Emd);
    }

    #[test]
    fn ladder_step_table_records_the_shipped_frames() {
        assert_eq!(
            &LADDER_STEP_FRAMES[..4],
            &[12, 29, 39, 0],
            "the plain climb's step frames"
        );
        assert_eq!(
            &LADDER_STEP_FRAMES[4..7],
            &[80, 100, 130],
            "the longer stair-clip tail"
        );
        // The SE ids are the transcribed globals.
        assert_eq!(
            (SE_LADDER_STEP, SE_LADDER_END, SE_LADDER_GRUNT),
            (0x23, 0x2D, 0x17)
        );
    }
}
