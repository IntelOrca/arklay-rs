//! The Yawn segment-link slots.
//!
//! Yawn is one model with fifteen joints and thirteen enemy slots: the head
//! runs the machine and the twelve body segments each mirror one joint of the
//! head's skeleton. The original `memcpy`s the head entity into
//! `g_EnemiesList[1..12]`, points each copy at `joints[2 + k]` and marks it
//! `behavior_flags = 1`; every tick those slots skip the state machine, mirror
//! their joint's world translation into their own position, take room
//! collision at that point, route damage to the head, and - on a wall hit -
//! drag the rest of the chain along behind them.
//!
//! Lua cannot allocate slots or hold joint pointers, so this module owns the
//! whole segment branch: [`spawn_segments`] builds the twelve slots out of the
//! head's copied record, [`segment_tick`] is the per-frame body update, and
//! [`set_status_range`] is the head's batched status writer (the original's
//! `for (i = last; ; i--) g_EnemiesList[i].status_flags ...` loops).

use crate::anim::Mat4x3;
use crate::effects::Attach;
use crate::game::{Entity, GameState};
use crate::state::RoomState;

use super::custom_anim::{YAWN_JOINTS, YawnPose, yawn_chain_align, yawn_chain_follow};

/// How many body segments a Yawn builds.
pub const SEGMENT_COUNT: usize = 12;

/// The joint a body segment mirrors: the clone at enemy-list index `k` tracks
/// `joints[2 + k]`.
pub fn segment_joint(index: usize) -> u8 {
    (2 + index) as u8
}

/// Build the twelve body segments for the Yawn in `slot`, exactly like the
/// original's clone loop: a full copy of the head per segment, the segment's
/// position from the joint it mirrors, `behavior_flags = 1`, `health = -1`,
/// the status bit `0x64`, and the two scripted forms clearing status bit 2.
/// Adds twelve to the live enemy count. Returns the segment slots.
pub fn spawn_segments(game: &mut GameState, slot: usize) -> Vec<usize> {
    let head = game.entities[slot];
    let free: Vec<usize> = (1..crate::game::ENTITY_COUNT)
        .filter(|index| *index != slot && !game.entities[*index].active())
        .take(SEGMENT_COUNT)
        .collect();
    if free.len() < SEGMENT_COUNT {
        eprintln!("[yawn] no room for {SEGMENT_COUNT} body segments (slot {slot})");
        return Vec::new();
    }

    let worlds = game.joint_worlds[slot].clone();
    let position = [
        i32::from(head.pos[0] as i16),
        i32::from(head.pos[1] as i16),
        i32::from(head.pos[2] as i16),
    ];
    let scripted = matches!(head.behavior_flags & 0x0F, 2 | 6);

    for (index, &target) in free.iter().enumerate() {
        let joint = segment_joint(index + 1);
        let mut segment = head;
        segment.yawn_head = Some(slot as u8);
        segment.yawn_joint = joint;
        segment.behavior_flags = 1;
        segment.health = -1;
        segment.status_flags |= 100;
        if scripted {
            segment.status_flags &= 0xFB;
        }
        segment.saved_pos = Some(position);
        if let Some(matrix) = worlds.get(usize::from(joint)) {
            segment.pos = matrix.t;
        }
        game.entities[target] = segment;
    }
    game.enemy_count = game.enemy_count.saturating_add(SEGMENT_COUNT as u8);
    free
}

/// The head slot a segment belongs to, or `None` when the entity is not a
/// Yawn body segment.
pub fn segment_head(entity: &Entity) -> Option<usize> {
    entity.yawn_head.map(usize::from)
}

