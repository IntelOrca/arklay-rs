//! Native movement helpers shared by the scripted (state 8) and follow
//! (state 9) drivers.
//!
//! The original's state-8 handlers steer with `turn_toward_target` /
//! `entity_rotate_toward_target`, trim their speed on the animation's footfall
//! frames with `entity_apply_walk_speed`, and move with `Add_speedXZ`; state 9
//! reuses the same helpers. The engine resolves every move against the room
//! collision with the entity's own radius and rolls a blocked move back, so a
//! scripted walk never pushes a character through geometry.
//!
//! State 9 adds the walk-zone graph the room data provides: [`walk_zone_find`]
//! locates the zone containing a point, [`walk_zone_shared_edge`] names the
//! crossing between two zones, and [`zone_path_find`] walks the zone adjacency
//! graph from the entity to the player. [`crossing_heading`] clamps the
//! crossing into the shared corridor with two wall probes, and the four
//! behaviours in [`update`] walk, run, pace or retreat on a distance ring.
//! A character is pushed out of the player's radius and out of every other
//! active character's radius after it moves, exactly like the original's
//! `ResolveEntityScaCollision` + `HandleEnemyPlayerCollisions` tail; the other
//! entity is never moved and no damage is transferred.

use std::collections::VecDeque;

use crate::game::{Entity, EntitySound, GameState};
use crate::model::Clip;
use crate::player;
use crate::sfx;
use crate::state::RoomState;

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

/// `Add_speedXZ`: move `distance` units along the entity's yaw plus `offset`,
/// resolved against the room collision with the entity's radius. A blocked
/// move rolls back to the pre-move position.
pub fn advance_xz(room: &RoomState, entity: &mut Entity, offset: u16, distance: i16) {
    try_advance_xz(room, entity, offset, distance);
}

/// [`advance_xz`] reporting whether the move was committed. The idle walk-01
/// behaviour uses the result as its collision probe: the original commits the
/// move and rolls it back on a hit, so "did not move" is the hit.
pub fn try_advance_xz(room: &RoomState, entity: &mut Entity, offset: u16, distance: i16) -> bool {
    let (dx, dz) = player::rotate_speed(entity.angle, offset, i32::from(distance));
    let proposed = [entity.pos[0] + dx, entity.pos[1], entity.pos[2] + dz];
    let radius = i32::from(entity.sca_radius);
    if player::position_blocked(room, proposed, radius) {
        return false;
    }
    entity.pos = proposed;
    true
}

