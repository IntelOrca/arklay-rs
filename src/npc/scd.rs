//! State-8 scripted-action handlers.
//!
//! The SCD actor opcodes only pose an entity: `act_anim_flags`/`act_anim_set`/
//! `act_anim_seq` write the state, behaviour, clip and scripted target, and the
//! native driver here runs the per-tick animation and movement. The handlers
//! are the original's behaviour table, indexed by `action_behavior` 0-10:
//!
//! - 00/01/10: plain animation playback (10 loops when `scd_entity_flags` bit
//!   4 asks), 07/09 play once and signal, 09 always in reverse;
//! - 02/03: turn to face the scripted `unk_c6`/`unk_c8` target, walk (03 runs)
//!   to it, trim the speed on the footfall frames and finish in range;
//! - 04/05: walk backwards to the target at the fast/slow pace;
//! - 06: turn in place;
//! - 08: the weapon-fire behaviour: play the scripted clip, spawn the muzzle
//!   flash, the ejected shell and the secondary flash on their trigger frames
//!   (the flamethrower instead sprays a type-0x0C billboard every sixth frame
//!   with a looping sound-cue countdown and a per-frame yaw sweep), then raise
//!   the completion flag the script waits on. An out-of-range `action_behavior`
//!   (>= 11) is a NULL table slot in the original and is recorded in the
//!   placeholder map instead of dispatching.
//!
//! Completion is signalled the way the scripts wait for it: the handler raises
//! `scd_anim_param` in the system flag bank, and the event script's `bit_test`
//! proceeds. A handler that finishes clears `action_behavior`/`action_state`
//! unless the `act_anim_seq` collision flag bit 7 asks it to keep running.

use std::rc::Rc;

use crate::effects::{self, Attach};
use crate::game::{BANK_SYSTEM, Entity, EntitySound, FlagBank, GameState};
use crate::model::Clip;
use crate::state::RoomState;

use super::anim::EntityAnim;
use super::walk;

/// One weapon-FX spawn record: the animation frame that triggers the spawn,
/// the billboard type and depth group, and the local offset in the character's
/// weapon-joint (or own-matrix) space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FireFx {
    /// `animation_frame_id` that fires this record (`0x63` = never).
    frame: u8,
    /// Billboard effect type.
    effect_type: u8,
    /// Depth group.
    depth: u8,
    /// Local offset X.
    x: i16,
    /// Local offset Y.
    y: i16,
    /// Local offset Z.
    z: i16,
}

/// The frame a disabled fire record uses: no shipped animation reaches it.
const FIRE_FX_DISABLED: u8 = 0x63;

/// Muzzle-flash table (record index = `behavior_flags - 2`), spawned in the
/// weapon hand's space.
#[rustfmt::skip]
const FIRE_FX_MUZZLE: [FireFx; 14] = [
    FireFx { frame: 0x01, effect_type: 0x11, depth: 0x00, x:  110, y:  540, z:   0 },
    FireFx { frame: 0x01, effect_type: 0x11, depth: 0x01, x:  640, y: 1110, z:   0 },
    FireFx { frame: 0x01, effect_type: 0x11, depth: 0x02, x:  160, y:  610, z:   0 },
    FireFx { frame: 0x01, effect_type: 0x11, depth: 0x0A, x:  160, y:  610, z:   0 },
    FireFx { frame: 0x00, effect_type: 0x00, depth: 0x00, x:    0, y:    0, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x08, depth: 0x07, x:  400, y:  660, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x08, depth: 0x07, x:  400, y:  660, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x08, depth: 0x07, x:  400, y:  660, z:   0 },
    FireFx { frame: 0x01, effect_type: 0x0B, depth: 0x09, x: -190, y: 1020, z:  90 },
    FireFx { frame: 0x01, effect_type: 0x0B, depth: 0x09, x: -190, y: 1020, z: -60 },
    FireFx { frame: 0x01, effect_type: 0x0B, depth: 0x09, x:  -60, y: 1040, z:  90 },
    FireFx { frame: 0x01, effect_type: 0x0B, depth: 0x09, x:  -60, y: 1040, z: -60 },
    FireFx { frame: 0x01, effect_type: 0x11, depth: 0x00, x:  110, y:  540, z:   0 },
    FireFx { frame: 0x01, effect_type: 0x11, depth: 0x00, x:  600, y: 1370, z:   0 },
];

/// Ejected-shell/smoke table, spawned in the character's own matrix.
#[rustfmt::skip]
const FIRE_FX_SHELL: [FireFx; 14] = [
    FireFx { frame: 0x03, effect_type: 0x05, depth: 0x00, x:  370, y: -2870, z: -220 },
    FireFx { frame: 0x19, effect_type: 0x05, depth: 0x09, x:  360, y: -2050, z: -440 },
    FireFx { frame: FIRE_FX_DISABLED, effect_type: 0x00, depth: 0x00, x: 0, y: 0, z: 0 },
    FireFx { frame: FIRE_FX_DISABLED, effect_type: 0x00, depth: 0x00, x: 0, y: 0, z: 0 },
    FireFx { frame: 0x00, effect_type: 0x00, depth: 0x00, x:    0, y:     0, z:    0 },
    FireFx { frame: FIRE_FX_DISABLED, effect_type: 0x00, depth: 0x00, x: 0, y: 0, z: 0 },
    FireFx { frame: FIRE_FX_DISABLED, effect_type: 0x00, depth: 0x00, x: 0, y: 0, z: 0 },
    FireFx { frame: FIRE_FX_DISABLED, effect_type: 0x00, depth: 0x00, x: 0, y: 0, z: 0 },
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x: 1400, y: -2800, z: -300 },
    FireFx { frame: 0x00, effect_type: 0x00, depth: 0x00, x:    0, y:     0, z:    0 },
    FireFx { frame: 0x00, effect_type: 0x00, depth: 0x00, x:    0, y:     0, z:    0 },
    FireFx { frame: 0x00, effect_type: 0x00, depth: 0x00, x:    0, y:     0, z:    0 },
    FireFx { frame: 0x03, effect_type: 0x05, depth: 0x00, x:  250, y: -1900, z: -250 },
    FireFx { frame: 0x03, effect_type: 0x05, depth: 0x00, x:  250, y: -1900, z: -250 },
];

