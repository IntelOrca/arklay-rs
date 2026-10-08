//! The player's scripted-animation states.
//!
//! The original dispatches the player's per-frame update through the player's
//! state byte (`entities[0].state()` here). Only state 1 maps the pad: state 8
//! runs the scripted action behaviors, state 4 blanks the message/input flags,
//! states 5-7 are the door and climb animation windows, and states 0/2/3 are
//! the spawn init, hit reaction and death fall. [`update`] runs the states the
//! engine reaches when the locomotion state must not.
//!
//! # Documented deviations from the original
//!
//! - **Only the remapped playback behavior is complete.** Behavior 1 (the
//!   `act_anim_flags` pose) plays the scripted clip and raises the completion
//!   flag exactly like the original. Behaviors 2-9 steer the player toward
//!   scripted waypoints in the original; the scripted steering is not ported,
//!   so they advance the selected clip and raise the flag on completion
//!   without moving the player, and do not hand control back on their own.
//!   Behavior 0 plays its clip from the terminal pose.
//! - **Weapon animations are absent.** Behavior 0's per-state weapon loads
//!   are inert without the held-weapon clip tables; the scripted body frame is
//!   posed directly.
//! - **The per-frame hold flag (`unk_e0` bit 0x80) is not applied.** It only
//!   stretches a scripted playback by one tick per step; the clip timing alone
//!   drives the port's clock.

use crate::game::{BANK_SYSTEM, Entity, GameState};
use crate::model::Clip;
use crate::npc::EntityAnim;
use crate::player::{ClipSource, PlayerState};

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

/// Clip playback step of the scripted behaviors (the original's
/// `Joint_move(..., 0x200)`).
const SCRIPTED_BLEND_STEP: u16 = 0x200;

/// Clip playback step of behavior 0 (the original's `Joint_move(..., 0x400)`).
const PLAIN_BLEND_STEP: u16 = 0x400;

/// Advance the player's scripted state one tick.
///
/// The caller only calls this when the player is not running the pad-driven
/// locomotion and the message gate lets the player state machine run. The
/// direction pad stays in `player.input` for the object probe; the action edge
/// is withheld because it belongs to the skipped input state.
pub fn update(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
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
        8 => scripted_animation(game, player, clips),
        // The original's state 4 suppresses every message/input flag.
        4 => game.message_flags = 0,
        // States 2/3 (hit reaction, death) and 5/6/7 (the door and climb
        // animation windows) hold the player still: only state 1 reads the
        // pad.
        _ => {}
    }
}

/// Dispatch one state-8 tick on `action_behavior`, then publish the frame the
/// player model renders.
fn scripted_animation(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    match game.entities[0].action_behavior {
        0 => behavior_plain(game, player, clips),
        _ => behavior_scripted(game, player, clips),
    }
}

/// The scripted playback behavior (`act_anim_flags` and the scripted walks).
///
/// The action state machine mirrors the original's behavior 1: state 0 remaps
/// the incoming clip, states 1-4 advance it, and state 5 raises the script's
/// completion flag and optionally loops. The behaviors that steer the player
/// in the original cannot move it here, so they run the same clip playback and
/// signal completion instead of an arrival.
fn behavior_scripted(game: &mut GameState, player: &mut PlayerState, clips: &[Clip]) {
    let GameState {
        entities,
        entity_anims,
        flags,
        ..
    } = game;
    let entity = &mut entities[0];
    let clock = &mut entity_anims[0];
    if entity.action_state == 0 {
        // `unk_8c` is the clip's blend flag: 7, or 0 when `unk_e0` bit
        // 0x20 asks for a snap.
        entity.unk_8c = if entity.unk_e0 & 0x20 != 0 { 0 } else { 7 };
        entity.move_speed_current = 0;
        entity.animation_frame_id = 0;
        entity.unk_bf = 0;
        let clip = entity.animation_id;
        if clip < 0x3E {
            if clip < 0x10 {
                let (state_base, remapped) = SCD_ANIM_REMAP[usize::from(clip)];
                set_clip(entity, remapped);
                entity.action_state = state_base + 1;
            } else {
                entity.action_state = 3;
            }
        } else {
            entity.action_state = 4;
            set_clip(entity, clip - 0x3E);
        }
        entity.attacking_direction = 1;
        // No frame is consumed on the pitch tick; show the scripted frame.
        publish_pose(entity, clock, player);
    } else if (1..=4).contains(&entity.action_state) {
        let state = entity.action_state;
        let reverse = entity.flags & 1 != 0;
        if clock.advance(entity, clips, reverse, SCRIPTED_BLEND_STEP) {
            entity.action_state = 5;
            // The original rebases the clip when the joint-pair states finish,
            // so a flag-0x10 loop restarts from the right entry.
            match state {
                2 => set_clip(entity, entity.animation_id.wrapping_add(5)),
                4 => set_clip(entity, entity.animation_id.wrapping_add(0x3E)),
                _ => {}
            }
        }
        publish(entity, clock, player);
    } else if entity.action_state == 5 {
        flags[usize::from(BANK_SYSTEM)].apply(entity.scd_anim_param, 0);
        entity.scd_timer = 0;
        if entity.flags & 0x10 != 0 {
            entity.action_state = 0;
        }
        publish_pose(entity, clock, player);
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
            entity.unk_bf = 0;
            entity.unk_8c = 3;
            set_clip(entity, 0);
            publish_pose(entity, clock, player);
        }
        1 => {
            let reverse = entity.flags & 1 != 0;
            clock.advance(entity, clips, reverse, PLAIN_BLEND_STEP);
            publish(entity, clock, player);
        }
        2..=5 => {
            entity.action_state = 6;
            entity.unk_8c = 3;
            // `plw_anim` poses a single scripted frame; show it directly.
            publish_pose(entity, clock, player);
        }
        _ => {}
    }
}

