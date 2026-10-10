//! Native movement helpers shared by the scripted (state 8) and follow
//! (state 9) drivers.
//!
//! The original's state-8 handlers steer with `turn_toward_target` /
//! `entity_rotate_toward_target`, trim their speed on the animation's footfall
//! frames with `entity_apply_walk_speed`, and move with `Add_speedXZ`; state 9
//! reuses the same helpers. `Add_speedXZ` is a bare un-collided add: a
//! character walks into geometry and is only pushed back out by the pass that
//! runs later in its own driver ([`update`]'s state-9 tail, the player's
//! [`crate::player_script`] tail, or the room-object pass). [`try_advance_xz`]
//! keeps the rollback only for the idle walk-01 probe, where the original
//! saves the moved words, probes, and restores them.
//!
//! State 9 adds the walk-zone graph the room data provides: [`walk_zone_find`]
//! locates the zone containing a point, [`walk_zone_shared_edge`] names the
//! crossing between two zones, and [`zone_path_find`] runs the original's
//! distance-weighted CW/CCW zone-ring walk from the entity to the player (a
//! `to.z == 0` target names a zone index instead, and the waypoint is that
//! zone's midpoint). [`crossing_heading`] clamps the crossing into the shared
//! corridor with two wall probes, and the four behaviours in [`update`] walk,
//! run, pace or retreat on a distance ring.
//!
//! The collision tail matches the original's order: the entity's SCA volume is
//! rotated into world space, [`separate_from_player`] and
//! [`separate_from_character`] run the volume penetration test (XZ radius,
//! height overlap, hit flag and the pre-move `position` degenerate fix), and
//! the end-of-frame room collision pass pushes the result out of walls and
//! rolls X/Z back to the last accepted position when it is still stuck. The
//! pushed entity is never the player and no damage is transferred.

use crate::enemy::data;
use crate::game::{Entity, EntitySound, GameState};
use crate::model::Clip;
use crate::player;
use crate::sfx;
use crate::state::RoomState;

use crate::anim::{Mat4x3, compose};

/// XZ distance from `entity` to `target`, rounded down.
pub fn xz_distance_to(entity: &Entity, target: [i32; 3]) -> i32 {
    let dx = i64::from(target[0] - entity.pos[0]);
    let dz = i64::from(target[2] - entity.pos[2]);
    ((dx * dx + dz * dz) as f64).sqrt() as i32
}

/// The angular step toward `target` the original's `turn_toward_target`
/// returns: zero when already aligned within `step * 2`, otherwise `+step` or
/// `-step` along the shorter turn. Used by the state-8 turn phases.
pub fn turn_toward_target(entity: &Entity, target: [i32; 3], step: i16) -> i16 {
    let target_angle = sfx::angle_between_xz(entity.pos[0], entity.pos[2], target[0], target[2]);
    let delta = target_angle
        .wrapping_sub(entity.angle)
        .wrapping_add(step as u16)
        & 0x0FFF;
    if i32::from(delta) < i32::from((step as u16).wrapping_mul(2)) {
        return 0;
    }
    if delta < 0x801 { step } else { -step }
}

/// The original's `entity_rotate_toward_target`: step the entity's yaw toward
/// `target` by at most `step`, snapping when the remaining turn is under two
/// steps. Bit 15 of `step` flips the direction (face away from the target).
pub fn rotate_toward_target(entity: &mut Entity, target: [i32; 3], step: u16) {
    let target_angle = sfx::angle_between_xz(entity.pos[0], entity.pos[2], target[0], target[2]);
    let mut step = step;
    let mut base = target_angle;
    if step & 0x8000 != 0 {
        step = step.wrapping_neg();
        base = (base + 0x800) & 0x0FFF;
    }
    let delta = step.wrapping_sub(entity.angle).wrapping_add(base) & 0x0FFF;
    if i32::from(delta) < i32::from(step as i16) * 2 {
        entity.angle = base;
        return;
    }
    entity.angle = entity.angle.wrapping_sub(step);
    if delta < 0x801 {
        entity.angle = entity.angle.wrapping_add(step.wrapping_mul(2));
    }
}

/// `entity_apply_walk_speed`: set this tick's speed, then trim it on the
/// animation frames where a foot is planted so the character does not slide.
/// The four range tests are unsigned byte subtractions, so a frame id below
/// the window wraps to a large value and fails, exactly like the original.
pub fn entity_apply_walk_speed(entity: &mut Entity, speed: i16) {
    entity.move_speed_current = speed as u16;
    let frame = entity.animation_frame_id;
    let mut current = entity.move_speed_current as i16;
    if frame.wrapping_sub(0x15) < 7 {
        current = current.wrapping_sub(0xD);
    }
    if frame.wrapping_sub(7) < 7 {
        current = current.wrapping_sub(0xD);
    }
    if frame.wrapping_sub(0x17) < 3 {
        current = current.wrapping_sub(0xE);
    }
    if frame.wrapping_sub(9) < 3 {
        current = current.wrapping_sub(0xE);
    }
    entity.move_speed_current = current as u16;
}

/// `entity_check_visual_range`: raise `status_flags` bit 5 when the flat
/// distance from the entity to the player is inside `range`, and return the
/// distance. Both axes are full 32-bit differences and the sum wraps through
/// the platform integer before the square root, exactly like the original; a
/// wrapped-negative sum reads as distance zero.
pub fn entity_check_visual_range(entity: &mut Entity, player_pos: [i32; 3], range: u32) -> u32 {
    let distance = player_flat_distance(entity, player_pos);
    if distance < range {
        entity.status_flags |= 0x20;
    }
    distance
}

/// `entity_check_alert_range`: the same distance and square root, raising
/// `status_flags` bit 7 instead.
pub fn entity_check_alert_range(entity: &mut Entity, player_pos: [i32; 3], range: u32) -> u32 {
    let distance = player_flat_distance(entity, player_pos);
    if distance < range {
        entity.status_flags |= 0x80;
    }
    distance
}

/// The shared flat entity-to-player distance both range checks measure.
fn player_flat_distance(entity: &Entity, player_pos: [i32; 3]) -> u32 {
    let dx = player_pos[0].wrapping_sub(entity.pos[0]);
    let dz = player_pos[2].wrapping_sub(entity.pos[2]);
    sqrt0(dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz)))
}

/// `is_facing_toward_entity`: is `target_angle` within half a turn of
/// `entity_angle`? The original compares the player's own facing against the
/// entity's yaw, with `0x400` folded in so the strict `< 0x800` window is
/// symmetric around straight ahead.
pub fn is_facing_toward_entity(entity_angle: u16, target_angle: u16) -> bool {
    let diff = i32::from(target_angle) - i32::from(entity_angle);
    ((diff + 0x400) as u32 & 0xFFF) < 0x800
}

/// The adder's path-result keep: `entity_pathfind_update`'s small bitfield is
/// reduced to its bit 0 in the scratch byte at entity `+0x16C`, and only when
/// no higher bit was returned. The low byte is masked before the word OR, so
/// the scratch word's high byte is untouched.
pub fn pathfind_track(entity: &mut Entity, path: u8) {
    if path & 0xFE == 0 {
        entity.attacking_direction = (entity.attacking_direction & 0xFE) | (path & 1);
    }
}

/// The wasp's path-result keep: the same reduction, but into the scratch byte
/// at entity `+0x16E` (the word the hover and victory behaviours gate on).
/// Only the low byte's bit 0 is rewritten; the high byte is untouched.
pub fn wasp_pathfind_track(entity: &mut Entity, path: u8) {
    if path & 0xFE == 0 {
        entity.tex_bank = (entity.tex_bank & 0xFE) | (path & 1);
    }
}

/// The monsters' end-of-frame room resolve with the original's result code:
/// push the entity out of the walls, or roll X/Z back to the accepted
/// position, then store the accepted result and the crossed floor/step height.
/// Returns `0` (clear), `1` (pushed), `2` (rolled back) or `3` (a floor/step
/// zone was crossed); the caller's consecutive-blocked counter counts
/// everything but `0`.
pub fn check_room_collision(room: &RoomState, entity: &mut Entity) -> u8 {
    let prev_pos = entity.saved_pos.unwrap_or(entity.pos);
    let radius = i32::from(entity.sca_radius);
    let (pos, code, floor_step) = player::resolve_collision_code(
        &room.collision,
        prev_pos,
        entity.pos,
        radius,
        entity.collision_flags,
    );
    entity.pos = pos;
    entity.saved_pos = Some(pos);
    entity.floor_step = floor_step;
    code
}

/// The hound's forward probe distance (the collision record's local X).
pub const PROBE_DISTANCE: i16 = 500;
/// The hound's forward probe radius (the collision record's radius).
pub const PROBE_RADIUS: i16 = 400;

/// The rotated probe step: `distance` along the entity's full rotation vector
/// (`pitch`/`angle`/`roll`), the `g_deadMoveValue` seed's other axes treated as
/// zero.
fn probe_step(entity: &Entity, distance: i16) -> [i32; 3] {
    let matrix =
        crate::anim::entity_matrix_rotated([0, 0, 0], entity.pitch, entity.angle, entity.roll);
    apply_matrix_lv(&matrix, [i32::from(distance), 0, 0])
}

/// The original's room resolve entered from the hound's probe: the same
/// two-pass walk as [`check_room_collision`] but at an explicit radius, with
/// the collision-flags bit-3 clear and the status bit-4 early-out, and leaving
/// the shared scratch vector set to the incoming point or the pass-1 push.
fn probe_room_collision(room: &RoomState, game: &mut GameState, slot: usize, radius: i16) -> u8 {
    game.entities[slot].collision_flags &= !0x08;
    if game.entities[slot].status_flags & 0x04 != 0 {
        return 0;
    }
    let entity = &game.entities[slot];
    let prev = entity.saved_pos.unwrap_or(entity.pos);
    let resolution = player::resolve_collision_full(
        &room.collision,
        prev,
        entity.pos,
        i32::from(radius),
        entity.collision_flags,
    );
    game.scratch_vec = resolution.scratch;
    let entity = &mut game.entities[slot];
    entity.pos = resolution.pos;
    entity.saved_pos = Some(resolution.pos);
    entity.floor_step = resolution.floor_step;
    resolution.code
}

/// The hound's `probe ahead`: step `distance` units along the entity's own
/// facing, ask the room collision at `radius` whether the stepped point is
/// inside geometry, then step straight back. Returns whether the resolve
/// reported a signed positive code (a push, rollback or floor crossing).
///
/// The probe is destructive on the entity's position: the room handlers may
/// push the stepped point and the resolve advances the accepted `position`
/// words, exactly like the original, so callers that want the probe to be a
/// pure query use [`probe_turn`].
pub fn probe_ahead(
    room: &RoomState,
    game: &mut GameState,
    slot: usize,
    distance: i16,
    radius: i16,
) -> bool {
    let step = probe_step(&game.entities[slot], distance);
    {
        let entity = &mut game.entities[slot];
        entity.pos[0] = entity.pos[0].wrapping_add(step[0]);
        entity.pos[2] = entity.pos[2].wrapping_add(step[2]);
    }
    let code = probe_room_collision(room, game, slot, radius);
    let entity = &mut game.entities[slot];
    entity.pos[0] = entity.pos[0].wrapping_sub(step[0]);
    entity.pos[2] = entity.pos[2].wrapping_sub(step[2]);
    code as i8 > 0
}

/// The hound's `probe turn`: "would I hit a wall if I turned by `angle_delta`
/// and ran at `speed_mul` times my current speed?" The scaled speed drives a
/// real `Add_speedXZ`, the probe runs, and every field the step touched
/// (`localMatrix.t` X/Z, the accepted `position` words and the live speed) is
/// restored. Returns the probe's blocked flag.
pub fn probe_turn(
    room: &RoomState,
    game: &mut GameState,
    slot: usize,
    angle_delta: i16,
    speed_mul: i16,
) -> bool {
    let saved_pos = game.entities[slot].saved_pos;
    let saved_x = game.entities[slot].pos[0];
    let saved_z = game.entities[slot].pos[2];
    let saved_speed = game.entities[slot].move_speed_current as i16;
    game.entities[slot].move_speed_current = saved_speed.wrapping_mul(speed_mul) as u16;
    {
        let entity = &mut game.entities[slot];
        advance_xz(entity, angle_delta as u16, entity.move_speed_current as i16);
    }
    let blocked = probe_ahead(room, game, slot, PROBE_DISTANCE, PROBE_RADIUS);
    let entity = &mut game.entities[slot];
    entity.pos[0] = saved_x;
    entity.pos[2] = saved_z;
    entity.saved_pos = saved_pos;
    entity.move_speed_current = saved_speed as u16;
    blocked
}

/// The hound's spawn nudge: rotate `distance` by the entity's full rotation
/// vector and add the X/Z components onto the accepted `position` words (the
/// original seeds the shared scratch vector with only its X overwritten). The
/// shared scratch vector picks up the rotated step, like the original.
pub fn nudge_spawn(game: &mut GameState, slot: usize, distance: i16) {
    let step = probe_step(&game.entities[slot], distance);
    game.scratch_vec = step;
    let entity = &mut game.entities[slot];
    let mut pos = entity.saved_pos.unwrap_or(entity.pos);
    pos[0] = i32::from((pos[0] as u16).wrapping_add(step[0] as u16) as i16);
    pos[2] = i32::from((pos[2] as u16).wrapping_add(step[2] as u16) as i16);
    entity.saved_pos = Some(pos);
}

/// The hound's head tracking: add the accumulated head yaw to the neck chain
/// (joints 1/2/3 at 1x/1.25x/1.5x), aim an imaginary entity at the head's
/// world position toward the player, integrate the turn into the stored swerve
/// and clamp it to `+/-0x100`. Returns whether the clamp bit.
///
/// The head world anchor is joint 3's previous-tick world translation plus its
/// local transform offset (the original reads the posed joint block before the
/// render pass recomposes it). The visible joint rotations are not offset in
/// the port's render pass, a documented deviation; the swerve arithmetic and
/// the clamp are exact.
pub fn head_track(game: &mut GameState, slot: usize, _clips: &[Clip]) -> bool {
    let entity = &game.entities[slot];
    let swerve = entity.cb_swerve();
    let world = game
        .joint_worlds
        .get(slot)
        .and_then(|worlds| worlds.get(3))
        .map_or([0, 0, 0], |matrix| matrix.t);
    let transform = game.entity_anims[slot]
        .skeleton
        .as_deref()
        .and_then(|skeleton| skeleton.relative.get(3))
        .map_or([0i32; 3], |offset| {
            [
                i32::from(offset[0]),
                i32::from(offset[1]),
                i32::from(offset[2]),
            ]
        });
    let head_x = world[0].wrapping_add(transform[0]);
    let head_z = world[2].wrapping_add(transform[2]);

    let mut temp = *entity;
    temp.pos[0] = head_x;
    temp.pos[2] = head_z;
    temp.angle = entity
        .angle
        .wrapping_add((swerve >> 1) as u16)
        .wrapping_add(swerve as u16);
    let player = game.entities[0].pos;
    let turn = turn_toward_target(&temp, player, 8);

    let mut swerve = swerve.wrapping_add(turn);
    let mut clamped = false;
    if swerve < -0x100 {
        swerve = -0x100;
        clamped = true;
    }
    if swerve > 0x100 {
        swerve = 0x100;
        clamped = true;
    }
    game.entities[slot].set_cb_swerve(swerve);
    clamped
}

/// `entity_swerve_around_obstacle`: obstacle-avoidance steering. Returns the
/// yaw delta to add to the entity's angle this frame and updates the entity's
/// `swerve`/`swerve_latch` scratch in place.
///
/// `angle_step` is the magnitude of the held turn, `blocked` the room
/// resolve's nonzero code, `seed` the frame count a fresh swerve holds (the
/// original passes it through the shared scratch; the crow writes 4), and
/// `target` the steering goal (the player position).
///
/// Three paths, exactly like the original:
/// - A newly blocked and unlatched: pick a 90-degree sidestep - at the
///   target's X with the entity's own Z, or (when `status_flags` bit 4 is
///   clear) the entity's own X with the target's Z - arm the latch, and return
///   the delta folded through the quadrant fix-up, which lands the turn on a
///   quadrant boundary instead of overshooting past it.
/// - Blocked resolved but frames remain: hold the same direction and count the
///   latch down.
/// - Latch exhausted: clear both fields and fall back to a plain
///   turn-toward-target step (`0`, `+step` or `-step`).
pub fn swerve_around_obstacle(
    entity: &mut Entity,
    target: [i32; 3],
    angle_step: i16,
    blocked: bool,
    seed: u8,
) -> i16 {
    let base = sfx::angle_between_xz(entity.pos[0], entity.pos[2], target[0], target[2]) as i16;

    if !blocked && entity.swerve_latch & 0x80 == 0 {
        entity.swerve_latch |= 0x80;
    }

    if blocked && entity.swerve_latch & 0x80 == 0 {
        // Path A: newly blocked.
        if entity.swerve == 0 {
            let mut swerve =
                sfx::angle_between_xz(entity.pos[0], entity.pos[2], target[0], entity.pos[2])
                    as i16;
            if entity.status_flags & 0x10 == 0 {
                swerve =
                    sfx::angle_between_xz(entity.pos[0], entity.pos[2], entity.pos[0], target[2])
                        as i16;
            }
            entity.swerve = swerve;
            let d = swerve
                .wrapping_sub(entity.angle as i16)
                .wrapping_add(angle_step) as u16
                & 0xFFF;
            entity.swerve = if d <= 0x800 { angle_step } else { -angle_step };
        }
        entity.swerve_latch = seed;

        let sw = entity.swerve;
        let yaw = entity.angle;
        let sum = i32::from(entity.angle) + i32::from(sw);

        // Quadrant unchanged - use the swerve as-is.
        if ((u32::from(yaw) >> 8) & 0x0C) == (((sum as u32) >> 8) & 0x0C) {
            return sw;
        }

        // Quadrant fix-up: rewrite the delta so the turn lands on the boundary.
        let scaled = if sw < 0 {
            i32::from(yaw & 0x3FF) + i32::from(sw)
        } else {
            i32::from(sw) - ((sum as u32 & 0xFFF) & 0x3FF) as i32
        };
        return scaled as i16;
    }

    if entity.swerve_latch & 0x7F != 0 {
        // Path B: hold the swerve, count the latch down (the whole byte,
        // so bit 7 rides along with the counter).
        entity.swerve_latch = entity.swerve_latch.wrapping_sub(1);
        let d = entity
            .swerve
            .wrapping_sub(entity.angle as i16)
            .wrapping_add(angle_step) as u16
            & 0xFFF;
        entity.swerve = if d <= 0x800 { angle_step } else { -angle_step };
        return entity.swerve;
    }

    // Path C: no swerve pending, plain turn-toward-target step.
    entity.swerve = 0;
    entity.swerve_latch = 0;
    let d = (angle_step as u16)
        .wrapping_sub(entity.angle)
        .wrapping_add(base as u16)
        & 0xFFF;
    if i32::from(angle_step) * 2 >= i32::from(d) {
        return 0;
    }
    if d <= 0x800 { angle_step } else { -angle_step }
}

/// `FUN_0048ae00`: the square joint-reach box. Compose `offset` (the local
/// probe position) onto `joint`'s world matrix, then test the player against a
/// `+/-radius` square in XZ only - height never participates. Both the joint
/// translation and the player position truncate through 16 bits before the
/// wrap-add, exactly like the original, so values past the 16-bit range read
/// wrapped.
pub fn joint_reach_test(
    joint: &Mat4x3,
    offset: [i32; 3],
    radius: i16,
    player_pos: [i32; 3],
) -> bool {
    let scratch = Mat4x3 {
        r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
        t: offset,
    };
    let local = compose(joint, &scratch);
    let reach = i32::from(radius) * 2;
    let dx =
        ((player_pos[0] as i16 as i32) - (local.t[0] as i16 as i32) + i32::from(radius)) as u16;
    if reach < i32::from(dx) {
        return false;
    }
    let dz =
        ((player_pos[2] as i16 as i32) - (local.t[2] as i16 as i32) + i32::from(radius)) as u16;
    i32::from(dz) <= reach
}

/// `Add_speedXZ`: move `distance` units along the entity's yaw plus `offset`.
/// A bare un-collided add - the original never tests the room here, so a
/// scripted walk can cross geometry and only the driver's own collision pass
/// (the state-9 tail or the room-object pass) pushes it back out.
///
/// The rotated vector is stored in the entity's `speed` field as three
/// 16-bit components and the position is advanced by those stored components,
/// exactly like the original's `ApplyMatrixSV` write-back plus signed re-add;
/// [`advance_speed`] can re-add it without recomputing the yaw.
pub fn advance_xz(entity: &mut Entity, offset: u16, distance: i16) {
    let (dx, dz) = player::rotate_speed(entity.angle, offset, i32::from(distance));
    entity.speed = [dx as i16, 0, dz as i16];
    entity.pos[0] += i32::from(entity.speed[0]);
    entity.pos[2] += i32::from(entity.speed[2]);
}

/// The original's stored-velocity re-add: add `ENTITY->speed` to the entity
/// position without recomputing the yaw, so a direction fixed at launch keeps
/// carrying the entity while its facing turns.
pub fn advance_speed(entity: &mut Entity) {
    entity.pos[0] += i32::from(entity.speed[0]);
    entity.pos[1] += i32::from(entity.speed[1]);
    entity.pos[2] += i32::from(entity.speed[2]);
}

/// The shared player-distance scratch the two spiders measure (`ws_dist`):
/// the branchless absolute values of the two 32-bit axis differences summed
/// into the flat Manhattan distance. The two sign-mask terms cancel exactly,
/// so this is `|dz| + |dx|`.
pub fn spider_distance(entity: &Entity, player_pos: [i32; 3]) -> i32 {
    let dz = player_pos[2].wrapping_sub(entity.pos[2]);
    let dx = player_pos[0].wrapping_sub(entity.pos[0]);
    let sdz = dz >> 31;
    let sdx = dx >> 31;
    ((dz ^ sdz) - sdz) - sdx + (dx ^ sdx)
}

