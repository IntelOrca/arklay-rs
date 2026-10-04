//! Player locomotion: input mapping, movement, room collision and camera cuts.
//!
//! This mirrors the original engine's fixed 30 Hz player step using the EMD/EMW
//! body clips: idle settle and breathe, walk, turn in place, backward walk and
//! run. The player lives on the room's XZ plane; the room collision path never
//! changes Y, so the spawn height is kept for the whole room.
//!
//! The original's slow-motion modifier halves the walk speed and holds each
//! locomotion frame for an extra tick while a room flag is set. No state
//! source sets that flag in this engine yet, so the port always runs at full
//! speed and the modifier's doubled footstep cadence is not modelled.

use crate::anim::AnimPlayer;
use crate::model::Clip;
use crate::state::{Collision, CollisionRect, RoomId, RoomState};

/// Collision radius of the Chris model.
pub const CHRIS_RADIUS: i32 = 422;
/// Collision radius of the Jill model.
pub const JILL_RADIUS: i32 = 372;

/// EMD clip 0: the three-frame idle settle pose.
const SETTLE_CLIP: usize = 0;
/// EMW clip 0: the transition from the settle pose into the breathe loop.
const BREATHE_IN_CLIP: usize = 0;
/// EMW clip 1: the looping breathe animation.
const BREATHE_CLIP: usize = 1;
/// EMW clip 2: the walk cycle, also used for turning in place.
const WALK_CLIP: usize = 2;
/// EMW clip 3: the run.
const RUN_CLIP: usize = 3;
/// EMD clip 3: the backward walk. The original plays it from the body model,
/// not the no-weapon EMW, and only falls back to EMD clip 2 while an enemy is
/// in view (this engine has no enemy visibility test yet).
const BACK_CLIP: usize = 3;

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

/// Per-character footfall speed table: two frame windows and two reductions.
const CHRIS_FOOTFALL: [u8; 4] = [0x15, 0x17, 0x0D, 0x0E];
/// Jill's footfall speed table.
const JILL_FOOTFALL: [u8; 4] = [0x14, 0x16, 0x0F, 0x0F];

/// Angular step of the PS1 trig tables: `2*pi / 4096`.
const ANGLE_STEP: f64 = 0.0015339807880859375;

/// Keyboard state for one tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub run: bool,
}

/// A footstep sound request emitted when a locomotion clip applies a contact
/// frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Footstep {
    /// Player position when the contact frame was applied.
    pub pos: [i32; 3],
    /// The contact frame: `0x08` or `0x16`.
    pub frame: u8,
    /// Entity sound type; walking, turning and running all use type 0.
    pub sound_type: u8,
}

/// Which model file's keyframes drive the current clip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipSource {
    /// The body EMD (idle settle and scripted poses).
    #[default]
    Emd,
    /// The no-weapon EMW (breathe, walk, turn, back, run).
    Emw,
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
    /// Footstep events emitted since the last [`PlayerState::take_footsteps`].
    footsteps: Vec<Footstep>,
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
        anim: AnimPlayer::new(SETTLE_CLIP),
        clip_source: ClipSource::Emd,
        behavior: BEHAVIOR_IDLE,
        idle_phase: 0,
        idle_ticks: 0,
        footsteps: Vec::new(),
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
    let behavior = behavior_for(input);
    if behavior != player.behavior {
        player.behavior = behavior;
        player.idle_ticks = 0;
        player.idle_phase = 0;
        player.set_clip(entry_clip_source(behavior), entry_clip(behavior));
    }

    player.angle = turned_angle(player.angle, behavior, input);

    let speed = match behavior {
        BEHAVIOR_WALK => walk_speed(player.radius, player.anim.frame),
        BEHAVIOR_TURN => 0,
        BEHAVIOR_BACK => BACK_SPEED,
        BEHAVIOR_RUN => RUN_SPEED,
        _ => 0,
    };
    let offset = if behavior == BEHAVIOR_BACK {
        BACK_OFFSET
    } else {
        0
    };

    let prev = player.pos;
    let (dx, dz) = rotate_speed(player.angle, offset, speed);
    let proposed = [prev[0] + dx, prev[1], prev[2] + dz];
    player.pos = resolve_collision(&room.collision, prev, proposed, player.radius);

    if behavior == BEHAVIOR_IDLE {
        player.idle_ticks = player.idle_ticks.saturating_add(1);
        match player.idle_phase {
            0 => {
                player.advance(emd_clips, emw_clips);
                if player.idle_ticks >= IDLE_SETTLE_TICKS {
                    player.idle_phase = 1;
                    player.set_clip(ClipSource::Emw, BREATHE_IN_CLIP);
                }
            }
            1 => {
                if player.advance(emd_clips, emw_clips) {
                    player.idle_phase = 2;
                    player.set_clip(ClipSource::Emw, BREATHE_CLIP);
                }
            }
            _ => {
                player.advance(emd_clips, emw_clips);
            }
        }
    } else {
        player.advance(emd_clips, emw_clips);
    }
    player.emit_footsteps(behavior);
}

impl PlayerState {
    fn set_clip(&mut self, source: ClipSource, clip: usize) {
        self.clip_source = source;
        self.anim.set_clip(clip);
    }

    fn advance(&mut self, emd_clips: &[Clip], emw_clips: &[Clip]) -> bool {
        match self.clip_source {
            ClipSource::Emd => self.anim.update(emd_clips),
            ClipSource::Emw => self.anim.update(emw_clips),
        }
    }

