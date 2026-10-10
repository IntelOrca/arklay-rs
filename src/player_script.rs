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
//! The handlers move the player with the original's un-collided `Add_speedXZ`
//! step; [`scripted_animation`] then runs the room collision pass over the
//! tick's movement, pushing the player out of any obstruction the step
//! crossed. (The NPC state-8 drivers keep their pre-check + rollback instead;
//! see [`crate::enemy::walk::advance_xz_blocked`].)
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

use crate::enemy::anim::EntityAnim;
use crate::enemy::walk;
use crate::game::{BANK_SYSTEM, Entity, EntitySound, FlagBank, GameState, MSF2_EFFECT_ZONE};
use crate::model::Clip;
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
        // The generic hit reaction. Plant 42's acid spit writes state 2 with
        // `action_behavior = 0x64` and the plant's sweep/throw reactions hand
        // over to state 6 below; both must run or the player freezes.
        2 => hit_reaction(game, player, room, emd_clips),
        // The death fall: the scream, the fall clip, the sliding corpse and
        // the blood pool, ending in the blocked state the DIED screen runs.
        3 => death_fall(game, player, room, emd_clips),
        // The bitten/grab hold. The zombie writes state 5 and flags the
        // release by setting action_state 3; the original's handler runs the
        // held clip out and its final sub-state returns the player to state
        // 1. The grabbed-pose playback stays the documented player-grab
        // deferral, but the exit runs so a bite cannot park the player.
        5 => {
            let entity = &mut game.entities[0];
            if entity.action_state >= 3 {
                entity.set_state(1);
                entity.set_ignore(0);
                entity.action_behavior = 0;
                entity.action_state = 0;
                entity.is_being_attacked = 0;
            }
        }
        // The grabbed-hold window. The monster plant writes animationId 6
        // with animFrameId 0x0F, whose entry in the animation-function table
        // is the three-state hold table; Plant 42's sweep and release write
        // animationId 6 with animFrameId 8, whose entry is the attacked-flag
        // table (knockdown / grabbed / thrown). The prelude the hold windows
        // share runs first: a held pose raises the message bit while the
        // object push or the health lock is up, then clears the lock.
        6 => anim_window1(game, player, room, emd_clips),
        // The swallow window. Yawn writes animationId 7 with animFrameId
        // 0x0D, whose entry in the animation-function table is the swallowed
        // player's own machine.
        7 => anim_window2(game, player, emd_clips),
        // States 2/3 (hit reaction, death) hold the player still: only state 1
        // reads the pad.
        _ => {}
    }
}

/// The state-1 entry checks the original runs before the pad-driven control:
/// a dead player falls into the death animation and stops, a hit pre-empts
/// control into the generic reaction, and a poison status drains health on
/// the shared timer. Returns true when the tick was consumed (the pad-driven
/// control must not run).
///
/// The caller runs this only while the player is in the pad-driven state with
/// the message gate clear, exactly where the original's `player_state_01`
/// control handler runs.
pub fn control_gate(game: &mut GameState, player: &mut PlayerState) -> bool {
    // 0x00495180: dead - fall into the death animation and stop.
    if game.entities[0].health < 0 {
        // A back-shot death plays turned around: attackDirection 0x7FFF
        // flips the facing 180 degrees.
        if game.entities[0].action_ticks_counter == 0x7FFF {
            game.entities[0].angle = game.entities[0].angle.wrapping_add(0x800);
        }
        let entity = &mut game.entities[0];
        entity.set_state(3);
        entity.set_ignore(0);
        entity.action_state = 0;
        entity.action_behavior = 200;
        entity.is_being_attacked = 1;
        // The effect-zone flag is the "cannot die" guard.
        if game.flags[5].bit(MSF2_EFFECT_ZONE) {
            entity.health = 1;
        }
        let _ = player;
        return true;
    }

    // 0x004951cc: taking a hit pre-empts control and runs the damage state.
    if game.entities[0].is_being_attacked & 0x3F != 0 {
        let entity = &mut game.entities[0];
        entity.action_state = 0;
        entity.set_state(2);
        entity.set_ignore(0);
        return true;
    }

    // 0x004951e6: the poison-style status drain. The timer byte is read
    // before it is decremented, so the drain fires on the frame the old value
    // was already zero.
    if game.health_status & 0x62 != 0 {
        let prev = (game.poison_timer & 0xFF) as u8;
        game.poison_timer = u16::from(prev.wrapping_sub(1));
        if prev == 0 {
            let fast = game.health_status & 0x40 != 0;
            game.poison_timer = if fast { 7 } else { 120 };
            game.entities[0].health = game.entities[0].health.wrapping_sub(2);
            // Only the 0x40 variant is allowed to drive health negative.
            if game.entities[0].health < 0 && !fast {
                game.entities[0].health = 1;
            }
        }
    }

    if game.entities[0].next_turn_timer != 0 {
        game.entities[0].next_turn_timer -= 1;
    }
    false
}

/// Player state 3 (the original's death fall).
///
/// State 0 sets up the scream, the fall clip and the slide speed and falls
/// through into the fall advance; the clip's completion moves to state 2,
/// which starts the blood pool (except in the three scripted-death rooms) and
/// state 3, whose 148-frame countdown ends at state 4 (block input).
fn death_fall(game: &mut GameState, player: &mut PlayerState, room: &RoomState, clips: &[Clip]) {
    // [thud frame, slide facing offset] per character: the crawl speed comes
    // from `move_speed_current`, the offset is 0 for Chris and a quarter turn
    // for Jill.
    const DEATH_FALL: [u16; 4] = [0x19, 0, 0xF, 0x400];
    /// The fall clip: the EMD body bank's clip 4.
    const FALL_CLIP: u8 = 4;
    /// The corpse-slide frame countdown's terminal value.
    const POOL_END: u16 = 0x20;

    let character = usize::from(game.id.player_flag & 1);
    let state = game.entities[0].action_state;
    if state == 0 {
        {
            let entity = &mut game.entities[0];
            entity.action_state = 1;
            entity.animation_frame_id = 0;
            entity.move_speed_current = DEATH_FALL[character * 2];
            entity.unk_bf = 0;
            entity.attack_anim = FALL_CLIP;
            entity.is_being_attacked = 1;
            entity.unk_8c = 0;
            entity.action_ticks_counter = 0xB4;
        }
        game.message_flags &= 0xFFBF;
        play_player_sound(game, room, 3, 3);
    }
    if state <= 1 {
        // The body thud at motion frame 0x19.
        if game.entities[0].animation_frame_id == 0x19 && game.entities[0].unk_bf == 1 {
            play_entity_thud(game, room);
        }
        let completed = advance_player_body_clip(game, player, clips, FALL_CLIP, false, 0x400);
        game.entities[0].action_state = game.entities[0]
            .action_state
            .wrapping_add(u8::from(completed));
        // The slide: the speed is the character's crawl, the table's second
        // value is the facing offset (0 for Chris, a quarter turn for Jill).
        let offset = DEATH_FALL[character * 2 + 1];
        let speed = game.entities[0].move_speed_current as i16;
        walk::advance_xz(&mut game.entities[0], offset, speed);
        return;
    }
    match state {
        2 => {
            if !crate::ui::death::skips_blood_pool(game.id) {
                // The blood pool reuses the player's ground quad: recoloured
                // and shrunk, then grown a step a frame in state 3.
                player.shadow_tint = 0x00FF_FF50;
                player.shadow_half_x -= 100;
                player.shadow_half_z -= 100;
                let entity = &mut game.entities[0];
                entity.action_state = 3;
                entity.is_being_attacked = 0x80;
                return;
            }
            game.entities[0].set_state(4);
        }
        3 => {
            player.shadow_half_x += 0x10;
            player.shadow_half_z += 0x10;
            let counter = game.entities[0].action_ticks_counter.wrapping_sub(1);
            game.entities[0].action_ticks_counter = counter;
            if counter == POOL_END {
                game.entities[0].set_state(4);
            }
        }
        _ => {}
    }
}

/// Advance the player's body clip without disturbing the state byte: the
/// original's death fall reads the clip from `attackAnim` while `animationId`
/// stays 3, so the state machine keeps dispatching the fall.
fn advance_player_body_clip(
    game: &mut GameState,
    player: &mut PlayerState,
    clips: &[Clip],
    clip: u8,
    reverse: bool,
    step: u16,
) -> bool {
    let state = game.entities[0].state();
    game.entities[0].animation_id = clip;
    let completed = advance_player_clip(game, player, clips, reverse, step);
    game.entities[0].set_state(state);
    completed
}