/// One body segment's frame, the original's `behavior_flags == 1` branch plus
/// the shared tail. Mirrors the joint, takes room collision, routes damage to
/// the head, drags the chain and updates the switch-zone bit.
pub fn segment_tick(game: &mut GameState, room: &RoomState, slot: usize) {
    let Some(head_slot) = segment_head(&game.entities[slot]) else {
        return;
    };
    let joint = usize::from(game.entities[slot].yawn_joint).min(YAWN_JOINTS - 1);

    // Mirror the joint this segment owns.
    let Some(world) = game.joint_worlds[head_slot].get(joint).copied() else {
        return;
    };
    game.entities[slot].pos = world.t;
    game.entities[slot].status_flags &= 0x1F;

    let player_pos = game.entities[0].pos;
    crate::enemy::walk::entity_check_visual_range(&mut game.entities[slot], player_pos, 4000);
    if game.entities[slot].pos[1] < -0x5DC {
        game.entities[slot].status_flags |= 0xC0;
    }

    // Damage anywhere on the body drains the head's health.
    if game.entities[slot].hit_state & 0x07 != 0 {
        game.entities[slot].hit_state = game.entities[slot].hit_state.wrapping_sub(1);
        let lift = if world.t[1] < -0x5DC { 300 } else { -300 };
        spawn_segment_effect(game, head_slot, joint, 0, 8, [0, lift, 0]);

        let phase = game.entities[slot].hit_state & 0x78;
        if phase == 0x40 || phase == 0x50 {
            let head = &mut game.entities[head_slot];
            head.health = head.health.wrapping_sub(0x41);
            let step = crate::enemy::walk::turn_toward_target(head, player_pos, 0x10);
            head.angle = head.angle.wrapping_add(step as u16);
            if head.health < 0 {
                head.set_state(3);
                head.set_ignore(0);
                head.action_behavior = 0;
                head.action_state = 0;
            }
        }
        if game.entities[slot].hit_state & 0x07 == 0 {
            let second = game.flags[usize::from(crate::game::BANK_SCENARIO)]
                .bit(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH);
            let head = &mut game.entities[head_slot];
            if second {
                head.health = head.health.wrapping_sub(5);
            } else {
                head.health = head.health.wrapping_sub(0x0F);
            }
            if joint < 10 && second {
                head.health = head.health.wrapping_sub(0x0F);
            }
            let step = crate::enemy::walk::turn_toward_target(head, player_pos, 0x10);
            head.angle = head.angle.wrapping_add(step as u16);
            if head.health < 0 {
                head.set_state(3);
                head.set_ignore(0);
                head.action_behavior = 0;
                head.action_state = 0;
            }
        }
        game.entities[slot].hit_state &= 0x87;
    }

    // Room collision at the mirrored point; a hit drags the chain.
    let result = crate::enemy::walk::check_room_collision(room, &mut game.entities[slot]);
    if result != 0 {
        let pushed = game.entities[slot].pos;
        let Some(pose) = game.entity_anims[head_slot].yawn.as_deref_mut() else {
            return;
        };
        drag_chain(pose, joint, pushed, &game.entities[head_slot]);
    }

    // The shared switch-zone tail: keep the death-park bit, refresh the rest.
    let zone = u8::from(super::in_camera_zone(
        room,
        room.current_cut,
        game.entities[slot].pos,
    ));
    game.entities[slot].has_enter_switch_zone =
        (game.entities[slot].has_enter_switch_zone & 0x80) | zone;

    // A dead head parks its segments and clears their hit latches.
    if game.entities[head_slot].health < 0 {
        game.entities[slot].status_flags |= 2;
        game.entities[slot].hit_state = 0x80;
    }
}

/// The original's segment collision drag: shift the segment (and, at joint 3,
/// the head joints it carries), then follow every joint from there to the
/// tail. The head's model scale ramp is applied to joint 3's world when the
/// joint-3 or joint-4 segment reports it.
fn drag_chain(pose: &mut YawnPose, joint: usize, pushed: [i32; 3], head: &Entity) {
    let delta = [
        pose.world[joint].t[0].wrapping_sub(pushed[0]),
        pose.world[joint].t[1].wrapping_sub(pushed[1]),
        pose.world[joint].t[2].wrapping_sub(pushed[2]),
    ];
    let (mut chain, limit) = if joint == 3 {
        // Joint 3 carries the head: shift joints 0-2 and joint 4 with it,
        // then follow from joint 3 through joint 13 (the original's loop
        // stops one link short of the tail in this branch).
        for back in 0..=2 {
            pose.world[back].t[0] = pose.world[back].t[0].wrapping_sub(delta[0]);
            pose.world[back].t[2] = pose.world[back].t[2].wrapping_sub(delta[2]);
        }
        pose.world[4].t[0] = pose.world[4].t[0].wrapping_sub(delta[0]);
        pose.world[4].t[2] = pose.world[4].t[2].wrapping_sub(delta[2]);
        pose.world[joint].t = pushed;
        (3usize, YAWN_JOINTS - 1)
    } else {
        pose.world[joint].t = pushed;
        if joint > 5 && head.yawn_form() == 0 {
            yawn_chain_align(pose, joint - 1, joint);
        }
        (joint, YAWN_JOINTS)
    };

    let head_speed = head.move_speed_current;
    let head_angle = head.angle as i16;
    while chain < limit {
        yawn_chain_follow(pose, chain - 1, chain, 0x30, head_speed, head_angle);
        chain += 1;
    }

    let ramp = head.yawn_scale_ramp();
    if ramp != 0 {
        let scale = [i32::from(ramp) - 300; 3];
        if joint == 4 && head.yawn_form() == 0 {
            crate::enemy::custom_anim::scale_columns_xyz(&mut pose.world[3], scale);
        }
        if joint == 3 {
            crate::enemy::custom_anim::scale_columns_xyz(&mut pose.world[3], scale);
        }
    }
}

