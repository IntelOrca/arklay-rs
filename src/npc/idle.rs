//! State-0 spawn init and the state-1 idle behaviours.
//!
//! The idle handlers run before the shared animation advance in
//! [`super::update_entity`]: the ones that animate return `true` and let the
//! common tail advance the clock, while the ones that need the completion
//! result advance it themselves and return `false`. Behaviours that only exist
//! to spawn effects (the blood of the two death behaviours) keep their
//! animation state machine but leave the effect calls to the milestone that
//! adds them.
//!
//! Behaviour 1 walks along its reversed facing (the original's
//! `Add_speedXZ(0x800)` moves by `move_speed_current` at yaw + 0x800) until the
//! room collision probe fires, then plays the knock animation; the move uses
//! the walk layer's collision-resolved [`super::walk::advance_xz`]. The voice
//! cue the original plays on the hit is out of M9 scope (no voice), so only the
//! animation state advances.

use crate::game::{Entity, FLAG_BANK_COUNT, FlagBank};
use crate::model::Clip;
use crate::state::{RoomId, RoomState};

use super::anim::EntityAnim;
use super::data::{self, IdleBehavior};
use super::walk;

/// The blend step the state-0 init poses with (the original's `0x400`).
const INIT_BLEND_STEP: u16 = 0x400;
/// The blend step the idle handlers advance their clips with.
const IDLE_BLEND_STEP: u16 = 0x400;

/// State-0 spawn init: drop into state 1 with the behaviour/action scratch
/// cleared, zero rotation X/Z (never the yaw the spawn record wrote), force
/// health to -1, give the character its collision radius, apply the
/// per-character pose variant the story flags select and pose the skeleton
/// once at the spawned clip frame.
///
/// `flags` are the game's flag banks and `room` the room being booted: the
/// wounded Rebecca and Wesker-variant poses and the power-room status bit are
/// flag/stage conditions the original's per-character init handlers read.
pub fn init(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    clips: &[Clip],
    flags: &[FlagBank; FLAG_BANK_COUNT],
    room: RoomId,
) {
    entity.set_state(1);
    entity.set_ignore(0);
    entity.action_behavior = 0;
    entity.action_state = 0;
    entity.pitch = 0;
    entity.roll = 0;
    entity.health = -1;
    entity.sca_radius = data::collision_radius(entity.id)
        .unwrap_or(i32::from(crate::game::DEFAULT_ENEMY_RADIUS)) as i16;

    // The three corpse props restart on animation 0 frame 0 instead of the
    // spawned pose.
    if data::is_corpse(entity.id) {
        entity.animation_id = 0;
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
    }
    apply_pose_variant(entity, flags, room);
    entity.blend_counter = 0;
    clock.advance(entity, clips, false, INIT_BLEND_STEP);
}

/// Apply the flag-gated per-character opening pose, mirroring the original's
/// Rebecca and Wesker init handlers:
///
/// - Rebecca (and her cutscene alias) with the partner-alive scenario bit set
///   opens on the wounded clip/frame; the three tinted joints and the resized
///   shadow are presentation and stay deferred with the shadow stretch.
/// - Wesker (and his alias) with the variant scenario bit set opens on his
///   later clip/frame; the enlarged shadow is likewise deferred, and the
///   tracking joint's zeroed yaw/pitch and 0x10 pitch step have no analogue
///   until joint-level control exists.
/// - Wesker in the lab power room additionally gets status bit 1, the
///   original's deactivation bit, which takes him out of the entity/player
///   separation pass while he still renders and animates.
fn apply_pose_variant(entity: &mut Entity, flags: &[FlagBank; FLAG_BANK_COUNT], room: RoomId) {
    if data::is_rebecca(entity.id) && flags[1].bit(data::REBECCA_WOUNDED_FLAG) {
        entity.animation_id = data::REBECCA_WOUNDED_ANIM;
        entity.animation_frame_id = data::REBECCA_WOUNDED_FRAME;
        entity.timing_control = 0;
    } else if data::is_wesker(entity.id) && flags[0].bit(data::WESKER_VARIANT_FLAG) {
        entity.animation_id = data::WESKER_VARIANT_ANIM;
        entity.animation_frame_id = data::WESKER_VARIANT_FRAME;
    }
    if data::is_wesker(entity.id)
        && room.stage == data::STAGE_LABORATORY_INDEX + 1
        && room.room == data::ROOM_POWER_ROOM
    {
        entity.status_flags |= 2;
    }
}