/// Play one entity-table sound (`PlayEntitySnd`): the room's footstep-zone
/// table shifted by the sound type and the effect-zone slow offset.
fn play_entity_thud(game: &mut GameState, room: &RoomState) {
    const THUD_TYPE: u8 = 2;
    let slow = game.flags[5].bit(MSF2_EFFECT_ZONE);
    let pos = game.entities[0].pos;
    let Some(name) = crate::sfx::footstep_sound(room, pos, THUD_TYPE, slow) else {
        return;
    };
    let column = crate::sfx::entity_sound_column(room, pos, THUD_TYPE, slow).unwrap_or(0);
    game.entity_sounds.push(EntitySound {
        name,
        bank: 2,
        column,
        pos,
    });
}

/// Clip playback step of the grabbed-hold table's handlers (the original's
/// three `player_plant_hold_*` handlers all advance with `0x80`).
const HOLD_BLEND_STEP: u16 = 0x80;

/// The `animFrameId` offset of the state-6 window: the original's
/// `player_state_anim_window1` dispatches `animFrameId + 0x13` into the
/// animation-function table.
const WINDOW1_BASE: u8 = 0x13;

/// The monster plant's hold-table index: the plant grab writes animationId 6
/// with animFrameId `0x0F`, and `0x0F + 0x13` selects the three-state hold
/// table (`entry` -> `struggle` -> `release`) that shares the player's
/// `action_state` byte with the grabber.
const PLANT_HOLD_INDEX: u8 = 0x0F + WINDOW1_BASE;

/// The attacked-flag table's index: Plant 42's sweep and release write
/// animationId 6 with animFrameId 8, and `8 + 0x13` selects the three-handler
/// table (`knockdown` / `grabbed` / `thrown`) dispatched on the player's
/// `action_behavior`.
const ATTACKED_FLAG_INDEX: u8 = 8 + WINDOW1_BASE;

/// The Tyrant's stagger table index: its claw attacks write animationId 6 with
/// animFrameId `0x0C`, and `0x0C + 0x13` selects the Tyrant's own three-entry
/// table (backhand/heavy swing stagger, swipe/slash/thrust stagger, and the
/// full knock-back with the wall slam) dispatched on the player's
/// `action_behavior`.
const TYRANT_STAGGER_INDEX: u8 = 0x0C + WINDOW1_BASE;

/// Player state 6 (the original's animation window 1).
///
/// The window prelude runs on the entry frame only: a held pose raises the
/// message bit while the object push or the health lock is up, then clears
/// the lock. The entry selected by `animFrameId` then runs once per tick. The
/// monster plant's hold table, Plant 42's attacked-flag table and the Tyrant's
/// stagger/knock-down table are wired; the other windows (the Yawn swallow)
/// keep their pose until their groups land.
fn anim_window1(game: &mut GameState, player: &mut PlayerState, room: &RoomState, clips: &[Clip]) {
    if game.entities[0].action_state == 0 {
        if game.object_push || game.health_status & 0x80 != 0 {
            game.message_flags |= 0x40;
        }
        let status = game.health_status & 0x7F;
        game.set_health_status(status);
    }
    let index = game.entities[0].ignore().wrapping_add(WINDOW1_BASE);
    if index == PLANT_HOLD_INDEX {
        plant_hold(game, player, clips);
    } else if index == ATTACKED_FLAG_INDEX {
        attacked_flag(game, player, room, clips);
    } else if index == TYRANT_STAGGER_INDEX {
        tyrant_stagger(game, player, room, clips);
    }
}