/// Secondary-flash table, spawned in the weapon hand's space.
#[rustfmt::skip]
const FIRE_FX_FLASH2: [FireFx; 14] = [
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x:  110, y:  500, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x:  640, y: 1060, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x:  160, y:  610, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x:  160, y:  610, z:   0 },
    FireFx { frame: 0x00, effect_type: 0x00, depth: 0x00, x:    0, y:    0, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x:  640, y: 1060, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x:  640, y: 1060, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x:  640, y: 1060, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x08, depth: 0x02, x:  430, y: -830, z:  90 },
    FireFx { frame: 0x02, effect_type: 0x08, depth: 0x02, x:  430, y: -830, z: -60 },
    FireFx { frame: 0x02, effect_type: 0x08, depth: 0x02, x:  570, y: -810, z:  90 },
    FireFx { frame: 0x02, effect_type: 0x08, depth: 0x02, x:  570, y: -810, z: -60 },
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x:  110, y:  500, z:   0 },
    FireFx { frame: 0x02, effect_type: 0x09, depth: 0x0B, x:  640, y: 1500, z:   0 },
];

/// The flamethrower's spray offset in the weapon hand's space.
const FLAME_OFFSET: [i32; 3] = [0x21C, 0x4EC, 0];
/// The flamethrower's sound-cue interval.
const FLAME_CUE_TICKS: u16 = 0x0F;

/// Dispatch one state-8 tick for entity `slot`. The original runs the
/// behaviour handler and then runs it a second time when `scd_entity_flags`
/// bit 1 is set, re-reading `action_behavior` for the repeat.
pub fn update(game: &mut GameState, slot: usize, room: &RoomState, clips: &[Clip]) {
    let behavior = game.entities[slot].action_behavior;
    if behavior >= 11 {
        // The original reports the NULL table slot and dispatches nothing; the
        // count makes the stall visible to tests and debugging.
        *game.npc_placeholders.entry(behavior).or_insert(0) += 1;
        return;
    }
    run(game, slot, room, clips, behavior);
    if game.entities[slot].flags & 2 != 0 {
        let repeat = game.entities[slot].action_behavior;
        run(game, slot, room, clips, repeat);
    }
    // `scd_entity_flags` bit 2 refreshes the held-weapon joint; the weapon
    // TMDs stay unpacked this milestone, so the bit is inert.
    // TODO(parity): (gameplay) the original runs EntityUpdateWeaponJoint on
    // `scd_entity_flags & 4` (hand selected by bit 3); without the held-weapon
    // TMDs the joint is never updated, so a script that raises the bit is a
    // no-op.
}

/// Run one behaviour handler with the entity, its clock, the system flag bank
/// and the entity sound queue split out of the game state.
fn run(game: &mut GameState, slot: usize, room: &RoomState, clips: &[Clip], behavior: u8) {
    // The fire handler needs the whole game state (the effect pool, the room
    // and weapon sprite metadata), so it runs before the field split.
    if behavior == 8 {
        handler_08(game, slot, room, clips);
        return;
    }
    let slow = game.flags[5].bit(crate::game::MSF2_EFFECT_ZONE);
    let GameState {
        entities,
        entity_anims,
        flags,
        entity_sounds,
        npc_placeholders,
        ..
    } = game;
    let entity = &mut entities[slot];
    let clock = &mut entity_anims[slot];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    match behavior {
        0 => handler_00(entity, clock, clips),
        1 => handler_01(entity, clock, clips, system),
        2 => handler_02(entity, clock, clips, room, system, entity_sounds, slow),
        3 => handler_03(entity, clock, clips, room, system, entity_sounds, slow),
        4 => handler_04(entity, clock, clips, room, system, entity_sounds, slow),
        5 => handler_05(entity, clock, clips, room, system, entity_sounds, slow),
        6 => handler_06(entity, clock, clips, system),
        7 => handler_07(entity, clock, clips, system),
        9 => handler_09(entity, clock, clips, system),
        10 => handler_10(entity, clock, clips, system),
        _ => {
            // The dispatch above handles 8 and `update` refuses >= 11.
            *npc_placeholders.entry(behavior).or_insert(0) += 1;
        }
    }
}

/// Raise the completion flag `scd_anim_param` in the system bank.
fn raise(system: &mut FlagBank, entity: &Entity) {
    system.apply(entity.scd_anim_param, 0);
}

/// The scripted walk target (`unk_c6`, 0, `unk_c8`).
fn scripted_target(entity: &Entity) -> [i32; 3] {
    [i32::from(entity.unk_c6), 0, i32::from(entity.unk_c8)]
}

/// Clear the behaviour when the arrival's collision flag bit 7 is clear.
fn finish_walk(entity: &mut Entity, system: &mut FlagBank) {
    raise(system, entity);
    if entity.collision_flags & 0x80 == 0 {
        entity.action_behavior = 0;
        entity.action_state = 0;
    }
}

/// Behaviour 0: play the scripted clip and nothing else. There is no
/// completion state; the script ends it with an explicit opcode.
fn handler_00(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip]) {
    if entity.action_state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.animation_id = 0;
        entity.action_state = 1;
        entity.blend_counter = 7;
    } else if entity.action_state != 1 {
        return;
    }
    clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
}

/// Behaviour 1: plain animation playback. Flag 0x80 holds every frame for an
/// extra tick; completion raises the script's flag and adds `scd_timer` to the
/// yaw once. Flag 0x10 loops back to the start instead of parking at state 2.
fn handler_01(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], system: &mut FlagBank) {
    let state = entity.action_state;
    if state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.action_state = 1;
        entity.blend_counter = 7;
        entity.move_speed_current = 0;
        if entity.flags & 0x20 != 0 {
            entity.blend_counter = 0;
        }
        entity.action_ticks_counter = 1;
    } else if state != 1 {
        if state != 2 {
            return;
        }
        raise(system, entity);
        entity.scd_timer = 0;
        if entity.flags & 0x10 == 0 {
            return;
        }
        entity.action_state = 0;
        return;
    }

    if entity.flags & 0x80 != 0 {
        let previous = entity.action_ticks_counter;
        entity.action_ticks_counter = previous.wrapping_sub(1);
        if previous == 0 {
            entity.action_ticks_counter = 1;
            return;
        }
    }

    if clock.advance(entity, clips, entity.flags & 1 != 0, 0x200) {
        entity.action_state = 2;
        entity.angle = entity.angle.wrapping_add(entity.scd_timer);
    }
}

