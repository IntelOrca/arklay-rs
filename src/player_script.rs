//! The player's scripted-animation states.
//!
//! The original dispatches the player's per-frame update through the player's
//! state byte (`entities[0].state()` here). Only state 1 maps the pad: state 8
//! runs the scripted action behaviors, state 4 blanks the message/input flags,
//! states 5-7 are the door and climb animation windows, and states 0/2/3 are
//! the spawn init, hit reaction and death fall. [`update`] runs the states the
//! engine reaches when the locomotion state must not.
//!
//! # The state-8 behavior table
//!
//! [`update`] dispatches state 8 on `action_behavior` exactly like the
//! original's `g_playerScdBehaviors[0..9]`:
//!
//! - 0: plain clip playback that parks on the terminal pose;
//! - 1: the remapped playback. A clip id below 0x10 selects its bank and
//!   action state through the original's pair table (EMD for the first pair,
//!   EMW for the second), 0x10..=0x3D plays the RDT room-animation bank, and
//!   0x3E and up rebases by 0x3E. Completion adds `scd_timer` to the facing
//!   angle and raises the script's flag;
//! - 2: turn in place then walk (EMW clip 2) to the scripted target, arriving
//!   within 150 units;
//! - 3: turn in place then run (EMW clip 3) to the target, decelerating over
//!   the EMD clip-0 stop before raising the flag;
//! - 4/5: walk backwards (EMD clip 3 fast / clip 2 slow) to the target,
//!   arriving within 100 units;
//! - 6: turn in place (EMW clip 2) at a fixed 0x38 step;
//! - 7/8/9: play the scripted clip once (EMW), with or without the per-tick
//!   yaw step and the mirror flag.
//!
//! The arrival gate that hands control back to state 1 is the player's
//! `health_status & 0x80` lock, not the NPC `collision_flags` bit.
//!
//! # Documented deviations from the original
//!
//! - **The damage bank is absent.** A clip id of 0x3E and up selects the
//!   original's EMD damage-scratch pair; the port plays the same EMD clip id
//!   rebased by 0x3E instead, because the scratch pair is not extracted.
//! - **Weapon animations are absent.** Behavior 0's per-state weapon loads are
//!   inert without the held-weapon clip tables; the scripted body frame is
//!   posed directly. State 8's held-weapon joint refresh (`flags` bit 2) is
//!   inert for the same reason.
//! - **A missing clip bank falls back to the body bank.** When the selected
//!   pair has no playable clip (a room without its player-animation pair, or
//!   the absent damage scratch), the port advances the EMD bank at the same
//!   id; the original would read a null or damaged pointer.

use crate::game::{BANK_SYSTEM, Entity, EntitySound, FlagBank, GameState, MSF2_EFFECT_ZONE};
use crate::model::Clip;
use crate::npc::anim::EntityAnim;
use crate::npc::walk;
use crate::player::{ClipSource, PlayerState};
use crate::state::RoomState;

/// Sentry stored in the player's internal locomotion behavior while a scripted
/// state owns the player, so the return to state 1 re-enters the correct
/// locomotion clip instead of keeping the scripted pose.
const SCRIPTED_BEHAVIOR: u8 = 0xFF;

/// The original's animation remap for incoming clip ids below 0x10:
/// `(action state base, clip)` pairs. A higher id plays from the third action
/// state without remapping, and 0x3E and up rebase by 0x3E.
const SCD_ANIM_REMAP: [(u8, u8); 10] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (0, 3),
    (0, 4),
    (1, 0),
    (1, 1),
    (1, 2),
    (1, 3),
    (1, 4),
];

/// Clip playback step of behavior 1 (the original's `Joint_move(..., 0x200)`).
const SCRIPTED_BLEND_STEP: u16 = 0x200;

/// Clip playback step of behavior 0 (the original's `Joint_move(..., 0x400)`).
const PLAIN_BLEND_STEP: u16 = 0x400;

/// Clip playback step of the steering behaviors 2-9 (the original's
/// `Joint_move(..., 0x400)`).
const MOVE_BLEND_STEP: u16 = 0x400;

/// EMW clip of the walk and in-place turn.
const WALK_CLIP: u8 = 2;
/// EMW clip of the run.
const RUN_CLIP: u8 = 3;
/// EMD clip of the backward walk.
const BACK_CLIP: u8 = 2;
/// EMD clip of the backward run.
const BACK_RUN_CLIP: u8 = 3;
/// EMD clip the run's decelerating stop plays.
const STOP_CLIP: u8 = 0;

/// Advance the player's scripted state one tick.
///
/// The caller only calls this when the player is not running the pad-driven
/// locomotion and the message gate lets the player state machine run. The
/// direction pad stays in `player.input` for the object probe; the action edge
/// is withheld because it belongs to the skipped input state. `room` supplies
/// the scripted walks' collision and footstep zones, and the three clip banks
/// are the EMD body, the no-weapon EMW and the RDT room-animation pair.
pub fn update(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    player.input.action_pressed = false;
    player.input.action_held = false;
    player.behavior = SCRIPTED_BEHAVIOR;
    match game.entities[0].state() {
        0 => {
            // The one-frame spawn init: clear the behavior words and hand the
            // player to the pad-driven state 1. The original runs this on the
            // first update after a character setup.
            let entity = &mut game.entities[0];
            entity.set_state(1);
            entity.set_ignore(0);
            entity.action_behavior = 0;
            entity.action_state = 0;
            entity.animation_frame_id = 0;
            entity.unk_bf = 0;
            entity.unk_8c = 0;
            entity.attack_anim = 0;
            entity.animation_id = 0;
            entity.move_speed_current = 0;
            entity.is_being_attacked = 0;
        }
        8 => scripted_animation(game, player, room, emd_clips, emw_clips, room_clips),
        // The original's state 4 suppresses every message/input flag.
        4 => game.message_flags = 0,
        // States 2/3 (hit reaction, death) and 5/6/7 (the door and climb
        // animation windows) hold the player still: only state 1 reads the
        // pad.
        _ => {}
    }
}