/// `entity_ballistic_step`: one frame of a projectile arc. Horizontally the
/// entity walks `fwd_step` units along its own yaw; vertically it subtracts
/// `vy0 + air_ticks * gravity` from Y, where `air_ticks` is the entity's
/// `death_timer`. The multiply and sum are 16-bit and wrap exactly like the
/// original's `IMUL AX`/`ADD AX`; the tick only counts while the entity is
/// still above `ground_y` (Y grows downward, so the landing test is
/// `Y > ground_y`). Returns the landing velocity (`0` while airborne).
pub fn entity_ballistic_step(
    entity: &mut Entity,
    fwd_step: i16,
    vy0: i16,
    gravity: i16,
    ground_y: i32,
) -> u16 {
    let (dx, dz) = player::rotate_speed(entity.angle, 0, i32::from(fwd_step));
    entity.pos[0] = entity.pos[0].wrapping_add(dx);
    entity.pos[2] = entity.pos[2].wrapping_add(dz);

    let product = u16::from(entity.death_timer).wrapping_mul(gravity as u16);
    let vy = product.wrapping_add(vy0 as u16);
    entity.pos[1] = entity.pos[1].wrapping_sub(i32::from(vy as i16));

    if entity.pos[1] > ground_y {
        entity.pos[1] = ground_y;
        return vy;
    }
    entity.death_timer = entity.death_timer.wrapping_add(1);
    0
}

/// Which leg-chain shape the caller wants. The two spiders compose the same
/// three joints in a different order and test a different joint's armed flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegChain {
    /// The WebSpinner: entity · joint0 · joint(8p+3) · joint(8p+4), gated on
    /// joint(8p+3)'s armed flag.
    Spinner,
    /// The Black Tiger: entity · joint0 · joint(8p+5) · joint(8p+4), gated on
    /// joint(8p+4)'s armed flag.
    Tiger,
}

/// `leg_reach`: the stride length of one leg pair, stored in
/// `move_speed_current`. The current pose's local transforms are composed onto
/// the entity matrix along the chain, the stored world translation of the tip
/// joint (the previous frame's posed skeleton, still in
/// [`crate::game::GameState::joint_worlds`] while the script runs) is
/// subtracted, and the XZ magnitude of the difference is the stride.
///
/// An armed joint (its active flag bit 0 cleared by an attack effect) zeroes
/// the stride and returns. `scale` scales the composed chain's rotation
/// columns when nonzero, the original's `ScaleMatrixCols`.
pub fn leg_reach(
    entity: &mut Entity,
    anim: &crate::enemy::EntityAnim,
    clips: &[Clip],
    stored_worlds: &[Mat4x3],
    chain: LegChain,
    part: u8,
    scale: u16,
) {
    let tip = usize::from(part) * 8 + 4;
    let gate = match chain {
        LegChain::Spinner => tip.wrapping_sub(1),
        LegChain::Tiger => tip,
    };
    if entity.joint_armed & (1 << gate) != 0 {
        entity.move_speed_current = 0;
        return;
    }

    let (Some(skeleton), Some(keyframes)) = (anim.skeleton.as_deref(), anim.keyframes.as_deref())
    else {
        return;
    };
    let Some(pose) = anim.pose_keyframe(entity, clips, keyframes) else {
        return;
    };
    let locals = crate::anim::joint_local_transforms(skeleton, &pose);
    let Some(root) = locals.first() else {
        return;
    };

    let entity_matrix =
        crate::anim::entity_matrix_rotated(entity.pos, entity.pitch, entity.angle, entity.roll);
    let mut scratch = compose(&entity_matrix, root);
    if scale != 0 {
        crate::anim::scale_columns(&mut scratch, scale);
    }
    let steps = match chain {
        LegChain::Spinner => [tip.wrapping_sub(1), tip],
        LegChain::Tiger => [tip.wrapping_add(1), tip],
    };
    for index in steps {
        if let Some(local) = locals.get(index) {
            scratch = compose(&scratch, local);
        }
    }

    let stored = stored_worlds.get(tip).map_or([0, 0, 0], |world| world.t);
    let dx = scratch.t[0].wrapping_sub(stored[0]);
    let dz = scratch.t[2].wrapping_sub(stored[2]);
    entity.move_speed_current =
        sfx::integer_sqrt(dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz))) as u16;
}

/// The NPC state-8 scripted walks: `Add_speedXZ` with a pre-move collision
/// probe.
///
/// The original's NPC state-8 handlers have no end-of-frame room resolve at
/// all, so an un-collided step can walk a scripted character straight through
/// geometry with nothing to push it back out. The port keeps the pre-check +
/// rollback for the NPC drivers only (documented deviation); the player's
/// state-8 driver moves un-collided and resolves in its own tail
/// ([`crate::player_script`]), and state 9 mirrors the original's un-collided
/// move and resolves in its own tail.
pub fn advance_xz_blocked(room: &RoomState, entity: &mut Entity, offset: u16, distance: i16) {
    let _ = try_advance_xz(room, entity, offset, distance);
}

/// The idle walk-01 collision probe: `Add_speedXZ` followed by the original's
/// save/probe/restore. The move is committed, the room collision is asked
/// whether the moved point is inside a wall, and a hit restores the pre-move
/// position. (The original restores its post-move saved words instead, keeping
/// the probe step inside the geometry; the port rolls that single step back,
/// since no other pass ever resolves it.)
pub fn try_advance_xz(room: &RoomState, entity: &mut Entity, offset: u16, distance: i16) -> bool {
    let saved = entity.pos;
    advance_xz(entity, offset, distance);
    let radius = i32::from(entity.sca_radius);
    if player::position_blocked(room, entity.pos, radius, entity.collision_flags) {
        entity.pos = saved;
        return false;
    }
    true
}

/// The state-8/9 footstep callback (`PlayEntitySnd`): resolve the entity's
/// floor zone sound for `sound_type` and queue it at the entity's position.
/// `slow` is the engine's `MSF2_EFFECT_ZONE` bit, which shifts the column by
/// -3. The engine's mixer consumes the queue; a room with no matching floor
/// zone or sound name stays silent.
pub fn footstep(
    sounds: &mut Vec<EntitySound>,
    room: &RoomState,
    entity: &Entity,
    sound_type: u8,
    slow: bool,
) {
    if let Some(name) = sfx::footstep_sound(room, entity.pos, sound_type, slow) {
        let column = sfx::entity_sound_column(room, entity.pos, sound_type, slow).unwrap_or(0);
        sounds.push(EntitySound {
            name,
            bank: 2,
            column,
            pos: entity.pos,
        });
    }
}

/// `walk_zone_find`: the walk zone containing `(x, z)`, searched from the last
/// record down like the original, so an overlap resolves to the later zone.
/// Containment is half-open with the original's wrapping unsigned-short
/// arithmetic ([`WalkZone::contains`]); a point outside every zone yields
/// `None` (the original's `0xFF`).
pub fn walk_zone_find(room: &RoomState, x: i32, z: i32) -> Option<u8> {
    room.walk_zones
        .iter()
        .enumerate()
        .rev()
        .find(|(_, zone)| zone.contains(x, z))
        .map(|(index, _)| index as u8)
}

/// `walk_zone_shared_edge`: the midpoint of the edge two zones share, and the
/// orientation flag the state-9 heading branches on. Zones sharing an X edge
/// (B's `x2` meets A's `x1`, or A's `x2` meets B's `x1`) return `0` with the
/// midpoint of the overlapping Z span; a shared Z edge returns `1` with the
/// midpoint of the overlapping X span. Zones that share no edge fall through
/// the Z branch and use B's `z1` as the crossing Z, the same coordinate the
/// original's stale global holds on that path.
pub fn walk_zone_shared_edge(room: &RoomState, zone_a: u8, zone_b: u8) -> (u8, [i32; 2]) {
    let (Some(a), Some(b)) = (
        room.walk_zones.get(usize::from(zone_a)),
        room.walk_zones.get(usize::from(zone_b)),
    ) else {
        return (1, [0, 0]);
    };
    let u = |value: i16| value as u16;
    let midpoint = |lo: u16, hi: u16| i32::from((lo.wrapping_add(hi) >> 1) as i16);

    if u(b.x2) == u(a.x1) || u(a.x2) == u(b.x1) {
        let shared_x = if u(b.x2) == u(a.x1) { a.x1 } else { b.x1 };
        let lo = u16::max(u(a.z1), u(b.z1));
        let hi = u16::min(u(a.z2), u(b.z2));
        (0, [i32::from(shared_x), midpoint(lo, hi)])
    } else {
        let shared_z = if u(b.z2) == u(a.z1) { a.z1 } else { b.z1 };
        let lo = u16::max(u(a.x1), u(b.x1));
        let hi = u16::min(u(a.x2), u(b.x2));
        (1, [midpoint(lo, hi), i32::from(shared_z)])
    }
}

/// The zone path from the entity to the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZonePath {
    /// Both points are in the same zone: walk straight at the player.
    Direct {
        /// The shared zone.
        zone: u8,
    },
    /// The next zone on the path and the midpoint of the edge it shares with
    /// the entity's zone.
    Cross {
        /// The entity's zone.
        from: u8,
        /// The next zone to walk into.
        next: u8,
        /// Midpoint of the shared edge, in XZ.
        crossing: [i32; 2],
    },
    /// No zone contains a point, or no path connects the two zones.
    Unreachable,
}

/// The original's zone-walk scratch block, per call. `idx` is the zones on
/// the current walk, `dir` the per-step scan bound, `best` the best-path
/// zones, `step`/`prev` the per-step walk and previous-position X/Z pairs.
///
/// `best` holds `step + 1` entries when the goal step is recorded, and `step`
/// can reach 15 (the walk refuses to extend past index 15), so the array must
/// hold the entry at index 16.
#[derive(Clone, Copy)]
struct ZoneWalk {
    idx: [u8; 0x10],
    dir: [u8; 0x10],
    best: [u8; 0x11],
    step: [[i16; 2]; 0x10],
    prev: [[i16; 2]; 0x10],
}

impl ZoneWalk {
    /// A walk with the entity's zone and position seeded. The original never
    /// clears the rest of the block, so stale scratch leaks between calls; the
    /// port starts every walk from zeroes so the same inputs always take the
    /// same route.
    fn new(start: u8, pos: [i32; 3]) -> Self {
        let mut walk = ZoneWalk {
            idx: [0; 0x10],
            dir: [0; 0x10],
            best: [0; 0x11],
            step: [[0; 2]; 0x10],
            prev: [[0; 2]; 0x10],
        };
        walk.prev[0] = [pos[0] as i16, pos[2] as i16];
        walk.idx[0] = start;
        walk.best[0] = start;
        walk
    }
}

/// The zone's adjacency word (`flags`), zero for an index outside the table.
fn zone_flags(room: &RoomState, zone: u8) -> u32 {
    room.walk_zones
        .get(usize::from(zone))
        .map_or(0, |entry| u32::from(entry.flags))
}

/// The original's `SquareRoot0` as an unsigned accumulator step: zero for a
/// non-positive value.
fn sqrt0(value: i32) -> u32 {
    if value <= 0 {
        0
    } else {
        (f64::from(value)).sqrt() as u32
    }
}

/// The shared-edge crossing between two zones as the original's two scratch
/// words (`g_playerDisplacement` = X, `player_distance_z` = Z).
fn crossing_xz(room: &RoomState, a: u8, b: u8) -> (i16, i16) {
    let (_, crossing) = walk_zone_shared_edge(room, a, b);
    (crossing[0] as i16, crossing[1] as i16)
}

/// The distance from a walk position's previous-position pair to its current
/// step pair, the segment length the ring walk subtracts on a backtrack.
fn step_length(walk: &ZoneWalk, i: usize) -> u32 {
    let dz = i32::from(walk.step[i][1]) - i32::from(walk.prev[i][1]);
    let dx = i32::from(walk.step[i][0]) - i32::from(walk.prev[i][0]);
    sqrt0(dz * dz + dx * dx)
}

/// `zone_walk_ccw`: the descending (counter-clockwise) ring walk. Starting
/// from `walk.idx[0]`, each step probes adjacent zones in descending index
/// order until the target zone is reached, keeping the shortest total path in
/// `walk.best`. Returns the first step's zone index, or `0xFF` when no path
/// exists. Fresh positions start at `count` so the descending scan has a clean
/// top and can never re-probe a consumed candidate.
///
/// Every candidate scan is bounded and non-wrapping: a probe below zone 0 ends
/// the scan instead of wrapping to `0xFF` and reading past the zone table the
/// way the original's byte arithmetic does. The same clamp keeps the ascending
/// walk below `count`.
fn zone_walk_ccw(
    room: &RoomState,
    count: u8,
    target: u8,
    target_x: i16,
    target_z: i16,
    walk: &mut ZoneWalk,
) -> u8 {
    for k in 1..0x10 {
        walk.idx[k] = count;
    }

    let mut step: i32 = 0;
    let mut best: u32 = u32::MAX;
    let mut dist: u32 = 0;

    loop {
        let i = step as usize;
        let flags = zone_flags(room, walk.idx[i]);
        let target_bit = 1u32 << (target & 0x1F);

        if flags & target_bit == 0 {
            // Target not adjacent: extend the walk.
            if flags & ((1u32 << (walk.dir[i] & 0x1F)) - 1) == 0 {
                if i != 0 {
                    dist = dist.wrapping_sub(step_length(walk, i));
                }
                step -= 1;
                if step < 0 {
                    return finish(best, walk);
                }
                continue;
            }

            let new_len = step + 1;
            if new_len >= 0x10 {
                return finish(best, walk);
            }
            step = new_len;
            let new_i = new_len as usize;

            // Monotone descending scan of the valid zones: each probe is the
            // previous candidate minus one.
            let mut matched = None;
            let mut candidate = walk.idx[new_i];
            loop {
                let zone = candidate.wrapping_sub(1);
                if zone >= count {
                    break;
                }
                walk.idx[new_i] = zone;
                if flags & (1u32 << (zone & 0x1F)) != 0 {
                    matched = Some(zone);
                    break;
                }
                candidate = zone;
            }
            let Some(zone) = matched else {
                if i != 0 {
                    dist = dist.wrapping_sub(step_length(walk, i));
                }
                step = new_len - 2;
                if step < 0 {
                    return finish(best, walk);
                }
                continue;
            };

            // A zone already on the path truncates the walk there.
            let mut visited = false;
            let mut back = step - 1;
            while back >= 0 {
                if walk.idx[back as usize] == zone {
                    visited = true;
                    break;
                }
                back -= 1;
            }
            if visited {
                step -= 1;
                if step < 0 {
                    return finish(best, walk);
                }
                continue;
            }

            let (cross_x, cross_z) = crossing_xz(room, walk.idx[new_i], walk.idx[new_i - 1]);
            let dx = i32::from(walk.step[new_i][0]) - i32::from(cross_x);
            let dz = i32::from(walk.step[new_i][1]) - i32::from(cross_z);
            let segment = sqrt0(dz * dz + dx * dx);
            dist = dist.wrapping_add(segment);
            if best <= dist {
                dist = dist.wrapping_sub(segment);
                step -= 1;
                if step < 0 {
                    return finish(best, walk);
                }
                continue;
            }

            walk.prev[new_i] = [cross_x, cross_z];
            walk.dir[new_i] = count;
            continue;
        }

        // Target adjacent: the goal step.
        walk.dir[i] = target;
        let (cross_x, cross_z) = crossing_xz(room, walk.idx[i], target);
        let back_dx = i32::from(cross_x) - i32::from(walk.prev[i][0]);
        let back_dz = i32::from(cross_z) - i32::from(walk.prev[i][1]);
        let goal_dx = i32::from(cross_x) - i32::from(target_x);
        let goal_dz = i32::from(cross_z) - i32::from(target_z);
        dist = dist.wrapping_add(sqrt0(back_dz * back_dz + back_dx * back_dx));
        dist = dist.wrapping_add(sqrt0(goal_dz * goal_dz + goal_dx * goal_dx));
        if dist < best {
            let mut n = step + 1;
            loop {
                // The original's scratch arrays overlap (idx[n] aliases
                // dir[n-1]); the goal write above just set dir[i] to the
                // target, so best[step+1] is the target zone itself.
                walk.best[n as usize] = if n == step + 1 {
                    target
                } else {
                    walk.idx[n as usize]
                };
                best = dist;
                n -= 1;
                if n == 0 {
                    break;
                }
            }
        }
        if step != 0 {
            dist = dist.wrapping_sub(sqrt0(goal_dz * goal_dz + goal_dx * goal_dx));
            dist = dist.wrapping_sub(sqrt0(back_dz * back_dz + back_dx * back_dx));
            dist = dist.wrapping_sub(step_length(walk, i));
        }
        step -= 1;
        if step < 0 {
            return finish(best, walk);
        }
    }
}

/// `zone_walk_cw`: the ascending (clockwise) ring walk. Same shape as
/// [`zone_walk_ccw`] with the scan inverted: each probe is the previous
/// candidate plus one (a fresh position starts at zero, so zone 0 is skipped
/// on the ascending scan exactly like the original, and the scan stops at
/// `count` rather than wrapping through zero).
fn zone_walk_cw(
    room: &RoomState,
    count: u8,
    target: u8,
    target_x: i16,
    target_z: i16,
    walk: &mut ZoneWalk,
) -> u8 {
    let mut step: i32 = 0;
    let mut best: u32 = u32::MAX;
    let mut dist: u32 = 0;

    loop {
        let i = step as usize;
        let flags = zone_flags(room, walk.idx[i]);
        let target_bit = 1u32 << (target & 0x1F);

        if flags & target_bit == 0 {
            if flags & !((1u32 << (walk.dir[i].wrapping_add(1) & 0x1F)) - 1) == 0 {
                if i != 0 {
                    dist = dist.wrapping_sub(step_length(walk, i));
                }
                step -= 1;
                if step < 0 {
                    return finish(best, walk);
                }
                continue;
            }

            let new_len = step + 1;
            if new_len >= 0x10 {
                return finish(best, walk);
            }
            step = new_len;
            let new_i = new_len as usize;

            let mut matched = None;
            let mut candidate = walk.idx[new_i];
            loop {
                let zone = candidate.wrapping_add(1);
                if zone >= count {
                    break;
                }
                walk.idx[new_i] = zone;
                if flags & (1u32 << (zone & 0x1F)) != 0 {
                    matched = Some(zone);
                    break;
                }
                candidate = zone;
            }
            let Some(zone) = matched else {
                if i != 0 {
                    dist = dist.wrapping_sub(step_length(walk, i));
                }
                step = new_len - 2;
                if step < 0 {
                    return finish(best, walk);
                }
                continue;
            };

            let mut visited = false;
            let mut back = step - 1;
            while back >= 0 {
                if walk.idx[back as usize] == zone {
                    visited = true;
                    break;
                }
                back -= 1;
            }
            if visited {
                step -= 1;
                if step < 0 {
                    return finish(best, walk);
                }
                continue;
            }

            let (cross_x, cross_z) = crossing_xz(room, walk.idx[new_i], walk.idx[new_i - 1]);
            let dx = i32::from(walk.step[new_i][0]) - i32::from(cross_x);
            let dz = i32::from(walk.step[new_i][1]) - i32::from(cross_z);
            let segment = sqrt0(dz * dz + dx * dx);
            dist = dist.wrapping_add(segment);
            if best <= dist {
                dist = dist.wrapping_sub(segment);
                step -= 1;
                if step < 0 {
                    return finish(best, walk);
                }
                continue;
            }

            // The original stores the crossing in `step` on the ascending
            // walk (the descending walk stores it in `prev`); both walks read
            // it back the same way.
            walk.step[new_i] = [cross_x, cross_z];
            walk.dir[new_i] = 0xFF;
            continue;
        }

        walk.dir[i] = target;
        let (cross_x, cross_z) = crossing_xz(room, walk.idx[i], target);
        let back_dx = i32::from(cross_x) - i32::from(walk.prev[i][0]);
        let back_dz = i32::from(cross_z) - i32::from(walk.prev[i][1]);
        let goal_dx = i32::from(cross_x) - i32::from(target_x);
        let goal_dz = i32::from(cross_z) - i32::from(target_z);
        dist = dist.wrapping_add(sqrt0(back_dz * back_dz + back_dx * back_dx));
        dist = dist.wrapping_add(sqrt0(goal_dz * goal_dz + goal_dx * goal_dx));
        if dist < best {
            let mut n = step + 1;
            loop {
                walk.best[n as usize] = if n == step + 1 {
                    target
                } else {
                    walk.idx[n as usize]
                };
                best = dist;
                n -= 1;
                if n == 0 {
                    break;
                }
            }
        }
        if step != 0 {
            dist = dist.wrapping_sub(sqrt0(goal_dz * goal_dz + goal_dx * goal_dx));
            dist = dist.wrapping_sub(sqrt0(back_dz * back_dz + back_dx * back_dx));
            dist = dist.wrapping_sub(step_length(walk, i));
        }
        step -= 1;
        if step < 0 {
            return finish(best, walk);
        }
    }
}

/// The walker exit: the best path's first step, or `0xFF` when none was found.
fn finish(best: u32, walk: &ZoneWalk) -> u8 {
    if best == u32::MAX { 0xFF } else { walk.best[1] }
}

/// `zone_path_find`: walk the zone adjacency graph from the entity's zone
/// toward the target zone with the original's distance-weighted CW/CCW ring
/// walks. Returns the next zone to walk into and the shared-edge crossing; the
/// same zone is [`ZonePath::Direct`] and a point outside the grid or a
/// disconnected target is [`ZonePath::Unreachable`].
///
/// A `to.z == 0` target is the original's alternate entry: `to.x` names a zone
/// index and the waypoint becomes that zone's midpoint. A start zone of 0
/// flips the chosen ring direction, exactly like the original.
pub fn zone_path_find(room: &RoomState, from: [i32; 3], to: [i32; 3]) -> ZonePath {
    let count = room.walk_zones.len().min(0xFF) as u8;
    let Some(start) = walk_zone_find(room, from[0], from[2]) else {
        return ZonePath::Unreachable;
    };

    let (target, target_x, target_z) = if to[2] as i16 == 0 {
        let target = (to[0] & 0xFF) as u8;
        let Some(zone) = room.walk_zones.get(usize::from(target)) else {
            return ZonePath::Unreachable;
        };
        let x = ((zone.x1 as u16).wrapping_add(zone.x2 as u16) >> 1) as i16;
        let z = ((zone.z1 as u16).wrapping_add(zone.z2 as u16) >> 1) as i16;
        (target, x, z)
    } else {
        let Some(target) = walk_zone_find(room, to[0], to[2]) else {
            return ZonePath::Unreachable;
        };
        (target, to[0] as i16, to[2] as i16)
    };

    if target == start {
        return ZonePath::Direct { zone: start };
    }

    // The shortest way around the zone ring: a delta past half the count
    // walks the other way. Starting in zone 0 flips the direction.
    let mut delta = i32::from(target) - i32::from(start);
    if delta < 0 {
        delta += i32::from(count);
    }
    let mut dir: i32 = if (i32::from(count) >> 1) < delta {
        1
    } else {
        -1
    };
    if start == 0 {
        dir = -dir;
    }

    let mut walk = ZoneWalk::new(start, from);
    let first = if dir < 1 {
        walk.dir[0] = count;
        zone_walk_ccw(room, count, target, target_x, target_z, &mut walk)
    } else {
        walk.dir[0] = 0xFF;
        zone_walk_cw(room, count, target, target_x, target_z, &mut walk)
    };
    if first == 0xFF {
        return ZonePath::Unreachable;
    }
    let (_, crossing) = walk_zone_shared_edge(room, start, first);
    ZonePath::Cross {
        from: start,
        next: first,
        crossing,
    }
}