    /// Emit a footstep when the displayed frame of a walk, turn or run clip is
    /// a contact frame.
    ///
    /// The original tests frames `0x08` and `0x16` on every frame rather than
    /// on the transition into them, so a contact frame held by its timing
    /// re-fires each tick it is displayed.
    fn emit_footsteps(&mut self, behavior: u8) {
        if self.clip_source != ClipSource::Emw
            || !matches!(behavior, BEHAVIOR_WALK | BEHAVIOR_TURN | BEHAVIOR_RUN)
        {
            return;
        }

        let frame = self.anim.display_frame;
        if frame == 0x08 || frame == 0x16 {
            self.footsteps.push(Footstep {
                pos: self.pos,
                frame: frame as u8,
                sound_type: 0,
            });
        }
    }

    /// Drain the footstep events emitted since the last call.
    pub fn take_footsteps(&mut self) -> Vec<Footstep> {
        std::mem::take(&mut self.footsteps)
    }
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
fn rotate_speed(angle: u16, offset: u16, speed: i32) -> (i32, i32) {
    let angle = angle.wrapping_add(offset) & 0x0FFF;
    let m00 = cos14(angle) >> 2;
    let m20 = (-sin14(angle)) >> 2;
    (fixed_mul_12(m00, speed), fixed_mul_12(m20, speed))
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
/// if it is still inside a blocking record.
fn resolve_collision(
    collision: &Collision,
    prev: [i32; 3],
    proposed: [i32; 3],
    radius: i32,
) -> [i32; 3] {
    let records = collision.records(proposed[0], proposed[2]);
    let mut pos = proposed;
    let mut hit = 0u16;

    for rect in records {
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
        if shape == 4 {
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
fn point_outside(x: i32, z: i32, x_hi: i32, z_hi: i32, x_lo: i32, z_lo: i32) -> bool {
    let a = x.wrapping_sub(x_lo);
    let b = x_hi.wrapping_sub(x);
    let c = z.wrapping_sub(z_lo);
    let d = z_hi.wrapping_sub(z);
    (a | b | c | d) < 0
}

/// Shapes 1 and 5: push the entity out of a rectangular obstacle.
///
/// The four exit depths are computed in 16-bit modular arithmetic with the
/// 18-unit skin. The axis whose push opposes this tick's movement is the face
/// the entity entered through; when both or neither do, or when a single-axis
/// push exceeds 400 units, the shallower correction wins.
fn push_rect(rect: &CollisionRect, prev: [i32; 3], pos: &mut [i32; 3], radius: i32) {
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

    // Selector bit 0: movement and push disagree in sign on X; bit 1: on Z.
    let movement_x = pos[0] - prev[0];
    let movement_z = pos[2] - prev[2];
    let selector = ((((movement_z >> 14) ^ (i32::from(push_z) >> 14)) & 2)
        | (((movement_x >> 15) ^ (i32::from(push_x) >> 15)) & 1)) as u8;

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

/// Shapes 3: push the entity out of a circular obstacle whose radius is the
/// record's X half-width plus the entity radius, centred on the box centre.
fn push_circle(rect: &CollisionRect, pos: &mut [i32; 3], radius: i32) {
    let extent = (u32::from(rect.x_max))
        .wrapping_sub(u32::from(rect.x_min))
        .wrapping_add((radius as u32).wrapping_mul(2));
    let reach = (extent as i32) / 2;

    let dz = (pos[2] - i32::from(rect.z_min) - reach) + radius;
    let dx = (pos[0] - i32::from(rect.x_min) - reach) + radius;
    let dist = integer_sqrt(dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz)));

    let penetration = reach - dist;
    if penetration < 1 {
        return;
    }
    if dist == 0 {
        pos[0] += penetration;
        return;
    }

    let push_x = penetration.wrapping_mul(dx) / dist;
    let push_z = penetration.wrapping_mul(dz) / dist;
    pos[2] += push_z;
    pos[0] += push_x;
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
    use crate::model::ClipFrame;
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
            anim: AnimPlayer::new(SETTLE_CLIP),
            clip_source: ClipSource::Emd,
            behavior: BEHAVIOR_IDLE,
            idle_phase: 0,
            idle_ticks: 0,
            footsteps: Vec::new(),
        }
    }

    /// Drive one tick with the same clip set standing in for EMD and EMW.
    fn step(player: &mut PlayerState, room: &RoomState, clips: &[Clip], input: Input) {
        update(player, room, clips, clips, input);
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
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].frame, 0x08);
        assert_eq!(events[1].frame, 0x16);
    }

    #[test]
    fn backward_walk_does_not_emit_footsteps() {
        let room = RoomState::default();
        let clips = clips();
        let mut player = player_at(1000, 1000);
        let input = Input {
            down: true,
            ..Input::default()
        };

        let events = step_events(&mut player, &room, &clips, input, 30);

        assert!(events.is_empty(), "{events:?}");
        assert_eq!(player.clip_source, ClipSource::Emd);
    }

    #[test]
    fn a_held_contact_frame_repeats_its_footstep() {
        let room = RoomState::default();
        let mut clips = clips();
        clips[WALK_CLIP].frames[0x08].timing = 2;
        let mut player = player_at(1000, 1000);
        let input = Input {
            up: true,
            ..Input::default()
        };

        // Advance to the tick that first applies contact frame 0x08.
        let mut ticks = 0;
        loop {
            step(&mut player, &room, &clips, input);
            let taken = player.take_footsteps();
            if !taken.is_empty() {
                assert_eq!(taken[0].frame, 0x08);
                break;
            }
            ticks += 1;
            assert!(ticks < 30, "the 0x08 contact frame was never applied");
        }

        // The next tick holds the same frame and must fire again.
        step(&mut player, &room, &clips, input);
        let held = player.take_footsteps();
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(held[0].frame, 0x08);

        // Once the clip moves on, the held frame stops firing.
        step(&mut player, &room, &clips, input);
        assert!(player.take_footsteps().is_empty(), "still on the contact");
    }
}