/// Store the scripted clip in both fields the player's +0xBD byte is split
/// into: the generic entity clip id and the player's `attackAnim` name.
fn set_clip(entity: &mut Entity, clip: u8) {
    entity.animation_id = clip;
    entity.attack_anim = clip;
}

/// Publish the entity's scripted animation words to the player model's clock
/// so the renderer poses the scripted clip.
fn publish(entity: &Entity, clock: &mut EntityAnim, player: &mut PlayerState) {
    let clip = usize::from(entity.animation_id);
    if player.anim.clip != clip {
        // A script switched clips: show the scripted frame instead of the
        // previous clip's applied frame.
        clock.player.clip = clip;
        clock.player.display_frame = usize::from(entity.animation_frame_id);
    }
    clock.sync(entity);
    player.anim = clock.player.clone();
    player.clip_source = ClipSource::Emd;
}

/// Publish a scripted pose that consumed no frame: the entity words win even
/// when the clip is unchanged, because the `plw_anim` pose setter can seek
/// within the clip it already selected.
fn publish_pose(entity: &Entity, clock: &mut EntityAnim, player: &mut PlayerState) {
    let frame = usize::from(entity.animation_frame_id);
    clock.player.clip = usize::from(entity.animation_id);
    clock.player.frame = frame;
    clock.player.display_frame = frame;
    clock.player.timing = u16::from(entity.timing_control);
    clock.player.blend_counter = u16::from(entity.blend_counter);
    player.anim = clock.player.clone();
    player.clip_source = ClipSource::Emd;
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

    #[test]
    fn behavior_one_advances_the_clip_and_raises_the_flag() {
        let clips = vec![clip(3, 1); 0x21];
        let mut game = test_game(8, 1, 0x20);
        let mut player = test_player();
        // First tick: state 0 pitches the clip and publishes it to the model.
        update(&mut game, &mut player, &clips);
        assert_eq!(game.entities[0].action_state, 3);
        assert_eq!(player.anim.clip, 0x20);
        assert_eq!(player.clip_source, ClipSource::Emd);

        // Three one-tick frames: the third advance wraps the clip and parks
        // the behavior in its signal state; the next tick raises the flag.
        for _ in 0..3 {
            update(&mut game, &mut player, &clips);
        }
        assert_eq!(game.entities[0].action_state, 5);
        assert!(!game.flags[usize::from(BANK_SYSTEM)].bit(0x21));
        update(&mut game, &mut player, &clips);
        assert!(
            game.flags[usize::from(BANK_SYSTEM)].bit(0x21),
            "the scripted completion flag was raised"
        );
    }

    #[test]
    fn a_low_clip_id_is_remapped() {
        let clips = vec![clip(2, 1); 5];
        let mut game = test_game(8, 1, 5);
        let mut player = test_player();
        update(&mut game, &mut player, &clips);
        assert_eq!(game.entities[0].animation_id, 0, "pair (1, 0)");
        assert_eq!(game.entities[0].action_state, 2);
        assert_eq!(game.entities[0].attack_anim, 0);
    }

    #[test]
    fn state_four_blanks_the_message_flags() {
        let mut game = test_game(4, 0, 0);
        game.message_flags = 0xFD3F;
        let mut player = test_player();
        update(&mut game, &mut player, &[]);
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
        update(&mut game, &mut player, &clips);
        assert_eq!(game.entities[0].action_state, 6);
        assert_eq!(player.anim.keyframe_index(&clips), 0);

        // A second `plw_anim` poses frame 2 of the same clip.
        game.entities[0].action_state = 2;
        game.entities[0].animation_frame_id = 2;
        update(&mut game, &mut player, &clips);
        assert_eq!(player.anim.keyframe_index(&clips), 2);
    }

    #[test]
    fn state_zero_initialises_to_the_control_state() {
        let mut game = test_game(0, 7, 9);
        game.entities[0].animation_frame_id = 5;
        let mut player = test_player();
        update(&mut game, &mut player, &[]);
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
        update(&mut game, &mut player, &[]);
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
        update(&mut game, &mut player, &clips);
        assert_eq!(player.anim.keyframe_index(&clips), 0);
        update(&mut game, &mut player, &clips);
        assert_eq!(player.anim.keyframe_index(&clips), 0, "frame 0 is applied");
        update(&mut game, &mut player, &clips);
        assert_eq!(player.anim.keyframe_index(&clips), 2);
    }
}