/// One state-1 tick. Returns whether the common tail should advance the
/// animation clock (the handlers that need the completion result advance it
/// themselves and return `false`).
pub fn update(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    clips: &[Clip],
    room: &RoomState,
) -> bool {
    match data::idle_behavior(entity.action_behavior) {
        IdleBehavior::ById => behavior_by_id(entity),
        IdleBehavior::Walk01 => {
            walk_01(entity, clock, clips, room);
            false
        }
        IdleBehavior::Walk02 => {
            walk_02(entity, clock, clips);
            false
        }
        IdleBehavior::Walk03 => {
            walk_03(entity, clock, clips);
            false
        }
        IdleBehavior::PlayAnim => play_anim(entity),
        IdleBehavior::Nop => false,
    }
}

/// Behaviour 0: re-dispatch on the entity id. The corpse props land on the
/// play-animation handler; every living character is a no-op.
fn behavior_by_id(entity: &mut Entity) -> bool {
    if data::idle_0_plays_animation(entity.id) {
        play_anim(entity)
    } else {
        false
    }
}

/// Behaviours 9, 10 and 13: rewind to animation 0 frame 0 on entry, then keep
/// playing. The common tail performs the advance.
fn play_anim(entity: &mut Entity) -> bool {
    if entity.action_state == 0 {
        entity.action_state = 1;
        entity.animation_id = 0;
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.blend_counter = 0;
    } else if entity.action_state != 1 {
        return false;
    }
    true
}

/// Behaviour 1: walk forward until the collision probe fires, then knock on
/// the obstacle. The move is the walk layer's collision-resolved
/// [`walk::advance_xz`]; a blocked move leaves the character in place and
/// advances to the knock state.
fn walk_01(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], room: &RoomState) {
    match entity.action_state {
        0 => {
            entity.action_state = 1;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.animation_id = 0x35;
            entity.blend_counter = 0;
            entity.move_speed_current = 1000;
            walk_01_step(entity, clock, clips, room);
        }
        1 => walk_01_step(entity, clock, clips, room),
        2 => {
            entity.action_state = 3;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.animation_id = 0x36;
            entity.blend_counter = 3;
            // The original plays voice 0xA9 on the hit; voices are out of M9
            // scope, so only the knock animation runs.
            walk_01_knock(entity, clock, clips);
        }
        3 => walk_01_knock(entity, clock, clips),
        _ => {}
    }
}

/// Behaviour 1, walking state: bleed 15 off the speed for every animation
/// frame spent, advance the clip, then move by `move_speed_current` with the
/// original's `Add_speedXZ(0x800)` (the movement angle is the yaw flipped 180
/// degrees, so the character walks backwards along its facing). A move the
/// room collision refuses (checked before the commit, the same rollback the
/// original's probe performs) advances to the knock state.
fn walk_01_step(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], room: &RoomState) {
    let trim = u16::from(entity.animation_frame_id) * 0xF;
    entity.move_speed_current = (entity.move_speed_current as i16).wrapping_sub(trim as i16) as u16;
    clock.advance(entity, clips, false, IDLE_BLEND_STEP);
    if !walk::try_advance_xz(room, entity, 0x800, entity.move_speed_current as i16) {
        entity.action_state = 2;
    }
}

/// Behaviour 1, knocking state: play the knock animation and advance the
/// action state when it completes.
fn walk_01_knock(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip]) {
    let done = clock.advance(entity, clips, false, IDLE_BLEND_STEP);
    entity.action_state = entity.action_state.wrapping_add(u8::from(done));
}

/// Behaviour 2: the scripted death. Only the animation state machine runs; the
/// blood billboards, the death timer and the ground-pool grow are effects and
/// stay absent. The state stops at 2 once the clip completes.
fn walk_02(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip]) {
    match entity.action_state {
        0 => {
            entity.action_state = 1;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.animation_id = 0x33;
            entity.blend_counter = 3;
            walk_02_step(entity, clock, clips);
        }
        1 => walk_02_step(entity, clock, clips),
        _ => {}
    }
}

/// Behaviour 2's shared state 0/1 body: advance and move to the (inert) pool
/// state on completion.
fn walk_02_step(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip]) {
    let done = clock.advance(entity, clips, false, IDLE_BLEND_STEP);
    entity.action_state = entity.action_state.wrapping_add(u8::from(done));
}

