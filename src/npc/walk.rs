//! Native movement helpers shared by the scripted (state 8) and follow
//! (state 9) drivers.
//!
//! The original's state-8 handlers steer with `turn_toward_target` /
//! `entity_rotate_toward_target`, trim their speed on the animation's footfall
//! frames with `entity_apply_walk_speed`, and move with `Add_speedXZ`; state 9
//! reuses the same helpers. The engine resolves every move against the room
//! collision with the entity's own radius and rolls a blocked move back, so a
//! scripted walk never pushes a character through geometry.

use crate::game::{Entity, EntitySound};
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
    let (dx, dz) = player::rotate_speed(entity.angle, offset, i32::from(distance));
    let proposed = [entity.pos[0] + dx, entity.pos[1], entity.pos[2] + dz];
    let radius = i32::from(entity.sca_radius);
    if !player::position_blocked(room, proposed, radius) {
        entity.pos = proposed;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Collision, CollisionRect};

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
        use crate::state::FootstepZone;
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
}