/// Behaviour 2: turn in place until aligned within 0x16A, then walk to the
/// scripted target with a footstep on frames 8 and 0x16, finishing within 150
/// units.
fn handler_02(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    clips: &[Clip],
    room: &RoomState,
    system: &mut FlagBank,
    sounds: &mut Vec<EntitySound>,
    slow: bool,
) {
    let target = scripted_target(entity);
    match entity.action_state {
        0 => {
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.animation_id = 7;
            entity.action_state = 1;
            entity.blend_counter = 7;
            handler_02_turn(entity, clock, clips, target);
        }
        1 => handler_02_turn(entity, clock, clips, target),
        2 => {
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.animation_id = 7;
            entity.action_state = 3;
            entity.blend_counter = 7;
            handler_02_walk(entity, clock, clips, room, system, sounds, slow, target);
        }
        3 => handler_02_walk(entity, clock, clips, room, system, sounds, slow, target),
        _ => {}
    }
}

fn handler_02_turn(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], target: [i32; 3]) {
    let step = entity.scd_timer as i16;
    let turn = walk::turn_toward_target(entity, target, step);
    entity.angle = entity.angle.wrapping_add(turn as u16);
    clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
    if walk::turn_toward_target(entity, target, 0x16A) == 0 {
        entity.action_state = 2;
    }
}

#[allow(clippy::too_many_arguments)] // the handler signature is fixed
fn handler_02_walk(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    clips: &[Clip],
    room: &RoomState,
    system: &mut FlagBank,
    sounds: &mut Vec<EntitySound>,
    slow: bool,
    target: [i32; 3],
) {
    let frame = entity.animation_frame_id;
    if frame == 8 || frame == 0x16 {
        walk::footstep(sounds, room, entity, 0, slow);
    }
    walk::entity_apply_walk_speed(entity, 0x5D);
    walk::rotate_toward_target(entity, target, entity.scd_timer);
    clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
    walk::advance_xz_blocked(room, entity, 0, entity.move_speed_current as i16);
    if walk::xz_distance_to(entity, target) < 0x96 {
        finish_walk(entity, system);
    }
}

/// Behaviour 3: turn to face the target, run to it, then decelerate for four
/// frames before signalling. The collision flag bit 7 finishes immediately in
/// range instead of decelerating.
fn handler_03(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    clips: &[Clip],
    room: &RoomState,
    system: &mut FlagBank,
    sounds: &mut Vec<EntitySound>,
    slow: bool,
) {
    let target = scripted_target(entity);
    match entity.action_state {
        0 => {
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.animation_id = 8;
            entity.action_state = 1;
            entity.blend_counter = 7;
            handler_03_turn(entity, clock, clips, target);
        }
        1 => handler_03_turn(entity, clock, clips, target),
        2 => {
            entity.move_speed_current = 0xD2;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.animation_id = 8;
            entity.action_state = 3;
            entity.blend_counter = 7;
            handler_03_walk(entity, clock, clips, room, system, sounds, slow, target);
        }
        3 => handler_03_walk(entity, clock, clips, room, system, sounds, slow, target),
        4 => {
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.animation_id = 0;
            entity.action_state = 5;
            entity.blend_counter = 7;
            entity.action_ticks_counter = 0;
            handler_03_stop(entity, clock, clips, room);
        }
        5 => handler_03_stop(entity, clock, clips, room),
        6 => {
            entity.action_behavior = 0;
            entity.action_state = 0;
            raise(system, entity);
        }
        _ => {}
    }
}

fn handler_03_turn(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], target: [i32; 3]) {
    let step = entity.scd_timer as i16;
    let turn = walk::turn_toward_target(entity, target, step);
    entity.angle = entity.angle.wrapping_add(turn as u16);
    clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
    if walk::turn_toward_target(entity, target, 0x16A) == 0 {
        entity.action_state = 2;
    }
}

#[allow(clippy::too_many_arguments)] // the handler signature is fixed
fn handler_03_walk(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    clips: &[Clip],
    room: &RoomState,
    system: &mut FlagBank,
    sounds: &mut Vec<EntitySound>,
    slow: bool,
    target: [i32; 3],
) {
    let frame = entity.animation_frame_id;
    if frame == 0 || frame == 10 {
        walk::footstep(sounds, room, entity, 1, slow);
    }
    walk::rotate_toward_target(entity, target, entity.scd_timer);
    clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
    walk::advance_xz_blocked(room, entity, 0, entity.move_speed_current as i16);
    if walk::xz_distance_to(entity, target) < 0xFA {
        entity.action_state = 4;
        if entity.collision_flags & 0x80 != 0 {
            entity.action_state = 3;
            raise(system, entity);
        }
    }
}

fn handler_03_stop(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], room: &RoomState) {
    clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
    entity.action_ticks_counter = entity.action_ticks_counter.wrapping_add(1);
    if (entity.action_ticks_counter as i16) > 3 {
        entity.action_state = 6;
    }
    entity.move_speed_current = (entity.move_speed_current as i16).wrapping_sub(0x1E) as u16;
    walk::advance_xz_blocked(room, entity, 0, entity.move_speed_current as i16);
}

/// The shared state 0/1 body of the backward walks: flip the facing 180
/// degrees, steer the back onto the target, flip back, then move backwards
/// along the heading. Arrival is 100 units.
fn backward_step(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    clips: &[Clip],
    room: &RoomState,
    system: &mut FlagBank,
    target: [i32; 3],
) {
    entity.angle = entity.angle.wrapping_add(0x800) & 0x0FFF;
    walk::rotate_toward_target(entity, target, entity.scd_timer);
    entity.angle = entity.angle.wrapping_sub(0x800);
    clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
    walk::advance_xz_blocked(room, entity, 0x800, entity.move_speed_current as i16);
    if walk::xz_distance_to(entity, target) < 100 {
        finish_walk(entity, system);
    }
}