/// Behaviour 3: the bleeding-out death. The animation plays to completion,
/// then the character clears status bit 1 and drops to health -1. The 250-tick
/// billboard grow is an effect and stays absent, so state 2 parks.
fn walk_03(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip]) {
    match entity.action_state {
        0 => {
            entity.action_state = 1;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.animation_id = 0x30;
            entity.blend_counter = 0;
            entity.hit_state = 0x80;
            entity.status_flags |= 6;
            walk_03_step(entity, clock, clips);
        }
        1 => walk_03_step(entity, clock, clips),
        2 => {
            entity.status_flags &= !2;
            entity.health = -1;
        }
        _ => {}
    }
}

/// Behaviour 3's shared state 0/1 body: advance and move to the park state on
/// completion.
fn walk_03_step(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip]) {
    let done = clock.advance(entity, clips, false, IDLE_BLEND_STEP);
    entity.action_state = entity.action_state.wrapping_add(u8::from(done));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Entity;
    use crate::model::{Clip, ClipFrame};

    fn no_flags() -> [FlagBank; FLAG_BANK_COUNT] {
        [FlagBank::new(); FLAG_BANK_COUNT]
    }

    fn room() -> RoomId {
        RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        }
    }

    fn clips() -> Vec<Clip> {
        vec![Clip {
            frames: vec![
                ClipFrame {
                    keyframe: 0,
                    timing: 1,
                },
                ClipFrame {
                    keyframe: 1,
                    timing: 1,
                },
            ],
        }]
    }

    #[test]
    fn init_sets_the_spawn_fields_and_poses_the_first_clip() {
        let mut entity = Entity {
            id: 0x23,
            angle: 0x456,
            animation_id: 0x10,
            animation_frame_id: 0x0C,
            timing_control: 1,
            health: 96,
            sca_radius: 1,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();
        init(&mut entity, &mut clock, &clips(), &no_flags(), room());

        assert_eq!(entity.state(), 1);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.action_behavior, 0);
        assert_eq!(entity.action_state, 0);
        assert_eq!(entity.pitch, 0);
        assert_eq!(entity.roll, 0);
        assert_eq!(entity.angle, 0x456, "init never touches the yaw");
        assert_eq!(entity.health, -1);
        assert_eq!(entity.sca_radius, 372);
        assert_eq!(entity.animation_id, 0x10, "the spawned clip is kept");
        // Frame 0x0C is out of range in the synthetic clip, so the clock
        // wraps to frame 0; display is frame 0.
        assert_eq!(clock.display_frame(), 0);
    }

    #[test]
    fn init_restarts_the_corpse_props_on_animation_zero() {
        for id in [0x25, 0x26, 0x29] {
            let mut entity = Entity {
                id,
                animation_id: 5,
                animation_frame_id: 3,
                timing_control: 2,
                blend_counter: 7,
                ..Entity::default()
            };
            let mut clock = EntityAnim::default();
            init(&mut entity, &mut clock, &clips(), &no_flags(), room());
            assert_eq!(entity.animation_id, 0, "id {id:#04x}");
            assert_eq!(entity.animation_frame_id, 1, "id {id:#04x}");
            assert_eq!(entity.blend_counter, 0, "id {id:#04x}");
        }
    }

    /// A clip table long enough for the Rebecca and Wesker variant frames.
    fn variant_clips() -> Vec<Clip> {
        let frame = ClipFrame {
            keyframe: 0,
            timing: 1,
        };
        let mut clips = vec![Clip::default(); 0x34];
        clips[usize::from(data::REBECCA_WOUNDED_ANIM)] = Clip {
            frames: vec![frame; usize::from(data::REBECCA_WOUNDED_FRAME) + 2],
        };
        clips[usize::from(data::WESKER_VARIANT_ANIM)] = Clip {
            frames: vec![frame; usize::from(data::WESKER_VARIANT_FRAME) + 2],
        };
        clips
    }

    #[test]
    fn init_applies_the_flag_gated_character_poses() {
        let mut flags = no_flags();
        flags[1].apply(data::REBECCA_WOUNDED_FLAG, 0);
        let mut entity = Entity {
            id: 0x23,
            animation_id: 0x10,
            animation_frame_id: 0x0C,
            timing_control: 1,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();
        init(&mut entity, &mut clock, &variant_clips(), &flags, room());
        assert_eq!(entity.animation_id, data::REBECCA_WOUNDED_ANIM);
        assert_eq!(
            clock.display_frame(),
            usize::from(data::REBECCA_WOUNDED_FRAME)
        );

        let mut flags = no_flags();
        flags[0].apply(data::WESKER_VARIANT_FLAG, 0);
        let mut entity = Entity {
            id: 0x24,
            animation_id: 0x10,
            animation_frame_id: 0x0C,
            timing_control: 1,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();
        init(&mut entity, &mut clock, &variant_clips(), &flags, room());
        assert_eq!(entity.animation_id, data::WESKER_VARIANT_ANIM);
        assert_eq!(
            clock.display_frame(),
            usize::from(data::WESKER_VARIANT_FRAME)
        );

        // Without the flags the spawned pose is kept.
        let mut entity = Entity {
            id: 0x23,
            animation_id: 0x10,
            animation_frame_id: 0,
            timing_control: 1,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();
        init(
            &mut entity,
            &mut clock,
            &variant_clips(),
            &no_flags(),
            room(),
        );
        assert_eq!(entity.animation_id, 0x10);
    }

    #[test]
    fn init_forces_weskers_status_bit_in_the_lab_power_room() {
        let power_room = RoomId {
            stage: data::STAGE_LABORATORY_INDEX + 1,
            room: data::ROOM_POWER_ROOM,
            player_flag: 0,
        };
        let mut wesker = Entity {
            id: 0x24,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();
        init(&mut wesker, &mut clock, &clips(), &no_flags(), power_room);
        assert_eq!(wesker.status_flags & 2, 2);

        // Another lab room, and another character, leave the bit clear.
        let other_room = RoomId {
            room: 0x10,
            ..power_room
        };
        let mut wesker = Entity {
            id: 0x24,
            ..Entity::default()
        };
        init(&mut wesker, &mut clock, &clips(), &no_flags(), other_room);
        assert_eq!(wesker.status_flags & 2, 0);

        let mut rebecca = Entity {
            id: 0x23,
            ..Entity::default()
        };
        init(&mut rebecca, &mut clock, &clips(), &no_flags(), power_room);
        assert_eq!(rebecca.status_flags & 2, 0);
    }

    /// A room whose low half (0 <= x <= 1000) is a wall, wide enough that the
    /// behaviour's step cannot jump clean over it.
    fn blocked_room() -> RoomState {
        RoomState {
            collision: crate::state::Collision {
                cell_x: 0,
                cell_z: 0,
                quadrants: std::array::from_fn(|_| {
                    vec![crate::state::CollisionRect {
                        x_max: 1000,
                        z_max: 2000,
                        x_min: 0,
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
    fn idle_walk_01_moves_until_the_collision_probe_fires() {
        let room = blocked_room();
        // `Add_speedXZ(0x800)` moves along yaw + 0x800, so angle 0 walks -X by
        // the trimmed `move_speed_current`: 1000 on the setup tick.
        let mut walker = Entity {
            id: 0x27,
            pos: [2500, 0, 500],
            angle: 0,
            sca_radius: 100,
            action_behavior: 1,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();
        update(&mut walker, &mut clock, &clips(), &room);
        assert_eq!(walker.action_state, 1);
        assert_eq!(
            walker.pos,
            [1500, 0, 500],
            "the walker stepped 1000 units along the reversed facing"
        );

        // The next tick trims 15 off the speed (frame 1) and steps 985, which
        // would cross into the wall at x=1000: the move is rolled back and the
        // knock state is entered.
        update(&mut walker, &mut clock, &clips(), &room);
        assert_eq!(walker.action_state, 2);
        assert_eq!(walker.pos, [1500, 0, 500], "the blocked move rolled back");
    }

    #[test]
    fn idle_9_rewinds_then_plays() {
        let mut entity = Entity {
            id: 0x23,
            action_behavior: 9,
            action_state: 0,
            animation_id: 7,
            animation_frame_id: 5,
            timing_control: 3,
            blend_counter: 4,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();

        assert!(update(
            &mut entity,
            &mut clock,
            &clips(),
            &RoomState::default()
        ));
        assert_eq!(entity.action_state, 1);
        assert_eq!(entity.animation_id, 0);
        assert_eq!(entity.animation_frame_id, 0);
        assert_eq!(entity.timing_control, 0);
        assert_eq!(entity.blend_counter, 0);

        // The common tail advances the rewound clip.
        clock.advance(&mut entity, &clips(), false, IDLE_BLEND_STEP);
        assert_eq!(clock.display_frame(), 0);
        assert!(update(
            &mut entity,
            &mut clock,
            &clips(),
            &RoomState::default()
        ));
    }

    #[test]
    fn idle_9_stops_advancing_past_state_1() {
        let mut entity = Entity {
            id: 0x23,
            action_behavior: 9,
            action_state: 2,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();
        assert!(!update(
            &mut entity,
            &mut clock,
            &clips(),
            &RoomState::default()
        ));
    }

    #[test]
    fn idle_0_nops_for_the_living_and_plays_for_the_corpses() {
        let mut living = Entity {
            id: 0x23,
            action_behavior: 0,
            animation_id: 4,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();
        assert!(!update(
            &mut living,
            &mut clock,
            &clips(),
            &RoomState::default()
        ));
        assert_eq!(living.animation_id, 4, "a living character is untouched");

        let mut corpse = Entity {
            id: 0x25,
            action_behavior: 0,
            action_state: 0,
            animation_id: 4,
            ..Entity::default()
        };
        assert!(update(
            &mut corpse,
            &mut clock,
            &clips(),
            &RoomState::default()
        ));
        assert_eq!(corpse.animation_id, 0);
        assert_eq!(corpse.action_state, 1);
    }

    #[test]
    fn nop_behaviours_do_nothing() {
        let mut entity = Entity {
            id: 0x22,
            action_behavior: 5,
            action_state: 3,
            animation_id: 9,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();
        assert!(!update(
            &mut entity,
            &mut clock,
            &clips(),
            &RoomState::default()
        ));
        assert_eq!((entity.action_state, entity.animation_id), (3, 9));
    }

    #[test]
    fn walk_01_runs_its_speed_trim_and_animation() {
        let mut clips = vec![Clip::default(); 0x37];
        let walk = Clip {
            frames: vec![
                ClipFrame {
                    keyframe: 0,
                    timing: 1,
                };
                4
            ],
        };
        clips[0x35] = walk.clone();
        clips[0x36] = walk;
        let mut entity = Entity {
            id: 0x27,
            action_behavior: 1,
            action_state: 0,
            animation_frame_id: 3,
            ..Entity::default()
        };
        let mut clock = EntityAnim::default();

        // The setup call rewinds to frame 0, sets the starting speed and
        // advances once.
        assert!(!update(
            &mut entity,
            &mut clock,
            &clips,
            &RoomState::default()
        ));
        assert_eq!(entity.action_state, 1);
        assert_eq!(entity.animation_id, 0x35);
        assert_eq!(entity.move_speed_current, 1000);
        assert_eq!(clock.display_frame(), 0);

        // The second call trims frame 1's 15 from the speed.
        assert!(!update(
            &mut entity,
            &mut clock,
            &clips,
            &RoomState::default()
        ));
        assert_eq!(entity.move_speed_current, 985);
        assert_eq!(clock.display_frame(), 1);
    }

    #[test]
    fn walk_02_and_walk_03_run_their_animation_state_machines() {
        let instant = ClipFrame {
            keyframe: 0,
            timing: 0,
        };
        let mut clips = vec![Clip::default(); 0x34];
        clips[0x30] = Clip {
            frames: vec![instant],
        };
        clips[0x33] = Clip {
            frames: vec![instant],
        };
        let mut clock = EntityAnim::default();
        let mut entity = Entity {
            id: 0x27,
            action_behavior: 2,
            ..Entity::default()
        };
        // The one-frame clip completes in the setup call, so the death
        // animation parks in its (effect-only) pool state.
        assert!(!update(
            &mut entity,
            &mut clock,
            &clips,
            &RoomState::default()
        ));
        assert_eq!((entity.action_state, entity.animation_id), (2, 0x33));

        let mut entity = Entity {
            id: 0x27,
            action_behavior: 3,
            ..Entity::default()
        };
        assert!(!update(
            &mut entity,
            &mut clock,
            &clips,
            &RoomState::default()
        ));
        assert_eq!(entity.action_state, 2);
        assert_eq!(entity.animation_id, 0x30);
        assert_eq!(entity.hit_state, 0x80);
        assert_eq!(entity.status_flags & 6, 6);

        // State 2 clears status bit 1 and forces health -1.
        entity.health = 40;
        assert!(!update(
            &mut entity,
            &mut clock,
            &clips,
            &RoomState::default()
        ));
        assert_eq!(entity.status_flags & 2, 0);
        assert_eq!(entity.health, -1);
    }
}