/// The state-8/9 footstep callback (`PlayEntitySnd`): resolve the entity's
/// floor zone sound for `sound_type` and queue it at the entity's position.
/// The engine's mixer consumes the queue; a room with no matching floor zone
/// or sound name stays silent.
pub fn footstep(sounds: &mut Vec<EntitySound>, room: &RoomState, entity: &Entity, sound_type: u8) {
    if let Some(name) = sfx::footstep_sound(room, entity.pos, sound_type, false) {
        sounds.push(EntitySound {
            name,
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

/// `zone_path_find`: a bidirectional BFS over the zone adjacency bitmask
/// (record `+10`), starting from the entity's zone and meeting a search from
/// the player's zone. Returns the next zone to walk into and the shared-edge
/// crossing; the same zone is [`ZonePath::Direct`] and a point outside the
/// grid or a disconnected target is [`ZonePath::Unreachable`].
pub fn zone_path_find(room: &RoomState, from: [i32; 3], to: [i32; 3]) -> ZonePath {
    let count = room.walk_zones.len();
    let Some(start) = walk_zone_find(room, from[0], from[2]) else {
        return ZonePath::Unreachable;
    };
    let Some(target) = walk_zone_find(room, to[0], to[2]) else {
        return ZonePath::Unreachable;
    };
    if start == target {
        return ZonePath::Direct { zone: start };
    }

    let mut seen_from = vec![false; count];
    let mut prev_from: Vec<Option<u8>> = vec![None; count];
    let mut seen_to = vec![false; count];
    let mut prev_to: Vec<Option<u8>> = vec![None; count];
    let mut queue_from = VecDeque::from([start]);
    let mut queue_to = VecDeque::from([target]);
    seen_from[usize::from(start)] = true;
    seen_to[usize::from(target)] = true;

    let mut meet = None;
    while !queue_from.is_empty() && !queue_to.is_empty() {
        for _ in 0..queue_from.len() {
            let zone = queue_from.pop_front().expect("queue is non-empty");
            for next in zone_neighbors(room, zone) {
                if seen_from[usize::from(next)] {
                    continue;
                }
                seen_from[usize::from(next)] = true;
                prev_from[usize::from(next)] = Some(zone);
                queue_from.push_back(next);
                if seen_to[usize::from(next)] {
                    meet = Some(next);
                    break;
                }
            }
            if meet.is_some() {
                break;
            }
        }
        if meet.is_some() {
            break;
        }
        for _ in 0..queue_to.len() {
            let zone = queue_to.pop_front().expect("queue is non-empty");
            for next in zone_neighbors(room, zone) {
                if seen_to[usize::from(next)] {
                    continue;
                }
                seen_to[usize::from(next)] = true;
                prev_to[usize::from(next)] = Some(zone);
                queue_to.push_back(next);
                if seen_from[usize::from(next)] {
                    meet = Some(next);
                    break;
                }
            }
            if meet.is_some() {
                break;
            }
        }
    }

    let Some(meet) = meet else {
        return ZonePath::Unreachable;
    };
    let next = if meet == start {
        // The target-side search reached the start: the first step is the
        // start's predecessor on that side.
        match prev_to[usize::from(start)] {
            Some(zone) => zone,
            None => return ZonePath::Unreachable,
        }
    } else {
        let mut node = meet;
        loop {
            match prev_from[usize::from(node)] {
                Some(previous) if previous == start => break node,
                Some(previous) => node = previous,
                None => return ZonePath::Unreachable,
            }
        }
    };
    let (_, crossing) = walk_zone_shared_edge(room, start, next);
    ZonePath::Cross {
        from: start,
        next,
        crossing,
    }
}

/// The zones adjacent to `zone`: every index whose bit is set in the zone's
/// adjacency word. The word is 16-bit, so only zones 0-15 can ever be named,
/// exactly like the original's `1 << (zone & 0x1F)` on a short.
fn zone_neighbors(room: &RoomState, zone: u8) -> Vec<u8> {
    let Some(entry) = room.walk_zones.get(usize::from(zone)) else {
        return Vec::new();
    };
    let flags = u32::from(entry.flags);
    (0..room.walk_zones.len())
        .filter(|index| flags & (1u32 << (index & 0x1F)) != 0)
        .map(|index| index as u8)
        .collect()
}

/// The original's corridor test: whether `(x, z)` sits inside the overlapping
/// span of the two zones' shared edge, with both span ends nudged inward when
/// a wall probe hits them. `flag` is [`walk_zone_shared_edge`]'s orientation
/// (0 = shared X edge, span in Z; 1 = shared Z edge, span in X). All span
/// arithmetic is unsigned-short, like the original.
fn corridor_open(
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
        |first: i32, second: i32| player::position_blocked(room, [first, 0, second], radius);
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
    let blocked = |x: i32, z: i32| player::position_blocked(room, [x, 0, z], radius);
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
                && !player::position_blocked(room, [pos_x, entity_pos[1], pos_z], radius)
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
fn heading_within(angle: u16, heading: u16, half: i32) -> bool {
    let delta = i32::from(heading) - i32::from(angle) + half;
    (0..=half * 2).contains(&delta)
}

/// `npc_walk_turn_toward_heading`: step the yaw toward the heading stored in
/// `reaction_timer`. Odd entity ids turn eight units faster. While the angular
/// difference is larger than the step the yaw moves by it (sign chosen by bit
/// 11 of the difference), otherwise it snaps to the heading.
fn turn_toward_heading(entity: &mut Entity, heading: i16, step: i16) {
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
fn reset_lookat(entity: &mut Entity, seed: u16) {
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
fn set_lookat_target(entity: &mut Entity, x: i32, z: i32, seed: u16) {
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
fn wander_lookat(entity: &mut Entity, param: u8, seed: u16) {
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
        advance_xz(room, entity, 0, entity.move_speed_current as i16);
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
    advance_xz(room, entity, 0, move_speed);
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
fn behavior_03(entity: &mut Entity, room: &RoomState, player_pos: [i32; 3], seed: u16) {
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
        advance_xz(room, entity, 0, -0x3C);
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
    advance_xz(room, entity, 0, entity.move_speed_current as i16);
    reset_lookat(entity, seed);
}

/// `npc_walk_footstep_sound`: the state-9 footfall frames. Animations 3 and 7
/// step on frames 8 and 0x16 with sound 0; animation 8 on frames 0 and 0xA
/// with sound 1.
fn walk_footstep_sound(sounds: &mut Vec<EntitySound>, room: &RoomState, entity: &Entity) {
    let sound_type = match (entity.animation_id, entity.animation_frame_id) {
        (3 | 7, 8 | 0x16) => 0,
        (8, 0 | 0x0A) => 1,
        _ => return,
    };
    footstep(sounds, room, entity, sound_type);
}

/// `ResolveEntityScaCollision` against the player: push the character out of
/// the player's radius along the separation line. The player is never moved
/// and no damage is transferred. A push that would put the character inside a
/// wall is retried on one axis at a time, then dropped, so the character never
/// lands in geometry.
pub fn separate_from_player(
    room: &RoomState,
    entity: &mut Entity,
    player_pos: [i32; 3],
    player_radius: i32,
    player_status: u8,
) {
    resolve_sca_collision(room, entity, player_pos, player_radius, player_status);
}

/// `ResolveEntityScaCollision` against another active character, the
/// `HandleEnemyPlayerCollisions` pass the original's state-9 tail runs after
/// the player pair: the character is pushed out of the other's radius and the
/// other is never moved.
pub fn separate_from_character(
    room: &RoomState,
    entity: &mut Entity,
    other_pos: [i32; 3],
    other_radius: i32,
    other_status: u8,
) {
    resolve_sca_collision(room, entity, other_pos, other_radius, other_status);
}

/// The shared pair resolve. `entity` is the original's `entB` (the one pushed)
/// and the other entity is `entA`; the original's guards are `entB->state == 4`
/// (the eating/headless state) and status bit 1 on either side (the
/// deactivation bit, e.g. the lab power-room Wesker).
fn resolve_sca_collision(
    room: &RoomState,
    entity: &mut Entity,
    other_pos: [i32; 3],
    other_radius: i32,
    other_status: u8,
) {
    if entity.state() == 4 || (entity.status_flags | other_status) & 2 != 0 {
        return;
    }
    let dx = entity.pos[0] - other_pos[0];
    let dz = entity.pos[2] - other_pos[2];
    let dist =
        ((i64::from(dx) * i64::from(dx) + i64::from(dz) * i64::from(dz)) as f64).sqrt() as i32;
    let radius = i32::from(entity.sca_radius);
    let penetration = other_radius + radius - (dist + 1);
    if penetration <= 0 {
        return;
    }

    let denominator = i64::from(dist + 1);
    let push_x = (i64::from(penetration) * i64::from(dx) / denominator) as i32;
    let push_z = (i64::from(penetration) * i64::from(dz) / denominator) as i32;
    if push_x == 0 && push_z == 0 {
        return;
    }

    let proposed = [
        entity.pos[0] + push_x,
        entity.pos[1],
        entity.pos[2] + push_z,
    ];
    if !player::position_blocked(room, proposed, radius) {
        entity.pos = proposed;
        return;
    }
    let x_only = [entity.pos[0] + push_x, entity.pos[1], entity.pos[2]];
    if push_x != 0 && !player::position_blocked(room, x_only, radius) {
        entity.pos = x_only;
        return;
    }
    let z_only = [entity.pos[0], entity.pos[1], entity.pos[2] + push_z];
    if push_z != 0 && !player::position_blocked(room, z_only, radius) {
        entity.pos = z_only;
    }
}

/// The player's collision radius for the room's character flag.
fn player_radius(player_flag: u8) -> i32 {
    if player_flag & 1 == 0 {
        player::CHRIS_RADIUS
    } else {
        player::JILL_RADIUS
    }
}

/// One state-9 tick: the follow/pathfind driver.
///
/// The first tick marks the character as ignoring the player and resets the
/// look-at. Every tick swaps the behaviour on the distance ring, records the
/// character's zone, runs the selected behaviour, advances the animation with
/// the blend step `0x1000 / (blend_counter + 1)` and the reverse bit from
/// `dir_control_flags`, queues the footfall sound and separates the character
/// from the player.
pub fn update(game: &mut GameState, slot: usize, room: &RoomState, clips: &[Clip]) {
    let player_pos = game.entities[0].pos;
    let player_status = game.entities[0].status_flags;
    let player_radius = player_radius(game.id.player_flag);
    let seed = game.rand_seed;

    let GameState {
        entities,
        entity_anims,
        entity_sounds,
        ..
    } = game;
    // The original's `HandleEnemyPlayerCollisions` scans the enemy list after
    // the player pair; collect the other active characters' positions, radii
    // and status flags before borrowing this slot mutably.
    let others: Vec<([i32; 3], i32, u8)> = entities
        .iter()
        .enumerate()
        .filter(|(index, other)| *index != 0 && *index != slot && other.status_flags != 0)
        .map(|(_, other)| (other.pos, i32::from(other.sca_radius), other.status_flags))
        .collect();
    let entity = &mut entities[slot];
    let clock = &mut entity_anims[slot];

    if entity.ignore() == 0 {
        entity.set_ignore(1);
        reset_lookat(entity, seed);
    }

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
    let blend_step = crate::npc::anim::blend_step(entity);
    let done = clock.advance(entity, clips, reverse, blend_step);
    entity.attacking_direction = u8::from(done);
    walk_footstep_sound(entity_sounds, room, entity);
    separate_from_player(room, entity, player_pos, player_radius, player_status);
    for (other_pos, other_radius, other_status) in others {
        separate_from_character(room, entity, other_pos, other_radius, other_status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GameState;
    use crate::state::{Collision, CollisionRect, FootstepZone, WalkZone};

    fn entity(pos: [i32; 3], angle: u16, frame: u8) -> Entity {
        Entity {
            pos,
            angle,
            animation_frame_id: frame,
            sca_radius: 100,
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
    fn advance_xz_rolls_back_a_blocked_move() {
        let room = blocked_room();
        // Angle 0 walks +X into the rectangle at x=1000.
        let mut e = entity([900, 0, 500], 0, 0);
        advance_xz(&room, &mut e, 0, 100);
        assert_eq!(e.pos, [900, 0, 500], "a blocked move rolls back");

        // Walking away is free.
        let mut e = entity([900, 0, 500], 0x800, 0);
        advance_xz(&room, &mut e, 0, 100);
        assert_eq!(e.pos, [800, 0, 500]);
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
        footstep(&mut sounds, &room, &e, 0);
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
        walk_footstep_sound(&mut sounds, &room, &e);
        assert_eq!(sounds.len(), 1);
        assert_eq!(sounds[0].name, "ft_wdA");

        sounds.clear();
        e.animation_id = 8;
        e.animation_frame_id = 0;
        walk_footstep_sound(&mut sounds, &room, &e);
        assert_eq!(sounds.len(), 1, "the fast walk uses the B footstep");
        assert_eq!(sounds[0].name, "ft_wdB");

        sounds.clear();
        e.animation_id = 7;
        e.animation_frame_id = 7;
        walk_footstep_sound(&mut sounds, &room, &e);
        assert!(sounds.is_empty(), "frame 7 is not a contact");
    }

    #[test]
    fn separation_pushes_the_character_out_of_the_player() {
        let room = corridor();
        let mut e = entity([1100, 0, 500], 0, 0);
        e.sca_radius = 100;
        separate_from_player(&room, &mut e, [1000, 0, 500], 100, 0);
        let dist = xz_distance_to(&e, [1000, 0, 500]);
        assert!(dist > 100, "pushed out of the overlap: {dist}");
    }

    #[test]
    fn separation_never_pushes_into_a_wall() {
        let mut room = corridor();
        // A wall sits on the side the character would be pushed toward.
        room.collision.quadrants[0].push(CollisionRect {
            x_max: 900,
            z_max: 700,
            x_min: 700,
            z_min: 0,
            kind: 1,
            flags: 0,
        });
        let mut e = entity([1050, 0, 300], 0, 0);
        e.sca_radius = 100;
        separate_from_player(&room, &mut e, [1150, 0, 300], 100, 0);
        assert!(
            !player::position_blocked(&room, e.pos, 100),
            "the push was rolled back instead of landing in the wall: {:?}",
            e.pos
        );
        assert_eq!(e.pos, [1050, 0, 300], "the blocked axis was dropped");
    }

    #[test]
    fn separation_skips_state_4_and_deactivated_pairs() {
        let room = corridor();

        // entB (the character) in state 4 is the eating/headless skip.
        let mut eating = entity([1050, 0, 300], 0, 0);
        eating.sca_radius = 100;
        eating.set_state(4);
        separate_from_player(&room, &mut eating, [1150, 0, 300], 100, 0);
        assert_eq!(eating.pos, [1050, 0, 300], "state 4 is skipped");

        // Status bit 1 on the character skips the pair.
        let mut deactivated = entity([1050, 0, 300], 0, 0);
        deactivated.sca_radius = 100;
        deactivated.status_flags |= 2;
        separate_from_player(&room, &mut deactivated, [1150, 0, 300], 100, 0);
        assert_eq!(
            deactivated.pos,
            [1050, 0, 300],
            "the character's deactivation bit is skipped"
        );

        // Status bit 1 on the player skips the pair too.
        let mut player_off = entity([1050, 0, 300], 0, 0);
        player_off.sca_radius = 100;
        separate_from_player(&room, &mut player_off, [1150, 0, 300], 100, 2);
        assert_eq!(
            player_off.pos,
            [1050, 0, 300],
            "the player's deactivation bit is skipped"
        );
    }

    #[test]
    fn separation_resolves_character_against_character() {
        let room = corridor();
        let mut e = entity([1050, 0, 300], 0, 0);
        e.sca_radius = 100;
        separate_from_character(&room, &mut e, [1150, 0, 300], 100, 0);
        let dist = xz_distance_to(&e, [1150, 0, 300]);
        assert!(dist > 100, "pushed out of the other character: {dist}");
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
}