/// The monster plant's grabbed-hold table, dispatched on the player's
/// `action_state`: 0 entry, 1 the struggle loop, 2 the release.
///
/// The grabber owns the exit: the drain step writes `action_state = 2` and the
/// release handler returns the player to the pad-driven state 1, spins a
/// player grabbed from behind (`animFrameId 2`) and clears the grabbed flag
/// bits. The clip played is the grab attack's own (`animationId`, the
/// grabber's per-facing pick), exactly like the original's `Joint_move` index
/// for the player.
fn plant_hold(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    let GameState {
        entities,
        entity_anims,
        player_flags,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    match entity.action_state {
        0 => {
            entity.action_state = 1;
            entity.animation_frame_id = 0;
            entity.timing_control = 0;
            entity.unk_8c = 0;
            clock.apply_anim_vertex(entity, clips);
            publish_pose(entity, clock, player, ClipSource::Emd);
        }
        1 => {
            clock.apply_anim_vertex(entity, clips);
            clock.advance(entity, clips, false, HOLD_BLEND_STEP);
            // The struggle loop's frames past 0x18 wrap back to 10 so the
            // hold cycles instead of running off the end of the clip.
            if entity.animation_frame_id > 0x18 {
                entity.animation_frame_id = 0x0A;
            }
            publish(entity, clock, player, ClipSource::Emd);
        }
        2 => {
            clock.apply_anim_vertex(entity, clips);
            if entity.animation_frame_id == 0x27 || entity.health < 0 {
                entity.set_state(1);
                entity.set_ignore(0);
                entity.action_behavior = 0;
                entity.action_state = 0;
                entity.is_being_attacked = 0;
                // A grab from behind spun the player around on entry.
                if entity.animation_id == 2 {
                    entity.angle = entity.angle.wrapping_add(0x800);
                }
                // The grabber's grabbed/step flags live in the shared player
                // flag byte; the release clears bits 1 and 3.
                *player_flags &= 0xF5;
            }
            clock.advance(entity, clips, false, HOLD_BLEND_STEP);
            publish(entity, clock, player, ClipSource::Emd);
        }
        _ => {}
    }
}

/// The `animFrameId` offset of the state-7 window: the original's
/// `player_state_anim_window2` dispatches `animFrameId + 0x26`.
const WINDOW2_BASE: u8 = 0x26;

/// The swallowed player's table index: Yawn's swallow writes animationId 7
/// with animFrameId `0x0D`.
const YAWN_SWALLOW_INDEX: u8 = 0x0D + WINDOW2_BASE;

/// Player state 7 (the original's animation window 2).
fn anim_window2(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    if game.entities[0].ignore().wrapping_add(WINDOW2_BASE) == YAWN_SWALLOW_INDEX {
        swallow_hold(game, player, clips);
    }
}

/// The Yawn head slot, for the swallowed player's head-death release and the
/// capture transform: the active `0x0D`/`0x12` entity that is not a body
/// segment.
fn yawn_head(game: &GameState) -> Option<usize> {
    (1..crate::game::ENTITY_COUNT).find(|&slot| {
        let entity = &game.entities[slot];
        entity.active() && matches!(entity.id, 0x0D | 0x12) && entity.behavior_flags != 1
    })
}

/// The swallowed player's own state machine (`player_anim_death_alt`):
/// action_state 0/1 advance the carried animation, 2 recomposes the player
/// matrix from the head joint and the shared capture transform every frame,
/// and the head's death hands the player back.
fn swallow_hold(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    let state = game.entities[0].action_state;
    if state == 0 {
        let entity = &mut game.entities[0];
        entity.action_state = 1;
        entity.animation_frame_id = 0;
        entity.unk_bf = 0;
        entity.is_being_attacked = 1;
        entity.animation_id = 2;
        entity.unk_8c = 3;
    }
    if state == 0 || state == 1 {
        let completed = advance_player_clip(game, player, clips, false, 0x400);
        game.entities[0].action_state = game.entities[0]
            .action_state
            .wrapping_add(u8::from(completed));
        let head_dead = yawn_head(game).is_some_and(|slot| game.entities[slot].health < 0);
        if head_dead {
            let entity = &mut game.entities[0];
            entity.set_state(1);
            entity.set_ignore(0);
            entity.action_behavior = 0;
            entity.action_state = 0;
            entity.is_being_attacked = 0;
            game.player_flags &= 0xF9;
        }
        return;
    }
    if state == 2 {
        game.entities[0].zone_flags |= 0x80;
        let Some(head) = yawn_head(game) else {
            return;
        };
        let Some(joint) = game.joint_worlds[head].first().copied() else {
            return;
        };
        let capture = game.yawn_capture;
        let held = crate::enemy::custom_anim::hold_player(&joint, &capture);
        // The original writes the player's whole local matrix; the port's
        // visible player poses from position and yaw only, so the composed
        // translation carries the hold and the captured orientation is a
        // documented approximation.
        game.entities[0].pos = held.t;
        player.pos = held.t;
    }
}

/// Player state 2 (the original's generic hit reaction).
///
/// Plant 42's acid spit writes `animationId = 2 / animFrameId = 0 /
/// action_behavior = 0x64` as one dword, so the state byte selects this
/// machine; `0x64`/`0x65` play the ordinary body clip, `0x66`/`0x67`/`0x68`
/// the damage clip selected by the attacked flag (the port plays the body
/// bank at the same id, the documented fallback).
fn hit_reaction(game: &mut GameState, player: &mut PlayerState, room: &RoomState, clips: &[Clip]) {
    if game.entities[0].ignore() != 0 {
        return;
    }
    if game.entities[0].action_state == 0 {
        if game.object_push || game.health_status & 0x80 != 0 {
            game.message_flags |= 0x40;
        }
        let status = game.health_status & 0x7F;
        game.set_health_status(status);
    }
    match game.entities[0].action_behavior {
        0x64 | 0x65 => {
            hit_react_common(game, player, room, clips, false, 0, 0);
            return;
        }
        0x66 => {
            hit_react_common(game, player, room, clips, true, 0, 0xFA);
            if game.entities[0].animation_frame_id > 3 {
                game.entities[0].move_speed_current = 0x1E;
            }
        }
        0x67 => {
            hit_react_common(game, player, room, clips, true, 0, 0xFA);
            if game.entities[0].animation_frame_id > 3 {
                game.entities[0].move_speed_current = 0x1E;
            }
            let speed = game.entities[0].move_speed_current as i16;
            walk::advance_xz(&mut game.entities[0], 0, speed);
            return;
        }
        0x68 => {
            hit_react_common(game, player, room, clips, true, 3, 0);
            return;
        }
        _ => return,
    }
    let speed = game.entities[0].move_speed_current as i16;
    walk::advance_xz(&mut game.entities[0], 0x800, speed);
}

/// The shared hit-reaction body: install the clip and hold, then advance the
/// clip until it completes and hand the player back to state 1.
#[allow(clippy::too_many_arguments)] // the handler signature mirrors the original
fn hit_react_common(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &[Clip],
    damage_anim: bool,
    sound: u8,
    start_speed: u16,
) {
    if game.entities[0].action_state == 0 {
        {
            let entity = &mut game.entities[0];
            entity.animation_id = if damage_anim {
                entity.is_being_attacked.wrapping_sub(1)
            } else {
                1
            };
            entity.attack_anim = entity.animation_id;
            entity.animation_frame_id = 0;
            entity.unk_bf = 0;
            entity.move_speed_current = start_speed;
            entity.action_state = 1;
            entity.unk_8c = 3;
        }
        play_player_sound(game, room, 3, sound);
    } else if game.entities[0].action_state != 1 {
        return;
    }
    if advance_player_clip(game, player, clips, false, 0x400) {
        let entity = &mut game.entities[0];
        return_to_control(entity);
        entity.is_being_attacked = 0;
    }
}

/// The state-6 attacked-flag table (`player_anim_set_attacked_flag`),
/// dispatched on the player's `action_behavior`: the fall/get-up chain, the
/// grabbed hold and the thrown drop.
fn attacked_flag(game: &mut GameState, player: &mut PlayerState, room: &RoomState, clips: &[Clip]) {
    match game.entities[0].action_behavior {
        0 => knockdown_recover(game, player, room, clips),
        1 => grabbed(game, player, clips),
        2 => thrown(game, player, room, clips),
        _ => {}
    }
}

/// Behavior 0: the knockdown's fall/slide/get-up chain. Its states 4 and 0x0B
/// are what give the player back to the controller.
fn knockdown_recover(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &[Clip],
) {
    let state = game.entities[0].action_state;
    match state {
        0 | 1 => {
            if state == 0 {
                {
                    let entity = &mut game.entities[0];
                    entity.action_state = 1;
                    entity.animation_frame_id = 0;
                    entity.unk_bf = 0;
                    set_clip(entity, 6);
                    entity.unk_8c = 4;
                    entity.move_speed_current = 1000;
                }
                play_player_sound(game, room, 3, 2);
            }
            let step = u16::from(game.entities[0].animation_frame_id).wrapping_mul(0xFFF1);
            game.entities[0].move_speed_current =
                game.entities[0].move_speed_current.wrapping_add(step);
            let completed = advance_player_clip(game, player, clips, false, 0x400);
            game.entities[0].action_state = game.entities[0]
                .action_state
                .wrapping_add(u8::from(completed));
            let speed = game.entities[0].move_speed_current as i16;
            walk::advance_xz(&mut game.entities[0], 0x800, speed);
            // The collision test is probe-only: the resolve runs on a copy and
            // only its hit result is kept.
            let mut probe = game.entities[0];
            if crate::enemy::walk::check_room_collision(room, &mut probe) != 0 {
                game.entities[0].action_state = 5;
            }
        }
        2 | 3 => {
            if state == 2 {
                {
                    let entity = &mut game.entities[0];
                    entity.action_state = 3;
                    entity.unk_8c = 3;
                    entity.unk_bf = 0;
                    set_clip(entity, 7);
                }
                play_player_sound(game, room, 2, 0x1D);
            }
            let completed = advance_player_clip(game, player, clips, false, 0x400);
            game.entities[0].action_state = game.entities[0]
                .action_state
                .wrapping_add(u8::from(completed));
            let speed = game.entities[0].move_speed_current as i16;
            walk::advance_xz(&mut game.entities[0], 0x800, speed);
            game.entities[0].move_speed_current =
                game.entities[0].move_speed_current.wrapping_sub(0xF);
            if (game.entities[0].move_speed_current as i16) < 0 {
                game.entities[0].move_speed_current = 0;
            }
        }
        4 => {
            let entity = &mut game.entities[0];
            return_to_control(entity);
            entity.is_being_attacked = 0;
        }
        5 | 6 => {
            if state == 5 {
                {
                    let entity = &mut game.entities[0];
                    entity.action_state = 6;
                    entity.animation_frame_id = 0;
                    entity.unk_bf = 0;
                    set_clip(entity, 4);
                    entity.unk_8c = 3;
                }
                play_player_sound(game, room, 2, 0x1A);
                play_player_sound(game, room, 3, 2);
            }
            advance_player_clip(game, player, clips, false, 0x400);
        }
        7 | 8 => {
            if state == 7 {
                let entity = &mut game.entities[0];
                entity.action_state = 8;
                entity.unk_bf = 0;
                set_clip(entity, 5);
                entity.unk_8c = 3;
            }
            advance_player_clip(game, player, clips, false, 0x400);
        }
        9 | 10 => {
            if state == 9 {
                let entity = &mut game.entities[0];
                entity.action_state = 10;
                entity.unk_bf = 0;
                set_clip(entity, 4);
                entity.unk_8c = 3;
            }
            let completed = advance_player_clip(game, player, clips, true, 0x400);
            game.entities[0].action_state = game.entities[0]
                .action_state
                .wrapping_add(u8::from(completed));
        }
        0x0B => {
            let entity = &mut game.entities[0];
            return_to_control(entity);
            entity.is_being_attacked = 0;
            entity.flags &= 0xFD;
        }
        _ => {}
    }
    publish_player(game, player);
}

/// Behavior 1: the held/grabbed pose, advanced entirely by the clip clock
/// while the grabber drives the position.
fn grabbed(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    let state = game.entities[0].action_state;
    match state {
        0 | 1 => {
            if state == 0 {
                let entity = &mut game.entities[0];
                entity.action_state = 1;
                entity.unk_8c = 3;
                entity.animation_frame_id = 0;
                entity.unk_bf = 0;
                set_clip(entity, 0);
            }
            let completed = advance_player_clip(game, player, clips, false, 0x400);
            game.entities[0].action_state = game.entities[0]
                .action_state
                .wrapping_add(u8::from(completed));
            return;
        }
        2 | 3 => {
            if state == 2 {
                let entity = &mut game.entities[0];
                entity.unk_bf = 0;
                set_clip(entity, 1);
                entity.action_state = 3;
                entity.unk_8c = 3;
            }
            advance_player_clip(game, player, clips, false, 0x400);
            return;
        }
        4 | 5 => {
            if state == 4 {
                let entity = &mut game.entities[0];
                entity.action_state = 5;
                set_clip(entity, 2);
                entity.unk_8c = 3;
                entity.animation_frame_id = 0;
                entity.unk_bf = 0;
            }
            advance_player_clip(game, player, clips, false, 0x400);
            return;
        }
        6 => {
            // The head gore state and its sprays: the joint flag write is
            // kept, the per-joint billboards stay with the joint-effect
            // deferral.
            let entity = &mut game.entities[0];
            entity.set_joint_flag(2, entity.joint_flag(2) | 0x0C);
            entity.action_state = 7;
        }
        _ => return,
    }
    publish_player(game, player);
}

/// Behavior 2: the thrown/dropped pose. The slide reads the capture matrix's
/// `t[0]` the sweep stored as the knock-back facing.
fn thrown(game: &mut GameState, player: &mut PlayerState, room: &RoomState, clips: &[Clip]) {
    let state = game.entities[0].action_state;
    match state {
        0 | 1 => {
            if state == 0 {
                let entity = &mut game.entities[0];
                entity.action_state = 1;
                set_clip(entity, 3);
                entity.animation_frame_id = 0;
                entity.unk_bf = 0;
                if entity.health < 0 {
                    play_player_sound(game, room, 3, 3);
                }
            }
            if game.entities[0].animation_frame_id == 0x0C {
                let entity = &mut game.entities[0];
                entity.action_state = 2;
                entity.action_ticks_counter = 0x5A;
                if entity.health < 0 {
                    entity.action_state = 8;
                }
            }
            advance_player_clip(game, player, clips, false, 0x400);
        }
        2 => {
            let pressed = game.player_mashing();
            let decrease = u16::from(pressed) * 3 + 1;
            game.entities[0].action_ticks_counter =
                game.entities[0].action_ticks_counter.wrapping_sub(decrease);
            if (game.entities[0].action_ticks_counter as i16) < 0 {
                game.entities[0].action_state = 3;
            }
            publish_player(game, player);
        }
        3 => {
            let completed = advance_player_clip(game, player, clips, false, 0x400);
            game.entities[0].action_state = game.entities[0]
                .action_state
                .wrapping_add(u8::from(completed));
        }
        4 => {
            let entity = &mut game.entities[0];
            entity.action_behavior = 0;
            entity.action_state = 7;
            publish_player(game, player);
        }
        5 | 6 => {
            if state == 5 {
                {
                    let entity = &mut game.entities[0];
                    entity.action_state = 6;
                    entity.animation_frame_id = 0;
                    entity.unk_bf = 0;
                    set_clip(entity, 3);
                    entity.unk_8c = 7;
                    entity.action_ticks_counter = 0;
                    entity.move_speed_current = 500;
                }
                play_player_sound(game, room, 3, 1);
                play_player_sound(game, room, 2, 0x19);
            }
            let completed = advance_player_clip(game, player, clips, false, 0x200);
            game.entities[0].action_state = game.entities[0]
                .action_state
                .wrapping_add(u8::from(completed));
            if game.entities[0].animation_frame_id < 0x0F {
                // Slide along the knock-back facing, then restore the real
                // one. The player's facing is the original's directionAngle.
                let saved = game.entities[0].angle;
                game.entities[0].angle = game.plant42_capture.t[0] as i16 as u16;
                let speed = game.entities[0].move_speed_current as i16;
                walk::advance_xz(&mut game.entities[0], 0, speed);
                let step = game.entities[0].action_ticks_counter as i16;
                game.entities[0].action_ticks_counter =
                    game.entities[0].action_ticks_counter.wrapping_add(1);
                game.entities[0].angle = saved;
                game.entities[0].move_speed_current = game.entities[0]
                    .move_speed_current
                    .wrapping_add(step.wrapping_mul(-5) as u16);
            }
        }
        7 => {
            let entity = &mut game.entities[0];
            entity.action_behavior = 0;
            entity.action_state = 7;
            publish_player(game, player);
        }
        _ => {}
    }
}

/// The Tyrant stagger windows (the original's table at the Tyrant's own data
/// block, dispatched on the player's `action_behavior`): 0 the backhand and
/// heavy-swing stagger, 1 the swipe/slash/thrust stagger, 2 the full
/// knock-back with the slide, wall slam and get-up.
fn tyrant_stagger(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &[Clip],
) {
    match game.entities[0].action_behavior {
        0 => tyrant_stagger_common(game, player, room, clips, 600, -0x28, -0x514, 0xDF4),
        1 => tyrant_stagger_common(game, player, room, clips, 500, -0x1E, -0x5DC, 500),
        2 => tyrant_knockback(game, player, room, clips),
        _ => {}
    }
}

/// The shared body of the two stagger variants: the entry pins the reaction
/// pose, the exit hands the player back; every live tick rotates a nearly-dead
/// player toward the attacker, sprays blood on the opening frames, advances
/// the damage clip and slides along the knock-back direction.
#[allow(clippy::too_many_arguments)] // the original's constants are parameters
fn tyrant_stagger_common(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &[Clip],
    start_speed: u16,
    decay: i32,
    blood_y: i32,
    push_bias: i32,
) {
    let sub = game.entities[0].action_state;
    if sub == 0 {
        let entity = &mut game.entities[0];
        entity.action_state = 1;
        entity.animation_frame_id = 0;
        entity.unk_bf = 0;
        entity.is_being_attacked = 1;
        entity.unk_8c = 3;
        entity.move_speed_current = start_speed;
    } else if sub != 1 {
        if sub == 2 {
            // `animationId = 1` hands the player back to the ordinary machine
            // (the port's state 1, with the window frame cleared).
            let entity = &mut game.entities[0];
            entity.animation_id = 1;
            entity.animation_frame_id = 0;
            entity.action_behavior = 0;
            entity.action_state = 0;
            entity.is_being_attacked = 0;
            entity.set_state(1);
            entity.set_ignore(0);
        }
        tyrant_stagger_slide(game, decay, push_bias);
        publish_player(game, player);
        return;
    }

    let attacker = game.player_attacker.map(usize::from);
    if game.entities[0].health < 0x1F
        && let Some(slot) = attacker
    {
        let target = game.entities[slot].pos;
        walk::rotate_toward_target(&mut game.entities[0], target, 0x40);
    }

    if game.entities[0].animation_frame_id == 3 {
        let sound = game.entities[0].attack_anim.wrapping_add(1);
        play_player_sound(game, room, 3, sound);
    }

    if game.entities[0].animation_frame_id < 4 {
        spawn_claw_blood(game, attacker, [0, 800, 0]);
        spawn_player_blood(game, [0, blood_y, 0]);
    }

    let completed = advance_player_clip(game, player, clips, false, 0x400);
    game.entities[0].action_state = game.entities[0]
        .action_state
        .wrapping_add(u8::from(completed));
    tyrant_stagger_slide(game, decay, push_bias);
    player.pos = game.entities[0].pos;
}

/// The stagger's shared tail: bleed the speed off and add the knock-back
/// step, biased by the attacker's facing.
fn tyrant_stagger_slide(game: &mut GameState, decay: i32, push_bias: i32) {
    player_decay_speed(game, decay);
    let attacker_angle = game
        .player_attacker
        .and_then(|slot| game.entities.get(usize::from(slot)))
        .map_or(0, |entity| i32::from(entity.angle));
    let offset = (attacker_angle - i32::from(game.entities[0].angle) + push_bias) as u16;
    let speed = game.entities[0].move_speed_current as i16;
    walk::advance_xz(&mut game.entities[0], offset, speed);
}

/// `ty_playerDecaySpeed`: `speed += frame * decay`, clamped at zero.
fn player_decay_speed(game: &mut GameState, decay: i32) {
    let frame = i32::from(game.entities[0].animation_frame_id);
    let value = i32::from(game.entities[0].move_speed_current) + frame * decay;
    let value = value as u16;
    game.entities[0].move_speed_current = if (value as i16) < 0 { 0 } else { value };
}

/// The full knock-back: state 1 launches the player, 2/3 impact and slide,
/// 5/6 the wall slam, 7..10 the get-up (played backwards) and 11 the return to
/// the controller.
fn tyrant_knockback(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &[Clip],
) {
    match game.entities[0].action_state {
        0 => {
            let entity = &mut game.entities[0];
            entity.action_state = 1;
            entity.animation_frame_id = 0;
            entity.unk_bf = 0;
            entity.is_being_attacked = 1;
            entity.attack_anim = 5;
            entity.unk_8c = 4;
            entity.move_speed_current = 900;
            tyrant_knockback_slide(game, player, room, clips);
        }
        1 => tyrant_knockback_slide(game, player, room, clips),
        2 => {
            {
                let entity = &mut game.entities[0];
                entity.action_state = 3;
                entity.unk_8c = 3;
                entity.unk_bf = 0;
                entity.attack_anim = 6;
            }
            play_player_sound(game, room, 2, 0x1F);
            tyrant_knockback_ground(game, player, clips);
        }
        3 => tyrant_knockback_ground(game, player, clips),
        4 => {
            let entity = &mut game.entities[0];
            entity.animation_id = 1;
            entity.animation_frame_id = 0;
            entity.action_behavior = 0;
            entity.action_state = 0;
            entity.is_being_attacked = 0;
            entity.set_state(1);
            entity.set_ignore(0);
        }
        5 => {
            {
                let entity = &mut game.entities[0];
                entity.action_state = 6;
                entity.animation_frame_id = 0;
                entity.unk_bf = 0;
                entity.attack_anim = 3;
                entity.unk_8c = 3;
            }
            play_player_sound(game, room, 2, 0x20);
            play_player_sound(game, room, 3, 2);
            // The original sprays two joint blood sheets (player joints 5/8,
            // depth 0x16) and three limb sheets (0/3/6, depth 0x11) with the
            // dead-move anchor shifted -400 on X; the port's player skeleton
            // is not the held pose, so one body sheet at the same offset is
            // the documented approximation.
            spawn_player_blood(game, [-400, 0, 0]);
            tyrant_knockback_slam(game, player, clips);
        }
        6 => tyrant_knockback_slam(game, player, clips),
        7 => {
            let entity = &mut game.entities[0];
            entity.action_state = 8;
            entity.unk_bf = 0;
            entity.attack_anim = 4;
            entity.unk_8c = 3;
            tyrant_knockback_getup(game, player, clips);
        }
        8 => tyrant_knockback_getup(game, player, clips),
        9 => {
            let entity = &mut game.entities[0];
            entity.action_state = 10;
            entity.unk_bf = 0;
            entity.attack_anim = 4;
            entity.unk_8c = 3;
            // Case 9 falls into case 10: the get-up plays backwards.
            tyrant_knockback_getup_reverse(game, player, clips);
        }
        10 => tyrant_knockback_getup_reverse(game, player, clips),
        11 => {
            let entity = &mut game.entities[0];
            entity.animation_id = 1;
            entity.animation_frame_id = 0;
            entity.action_behavior = 0;
            entity.action_state = 0;
            entity.is_being_attacked = 0;
            entity.set_state(1);
            entity.set_ignore(0);
            entity.flags &= 0xFD;
        }
        _ => {}
    }
}

/// Knock-back states 0/1: decelerate, advance the launch clip, slide, then
/// probe the room; a wall hit jumps straight to the slam.
fn tyrant_knockback_slide(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &[Clip],
) {
    let step = u16::from(game.entities[0].animation_frame_id).wrapping_mul(0xFFF4); // * -0xc
    game.entities[0].move_speed_current = game.entities[0].move_speed_current.wrapping_add(step);
    if game.entities[0].animation_frame_id == 3 {
        play_player_sound(game, room, 3, 2);
    }
    let completed = advance_player_clip(game, player, clips, false, 0x400);
    game.entities[0].action_state = game.entities[0]
        .action_state
        .wrapping_add(u8::from(completed));
    let speed = game.entities[0].move_speed_current as i16;
    walk::advance_xz(&mut game.entities[0], 0x800, speed);
    player.pos = game.entities[0].pos;

    let before = game.entities[0].pos;
    let code = walk::check_room_collision(room, &mut game.entities[0]);
    game.entities[0].pos = before;
    if code != 0 {
        game.entities[0].action_state = 5;
        game.entities[0].is_being_attacked = 0;
    }
}

/// Knock-back states 2/3: the ground slide with the joint blood sheets.
fn tyrant_knockback_ground(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    if game.entities[0].animation_frame_id & 1 == 0 && game.entities[0].animation_frame_id < 10 {
        spawn_player_blood(game, [0, 0, 0]);
    }
    let completed = advance_player_clip(game, player, clips, false, 0x400);
    game.entities[0].action_state = game.entities[0]
        .action_state
        .wrapping_add(u8::from(completed));
    let speed = game.entities[0].move_speed_current as i16;
    walk::advance_xz(&mut game.entities[0], 0x800, speed);
    player.pos = game.entities[0].pos;
    let value = game.entities[0].move_speed_current.wrapping_sub(0x0C);
    game.entities[0].move_speed_current = if (value as i16) < 0 { 0 } else { value };
}

/// Knock-back states 5/6: the wall slam, held on the impact frame.
fn tyrant_knockback_slam(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    let completed = advance_player_clip(game, player, clips, false, 0x400);
    game.entities[0].action_state = game.entities[0]
        .action_state
        .wrapping_add(u8::from(completed));
}

/// Knock-back states 7..10: the get-up.
fn tyrant_knockback_getup(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    let completed = advance_player_clip(game, player, clips, false, 0x400);
    game.entities[0].action_state = game.entities[0]
        .action_state
        .wrapping_add(u8::from(completed));
}

/// Knock-back states 9/10: the get-up played backwards (the original's
/// reverse `Joint_move` through the joint-move pair; the port plays the body
/// bank backwards, the documented fallback).
fn tyrant_knockback_getup_reverse(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    let completed = advance_player_clip(game, player, clips, true, 0x400);
    game.entities[0].action_state = game.entities[0]
        .action_state
        .wrapping_add(u8::from(completed));
}

/// A blood sheet attached to the current attacker's claw joint (its joint 8
/// world matrix) with a local offset.
fn spawn_claw_blood(game: &mut GameState, attacker: Option<usize>, offset: [i32; 3]) {
    let Some(slot) = attacker else {
        return;
    };
    let room_effects = std::rc::Rc::clone(&game.room_effects);
    crate::effects::create_attached(
        game,
        &room_effects,
        0,
        0,
        crate::effects::Attach::Joint(slot as u8, 8),
        offset,
        0,
        0,
    );
}

/// A blood sheet at the player's own transform with a local offset.
fn spawn_player_blood(game: &mut GameState, offset: [i32; 3]) {
    let room_effects = std::rc::Rc::clone(&game.room_effects);
    crate::effects::create_attached(
        game,
        &room_effects,
        0,
        0,
        crate::effects::Attach::Player,
        offset,
        0,
        0,
    );
}

/// Queue a `Play3DSnd` cue at the player's position.
fn play_player_sound(game: &mut GameState, room: &RoomState, bank: u8, id: u8) {
    let character = game.id.player_flag;
    if let Some((name, bank, column)) = crate::sfx::play_3d_cue(room, character, bank, id) {
        let pos = game.entities[0].pos;
        game.entity_sounds.push(EntitySound {
            name,
            bank,
            column,
            pos,
        });
    }
}

/// Advance the player's damage clip one tick and publish the words. The
/// original reads the clip index from `attackAnim` and the animation pair
/// from the damage scratch; the port plays the body EMD bank at the same id
/// (the scratch pair is not extracted), the documented fallback.
fn advance_player_clip(
    game: &mut GameState,
    player: &mut PlayerState,
    clips: &[Clip],
    reverse: bool,
    step: u16,
) -> bool {
    let GameState {
        entities,
        entity_anims,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    let completed = clock.advance(entity, clips, reverse, step);
    publish(entity, clock, player, ClipSource::Emd);
    completed
}

/// Publish the player's current scripted words to the render clock.
fn publish_player(game: &mut GameState, player: &mut PlayerState) {
    let GameState {
        entities,
        entity_anims,
        ..
    } = game;
    publish(&entities[0], &mut entity_anims[0], player, ClipSource::Emd);
}

/// Dispatch one state-8 tick on `action_behavior`, resolve the tick's movement
/// against the room collision, then mirror the entity's position and facing
/// onto the visible player.
///
/// The original runs the handler a second time when `flags` bit 1 is set,
/// re-reading `action_behavior` for the repeat, exactly like the NPC driver.
/// The handlers step with the un-collided `Add_speedXZ`, so this tail is the
/// player's only collision pass: it pushes the player out of any obstruction
/// the step crossed, anchored at the pre-dispatch position.
fn scripted_animation(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    emd_clips: &[Clip],
    emw_clips: &[Clip],
    room_clips: &[Clip],
) {
    let prev_pos = game.entities[0].pos;
    let behavior = game.entities[0].action_behavior;
    dispatch_behavior(
        game, player, room, emd_clips, emw_clips, room_clips, behavior,
    );
    if game.entities[0].flags & 2 != 0 {
        let repeat = game.entities[0].action_behavior;
        dispatch_behavior(game, player, room, emd_clips, emw_clips, room_clips, repeat);
    }
    let entity = &mut game.entities[0];
    entity.pos = crate::player::resolve_collision(
        &room.collision,
        prev_pos,
        entity.pos,
        i32::from(entity.sca_radius),
        entity.collision_flags,
    );
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
            run_stop_step(entity, clock, emd_clips);
        }
        5 => run_stop_step(entity, clock, emd_clips),
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
    backward_step(entity, clock, clips, system, health_locked, speed);
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
    walk::advance_xz(entity, 0x800, entity.move_speed_current as i16);
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
    walk::advance_xz(entity, 0, entity.move_speed_current as i16);
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
    walk::advance_xz(entity, 0, entity.move_speed_current as i16);
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
fn run_stop_step(entity: &mut Entity, clock: &mut EntityAnim, clips: &[Clip]) {
    clock.advance(entity, clips, entity.flags & 1 != 0, MOVE_BLEND_STEP);
    entity.action_ticks_counter = entity.action_ticks_counter.wrapping_add(1);
    if (entity.action_ticks_counter as i16) > 3 {
        entity.action_state = 6;
    }
    entity.move_speed_current = (entity.move_speed_current as i16).wrapping_sub(0x1E) as u16;
    walk::advance_xz(entity, 0, entity.move_speed_current as i16);
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
        ClipSource::Weapon => &[],
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
    fn the_bitten_hold_releases_the_player_after_the_grabber_flags_it() {
        let mut game = test_game(5, 0, 0);
        let mut player = test_player();
        game.entities[0].action_state = 0;
        game.entities[0].is_being_attacked = 1;
        // Still held: the grabber has not written the release sub-state.
        run(&mut game, &mut player, &[], &[], &[]);
        assert_eq!(game.entities[0].state(), 5);
        assert_eq!(game.entities[0].is_being_attacked, 1);

        // The zombie writes action_state 3 when the bite timer expires: the
        // player returns to the pad-driven state and the latch clears.
        game.entities[0].action_state = 3;
        run(&mut game, &mut player, &[], &[], &[]);
        assert_eq!(game.entities[0].state(), 1);
        assert_eq!(game.entities[0].ignore(), 0);
        assert_eq!(game.entities[0].action_behavior, 0);
        assert_eq!(game.entities[0].action_state, 0);
        assert_eq!(game.entities[0].is_being_attacked, 0);
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

    /// The monster plant grabs by writing player state 6 / frame window 0x0F;
    /// the entry selects the hold table, which advances the grab clip and
    /// releases the player when the drain step flags action_state 2.
    fn plant_hold_game(action_state: u8) -> GameState {
        let mut game = test_game(6, 0, 2);
        game.entities[0].set_ignore(0x0F);
        game.entities[0].action_state = action_state;
        game
    }

    #[test]
    fn the_plant_hold_entry_starts_the_struggle_loop() {
        let emd = vec![clip(0x40, 1); 4];
        let mut game = plant_hold_game(0);
        game.health_status = 0x80;
        game.object_push = true;
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].action_state, 1, "entry advances the hold");
        assert_eq!(game.entities[0].animation_frame_id, 0);
        assert_eq!(game.entities[0].unk_8c, 0);
        assert_eq!(
            game.message_flags & 0x40,
            0x40,
            "a held pose raises the message bit while the push/lock is up"
        );
        assert_eq!(game.health_status & 0x80, 0, "the lock is cleared");
        assert_eq!(player.anim.clip, 2, "the grab clip stays selected");
        assert_eq!(player.clip_source, ClipSource::Emd);
    }

    #[test]
    fn the_plant_hold_struggle_wraps_past_frame_18() {
        let emd = vec![clip(0x40, 1); 4];
        let mut game = plant_hold_game(1);
        let mut player = test_player();
        for _ in 0..0x19 {
            update(
                &mut game,
                &mut player,
                &RoomState::default(),
                &emd,
                &[],
                &[],
            );
        }
        assert_eq!(
            game.entities[0].animation_frame_id, 0x0A,
            "frames past 0x18 wrap back to 10"
        );
        assert_eq!(game.entities[0].action_state, 1, "still held");
    }

    #[test]
    fn the_plant_hold_release_returns_control_and_spins_a_back_grab() {
        let emd = vec![clip(0x40, 1); 4];
        let mut game = plant_hold_game(2);
        game.entities[0].animation_frame_id = 0x27;
        game.player_flags = 0x0002;
        game.entities[0].is_being_attacked = 1;
        game.entities[0].angle = 0x100;
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].state(), 1, "back to the pad-driven state");
        assert_eq!(game.entities[0].ignore(), 0);
        assert_eq!(game.entities[0].action_behavior, 0);
        assert_eq!(game.entities[0].action_state, 0);
        assert_eq!(game.entities[0].is_being_attacked, 0);
        assert_eq!(
            game.entities[0].angle, 0x900,
            "attackAnim 2 spins the player"
        );
        assert_eq!(game.player_flags & 0x0002, 0, "the grabbed flag clears");
    }

    #[test]
    fn the_plant_hold_release_also_fires_when_the_player_died() {
        let emd = vec![clip(0x40, 1); 4];
        let mut game = plant_hold_game(2);
        game.entities[0].animation_frame_id = 0x10;
        game.entities[0].health = -1;
        game.entities[0].animation_id = 1;
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].state(), 1);
        assert_eq!(game.entities[0].angle, 0, "a front grab does not spin");
    }

    /// Yawn's swallow writes player state 7 with animFrameId 0x0D; the state-7
    /// window runs the swallowed player's own machine.
    fn swallow_game(action_state: u8) -> GameState {
        let mut game = test_game(7, 0, 0);
        game.entities[0].set_ignore(0x0D);
        game.entities[0].action_state = action_state;
        let head = &mut game.entities[1];
        head.id = 0x0D;
        head.set_active(true);
        head.status_flags = 1;
        head.behavior_flags = 2;
        head.health = 0x0BEA;
        let identity = crate::anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        };
        game.joint_worlds[1] = vec![identity; 15];
        game
    }

    #[test]
    fn the_swallow_entry_starts_the_carried_pose() {
        let emd = vec![clip(0x40, 1); 4];
        let mut game = swallow_game(0);
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].action_state, 1, "the entry advances");
        assert_eq!(
            game.entities[0].animation_frame_id, 1,
            "the entry falls through into the carried tick"
        );
        assert_eq!(game.entities[0].animation_id, 2, "the carried clip");
        assert_eq!(game.entities[0].unk_8c, 3);
        assert_eq!(game.entities[0].is_being_attacked, 1);
    }

    #[test]
    fn the_swallow_hold_recomposes_the_player_from_the_head_joint() {
        let emd = vec![clip(0x40, 1); 4];
        let mut game = swallow_game(2);
        game.joint_worlds[1][0].t = [1000, 0, 2000];
        game.yawn_capture = crate::anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [50, 700, 0],
        };
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].zone_flags & 0x80, 0x80);
        assert_eq!(game.entities[0].pos, [1050, 700, 2000]);
        assert_eq!(player.pos, [1050, 700, 2000]);
    }

    #[test]
    fn the_swallow_release_follows_the_head_death() {
        let emd = vec![clip(0x40, 1); 4];
        let mut game = swallow_game(1);
        game.entities[1].health = -1;
        game.player_flags = 0x06;
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].state(), 1, "back to the pad-driven state");
        assert_eq!(game.entities[0].ignore(), 0);
        assert_eq!(game.entities[0].action_behavior, 0);
        assert_eq!(game.entities[0].action_state, 0);
        assert_eq!(game.entities[0].is_being_attacked, 0);
        assert_eq!(game.player_flags & 0x06, 0, "the grabbed bits clear");
    }

    #[test]
    fn other_state_six_windows_hold_the_pose_until_their_groups_land() {
        let emd = vec![clip(0x40, 1); 4];
        let mut game = test_game(6, 5, 2);
        game.entities[0].set_ignore(0x09);
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(
            game.entities[0].state(),
            6,
            "the unknown window stays parked"
        );
        assert_eq!(game.entities[0].action_behavior, 5);
        assert_eq!(game.entities[0].action_state, 0);
    }

    /// Plant 42's acid spit writes player state 2 / behavior 0x64; the
    /// generic hit reaction plays the body clip and hands control back.
    #[test]
    fn the_spit_hit_reaction_plays_the_body_clip_and_returns_control() {
        let emd = vec![clip(2, 1); 4];
        let mut game = test_game(2, 0x64, 0);
        game.entities[0].is_being_attacked = 1;
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].action_state, 1);
        assert_eq!(game.entities[0].animation_id, 1, "the ordinary clip");
        assert_eq!(game.entities[0].move_speed_current, 0);
        assert_eq!(game.entities[0].unk_8c, 3);
        // Two-frame clip: the second advance wraps and returns control.
        for _ in 0..4 {
            update(
                &mut game,
                &mut player,
                &RoomState::default(),
                &emd,
                &[],
                &[],
            );
            if game.entities[0].state() == 1 {
                break;
            }
        }
        assert_eq!(game.entities[0].state(), 1, "control returns");
        assert_eq!(game.entities[0].is_being_attacked, 0);
    }

    /// The heavy reactions select the damage clip from the attacked flag.
    #[test]
    fn the_heavy_hit_reaction_selects_the_attacked_clip() {
        let emd = vec![clip(2, 1); 4];
        let mut game = test_game(2, 0x66, 0);
        game.entities[0].is_being_attacked = 3;
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].animation_id, 2, "the attacked clip");
        assert_eq!(
            game.entities[0].move_speed_current, 0xFA,
            "the heavy start speed"
        );
    }

    /// The attacked-flag table's behavior 0: the fall/slide/get-up chain.
    #[test]
    fn the_knockdown_falls_slides_and_recovers() {
        let emd = vec![clip(2, 1); 12];
        let mut game = test_game(6, 0, 6);
        game.entities[0].set_ignore(8);
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].action_state, 1);
        assert_eq!(game.entities[0].animation_id, 6, "the fall clip");
        assert_eq!(game.entities[0].unk_8c, 4);
        assert_eq!(game.entities[0].move_speed_current, 1000);
        for _ in 0..40 {
            update(
                &mut game,
                &mut player,
                &RoomState::default(),
                &emd,
                &[],
                &[],
            );
            if game.entities[0].state() == 1 {
                break;
            }
        }
        assert_eq!(game.entities[0].state(), 1, "the get-up returns control");
        assert_eq!(game.entities[0].is_being_attacked, 0);
        assert_eq!(
            game.player_flags & 0xFD,
            game.player_flags,
            "the flag clears"
        );
    }

    /// The attacked-flag table's behavior 2: the thrown slide reads the
    /// capture matrix's `t[0]` as the knock-back facing.
    #[test]
    fn the_thrown_slide_reads_the_capture_matrix() {
        let emd = vec![clip(0x40, 1); 4];
        let mut game = test_game(6, 2, 3);
        game.entities[0].set_ignore(8);
        game.entities[0].action_state = 6;
        game.entities[0].animation_id = 3;
        game.entities[0].attack_anim = 3;
        game.entities[0].animation_frame_id = 0;
        game.entities[0].move_speed_current = 500;
        game.entities[0].action_ticks_counter = 0;
        game.entities[0].angle = 0x100;
        game.entities[0].pos = [100, 0, 100];
        game.plant42_capture.t[0] = 0x800;
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].angle, 0x100, "the real facing is restored");
        let (dx, dz) = crate::player::rotate_xz(0x800, 500, 0);
        assert_eq!(game.entities[0].pos, [100 + dx, 0, 100 + dz]);
        assert_eq!(game.entities[0].action_ticks_counter, 1);
        assert_eq!(
            game.entities[0].move_speed_current, 500,
            "step 0 costs nothing"
        );
    }

    /// A room whose one record blocks everything around the player origin, so
    /// the Tyrant knock-back's collision probe reports a wall hit.
    fn tyrant_walled_room() -> RoomState {
        use crate::state::{Collision, CollisionRect};
        RoomState {
            collision: Collision {
                cell_x: 0,
                cell_z: 0,
                quadrants: std::array::from_fn(|_| {
                    vec![CollisionRect {
                        x_max: 3000,
                        z_max: 3000,
                        x_min: 500,
                        z_min: 500,
                        kind: 1,
                        flags: 0x300,
                    }]
                }),
            },
            ..RoomState::default()
        }
    }

    /// The Tyrant's stagger window (behaviour 1): the entry pins the reaction
    /// pose, the clip advances and the exit hands the player back.
    #[test]
    fn the_tyrant_stagger_window_plays_and_returns() {
        let emd = vec![clip(0x40, 1); 12];
        let mut game = test_game(6, 1, 6);
        game.entities[0].set_ignore(0x0C);
        game.entities[0].animation_id = 6;
        game.entities[0].attack_anim = 6;
        game.entities[0].health = 100;
        game.entities[0].angle = 0;
        game.entities[0].pos = [0, 0, 0];
        game.entities[2].id = 0x0C;
        game.entities[2].set_active(true);
        game.entities[2].angle = 0;
        game.player_attacker = Some(2);
        let mut player = test_player();
        run(&mut game, &mut player, &emd, &[], &[]);
        assert_eq!(game.entities[0].action_state, 1);
        assert_eq!(game.entities[0].is_being_attacked, 1);
        assert_eq!(game.entities[0].unk_8c, 3);
        assert_eq!(
            game.entities[0].move_speed_current,
            500 - 0x1E,
            "the entry tick also decays by its frame count"
        );
        for _ in 0..200 {
            run(&mut game, &mut player, &emd, &[], &[]);
            if game.entities[0].state() == 1 {
                break;
            }
        }
        assert_eq!(game.entities[0].state(), 1, "the exit hands control back");
        assert_eq!(game.entities[0].is_being_attacked, 0);
        assert_eq!(game.entities[0].action_behavior, 0);
    }

    /// The Tyrant's backhand stagger (behaviour 0) starts faster and decays
    /// harder than the swipe's.
    #[test]
    fn the_backhand_stagger_consumes_its_constants() {
        let emd = vec![clip(0x40, 1); 12];
        let mut game = test_game(6, 0, 6);
        game.entities[0].set_ignore(0x0C);
        game.entities[0].animation_id = 6;
        game.entities[0].animation_frame_id = 2;
        game.entities[0].health = 100;
        game.entities[2].id = 0x0C;
        game.entities[2].set_active(true);
        game.player_attacker = Some(2);
        let mut player = test_player();
        run(&mut game, &mut player, &emd, &[], &[]);
        assert_eq!(game.entities[0].action_state, 1);
        assert_eq!(
            game.entities[0].move_speed_current,
            600 - 0x28,
            "the entry tick decays by its frame count"
        );
        // The next tick decays again by one more frame.
        run(&mut game, &mut player, &emd, &[], &[]);
        assert_eq!(game.entities[0].move_speed_current, 600 - 0x78);
    }

    /// The Tyrant's knock-back (behaviour 2): the wall probe aborts the slide
    /// into the slam and the get-up eventually returns control.
    #[test]
    fn the_tyrant_knockback_wall_slams_and_gets_up() {
        let emd = vec![clip(0x40, 1); 12];
        let mut game = test_game(6, 2, 6);
        game.entities[0].set_ignore(0x0C);
        game.entities[0].animation_id = 6;
        game.entities[0].health = 100;
        game.entities[0].pos = [1000, 0, 1000];
        game.entities[0].saved_pos = Some([1000, 0, 1000]);
        game.entities[0].sca_radius = 422;
        game.entities[0].status_flags &= !4;
        game.entities[0].collision_flags = 0x10;
        game.entities[2].id = 0x0C;
        game.entities[2].set_active(true);
        game.entities[2].angle = 0;
        game.player_attacker = Some(2);
        let room = tyrant_walled_room();
        let mut player = test_player();
        update(&mut game, &mut player, &room, &emd, &[], &[]);
        assert_eq!(
            game.entities[0].action_state, 5,
            "the wall aborts to the slam"
        );
        assert_eq!(game.entities[0].move_speed_current, 900);
        assert_eq!(game.entities[0].is_being_attacked, 0);
        for _ in 0..600 {
            update(&mut game, &mut player, &room, &emd, &[], &[]);
            if game.entities[0].state() == 1 {
                break;
            }
        }
        assert_eq!(game.entities[0].state(), 1, "the get-up returns control");
        assert_eq!(game.entities[0].is_being_attacked, 0);
    }

    /// The knock-back's own state 4 also returns control when no wall is hit.
    #[test]
    fn the_tyrant_knockback_recovers_in_the_open() {
        let emd = vec![clip(2, 1); 12];
        let mut game = test_game(6, 2, 6);
        game.entities[0].set_ignore(0x0C);
        game.entities[0].animation_id = 6;
        game.entities[0].health = 100;
        game.entities[2].id = 0x0C;
        game.entities[2].set_active(true);
        game.player_attacker = Some(2);
        let mut player = test_player();
        for _ in 0..400 {
            run(&mut game, &mut player, &emd, &[], &[]);
            if game.entities[0].state() == 1 {
                break;
            }
        }
        assert_eq!(game.entities[0].state(), 1);
        assert_eq!(game.entities[0].flags & 2, 0, "the combat flag clears");
    }

    /// The attacked-flag table's behavior 1: the grabbed hold advances its
    /// three grab clips and cycles back to the first.
    #[test]
    fn the_grabbed_hold_advances_its_clips() {
        let emd = vec![clip(1, 1); 12];
        let mut game = test_game(6, 1, 0);
        game.entities[0].set_ignore(8);
        let mut player = test_player();
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].animation_id, 0);
        assert_eq!(
            game.entities[0].action_state, 2,
            "a one-frame clip wraps on the entry tick"
        );
        // State 2 selects the second clip and its advance wraps into state 4.
        update(
            &mut game,
            &mut player,
            &RoomState::default(),
            &emd,
            &[],
            &[],
        );
        assert_eq!(game.entities[0].action_state, 3);
        assert_eq!(game.entities[0].animation_id, 1, "the second grab clip");
        // Case 3 holds the second clip: the pose loops until the grabber
        // writes the next sub-state.
        for _ in 0..3 {
            update(
                &mut game,
                &mut player,
                &RoomState::default(),
                &emd,
                &[],
                &[],
            );
        }
        assert_eq!(game.entities[0].action_state, 3, "the held loop stays");
        assert_eq!(game.entities[0].animation_id, 1);
    }

    /// The state-1 entry checks: the death trigger (with the back-shot flip
    /// and the cannot-die guard), the hit pre-emption and the poison drain.
    #[test]
    fn the_control_gate_handles_death_hits_and_poison() {
        // A back-shot death plays turned around.
        let mut game = test_game(1, 0, 0);
        let mut player = test_player();
        game.entities[0].health = -1;
        game.entities[0].action_ticks_counter = 0x7FFF;
        game.entities[0].angle = 0x100;
        assert!(control_gate(&mut game, &mut player));
        assert_eq!(game.entities[0].state(), 3);
        assert_eq!(game.entities[0].action_behavior, 200);
        assert_eq!(game.entities[0].action_state, 0);
        assert_eq!(game.entities[0].is_being_attacked, 1);
        assert_eq!(game.entities[0].angle, 0x900, "the facing flipped");

        // The effect-zone flag is the cannot-die guard.
        let mut game = test_game(1, 0, 0);
        let mut player = test_player();
        game.entities[0].health = -1;
        game.flags[5].apply(MSF2_EFFECT_ZONE, 0);
        assert!(control_gate(&mut game, &mut player));
        assert_eq!(game.entities[0].health, 1);
        assert_eq!(game.entities[0].state(), 3, "the state still moves on");

        // A hit pre-empts control into the generic reaction.
        let mut game = test_game(1, 0, 0);
        let mut player = test_player();
        game.entities[0].is_being_attacked = 1;
        game.entities[0].action_state = 9;
        assert!(control_gate(&mut game, &mut player));
        assert_eq!(game.entities[0].state(), 2);
        assert_eq!(game.entities[0].action_state, 0);
        assert_eq!(game.entities[0].ignore(), 0);

        // No gate: the tick continues to the pad-driven control.
        let mut game = test_game(1, 0, 0);
        let mut player = test_player();
        assert!(!control_gate(&mut game, &mut player));

        // The poison drain: the timer is read before the decrement, and only
        // the fast 0x40 variant may drive health negative.
        let mut game = test_game(1, 0, 0);
        let mut player = test_player();
        game.entities[0].health = 100;
        game.health_status = 0x02;
        game.poison_timer = 1;
        assert!(!control_gate(&mut game, &mut player));
        assert_eq!(game.poison_timer, 0);
        assert_eq!(game.entities[0].health, 100, "not yet");
        assert!(!control_gate(&mut game, &mut player));
        assert_eq!(game.entities[0].health, 98, "the drain fires on zero");
        assert_eq!(game.poison_timer, 120, "the slow refill");
        game.health_status = 0x42;
        game.poison_timer = 0;
        game.entities[0].health = 1;
        assert!(!control_gate(&mut game, &mut player));
        assert_eq!(game.entities[0].health, -1, "the fast variant kills");
        assert_eq!(game.poison_timer, 7);
    }

    /// The death fall: the setup, the clip advance, the blood pool and the
    /// 148-frame countdown into the blocked state.
    #[test]
    fn the_death_fall_runs_the_clip_pool_and_blocked_state() {
        let emd = vec![clip(0x20, 1); 8];
        let mut game = test_game(3, 200, 0);
        let mut player = test_player();

        run(&mut game, &mut player, &emd, &[], &[]);
        assert_eq!(game.entities[0].action_state, 1);
        assert_eq!(game.entities[0].attack_anim, 4, "the fall clip");
        assert_eq!(player.anim.clip, 4);
        assert_eq!(game.entities[0].move_speed_current, 0x19, "Chris");
        assert_eq!(game.entities[0].action_ticks_counter, 0xB4);
        assert_eq!(game.entities[0].is_being_attacked, 1);

        // The 0x20-frame clip completes on the 0x20th tick.
        for _ in 1..0x20 {
            run(&mut game, &mut player, &emd, &[], &[]);
        }
        assert_eq!(game.entities[0].action_state, 2);
        assert_eq!(game.entities[0].state(), 3, "the state byte is untouched");

        // State 2 starts the blood pool and state 3.
        run(&mut game, &mut player, &emd, &[], &[]);
        assert_eq!(game.entities[0].action_state, 3);
        assert_eq!(player.shadow_tint, 0x00FF_FF50);
        assert_eq!(
            player.shadow_half_x,
            crate::shadow::PLAYER_HALF_X - 100,
            "the pool shrinks by 100"
        );

        // The countdown runs 0xB4 down to 0x20, growing the pool 16 a frame.
        let before = player.shadow_half_x;
        let mut ticks = 0;
        while game.entities[0].state() == 3 {
            run(&mut game, &mut player, &emd, &[], &[]);
            ticks += 1;
            assert!(ticks < 400, "the countdown must terminate");
        }
        assert_eq!(ticks, 0xB4 - 0x20, "148 frames");
        assert_eq!(game.entities[0].state(), 4, "the blocked state");
        assert_eq!(player.shadow_half_x, before + 0x10 * 148);
    }

    /// The death slide moves by the character's crawl speed rotated by the
    /// table's facing offset: Chris slides along his facing, Jill a quarter
    /// turn off it.
    #[test]
    fn the_death_slide_uses_the_character_offset() {
        let emd = vec![clip(0x20, 1); 8];
        let mut game = test_game(3, 200, 0);
        let mut player = test_player();
        game.entities[0].angle = 0;
        run(&mut game, &mut player, &emd, &[], &[]);
        assert_eq!(game.entities[0].move_speed_current, 0x19, "Chris' crawl");
        assert!(
            (0x18..=0x19).contains(&game.entities[0].speed[0]),
            "the crawl runs along the facing: {:?}",
            game.entities[0].speed
        );
        assert_eq!(game.entities[0].speed[2], 0, "along the facing");

        let mut game = test_game(3, 200, 0);
        game.id.player_flag = 1;
        let mut player = test_player();
        game.entities[0].angle = 0;
        run(&mut game, &mut player, &emd, &[], &[]);
        assert_eq!(game.entities[0].move_speed_current, 0xF, "Jill's crawl");
        assert_eq!(game.entities[0].speed[0], 0);
        assert!(
            (-0x10..=-0xF).contains(&game.entities[0].speed[2]),
            "a quarter turn off the facing: {:?}",
            game.entities[0].speed
        );
    }

    /// The three scripted-death rooms skip the blood pool and hand the player
    /// straight to the blocked state.
    #[test]
    fn the_scripted_death_rooms_skip_the_pool() {
        for room_text in ["205", "409", "507"] {
            let emd = vec![clip(2, 1); 8];
            let mut game = test_game(3, 200, 0);
            game.id = RoomId::parse(room_text).unwrap();
            let mut player = test_player();
            run(&mut game, &mut player, &emd, &[], &[]);
            run(&mut game, &mut player, &emd, &[], &[]);
            run(&mut game, &mut player, &emd, &[], &[]);
            assert_eq!(game.entities[0].state(), 4, "{room_text} skips the pool");
            assert_eq!(
                player.shadow_tint,
                crate::shadow::PLAYER_COLOR,
                "{room_text} keeps the shadow tint"
            );
        }
    }
}