/// A billboard attached to the head's joint, the original's
/// `Effect_CreateBillboard(..., &seg->world, ...)`.
fn spawn_segment_effect(
    game: &mut GameState,
    head_slot: usize,
    joint: usize,
    effect_type: u8,
    depth: u8,
    offset: [i32; 3],
) {
    let room_effects = std::rc::Rc::clone(&game.room_effects);
    crate::effects::create_attached(
        game,
        &room_effects,
        effect_type,
        depth,
        Attach::Joint(head_slot as u8, joint as u8),
        offset,
        0,
        0,
    );
}

/// The head's batched status write over the enemy-list range `first..=last`
/// (0 is the head, 1..12 the segments): `status = (status & and) | or`. The
/// original's three swallow/death/flee loops write exactly this.
pub fn set_status_range(
    game: &mut GameState,
    head_slot: usize,
    first: usize,
    last: usize,
    and: u8,
    or: u8,
) {
    let Some(segments) = segment_slots(game, head_slot) else {
        return;
    };
    for index in first.min(last)..=first.max(last) {
        let slot = if index == 0 {
            head_slot
        } else {
            match segments.get(index - 1) {
                Some(slot) => *slot,
                None => continue,
            }
        };
        let entity = &mut game.entities[slot];
        entity.status_flags = (entity.status_flags & and) | or;
    }
}

/// The twelve segment slots of the Yawn in `head_slot`, in enemy-list order,
/// or `None` when the head has no complete body.
pub fn segment_slots(game: &GameState, head_slot: usize) -> Option<Vec<usize>> {
    let mut slots: Vec<(u8, usize)> = (1..crate::game::ENTITY_COUNT)
        .filter(|index| {
            game.entities[*index].yawn_head == Some(head_slot as u8)
                && game.entities[*index].behavior_flags == 1
        })
        .map(|index| (game.entities[index].yawn_joint, index))
        .collect();
    slots.sort_unstable();
    if slots.len() < SEGMENT_COUNT {
        return None;
    }
    Some(slots.into_iter().map(|(_, slot)| slot).collect())
}