/// The original's corridor test: whether `(x, z)` sits inside the overlapping
/// span of the two zones' shared edge, with both span ends nudged inward when
/// a wall probe hits them. `flag` is [`walk_zone_shared_edge`]'s orientation
/// (0 = shared X edge, span in Z; 1 = shared Z edge, span in X). All span
/// arithmetic is unsigned-short, like the original.
pub(crate) fn corridor_open(
    room: &RoomState,
    flag: u8,
    x: i32,
    z: i32,
    zone_a: u8,
    zone_b: u8,
    radius: i32,
) -> bool {
    let (Some(a), Some(b)) = (
        room.walk_zones.get(usize::from(zone_a)),
        room.walk_zones.get(usize::from(zone_b)),
    ) else {
        return false;
    };
    let u = |value: i16| value as u16;
    let (mut lo, mut hi, pos) = if flag == 0 {
        (
            u16::max(u(a.z1), u(b.z1)),
            u16::min(u(a.z2), u(b.z2)),
            z as u16,
        )
    } else {
        (
            u16::max(u(a.x1), u(b.x1)),
            u16::min(u(a.x2), u(b.x2)),
            x as u16,
        )
    };

    let diff = i32::from(hi) - i32::from(lo);
    let step = (diff >> 3).wrapping_add(0x280) as u16;
    let probe =
        |first: i32, second: i32| player::position_blocked(room, [first, 0, second], radius, 0);
    if flag == 0 {
        if probe(x, i32::from(lo as i16)) {
            lo = lo.wrapping_add(step);
        }
        if probe(x, i32::from(hi as i16)) {
            hi = hi.wrapping_sub(step);
        }
    } else {
        if probe(i32::from(lo as i16), z) {
            lo = lo.wrapping_add(step);
        }
        if probe(i32::from(hi as i16), z) {
            hi = hi.wrapping_sub(step);
        }
    }

    hi >= lo && pos >= lo && pos < hi
}

/// `FUN_00460180`: clamp `value` into the corridor span `[lo, hi]`, nudging
/// both ends inward by `(hi - lo) >> 3 + 0x280` when a wall probe hits them.
/// Bit 15 of `probe_with_flag` picks the probe axis: set probes `(lo, probe)`
/// and `(hi, probe)` along X, clear probes `(probe, lo)` and `(probe, hi)`
/// along Z. A collapsed span returns its midpoint. The arithmetic is signed
/// 16-bit like the original.
fn clamp_corridor(
    room: &RoomState,
    value: i32,
    probe_with_flag: u16,
    lo: u16,
    hi: u16,
    radius: i32,
) -> i16 {
    let probe = (probe_with_flag & 0x7FFF) as i16 as i32;
    let mut lo = lo as i16;
    let mut hi = hi as i16;
    let step = (hi.wrapping_sub(lo) >> 3).wrapping_add(0x280);
    let blocked = |x: i32, z: i32| player::position_blocked(room, [x, 0, z], radius, 0);
    if probe_with_flag & 0x8000 != 0 {
        if blocked(i32::from(lo), probe) {
            lo = lo.wrapping_add(step);
        }
        if blocked(i32::from(hi), probe) {
            hi = hi.wrapping_sub(step);
        }
    } else {
        if blocked(probe, i32::from(lo)) {
            lo = lo.wrapping_add(step);
        }
        if blocked(probe, i32::from(hi)) {
            hi = hi.wrapping_sub(step);
        }
    }

    if hi <= lo {
        return ((i32::from(lo) + i32::from(hi)) >> 1) as i16;
    }
    let value = value as i16;
    if value < lo {
        lo
    } else if value > hi {
        hi
    } else {
        value
    }
}

/// `zone_crossing_heading`: clamp the character's own position into the shared
/// corridor between its zone and the path's next zone, with the same wall-probe
/// nudges. Returns the clamped waypoint and the heading toward it. An invalid
/// zone or a missing path falls back to the entity's own position and heading
/// zero, the deterministic stand-in for the original's out-of-table read.
pub fn crossing_heading(
    room: &RoomState,
    entity_pos: [i32; 3],
    entity_zone: Option<u8>,
    next_zone: u8,
    radius: i32,
) -> ([i32; 3], u16) {
    let zone = match entity_zone {
        Some(zone) => zone & 0x0F,
        None => match walk_zone_find(room, entity_pos[0], entity_pos[2]) {
            Some(zone) => zone,
            None => return (entity_pos, 0),
        },
    };
    let (Some(entry), Some(path)) = (
        room.walk_zones.get(usize::from(zone)),
        room.walk_zones.get(usize::from(next_zone)),
    ) else {
        return (entity_pos, 0);
    };

    let (cross_x, cross_z);
    if path.x2 == entry.x1 || entry.x2 == path.x1 {
        let shared_x = if path.x2 == entry.x1 {
            entry.x1
        } else {
            path.x1
        };
        let lo = u16::max(entry.z1 as u16, path.z1 as u16);
        let hi = u16::min(entry.z2 as u16, path.z2 as u16);
        cross_x = shared_x;
        cross_z = clamp_corridor(room, entity_pos[2], shared_x as u16, lo, hi, radius);
    } else {
        let shared_z = if path.z2 == entry.z1 {
            entry.z1
        } else {
            path.z1
        };
        let lo = u16::max(entry.x1 as u16, path.x1 as u16);
        let hi = u16::min(entry.x2 as u16, path.x2 as u16);
        cross_x = clamp_corridor(
            room,
            entity_pos[0],
            (shared_z as u16) | 0x8000,
            lo,
            hi,
            radius,
        );
        cross_z = shared_z;
    }

    let waypoint = [i32::from(cross_x), entity_pos[1], i32::from(cross_z)];
    let heading = sfx::angle_between_xz(entity_pos[0], entity_pos[2], waypoint[0], waypoint[2]);
    (waypoint, heading)
}

/// The `.y` component of the 3D cross product of two XZ vectors, the sign
/// test `room_check_sight_blocked` runs its segment-diagonal crossings with.
fn cross_y(a: [i32; 2], b: [i32; 2]) -> i32 {
    a[1].wrapping_mul(b[0])
        .wrapping_sub(b[1].wrapping_mul(a[0]))
}

/// One segment-vs-segment test from `room_check_sight_blocked`: does the ray
/// `ent -> ent + delta` cross the box diagonal `a -> b`? Both halves are the
/// standard 2D straddle test on the sign of the cross product. The
/// `normalize_a` half shrinks `a - ent` through `VectorNormal` in the original;
/// scaling cannot change the cross product's sign, so the port keeps the raw
/// vector.
fn ray_crosses_diagonal(
    ent: [i32; 2],
    delta: [i32; 2],
    a: [i32; 2],
    b: [i32; 2],
    normalize_a: bool,
) -> bool {
    let _ = normalize_a;
    let diag = [b[0] - a[0], b[1] - a[1]];
    let p1 = [ent[0] + delta[0] - a[0], ent[1] + delta[1] - a[1]];
    let p0 = [ent[0] - a[0], ent[1] - a[1]];
    if (cross_y(diag, p1) ^ cross_y(diag, p0)) >= 0 {
        return false;
    }

    let p1 = [b[0] - ent[0], b[1] - ent[1]];
    let p0 = [a[0] - ent[0], a[1] - ent[1]];
    (cross_y(delta, p1) ^ cross_y(delta, p0)) < 0
}

/// `room_check_sight_blocked`: does the straight line from the entity to the
/// entity plus `delta` cross a sight-blocking boundary record of quadrant
/// `cell` (0-3)? The per-record test walks the box's two diagonals; only
/// fully-blocking records (`flags & 0x300 == 0x300`) occlude, except for Yawn
/// (ids 13 and 18), for which every record occludes. Coordinates are scaled
/// down by 18 before the cross products to keep them inside 32 bits.
///
/// The original also clobbers each tested record's `flags` down to the two
/// blocking bits; the port cannot write through the shared room, so that
/// destructive side effect is dropped here.
pub(crate) fn room_check_sight_blocked(
    room: &RoomState,
    entity: &Entity,
    ent_pos: [i32; 3],
    delta: [i32; 3],
    cell: u8,
) -> u8 {
    let quadrant = usize::from(cell & 3);
    let records = &room.collision.quadrants[quadrant];

    let ent_x = ent_pos[0] / 18;
    let ent_z = ent_pos[2] / 18;
    let dir_x = delta[0] / 18;
    let dir_z = delta[2] / 18;
    let ent = [ent_x, ent_z];
    let dir = [dir_x, dir_z];

    for rec in records {
        let blocking = rec.flags & 0x300;
        if blocking != 0x300 && entity.id != 13 && entity.id != 18 {
            continue;
        }
        if rec.kind == 4 || rec.kind == 5 {
            continue;
        }
        let x_max = i32::from(rec.x_max / 18);
        let z_max = i32::from(rec.z_max / 18);
        let x_min = i32::from(rec.x_min / 18);
        let z_min = i32::from(rec.z_min / 18);
        if ray_crosses_diagonal(ent, dir, [x_max, z_min], [x_min, z_max], true)
            || ray_crosses_diagonal(ent, dir, [x_min, z_min], [x_max, z_max], false)
        {
            return 1;
        }
    }
    0
}

/// `entity_check_angular_los`: is `target` outside the entity's angular field
/// of view (`fov_half` either side), or is the sight line blocked? Returns 1
/// when the target is not visible. `cell` is the pathfind counter byte the
/// original passes to `room_check_sight_blocked` as the quadrant index; the
/// caller only reaches this with the counter at 0-3.
fn entity_check_angular_los(
    room: &RoomState,
    entity: &Entity,
    fov_half: i16,
    target: [i32; 3],
    cell: u8,
) -> u8 {
    let target_angle = sfx::angle_between_xz(entity.pos[0], entity.pos[2], target[0], target[2]);
    let delta = (fov_half as u16)
        .wrapping_sub(entity.angle)
        .wrapping_add(target_angle)
        & 0x0FFF;
    if i32::from(fov_half) * 2 < i32::from(delta) {
        return 1;
    }

    let dir = [target[0] - entity.pos[0], 0, target[2] - entity.pos[2]];
    room_check_sight_blocked(room, entity, entity.pos, dir, cell)
}

/// `entity_pathfind_update`: the obstacle pathfinder state machine the state-9
/// driver runs before the behaviour. `pathfind_state` is a 5-bit counter plus
/// the line-of-sight bit at 5; every advance increments the whole byte, so the
/// LOS bit rides along until it is explicitly cleared on the counter-3 frame.
/// When the counter reaches 3 with a clear sight line, the player position is
/// latched into `player_pos_x`/`player_pos_z` as the movement waypoint.
///
/// Returns 0 (blocked at the waypoint frame), 1 (waypoint refreshed) or
/// 2 (still counting); the state-9 driver ignores the value, exactly like the
/// original.
pub fn entity_pathfind_update(room: &RoomState, entity: &mut Entity, player_pos: [i32; 3]) -> u8 {
    let val = entity.pathfind_state;
    let counter = val & 0x1F;
    if counter > 3 {
        entity.pathfind_state = entity.pathfind_state.wrapping_add(1);
        if entity.pathfind_state & 0x1F > 0x0F {
            entity.pathfind_state &= 0xC0;
        }
        return 2;
    }

    let result = entity_check_angular_los(room, entity, 1512, player_pos, counter);
    entity.pathfind_state = (result << 5) | val;

    let val = entity.pathfind_state;
    let counter = val & 0x1F;
    if counter == 3 {
        if val & 0x20 == 0 {
            entity.player_pos_x = player_pos[0] as i16;
            entity.player_pos_z = player_pos[2] as i16;
            entity.pathfind_state = entity.pathfind_state.wrapping_add(1) & !0x20;
            return 1;
        }
        entity.pathfind_state = entity.pathfind_state.wrapping_add(1) & !0x20;
        return 0;
    }

    entity.pathfind_state = entity.pathfind_state.wrapping_add(1);
    2
}

/// `npc_walk_choose_heading`: run [`zone_path_find`] from the character to the
/// player and pick the walk heading and waypoint.
///
/// A direct path heads straight at the player. A crossing is extrapolated from
/// the shared edge onto the character-player line and accepted when the
/// corridor test passes and the point is clear of walls; otherwise
/// [`crossing_heading`] clamps the character's own position into the corridor.
/// The waypoint lands in `player_pos_x`/`player_pos_z`, the path result in
/// `bob_speed` and the heading in `reaction_timer`; the heading is returned.
fn choose_heading(entity: &mut Entity, room: &RoomState, player_pos: [i32; 3]) -> i16 {
    let entity_pos = entity.pos;
    let radius = i32::from(entity.sca_radius);
    let heading = match zone_path_find(room, entity_pos, player_pos) {
        ZonePath::Direct { zone } => {
            entity.bob_speed = zone | 0x10;
            entity.player_pos_x = player_pos[0] as i16;
            entity.player_pos_z = player_pos[2] as i16;
            sfx::angle_between_xz(entity_pos[0], entity_pos[2], player_pos[0], player_pos[2])
        }
        ZonePath::Cross {
            from,
            next,
            crossing,
        } => {
            entity.bob_speed = next;
            let (edge_flag, _) = walk_zone_shared_edge(room, from, next);
            let mut pos_x = 0;
            let mut pos_z = 0;
            let mut degenerate = false;
            if edge_flag == 0 {
                let denominator = player_pos[0] - entity_pos[0];
                if denominator == 0 {
                    degenerate = true;
                } else {
                    pos_x = crossing[0];
                    pos_z = entity_pos[2]
                        + (crossing[0] - entity_pos[0]) * (player_pos[2] - entity_pos[2])
                            / denominator;
                }
            } else {
                let denominator = player_pos[2] - entity_pos[2];
                if denominator == 0 {
                    degenerate = true;
                } else {
                    pos_z = crossing[1];
                    pos_x = entity_pos[0]
                        + (crossing[1] - entity_pos[2]) * (player_pos[0] - entity_pos[0])
                            / denominator;
                }
            }

            if !degenerate
                && corridor_open(room, edge_flag, pos_x, pos_z, from, next, radius)
                && !player::position_blocked(
                    room,
                    [pos_x, entity_pos[1], pos_z],
                    radius,
                    entity.collision_flags,
                )
            {
                entity.player_pos_x = pos_x as i16;
                entity.player_pos_z = pos_z as i16;
                sfx::angle_between_xz(entity_pos[0], entity_pos[2], pos_x, pos_z)
            } else {
                let (waypoint, heading) =
                    crossing_heading(room, entity_pos, Some(from), next, radius);
                entity.player_pos_x = waypoint[0] as i16;
                entity.player_pos_z = waypoint[2] as i16;
                heading
            }
        }
        ZonePath::Unreachable => {
            entity.bob_speed = 0xFF;
            entity.player_pos_x = player_pos[0] as i16;
            entity.player_pos_z = player_pos[2] as i16;
            sfx::angle_between_xz(entity_pos[0], entity_pos[2], player_pos[0], player_pos[2])
        }
    };
    entity.reaction_timer = heading as i16;
    heading as i16
}

/// The unsigned angular window test the follow behaviours use: the heading is
/// within `half` of the entity's yaw, boundaries included. The original
/// computes `(unsigned)(heading - angle + half) < half * 2 + 1` in int
/// arithmetic, so the exact `delta == half * 2` boundary passes: behaviour 1
/// and behaviour 2's second test use `< 0x301` / `<= 0x400` for `half` 0x180 /
/// 0x200, and behaviour 3 uses `< 0x401`.
pub(crate) fn heading_within(angle: u16, heading: u16, half: i32) -> bool {
    let delta = i32::from(heading) - i32::from(angle) + half;
    (0..=half * 2).contains(&delta)
}

/// `npc_walk_turn_toward_heading`: step the yaw toward the heading stored in
/// `reaction_timer`. Odd entity ids turn eight units faster. While the angular
/// difference is larger than the step the yaw moves by it (sign chosen by bit
/// 11 of the difference), otherwise it snaps to the heading.
pub(crate) fn turn_toward_heading(entity: &mut Entity, heading: i16, step: i16) {
    let step = step.wrapping_add((entity.id & 1) as i16 * 8);
    let delta = i32::from(heading) - i32::from(entity.angle);
    if ((i32::from(step) * 2) as u32) < (i32::from(step) + delta) as u32 {
        let step = if delta & 0x800 != 0 { -step } else { step };
        entity.angle = entity.angle.wrapping_add(step as u16) & 0x0FFF;
        return;
    }
    entity.angle = heading as u16 & 0x0FFF;
}

/// `npc_walk_reset_lookat`: look at nothing. Reseed the wander countdown from
/// the frame's random seed, clear the wander state and park the look-at
/// fields.
pub(crate) fn reset_lookat(entity: &mut Entity, seed: u16) {
    entity.seq_counter = (((u32::from(seed) & 0x18) + 0x30) >> 1) as u8;
    entity.angle_turn_delta = 0;
    entity.move_timer = 0;
    entity.is_moving = 0;
    entity.move_max_steps = 0;
    entity.look_at_flags = 0x10;
    entity.target = [0, 0, 0];
    entity.look_at_yaw_step = 0xC0;
    entity.look_at_pitch_step = 0x40;
}

/// `npc_walk_set_lookat_target`: look at `(x, z)` with the same reseed.
pub(crate) fn set_lookat_target(entity: &mut Entity, x: i32, z: i32, seed: u16) {
    entity.seq_counter = (((u32::from(seed) & 0x18) + 0x30) >> 1) as u8;
    entity.angle_turn_delta = 0;
    entity.move_timer = 0;
    entity.is_moving = 0;
    entity.move_max_steps = 0;
    entity.look_at_flags = 0x11;
    entity.target = [x, 0, z];
    entity.look_at_yaw_step = 0xC0;
    entity.look_at_pitch_step = 0x40;
}

/// The random look-at wander: force the absolute-angle mode, count the reseed
/// down and, when it expires, repaint the look-at angles from the frame's
/// seed. The draw mask depends on the caller's `param` (the behaviours pass
/// the id parity) and the new angle is forced to differ from the previous one.
pub(crate) fn wander_lookat(entity: &mut Entity, param: u8, seed: u16) {
    entity.look_at_flags = 0x33;
    let mask = (!(param << 2) & 4) + 3;

    entity.seq_counter = entity.seq_counter.wrapping_sub(1);
    if entity.seq_counter != 0 {
        return;
    }
    entity.seq_counter = ((u32::from(seed) & 0x18) + 0x30) as u8;

    let mut b4 = (seed & u16::from(mask)) as u8;
    let mut b5 = ((seed >> 4) & 7) as u8;
    let mut b2 = (seed >> 8) as u8;
    let mut pitch_state = b2 & 0x18;
    b2 = (b2 >> 4) & 0x18;

    if b4 == entity.angle_turn_delta || ((b4 | entity.angle_turn_delta) & 3) == 0 {
        b4 = b4.wrapping_add(1) & mask;
    }
    entity.angle_turn_delta = b4;
    let mut yaw = i32::from(b4 & 3) * 0x60;
    if b4 & 4 == 0 {
        yaw = -yaw;
    }
    entity.target[1] = yaw & 0xFFF;

    if b5 == entity.move_timer || ((b5 | entity.move_timer) & 3) == 0 {
        b5 = b5.wrapping_add(1) & 7;
    }
    entity.move_timer = b5;
    let mut pitch = i32::from(b5 & 3) * 0xA0;
    if b5 & 4 != 0 {
        pitch = -pitch;
    }
    entity.target[0] = pitch & 0xFFF;

    if entity.is_moving == pitch_state {
        pitch_state = (pitch_state + 8) & 0x18;
    }
    entity.is_moving = pitch_state;
    entity.look_at_pitch_step = pitch_state.wrapping_add(0x14);

    if entity.move_max_steps == b2 {
        b2 = b2.wrapping_add(8);
    }
    entity.move_max_steps = b2;
    entity.look_at_yaw_step = b2.wrapping_add(0x28);
}

/// The follow behaviour distance ring: `(near, far)` thresholds per behaviour.
/// The near test swaps when the player is closer than the first value, the far
/// test when the player is at least the second; a zero threshold is disabled.
const NEAR_THRESHOLDS: [i32; 4] = [0x0708, 0x0AF0, 0x1194, 0];
const FAR_THRESHOLDS: [i32; 4] = [0x1194, 0x1964, 0, 0x09C4];
const NEAR_SWAP: [u8; 4] = [3, 0, 1, 3];
const FAR_SWAP: [u8; 4] = [1, 2, 2, 0];

/// `FUN_00471e90`: swap the follow behaviour when the player crosses a
/// distance threshold. The 16-bit store the original performs clears
/// `action_state` along with `action_behavior`.
fn swap_behavior(entity: &mut Entity, player_pos: [i32; 3]) {
    let dx = i64::from(entity.pos[0] - player_pos[0]);
    let dz = i64::from(entity.pos[2] - player_pos[2]);
    let dist = ((dx * dx + dz * dz) as f64).sqrt() as i32;
    let behavior = usize::from(entity.action_behavior);
    if behavior >= 4 {
        return;
    }
    if NEAR_THRESHOLDS[behavior] != 0 && dist < NEAR_THRESHOLDS[behavior] {
        entity.action_behavior = NEAR_SWAP[behavior];
        entity.action_state = 0;
        return;
    }
    if FAR_THRESHOLDS[behavior] != 0 && dist >= FAR_THRESHOLDS[behavior] {
        entity.action_behavior = FAR_SWAP[behavior];
        entity.action_state = 0;
    }
}

/// Behaviour 0: pace in place. A random wait countdown (`tex_bank`), then a
/// short walk on animation 5 until the animation reports done, then animation
/// 6 while the wander look-at runs.
fn behavior_00(entity: &mut Entity, seed: u16) {
    match entity.action_state {
        0 => {
            entity.action_state = 1;
            entity.animation_id = 0;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.blend_counter = 7;
            entity.tex_bank = ((seed & 0x38) + 0x40) as u8;
            reset_lookat(entity, seed);
        }
        1 => {
            entity.tex_bank = entity.tex_bank.wrapping_sub(1);
            if entity.tex_bank != 0 {
                wander_lookat(entity, 0, seed);
                return;
            }
            entity.action_state = 2;
            entity.animation_id = 5;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.blend_counter = 7;
            reset_lookat(entity, seed);
            behavior_00_walk(entity, seed);
        }
        2 => behavior_00_walk(entity, seed),
        3 => wander_lookat(entity, !(entity.id & 1), seed),
        _ => {}
    }
}