/// Behaviour 4: walk backwards at the fast pace (animation 3), with a
/// footstep on frames 8 and 0x16.
fn handler_04(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    clips: &[Clip],
    room: &RoomState,
    system: &mut FlagBank,
    sounds: &mut Vec<EntitySound>,
    slow: bool,
) {
    let target = scripted_target(entity);
    if entity.action_state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.action_state = 1;
        entity.blend_counter = 7;
        entity.animation_id = 3;
    } else if entity.action_state != 1 {
        return;
    }

    let frame = entity.animation_frame_id;
    if frame == 8 || frame == 0x16 {
        walk::footstep(sounds, room, entity, 0, slow);
    }
    entity.move_speed_current = 0x3C;
    // The original's `frame > 4 || frame < 8` is true for every byte, so the
    // +4 always applies; kept as written.
    if frame > 4 || frame < 8 {
        entity.move_speed_current = (entity.move_speed_current as i16).wrapping_add(4) as u16;
    }
    backward_step(entity, clock, clips, room, system, target);
}

/// Behaviour 5: walk backwards at the slow pace (animation 2). The footsteps
/// fire only while `timing_control` is exactly 2, so a held frame steps once.
fn handler_05(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    clips: &[Clip],
    room: &RoomState,
    system: &mut FlagBank,
    sounds: &mut Vec<EntitySound>,
    slow: bool,
) {
    let target = scripted_target(entity);
    if entity.action_state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.action_state = 1;
        entity.blend_counter = 7;
        entity.animation_id = 2;
        entity.move_speed_current = 0x1D;
    } else if entity.action_state != 1 {
        return;
    }

    let frame = entity.animation_frame_id as i8;
    if (frame == 7 || frame == 0x1B) && entity.timing_control as i8 == 2 {
        walk::footstep(sounds, room, entity, 0, slow);
    }
    backward_step(entity, clock, clips, room, system, target);
}

/// Behaviour 6: turn in place toward the scripted target, finishing once
/// aligned within 0x28.
fn handler_06(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], system: &mut FlagBank) {
    let state = entity.action_state;
    if state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.animation_id = 7;
        entity.action_state = 1;
        entity.blend_counter = 7;
    } else if state != 1 {
        if state != 2 {
            return;
        }
        entity.action_behavior = 0;
        entity.action_state = 0;
        raise(system, entity);
        return;
    }

    let target = scripted_target(entity);
    walk::rotate_toward_target(entity, target, entity.scd_timer);
    clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
    if walk::turn_toward_target(entity, target, 0x28) == 0 {
        entity.action_state = 2;
    }
}

/// Behaviour 7: play the scripted clip to its end, signal, and add
/// `scd_timer` to the yaw on every tick including after completion.
fn handler_07(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], system: &mut FlagBank) {
    let state = entity.action_state as i8;
    if state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.action_state = 1;
        entity.blend_counter = 7;
    } else if state != 1 {
        if state == 2 {
            raise(system, entity);
        }
        entity.angle = entity.angle.wrapping_add(entity.scd_timer);
        return;
    }

    let done = clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
    entity.action_state = entity.action_state.wrapping_add(u8::from(done));
    entity.angle = entity.angle.wrapping_add(entity.scd_timer);
}

/// Behaviour 9: play the scripted clip in reverse (hard-coded, not the
/// `scd_entity_flags` bit), then clear the behaviour and signal.
fn handler_09(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], system: &mut FlagBank) {
    let state = entity.action_state as i8;
    if state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.action_state = 1;
        entity.blend_counter = 3;
    } else if state != 1 {
        if state != 2 {
            return;
        }
        entity.action_behavior = 0;
        entity.action_state = 0;
        raise(system, entity);
        return;
    }

    let done = clock.advance(entity, clips, true, 0x400);
    entity.action_state = entity.action_state.wrapping_add(u8::from(done));
}

/// Behaviour 10: play the clip, signal, and loop back to the start while
/// `scd_entity_flags` bit 4 is set.
fn handler_10(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip], system: &mut FlagBank) {
    let state = entity.action_state as i8;
    if state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.action_state = 1;
        entity.blend_counter = 7;
        entity.move_speed_current = 0;
    } else if state != 1 {
        if state != 2 {
            return;
        }
        raise(system, entity);
        if entity.flags & 0x10 == 0 {
            return;
        }
        entity.action_state = 0;
        return;
    }

    let done = clock.advance(entity, clips, entity.flags & 1 != 0, 0x200);
    entity.action_state = entity.action_state.wrapping_add(u8::from(done));
}

/// Behaviour 8: the weapon-fire handler.
///
/// The weapon selector is the spawn record's `behavior_flags - 2`. States 0/1
/// play the scripted clip and spawn the muzzle flash, the ejected shell and
/// the secondary flash from the three per-weapon trigger tables; state 2
/// raises `scd_anim_param` in the system bank, which is the flag a waiting
/// `dountil` loop tests; weapon id 3 skips the shot effects and plays
/// animation 0x17 straight into the flamethrower states 4/5 (a type-0x0C
/// billboard every sixth frame, a sound-cue countdown and a per-frame yaw
/// sweep).
///
/// # Documented deviations
///
/// The gunfire spawns live in the character's weapon-hand joint space in the
/// original (`joints + 0x70C` for the muzzle and secondary flash, the entity's
/// own matrix for the shell). This port has no per-joint matrices at the game
/// layer, so both resolve to the character's entity matrix
/// ([`Attach::Entity`]) - the same approximation the effect engine's attach
/// seam documents. The per-frame offsets are the original's, so the flash
/// pivots around the character's origin instead of the raised hand.
///
/// The flamethrower's two looping 3D sound cues (enemy bank ids 0x1E and
/// 0x1F) are not queued: the pack ships no enemy sound bank, so only the cue
/// timing is modelled.
fn handler_08(game: &mut GameState, slot: usize, _room: &RoomState, clips: &[Clip]) {
    let state = game.entities[slot].action_state;
    let weapon = game.entities[slot].behavior_flags.wrapping_sub(2);
    match state {
        0 => {
            {
                let entity = &mut game.entities[slot];
                entity.animation_frame_id = 0;
                entity.timing_control = 0;
                entity.action_state = 1;
                entity.blend_counter = 3;
                if weapon == 3 {
                    entity.action_state = 3;
                    entity.animation_id = 0x17;
                }
            }
            if weapon != 3 {
                fire_fx_spawns(game, slot, weapon);
            }
            fire_play_anim(game, slot, clips);
        }
        1 => {
            fire_fx_spawns(game, slot, weapon);
            fire_play_anim(game, slot, clips);
        }
        2 => {
            let entity = game.entities[slot];
            let system = &mut game.flags[usize::from(BANK_SYSTEM)];
            raise(system, &entity);
        }
        3 => fire_play_anim(game, slot, clips),
        4 => {
            {
                let entity = &mut game.entities[slot];
                entity.action_state = 5;
                entity.timing_control = 0;
                entity.animation_id = 0x14;
                entity.blend_counter = 3;
                entity.action_ticks_counter = FLAME_CUE_TICKS;
            }
            fire_flame_step(game, slot, clips);
        }
        5 => fire_flame_step(game, slot, clips),
        _ => {}
    }
}