/// The joint world matrices of a Yawn head, or an empty slice.
pub fn head_worlds(game: &GameState, head_slot: usize) -> &[Mat4x3] {
    game.joint_worlds.get(head_slot).map_or(&[], Vec::as_slice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GameState;

    fn yawn_game() -> GameState {
        let mut game = GameState::default();
        game.id = crate::state::RoomId::parse("40C0").unwrap();
        let entity = &mut game.entities[1];
        entity.id = 0x0D;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.health = 0x0BEA;
        entity.pos = [1000, 0, 1000];
        entity.shadow_half_x = 1000;
        entity.shadow_half_z = 1000;
        game.enemy_count = 1;
        game.joint_worlds[1] = (0..YAWN_JOINTS)
            .map(|index| Mat4x3 {
                r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                t: [1000 + index as i32, -100, 1000 + index as i32],
            })
            .collect();
        game
    }

    #[test]
    fn spawn_builds_twelve_segments_linked_to_their_joints() {
        let mut game = yawn_game();
        let slots = spawn_segments(&mut game, 1);
        assert_eq!(slots.len(), SEGMENT_COUNT);
        assert_eq!(game.enemy_count, 13);
        for (index, &slot) in slots.iter().enumerate() {
            let segment = &game.entities[slot];
            assert_eq!(segment.id, 0x0D);
            assert!(segment.active());
            assert_eq!(segment.behavior_flags, 1);
            assert_eq!(segment.health, -1);
            assert_eq!(segment.status_flags & 100, 100);
            assert_eq!(segment.yawn_head, Some(1));
            assert_eq!(segment.yawn_joint, 2 + index as u8 + 1);
            let joint = 2 + index + 1;
            assert_eq!(segment.pos, game.joint_worlds[1][joint].t);
            assert_eq!(segment.sca_radius, game.entities[1].sca_radius);
            assert_eq!(
                segment.saved_pos,
                Some([1000, 0, 1000]),
                "the short position words"
            );
        }
    }

    #[test]
    fn scripted_forms_clear_the_visible_status_bit() {
        let mut game = yawn_game();
        game.entities[1].behavior_flags = 2;
        let slots = spawn_segments(&mut game, 1);
        for &slot in &slots {
            assert_eq!(game.entities[slot].status_flags & 4, 0);
        }
    }

    #[test]
    fn segment_tick_mirrors_the_joint_and_tracks_the_zone() {
        let mut game = yawn_game();
        let slots = spawn_segments(&mut game, 1);
        let room = RoomState::default();
        let joint = usize::from(game.entities[slots[1]].yawn_joint);
        game.joint_worlds[1][joint].t = [1500, -50, 1200];
        segment_tick(&mut game, &room, slots[1]);
        let segment = &game.entities[slots[1]];
        assert_eq!(segment.pos, [1500, -50, 1200]);
        assert_eq!(
            segment.status_flags & 0x1F,
            1 | 4,
            "active plus the visual-range bit"
        );
        // The segment is above the ceiling threshold: the ceiling bits set.
        game.joint_worlds[1][joint].t[1] = -0x600;
        segment_tick(&mut game, &room, slots[1]);
        assert_eq!(game.entities[slots[1]].status_flags & 0xC0, 0xC0);
    }

    #[test]
    fn segment_damage_drains_the_head_health() {
        let mut game = yawn_game();
        let slots = spawn_segments(&mut game, 1);
        let room = RoomState::default();
        // A light hit latched on the segment: the counter ticks and, when the
        // low bits empty, the head loses 0x0F.
        game.entities[slots[0]].hit_state = 0x01;
        segment_tick(&mut game, &room, slots[0]);
        assert_eq!(game.entities[1].health, 0x0BEA - 0x0F);
        assert_eq!(game.entities[slots[0]].hit_state, 0);

        // A heavy hit (phase 0x40) does 0x41; the counter lands on 0x40,
        // whose low bits are also empty, so the light tick fires too.
        game.entities[slots[0]].hit_state = 0x41;
        segment_tick(&mut game, &room, slots[0]);
        assert_eq!(game.entities[1].health, 0x0BEA - 0x0F - 0x41 - 0x0F);
    }

    #[test]
    fn a_killing_body_hit_parks_the_head_and_its_segments() {
        let mut game = yawn_game();
        let slots = spawn_segments(&mut game, 1);
        let room = RoomState::default();
        game.entities[1].health = 1;
        game.entities[slots[0]].hit_state = 0x41;
        segment_tick(&mut game, &room, slots[0]);
        assert!(game.entities[1].health < 0);
        assert_eq!(game.entities[1].state(), 3);
        // The tail segment observes the dead head and parks itself.
        segment_tick(&mut game, &room, slots[11]);
        assert_eq!(game.entities[slots[11]].status_flags & 2, 2);
        assert_eq!(game.entities[slots[11]].hit_state, 0x80);
    }

    #[test]
    fn status_range_writes_the_head_and_segment_batches() {
        let mut game = yawn_game();
        let slots = spawn_segments(&mut game, 1);
        set_status_range(&mut game, 1, 0, 12, 0xFF, 4);
        assert_eq!(game.entities[1].status_flags & 4, 4);
        for &slot in &slots {
            assert_eq!(game.entities[slot].status_flags & 4, 4);
        }
        set_status_range(&mut game, 1, 0, 3, 0xFB, 0);
        assert_eq!(game.entities[1].status_flags & 4, 0);
        assert_eq!(game.entities[slots[0]].status_flags & 4, 0);
        assert_eq!(
            game.entities[slots[3]].status_flags & 4,
            4,
            "outside the range"
        );
    }

    #[test]
    fn segment_slots_orders_by_joint() {
        let mut game = yawn_game();
        let spawned = spawn_segments(&mut game, 1);
        let slots = segment_slots(&game, 1).unwrap();
        assert_eq!(slots, spawned);
        assert_eq!(game.entities[slots[0]].yawn_joint, 3);
        assert_eq!(game.entities[slots[11]].yawn_joint, 14);
    }
}