/// Behaviour 0's walk state: switch to animation 6 and the parity wander once
/// the short walk's animation completes.
fn behavior_00_walk(entity: &mut Entity, seed: u16) {
    if entity.attacking_direction & 1 != 0 {
        entity.action_state = 3;
        entity.animation_id = 6;
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.blend_counter = 7;
        wander_lookat(entity, !(entity.id & 1), seed);
    }
}

/// Behaviour 1: walk to the player on animation 7 at speed 0x5D. While the
/// heading is within ±0x180 of the yaw the character walks; otherwise it only
/// turns, at half the step.
fn behavior_01(entity: &mut Entity, room: &RoomState, player_pos: [i32; 3], seed: u16) {
    let heading = choose_heading(entity, room, player_pos);
    if entity.action_state == 0 {
        entity.action_state = 1;
        if entity.animation_id != 7 {
            entity.animation_id = 7;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.blend_counter = 7;
        }
    }

    if heading_within(entity.angle, heading as u16, 0x180) {
        turn_toward_heading(entity, heading, 0x30);
        entity_apply_walk_speed(entity, 0x5D);
        advance_xz(entity, 0, entity.move_speed_current as i16);
        wander_lookat(entity, 0, seed);
        return;
    }
    turn_toward_heading(entity, heading, 0x28);
    wander_lookat(entity, 0, seed);
}

/// Behaviour 2: fast walk to the player. On animation 8 at speed 0xD2 while
/// the heading is within ±0x180; when it is far off the character falls back
/// to animation 7, and a second ±0x200 test skips the movement entirely.
fn behavior_02(entity: &mut Entity, room: &RoomState, player_pos: [i32; 3], seed: u16) {
    let heading = choose_heading(entity, room, player_pos);
    let move_speed = if heading_within(entity.angle, heading as u16, 0x180) {
        if entity.animation_id != 8 {
            entity.animation_id = 8;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.blend_counter = 7;
        }
        turn_toward_heading(entity, heading, 0x60);
        0xD2
    } else {
        if entity.animation_id != 7 {
            entity.animation_id = 7;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.blend_counter = 7;
        }
        turn_toward_heading(entity, heading, 0x30);
        if !heading_within(entity.angle, heading as u16, 0x200) {
            set_lookat_target(
                entity,
                i32::from(entity.player_pos_x),
                i32::from(entity.player_pos_z),
                seed,
            );
            return;
        }
        entity_apply_walk_speed(entity, 0x5D);
        entity.move_speed_current as i16
    };
    advance_xz(entity, 0, move_speed);
    set_lookat_target(
        entity,
        i32::from(entity.player_pos_x),
        i32::from(entity.player_pos_z),
        seed,
    );
}

/// Behaviour 3: face the player and keep the distance. While the player is
/// within ±0x200 of straight ahead the character walks backward on animation 3
/// at -0x3C with the look-at locked on the player; otherwise it turns 180
/// degrees and walks forward on animation 7 with the look-at reset.
fn behavior_03(entity: &mut Entity, _room: &RoomState, player_pos: [i32; 3], seed: u16) {
    let mut heading =
        sfx::angle_between_xz(entity.pos[0], entity.pos[2], player_pos[0], player_pos[2]);
    entity.reaction_timer = heading as i16;
    if heading_within(entity.angle, heading, 0x200) {
        if entity.animation_id != 3 {
            entity.animation_id = 3;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.blend_counter = 7;
        }
        turn_toward_heading(entity, heading as i16, 0x30);
        advance_xz(entity, 0, -0x3C);
        set_lookat_target(entity, player_pos[0], player_pos[2], seed);
        return;
    }

    if entity.animation_id != 7 {
        entity.animation_id = 7;
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.blend_counter = 7;
    }
    heading = heading.wrapping_add(0x800) & 0x0FFF;
    entity.reaction_timer = heading as i16;
    turn_toward_heading(entity, heading as i16, 0x30);
    entity_apply_walk_speed(entity, 0x5D);
    advance_xz(entity, 0, entity.move_speed_current as i16);
    reset_lookat(entity, seed);
}

/// `npc_walk_footstep_sound`: the state-9 footfall frames. Animations 3 and 7
/// step on frames 8 and 0x16 with sound 0; animation 8 on frames 0 and 0xA
/// with sound 1.
fn walk_footstep_sound(
    sounds: &mut Vec<EntitySound>,
    room: &RoomState,
    entity: &Entity,
    slow: bool,
) {
    let sound_type = match (entity.animation_id, entity.animation_frame_id) {
        (3 | 7, 8 | 0x16) => 0,
        (8, 0 | 0x0A) => 1,
        _ => return,
    };
    footstep(sounds, room, entity, sound_type, slow);
}

/// One entity's SCA collision volume: the entity-local centre the original's
/// records carry, plus its half-height and XZ radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaHit {
    /// Entity-local cylinder centre; Y is the height offset.
    pub offset: [i16; 3],
    /// Cylinder half-height.
    pub half_height: i16,
    /// Cylinder radius.
    pub radius: i16,
    /// World-space bias applied after the local offset is rotated: the port's
    /// model of a script's per-frame `pSca_hit_data` retarget, which replaces
    /// the volume's rotated world centre with a posed joint's offset. Zero
    /// for every entity whose record is not overridden.
    pub world_bias: [i16; 3],
}

impl ScaHit {
    /// The player's single volume: centred one body-height below the origin,
    /// the record `check_room_collision` reads the radius from.
    pub fn player(radius: i32) -> Self {
        let height = crate::objects::player_height(radius);
        ScaHit {
            offset: [0, -height, 0],
            half_height: height,
            radius: radius as i16,
            world_bias: [0; 3],
        }
    }

    /// A character's record volume. The radius always comes from the live
    /// `sca_radius`; an id with no SCA record (the synthetic fixtures) falls
    /// back to the standing body so the Y test still overlaps.
    pub fn character(id: u8, radius: i16) -> Self {
        let standing = ScaHit {
            offset: [0, -0x5FA, 0],
            half_height: 0x5FA,
            radius,
            world_bias: [0; 3],
        };
        match data::sca_volume(id) {
            Some(volume) => ScaHit {
                offset: volume.offset,
                half_height: volume.half_height,
                radius,
                world_bias: [0; 3],
            },
            None => standing,
        }
    }

    /// The live record volume of any entity: characters resolve through the
    /// per-character data table (their init stores the same values on the
    /// entity), while monsters and other ids read the fields the script's
    /// `e:set_sca` wrote. The radius always comes from the live
    /// `sca_radius`.
    pub fn of(entity: &Entity) -> Self {
        match data::sca_volume(entity.id) {
            Some(volume) => ScaHit {
                offset: volume.offset,
                half_height: volume.half_height,
                radius: entity.sca_radius,
                world_bias: [0; 3],
            },
            None => ScaHit {
                offset: entity.sca_offset,
                half_height: entity.sca_half_height,
                radius: entity.sca_radius,
                world_bias: entity.sca_hit_delta,
            },
        }
    }

    /// `SetEntityScaHitData`: the world-space volume centre, the local offset
    /// rotated by the entity's yaw, then the script's per-frame world bias.
    pub(crate) fn world_offset(&self, angle: u16) -> [i32; 3] {
        let (x, z) = player::rotate_xz(angle, i32::from(self.offset[0]), i32::from(self.offset[2]));
        [
            x + i32::from(self.world_bias[0]),
            i32::from(self.offset[1]) + i32::from(self.world_bias[1]),
            z + i32::from(self.world_bias[2]),
        ]
    }
}

/// The snapshot of another entity's separation inputs: position, angle, its
/// SCA volume list and status flags.
type SeparationTarget = ([i32; 3], u16, ([ScaHit; 2], usize), u8);

/// The entity's SCA volume list in the original's record order: the primary
/// record first, then the optional second profile volume
/// ([`crate::game::Entity::sca2`]). Returns the volumes and the live count.
pub fn entity_sca_volumes(entity: &Entity) -> ([ScaHit; 2], usize) {
    let first = ScaHit::of(entity);
    match entity.sca2 {
        Some(volume) => (
            [
                first,
                ScaHit {
                    offset: volume.offset,
                    half_height: volume.half_height,
                    radius: volume.radius,
                    world_bias: [0; 3],
                },
            ],
            2,
        ),
        None => ([first, first], 1),
    }
}

/// `ResolveEntityScaCollision` against the player: push the character out of
/// the player's SCA volume. The player is never moved and no damage is
/// transferred. `prev_pos` is the character's stored `position` word (+0x6C),
/// the last room-collision-accepted position, which breaks a degenerate
/// overlap when the character ran through the other volume in one frame.
pub fn separate_from_player(
    entity: &mut Entity,
    prev_pos: [i32; 3],
    player_pos: [i32; 3],
    player_angle: u16,
    player_radius: i32,
    player_status: u8,
) -> bool {
    resolve_sca_collision(
        entity,
        prev_pos,
        player_pos,
        player_angle,
        &[ScaHit::player(player_radius)],
        player_status,
    )
}

/// `ResolveEntityScaCollision` against another active character, the
/// `HandleEnemyPlayerCollisions` pass the original's state-9 tail runs after
/// the player pair: the character is pushed out of the other's volumes and the
/// other is never moved.
pub fn separate_from_character(
    entity: &mut Entity,
    prev_pos: [i32; 3],
    other_pos: [i32; 3],
    other_angle: u16,
    other: &[ScaHit],
    other_status: u8,
) -> bool {
    resolve_sca_collision(
        entity,
        prev_pos,
        other_pos,
        other_angle,
        other,
        other_status,
    )
}

/// The whole state-9 SCA separation pass for one slot: the player pair first,
/// then every other entity with any status bit set, in ascending slot order.
/// Returns the player pair's hit flag, the value the monster drivers park as
/// their "touching the player" word.
///
/// This mirrors the snapshot and loop in [`update`]; collecting the others at
/// call time is equivalent because the driver only mutates its own slot, so no
/// other entity changes between the driver's entry and its separation tail.
pub fn separate_all(game: &mut GameState, slot: usize) -> bool {
    let player_pos = game.entities[0].pos;
    let player_angle = game.entities[0].angle;
    let player_status = game.entities[0].status_flags;
    let player_radius = player_radius(game.id.player_flag);
    let others: Vec<SeparationTarget> = game
        .entities
        .iter()
        .enumerate()
        .filter(|(index, other)| *index != 0 && *index != slot && other.status_flags != 0)
        .map(|(_, other)| {
            (
                other.pos,
                other.angle,
                entity_sca_volumes(other),
                other.status_flags,
            )
        })
        .collect();
    let entity = &mut game.entities[slot];
    let prev_pos = entity.saved_pos.unwrap_or(entity.pos);
    let touched = separate_from_player(
        entity,
        prev_pos,
        player_pos,
        player_angle,
        player_radius,
        player_status,
    );
    for (other_pos, other_angle, (other, other_count), other_status) in others {
        separate_from_character(
            entity,
            prev_pos,
            other_pos,
            other_angle,
            &other[..other_count],
            other_status,
        );
    }
    touched
}

/// The original's `SquareRoot0`: integer square root, zero for non-positive.
fn integer_sqrt(value: i32) -> i32 {
    if value <= 0 {
        0
    } else {
        (f64::from(value)).sqrt() as i32
    }
}

/// The shared volume-list resolve. `entity` is the original's `entB` (the one
/// pushed) and the other entity is `entA`; the original's guards are
/// `entB->state == 4` (the eating/headless state) and status bit 1 on either
/// side (the deactivation bit, e.g. the lab power-room Wesker).
///
/// Both volume lists are walked as nested loops, the other entity's record
/// outer and the pushed entity's inner, exactly like the original: every
/// volume pair that overlaps contributes its push to `entity` in order.
///
/// The penetration test is the volume model: XZ distance between the two
/// rotated volume centres against the summed radii, then the vertical centres
/// against the summed half-heights. A pair that only overlaps in Y because the
/// character moved across the other this frame is corrected from the pre-move
/// `position`, exactly like the original's degenerate-overlap branch. Returns
/// the pair hit flag.
fn resolve_sca_collision(
    entity: &mut Entity,
    prev_pos: [i32; 3],
    other_pos: [i32; 3],
    other_angle: u16,
    other: &[ScaHit],
    other_status: u8,
) -> bool {
    if entity.state() == 4 || (entity.status_flags | other_status) & 2 != 0 {
        return false;
    }

    let (own, own_count) = entity_sca_volumes(entity);
    let mut hit = false;
    for a_volume in other {
        let a = a_volume.world_offset(other_angle);
        for b_volume in &own[..own_count] {
            let b = b_volume.world_offset(entity.angle);
            let dx = (b[0] - a[0]) - other_pos[0] + entity.pos[0];
            let dz = (b[2] - a[2]) - other_pos[2] + entity.pos[2];
            let dist = integer_sqrt(dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz)));
            let penetration = i32::from(a_volume.radius) + i32::from(b_volume.radius) - (dist + 1);
            if penetration <= 0 {
                continue;
            }

            let dy = b[1] + (entity.pos[1] - a[1]) - other_pos[1];
            let max_height = i32::from(a_volume.half_height) + i32::from(b_volume.half_height);
            if -max_height >= dy || dy >= max_height {
                continue;
            }

            let mut push_x = penetration.wrapping_mul(dx) / (dist + 1);
            let mut push_z = penetration.wrapping_mul(dz) / (dist + 1);

            // The pre-move Y says whether the pair already overlapped
            // vertically before this frame's movement; when it did not, a
            // horizontal crossing (the other centre now strictly between the
            // old and new own centre) is a tunnelling overlap and the push is
            // rewritten to the other side.
            let dy2 = b[1] + (prev_pos[1] - a[1]) - other_pos[1];
            if dy2 <= -max_height || max_height <= dy2 {
                let other_radius = i32::from(a_volume.radius);
                let pos_xa = other_pos[0];
                if (prev_pos[0] < pos_xa && pos_xa < entity.pos[0])
                    || (pos_xa < prev_pos[0] && entity.pos[0] < pos_xa)
                {
                    if -push_x < 1 {
                        push_x = -(-push_x + other_radius * 2);
                    } else {
                        push_x += other_radius * 2;
                    }
                }
                let pos_za = other_pos[2];
                if (prev_pos[2] < pos_za && pos_za < entity.pos[2])
                    || (pos_za < prev_pos[2] && entity.pos[2] < pos_za)
                {
                    if -push_z < 1 {
                        push_z = -(-push_z + other_radius * 2);
                    } else {
                        push_z += other_radius * 2;
                    }
                }
            }

            entity.pos[0] += push_x;
            entity.pos[2] += push_z;
            hit = true;
        }
    }
    hit
}

/// The player's collision radius for the room's character flag.
pub(crate) fn player_radius(player_flag: u8) -> i32 {
    if player_flag & 1 == 0 {
        player::CHRIS_RADIUS
    } else {
        player::JILL_RADIUS
    }
}

/// One state-9 tick: the follow/pathfind driver.
///
/// The first tick marks the character as ignoring the player and resets the
/// look-at. Every tick runs the obstacle pathfinder, swaps the behaviour on
/// the distance ring, records the character's zone, runs the selected
/// behaviour and advances the animation with the blend step
/// `0x1000 / (blend_counter + 1)` and the reverse bit from `dir_control_flags`.
/// The tail is the original's collision order: queue the footfall sound, run
/// the SCA volume pairs (the player first, then every other active character),
/// then resolve the result against the room collision with the last accepted
/// position as the rollback point.
pub fn update(game: &mut GameState, slot: usize, room: &RoomState, clips: &[Clip]) {
    let player_pos = game.entities[0].pos;
    let player_angle = game.entities[0].angle;
    let player_status = game.entities[0].status_flags;
    let player_radius = player_radius(game.id.player_flag);
    let seed = game.rand_seed;
    let slow = game.flags[5].bit(crate::game::MSF2_EFFECT_ZONE);

    let GameState {
        entities,
        entity_anims,
        entity_sounds,
        ..
    } = game;
    // The original's `HandleEnemyPlayerCollisions` scans the enemy list after
    // the player pair; collect the other active characters' positions, angles,
    // volumes and status flags before borrowing this slot mutably.
    let others: Vec<SeparationTarget> = entities
        .iter()
        .enumerate()
        .filter(|(index, other)| *index != 0 && *index != slot && other.status_flags != 0)
        .map(|(_, other)| {
            (
                other.pos,
                other.angle,
                entity_sca_volumes(other),
                other.status_flags,
            )
        })
        .collect();
    let entity = &mut entities[slot];
    let clock = &mut entity_anims[slot];

    if entity.ignore() == 0 {
        entity.set_ignore(1);
        reset_lookat(entity, seed);
    }

    // The original's `position` word (+0x6C) is the last position its
    // collision pass accepted; the object pass can shove the live matrix after
    // the state-9 tail without rewriting it, so the tail must read the stored
    // word, not the current `pos`.
    let prev_pos = entity.saved_pos.unwrap_or(entity.pos);
    entity_pathfind_update(room, entity, player_pos);
    swap_behavior(entity, player_pos);
    entity.splatter_flag = walk_zone_find(room, entity.pos[0], entity.pos[2]).unwrap_or(0xFF);
    match entity.action_behavior {
        0 => behavior_00(entity, seed),
        1 => behavior_01(entity, room, player_pos, seed),
        2 => behavior_02(entity, room, player_pos, seed),
        3 => behavior_03(entity, room, player_pos, seed),
        _ => {}
    }

    let reverse = entity.dir_control_flags & 1 != 0;
    let blend_step = crate::enemy::anim::blend_step(entity);
    let done = clock.advance(entity, clips, reverse, blend_step);
    entity.attacking_direction = u8::from(done);
    walk_footstep_sound(entity_sounds, room, entity, slow);

    separate_from_player(
        entity,
        prev_pos,
        player_pos,
        player_angle,
        player_radius,
        player_status,
    );
    for (other_pos, other_angle, (other, other_count), other_status) in others {
        separate_from_character(
            entity,
            prev_pos,
            other_pos,
            other_angle,
            &other[..other_count],
            other_status,
        );
    }
    // check_room_collision: push out of walls, or roll X/Z back to the
    // accepted position when the pushed result is still stuck. Accepting the
    // result advances the stored `position` word, like the original's
    // `collision_accept`.
    entity.pos = player::resolve_collision(
        &room.collision,
        prev_pos,
        entity.pos,
        i32::from(entity.sca_radius),
        entity.collision_flags,
    );
    entity.saved_pos = Some(entity.pos);
}

/// `checkAngularViewAndDistance`: is `target` inside the entity's angular
/// wedge (`fov_half` either side) and within `max_distance`? Returns 1 when
/// both hold. The range test folds the sign correction into the Manhattan
/// sum; the wedge test builds the entity's left and right edge vectors by
/// composing two yaw rotations (so the second `RotMatrixY` compounds onto the
/// first) and compares the sign bits of the two edge-to-target cross products.
pub fn angular_view_and_distance(
    entity: &Entity,
    fov_half: i16,
    max_distance: i16,
    target: [i32; 3],
) -> u8 {
    let dx = target[0].wrapping_sub(entity.pos[0]);
    let dz = target[2].wrapping_sub(entity.pos[2]);
    let abs_dx = (dx ^ (dx >> 31)).wrapping_sub(dx >> 31);
    let abs_dz = (dz ^ (dz >> 31)).wrapping_sub(dz >> 31);
    if i32::from(max_distance as u16) < abs_dx.wrapping_sub(dx >> 31).wrapping_add(abs_dz) {
        return 0;
    }

    let yaw = |angle: i32| crate::anim::Mat4x3 {
        r: crate::anim::rotation_matrix(0, angle, 0),
        t: [0; 3],
    };
    let forward = [2000, 0, 0];
    let mut matrix = yaw(i32::from(entity.angle) - i32::from(fov_half));
    let left = apply_matrix_lv(&matrix, forward);
    matrix = compose(&yaw(i32::from(fov_half) * 2), &matrix);
    let right = apply_matrix_lv(&matrix, forward);
    let dir = [dx, 0, dz];

    let left_y = left[2]
        .wrapping_mul(dir[0])
        .wrapping_sub(dir[2].wrapping_mul(left[0])) as u32
        & 0x8000_0000;
    let right_y = right[2]
        .wrapping_mul(dir[0])
        .wrapping_sub(dir[2].wrapping_mul(right[0])) as u32
        & 0x8000_0000;
    u8::from(left_y < right_y)
}

/// `ApplyMatrixLV`: rotate `v` by the matrix's 4.12 rotation with the game's
/// Y-sign conjugation. The translation is not read.
fn apply_matrix_lv(matrix: &crate::anim::Mat4x3, v: [i32; 3]) -> [i32; 3] {
    compose(
        matrix,
        &crate::anim::Mat4x3 {
            r: [[0; 3]; 3],
            t: v,
        },
    )
    .t
}

/// `check_line_of_sight`: 0 when the entity's sight line to `target` is clear,
/// 1 when a fully-blocking boundary record crosses it. The quadrant comes from
/// the target's own boundary cell (`ChkOutsideCell` with the zero offset).
pub fn check_line_of_sight(room: &RoomState, entity: &Entity, target: [i32; 3]) -> u8 {
    let cell = player::quadrant(&room.collision, target[0], target[2]);
    let delta = [
        target[0].wrapping_sub(entity.pos[0]),
        target[1].wrapping_sub(entity.pos[1]),
        target[2].wrapping_sub(entity.pos[2]),
    ];
    room_check_sight_blocked(room, entity, entity.pos, delta, cell)
}

