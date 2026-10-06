//! Native movement helpers shared by the scripted (state 8) and follow
//! (state 9) drivers.
//!
//! The original's state-8 handlers steer with `turn_toward_target` /
//! `entity_rotate_toward_target`, trim their speed on the animation's footfall
//! frames with `entity_apply_walk_speed`, and move with `Add_speedXZ`; state 9
//! reuses the same helpers. `Add_speedXZ` is a bare un-collided add: a
//! character walks into geometry and is only pushed back out by the pass that
//! runs later in its own driver ([`update`]'s state-9 tail, or the room-object
//! pass). [`try_advance_xz`] keeps the rollback only for the idle walk-01
//! probe, where the original saves the moved words, probes, and restores them.
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

use crate::game::{Entity, EntitySound, GameState};
use crate::model::Clip;
use crate::npc::data;
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

/// `Add_speedXZ`: move `distance` units along the entity's yaw plus `offset`.
/// A bare un-collided add - the original never tests the room here, so a
/// scripted walk can cross geometry and only the driver's own collision pass
/// (the state-9 tail or the room-object pass) pushes it back out.
pub fn advance_xz(entity: &mut Entity, offset: u16, distance: i16) {
    let (dx, dz) = player::rotate_speed(entity.angle, offset, i32::from(distance));
    entity.pos[0] += dx;
    entity.pos[2] += dz;
}

/// The state-8 scripted walks: `Add_speedXZ` with a pre-move collision probe.
///
/// The original's state-8 handlers have no end-of-frame room resolve at all,
/// so an un-collided step can walk a scripted character straight through
/// geometry with nothing to push it back out. The port keeps the pre-check +
/// rollback for this driver only (documented deviation); state 9 mirrors the
/// original's un-collided move and resolves in its own tail.
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
    if player::position_blocked(room, entity.pos, radius) {
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
#[derive(Clone, Copy)]
struct ZoneWalk {
    idx: [u8; 0x10],
    dir: [u8; 0x10],
    best: [u8; 0x0C],
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
            best: [0; 0x0C],
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
/// top and can never re-probe a consumed candidate (the original's byte wrap
/// read past the zone table there).
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
/// on the ascending scan exactly like the original).
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
fn room_check_sight_blocked(
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
        };
        match data::sca_volume(id) {
            Some(volume) => ScaHit {
                offset: volume.offset,
                half_height: volume.half_height,
                radius,
            },
            None => standing,
        }
    }

    /// `SetEntityScaHitData`: the world-space volume centre, the local offset
    /// rotated by the entity's yaw.
    pub(crate) fn world_offset(&self, angle: u16) -> [i32; 3] {
        let (x, z) = player::rotate_xz(angle, i32::from(self.offset[0]), i32::from(self.offset[2]));
        [x, i32::from(self.offset[1]), z]
    }
}

/// `ResolveEntityScaCollision` against the player: push the character out of
/// the player's SCA volume. The player is never moved and no damage is
/// transferred. `prev_pos` is the character's pre-move `position` word, which
/// breaks a degenerate overlap when the character ran through the other
/// volume in one frame.
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
        ScaHit::player(player_radius),
        player_status,
    )
}