/// Dispatch one state-8 tick on `action_behavior`, then mirror the entity's
/// position and facing onto the visible player.
///
/// The original runs the handler a second time when `flags` bit 1 is set,
/// re-reading `action_behavior` for the repeat, exactly like the NPC driver.
fn scripted_animation(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    let behavior = game.entities[0].action_behavior;
    dispatch_behavior(
        game, player, room, emd_clips, emw_clips, room_clips, behavior,
    );
    if game.entities[0].flags & 2 != 0 {
        let repeat = game.entities[0].action_behavior;
        dispatch_behavior(game, player, room, emd_clips, emw_clips, room_clips, repeat);
    }
    let entity = &game.entities[0];
    player.pos = entity.pos;
    player.angle = entity.angle;
}

/// Run one behavior handler. The original's `flags` bit 2 refreshes the
/// held-weapon joint afterwards; without the held-weapon TMDs that pass is
/// inert.
#[allow(clippy::too_many_arguments)] // the dispatch signature is fixed
fn dispatch_behavior(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
    behavior: u8,
) {
    match behavior {
        0 => behavior_plain(game, player, emd_clips),
        1 => behavior_remapped(game, player, emd_clips, emw_clips, room_clips),
        2 => behavior_walk(game, player, room, emw_clips),
        3 => behavior_run(game, player, room, emd_clips, emw_clips),
        4 => behavior_backward(game, player, room, emd_clips, BACK_RUN_CLIP, 0x40),
        5 => behavior_backward_slow(game, player, room, emd_clips),
        6 => behavior_turn(game, player, emw_clips),
        7 => behavior_pose_turn(game, player, emw_clips),
        8 => behavior_pose(game, player, emw_clips),
        9 => behavior_pose_mirrored(game, player, emw_clips),
        // The original reports an out-of-range behavior and dispatches
        // nothing; the port leaves the player on the last scripted frame.
        _ => {}
    }
}

/// Behavior 0: plain clip playback.
///
/// The original's states 2-5 load the equipped weapon's animation and park on
/// the terminal state 6; without the held-weapon clips the scripted frame
/// poses directly.
fn behavior_plain(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    let entity = &mut game.entities[0];
    let clock = &mut game.entity_anims[0];
    match entity.action_state {
        0 => {
            entity.action_state = 1;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.unk_bf = 0;
            entity.blend_counter = 3;
            entity.unk_8c = 3;
            set_clip(entity, 0);
            publish_pose(entity, clock, player, ClipSource::Emd);
        }
        1 => {
            let reverse = entity.flags & 1 != 0;
            clock.advance(entity, clips, reverse, PLAIN_BLEND_STEP);
            publish(entity, clock, player, ClipSource::Emd);
        }
        2..=5 => {
            entity.action_state = 6;
            entity.blend_counter = 3;
            entity.unk_8c = 3;
            // `plw_anim` poses a single scripted frame; show it directly.
            publish_pose(entity, clock, player, ClipSource::Emd);
        }
        _ => {}
    }
}