/// The field-agnostic core of `entity_update_wander_turn`: the callers pass
/// the entity's own waypoint pair and their two out-parameter fields, because
/// each monster family stores the control byte and the turn counter at a
/// different offset.
#[allow(clippy::too_many_arguments)]
fn wander_turn_core(
    movement_dist: u32,
    angle_step: u16,
    turn_limit: u8,
    rand_seed: u16,
    speed_div: i32,
    angle: &mut u16,
    pos: [i32; 2],
    waypoint: [i16; 2],
    control_flags: &mut u8,
    turn_counter: &mut u8,
) -> u8 {
    if *control_flags & 0x80 != 0 {
        let factor = 1 - i32::from((*control_flags & 0x40) >> 5);
        *angle = angle.wrapping_add((factor * i32::from(angle_step)) as u16);
        let speed_times_three = speed_div.wrapping_mul(3);
        let threshold = ((speed_times_three + ((speed_times_three >> 31) & 3)) >> 2) as u32;
        if threshold < movement_dist {
            let new_count = turn_counter.wrapping_sub(1);
            *turn_counter = new_count;
            if new_count == 0 {
                *control_flags = 0;
                *turn_counter = 0;
            }
        }
        return 1;
    }

    if movement_dist < (speed_div.wrapping_mul(2) / 3) as u32 {
        let new_count = turn_counter.wrapping_add(1);
        *turn_counter = new_count;
        if turn_limit < new_count {
            let flags = *control_flags;
            *control_flags = ((rand_seed & 0x40) as u8) | flags | 0x80;
            *turn_counter = turn_limit / 6;
        }
    } else {
        *control_flags = 0;
        *turn_counter = 0;
    }

    let mut step = angle_step;
    let mut target = sfx::angle_between_xz(
        pos[0],
        pos[1],
        i32::from(waypoint[0]),
        i32::from(waypoint[1]),
    );
    if step & 0x8000 != 0 {
        step = step.wrapping_neg();
        target = (target + 0x800) & 0x0FFF;
    }
    let delta = step.wrapping_sub(*angle).wrapping_add(target) & 0x0FFF;
    if (delta as i32) < i32::from(step.wrapping_mul(2) as i16) {
        *angle = target;
        return 0;
    }
    *angle = angle.wrapping_sub(step);
    if delta < 0x801 {
        *angle = angle.wrapping_add(step.wrapping_mul(2));
    }
    0
}

/// `entity_update_wander_turn`: the stuck-detection steering the zombie walk
/// behaviours run every frame. `movement_dist` is the word at entity +0x17A
/// and `angle_step` the turn magnitude (bit 15 flips the direction); the
/// direction-control flags at +0x16D and the byte counter at +0x179 are the
/// original's two out-parameters, updated in place. Returns 1 while the
/// flags' 0x80 path owns the turn, 0 when steering at the waypoint.
pub fn update_wander_turn(
    entity: &mut Entity,
    movement_dist: u32,
    angle_step: u16,
    turn_limit: u8,
    rand_seed: u16,
) -> u8 {
    let speed_div = i32::from(entity.move_speed_current as i16);
    let mut turn_counter = entity.zombie_unk_179();
    let pos = [entity.pos[0], entity.pos[2]];
    let waypoint = [entity.player_pos_x, entity.player_pos_z];
    let result = wander_turn_core(
        movement_dist,
        angle_step,
        turn_limit,
        rand_seed,
        speed_div,
        &mut entity.angle,
        pos,
        waypoint,
        &mut entity.dir_control_flags,
        &mut turn_counter,
    );
    entity.set_zombie_unk_179(turn_counter);
    result
}

/// The hunter's call of `entity_update_wander_turn`: the same core, but the
/// control byte is `H_ROOM_HIT` at +0x180 and the turn counter the low byte
/// of the path latch at +0x170. The hunter's chase and approach behaviours
/// both pass those fields, so the call is a named helper instead of exposing
/// field pointers.
pub fn hunter_wander_turn(
    entity: &mut Entity,
    movement_dist: u32,
    angle_step: u16,
    turn_limit: u8,
    rand_seed: u16,
) -> u8 {
    let speed_div = i32::from(entity.move_speed_current as i16);
    let mut turn_counter = entity.angle_turn_delta;
    let pos = [entity.pos[0], entity.pos[2]];
    let waypoint = [entity.player_pos_x, entity.player_pos_z];
    let result = wander_turn_core(
        movement_dist,
        angle_step,
        turn_limit,
        rand_seed,
        speed_div,
        &mut entity.angle,
        pos,
        waypoint,
        &mut entity.move_speed_byte,
        &mut turn_counter,
    );
    entity.angle_turn_delta = turn_counter;
    result
}

/// The Tyrant's call of `entity_update_wander_turn`: the same core, with the
/// control byte at +0x16C (`attacking_direction`, the byte the room-collision
/// tail also ORs into) and the turn counter at +0x17E (`behavior_step`). The
/// walk and the SCD walk-to both steer through it.
pub fn tyrant_wander_turn(
    entity: &mut Entity,
    movement_dist: u32,
    angle_step: u16,
    turn_limit: u8,
    rand_seed: u16,
) -> u8 {
    let speed_div = i32::from(entity.move_speed_current as i16);
    let mut turn_counter = entity.behavior_step;
    let pos = [entity.pos[0], entity.pos[2]];
    let waypoint = [entity.player_pos_x, entity.player_pos_z];
    let result = wander_turn_core(
        movement_dist,
        angle_step,
        turn_limit,
        rand_seed,
        speed_div,
        &mut entity.angle,
        pos,
        waypoint,
        &mut entity.attacking_direction,
        &mut turn_counter,
    );
    entity.behavior_step = turn_counter;
    result
}

/// `check_room_collision_two_point` for an entity: rotate the two body-local
/// floor-probe endpoints by the entity yaw, run the original's two-endpoint
/// boundary walk (end B first) and either accept the pushed position or, when
/// end A is still blocked, re-test end B in its previous quadrant and roll the
/// position and angle back. Returns end B's flag bits (0/1/2) on acceptance,
/// `0x80` on the rollback, and 0 for a deactivated entity.
pub fn two_point_probe(
    collision: &crate::state::Collision,
    entity: &mut Entity,
    end_a: [i16; 2],
    end_b: [i16; 2],
) -> u8 {
    if entity.status_flags & 0x04 != 0 {
        return 0;
    }
    let committed = entity.saved_pos.unwrap_or(entity.pos);
    let yaw = entity.angle;
    let mirror = entity.mirror_angle;
    let radius = i32::from(entity.sca_radius);
    let mut pos = entity.pos;
    let split = player::two_point_probe_split(
        collision,
        &mut pos,
        committed,
        yaw,
        mirror,
        [end_a, end_b],
        radius,
    );

    if split.bits_a == 0 {
        entity.pos = pos;
        entity.saved_pos = Some(pos);
        entity.mirror_angle = yaw;
        return split.bits_b as u8;
    }

    let (world_x, world_z) = player::rotate_xz(mirror, i32::from(end_b[0]), i32::from(end_b[1]));
    let world_b = [committed[0] + world_x, committed[1], committed[2] + world_z];
    let extra =
        player::two_point_retest_previous(collision, split.point_b, world_b, split.cell_b, radius);
    if extra != 0 {
        entity.pos = committed;
        entity.angle = mirror;
        return 0x80;
    }
    entity.pos = pos;
    entity.saved_pos = Some(pos);
    entity.mirror_angle = yaw;
    1
}

/// `zone_path_find`'s entity out-parameter form: the zombie's distance driver
/// calls it with the player's position and reads the steered waypoint back out
/// of `player_pos_x`/`player_pos_z`. A direct path stores the target, a
/// crossing stores the shared-edge crossing, and an unreachable target writes
/// nothing.
pub fn zone_path_update(room: &RoomState, entity: &mut Entity, px: i32, pz: i32) {
    match zone_path_find(room, entity.pos, [px, 0, pz]) {
        ZonePath::Direct { .. } => {
            entity.player_pos_x = px as i16;
            entity.player_pos_z = pz as i16;
        }
        ZonePath::Cross { crossing, .. } => {
            entity.player_pos_x = crossing[0] as i16;
            entity.player_pos_z = crossing[1] as i16;
        }
        ZonePath::Unreachable => {}
    }
}

/// `blood_splatter_physics`: the bleeding-joint physics the zombie update runs
/// on the head joint while its part word is set. The joint's scratch block
/// carries the fall frame counter and bounce flags, the X velocity and the Y
/// acceleration; the joint's posed world translation is stepped, probed
/// against the radius-zero boundary query, and a blood billboard plus the
/// impact cue are emitted on each wall or floor hit.
///
/// The falling displacement lives on the joint's blood scratch and is stepped
/// from the posed world translation each tick; the spawned billboards anchor
/// to the joint's posed matrix (the port's joint-attached effect), so the
/// transient falling offset itself is the documented joint-object deferral.
/// The scratch (counter, flags, velocity, acceleration) and the cues are the
/// observable contract.
pub fn blood_splatter(
    game: &mut GameState,
    room: &RoomState,
    slot: usize,
    joint: usize,
    gravity_step: i16,
) {
    let joint = joint & 31;
    let entity = &game.entities[slot];
    let scratch = entity.joint_blood[joint];
    let mut counter = scratch.counter;
    let mut flags = scratch.flags;
    let mut vel_x = scratch.vel_x;
    let mut vel_y = scratch.vel_y;

    let world = game
        .joint_worlds
        .get(slot)
        .and_then(|worlds| worlds.get(joint))
        .map_or([0, 0, 0], |matrix| matrix.t);
    if world[1] >= -100 && flags & 0x1F >= 6 {
        return;
    }

    let (local_x, local_z) =
        player::rotate_xz(entity.angle, i32::from(vel_x), i32::from(scratch.rot_z));
    let sign_x = if flags & 0x40 != 0 { -1 } else { 1 };
    let sign_z = if flags & 0x80 != 0 { -1 } else { 1 };

    let saved_x = world[0];
    let saved_z = world[2];
    let mut pos_x = saved_x + sign_x * local_x;
    let mut pos_z = saved_z + sign_z * local_z;
    let mut pos_y = world[1];

    let mut hit = player::point_blocked_flags(&room.collision, [pos_x, pos_y, pos_z], [0; 3]);
    if hit != 0 {
        flags ^= 0x40;
        let sign_x = if flags & 0x40 != 0 { -1 } else { 1 };
        let sign_z = if flags & 0x80 != 0 { -1 } else { 1 };
        pos_x = sign_x * local_x + saved_x;
        pos_z = sign_z * local_z + saved_z;
        hit = player::point_blocked_flags(&room.collision, [pos_x, pos_y, pos_z], [0; 3]);
        if hit != 0 {
            flags ^= 0xC0;
        }
        pos_x = saved_x;
        pos_z = saved_z;
        vel_x >>= 1;
        spawn_blood_billboard(game, slot, joint);
    }

    let accel = i32::from(vel_y) - i32::from(counter) * i32::from(gravity_step);
    vel_y = accel as i16;
    pos_y = pos_y.wrapping_sub(accel);

    if pos_y > -0x65 {
        pos_y = -100;
        counter = 0;
        flags = flags.wrapping_add(1);
        vel_x = vel_x.wrapping_add(0x28);
        vel_y = ((-accel) >> 2) as i16;
        spawn_blood_billboard(game, slot, joint);
        queue_enemy_cue(game, room, slot, 8);
    }

    if vel_x > 0 {
        vel_x = 0;
    }
    counter = counter.wrapping_add(1);

    if let Some(worlds) = game.joint_worlds.get_mut(slot)
        && let Some(matrix) = worlds.get_mut(joint)
    {
        matrix.t = [pos_x, pos_y, pos_z];
    }
    game.entities[slot].joint_blood[joint] = crate::game::JointBlood {
        counter,
        flags,
        vel_x,
        vel_y,
        rot_z: scratch.rot_z,
    };
}

/// The blood billboard at `joint`'s (stepped) world matrix.
fn spawn_blood_billboard(game: &mut GameState, slot: usize, joint: usize) {
    let room_effects = std::rc::Rc::clone(&game.room_effects);
    crate::effects::create_attached(
        game,
        &room_effects,
        0,
        0,
        crate::effects::Attach::Joint(slot as u8, joint as u8),
        [0, 0, 0],
        0,
        0,
    );
}

/// Queue one enemy-bank cue at the entity, the shared `Snd_em` tail.
fn queue_enemy_cue(game: &mut GameState, room: &RoomState, slot: usize, id: u8) {
    let group = (game.entities[slot].variant >> 4) & 0x7;
    if let Some((name, column)) = sfx::enemy_sound(room, id, group) {
        let pos = game.entities[slot].pos;
        game.entity_sounds.push(EntitySound {
            name,
            bank: 2,
            column,
            pos,
        });
    }
}

/// `zombie_body_part_physics`: derive the crawl speed from the leg joint
/// chain. The entity's local matrix (rebuilt from its rotation triple) is
/// composed with the two shared leg transforms and the three transforms of
/// the leg the `param` nibble selects (joints `3 + 3*param ..= 5 + 3*param`),
/// the composed translation is made relative to that leg's end world
/// translation, its Y is dropped, and the XZ magnitude lands in
/// `move_speed_current`.
pub fn body_part_speed(game: &mut GameState, clips: &[Clip], slot: usize, param: u8) {
    let entity = &game.entities[slot];
    let locals = game.entity_anims[slot].local_transforms(entity, clips);
    let mut scratch =
        crate::anim::entity_matrix_rotated(entity.pos, entity.pitch, entity.angle, entity.roll);
    for index in [0usize, 1] {
        if let Some(transform) = locals.get(index) {
            scratch = compose(&scratch, transform);
        }
    }
    let first = 3 + usize::from(param) * 3;
    for index in first..first + 3 {
        if let Some(transform) = locals.get(index) {
            scratch = compose(&scratch, transform);
        }
    }
    let end = first + 2;
    if let Some(world) = game
        .joint_worlds
        .get(slot)
        .and_then(|worlds| worlds.get(end))
    {
        scratch.t[0] = scratch.t[0].wrapping_sub(world.t[0]);
        scratch.t[2] = scratch.t[2].wrapping_sub(world.t[2]);
    }
    scratch.t[1] = 0;
    let magnitude = integer_sqrt(
        scratch.t[2]
            .wrapping_mul(scratch.t[2])
            .wrapping_add(scratch.t[0].wrapping_mul(scratch.t[0])),
    );
    game.entities[slot].move_speed_current = magnitude as u16;
}

/// The hunter's body recenter (`hunter_recenter_on_joint`): recompose the
/// death joint chain from the entity's local rotation matrix through the
/// displayed pose's joint transforms, drop the composed Y, subtract the
/// chain-tip joint's world translation, and shift the entity X/Z by the
/// difference so the falling body pivots around the joint that reached the
/// ground instead of sliding through it.
///
/// `which` 0 composes joints 0, 9, 10 and 11 and compares against joint 12's
/// world translation; `which` 1 composes joints 0, 12, 13 and 14 against
/// joint 15's.
pub fn hunter_recenter_on_joint(game: &mut GameState, clips: &[Clip], slot: usize, which: u8) {
    let entity = game.entities[slot];
    let locals = game.entity_anims[slot].local_transforms(&entity, clips);
    let tip = if which == 0 { 12usize } else { 15usize };
    let mut scratch =
        crate::anim::entity_matrix_rotated(entity.pos, entity.pitch, entity.angle, entity.roll);
    for index in [0usize, tip - 3, tip - 2, tip - 1] {
        if let Some(transform) = locals.get(index) {
            scratch = compose(&scratch, transform);
        }
    }
    scratch.t[1] = 0;
    if let Some(world) = game
        .joint_worlds
        .get(slot)
        .and_then(|worlds| worlds.get(tip))
    {
        scratch.t[0] = scratch.t[0].wrapping_sub(world.t[0]);
        scratch.t[2] = scratch.t[2].wrapping_sub(world.t[2]);
    }
    let entity = &mut game.entities[slot];
    entity.pos[0] = entity.pos[0].wrapping_sub(scratch.t[0]);
    entity.pos[2] = entity.pos[2].wrapping_sub(scratch.t[2]);
}