/// `ResolveEntityScaCollision` against another active character, the
/// `HandleEnemyPlayerCollisions` pass the original's state-9 tail runs after
/// the player pair: the character is pushed out of the other's volume and the
/// other is never moved.
pub fn separate_from_character(
    entity: &mut Entity,
    prev_pos: [i32; 3],
    other_pos: [i32; 3],
    other_angle: u16,
    other: ScaHit,
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

/// The original's `SquareRoot0`: integer square root, zero for non-positive.
fn integer_sqrt(value: i32) -> i32 {
    if value <= 0 {
        0
    } else {
        (f64::from(value)).sqrt() as i32
    }
}

/// The shared pair resolve. `entity` is the original's `entB` (the one pushed)
/// and the other entity is `entA`; the original's guards are `entB->state == 4`
/// (the eating/headless state) and status bit 1 on either side (the
/// deactivation bit, e.g. the lab power-room Wesker).
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
    other: ScaHit,
    other_status: u8,
) -> bool {
    if entity.state() == 4 || (entity.status_flags | other_status) & 2 != 0 {
        return false;
    }

    let own = ScaHit::character(entity.id, entity.sca_radius);
    let a = other.world_offset(other_angle);
    let b = own.world_offset(entity.angle);
    let dx = (b[0] - a[0]) - other_pos[0] + entity.pos[0];
    let dz = (b[2] - a[2]) - other_pos[2] + entity.pos[2];
    let dist = integer_sqrt(dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz)));
    let penetration = i32::from(other.radius) + i32::from(own.radius) - (dist + 1);
    if penetration <= 0 {
        return false;
    }

    let dy = b[1] + (entity.pos[1] - a[1]) - other_pos[1];
    let max_height = i32::from(other.half_height) + i32::from(own.half_height);
    if -max_height >= dy || dy >= max_height {
        return false;
    }

    let mut push_x = penetration.wrapping_mul(dx) / (dist + 1);
    let mut push_z = penetration.wrapping_mul(dz) / (dist + 1);

    // The pre-move Y says whether the pair already overlapped vertically
    // before this frame's movement; when it did not, a horizontal crossing
    // (the other centre now strictly between the old and new own centre) is a
    // tunnelling overlap and the push is rewritten to the other side.
    let dy2 = b[1] + (prev_pos[1] - a[1]) - other_pos[1];
    if dy2 <= -max_height || max_height <= dy2 {
        let other_radius = i32::from(other.radius);
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
    true
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
    let others: Vec<([i32; 3], u16, ScaHit, u8)> = entities
        .iter()
        .enumerate()
        .filter(|(index, other)| *index != 0 && *index != slot && other.status_flags != 0)
        .map(|(_, other)| {
            (
                other.pos,
                other.angle,
                ScaHit::character(other.id, other.sca_radius),
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

    // The end-of-frame collision pass accepts the final position and the next
    // tick's degenerate-overlap test reads it back as `position`; the position
    // on entry is exactly that accepted word.
    let prev_pos = entity.pos;
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
    let blend_step = crate::npc::anim::blend_step(entity);
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
    for (other_pos, other_angle, other, other_status) in others {
        separate_from_character(
            entity,
            prev_pos,
            other_pos,
            other_angle,
            other,
            other_status,
        );
    }
    // check_room_collision: push out of walls, or roll X/Z back to the
    // accepted position when the pushed result is still stuck.
    entity.pos = player::resolve_collision(
        &room.collision,
        prev_pos,
        entity.pos,
        i32::from(entity.sca_radius),
    );
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
            player::position_blocked(&room, e.pos, 100),
            "the bare move can land inside geometry"
        );

        // The end-of-frame tail resolves it back out.
        let resolved = player::resolve_collision(&room.collision, [900, 0, 500], e.pos, 100);
        assert!(
            !player::position_blocked(&room, resolved, 100),
            "the collision pass pushes the character out: {resolved:?}"
        );
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
        let resolved = player::resolve_collision(&room.collision, prev, proposed, 100);
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
        };
        assert!(!separate_from_character(
            &mut e,
            prev,
            [1150, 0, 300],
            0,
            high,
            0
        ));
        assert_eq!(e.pos, prev, "the vertical gap skips the pair");

        // Lower the other volume until the half-heights overlap: the push runs.
        let low = ScaHit {
            offset: [0, 0, 0],
            half_height: 0x5FA,
            radius: 100,
        };
        assert!(separate_from_character(
            &mut e,
            prev,
            [1150, 0, 300],
            0,
            low,
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
        };
        assert_eq!(hit.world_offset(0), [0x257, -0xB4, -0xC7]);
        assert_eq!(hit.world_offset(0x400), [-0xC7, -0xB4, -0x258]);
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
            player::position_blocked(&room, e.pos, 100),
            "the SCA resolve itself does not consult the room: {:?}",
            e.pos
        );
        // The end-of-frame tail then pushes it clear.
        let resolved = player::resolve_collision(&room.collision, prev, e.pos, 100);
        assert!(
            !player::position_blocked(&room, resolved, 100),
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
            other,
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
            other,
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
}