/// Spawn the weapon's muzzle flash, ejected shell and secondary flash for the
/// current animation frame, exactly in the original's order. The returned
/// shell/flash slots get the weapon id copied into their animation header, the
/// tag the behaviours read.
fn fire_fx_spawns(game: &mut GameState, slot: usize, weapon: u8) {
    if usize::from(weapon) >= FIRE_FX_MUZZLE.len() {
        // The original indexes the tables with a raw byte and never bounds it;
        // a weapon id this high means `behavior_flags` was never initialised.
        return;
    }
    let entity = game.entities[slot];
    let frame = entity.animation_frame_id;
    let attach = Attach::Entity(slot as u8);
    let room_effects = Rc::clone(&game.room_effects);
    let row = FIRE_FX_MUZZLE[usize::from(weapon)];

    if frame == row.frame {
        let pos = [i32::from(row.x), i32::from(row.y), i32::from(row.z)];
        effects::create_attached(
            game,
            &room_effects,
            row.effect_type,
            row.depth,
            attach,
            pos,
            0,
            0,
        );
        if weapon == 2 {
            effects::create_attached(
                game,
                &room_effects,
                0x11,
                0x03,
                attach,
                [0x96, 0x17C, 0],
                0,
                0,
            );
        }
    }

    let row = FIRE_FX_SHELL[usize::from(weapon)];
    if frame == row.frame {
        // Odd-id characters lift the shell by 300, scaled by (1 - weapon) -
        // negative for weapon >= 2, exactly the original's signed arithmetic.
        let lift = i32::from(entity.id & 1) * (1 - i32::from(weapon)) * 300;
        let pos = [i32::from(row.x), lift + i32::from(row.y), i32::from(row.z)];
        let yaw = if weapon == 8 { 0 } else { 0x555 };
        if let Some(child) = effects::create_attached(
            game,
            &room_effects,
            row.effect_type,
            row.depth,
            attach,
            pos,
            yaw,
            0,
        ) {
            fire_fx_tag(game, child, 3, weapon);
        }
    }

    let row = FIRE_FX_FLASH2[usize::from(weapon)];
    if frame == row.frame {
        let pos = [i32::from(row.x), i32::from(row.y), i32::from(row.z)];
        if let Some(child) = effects::create_attached(
            game,
            &room_effects,
            row.effect_type,
            row.depth,
            attach,
            pos,
            0,
            0,
        ) {
            fire_fx_tag(game, child, 0, weapon);
        }
    }
}

/// Copy the weapon id into one header byte of a spawned slot (the original's
/// `fire_fx_tag`). A full pool never yields a slot, so no stray write occurs.
fn fire_fx_tag(game: &mut GameState, slot: u8, field: usize, weapon: u8) {
    if let Some(effect) = game.effects.slot_mut(usize::from(slot))
        && let Some(byte) = effect.header.get_mut(field)
    {
        *byte = weapon;
    }
}

/// The shared animation step of states 0/1/3: `Joint_move(0, ..., 0x400)` and
/// add the completion result to the action state.
fn fire_play_anim(game: &mut GameState, slot: usize, clips: &[Clip]) {
    let (entities, anims) = (&mut game.entities, &mut game.entity_anims);
    let done = anims[slot].advance(&mut entities[slot], clips, false, 0x400);
    let entity = &mut entities[slot];
    entity.action_state = entity.action_state.wrapping_add(u8::from(done));
}