/// The hunter's mouth-joint tracking (`hunter_track_player_joint` and
/// `hunter_scd_track_joint`): recompose the grab chain from the entity's
/// local rotation matrix through the displayed pose's joint transforms, take
/// the composed translation, add the 200-unit forward offset rotated by
/// `(11 - sel) * 600 + yaw`, and store the result into `target`'s tracked
/// joint-1 slot ([`crate::game::Entity::joint_track`]).
///
/// `sel` is the hunter's mouth-joint selector. The held-player chain walks
/// joints 0 and 1 then the three below the mouth joint; the scripted-bite
/// chain walks the fixed joints 0, 1, 3, 4 and 5. Chain indices outside the
/// skeleton are skipped (the original walks past the joint array; nothing in
/// the port reads the bytes there).
pub fn hunter_track_joint(
    game: &mut GameState,
    clips: &[Clip],
    slot: usize,
    target: usize,
    sel: u8,
    scripted: bool,
) -> Option<[i32; 4]> {
    let entity = game.entities[slot];
    let locals = game.entity_anims[slot].local_transforms(&entity, clips);
    let mut scratch =
        crate::anim::entity_matrix_rotated(entity.pos, entity.pitch, entity.angle, entity.roll);
    if scripted {
        for index in [0usize, 1, 3, 4, 5] {
            if let Some(transform) = locals.get(index) {
                scratch = compose(&scratch, transform);
            }
        }
    } else {
        let sel = i32::from(sel);
        for index in [0i32, 1, sel - 3, sel - 2, sel - 1] {
            if index >= 0
                && let Some(transform) = locals.get(index as usize)
            {
                scratch = compose(&scratch, transform);
            }
        }
    }
    let yaw = ((11 - i32::from(sel)) * 600 + i32::from(entity.angle)) as u16 & 0x0FFF;
    let (rx, rz) = player::rotate_xz(yaw, 200, 0);
    let tracked = [
        scratch.t[0].wrapping_add(rx),
        scratch.t[1],
        scratch.t[2].wrapping_add(rz),
        0,
    ];
    if let Some(target) = game.entities.get_mut(target) {
        target.joint_track = tracked;
    }
    Some(tracked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GameState;
    use crate::state::{Collision, CollisionRect, FootstepZone, WalkZone};

    fn entity(pos: [i32; 3], angle: u16, frame: u8) -> Entity {
        // A spawned entity carries the default collision record until its own
        // init (or `e:set_sca`) overwrites it.
        Entity {
            pos,
            angle,
            animation_frame_id: frame,
            sca_radius: 100,
            sca_half_height: crate::game::DEFAULT_ENEMY_HALF_HEIGHT,
            sca_offset: crate::game::DEFAULT_ENEMY_SCA_OFFSET,
            ..Entity::default()
        }
    }

    #[test]
    fn walk_speed_trims_on_the_footfall_frames() {
        let mut e = entity([0, 0, 0], 0, 0);
        entity_apply_walk_speed(&mut e, 0x5D);
        assert_eq!(e.move_speed_current, 0x5D);

        // Frames 7..=0xD lose 0xD; 9..=0xB lose an extra 0xE.
        e.animation_frame_id = 7;
        entity_apply_walk_speed(&mut e, 0x5D);
        assert_eq!(e.move_speed_current, 0x5D - 0xD);
        e.animation_frame_id = 9;
        entity_apply_walk_speed(&mut e, 0x5D);
        assert_eq!(e.move_speed_current, 0x5D - 0xD - 0xE);

        // Frames 0x15..=0x1B lose 0xD; 0x17..=0x19 lose an extra 0xE.
        e.animation_frame_id = 0x16;
        entity_apply_walk_speed(&mut e, 0x5D);
        assert_eq!(e.move_speed_current, 0x5D - 0xD);
        e.animation_frame_id = 0x18;
        entity_apply_walk_speed(&mut e, 0x5D);
        assert_eq!(e.move_speed_current, 0x5D - 0xD - 0xE);
    }

    #[test]
    fn turn_toward_target_steps_along_the_short_turn() {
        let e = entity([0, 0, 0], 0, 0);
        // Target straight along -Z is angle 0x400: +step.
        assert_eq!(turn_toward_target(&e, [0, 0, -100], 0x10), 0x10);
        // Aligned within two steps reports zero.
        let aligned = entity([0, 0, 0], 0x400, 0);
        assert_eq!(turn_toward_target(&aligned, [0, 0, -100], 0x10), 0);
        // Target along +Z is 0xC00: -step.
        assert_eq!(turn_toward_target(&e, [0, 0, 100], 0x10), -0x10);
    }

    #[test]
    fn rotate_toward_target_snaps_and_supports_the_away_bit() {
        let mut e = entity([0, 0, 0], 0x3F0, 0);
        for _ in 0..4 {
            rotate_toward_target(&mut e, [0, 0, -100], 0x100);
        }
        assert_eq!(e.angle, 0x400);

        // Bit 15 faces away: target -Z becomes +Z (0xC00).
        let mut e = entity([0, 0, 0], 0, 0);
        rotate_toward_target(&mut e, [0, 0, -100], 0x8000 | 0x100);
        assert_eq!(e.angle, 0xC00);
    }

    #[test]
    fn visual_and_alert_range_set_their_status_bits() {
        let mut e = entity([0, 0, 0], 0, 0);
        e.status_flags = 1;
        let player = [3000, 0, 4000];
        assert_eq!(entity_check_visual_range(&mut e, player, 5001), 5000);
        assert_eq!(e.status_flags & 0x20, 0x20, "inside the range");
        e.status_flags = 1;
        assert_eq!(
            entity_check_visual_range(&mut e, player, 5000),
            5000,
            "the range test is strictly inside"
        );
        assert_eq!(e.status_flags & 0x20, 0);
        assert_eq!(entity_check_alert_range(&mut e, player, 5001), 5000);
        assert_eq!(e.status_flags & 0x80, 0x80);
    }

    #[test]
    fn is_facing_toward_entity_uses_the_half_turn_window() {
        assert!(is_facing_toward_entity(0, 0));
        assert!(is_facing_toward_entity(0, 0x3FF));
        assert!(!is_facing_toward_entity(0, 0x400), "exactly behind");
        assert!(
            is_facing_toward_entity(0, 0xC00),
            "the window wraps to the boundary on the other side"
        );
        assert!(is_facing_toward_entity(0x400, 0x100));
        assert!(!is_facing_toward_entity(0x400, 0x900));
    }

    #[test]
    fn pathfind_track_keeps_only_bit_zero_when_no_other_bit_is_set() {
        let mut e = entity([0, 0, 0], 0, 0);
        e.attacking_direction = 0xFC;
        e.dir_control_flags = 0x12;
        pathfind_track(&mut e, 0x01);
        assert_eq!(e.attacking_direction, 0xFD, "only bit 0 is rewritten");
        assert_eq!(e.dir_control_flags, 0x12, "the high byte is untouched");
        pathfind_track(&mut e, 0x02);
        assert_eq!(e.attacking_direction, 0xFD, "a higher bit writes nothing");
        pathfind_track(&mut e, 0x00);
        assert_eq!(e.attacking_direction, 0xFC);
    }

    #[test]
    fn wasp_pathfind_track_writes_the_second_scratch_word() {
        let mut e = entity([0, 0, 0], 0, 0);
        e.tex_bank = 0xFE;
        e.seq_counter = 0x12;
        wasp_pathfind_track(&mut e, 0x01);
        assert_eq!(e.tex_bank, 0xFF, "only bit 0 of the low byte is rewritten");
        assert_eq!(e.seq_counter, 0x12, "the high byte is untouched");
        wasp_pathfind_track(&mut e, 0x02);
        assert_eq!(e.tex_bank, 0xFF, "a higher bit writes nothing");
        wasp_pathfind_track(&mut e, 0x00);
        assert_eq!(e.tex_bank, 0xFE);
    }

    #[test]
    fn check_room_collision_returns_the_resolve_code() {
        let room = blocked_room();
        let mut inside = entity([900, 0, 500], 0, 0);
        inside.pos = [1200, 0, 500];
        assert_eq!(
            check_room_collision(&room, &mut inside),
            0,
            "a record without the 0x300 bits pushes but reports no hit"
        );
        assert!(
            !player::position_blocked(&room, inside.pos, 100, inside.collision_flags),
            "the pushed position is clear"
        );
        assert_eq!(
            inside.saved_pos,
            Some(inside.pos),
            "the accepted point stores"
        );

        let mut blocking = blocked_room();
        for rect in &mut blocking.collision.quadrants[0] {
            rect.flags = 0x300;
        }
        let mut inside = entity([900, 0, 500], 0, 0);
        inside.pos = [1200, 0, 500];
        assert_eq!(check_room_collision(&blocking, &mut inside), 1);

        let mut clear = entity([100, 0, 100], 0, 0);
        assert_eq!(check_room_collision(&room, &mut clear), 0);
    }

    #[test]
    fn check_room_collision_keeps_the_crossed_floor_step() {
        let mut room = RoomState::default();
        room.collision.quadrants[0].push(CollisionRect {
            x_max: 2000,
            z_max: 2000,
            x_min: 0,
            z_min: 0,
            kind: 0x8300,
            flags: 0x0004,
        });
        let mut e = entity([500, 0, 500], 0, 0);
        e.collision_flags = 0x04;
        assert_eq!(check_room_collision(&room, &mut e), 3);
        assert_eq!(e.floor_step, -3399, "(-3400) | 1");
        assert_eq!(e.pos, [500, 0, 500], "a floor zone never pushes");

        // Without the floor-reporting flag the same record has no handler.
        let mut e = entity([500, 0, 500], 0, 0);
        assert_eq!(check_room_collision(&room, &mut e), 0);
        assert_eq!(e.floor_step, 0);
    }

    #[test]
    fn joint_reach_box_is_a_square_around_the_joint() {
        let joint = Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [1000, -500, 2000],
        };
        assert!(joint_reach_test(&joint, [0, 0, 0], 900, [1000, 0, 2000]));
        assert!(joint_reach_test(&joint, [0, 0, 0], 900, [1900, 0, 2000]));
        assert!(!joint_reach_test(&joint, [0, 0, 0], 900, [1901, 0, 2000]));
        assert!(
            joint_reach_test(&joint, [0, 0, 0], 900, [1000, 99999, 2000]),
            "height never participates"
        );
        assert!(joint_reach_test(&joint, [500, 0, 0], 900, [2400, 0, 2000]));
        assert!(!joint_reach_test(&joint, [500, 0, 0], 900, [2410, 0, 2000]));
    }

    #[test]
    fn separate_all_reports_the_player_pair_hit() {
        let mut game = GameState::default();
        game.entities[0].pos = [500, 0, 500];
        game.entities[0].set_active(true);
        game.entities[1] = entity([500, 0, 500], 0, 0);
        game.entities[1].set_active(true);
        assert!(separate_all(&mut game, 1), "the overlap reports a touch");

        let mut game = GameState::default();
        game.entities[0].pos = [500, 0, 500];
        game.entities[1] = entity([5000, 0, 500], 0, 0);
        game.entities[1].set_active(true);
        assert!(!separate_all(&mut game, 1), "a distant player is no touch");
    }

    fn blocked_room() -> RoomState {
        RoomState {
            collision: Collision {
                cell_x: 0,
                cell_z: 0,
                quadrants: std::array::from_fn(|_| {
                    vec![CollisionRect {
                        x_max: 2000,
                        z_max: 2000,
                        x_min: 1000,
                        z_min: 0,
                        kind: 1,
                        flags: 0,
                    }]
                }),
            },
            ..RoomState::default()
        }
    }

    #[test]
    fn advance_xz_is_un_collided() {
        let room = blocked_room();
        // Angle 0 walks +X straight into the rectangle at x=1000: Add_speedXZ
        // never consults the room, so the move commits. The 4.12 cosine
        // saturates one unit short of 100 at yaw 0, the established
        // `rotate_speed` behaviour.
        let mut e = entity([900, 0, 500], 0, 0);
        advance_xz(&mut e, 0, 100);
        assert_eq!(e.pos, [999, 0, 500]);
        assert!(
            player::position_blocked(&room, e.pos, 100, e.collision_flags),
            "the bare move can land inside geometry"
        );

        // The end-of-frame tail resolves it back out.
        let resolved = player::resolve_collision(
            &room.collision,
            [900, 0, 500],
            e.pos,
            100,
            e.collision_flags,
        );
        assert!(
            !player::position_blocked(&room, resolved, 100, e.collision_flags),
            "the collision pass pushes the character out: {resolved:?}"
        );
    }

    #[test]
    fn advance_xz_stores_the_vector_and_advance_speed_re_adds() {
        // Yaw 0 walks +X; the 4.12 cosine saturates one unit short of 100, the
        // established `rotate_speed` behaviour.
        let mut e = entity([0, 0, 0], 0, 0);
        advance_xz(&mut e, 0, 100);
        assert_eq!(e.speed, [99, 0, 0], "the rotated vector stores");
        assert_eq!(e.pos, [99, 0, 0]);

        // Re-adding the stored vector moves without recomputing the yaw, so a
        // turned entity keeps the launch direction.
        e.angle = 0x400;
        advance_speed(&mut e);
        assert_eq!(e.pos, [198, 0, 0], "the stored X vector, not the new yaw");

        // A negative distance stores the sign-extended components too.
        let mut e = entity([0, 0, 0], 0, 0);
        advance_xz(&mut e, 0, -100);
        assert_eq!(e.speed, [-99, 0, 0]);
        assert_eq!(e.pos, [-99, 0, 0]);
    }

    #[test]
    fn swerve_picks_a_sidestep_and_counts_the_latch_down() {
        // Facing straight at the target: the sidestep aims at the target's Z
        // (status bit 4 clear), so the fresh swerve turns +0x40.
        let target = [100, 0, 0];
        let mut e = entity([0, 0, 0], 0, 0);
        assert_eq!(
            swerve_around_obstacle(&mut e, target, 0x40, true, 4),
            0x40,
            "a fresh block picks the right-hand sidestep"
        );
        assert_eq!((e.swerve, e.swerve_latch), (0x40, 4));
        assert_eq!(
            swerve_around_obstacle(&mut e, target, 0x40, true, 4),
            0x40,
            "a still-blocked frame holds the same direction"
        );
        assert_eq!((e.swerve, e.swerve_latch), (0x40, 4));

        // Once the obstacle clears, the latch counts down one frame per tick
        // with bit 7 set on the first clear frame.
        for remaining in [0x83u8, 0x82, 0x81, 0x80] {
            assert_eq!(swerve_around_obstacle(&mut e, target, 0x40, false, 4), 0x40);
            assert_eq!(e.swerve_latch, remaining);
        }
        // The next clear frame exhausts the low seven bits and falls back to a
        // plain turn: the entity is already aligned, so the delta is zero.
        assert_eq!(swerve_around_obstacle(&mut e, target, 0x40, false, 4), 0);
        assert_eq!((e.swerve, e.swerve_latch), (0, 0));
    }

    #[test]
    fn swerve_folds_the_turn_onto_the_quadrant_boundary() {
        let target = [100, 0, 0];
        // Just below the 0x400 boundary with a positive swerve: the delta is
        // rewritten to land exactly on it.
        let mut e = entity([0, 0, 0], 0x3E0, 0);
        assert_eq!(swerve_around_obstacle(&mut e, target, 0x40, true, 4), 0x20);
        assert_eq!(e.angle.wrapping_add(0x20), 0x400);

        // Just above it with a negative swerve (status bit 4 picks the other
        // sidestep): the same clamp from the other side.
        let mut e = entity([0, 0, 0], 0x420, 0);
        e.status_flags = 0x10;
        assert_eq!(swerve_around_obstacle(&mut e, target, 0x40, true, 4), -0x20);
        assert_eq!(e.angle.wrapping_sub(0x20), 0x400);
    }

    #[test]
    fn swerve_falls_back_to_a_plain_turn_step() {
        let target = [100, 0, 0];
        let mut e = entity([0, 0, 0], 0x100, 0);
        assert_eq!(
            swerve_around_obstacle(&mut e, target, 0x40, false, 4),
            -0x40
        );
        assert_eq!((e.swerve, e.swerve_latch), (0, 0));
        // A frame with a nonzero low latch is path B: bit 7 is set by the
        // clear-obstacle statement and the whole byte counts down.
        let mut e = entity([0, 0, 0], 0, 0);
        e.swerve = 0x40;
        e.swerve_latch = 2;
        assert_eq!(swerve_around_obstacle(&mut e, target, 0x40, false, 4), 0x40);
        assert_eq!(e.swerve_latch, 0x81);
    }

    #[test]
    fn try_advance_xz_probes_and_restores() {
        let room = blocked_room();
        // The idle walk-01 probe: the move is committed for the test and then
        // restored, and a blocked point reports the hit as `false`.
        let mut e = entity([900, 0, 500], 0, 0);
        assert!(!try_advance_xz(&room, &mut e, 0, 100));
        assert_eq!(e.pos, [900, 0, 500], "the probe rolls the move back");

        // Walking away is clear: the move stays committed.
        let mut e = entity([900, 0, 500], 0x800, 0);
        assert!(try_advance_xz(&room, &mut e, 0, 100));
        assert_eq!(e.pos, [800, 0, 500]);
    }

    #[test]
    fn diagonal_approach_slides_along_the_wall() {
        let room = blocked_room();
        // The character steps diagonally into the wall's west face: only the X
        // movement disagrees with the push, so X is corrected and the Z step is
        // kept - the character slides instead of freezing.
        let prev = [900, 0, 500];
        let proposed = [1000, 0, 600];
        let resolved = player::resolve_collision(&room.collision, prev, proposed, 100, 0);
        assert_eq!(resolved, [882, 0, 600], "X pushed out, Z kept");
        assert!(
            resolved[2] > prev[2],
            "the slide kept the along-wall movement"
        );
    }

    #[test]
    fn footstep_resolves_the_room_sound() {
        let room = RoomState {
            stage: 1,
            room: 0,
            footstep_zones: vec![FootstepZone {
                base_x: 0,
                base_z: 0,
                width: 0x8000,
                height: 0x8000,
                sound_data: 0x2D,
            }],
            ..RoomState::default()
        };
        let e = entity([100, 0, 100], 0, 0);
        let mut sounds = Vec::new();
        footstep(&mut sounds, &room, &e, 0, false);
        assert_eq!(sounds.len(), 1);
        assert_eq!(sounds[0].pos, [100, 0, 100]);
        assert!(sounds[0].name.starts_with("ft_"));
    }

    /// One walk zone; `flags` names the adjacent zone indices.
    fn zone(x1: i16, z1: i16, x2: i16, z2: i16, flags: u16) -> WalkZone {
        WalkZone {
            x1,
            z1,
            x2,
            z2,
            field_08: 0,
            flags,
        }
    }

    /// A three-zone corridor along X: 0 [0,100) x [0,100), 1 [100,200), 2
    /// [200,300), chained by their adjacency bits.
    fn corridor() -> RoomState {
        RoomState {
            walk_zones: vec![
                zone(0, 0, 100, 100, 1 << 1),
                zone(100, 0, 200, 100, (1 << 0) | (1 << 2)),
                zone(200, 0, 300, 100, 1 << 1),
            ],
            ..RoomState::default()
        }
    }

    /// An L of three zones: 0 and 1 share the X edge at x=100, 1 and 2 share
    /// the Z edge at z=100.
    fn corner() -> RoomState {
        RoomState {
            walk_zones: vec![
                zone(0, 0, 100, 100, 1 << 1),
                zone(100, 0, 200, 100, (1 << 0) | (1 << 2)),
                zone(100, 100, 200, 200, 1 << 1),
            ],
            ..RoomState::default()
        }
    }

    /// A chain of `count` zones along X: zone `i` is [100i, 100(i+1)) x [0,
    /// 100), adjacent to `i-1` and `i+1`.
    fn chain(count: usize) -> RoomState {
        RoomState {
            walk_zones: (0..count)
                .map(|i| {
                    let mut flags = 0u16;
                    if i > 0 {
                        flags |= 1 << (i - 1);
                    }
                    if i + 1 < count {
                        flags |= 1 << (i + 1);
                    }
                    zone((i * 100) as i16, 0, ((i + 1) * 100) as i16, 100, flags)
                })
                .collect(),
            ..RoomState::default()
        }
    }

    /// A four-zone ring around a 200x200 square: 0 SW, 1 SE, 2 NE, 3 NW, each
    /// adjacent to its two neighbours, so both ways around reach the far zone.
    fn ring() -> RoomState {
        RoomState {
            walk_zones: vec![
                zone(0, 0, 100, 100, (1 << 1) | (1 << 3)),
                zone(100, 0, 200, 100, (1 << 0) | (1 << 2)),
                zone(100, 100, 200, 200, (1 << 1) | (1 << 3)),
                zone(0, 100, 100, 200, (1 << 0) | (1 << 2)),
            ],
            ..RoomState::default()
        }
    }

    #[test]
    fn walk_zone_find_is_half_open_and_wraps_unsigned() {
        let room = corridor();
        assert_eq!(walk_zone_find(&room, 0, 0), Some(0), "x1 is inside");
        assert_eq!(walk_zone_find(&room, 99, 99), Some(0));
        assert_eq!(
            walk_zone_find(&room, 100, 50),
            Some(1),
            "the shared edge belongs to the later zone"
        );
        assert_eq!(walk_zone_find(&room, 300, 50), None, "x2 is outside");

        // A negative coordinate wraps through the unsigned subtraction.
        let negative = RoomState {
            walk_zones: vec![zone(-100, -100, 0, 0, 0)],
            ..RoomState::default()
        };
        assert_eq!(walk_zone_find(&negative, -1, -1), Some(0));
        assert_eq!(walk_zone_find(&negative, 0, 0), None, "x2/z2 are half-open");
        assert_eq!(walk_zone_find(&negative, -101, -1), None);
    }

    #[test]
    fn walk_zone_shared_edge_names_the_crossing() {
        let room = corner();
        let (flag, crossing) = walk_zone_shared_edge(&room, 0, 1);
        assert_eq!(flag, 0, "the 0/1 edge runs along X");
        assert_eq!(crossing, [100, 50]);

        let (flag, crossing) = walk_zone_shared_edge(&room, 1, 2);
        assert_eq!(flag, 1, "the 1/2 edge runs along Z");
        assert_eq!(crossing, [150, 100]);

        // The reverse direction names the same midpoint.
        assert_eq!(walk_zone_shared_edge(&room, 1, 0), (0, [100, 50]));
    }

    #[test]
    fn zone_path_find_returns_direct_adjacent_and_corner_steps() {
        let room = corridor();
        assert_eq!(
            zone_path_find(&room, [50, 0, 50], [60, 0, 60]),
            ZonePath::Direct { zone: 0 }
        );

        let path = zone_path_find(&room, [50, 0, 50], [250, 0, 50]);
        assert_eq!(
            path,
            ZonePath::Cross {
                from: 0,
                next: 1,
                crossing: [100, 50]
            }
        );

        let path = zone_path_find(&corner(), [50, 0, 50], [150, 0, 150]);
        assert_eq!(
            path,
            ZonePath::Cross {
                from: 0,
                next: 1,
                crossing: [100, 50]
            },
            "the corner route starts through the middle zone"
        );
    }

    #[test]
    fn zone_path_find_wraps_around_the_ring() {
        let room = ring();
        // Zone 0 to zone 2 is not adjacent: the ring walk leaves through the
        // NW neighbour. (Both two-hop routes are equal length; the CW walk's
        // unwritten previous-position scratch is what selects zone 3, the same
        // deterministic fresh-scratch result as the original's split arrays.)
        assert_eq!(
            zone_path_find(&room, [50, 0, 50], [150, 0, 150]),
            ZonePath::Cross {
                from: 0,
                next: 3,
                crossing: [50, 100]
            }
        );
        // Zone 2 to zone 0 takes the same side of the ring back through
        // zone 3, whose shared edge with zone 0 the crossing names.
        assert_eq!(
            zone_path_find(&room, [150, 0, 150], [50, 0, 50]),
            ZonePath::Cross {
                from: 2,
                next: 3,
                crossing: [100, 150]
            }
        );
    }

    #[test]
    fn zone_path_find_walks_a_deep_chain_to_the_last_zone() {
        // A chain forces the ring walk to step through every zone before the
        // goal step; the scratch copies the whole best path, whose length
        // reaches one past the last visited zone. Sixteen zones put `step` at
        // its maximum of 15, so the copy reaches index 16.
        for count in [13usize, 16] {
            let room = chain(count);
            assert_eq!(
                zone_path_find(&room, [50, 0, 50], [(count - 1) as i32, 0, 0]),
                ZonePath::Cross {
                    from: 0,
                    next: 1,
                    crossing: [100, 50]
                }
            );
        }
    }

    #[test]
    fn zone_path_find_zone_index_mode_uses_the_zone_midpoint() {
        let room = ring();
        // to.z == 0: to.x names a zone. Zone 2's midpoint is (150, 150), and
        // the route from zone 0 starts the same way as the point-target form
        // (the point [150, 0, 150] resolves into zone 2 as well).
        let expected = ZonePath::Cross {
            from: 0,
            next: 3,
            crossing: [50, 100],
        };
        assert_eq!(zone_path_find(&room, [50, 0, 50], [2, 0, 0]), expected);
        assert_eq!(zone_path_find(&room, [50, 0, 50], [150, 0, 150]), expected);
        // Standing in the named zone is a direct path.
        assert_eq!(
            zone_path_find(&room, [150, 0, 150], [2, 0, 0]),
            ZonePath::Direct { zone: 2 }
        );
        // An index outside the table has no zone record to name.
        assert_eq!(
            zone_path_find(&room, [50, 0, 50], [9, 0, 0]),
            ZonePath::Unreachable
        );
    }

    #[test]
    fn zone_path_find_reports_unreachable_targets() {
        let room = RoomState {
            walk_zones: vec![zone(0, 0, 100, 100, 0), zone(1000, 1000, 1100, 1100, 0)],
            ..RoomState::default()
        };
        assert_eq!(
            zone_path_find(&room, [50, 0, 50], [1050, 0, 1050]),
            ZonePath::Unreachable
        );
        assert_eq!(
            zone_path_find(&room, [5000, 0, 5000], [1050, 0, 1050]),
            ZonePath::Unreachable,
            "a point outside every zone has no path"
        );
    }

    #[test]
    fn entity_pathfind_update_cycles_and_latches_the_waypoint() {
        let room = corridor();
        let mut e = Entity {
            id: 0x23,
            pos: [50, 0, 50],
            ..Entity::default()
        };
        let player = [80, 0, 50];
        // A clear straight-ahead line: the counter just advances.
        for _ in 0..3 {
            assert_eq!(entity_pathfind_update(&room, &mut e, player), 2);
        }
        // Counter 3 with the LOS bit clear: latch the player waypoint.
        assert_eq!(entity_pathfind_update(&room, &mut e, player), 1);
        assert_eq!((e.player_pos_x, e.player_pos_z), (80, 50));
        assert_eq!(e.pathfind_state & 0x1F, 4);
        assert_eq!(e.pathfind_state & 0x20, 0, "the LOS bit stays clear");

        // Past 3 the byte just counts; at 0x10 it clamps back to zero.
        for _ in 0..12 {
            assert_eq!(entity_pathfind_update(&room, &mut e, player), 2);
        }
        assert_eq!(e.pathfind_state & 0x1F, 0, "the counter clamps at 16");
    }

    #[test]
    fn entity_pathfind_update_blocked_los_skips_the_latch() {
        let room = corridor();
        let mut e = Entity {
            id: 0x23,
            pos: [50, 0, 50],
            angle: 0,
            ..Entity::default()
        };
        // The player directly behind: outside the 1512 angular window, so
        // every LOS result is blocked.
        let player = [-100, 0, 50];
        for _ in 0..3 {
            entity_pathfind_update(&room, &mut e, player);
        }
        let waypoint = (e.player_pos_x, e.player_pos_z);
        assert_eq!(entity_pathfind_update(&room, &mut e, player), 0);
        assert_eq!(
            (e.player_pos_x, e.player_pos_z),
            waypoint,
            "the blocked frame does not latch a waypoint"
        );
        assert_eq!(e.pathfind_state & 0x1F, 4);
        assert_eq!(e.pathfind_state & 0x20, 0, "the LOS bit is cleared");
    }

    #[test]
    fn crossing_heading_clamps_the_crossing_out_of_a_wall() {
        // Two zones sharing the X edge at x=100 with a Z overlap [0, 8000).
        let mut room = RoomState {
            walk_zones: vec![
                zone(0, 0, 100, 8000, 1 << 1),
                zone(100, 0, 200, 8000, 1 << 0),
            ],
            ..RoomState::default()
        };
        // A wall at the corridor's far end.
        room.collision.quadrants[0].push(CollisionRect {
            x_max: 300,
            z_max: 8100,
            x_min: 0,
            z_min: 7900,
            kind: 1,
            flags: 0,
        });

        // The character sits past the blocked end; the clamp pulls it in.
        let (waypoint, heading) = crossing_heading(&room, [50, 0, 7950], Some(0), 1, 100);
        assert!(
            waypoint[2] < 7950,
            "the blocked corridor end is nudged inward: {waypoint:?}"
        );
        assert_eq!(waypoint[0], 100, "the shared X edge is kept");
        assert_eq!(
            heading,
            sfx::angle_between_xz(50, 7950, waypoint[0], waypoint[2])
        );

        // Without the wall the value itself is kept.
        room.collision.quadrants[0].clear();
        let (waypoint, _) = crossing_heading(&room, [50, 0, 3000], Some(0), 1, 100);
        assert_eq!(waypoint, [100, 0, 3000]);
    }

    #[test]
    fn crossing_heading_falls_back_to_the_entity_position() {
        let room = corridor();
        let (waypoint, heading) = crossing_heading(&room, [50, 0, 50], Some(0), 9, 100);
        assert_eq!(waypoint, [50, 0, 50]);
        assert_eq!(heading, 0);
    }

    #[test]
    fn choose_heading_walks_straight_within_one_zone() {
        let room = corridor();
        let mut e = entity([50, 0, 50], 0, 0);
        let heading = choose_heading(&mut e, &room, [80, 0, 50]);
        assert_eq!(heading, 0, "the player is due +X");
        assert_eq!(e.bob_speed, 0x10, "the direct result carries bit 4");
        assert_eq!((e.player_pos_x, e.player_pos_z), (80, 50));
        assert_eq!(e.reaction_timer, 0);
    }

    #[test]
    fn state9_entry_resets_the_look_at_and_marks_the_ignore() {
        let room = corridor();
        let mut game = GameState::default();
        game.entities[0].pos = [2000, 0, 0];
        game.entities[1] = Entity {
            id: 0x23,
            action_behavior: 0,
            action_state: 0,
            ..Entity::default()
        };
        game.entities[1].set_active(true);
        update(&mut game, 1, &room, &[]);
        assert_eq!(game.entities[1].ignore(), 1);
        assert_eq!(game.entities[1].look_at_flags, 0x10);
        assert_eq!(game.entities[1].action_state, 1, "the pace wait started");
    }

    #[test]
    fn state9_tail_rolls_back_to_the_stored_position_not_the_push() {
        // The room-object pass runs after the state-9 tail and shoves the
        // character without touching the original's `position` word. The next
        // tail's room resolve must roll back to that stored word, not to the
        // shove it just received.
        let mut room = RoomState::default();
        room.collision.quadrants[0].push(CollisionRect {
            x_max: 1400,
            z_max: 1000,
            x_min: 1000,
            z_min: 0,
            kind: 1,
            flags: 0x300,
        });
        let accepted = [100, 0, 500];
        let pushed = [1200, 0, 500];
        let mut game = GameState::default();
        game.entities[0].pos = [0, 0, 0];
        game.entities[1] = Entity {
            id: 0x23,
            pos: pushed,
            sca_radius: 100,
            action_behavior: 4,
            saved_pos: Some(accepted),
            ..Entity::default()
        };
        game.entities[1].set_active(true);
        update(&mut game, 1, &room, &[]);
        assert_eq!(
            game.entities[1].pos, accepted,
            "the tail kept the object-pass push instead of the accepted position"
        );
        assert_eq!(game.entities[1].saved_pos, Some(accepted));
    }

    #[test]
    fn distance_thresholds_swap_the_follow_behaviour() {
        let room = corridor();
        let mut game = GameState::default();
        let mut entity = Entity {
            id: 0x23,
            action_behavior: 1,
            action_state: 1,
            animation_id: 7,
            ..Entity::default()
        };
        entity.set_active(true);
        entity.sca_radius = 100;
        game.entities[1] = entity;
        // Player close: behaviour 1 swaps to the pace (0).
        game.entities[0].pos = [1000, 0, 50];
        game.entities[1].pos = [1500, 0, 50];
        update(&mut game, 1, &room, &[]);
        assert_eq!(game.entities[1].action_behavior, 0);

        // Player far: behaviour 1 swaps to the fast walk (2).
        let mut entity = Entity {
            id: 0x23,
            action_behavior: 1,
            action_state: 1,
            ..Entity::default()
        };
        entity.set_active(true);
        entity.sca_radius = 100;
        game.entities[1] = entity;
        game.entities[0].pos = [8000, 0, 0];
        update(&mut game, 1, &room, &[]);
        assert_eq!(game.entities[1].action_behavior, 2);

        // A close player keeps behaviour 3 backing away (its near slot is
        // disabled); a far one swaps to the pace.
        let mut entity = Entity {
            id: 0x23,
            action_behavior: 3,
            action_state: 1,
            ..Entity::default()
        };
        entity.set_active(true);
        entity.sca_radius = 100;
        game.entities[1] = entity;
        game.entities[0].pos = [1500, 0, 0];
        update(&mut game, 1, &room, &[]);
        assert_eq!(game.entities[1].action_behavior, 3);

        game.entities[0].pos = [6000, 0, 0];
        update(&mut game, 1, &room, &[]);
        assert_eq!(game.entities[1].action_behavior, 0, "past 0x09C4");
    }

    #[test]
    fn walk_to_player_closes_the_distance() {
        let room = corridor();
        let mut game = GameState::default();
        game.entities[0].pos = [2900, 0, 50];
        game.entities[1] = Entity {
            id: 0x23,
            pos: [50, 0, 50],
            action_behavior: 1,
            action_state: 1,
            animation_id: 7,
            sca_radius: 100,
            ..Entity::default()
        };
        game.entities[1].set_active(true);

        let start = xz_distance_to(&game.entities[1], game.entities[0].pos);
        for _ in 0..30 {
            update(&mut game, 1, &room, &[]);
            if game.entities[1].action_behavior != 1 {
                break;
            }
        }
        let end = xz_distance_to(&game.entities[1], game.entities[0].pos);
        assert!(end < start, "distance {start} -> {end}");
        assert!(end < 0x0AF0, "the near threshold swaps the walk: {end}");
        assert_eq!(
            game.entities[1].action_behavior, 0,
            "the walk yields to the pace behaviour"
        );
    }

    #[test]
    fn fast_walk_advances_further_than_the_walk() {
        let room = corridor();
        let run = |behavior: u8| {
            let mut game = GameState::default();
            game.entities[0].pos = [2000, 0, 50];
            game.entities[1] = Entity {
                id: 0x23,
                pos: [50, 0, 50],
                angle: 0,
                action_behavior: behavior,
                action_state: 1,
                animation_id: if behavior == 2 { 8 } else { 7 },
                sca_radius: 100,
                ..Entity::default()
            };
            game.entities[1].set_active(true);
            for _ in 0..10 {
                update(&mut game, 1, &room, &[]);
            }
            game.entities[1].pos[0]
        };
        let walk = run(1);
        let fast = run(2);
        assert!(
            fast > walk,
            "the fast walk covers more ground ({fast} vs {walk})"
        );
    }

    #[test]
    fn heading_window_includes_the_exact_boundary() {
        // The original tests `< half * 2 + 1`, so `delta == half * 2` is
        // inside for behaviour 1 (half 0x180), behaviour 2's second test and
        // behaviour 3 (both 0x200).
        assert!(heading_within(0, 0x180, 0x180));
        assert!(heading_within(0, 0x200, 0x200));
        assert!(!heading_within(0, 0x181, 0x180));
        assert!(!heading_within(0, 0x201, 0x200));
        // A negative delta wraps through the original's unsigned cast and
        // fails, so a heading on the far side of zero does not pass.
        assert!(!heading_within(0x200, 0xFFF, 0x200));
    }

    #[test]
    fn face_and_retreat_walks_back_at_the_exact_window_boundary() {
        let room = corridor();
        let mut game = GameState::default();
        // The player sits along heading 0x200 and the character's yaw is 0, so
        // the window delta is exactly 0x200 + 0x200 = 0x400. The original's
        // `< 0x401` includes it: the character backs away on animation 3.
        game.entities[0].pos = [100, 0, -100];
        game.entities[1] = Entity {
            id: 0x23,
            pos: [0, 0, 0],
            angle: 0,
            action_behavior: 3,
            action_state: 1,
            sca_radius: 100,
            ..Entity::default()
        };
        game.entities[1].set_active(true);
        update(&mut game, 1, &room, &[]);
        assert_eq!(
            game.entities[1].animation_id, 3,
            "the exact 0x400 boundary takes the backward walk"
        );
    }

    #[test]
    fn face_and_retreat_backs_away_from_the_player() {
        let room = corridor();
        let mut game = GameState::default();
        game.entities[0].pos = [1000, 0, 50];
        game.entities[1] = Entity {
            id: 0x23,
            pos: [500, 0, 50],
            angle: 0,
            action_behavior: 3,
            action_state: 1,
            sca_radius: 100,
            ..Entity::default()
        };
        game.entities[1].set_active(true);
        for _ in 0..5 {
            update(&mut game, 1, &room, &[]);
        }
        assert_eq!(game.entities[1].animation_id, 3, "the backward clip");
        assert!(
            game.entities[1].pos[0] < 500,
            "the character backs away: {:?}",
            game.entities[1].pos
        );
    }

    #[test]
    fn pace_waits_then_walks_and_falls_through() {
        let seed = 0xACE1;
        let mut e = Entity {
            id: 0x23,
            ..Entity::default()
        };
        behavior_00(&mut e, seed);
        assert_eq!(e.action_state, 1);
        assert!(e.tex_bank >= 0x40, "the wait is seeded");

        // Drain the wait: the next tick starts the short walk.
        e.tex_bank = 1;
        behavior_00(&mut e, seed);
        assert_eq!(e.action_state, 2);
        assert_eq!(e.animation_id, 5);

        // The animation-done flag (bit 0 of attacking_direction) moves on.
        e.attacking_direction = 1;
        behavior_00(&mut e, seed);
        assert_eq!(e.action_state, 3);
        assert_eq!(e.animation_id, 6);
    }

    #[test]
    fn turn_toward_heading_adds_the_id_parity_step() {
        // Odd id: the 0x30 step becomes 0x38, and 4 ticks cover 0xE0.
        let mut odd = Entity {
            id: 0x23,
            angle: 0,
            ..Entity::default()
        };
        for _ in 0..4 {
            turn_toward_heading(&mut odd, 0x400, 0x30);
        }
        assert_eq!(odd.angle, 0xE0);

        let mut even = Entity {
            id: 0x22,
            angle: 0,
            ..Entity::default()
        };
        for _ in 0..4 {
            turn_toward_heading(&mut even, 0x400, 0x30);
        }
        assert_eq!(even.angle, 0xC0);
    }

    #[test]
    fn wander_lookat_reseeds_and_forces_a_change() {
        let seed = 0xACE1;
        let mut e = Entity {
            id: 0x23,
            seq_counter: 1,
            angle_turn_delta: 0xFF,
            ..Entity::default()
        };
        wander_lookat(&mut e, 0, seed);
        assert_eq!(e.look_at_flags, 0x33);
        assert!(e.seq_counter >= 0x18, "the reseed is at least 0x18");
        assert!(e.look_at_yaw_step > 0x28);
        assert!(e.look_at_pitch_step > 0x14);

        // A held countdown leaves the angles alone.
        let mut held = e;
        held.seq_counter = 5;
        let before = (held.angle_turn_delta, held.move_timer, held.is_moving);
        wander_lookat(&mut held, 0, seed);
        assert_eq!(held.seq_counter, 4);
        assert_eq!(
            (held.angle_turn_delta, held.move_timer, held.is_moving),
            before
        );
    }

    #[test]
    fn state9_footsteps_follow_the_walk_clips() {
        let room = RoomState {
            stage: 1,
            room: 0,
            footstep_zones: vec![FootstepZone {
                base_x: 0,
                base_z: 0,
                width: 0x8000,
                height: 0x8000,
                sound_data: 0x2D,
            }],
            ..RoomState::default()
        };
        let mut sounds = Vec::new();
        let mut e = entity([100, 0, 100], 0, 0);
        e.animation_id = 7;
        e.animation_frame_id = 8;
        walk_footstep_sound(&mut sounds, &room, &e, false);
        assert_eq!(sounds.len(), 1);
        assert_eq!(sounds[0].name, "ft_wdA");

        sounds.clear();
        e.animation_id = 8;
        e.animation_frame_id = 0;
        walk_footstep_sound(&mut sounds, &room, &e, false);
        assert_eq!(sounds.len(), 1, "the fast walk uses the B footstep");
        assert_eq!(sounds[0].name, "ft_wdB");

        sounds.clear();
        e.animation_id = 7;
        e.animation_frame_id = 7;
        walk_footstep_sound(&mut sounds, &room, &e, false);
        assert!(sounds.is_empty(), "frame 7 is not a contact");
    }

    #[test]
    fn effect_zone_flag_shifts_the_state9_footstep_column() {
        let room = RoomState {
            stage: 1,
            room: 6,
            footstep_zones: vec![FootstepZone {
                base_x: 0,
                base_z: 0,
                width: 0x8000,
                height: 0x8000,
                sound_data: 45,
            }],
            ..RoomState::default()
        };
        let mut game = GameState::new(crate::state::RoomId::parse("1060").unwrap(), &room);
        // Clip 7 has enough frames that the clock advances the placed frame
        // 7 onto the contact frame 8.
        let clips = vec![
            Clip {
                frames: vec![
                    crate::model::ClipFrame {
                        keyframe: 0,
                        timing: 0,
                    };
                    32
                ],
            };
            9
        ];
        let place = |game: &mut GameState| {
            let mut e = Entity {
                id: 0x23,
                action_behavior: 4,
                animation_id: 7,
                animation_frame_id: 7,
                pos: [100, 0, 100],
                ..Entity::default()
            };
            e.set_active(true);
            game.entities[1] = e;
            game.entity_sounds.clear();
        };

        place(&mut game);
        update(&mut game, 1, &room, &clips);
        assert_eq!(
            game.entity_sounds.last().map(|sound| sound.name),
            Some("ft_stwp")
        );

        // The room-action effect-zone bit (MSF2_EFFECT_ZONE) drops the column
        // by three: the concrete footstep variant plays.
        game.flags[5].apply(crate::game::MSF2_EFFECT_ZONE, 0);
        place(&mut game);
        update(&mut game, 1, &room, &clips);
        assert_eq!(
            game.entity_sounds.last().map(|sound| sound.name),
            Some("ft_cpA")
        );
    }

    #[test]
    fn separation_pushes_the_character_out_of_the_player() {
        let mut e = entity([1100, 0, 500], 0, 0);
        e.sca_radius = 100;
        let prev = e.pos;
        assert!(separate_from_player(
            &mut e,
            prev,
            [1000, 0, 500],
            0,
            100,
            0
        ));
        let dist = xz_distance_to(&e, [1000, 0, 500]);
        assert!(dist > 100, "pushed out of the overlap: {dist}");
    }

    #[test]
    fn sca_volume_heights_gate_the_push() {
        // The sprawled Enrico volume sits only 0xB4 above its base; a standing
        // character whose volume bottom is far above it does not push the pair.
        let mut e = entity([1050, 0, 300], 0, 0);
        e.id = 0x28;
        e.sca_radius = 500;
        let prev = e.pos;
        // A synthetic other volume 6000 units above the sprawled body.
        let high = ScaHit {
            offset: [0, 6000, 0],
            half_height: 0x5FA,
            radius: 100,
            world_bias: [0; 3],
        };
        assert!(!separate_from_character(
            &mut e,
            prev,
            [1150, 0, 300],
            0,
            &[high],
            0
        ));
        assert_eq!(e.pos, prev, "the vertical gap skips the pair");

        // Lower the other volume until the half-heights overlap: the push runs.
        let low = ScaHit {
            offset: [0, 0, 0],
            half_height: 0x5FA,
            radius: 100,
            world_bias: [0; 3],
        };
        assert!(separate_from_character(
            &mut e,
            prev,
            [1150, 0, 300],
            0,
            &[low],
            0
        ));
        assert_ne!(e.pos, prev);
    }

    #[test]
    fn sca_volume_offset_rotates_with_the_yaw() {
        // Forest's record volume centres 0x258 along local +X and -0xC8 along
        // local Z. At yaw 0x400 local +X faces -Z and local +Z faces +X, so
        // the rotated centre is (-0xC8, -0xB4, -0x258).
        let volume = data::sca_volume(0x26).unwrap();
        let hit = ScaHit {
            offset: volume.offset,
            half_height: volume.half_height,
            radius: volume.radius,
            world_bias: [0; 3],
        };
        assert_eq!(hit.world_offset(0), [0x257, -0xB4, -0xC7]);
        assert_eq!(hit.world_offset(0x400), [-0xC7, -0xB4, -0x258]);
    }

    #[test]
    fn the_sca_hit_bias_shifts_the_volume_centre_after_the_rotation() {
        let mut e = entity([0, 0, 0], 0, 0);
        e.id = 0x0F;
        e.sca_radius = 400;
        e.sca_half_height = 100;
        e.sca_offset = [100, 0, 0];
        e.sca_hit_delta = [10, 5, -20];
        let hit = ScaHit::of(&e);
        // The record offset rotates first (local +X to -Z at yaw 0x400), then
        // the world bias is added unrotated.
        assert_eq!(hit.world_offset(0x400), [10, 5, -120]);

        // The bias makes a volume that just missed the player overlap it.
        let prev = e.pos;
        e.sca_offset = [0, 0, 0];
        e.sca_hit_delta = [0, 0, 0];
        let other = ScaHit {
            offset: [0, 0, 0],
            half_height: 100,
            radius: 100,
            world_bias: [0; 3],
        };
        assert!(!separate_from_character(
            &mut e,
            prev,
            [700, 0, 0],
            0,
            &[other],
            0
        ));
        e.sca_hit_delta = [600, 0, 0];
        assert!(separate_from_character(
            &mut e,
            prev,
            [700, 0, 0],
            0,
            &[other],
            0
        ));
    }

    #[test]
    fn sca_hit_of_reads_characters_from_the_table_and_monsters_live() {
        // A character resolves through the per-character data table, with
        // the radius from the live field.
        let mut character = Entity {
            id: 0x26,
            sca_radius: 123,
            ..Entity::default()
        };
        let hit = ScaHit::of(&character);
        let volume = data::sca_volume(0x26).unwrap();
        assert_eq!(hit.offset, volume.offset);
        assert_eq!(hit.half_height, volume.half_height);
        assert_eq!(hit.radius, 123);

        // A monster reads the fields `e:set_sca` wrote - Plant 42's roots
        // record: centred, 0x1770 half-height, 0x07D0 radius.
        character.id = 0x0e;
        character.sca_radius = 0x07D0;
        character.sca_half_height = 0x1770;
        character.sca_offset = [0, 0, 0];
        let hit = ScaHit::of(&character);
        assert_eq!(hit.offset, [0, 0, 0]);
        assert_eq!(hit.half_height, 0x1770);
        assert_eq!(hit.radius, 0x07D0);
    }

    #[test]
    fn sca_resolve_is_un_collided_and_the_room_pass_clears_it() {
        let mut room = corridor();
        // A wall sits just east of the character, on the side the player push
        // drives it toward.
        room.collision.quadrants[0].push(CollisionRect {
            x_max: 1400,
            z_max: 700,
            x_min: 1200,
            z_min: 0,
            kind: 1,
            flags: 0,
        });
        let mut e = entity([1050, 0, 300], 0, 0);
        e.sca_radius = 100;
        let prev = e.pos;
        // The player sits west of the character, so the push drives it east
        // into the wall.
        assert!(separate_from_player(&mut e, prev, [950, 0, 300], 0, 100, 0));
        assert!(
            player::position_blocked(&room, e.pos, 100, e.collision_flags),
            "the SCA resolve itself does not consult the room: {:?}",
            e.pos
        );
        // The end-of-frame tail then pushes it clear.
        let resolved =
            player::resolve_collision(&room.collision, prev, e.pos, 100, e.collision_flags);
        assert!(
            !player::position_blocked(&room, resolved, 100, e.collision_flags),
            "the room pass clears the pushed point: {resolved:?}"
        );
    }

    #[test]
    fn separation_skips_state_4_and_deactivated_pairs() {
        // entB (the character) in state 4 is the eating/headless skip.
        let mut eating = entity([1050, 0, 300], 0, 0);
        eating.sca_radius = 100;
        eating.set_state(4);
        let prev = eating.pos;
        assert!(!separate_from_player(
            &mut eating,
            prev,
            [1150, 0, 300],
            0,
            100,
            0
        ));
        assert_eq!(eating.pos, prev, "state 4 is skipped");

        // Status bit 1 on the character skips the pair.
        let mut deactivated = entity([1050, 0, 300], 0, 0);
        deactivated.sca_radius = 100;
        deactivated.status_flags |= 2;
        let prev = deactivated.pos;
        assert!(!separate_from_player(
            &mut deactivated,
            prev,
            [1150, 0, 300],
            0,
            100,
            0
        ));
        assert_eq!(
            deactivated.pos, prev,
            "the character's deactivation bit is skipped"
        );

        // Status bit 1 on the player skips the pair too.
        let mut player_off = entity([1050, 0, 300], 0, 0);
        player_off.sca_radius = 100;
        let prev = player_off.pos;
        assert!(!separate_from_player(
            &mut player_off,
            prev,
            [1150, 0, 300],
            0,
            100,
            2
        ));
        assert_eq!(
            player_off.pos, prev,
            "the player's deactivation bit is skipped"
        );
    }

    #[test]
    fn separation_resolves_character_against_character() {
        let mut e = entity([1050, 0, 300], 0, 0);
        e.sca_radius = 100;
        let prev = e.pos;
        let other = ScaHit::character(0x27, 100);
        assert!(separate_from_character(
            &mut e,
            prev,
            [1150, 0, 300],
            0,
            &[other],
            0
        ));
        let dist = xz_distance_to(&e, [1150, 0, 300]);
        assert!(dist > 100, "pushed out of the other character: {dist}");
    }

    #[test]
    fn previous_position_breaks_a_degenerate_crossing() {
        // The character's pre-move position was west of the other volume and
        // its current position east of it: the pair overlaps in XZ but not in
        // the pre-move Y, so the push is rewritten by the radius to the near
        // side instead of the far one.
        let mut e = entity([1100, 0, 0], 0, 0);
        e.sca_radius = 100;
        // The pre-move Y is far above the other volume, so the pair did not
        // overlap vertically before the move; the current Y does overlap.
        let prev = [900, 4000, 0];
        let other = ScaHit::character(0x27, 100);
        assert!(separate_from_character(
            &mut e,
            prev,
            [1000, 0, 0],
            0,
            &[other],
            0
        ));
        assert!(e.pos[0] <= 1000, "kept west of the crossed volume");
    }

    #[test]
    fn state9_tail_resolves_the_character_against_other_characters() {
        let room = corridor();
        let mut game = GameState::default();
        // The player sits outside behaviour 0's swap ring, so only the
        // character pair resolve can move the slot.
        game.entities[0].pos = [2500, 0, 2500];
        game.entities[1] = Entity {
            id: 0x23,
            pos: [1100, 0, 500],
            action_behavior: 0,
            sca_radius: 100,
            ..Entity::default()
        };
        game.entities[1].set_active(true);
        // Another active character overlapping slot 1 from the east; the tail
        // must push slot 1 west, away from it.
        game.entities[2] = Entity {
            id: 0x27,
            pos: [1200, 0, 500],
            sca_radius: 100,
            ..Entity::default()
        };
        game.entities[2].set_active(true);
        update(&mut game, 1, &room, &[]);
        let entity = game.entities[1];
        assert!(
            entity.pos[0] < 1100,
            "the pair resolve pushed the character west: {:?}",
            entity.pos
        );
        assert_eq!(entity.pos[2], 500);
    }

    #[test]
    fn spider_distance_carries_the_negative_x_unit() {
        let mut e = entity([100, 0, 100], 0, 0);
        // dx = +40, dz = -30: |dz| + |dx| = 70.
        assert_eq!(spider_distance(&e, [140, 0, 70]), 70);
        // The sign masks cancel: dx = -40, dz = +30 is 70 too.
        assert_eq!(spider_distance(&e, [60, 0, 130]), 70);
        e.pos = [0, 0, 0];
        assert_eq!(spider_distance(&e, [0, 0, 0]), 0);
        assert_eq!(spider_distance(&e, [-5, 0, 0]), 5);
    }

    #[test]
    fn ballistic_step_walks_the_yaw_and_wraps_the_vy() {
        let mut e = entity([0, 0, 0], 0, 0);
        // fwd 100 along yaw 0 is +X (99 after the 14-bit trig truncation);
        // vy = 0 * gravity + 100 -> Y -= 100.
        assert_eq!(entity_ballistic_step(&mut e, 100, 100, -30, 0), 0);
        assert_eq!(e.pos, [99, -100, 0]);
        assert_eq!(e.death_timer, 1);
        // Next tick: vy = 1 * -30 + 100 = 70 -> Y -= 70.
        assert_eq!(entity_ballistic_step(&mut e, 100, 100, -30, 0), 0);
        assert_eq!(e.pos, [198, -170, 0]);
        assert_eq!(e.death_timer, 2);
        // Ground clamp: the entity is below the floor, so the step snaps Y to
        // the ground and reports the velocity without ticking.
        e.pos[1] = 25;
        e.death_timer = 3;
        // vy = 3 * -30 + 10 = -80 -> Y = 25 + 80 = 105 > 0 -> clamp, return.
        assert_eq!(
            entity_ballistic_step(&mut e, 0, 10, -30, 0),
            (-80i16) as u16
        );
        assert_eq!(e.pos[1], 0);
        assert_eq!(e.death_timer, 3, "landing does not tick");
    }

    #[test]
    fn ballistic_vy_multiply_wraps_16_bits() {
        let mut e = entity([0, 0, 0], 0, 0);
        e.death_timer = 2;
        // 2 * 0x8000 wraps to 0 in 16 bits; + 5 = 5.
        assert_eq!(entity_ballistic_step(&mut e, 0, 5, i16::MIN, 0), 0);
        assert_eq!(e.pos[1], -5);
    }

    /// A synthetic 20-joint spider model whose joint 3 and 4 form a leg chain
    /// under joint 0, with nonzero bind offsets so the composed tip has a real
    /// position.
    fn leg_skeleton(children_of_three: bool) -> crate::model::Skeleton {
        use crate::model::Skeleton;
        let mut relative = vec![[0i16, 0, 0]; 20];
        let children = if children_of_three {
            relative[3] = [10, 0, 0];
            relative[4] = [20, 0, 0];
            (0..20)
                .map(|index| match index {
                    0 => vec![3],
                    3 => vec![4],
                    _ => vec![],
                })
                .collect()
        } else {
            relative[5] = [10, 0, 0];
            relative[4] = [20, 0, 0];
            (0..20)
                .map(|index| match index {
                    0 => vec![5],
                    5 => vec![4],
                    _ => vec![],
                })
                .collect()
        };
        Skeleton { relative, children }
    }

    fn leg_anim(skeleton: crate::model::Skeleton) -> crate::enemy::EntityAnim {
        use crate::model::Keyframe;
        use std::sync::Arc;
        let keyframes = Arc::new(vec![Keyframe {
            offset: [0, 0, 0],
            rotations: vec![[0, 0, 0]; 20],
        }]);
        crate::enemy::EntityAnim {
            keyframes: Some(keyframes),
            skeleton: Some(Arc::new(skeleton)),
            ..Default::default()
        }
    }

    #[test]
    fn leg_reach_measures_the_tip_stride_and_gates_on_armed_joints() {
        use crate::model::{Clip, ClipFrame};
        let clips = vec![Clip {
            frames: vec![ClipFrame {
                keyframe: 0,
                timing: 1,
            }],
        }];
        let anim = leg_anim(leg_skeleton(true));
        let mut e = entity([100, 0, 200], 0, 0);
        e.animation_id = 0;
        let stored = anim.joint_worlds(&e, &clips);

        // The full hierarchy composes the same chain: zero stride.
        leg_reach(&mut e, &anim, &clips, &stored, LegChain::Spinner, 0, 0);
        assert_eq!(e.move_speed_current, 0);

        // A stale stored tip 50 units east reads as a 50-unit stride.
        let mut stale = stored.clone();
        stale[4].t[0] += 50;
        leg_reach(&mut e, &anim, &clips, &stale, LegChain::Spinner, 0, 0);
        assert_eq!(e.move_speed_current, 50);

        // An armed leg joint zeroes the stride.
        e.joint_armed = 1 << 3;
        leg_reach(&mut e, &anim, &clips, &stale, LegChain::Spinner, 0, 0);
        assert_eq!(e.move_speed_current, 0);
    }

    #[test]
    fn tiger_leg_reach_composes_the_other_chain() {
        use crate::model::{Clip, ClipFrame};
        let clips = vec![Clip {
            frames: vec![ClipFrame {
                keyframe: 0,
                timing: 1,
            }],
        }];
        let anim = leg_anim(leg_skeleton(false));
        let mut e = entity([100, 0, 200], 0, 0);
        e.animation_id = 0;
        let stored = anim.joint_worlds(&e, &clips);
        leg_reach(&mut e, &anim, &clips, &stored, LegChain::Tiger, 0, 0);
        assert_eq!(e.move_speed_current, 0);
        // The Tiger gates on the tip joint itself.
        e.joint_armed = 1 << 4;
        leg_reach(&mut e, &anim, &clips, &stored, LegChain::Tiger, 0, 0);
        assert_eq!(e.move_speed_current, 0);
    }

    #[test]
    fn a_two_volume_profile_pushes_on_either_box() {
        let other = ScaHit {
            offset: [0, 0, 0],
            half_height: 180,
            radius: 100,
            world_bias: [0; 3],
        };
        let make = || {
            let mut e = entity([0, 0, 0], 0, 0);
            e.sca_radius = 2000;
            e.sca_half_height = 180;
            e.sca_offset = [0, -180, 1000];
            e.sca2 = Some(crate::game::ScaVolume {
                radius: 2000,
                half_height: 180,
                offset: [0, -180, -1000],
            });
            e
        };
        let (volumes, count) = entity_sca_volumes(&make());
        assert_eq!(count, 2);
        assert_eq!(volumes[0].offset, [0, -180, 1000]);
        assert_eq!(volumes[1].offset, [0, -180, -1000]);

        // The other sits past the front box: the entity is pushed back.
        let mut e = make();
        let prev = e.pos;
        assert!(separate_from_character(
            &mut e,
            prev,
            [0, 0, 1500],
            0,
            &[other],
            0
        ));
        assert!(e.pos[2] < 0, "pushed away from the front box");
        // And past the rear box: pushed the other way.
        let mut e = make();
        let prev = e.pos;
        assert!(separate_from_character(
            &mut e,
            prev,
            [0, 0, -1500],
            0,
            &[other],
            0
        ));
        assert!(e.pos[2] > 0, "pushed away from the rear box");
    }

    #[test]
    fn angular_view_and_distance_covers_wedge_and_range() {
        let e = entity([0, 0, 0], 0, 0);
        // Straight ahead, inside the 700 half-angle and the 1500 range.
        assert_eq!(angular_view_and_distance(&e, 700, 1500, [1000, 0, 0]), 1);
        // A shallow target is still inside the wedge (the range gate is the
        // folded Manhattan sum, so 45 degrees at 1414 Euclidean is out).
        assert_eq!(angular_view_and_distance(&e, 700, 1500, [1000, 0, -300]), 1);
        // Directly behind is outside the wedge.
        assert_eq!(angular_view_and_distance(&e, 700, 1500, [-1000, 0, 0]), 0);
        // Beyond the range: the folded Manhattan sum exceeds 1500.
        assert_eq!(angular_view_and_distance(&e, 700, 1500, [2000, 0, 0]), 0);
        // Height never participates.
        assert_eq!(angular_view_and_distance(&e, 700, 1500, [1000, 9000, 0]), 1);
    }

    #[test]
    fn line_of_sight_sees_through_and_stops_at_walls() {
        let e = entity([100, 0, 100], 0, 0);
        // No collision records: the sight line is clear.
        assert_eq!(
            check_line_of_sight(&RoomState::default(), &e, [1000, 0, 100]),
            0
        );

        // A fully blocking record between the entity and the target.
        let mut room = RoomState::default();
        room.collision.quadrants[0].push(CollisionRect {
            x_max: 600,
            z_max: 300,
            x_min: 500,
            z_min: 0,
            kind: 1,
            flags: 0x300,
        });
        assert_eq!(check_line_of_sight(&room, &e, [1000, 0, 100]), 1);
        // The same record behind the entity does not block the forward ray.
        assert_eq!(check_line_of_sight(&room, &e, [-1000, 0, 100]), 0);
    }

    #[test]
    fn wander_turn_sticks_and_steers_at_the_waypoint() {
        let mut e = entity([0, 0, 0], 0, 0);
        e.move_speed_current = 45;
        e.player_pos_x = 0;
        e.player_pos_z = -1000; // the waypoint at +0x400

        // A long frame step clears the flags and steps toward the waypoint.
        assert_eq!(update_wander_turn(&mut e, 10_000, 24, 60, 0), 0);
        assert_eq!(e.dir_control_flags, 0);
        assert_eq!(e.angle, 24);

        // A short step past the turn limit arms the latch from the frame seed.
        e.angle = 0;
        e.set_zombie_unk_179(60);
        update_wander_turn(&mut e, 1, 24, 60, 0x40);
        assert_eq!(e.dir_control_flags, 0xC0, "the seed's 0x40 bit and 0x80");
        assert_eq!(e.zombie_unk_179(), 10, "turn_limit / 6");
        assert_eq!(e.angle, 24, "the arming frame still steers");

        // The latched path owns the yaw: bit 0x40 selects the negative step.
        assert_eq!(update_wander_turn(&mut e, 10_000, 24, 60, 0), 1);
        assert_eq!(e.angle, 0);
        assert_eq!(e.zombie_unk_179(), 9, "the latch counts down");
    }

    #[test]
    fn two_point_probe_pushes_and_commits_the_body() {
        // A rectangle in front of the prone body's +600 end.
        let mut room = RoomState::default();
        room.collision.quadrants[0].push(CollisionRect {
            x_max: 900,
            z_max: 200,
            x_min: 500,
            z_min: 0,
            kind: 3,
            flags: 0,
        });
        let mut e = entity([0, 0, 0], 0, 0);
        let bits = two_point_probe(&room.collision, &mut e, [-600, 0], [600, 0]);
        assert_eq!(bits, 0, "no blocking flag bits");
        assert_ne!(e.pos, [0, 0, 0], "the push moved the body");
        assert_eq!(e.saved_pos, Some(e.pos), "the accepted position committed");
        assert_eq!(e.mirror_angle, e.angle);

        // A deactivated entity never probes.
        let mut e = entity([0, 0, 0], 0, 0);
        e.status_flags = 0x04;
        assert_eq!(
            two_point_probe(&room.collision, &mut e, [-600, 0], [600, 0]),
            0
        );
        assert_eq!(e.pos, [0, 0, 0]);
    }

    #[test]
    fn blood_splatter_falls_to_the_resting_height() {
        let mut game = GameState::default();
        game.entities[1].joint_blood[2] = crate::game::JointBlood::default();
        blood_splatter(&mut game, &RoomState::default(), 1, 2, 6);
        let scratch = game.entities[1].joint_blood[2];
        assert_eq!(scratch.counter, 1, "the gravity counter advanced");
        assert_eq!(scratch.flags, 1, "the resting frame bumped the flags");
        assert_eq!(scratch.vel_x, 0, "the resting velocity is clamped to <= 0");
        // The joint's world Y is clamped to the exact resting height.
        assert_eq!(game.joint_worlds.len(), crate::game::ENTITY_COUNT);
    }

    #[test]
    fn body_part_speed_zeroes_without_a_skeleton() {
        let mut game = GameState::default();
        game.entities[1].move_speed_current = 999;
        body_part_speed(&mut game, &[], 1, 1);
        assert_eq!(game.entities[1].move_speed_current, 0);
    }

    #[test]
    fn zone_path_update_writes_the_direct_target() {
        use crate::state::WalkZone;
        let mut room = RoomState::default();
        room.walk_zones.push(WalkZone {
            x1: 0,
            z1: 0,
            x2: 4000,
            z2: 4000,
            field_08: 0,
            flags: 0,
        });
        let mut e = entity([100, 0, 100], 0, 0);
        zone_path_update(&room, &mut e, 500, 600);
        assert_eq!(e.player_pos_x, 500);
        assert_eq!(e.player_pos_z, 600);

        // An unreachable target (outside every zone) writes nothing.
        let mut e = entity([100, 0, 100], 0, 0);
        e.player_pos_x = 77;
        zone_path_update(&room, &mut e, 9000, 9000);
        assert_eq!(e.player_pos_x, 77);
    }
    /// A room with one blocking rectangle ahead of the origin, for the
    /// hound's forward probes.
    fn probe_wall_room() -> RoomState {
        RoomState {
            collision: Collision {
                cell_x: 0,
                cell_z: 0,
                quadrants: std::array::from_fn(|_| {
                    vec![CollisionRect {
                        x_max: 4000,
                        z_max: 4000,
                        x_min: 300,
                        z_min: 0,
                        kind: 1,
                        flags: 0x300,
                    }]
                }),
            },
            ..RoomState::default()
        }
    }

    #[test]
    fn probe_ahead_steps_out_and_writes_the_shared_scratch() {
        // Clear room: the probe point is left in the scratch vector and the
        // step is rolled back off the entity.
        let room = RoomState::default();
        let mut game = GameState::default();
        game.entities[1].pos = [40, 7, -60];
        game.entities[1].saved_pos = Some([40, 7, -60]);
        game.entities[1].sca_radius = 400;
        assert!(!probe_ahead(&room, &mut game, 1, 500, 400));
        assert_eq!(game.entities[1].pos, [40, 7, -60], "the step rolled back");
        assert_eq!(game.scratch_vec, [539, 7, -60], "the probe point");

        // A wall in the probe's path reports blocked.
        let room = probe_wall_room();
        let mut game = GameState::default();
        game.entities[1].pos = [0, 0, 0];
        game.entities[1].saved_pos = Some([0, 0, 0]);
        assert!(probe_ahead(&room, &mut game, 1, 500, 400));
    }

    #[test]
    fn probe_ahead_honours_the_airborne_early_out() {
        let room = probe_wall_room();
        let mut game = GameState::default();
        game.entities[1].pos = [0, 0, 0];
        game.entities[1].saved_pos = Some([0, 0, 0]);
        game.entities[1].status_flags |= 0x04;
        assert!(!probe_ahead(&room, &mut game, 1, 500, 400));
        assert_eq!(game.scratch_vec, [0, 0, 0], "the scratch is untouched");
    }

    #[test]
    fn probe_turn_scales_the_speed_and_restores_everything() {
        let room = RoomState::default();
        let mut game = GameState::default();
        game.entities[1].pos = [0, 0, 0];
        game.entities[1].saved_pos = Some([11, 0, 22]);
        game.entities[1].angle = 0;
        game.entities[1].move_speed_current = 100;

        assert!(!probe_turn(&room, &mut game, 1, 0x400, 1));
        assert_eq!(
            game.entities[1].pos,
            [0, 0, 0],
            "the probe step rolled back"
        );
        assert_eq!(game.entities[1].saved_pos, Some([11, 0, 22]));
        assert_eq!(game.entities[1].move_speed_current, 100);

        // The scratch carries the probe point: the turn step first moved the
        // entity 100 along angle 0x400 (-Z), then the probe stepped 500 +X.
        assert_eq!(game.scratch_vec, [499, 0, -100]);
    }

    #[test]
    fn nudge_spawn_adds_the_rotated_step_to_the_position_words() {
        let mut game = GameState::default();
        game.entities[1].pos = [100, 0, 200];
        game.entities[1].saved_pos = Some([100, 0, 200]);
        game.entities[1].angle = 0;
        nudge_spawn(&mut game, 1, 500);
        assert_eq!(game.entities[1].saved_pos, Some([599, 0, 200]));
        assert_eq!(game.scratch_vec, [499, 0, 0]);

        // A three-quarter turn walks the step along +Z.
        game.entities[1].angle = 0xC00;
        game.entities[1].saved_pos = Some([0, 0, 0]);
        nudge_spawn(&mut game, 1, 500);
        assert_eq!(game.entities[1].saved_pos, Some([0, 0, 499]));
    }

    #[test]
    fn head_track_integrates_the_swerve_and_clamps() {
        use crate::anim::Mat4x3;
        let mut game = GameState::default();
        game.entities[0].pos = [0, 0, -100];
        game.entities[1].pos = [0, 0, 0];
        game.entities[1].angle = 0;
        let identity = Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        };
        game.joint_worlds[1] = vec![identity; 5];
        let skeleton = crate::model::Skeleton {
            relative: vec![[0, 0, 0]; 5],
            ..crate::model::Skeleton::default()
        };
        game.entity_anims[1].skeleton = Some(std::sync::Arc::new(skeleton));

        // The player sits at +Z of the head: the turn step walks the swerve
        // toward -Z (positive angle), well inside the clamp.
        assert!(!head_track(&mut game, 1, &[]));
        assert!(game.entities[1].cb_swerve() > 0);

        // Push the swerve past the low clamp: it pins at -0x100 and reports
        // the clamp bit.
        game.entities[0].pos = [0, 0, 100];
        game.entities[1].set_cb_swerve(-0x100);
        game.entities[1].angle = 0;
        assert!(head_track(&mut game, 1, &[]));
        assert_eq!(game.entities[1].cb_swerve(), -0x100);
    }

    #[test]
    fn hunter_wander_turn_uses_the_room_hit_and_path_latch_fields() {
        // The flags' 0x80 path owns the turn and steps the yaw by the angle.
        let mut entity = Entity {
            move_speed_current: 40,
            player_pos_x: 500,
            player_pos_z: 0,
            ..Entity::default()
        };
        entity.move_speed_byte = 0x80;
        entity.angle_turn_delta = 3;
        assert_eq!(hunter_wander_turn(&mut entity, 100, 0x18, 0x3C, 0), 1);
        assert_eq!(entity.angle, 0x18);
        assert_eq!(entity.angle_turn_delta, 2, "the counter decrements");
        assert_eq!(
            entity.dir_control_flags, 0,
            "the zombie's byte is untouched"
        );
        assert_eq!(entity.zombie_unk_179(), 0, "and so is its counter");

        // The stuck path arms the room-hit latch from the frame seed and
        // seeds the counter from the turn limit.
        let mut entity = Entity {
            move_speed_current: 300,
            ..Entity::default()
        };
        entity.angle_turn_delta = 0x3C;
        assert_eq!(hunter_wander_turn(&mut entity, 1, 0x18, 0x3C, 0x40), 0);
        assert_eq!(entity.move_speed_byte, 0xC0, "the seed bit lands in 0x40");
        assert_eq!(entity.angle_turn_delta, 0x3C / 6);
    }

    /// The identity pose and one-frame clips the hunter track/recenter tests
    /// compose through.
    fn hunter_pose_game() -> (GameState, Vec<crate::model::Clip>) {
        use crate::model::{Clip, ClipFrame, Keyframe, Skeleton};
        use std::sync::Arc;
        let clips = vec![Clip {
            frames: vec![ClipFrame {
                keyframe: 0,
                timing: 1,
            }],
        }];
        let mut game = GameState::default();
        game.entities[1].pos = [100, 0, 200];
        game.entities[1].saved_pos = Some(game.entities[1].pos);
        game.entity_anims[1].keyframes = Some(Arc::new(vec![Keyframe {
            offset: [0, 0, 0],
            rotations: vec![[0, 0, 0]; 32],
        }]));
        game.entity_anims[1].skeleton = Some(Arc::new(Skeleton {
            relative: vec![[0, 0, 0]; 32],
            children: vec![vec![]; 32],
        }));
        (game, clips)
    }

    #[test]
    fn hunter_recenter_pivots_the_body_on_the_chain_tip() {
        let (mut game, clips) = hunter_pose_game();
        let identity = crate::anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        };
        game.joint_worlds[1] = vec![identity; 16];
        game.joint_worlds[1][12].t = [130, 50, 260];
        hunter_recenter_on_joint(&mut game, &clips, 1, 0);
        assert_eq!(
            game.entities[1].pos,
            [130, 0, 260],
            "the X/Z shift lands on the tip joint"
        );
    }

    #[test]
    fn hunter_track_joint_composes_the_chain_and_the_mouth_offset() {
        let (mut game, clips) = hunter_pose_game();
        let identity = crate::anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        };
        game.joint_worlds[1] = vec![identity; 16];
        game.joint_worlds[1][6].t = [100, 0, 200];
        let tracked = hunter_track_joint(&mut game, &clips, 1, 0, 6, false).unwrap();
        let (rx, rz) = player::rotate_xz((11 - 6) * 600, 200, 0);
        assert_eq!(tracked, [100 + rx, 0, 200 + rz, 0]);
        assert_eq!(game.entities[0].joint_track, tracked);

        // The scripted chain walks the fixed joints and writes slot 1.
        let tracked = hunter_track_joint(&mut game, &clips, 1, 1, 6, true).unwrap();
        assert_eq!(tracked, [100 + rx, 0, 200 + rz, 0]);
        assert_eq!(game.entities[1].joint_track, tracked);
    }

    #[test]
    fn tyrant_wander_turn_steers_through_its_own_byte_pair() {
        let mut e = entity([0, 0, 0], 0, 0);
        e.move_speed_current = 100;
        e.player_pos_x = 5000;
        e.player_pos_z = 0;
        // A stalled mover trips the counter; the 0x80 latch then turns it by
        // the step and returns 1 until the counter expires.
        assert_eq!(tyrant_wander_turn(&mut e, 0, 0x28, 0x28, 0), 0);
        assert_eq!(e.behavior_step, 1);
        assert_eq!(e.attacking_direction, 0);
        e.attacking_direction = 0x80 | 0x40;
        assert_eq!(tyrant_wander_turn(&mut e, 0, 0x28, 0x28, 0), 1);
        assert_eq!(e.angle, 0u16.wrapping_sub(0x28), "the 0x40 bit flips it");
        e.angle = 0;
        e.attacking_direction = 0x80;
        assert_eq!(tyrant_wander_turn(&mut e, 0, 0x28, 0x28, 0), 1);
        assert_eq!(e.angle, 0x28, "without 0x40 the step turns");
    }
}