/// Behavior 1: remapped clip playback.
///
/// The incoming id selects a clip bank and action state in state 0; states
/// 1-4 each advance that bank with the 0x200 blend step and converge on state
/// 5, which raises the script's completion flag and loops when `flags` bit
/// 0x10 is set.
fn behavior_remapped(
    game: &mut GameState,
    player: &mut PlayerState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    let GameState {
        entities,
        entity_anims,
        flags,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    match entity.action_state {
        0 => {
            // `unk_8c` is the clip's blend flag: 7, or 0 when `flags` bit
            // 0x20 asks for a snap.
            entity.blend_counter = if entity.flags & 0x20 != 0 { 0 } else { 7 };
            entity.unk_8c = entity.blend_counter;
            entity.move_speed_current = 0;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.unk_bf = 0;
            let incoming = entity.animation_id;
            if incoming < 0x3E {
                if incoming < 0x10 {
                    let (state_base, remapped) = SCD_ANIM_REMAP[usize::from(incoming)];
                    set_clip(entity, remapped);
                    entity.action_state = state_base + 1;
                } else {
                    // The RDT room-animation bank plays at the id directly.
                    entity.action_state = 3;
                    set_clip(entity, incoming);
                }
            } else {
                entity.action_state = 4;
                set_clip(entity, incoming - 0x3E);
            }
            entity.attacking_direction = 1;
            // No frame is consumed on the pitch tick; show the scripted frame.
            publish_pose(entity, clock, player, state_source(entity.action_state));
        }
        state @ 1..=4 => {
            // `flags` bit 0x80 holds every frame for an extra tick: the
            // counter decrements each tick and the hold runs whenever it
            // enters on zero, exactly the original's post-decrement test.
            if entity.flags & 0x80 != 0 {
                let entry = entity.attacking_direction;
                entity.attacking_direction = entity.attacking_direction.wrapping_sub(1);
                if entry == 0 {
                    entity.attacking_direction = 1;
                    publish_pose(entity, clock, player, state_source(state));
                    return;
                }
            }
            let source = state_source(state);
            // A room without its animation pair falls back to the body bank
            // (documented deviation): the original reads the pair's null
            // pointer and cannot advance either.
            let (source, clips) = bank_for(
                source,
                entity.animation_id,
                emd_clips,
                emw_clips,
                room_clips,
            );
            if clock.advance(entity, clips, entity.flags & 1 != 0, SCRIPTED_BLEND_STEP) {
                entity.action_state = 5;
                entity.angle = entity.angle.wrapping_add(entity.scd_timer);
                // The original rebases the clip when the pair's states finish,
                // so a flag-0x10 loop restarts from the right entry.
                match state {
                    2 => set_clip(entity, entity.animation_id.wrapping_add(5)),
                    4 => set_clip(entity, entity.animation_id.wrapping_add(0x3E)),
                    _ => {}
                }
            }
            publish(entity, clock, player, source);
        }
        5 => {
            system.apply(entity.scd_anim_param, 0);
            entity.scd_timer = 0;
            if entity.flags & 0x10 != 0 {
                entity.action_state = 0;
            }
            // The completion tick consumed no frame: keep the last bank.
            publish_pose_keep(entity, clock, player);
        }
        _ => {}
    }
}

/// Behavior 2: turn in place, then walk to the scripted target.
///
/// State 0/1 turn at the script's `scd_timer` step until aligned within
/// 0x16A, state 2/3 walk (EMW clip 2, speed 0x5D, footsteps on frames 8 and
/// 0x16) and finish within 150 units.
fn behavior_walk(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    emw_clips: &[Clip],
) {
    let slow = game.flags[5].bit(MSF2_EFFECT_ZONE);
    let health_locked = game.health_status & 0x80 != 0;
    let GameState {
        entities,
        entity_anims,
        flags,
        entity_sounds,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    let target = scripted_target(entity);
    match entity.action_state {
        0 => {
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.unk_bf = 0;
            set_clip(entity, WALK_CLIP);
            entity.action_state = 1;
            entity.blend_counter = 3;
            entity.unk_8c = 3;
            walk_turn_step(entity, clock, target, emw_clips);
        }
        1 => walk_turn_step(entity, clock, target, emw_clips),
        2 => {
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.unk_bf = 0;
            set_clip(entity, WALK_CLIP);
            entity.action_state = 3;
            entity.blend_counter = 3;
            entity.unk_8c = 3;
            walk_move_step(
                entity,
                clock,
                room,
                emw_clips,
                entity_sounds,
                system,
                slow,
                health_locked,
                target,
            );
        }
        3 => walk_move_step(
            entity,
            clock,
            room,
            emw_clips,
            entity_sounds,
            system,
            slow,
            health_locked,
            target,
        ),
        _ => {}
    }
    publish(entity, clock, player, ClipSource::Emw);
}

/// Behavior 3: turn in place, then run to the scripted target.
///
/// State 0/1 turn until aligned within 0x16A, state 2/3 run (EMW clip 3,
/// speed 0xD2, footsteps on frames 0 and 10) until within 250 units, state 4/5
/// shed 0x1E speed per tick over the EMD clip-0 stop, and state 6 hands the
/// player back to state 1 and raises the flag. The health lock finishes in
/// range without the deceleration.
fn behavior_run(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
) {
    let slow = game.flags[5].bit(MSF2_EFFECT_ZONE);
    let health_locked = game.health_status & 0x80 != 0;
    let GameState {
        entities,
        entity_anims,
        flags,
        entity_sounds,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    let target = scripted_target(entity);
    let state = entity.action_state;
    match state {
        0 => {
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.unk_bf = 0;
            set_clip(entity, WALK_CLIP);
            entity.action_state = 1;
            entity.blend_counter = 3;
            entity.unk_8c = 3;
            walk_turn_step(entity, clock, target, emw_clips);
        }
        1 => walk_turn_step(entity, clock, target, emw_clips),
        2 => {
            entity.move_speed_current = 0xD2;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.unk_bf = 0;
            set_clip(entity, RUN_CLIP);
            entity.action_state = 3;
            entity.blend_counter = 3;
            entity.unk_8c = 3;
            run_move_step(
                entity,
                clock,
                room,
                emw_clips,
                entity_sounds,
                system,
                slow,
                health_locked,
                target,
            );
        }
        3 => run_move_step(
            entity,
            clock,
            room,
            emw_clips,
            entity_sounds,
            system,
            slow,
            health_locked,
            target,
        ),
        4 => {
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.unk_bf = 0;
            set_clip(entity, STOP_CLIP);
            entity.action_state = 5;
            entity.blend_counter = 3;
            entity.unk_8c = 3;
            entity.action_ticks_counter = 0;
            run_stop_step(entity, clock, room, emd_clips);
        }
        5 => run_stop_step(entity, clock, room, emd_clips),
        6 => {
            return_to_control(entity);
            system.apply(entity.scd_anim_param, 0);
        }
        _ => {}
    }
    // The stop plays from the body bank; the earlier states from the EMW.
    let source = if state >= 4 {
        ClipSource::Emd
    } else {
        ClipSource::Emw
    };
    publish(entity, clock, player, source);
}

/// Behavior 4: walk backwards to the scripted target at the fast pace.
///
/// The facing is flipped 180 degrees around the rotate-toward-target call so
/// the turn aligns the player's back with the target, then the flip is undone
/// and `Add_speedXZ(0x800)` drives the backward motion. Arrival is 100 units.
fn behavior_backward(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &[Clip],
    clip: u8,
    speed: u16,
) {
    let slow = game.flags[5].bit(MSF2_EFFECT_ZONE);
    let health_locked = game.health_status & 0x80 != 0;
    let GameState {
        entities,
        entity_anims,
        flags,
        entity_sounds,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    if entity.action_state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.unk_bf = 0;
        entity.blend_counter = 3;
        entity.unk_8c = 3;
        entity.action_state = 1;
        set_clip(entity, clip);
    } else if entity.action_state != 1 {
        return;
    }
    let frame = entity.animation_frame_id;
    if frame == 8 || frame == 0x16 {
        walk::footstep(entity_sounds, room, entity, 0, slow);
    }
    backward_step(entity, clock, room, clips, system, health_locked, speed);
    publish(entity, clock, player, ClipSource::Emd);
}

/// Behavior 5: walk backwards to the scripted target at the slow pace (EMD
/// clip 2, speed 0x1D). The footstep fires only while the held frame's timing
/// byte is exactly 2, which the shipped clips never reach.
fn behavior_backward_slow(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &[Clip],
) {
    let slow = game.flags[5].bit(MSF2_EFFECT_ZONE);
    let health_locked = game.health_status & 0x80 != 0;
    let GameState {
        entities,
        entity_anims,
        flags,
        entity_sounds,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    if entity.action_state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.unk_bf = 0;
        entity.blend_counter = 3;
        entity.unk_8c = 3;
        entity.action_state = 1;
        set_clip(entity, BACK_CLIP);
        entity.move_speed_current = 0x1D;
    } else if entity.action_state != 1 {
        return;
    }
    let frame = entity.animation_frame_id as i8;
    if (frame == 7 || frame == 0x1B) && entity.timing_control as i8 == 2 {
        walk::footstep(entity_sounds, room, entity, 0, slow);
    }
    backward_step(
        entity,
        clock,
        room,
        clips,
        system,
        health_locked,
        entity.move_speed_current,
    );
    publish(entity, clock, player, ClipSource::Emd);
}

/// The shared backward-walk body of behaviors 4/5: flip the facing 180
/// degrees, steer the back onto the target, flip back, then move backwards
/// along the heading. The footstep check stays with the callers, because the
/// two behaviours test different frames.
fn backward_step(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    room: &RoomState,
    clips: &[Clip],
    system: &mut FlagBank,
    health_locked: bool,
    speed: u16,
) {
    entity.move_speed_current = speed;
    let target = scripted_target(entity);
    // The +0x800 is masked into the 0..0xFFF angle space, the -0x800 is not;
    // the asymmetry is the original's.
    entity.angle = entity.angle.wrapping_add(0x800) & 0x0FFF;
    walk::rotate_toward_target(entity, target, entity.scd_timer);
    entity.angle = entity.angle.wrapping_sub(0x800);
    clock.advance(entity, clips, entity.flags & 1 != 0, MOVE_BLEND_STEP);
    walk::advance_xz_blocked(room, entity, 0x800, entity.move_speed_current as i16);
    if walk::xz_distance_to(entity, target) < 100 {
        system.apply(entity.scd_anim_param, 0);
        if !health_locked {
            return_to_control(entity);
        }
    }
}

/// Behavior 6: turn in place toward the scripted target at a fixed 0x38 step,
/// finishing when `turn_toward_target` reports the remaining angle closed at
/// the script's own step.
fn behavior_turn(game: &mut GameState, player: &mut PlayerState, emw_clips: &[Clip]) {
    let GameState {
        entities,
        entity_anims,
        flags,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    match entity.action_state {
        0 => {
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.unk_bf = 0;
            set_clip(entity, WALK_CLIP);
            entity.action_state = 1;
            entity.blend_counter = 3;
            entity.unk_8c = 3;
        }
        1 => {}
        2 => {
            return_to_control(entity);
            system.apply(entity.scd_anim_param, 0);
            return;
        }
        _ => return,
    }
    let target = scripted_target(entity);
    walk::rotate_toward_target(entity, target, 0x38);
    clock.advance(entity, emw_clips, entity.flags & 1 != 0, MOVE_BLEND_STEP);
    if walk::turn_toward_target(entity, target, entity.scd_timer as i16) == 0 {
        entity.action_state = 2;
    }
    publish(entity, clock, player, ClipSource::Emw);
}

/// Behavior 7: play the scripted clip once (EMW), adding `scd_timer` to the
/// yaw on every tick including after completion. The mirror flag is the
/// script's own (`flags` bit 0).
fn behavior_pose_turn(game: &mut GameState, player: &mut PlayerState, emw_clips: &[Clip]) {
    let GameState {
        entities,
        entity_anims,
        flags,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    if entity.action_state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.unk_bf = 0;
        entity.blend_counter = 3;
        entity.unk_8c = 3;
        set_clip(entity, entity.animation_id.wrapping_sub(5));
        entity.action_state = 1;
    }
    if entity.action_state == 1 {
        let done = clock.advance(entity, emw_clips, entity.flags & 1 != 0, MOVE_BLEND_STEP);
        entity.action_state = entity.action_state.wrapping_add(u8::from(done));
    } else if entity.action_state == 2 {
        system.apply(entity.scd_anim_param, 0);
    }
    entity.angle = entity.angle.wrapping_add(entity.scd_timer);
    publish(entity, clock, player, ClipSource::Emw);
}

/// Behavior 8: as 7 but never mirrored and with no yaw step.
fn behavior_pose(game: &mut GameState, player: &mut PlayerState, emw_clips: &[Clip]) {
    let GameState {
        entities,
        entity_anims,
        flags,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    if entity.action_state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.unk_bf = 0;
        entity.blend_counter = 3;
        entity.unk_8c = 3;
        set_clip(entity, entity.animation_id.wrapping_sub(5));
        entity.action_state = 1;
    } else if entity.action_state != 1 {
        if entity.action_state == 2 {
            system.apply(entity.scd_anim_param, 0);
        }
        return;
    }
    let done = clock.advance(entity, emw_clips, false, MOVE_BLEND_STEP);
    entity.action_state = entity.action_state.wrapping_add(u8::from(done));
    publish(entity, clock, player, ClipSource::Emw);
}

/// Behavior 9: as 8 but always mirrored, and on completion it hands the player
/// back to state 1.
fn behavior_pose_mirrored(game: &mut GameState, player: &mut PlayerState, emw_clips: &[Clip]) {
    let GameState {
        entities,
        entity_anims,
        flags,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let system = &mut flags[usize::from(BANK_SYSTEM)];
    if entity.action_state == 0 {
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.unk_bf = 0;
        entity.blend_counter = 3;
        entity.unk_8c = 3;
        set_clip(entity, entity.animation_id.wrapping_sub(5));
        entity.action_state = 1;
    } else if entity.action_state != 1 {
        if entity.action_state != 2 {
            return;
        }
        return_to_control(entity);
        system.apply(entity.scd_anim_param, 0);
        return;
    }
    let done = clock.advance(entity, emw_clips, true, MOVE_BLEND_STEP);
    entity.action_state = entity.action_state.wrapping_add(u8::from(done));
    publish(entity, clock, player, ClipSource::Emw);
}

/// The turn phase shared by behaviors 2/3: step the facing toward the target
/// by `scd_timer`, advance the clip and finish the turn within 0x16A.
fn walk_turn_step(entity: &mut Entity, clock: &mut EntityAnim, target: [i32; 3], clips: &[Clip]) {
    let turn = walk::turn_toward_target(entity, target, entity.scd_timer as i16);
    entity.angle = entity.angle.wrapping_add(turn as u16);
    clock.advance(entity, clips, entity.flags & 1 != 0, MOVE_BLEND_STEP);
    if walk::turn_toward_target(entity, target, 0x16A) == 0 {
        entity.action_state = 2;
    }
}

/// Behavior 2's walk phase: footstep on frames 8 and 0x16, trim the speed on
/// the footfall frames, steer toward the target and finish within 150 units.
#[allow(clippy::too_many_arguments)] // the handler signature is fixed
fn walk_move_step(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    room: &RoomState,
    clips: &[Clip],
    sounds: &mut Vec<EntitySound>,
    system: &mut FlagBank,
    slow: bool,
    health_locked: bool,
    target: [i32; 3],
) {
    let frame = entity.animation_frame_id;
    if frame == 8 || frame == 0x16 {
        walk::footstep(sounds, room, entity, 0, slow);
    }
    walk::entity_apply_walk_speed(entity, 0x5D);
    walk::rotate_toward_target(entity, target, entity.scd_timer);
    clock.advance(entity, clips, entity.flags & 1 != 0, MOVE_BLEND_STEP);
    walk::advance_xz_blocked(room, entity, 0, entity.move_speed_current as i16);
    if walk::xz_distance_to(entity, target) < 0x96 {
        system.apply(entity.scd_anim_param, 0);
        if !health_locked {
            return_to_control(entity);
        }
    }
}

/// Behavior 3's run phase: footstep on frames 0 and 10, steer toward the
/// target and finish within 250 units. The health lock raises the flag in
/// range instead of entering the deceleration.
#[allow(clippy::too_many_arguments)] // the handler signature is fixed
fn run_move_step(
    entity: &mut Entity,
    clock: &mut EntityAnim,
    room: &RoomState,
    clips: &[Clip],
    sounds: &mut Vec<EntitySound>,
    system: &mut FlagBank,
    slow: bool,
    health_locked: bool,
    target: [i32; 3],
) {
    let frame = entity.animation_frame_id;
    if frame == 0 || frame == 10 {
        walk::footstep(sounds, room, entity, 1, slow);
    }
    walk::rotate_toward_target(entity, target, entity.scd_timer);
    clock.advance(entity, clips, entity.flags & 1 != 0, MOVE_BLEND_STEP);
    walk::advance_xz_blocked(room, entity, 0, entity.move_speed_current as i16);
    if walk::xz_distance_to(entity, target) < 0xFA {
        if health_locked {
            system.apply(entity.scd_anim_param, 0);
        } else {
            entity.action_state = 4;
        }
    }
}

/// Behavior 3's deceleration: advance the stop clip, count four ticks, shed
/// 0x1E speed each tick and keep moving.
fn run_stop_step(entity: &mut Entity, clock: &mut EntityAnim, room: &RoomState, clips: &[Clip]) {
    clock.advance(entity, clips, entity.flags & 1 != 0, MOVE_BLEND_STEP);
    entity.action_ticks_counter = entity.action_ticks_counter.wrapping_add(1);
    if (entity.action_ticks_counter as i16) > 3 {
        entity.action_state = 6;
    }
    entity.move_speed_current = (entity.move_speed_current as i16).wrapping_sub(0x1E) as u16;
    walk::advance_xz_blocked(room, entity, 0, entity.move_speed_current as i16);
}

/// The scripted walk target (`unk_c6`, 0, `unk_c8`).
fn scripted_target(entity: &Entity) -> [i32; 3] {
    [i32::from(entity.unk_c6), 0, i32::from(entity.unk_c8)]
}

/// The clip bank of behavior 1's action states: state 1 is the EMD pair,
/// state 2 the EMW pair, state 3 the RDT room bank and state 4 the (absent)
/// EMD damage scratch.
fn state_source(state: u8) -> ClipSource {
    match state {
        2 => ClipSource::Emw,
        3 => ClipSource::Room,
        _ => ClipSource::Emd,
    }
}

/// Select one of the three clip banks.
fn bank_clips<'a>(
    source: ClipSource,
    emd: &'a [Clip],
    emw: &'a [Clip],
    room: &'a [Clip],
) -> &'a [Clip] {
    match source {
        ClipSource::Emd => emd,
        ClipSource::Emw => emw,
        ClipSource::Room => room,
    }
}

/// Select the clip bank for behavior 1, falling back to the body bank when the
/// requested bank has no playable clip at `id`. The fallback covers the damage
/// scratch pair (not extracted) and a room whose player-animation pair is
/// absent; the original would read a null or damaged pointer there.
fn bank_for<'a>(
    source: ClipSource,
    id: u8,
    emd: &'a [Clip],
    emw: &'a [Clip],
    room: &'a [Clip],
) -> (ClipSource, &'a [Clip]) {
    let clips = bank_clips(source, emd, emw, room);
    if clips
        .get(usize::from(id))
        .is_some_and(|clip| !clip.frames.is_empty())
    {
        (source, clips)
    } else {
        (ClipSource::Emd, emd)
    }
}

/// Hand the player back to the pad-driven state 1, the original's
/// `animationId = 1; animFrameId = 0; action_behavior = 0; action_state = 0`.
fn return_to_control(entity: &mut Entity) {
    entity.set_state(1);
    entity.animation_frame_id = 0;
    entity.action_behavior = 0;
    entity.action_state = 0;
}

/// Store the scripted clip in both fields the player's +0xBD byte is split
/// into: the generic entity clip id and the player's `attackAnim` name.
fn set_clip(entity: &mut Entity, clip: u8) {
    entity.animation_id = clip;
    entity.attack_anim = clip;
}

/// Publish the entity's scripted animation words to the player model's clock
/// so the renderer poses the scripted clip from `source`'s bank.
fn publish(entity: &Entity, clock: &mut EntityAnim, player: &mut PlayerState, source: ClipSource) {
    let clip = usize::from(entity.animation_id);
    if player.anim.clip != clip || player.clip_source != source {
        // A script switched clips or banks: show the scripted frame instead of
        // the previous clip's applied frame.
        clock.player.clip = clip;
        clock.player.display_frame = usize::from(entity.animation_frame_id);
    }
    clock.sync(entity);
    player.anim = clock.player.clone();
    player.clip_source = source;
}

/// Publish a scripted pose that consumed no frame: the entity words win even
/// when the clip is unchanged, because the `plw_anim` pose setter can seek
/// within the clip it already selected.
fn publish_pose(
    entity: &Entity,
    clock: &mut EntityAnim,
    player: &mut PlayerState,
    source: ClipSource,
) {
    let frame = usize::from(entity.animation_frame_id);
    clock.player.clip = usize::from(entity.animation_id);
    clock.player.frame = frame;
    clock.player.display_frame = frame;
    clock.player.timing = u16::from(entity.timing_control);
    clock.player.blend_counter = u16::from(entity.blend_counter);
    player.anim = clock.player.clone();
    player.clip_source = source;
}

/// [`publish_pose`] for the remapped playback's completion state, where the
/// script keeps the bank the completed pair played from.
fn publish_pose_keep(entity: &Entity, clock: &mut EntityAnim, player: &mut PlayerState) {
    let frame = usize::from(entity.animation_frame_id);
    clock.player.clip = usize::from(entity.animation_id);
    clock.player.frame = frame;
    clock.player.display_frame = frame;
    clock.player.timing = u16::from(entity.timing_control);
    clock.player.blend_counter = u16::from(entity.blend_counter);
    player.anim = clock.player.clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GameState;
    use crate::model::{Clip, ClipFrame};
    use crate::state::{RoomId, RoomState};

    fn clip(frames: usize, timing: u16) -> Clip {
        Clip {
            frames: (0..frames)
                .map(|_| ClipFrame {
                    keyframe: 0,
                    timing,
                })
                .collect(),
        }
    }

    fn test_player() -> PlayerState {
        crate::player::spawn(
            RoomId {
                stage: 1,
                room: 0,
                player_flag: 0,
            },
            &RoomState::default(),
        )
    }

    fn test_game(state: u8, behavior: u8, animation: u8) -> GameState {
        let mut game = GameState::new(
            RoomId {
                stage: 1,
                room: 0,
                player_flag: 0,
            },
            &RoomState::default(),
        );
        game.entities[0].set_state(state);
        game.entities[0].action_behavior = behavior;
        game.entities[0].animation_id = animation;
        game.entities[0].attack_anim = animation;
        game.entities[0].scd_anim_param = 0x21;
        game
    }

    fn system_bit(game: &GameState, bit: u8) -> bool {
        game.flags[usize::from(BANK_SYSTEM)].bit(bit)
    }

    fn run(
        game: &mut GameState,
        player: &mut PlayerState,
        emd: &[Clip],
        emw: &[Clip],
        room: &[Clip],
    ) {
        update(game, player, &RoomState::default(), emd, emw, room);
    }

    #[test]
    fn behavior_one_advances_the_clip_and_raises_the_flag() {
        // Incoming 0x20 selects the room-animation bank at state 3.
        let room = vec![clip(3, 1); 0x30];
        let mut game = test_game(8, 1, 0x20);
        let mut player = test_player();
        // First tick: state 0 pitches the clip and publishes it to the model.
        run(&mut game, &mut player, &[], &[], &room);
        assert_eq!(game.entities[0].action_state, 3);
        assert_eq!(player.anim.clip, 0x20);
        assert_eq!(player.clip_source, ClipSource::Room);

        // Three one-tick frames: the third advance wraps the clip and parks
        // the behavior in its signal state; the next tick raises the flag.
        for _ in 0..3 {
            run(&mut game, &mut player, &[], &[], &room);
        }
        assert_eq!(game.entities[0].action_state, 5);
        assert!(!system_bit(&game, 0x21));
        run(&mut game, &mut player, &[], &[], &room);
        assert!(
            system_bit(&game, 0x21),
            "the scripted completion flag was raised"
        );
        assert_eq!(game.entities[0].scd_timer, 0, "the turn step is consumed");
    }

    #[test]
    fn behavior_one_selects_the_emd_pair() {
        let emd = vec![clip(1, 0); 5];
        let mut game = test_game(8, 1, 3);
        let mut player = test_player();
        run(&mut game, &mut player, &emd, &[], &[]);
        assert_eq!(game.entities[0].animation_id, 3, "pair (0, 3)");
        assert_eq!(game.entities[0].action_state, 1);
        assert_eq!(player.clip_source, ClipSource::Emd);
    }

    #[test]
    fn behavior_one_selects_the_emw_pair() {
        let emw = vec![clip(1, 0); 5];
        let mut game = test_game(8, 1, 5);
        let mut player = test_player();
        run(&mut game, &mut player, &[], &emw, &[]);
        assert_eq!(game.entities[0].animation_id, 0, "pair (1, 0)");
        assert_eq!(game.entities[0].action_state, 2);
        assert_eq!(player.clip_source, ClipSource::Emw);
    }

    #[test]
    fn behavior_one_loops_the_emw_pair_on_flag_0x10() {
        let emw = vec![clip(1, 0); 10];
        let mut game = test_game(8, 1, 5);
        game.entities[0].flags = 0x10;
        let mut player = test_player();
        run(&mut game, &mut player, &[], &emw, &[]);
        assert_eq!(game.entities[0].action_state, 2);
        // The one-frame clip completes on the next tick and rebases +5 into
        // the signal state; the tick after raises the flag and loops.
        run(&mut game, &mut player, &[], &emw, &[]);
        assert_eq!(game.entities[0].action_state, 5);
        run(&mut game, &mut player, &[], &emw, &[]);
        assert!(system_bit(&game, 0x21));
        assert_eq!(game.entities[0].action_state, 0, "flag 0x10 loops");
        run(&mut game, &mut player, &[], &emw, &[]);
        assert_eq!(game.entities[0].animation_id, 0, "pair (1, 0) again");
        assert_eq!(game.entities[0].action_state, 2);
    }

    #[test]
    fn behavior_one_adds_the_timer_to_the_facing_on_completion() {
        let room = vec![clip(1, 0); 0x30];
        let mut game = test_game(8, 1, 0x20);
        game.entities[0].scd_timer = 0x28;
        let mut player = test_player();
        run(&mut game, &mut player, &[], &[], &room);
        run(&mut game, &mut player, &[], &[], &room);
        assert_eq!(game.entities[0].angle, 0x28);
    }

    #[test]
    fn the_double_step_runs_the_behavior_twice() {
        let emd = vec![clip(1, 0); 5];
        let mut game = test_game(8, 1, 0);
        game.entities[0].flags = 2;
        let mut player = test_player();
        // The first run selects the EMD pair in state 0; the repeat advances
        // the one-frame clip straight into the signal state.
        run(&mut game, &mut player, &emd, &[], &[]);
        assert_eq!(game.entities[0].action_state, 5);
        run(&mut game, &mut player, &emd, &[], &[]);
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn behavior_two_walks_to_the_target_and_returns_to_control() {
        let emw = vec![clip(1, 0); 8];
        let mut game = test_game(8, 2, 0);
        game.entities[0].unk_c6 = 1000;
        game.entities[0].unk_c8 = 0;
        game.entities[0].scd_timer = 0x40;
        let mut player = test_player();
        for _ in 0..400 {
            run(&mut game, &mut player, &[], &emw, &[]);
            if game.entities[0].state() == 1 {
                break;
            }
        }
        let entity = game.entities[0];
        assert_eq!(entity.state(), 1, "the walk handed control back");
        assert_eq!(entity.action_behavior, 0);
        assert!(
            walk::xz_distance_to(&entity, [1000, 0, 0]) < 0x96,
            "arrived at {:?}",
            entity.pos
        );
        assert!(system_bit(&game, 0x21));
        assert_eq!(player.pos, entity.pos, "the visible player moved too");
    }

    #[test]
    fn behavior_two_keeps_running_while_the_health_lock_is_set() {
        let emw = vec![clip(1, 0); 8];
        let mut game = test_game(8, 2, 0);
        game.entities[0].unk_c6 = 100;
        game.entities[0].unk_c8 = 0;
        game.entities[0].scd_timer = 0x40;
        game.health_status = 0x80;
        let mut player = test_player();
        for _ in 0..120 {
            run(&mut game, &mut player, &[], &emw, &[]);
        }
        assert_eq!(game.entities[0].state(), 8, "the lock keeps the behavior");
        assert_eq!(game.entities[0].action_behavior, 2);
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn behavior_three_runs_and_decelerates_into_control() {
        let emw = vec![clip(1, 0); 8];
        let emd = vec![clip(1, 0); 4];
        let mut game = test_game(8, 3, 0);
        game.entities[0].unk_c6 = 2000;
        game.entities[0].unk_c8 = 0;
        game.entities[0].scd_timer = 0x40;
        let mut player = test_player();
        let start = game.entities[0].pos;
        let mut saw_slowdown = false;
        for _ in 0..400 {
            run(&mut game, &mut player, &emd, &emw, &[]);
            if game.entities[0].action_state == 5 && game.entities[0].move_speed_current < 0xD2 {
                saw_slowdown = true;
            }
            if game.entities[0].state() == 1 {
                break;
            }
        }
        let entity = game.entities[0];
        assert_eq!(entity.state(), 1);
        assert_ne!(entity.pos, start, "the run moved the player");
        assert!(walk::xz_distance_to(&entity, [2000, 0, 0]) < 0x300);
        assert!(saw_slowdown, "the stop shed speed");
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn behavior_four_walks_backwards_to_the_target() {
        let emd = vec![clip(1, 0); 8];
        let mut game = test_game(8, 4, 0);
        game.entities[0].unk_c6 = 600;
        game.entities[0].unk_c8 = 0;
        game.entities[0].scd_timer = 0x40;
        let mut player = test_player();
        for _ in 0..400 {
            run(&mut game, &mut player, &emd, &[], &[]);
            if game.entities[0].state() == 1 {
                break;
            }
        }
        let entity = game.entities[0];
        assert_eq!(entity.state(), 1);
        assert!(walk::xz_distance_to(&entity, [600, 0, 0]) < 100);
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn behavior_five_walks_backwards_slowly() {
        let emd = vec![clip(1, 0); 8];
        let mut game = test_game(8, 5, 0);
        game.entities[0].unk_c6 = 0;
        game.entities[0].unk_c8 = 600;
        game.entities[0].scd_timer = 0x40;
        let mut player = test_player();
        for _ in 0..400 {
            run(&mut game, &mut player, &emd, &[], &[]);
            if game.entities[0].state() == 1 {
                break;
            }
        }
        let entity = game.entities[0];
        assert_eq!(entity.state(), 1);
        assert!(walk::xz_distance_to(&entity, [0, 0, 600]) < 100);
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn behavior_six_turns_in_place_then_signals() {
        let emw = vec![clip(1, 0); 8];
        let mut game = test_game(8, 6, 0);
        game.entities[0].unk_c6 = 0;
        game.entities[0].unk_c8 = 100;
        game.entities[0].scd_timer = 0x28;
        let mut player = test_player();
        for _ in 0..40 {
            run(&mut game, &mut player, &[], &emw, &[]);
            if game.entities[0].state() == 1 {
                break;
            }
        }
        assert_eq!(game.entities[0].state(), 1);
        // The fixed 0x38 rotate step can stop one step short of the target
        // when the completion window is wider; the original's own test is the
        // remaining-angle check, so assert that instead of an exact angle.
        assert_eq!(
            walk::turn_toward_target(&game.entities[0], [0, 0, 100], 0x28),
            0,
            "the facing closed within the script's step"
        );
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn behavior_seven_applies_the_yaw_step_and_signals() {
        let emw = vec![clip(1, 0); 8];
        let mut game = test_game(8, 7, 5);
        game.entities[0].scd_timer = 0x10;
        let mut player = test_player();
        run(&mut game, &mut player, &[], &emw, &[]);
        assert_eq!(game.entities[0].animation_id, 0);
        assert_eq!(game.entities[0].action_state, 2);
        assert_eq!(game.entities[0].angle, 0x10);
        run(&mut game, &mut player, &[], &emw, &[]);
        assert!(system_bit(&game, 0x21));
        assert_eq!(game.entities[0].angle, 0x20, "the yaw step runs on state 2");
    }

    #[test]
    fn behavior_eight_never_mirrors_or_turns() {
        let emw = vec![clip(1, 0); 8];
        let mut game = test_game(8, 8, 5);
        game.entities[0].scd_timer = 0x10;
        let mut player = test_player();
        run(&mut game, &mut player, &[], &emw, &[]);
        assert_eq!(game.entities[0].angle, 0);
        run(&mut game, &mut player, &[], &emw, &[]);
        assert!(system_bit(&game, 0x21));
        assert_eq!(game.entities[0].angle, 0);
        assert_eq!(game.entities[0].state(), 8, "b8 stays on the pose");
    }

    #[test]
    fn behavior_nine_returns_to_control() {
        let emw = vec![clip(1, 0); 8];
        let mut game = test_game(8, 9, 5);
        let mut player = test_player();
        run(&mut game, &mut player, &[], &emw, &[]);
        run(&mut game, &mut player, &[], &emw, &[]);
        assert_eq!(game.entities[0].state(), 1);
        assert_eq!(game.entities[0].action_behavior, 0);
        assert!(system_bit(&game, 0x21));
    }

    #[test]
    fn state_four_blanks_the_message_flags() {
        let mut game = test_game(4, 0, 0);
        game.message_flags = 0xFD3F;
        let mut player = test_player();
        run(&mut game, &mut player, &[], &[], &[]);
        assert_eq!(game.message_flags, 0);
        assert_eq!(player.behavior, SCRIPTED_BEHAVIOR);
    }

    #[test]
    fn a_plain_pose_seek_moves_within_the_same_clip() {
        let clips = vec![
            Clip {
                frames: (0..4)
                    .map(|index| ClipFrame {
                        keyframe: index,
                        timing: 1,
                    })
                    .collect(),
            };
            0x21
        ];
        let mut game = test_game(8, 0, 0x20);
        game.entities[0].action_state = 2;
        let mut player = test_player();
        run(&mut game, &mut player, &clips, &[], &[]);
        assert_eq!(game.entities[0].action_state, 6);
        assert_eq!(player.anim.keyframe_index(&clips), 0);

        // A second `plw_anim` poses frame 2 of the same clip.
        game.entities[0].action_state = 2;
        game.entities[0].animation_frame_id = 2;
        run(&mut game, &mut player, &clips, &[], &[]);
        assert_eq!(player.anim.keyframe_index(&clips), 2);
    }

    #[test]
    fn state_zero_initialises_to_the_control_state() {
        let mut game = test_game(0, 7, 9);
        game.entities[0].animation_frame_id = 5;
        let mut player = test_player();
        run(&mut game, &mut player, &[], &[], &[]);
        assert_eq!(game.entities[0].state(), 1);
        assert_eq!(game.entities[0].action_behavior, 0);
        assert_eq!(game.entities[0].action_state, 0);
        assert_eq!(game.entities[0].animation_id, 0);
        assert_eq!(game.entities[0].animation_frame_id, 0);
        assert_eq!(player.behavior, SCRIPTED_BEHAVIOR);
    }

    #[test]
    fn a_non_control_state_never_reads_the_action_edge() {
        let mut game = test_game(5, 0, 0);
        let mut player = test_player();
        player.input.action_pressed = true;
        player.input.action_held = true;
        player.input.up = true;
        run(&mut game, &mut player, &[], &[], &[]);
        assert!(!player.input.action_pressed);
        assert!(!player.input.action_held);
        assert!(player.input.up, "the direction pad stays published");
    }

    #[test]
    fn the_keyframe_comes_from_the_scripted_clip_bank() {
        // One clip with two frames, the second on keyframe 2.
        let clips = vec![Clip {
            frames: vec![
                ClipFrame {
                    keyframe: 0,
                    timing: 1,
                },
                ClipFrame {
                    keyframe: 2,
                    timing: 1,
                },
            ],
        }];
        let mut game = test_game(8, 1, 0);
        let mut player = test_player();
        run(&mut game, &mut player, &clips, &[], &[]);
        assert_eq!(player.anim.keyframe_index(&clips), 0);
        run(&mut game, &mut player, &clips, &[], &[]);
        assert_eq!(player.anim.keyframe_index(&clips), 0, "frame 0 is applied");
        run(&mut game, &mut player, &clips, &[], &[]);
        assert_eq!(player.anim.keyframe_index(&clips), 2);
    }

    #[test]
    fn a_scripted_walk_queues_its_footsteps() {
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
        let emw = vec![clip(0x20, 1); 8];
        let mut game = test_game(8, 2, 0);
        game.entities[0].action_state = 3;
        game.entities[0].animation_frame_id = 8;
        game.entities[0].unk_c6 = 0x8000;
        game.entities[0].unk_c8 = 0;
        game.entities[0].scd_timer = 0x40;
        let mut player = test_player();
        update(&mut game, &mut player, &room, &[], &emw, &[]);
        assert_eq!(game.entity_sounds.len(), 1, "frame 8 is a contact frame");
        assert_eq!(game.entity_sounds[0].name, "ft_wdA");
    }

    #[test]
    fn the_backward_walks_step_on_their_own_frames() {
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
        let emd = vec![clip(0x20, 1); 8];

        // Behavior 4 steps on the backward run's contact frames 8/0x16.
        let mut game = test_game(8, 4, 0);
        game.entities[0].action_state = 1;
        game.entities[0].animation_frame_id = 8;
        game.entities[0].unk_c6 = 0x8000;
        game.entities[0].unk_c8 = 0;
        game.entities[0].scd_timer = 0x40;
        let mut player = test_player();
        update(&mut game, &mut player, &room, &emd, &[], &[]);
        assert_eq!(game.entity_sounds.len(), 1, "behavior 4 steps on frame 8");

        // Behavior 5 only steps on frames 7/0x1B while the held frame's
        // timing byte is exactly 2; frame 8 never steps.
        let mut game = test_game(8, 5, 0);
        game.entities[0].action_state = 1;
        game.entities[0].animation_frame_id = 8;
        game.entities[0].timing_control = 2;
        game.entities[0].unk_c6 = 0x8000;
        game.entities[0].unk_c8 = 0;
        game.entities[0].scd_timer = 0x40;
        let mut player = test_player();
        update(&mut game, &mut player, &room, &emd, &[], &[]);
        assert!(game.entity_sounds.is_empty(), "frame 8 is not a b5 frame");

        let mut game = test_game(8, 5, 0);
        game.entities[0].action_state = 1;
        game.entities[0].animation_frame_id = 7;
        game.entities[0].timing_control = 2;
        game.entities[0].unk_c6 = 0x8000;
        game.entities[0].unk_c8 = 0;
        game.entities[0].scd_timer = 0x40;
        let mut player = test_player();
        update(&mut game, &mut player, &room, &emd, &[], &[]);
        assert_eq!(game.entity_sounds.len(), 1, "frame 7 with timing 2 steps");
    }
}