/// State 5: spray a type-0x0C billboard every sixth frame, count the looping
/// sound cue down, advance the clip and sweep the yaw by `scd_timer`.
fn fire_flame_step(game: &mut GameState, slot: usize, clips: &[Clip]) {
    if game.entities[slot].animation_frame_id.is_multiple_of(6) {
        let attach = Attach::Entity(slot as u8);
        let room_effects = Rc::clone(&game.room_effects);
        effects::create_attached(game, &room_effects, 0x0C, 0, attach, FLAME_OFFSET, 0, 0);
    }
    let ticks = game.entities[slot].action_ticks_counter;
    game.entities[slot].action_ticks_counter = ticks.wrapping_sub(1);
    if ticks == 0 {
        game.entities[slot].action_ticks_counter = FLAME_CUE_TICKS;
        // TODO(parity): (audio) the original queues the two flamethrower 3D
        // cues here (enemy bank ids 0x1E/0x1F); the pack ships no enemy sound
        // bank, so the cue reload is modelled and the audio is deferred.
    }
    let (entities, anims) = (&mut game.entities, &mut game.entity_anims);
    anims[slot].advance(&mut entities[slot], clips, false, 0x400);
    let entity = &mut entities[slot];
    entity.angle = entity.angle.wrapping_add(entity.scd_timer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::fixtures::{block, sprite};
    use crate::game::Entity;
    use crate::model::ClipFrame;

    fn clip(frames: usize, timing: u16) -> Clip {
        Clip {
            frames: vec![
                ClipFrame {
                    keyframe: 0,
                    timing,
                };
                frames
            ],
        }
    }

    /// Instant one-frame clips for every animation id the handlers select, so
    /// a handler completes on the advance after its setup.
    fn clips() -> Vec<Clip> {
        vec![clip(1, 0); 0x30]
    }

    /// One frame per tick for every clip the fire handler plays.
    fn fire_clips() -> Vec<Clip> {
        vec![clip(0x40, 1); 0x30]
    }

    fn game_with(entity: Entity) -> GameState {
        let mut game = GameState::default();
        game.entities[1] = entity;
        game.entities[1].set_active(true);
        game
    }

    /// A game with the six weapon effect sprites the fire tables reference,
    /// each carrying a full eight-row animation table.
    fn fire_game(entity: Entity) -> GameState {
        let mut game = game_with(entity);
        for index in [0u8, 5, 8, 9, 11, 12, 17] {
            game.weapon_effects.sprites.push(sprite(
                index,
                std::array::from_fn(|_| vec![vec![block(1, 0, 0)]]),
            ));
        }
        game
    }

    fn state8(behavior: u8) -> Entity {
        Entity {
            id: 0x27,
            action_behavior: behavior,
            ..Entity::default()
        }
    }

    fn system_bit(game: &GameState, bit: u8) -> bool {
        game.flags[usize::from(BANK_SYSTEM)].bit(bit)
    }

    #[test]
    fn handler_01_plays_and_raises_the_completion_flag() {
        let mut entity = state8(1);
        entity.flags = 0x10;
        entity.scd_anim_param = 0x21;
        let mut game = game_with(entity);
        update(&mut game, 1, &RoomState::default(), &clips());
        assert_eq!(game.entities[1].action_state, 2, "the clip completed");

        // The next tick raises the wait bit and the 0x10 loop flag resets the
        // behaviour; the timer is cleared on the way.
        update(&mut game, 1, &RoomState::default(), &clips());
        assert!(system_bit(&game, 0x21), "the script's wait bit is raised");
        assert_eq!(game.entities[1].scd_timer, 0);
        assert_eq!(game.entities[1].action_state, 0, "flag 0x10 loops");
    }

    #[test]
    fn handler_01_hold_flag_skips_every_other_tick() {
        let mut entity = state8(1);
        entity.flags = 0x80;
        entity.action_state = 1;
        entity.action_ticks_counter = 0;
        let mut game = game_with(entity);
        let before = game.entities[1].animation_frame_id;
        update(&mut game, 1, &RoomState::default(), &clips());
        assert_eq!(
            game.entities[1].animation_frame_id, before,
            "the held tick does not consume a frame"
        );
        assert_eq!(game.entities[1].action_ticks_counter, 1);
    }

    #[test]
    fn handler_02_turns_then_walks_to_the_scripted_target() {
        let mut entity = state8(2);
        entity.unk_c6 = 1000;
        entity.unk_c8 = 0;
        entity.scd_timer = 0x40;
        entity.scd_anim_param = 0x21;
        let mut game = game_with(entity);
        let room = RoomState::default();
        for _ in 0..200 {
            update(&mut game, 1, &room, &clips());
            if game.entities[1].action_behavior == 0 {
                break;
            }
        }
        let entity = game.entities[1];
        assert_eq!(entity.action_behavior, 0, "the walk completed");
        assert!(walk::xz_distance_to(&entity, [1000, 0, 0]) < 0x96);
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn handler_02_keeps_running_when_the_collision_flag_is_set() {
        let mut entity = state8(2);
        entity.unk_c6 = 100;
        entity.unk_c8 = 0;
        entity.scd_timer = 0x40;
        entity.scd_anim_param = 0x21;
        entity.collision_flags = 0x80;
        let mut game = game_with(entity);
        let room = RoomState::default();
        for _ in 0..60 {
            update(&mut game, 1, &room, &clips());
        }
        assert_eq!(game.entities[1].action_behavior, 2, "kept by flag 0x80");
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn handler_03_runs_to_the_target_and_decelerates() {
        let mut entity = state8(3);
        entity.unk_c6 = 2000;
        entity.unk_c8 = 0;
        entity.scd_timer = 0x40;
        entity.scd_anim_param = 0x21;
        let mut game = game_with(entity);
        let room = RoomState::default();
        let start = game.entities[1].pos;
        for _ in 0..200 {
            update(&mut game, 1, &room, &clips());
            if game.entities[1].action_behavior == 0 {
                break;
            }
        }
        let entity = game.entities[1];
        assert_eq!(entity.action_behavior, 0);
        assert_ne!(entity.pos, start, "the run moved the entity");
        assert!(walk::xz_distance_to(&entity, [2000, 0, 0]) < 0x300);
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn handler_04_walks_backwards_toward_the_target() {
        let mut entity = state8(4);
        entity.unk_c6 = 600;
        entity.unk_c8 = 0;
        entity.scd_timer = 0x40;
        entity.scd_anim_param = 0x21;
        let mut game = game_with(entity);
        let room = RoomState::default();
        for _ in 0..200 {
            update(&mut game, 1, &room, &clips());
            if game.entities[1].action_behavior == 0 {
                break;
            }
        }
        let entity = game.entities[1];
        assert_eq!(entity.action_behavior, 0);
        assert!(walk::xz_distance_to(&entity, [600, 0, 0]) < 100);
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn handler_05_footsteps_only_while_timing_is_two() {
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
        let mut entity = state8(5);
        entity.action_state = 1;
        entity.unk_c6 = 0;
        entity.unk_c8 = 1000;
        entity.scd_timer = 0x40;
        entity.animation_id = 2;
        entity.animation_frame_id = 7;
        entity.timing_control = 2;
        entity.move_speed_current = 0x1D;
        let mut game = game_with(entity);
        update(&mut game, 1, &room, &clips());
        assert_eq!(game.entity_sounds.len(), 1, "frame 7 with timing 2 steps");
        assert_eq!(game.entity_sounds[0].name, "ft_wdA");

        // The same frame with timing 1 is a held tick, not a contact.
        let mut entity = state8(5);
        entity.action_state = 1;
        entity.unk_c6 = 0;
        entity.unk_c8 = 1000;
        entity.scd_timer = 0x40;
        entity.animation_id = 2;
        entity.animation_frame_id = 7;
        entity.timing_control = 1;
        let mut game = game_with(entity);
        update(&mut game, 1, &room, &clips());
        assert!(game.entity_sounds.is_empty());
    }

    #[test]
    fn handler_06_turns_in_place_then_signals() {
        let mut entity = state8(6);
        entity.unk_c6 = 0;
        entity.unk_c8 = 100;
        entity.scd_timer = 0x40;
        entity.scd_anim_param = 0x21;
        let mut game = game_with(entity);
        let room = RoomState::default();
        for _ in 0..40 {
            update(&mut game, 1, &room, &clips());
            if game.entities[1].action_behavior == 0 {
                break;
            }
        }
        assert_eq!(game.entities[1].action_behavior, 0);
        assert_eq!(game.entities[1].angle, 0xC00, "facing the target");
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn handler_07_signals_and_applies_the_yaw_step() {
        let mut entity = state8(7);
        entity.flags = 0;
        entity.scd_timer = 0x10;
        entity.scd_anim_param = 0x21;
        let mut game = game_with(entity);
        update(&mut game, 1, &RoomState::default(), &clips());
        assert_eq!(game.entities[1].action_state, 2);
        assert_eq!(game.entities[1].angle, 0x10);
        update(&mut game, 1, &RoomState::default(), &clips());
        assert!(system_bit(&game, 0x21));
        assert_eq!(game.entities[1].angle, 0x20, "the yaw step runs on state 2");
    }

    #[test]
    fn handler_09_plays_in_reverse_then_clears() {
        let mut entity = state8(9);
        entity.scd_anim_param = 0x21;
        let mut game = game_with(entity);
        for _ in 0..2 {
            update(&mut game, 1, &RoomState::default(), &clips());
        }
        assert_eq!(game.entities[1].action_behavior, 0);
        assert_eq!(game.entities[1].action_state, 0);
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn handler_10_loops_with_bit_4() {
        let mut entity = state8(10);
        entity.flags = 0x10;
        entity.scd_anim_param = 0x21;
        let mut game = game_with(entity);
        for _ in 0..4 {
            update(&mut game, 1, &RoomState::default(), &clips());
        }
        assert_eq!(game.entities[1].action_state, 0, "flag 0x10 re-arms");
        assert!(system_bit(&game, 0x21));

        let mut entity = state8(10);
        entity.scd_anim_param = 0x22;
        let mut game = game_with(entity);
        for _ in 0..4 {
            update(&mut game, 1, &RoomState::default(), &clips());
        }
        assert_eq!(
            game.entities[1].action_state, 2,
            "without the flag it parks"
        );
        assert!(system_bit(&game, 0x22));
    }

    #[test]
    fn handler_08_bounds_and_empty_rows() {
        assert_eq!(FIRE_FX_MUZZLE.len(), 14);
        assert_eq!(FIRE_FX_SHELL.len(), 14);
        assert_eq!(FIRE_FX_FLASH2.len(), 14);
        // Weapon 2 (Barry): the muzzle at frame 1, the special depth-3 extra,
        // the disabled shell and the frame-2 secondary flash.
        assert_eq!(
            FIRE_FX_MUZZLE[2],
            FireFx {
                frame: 1,
                effect_type: 0x11,
                depth: 2,
                x: 160,
                y: 610,
                z: 0
            }
        );
        assert_eq!(
            FIRE_FX_SHELL[0],
            FireFx {
                frame: 3,
                effect_type: 0x05,
                depth: 0,
                x: 370,
                y: -2870,
                z: -220
            }
        );
        assert_eq!(
            FIRE_FX_SHELL[1],
            FireFx {
                frame: 0x19,
                effect_type: 0x05,
                depth: 9,
                x: 360,
                y: -2050,
                z: -440
            }
        );
        assert_eq!(FIRE_FX_SHELL[2].frame, FIRE_FX_DISABLED);
        assert_eq!(
            FIRE_FX_FLASH2[8],
            FireFx {
                frame: 2,
                effect_type: 0x08,
                depth: 2,
                x: 430,
                y: -830,
                z: 90
            }
        );
        assert_eq!(
            FIRE_FX_FLASH2[13],
            FireFx {
                frame: 2,
                effect_type: 0x09,
                depth: 0x0B,
                x: 640,
                y: 1500,
                z: 0
            }
        );

        // A weapon id past the tables spawns nothing (the original's
        // unbounded index is reported by the port instead of read).
        let mut entity = Entity {
            id: 0x27,
            behavior_flags: 200,
            action_behavior: 8,
            scd_anim_param: 0x21,
            ..Entity::default()
        };
        entity.animation_id = 0x11;
        let mut game = fire_game(entity);
        update(&mut game, 1, &RoomState::default(), &fire_clips());
        assert_eq!(game.effects.active_count(), 0);
    }

    #[test]
    fn handler_08_spawns_the_muzzle_and_secondary_flash_on_their_frames() {
        // Barry's record: behavior_flags 4 -> weapon 2.
        let mut entity = Entity {
            id: 0x22,
            behavior_flags: 4,
            action_behavior: 8,
            scd_anim_param: 0x21,
            animation_id: 0x11,
            ..Entity::default()
        };
        entity.set_active(true);
        let mut game = fire_game(entity);
        let room = RoomState::default();
        let clips = fire_clips();

        // Frame 0: nothing fires; the clock publishes frame 1.
        update(&mut game, 1, &room, &clips);
        assert_eq!(game.effects.active_count(), 0);
        assert_eq!(game.entities[1].animation_frame_id, 1);

        // Frame 1: the muzzle flash (depth 2) and the weapon-2 extra (depth 3).
        update(&mut game, 1, &room, &clips);
        let spawned: Vec<(u8, u8)> = game
            .effects
            .active()
            .map(|(_, effect)| (effect.effect_type, effect.depth_group))
            .collect();
        assert_eq!(spawned, vec![(0x11, 3), (0x11, 2)], "two muzzle slots");
        assert_eq!(game.entities[1].animation_frame_id, 2);

        // Frame 2: the secondary flash, tagged header[0] with the weapon id.
        update(&mut game, 1, &room, &clips);
        let flash = game
            .effects
            .active()
            .find(|(_, effect)| effect.effect_type == 9)
            .expect("the secondary flash spawned");
        assert_eq!(flash.1.depth_group, 0x0B);
        assert_eq!(flash.1.header[0], 2);
        assert_eq!(flash.1.attach, effects::Attach::Entity(1));

        // The clip runs out, state 2 raises the wait bit on the next tick.
        for _ in 0..0x80 {
            if game.entities[1].action_state == 2 {
                break;
            }
            update(&mut game, 1, &room, &clips);
        }
        assert_eq!(game.entities[1].action_state, 2);
        update(&mut game, 1, &room, &clips);
        assert!(system_bit(&game, 0x21), "the script's wait bit is raised");
    }

    #[test]
    fn handler_08_shell_tag_and_lift_match_the_record() {
        // Weapon 0 with an odd id: the shell lifts by 300.
        let entity = Entity {
            id: 0x27,
            behavior_flags: 2,
            action_behavior: 8,
            scd_anim_param: 0x21,
            animation_id: 0x11,
            ..Entity::default()
        };
        let mut game = fire_game(entity);
        let room = RoomState::default();
        let clips = fire_clips();

        for _ in 0..4 {
            update(&mut game, 1, &room, &clips);
        }
        let shell = game
            .effects
            .active()
            .find(|(_, effect)| effect.effect_type == 5)
            .expect("the shell spawned on frame 3");
        assert_eq!(shell.1.depth_group, 0);
        assert_eq!(shell.1.local_offset, [370, -2570, -220], "lifted by 300");
        assert_eq!(shell.1.yaw, 0x555, "the non-weapon-8 shell yaw");
        assert_eq!(shell.1.header[3], 0, "tagged with the weapon id");
    }

    #[test]
    fn handler_08_state2_raises_the_completion_flag() {
        let entity = Entity {
            id: 0x22,
            behavior_flags: 4,
            action_behavior: 8,
            action_state: 2,
            scd_anim_param: 0x21,
            ..Entity::default()
        };
        let mut game = fire_game(entity);
        update(&mut game, 1, &RoomState::default(), &fire_clips());
        assert!(system_bit(&game, 0x21));
        assert_eq!(game.entities[1].action_behavior, 8, "state 2 never clears");
    }

    #[test]
    fn handler_08_weapon3_plays_0x17_without_effects_into_the_flame_loop() {
        // Weapon 3 (behavior_flags 5) is the flamethrower: no shot effects.
        let entity = Entity {
            id: 0x2b,
            behavior_flags: 5,
            action_behavior: 8,
            scd_anim_param: 0x21,
            ..Entity::default()
        };
        let mut game = fire_game(entity);
        let room = RoomState::default();
        let clips = fire_clips();

        update(&mut game, 1, &room, &clips);
        assert_eq!(game.entities[1].action_state, 3);
        assert_eq!(game.entities[1].animation_id, 0x17);
        assert_eq!(game.effects.active_count(), 0, "no shot effects");

        // Run out the 0x17 clip: state 4 sets up 0x14 and sprays frame 0.
        for _ in 0..0x40 {
            if game.entities[1].action_state == 5 {
                break;
            }
            update(&mut game, 1, &room, &clips);
        }
        assert_eq!(game.entities[1].action_state, 5);
        assert_eq!(game.entities[1].animation_id, 0x14);
        assert!(
            game.effects
                .active()
                .any(|(_, effect)| effect.effect_type == 0x0C),
            "the flamethrower spray spawned"
        );
        assert_eq!(game.entities[1].action_ticks_counter, 0x0E);
    }

    #[test]
    fn handler_08_flame_loop_spawns_every_sixth_frame_and_sweeps_the_yaw() {
        let entity = Entity {
            id: 0x2b,
            behavior_flags: 5,
            action_behavior: 8,
            action_state: 5,
            animation_id: 0x14,
            animation_frame_id: 0,
            action_ticks_counter: 0x0F,
            scd_timer: 0x10,
            scd_anim_param: 0x21,
            ..Entity::default()
        };
        let mut game = fire_game(entity);
        let room = RoomState::default();
        let clips = fire_clips();

        update(&mut game, 1, &room, &clips);
        assert_eq!(game.effects.active_count(), 1, "frame 0 sprays");
        assert_eq!(game.entities[1].action_ticks_counter, 0x0E);
        assert_eq!(game.entities[1].angle, 0x10, "the yaw swept");

        // Frames 1..5 do not spray; frame 6 does.
        for _ in 0..5 {
            update(&mut game, 1, &room, &clips);
        }
        assert_eq!(game.entities[1].animation_frame_id, 6);
        update(&mut game, 1, &room, &clips);
        assert_eq!(game.effects.active_count(), 2, "frame 6 sprays again");

        // The cue countdown reloads at zero without touching the loop.
        {
            let entity = &mut game.entities[1];
            entity.action_ticks_counter = 0;
        }
        update(&mut game, 1, &room, &clips);
        assert_eq!(game.entities[1].action_ticks_counter, FLAME_CUE_TICKS);
        assert_eq!(game.entities[1].action_state, 5, "the loop never exits");
    }

    #[test]
    fn handler_08_double_step_runs_the_handler_twice() {
        // A one-frame clip: the first run completes the animation into state
        // 2 and the second run raises the flag in the same tick.
        let mut entity = Entity {
            id: 0x22,
            behavior_flags: 4,
            action_behavior: 8,
            scd_anim_param: 0x21,
            animation_id: 0x11,
            flags: 2,
            ..Entity::default()
        };
        entity.set_active(true);
        let mut game = fire_game(entity);
        update(&mut game, 1, &RoomState::default(), &clips());
        assert!(system_bit(&game, 0x21), "the second run raised the bit");
        assert!(game.npc_placeholders.is_empty());
    }

    #[test]
    fn out_of_range_behavior_is_recorded_and_inert() {
        let mut game = game_with(state8(11));
        update(&mut game, 1, &RoomState::default(), &clips());
        assert_eq!(game.npc_placeholders.get(&11), Some(&1));
    }
}
