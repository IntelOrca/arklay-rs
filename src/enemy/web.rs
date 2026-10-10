//! The shared web-thread clone subsystem.
//!
//! The WebSpinner and the Black Tiger both shoot threads of web that fly at
//! the player: each shot copies the spider's whole entity record into an arena
//! outside the 30-slot enemy list, fans the copies by a quarter turn each, and
//! flies them with their own seven-state machine. The subsystem is shared
//! verbatim by both ids; the scripts drive it with `e:web_spawn(count)` and
//! `e:web_update(count)`.
//!
//! # Ownership
//!
//! [`spawn`] drops the parent's previous chain and allocates a fresh linked
//! chain in the fixed-capacity arena, storing the head on the parent entity
//! ([`crate::game::Entity::clone_head`]). A dead or re-scripted parent simply
//! stops calling [`update`], so its clones stop being ticked and stop drawing;
//! the whole arena resets with the room ([`crate::game::GameState::enter_room`]).
//!
//! # Update
//!
//! [`update`] walks the parent's chain for the count the script passes, in
//! chain order. Each clone re-tests the camera zone, runs its mini-machine
//! while monsters are unpaused, sticks to a moving player inside 400 units,
//! flies ballistically at the player and drains health on contact inside 800,
//! resolves the room collision with its inherited radius and marks itself live
//! for the render pass. The RNG draw order matches the script's platform
//! stream: spawn draws one `rand() & 3` per clone in fan order, the state-0,
//! state-2 and state-4 transitions draw in the exact order listed below.

use crate::enemy::EntityAnim;
use crate::enemy::walk;
use crate::game::{Entity, EntitySound, GameState};
use crate::state::RoomState;

/// The clone arena capacity (the original's room-data buffer fits far more,
/// but the shipped shot maxima are 156 and two chains never coexist for long).
pub const CLONE_CAP: usize = 160;

/// One web-thread clone: a full entity copy, the frozen animation snapshot it
/// was spawned with, its owner slot and the next link of its parent's chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebClone {
    /// The copied entity record. The update mutates it in place.
    pub entity: Entity,
    /// The parent's animation clock at spawn time. The update never advances
    /// it, so the displayed pose stays frozen exactly like the original's
    /// hand-rolled anim object.
    pub anim: EntityAnim,
    /// Entity slot of the spider that owns the chain.
    pub owner: u8,
    /// Next clone in the chain, or `None` at the tail.
    pub next: Option<u16>,
    /// Whether the chain was walked this tick; the render pass draws only
    /// live clones, so a parent that stops updating stops drawing.
    pub live: bool,
}

/// The clone's state word (`state | ignore << 8`).
fn clone_state(entity: &Entity) -> u8 {
    entity.state()
}

/// `ws_dist`: the shared flat Manhattan measure of the clone-to-player
/// displacement.
fn clone_distance(entity: &Entity, player_pos: [i32; 3]) -> i32 {
    walk::spider_distance(entity, player_pos)
}

/// Queue the clone's enemy-bank cue at the clone's own position (the original
/// walks the clone through the global entity pointer, so `Snd_em` uses the
/// clone's inherited variant and position).
fn play_clone_sound(game: &mut GameState, room: &RoomState, clone: &Entity, id: u8) {
    let group = (clone.variant >> 4) & 0x7;
    if let Some((name, column)) = crate::sfx::enemy_sound(room, id, group) {
        game.entity_sounds.push(EntitySound {
            name,
            bank: 2,
            column,
            pos: clone.pos,
        });
    }
}

/// Drop the chain `head` holds, walking its links back into the free list.
fn free_chain(game: &mut GameState, head: Option<u16>) {
    let mut index = head;
    while let Some(i) = index {
        let Some(clone) = game.web_clones[usize::from(i)].take() else {
            break;
        };
        index = clone.next;
    }
}

/// Spawn `count` web threads from the spider in `parent`.
///
/// The parent's previous chain is dropped first, the count is clamped to the
/// arena's free slots (logged once), and each clone is a full copy of the
/// parent with the yaw fanned by `remaining * 0x100`, state 0, and the
/// inherited sub flag set one time in four. The RNG draws happen after each
/// copy, `remaining` running from the requested count down to one, so the
/// first clone in the chain consumes the first draw. Returns the chain head.
pub fn spawn(game: &mut GameState, parent: usize, count: u8) -> Option<u16> {
    let old_head = game.entities[parent].clone_head.take();
    game.entities[parent].clone_count = 0;
    free_chain(game, old_head);

    let requested = usize::from(count);
    let free = game.web_clones.iter().filter(|slot| slot.is_none()).count();
    let spawn_count = requested.min(free).min(CLONE_CAP);
    if spawn_count < requested && !game.web_cap_logged {
        game.web_cap_logged = true;
        eprintln!("[web] clone arena full: spawning {spawn_count} of {requested} threads");
    }
    if spawn_count == 0 {
        return None;
    }

    let parent_entity = game.entities[parent];
    let parent_anim = game.entity_anims[parent].clone();
    let mut head: Option<u16> = None;
    let mut tail: Option<u16> = None;
    // The original's loop runs `remaining` from the requested count down to 1;
    // a capped spawn keeps the first `spawn_count` fan angles.
    for remaining in (requested - spawn_count + 1..=requested).rev() {
        let Some(slot) = game.web_clones.iter().position(|clone| clone.is_none()) else {
            break;
        };
        let mut entity = parent_entity;
        entity.angle = entity
            .angle
            .wrapping_add((remaining as u16).wrapping_mul(0x100));
        entity.clone_head = None;
        entity.clone_count = 0;
        entity.set_state(0);
        entity.action_state = u8::from((crate::game::platform_rand(&mut game.rand_state) & 3) == 0);

        let index = slot as u16;
        game.web_clones[slot] = Some(WebClone {
            entity,
            anim: parent_anim.clone(),
            owner: parent as u8,
            next: None,
            live: false,
        });
        match tail {
            Some(previous) => {
                if let Some(clone) = game.web_clones[usize::from(previous)].as_mut() {
                    clone.next = Some(index);
                }
            }
            None => head = Some(index),
        }
        tail = Some(index);
    }

    game.entities[parent].clone_head = head;
    game.entities[parent].clone_count = spawn_count as u8;
    head
}

/// One tick of the parent's clone chain.
///
/// `count` is the script's requested thread count; the walk stops at the
/// smaller of that and the spawned count. A spent clone (`status_flags == 0`)
/// only re-tests its camera zone. A live clone runs its mini-machine while
/// monsters are unpaused, then always resolves the room collision and is
/// marked live for the render pass.
pub fn update(game: &mut GameState, room: &RoomState, parent: usize, count: u8) {
    let spawned = game.entities[parent].clone_count;
    let walk_count = usize::from(count).min(usize::from(spawned));
    let paused = game.message_freezes_monsters();
    let mut index = game.entities[parent].clone_head;

    for _ in 0..walk_count {
        let Some(i) = index else {
            break;
        };
        let slot = usize::from(i);
        let Some(clone) = game.web_clones[slot].as_ref() else {
            break;
        };
        let next = clone.next;
        let mut entity = clone.entity;

        // The camera-zone bit gates the clone's shadow and sprite every frame.
        entity.has_enter_switch_zone = u8::from(crate::enemy::in_camera_zone(
            room,
            room.current_cut,
            entity.pos,
        ));

        if entity.status_flags == 0 {
            // Spent: the shadow queue only.
        } else if !paused {
            let player = game.entities[0];
            let distance = clone_distance(&entity, player.pos);

            if entity.reaction_timer != 0 {
                entity.reaction_timer = entity.reaction_timer.wrapping_sub(1);
            }

            // Sticking to a moving player inside 400 units: the thread dies,
            // drops a web-coloured shadow and a sticky billboard, and parks on
            // state 6.
            if distance < 400 && player.move_speed_current != 0 && clone_state(&entity) != 5 {
                entity.status_flags = 0;
                entity.shadow_offset = [0, 0, 0];
                entity.shadow_half_x = 300;
                entity.shadow_half_z = 300;
                entity.shadow_tint = 0x00FF_55DF;
                let room_effects = std::rc::Rc::clone(&game.room_effects);
                crate::effects::create_attached(
                    game,
                    &room_effects,
                    0,
                    10,
                    crate::effects::Attach::WebClone(i),
                    [0, 0, 0],
                    0,
                    0,
                );
                if entity.reaction_timer == 0 {
                    play_clone_sound(game, room, &entity, 6);
                    entity.reaction_timer = -0x2D;
                }
                entity.set_state(6);
            }

            // The clone's own seven-state machine.
            match clone_state(&entity) {
                0 => {
                    entity.action_ticks_counter =
                        crate::game::platform_rand(&mut game.rand_state) & 0x3F;
                    let d = i32::from(crate::game::platform_rand(&mut game.rand_state) & 0x3F);
                    let spin =
                        -(i32::from(crate::game::platform_rand(&mut game.rand_state) & 1)) * d;
                    entity.set_writhe_velocity(spin as i16);
                    entity.set_state(1);
                    entity.move_speed_current =
                        (crate::game::platform_rand(&mut game.rand_state) & 0x1F) + 100;
                    entity.set_room_collision(0xFF80);
                }
                1 => {
                    let player = game.entities[0];
                    let mut turn = walk::turn_toward_target(&entity, player.pos, 0x3E);
                    if player.move_speed_current != 0 && entity.action_state != 0 {
                        turn = -turn;
                    }
                    entity.angle = entity.angle.wrapping_add(entity.writhe_velocity() as u16);
                    entity.angle = entity.angle.wrapping_add(turn as u16);
                    let swing = entity.room_collision() as i16;
                    entity.angle = entity.angle.wrapping_add(swing as u16);
                    entity.set_room_collision(swing.wrapping_neg() as u16);
                    let s = entity.action_ticks_counter as i16;
                    entity.action_ticks_counter = s.wrapping_sub(1) as u16;
                    if s == 0 {
                        entity.set_state(if turn == 0 { 4 } else { 2 });
                    }
                }
                2 => {
                    entity.action_ticks_counter =
                        crate::game::platform_rand(&mut game.rand_state) & 0x1F;
                    entity.set_state(3);
                    entity.move_speed_current = 0;
                    entity.set_room_collision(0);
                }
                3 => {
                    let s = entity.action_ticks_counter as i16;
                    entity.action_ticks_counter = s.wrapping_sub(1) as u16;
                    if s == 0 {
                        entity.set_state(0);
                    }
                }
                4 => {
                    entity.move_speed_current =
                        (crate::game::platform_rand(&mut game.rand_state) & 0x1F) + 0x8C;
                    entity.set_state(5);
                    entity.death_timer = 0;
                    entity.set_writhe_velocity(
                        ((crate::game::platform_rand(&mut game.rand_state) & 0x7F) + 100) as i16,
                    );
                    entity.set_tint_flashes(0);
                    entity.set_room_collision(0);
                }
                5 => {
                    let fwd = entity.move_speed_current as i16;
                    let vy0 = entity.writhe_velocity();
                    let arc = walk::entity_ballistic_step(&mut entity, fwd, vy0, -30, 0);
                    if arc != 0 {
                        entity.set_state(2);
                    }
                    if distance < 800 && entity.tint_flashes() == 0 {
                        entity.angle = entity.angle.wrapping_add(0x400);
                        entity.set_tint_flashes(1);
                        let room_effects = std::rc::Rc::clone(&game.room_effects);
                        crate::effects::create_attached(
                            game,
                            &room_effects,
                            0,
                            0,
                            crate::effects::Attach::WebClone(i),
                            [0, 0, 0],
                            0,
                            0,
                        );
                        entity.move_speed_current =
                            (entity.move_speed_current as i16).wrapping_sub(0x3C) as u16;
                        let player = &mut game.entities[0];
                        let damage = if game.flags[usize::from(crate::game::BANK_SCENARIO)]
                            .bit(crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH)
                        {
                            3
                        } else {
                            2
                        };
                        player.health = player.health.wrapping_sub(damage);
                        if player.health < 0 {
                            player.health = 1;
                        }
                        player.action_behavior = 100;
                        player.is_being_attacked = 1;
                    }
                }
                _ => {}
            }

            // The swing offset and forward step carry the clone; a dangle
            // (state 6) stops moving.
            if clone_state(&entity) < 6 {
                let offset = entity.room_collision();
                let step = entity.move_speed_current as i16;
                walk::advance_xz(&mut entity, offset, step);
            }
        }

        walk::check_room_collision(room, &mut entity);

        if let Some(clone) = game.web_clones[slot].as_mut() {
            clone.entity = entity;
            clone.live = true;
        }
        index = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Zone;

    fn room() -> RoomState {
        RoomState {
            stage: 1,
            room: 1,
            zones: vec![Zone {
                cam_from: 0,
                cam_to: 0,
                corners: [[0, 0], [0, 5000], [5000, 5000], [5000, 0]],
            }],
            ..RoomState::default()
        }
    }

    fn game_with_parent() -> GameState {
        use crate::effects::fixtures::{block, sprite};
        let mut game = GameState::default();
        game.weapon_effects.sprites.push(sprite(
            0,
            std::array::from_fn(|_| vec![vec![block(1, 0, 0)]]),
        ));
        let entity = &mut game.entities[1];
        entity.id = 0x03;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.angle = 0;
        entity.pos = [1000, 0, 1000];
        entity.saved_pos = Some(entity.pos);
        game.enemy_count = 1;
        game
    }

    #[test]
    fn spawn_fans_draws_and_links_in_order() {
        let mut game = game_with_parent();
        game.rand_state = 3;
        let mut expected = 3u32;
        let draws: Vec<u16> = (0..3)
            .map(|_| crate::game::platform_rand(&mut expected) & 3)
            .collect();
        let head = spawn(&mut game, 1, 3).expect("a chain");
        assert_eq!(game.rand_state, expected, "one draw per clone");
        assert_eq!(game.entities[1].clone_count, 3);
        assert_eq!(game.entities[1].clone_head, Some(head));

        let mut index = Some(head);
        let mut angles = Vec::new();
        let mut states = Vec::new();
        while let Some(i) = index {
            let clone = game.web_clones[usize::from(i)].as_ref().unwrap();
            assert_eq!(clone.owner, 1);
            assert!(!clone.live, "the render pass marks clones live per tick");
            angles.push(clone.entity.angle);
            states.push(clone.entity.action_state);
            index = clone.next;
        }
        assert_eq!(angles, vec![0x300, 0x200, 0x100]);
        assert_eq!(
            states,
            draws
                .iter()
                .map(|draw| u8::from(draw & 3 == 0))
                .collect::<Vec<u8>>()
        );
    }

    #[test]
    fn respawn_replaces_the_old_chain() {
        let mut game = game_with_parent();
        let head = spawn(&mut game, 1, 2).unwrap();
        let old_next = game.web_clones[usize::from(head)].as_ref().unwrap().next;
        assert!(old_next.is_some());
        let head = spawn(&mut game, 1, 1).unwrap();
        assert_eq!(game.entities[1].clone_count, 1);
        assert_eq!(
            game.web_clones[usize::from(head)].as_ref().unwrap().next,
            None
        );
        let live = game.web_clones.iter().flatten().count();
        assert_eq!(live, 1, "the old chain was freed");
    }

    #[test]
    fn a_spent_clone_skips_the_mini_machine() {
        let mut game = game_with_parent();
        let head = spawn(&mut game, 1, 1).unwrap();
        let slot = usize::from(head);
        {
            let clone = game.web_clones[slot].as_mut().unwrap();
            clone.entity.status_flags = 0;
            clone.entity.set_state(3);
            clone.entity.action_ticks_counter = 5;
        }
        update(&mut game, &room(), 1, 1);
        let clone = game.web_clones[slot].as_ref().unwrap();
        assert_eq!(
            clone.entity.action_ticks_counter, 5,
            "no AI on a spent clone"
        );
        assert_eq!(clone.entity.state(), 3);
        assert!(clone.live, "spent clones still draw");
    }

    #[test]
    fn a_clone_sticks_to_a_moving_player() {
        let mut game = game_with_parent();
        game.entities[0].pos = [1000, 0, 1050];
        game.entities[0].move_speed_current = 5;
        let head = spawn(&mut game, 1, 1).unwrap();
        let slot = usize::from(head);
        let before = game.entities[0].pos;
        update(&mut game, &room(), 1, 1);
        let clone = game.web_clones[slot].as_ref().unwrap();
        assert_eq!(clone.entity.status_flags, 0, "the thread is spent");
        assert_eq!(clone.entity.state(), 6);
        assert_eq!(clone.entity.shadow_half_x, 300);
        assert_eq!(clone.entity.shadow_tint, 0x00FF_55DF);
        assert_eq!(clone.entity.reaction_timer, -0x2D, "the sticky sound timer");
        assert_eq!(game.entities[0].pos, before, "sticking deals no damage");
        assert!(clone.live);
        assert_eq!(clone.entity.has_enter_switch_zone, 1);
    }

    #[test]
    fn clone_state_zero_draws_four_values_in_order() {
        let mut game = game_with_parent();
        game.entities[0].pos = [0, 0, 0];
        let head = spawn(&mut game, 1, 1).unwrap();
        let slot = usize::from(head);
        game.rand_state = 9;
        let mut expected = 9u32;
        let c4 = crate::game::platform_rand(&mut expected) & 0x3F;
        let d = crate::game::platform_rand(&mut expected) & 0x3F;
        let spin = -(i32::from(crate::game::platform_rand(&mut expected) & 1)) * i32::from(d);
        let c2 = crate::game::platform_rand(&mut expected) & 0x1F;
        update(&mut game, &room(), 1, 1);
        assert_eq!(game.rand_state, expected);
        let clone = game.web_clones[slot].as_ref().unwrap();
        assert_eq!(clone.entity.action_ticks_counter, c4);
        assert_eq!(clone.entity.writhe_velocity(), spin as i16);
        assert_eq!(clone.entity.move_speed_current, c2 + 100);
        assert_eq!(clone.entity.state(), 1);
        assert_eq!(clone.entity.room_collision(), 0xFF80, "the swing seed");
    }

    #[test]
    fn clone_state_four_draws_two_values() {
        let mut game = game_with_parent();
        game.entities[0].pos = [0, 0, 0];
        let head = spawn(&mut game, 1, 1).unwrap();
        let slot = usize::from(head);
        game.web_clones[slot].as_mut().unwrap().entity.set_state(4);
        game.rand_state = 11;
        let mut expected = 11u32;
        let c2 = crate::game::platform_rand(&mut expected) & 0x1F;
        let spin = crate::game::platform_rand(&mut expected) & 0x7F;
        update(&mut game, &room(), 1, 1);
        assert_eq!(game.rand_state, expected);
        let clone = game.web_clones[slot].as_ref().unwrap();
        assert_eq!(clone.entity.move_speed_current, c2 + 0x8C);
        assert_eq!(clone.entity.writhe_velocity(), (spin + 100) as i16);
        assert_eq!(clone.entity.death_timer, 0);
        assert_eq!(clone.entity.state(), 5);
    }

    #[test]
    fn clone_ballistic_contact_drains_the_player() {
        let mut game = game_with_parent();
        game.entities[0].pos = [1000, 0, 1500];
        let head = spawn(&mut game, 1, 1).unwrap();
        let slot = usize::from(head);
        {
            let clone = game.web_clones[slot].as_mut().unwrap();
            clone.entity.set_state(5);
            clone.entity.move_speed_current = 100;
            clone.entity.set_writhe_velocity(50);
            clone.entity.pos = [1000, -50, 1000];
            clone.entity.saved_pos = Some(clone.entity.pos);
        }
        game.entities[0].health = 100;
        update(&mut game, &room(), 1, 1);
        let clone = game.web_clones[slot].as_ref().unwrap();
        assert_eq!(clone.entity.tint_flashes(), 1, "the contact latch");
        assert_eq!(
            clone.entity.angle, 0x500,
            "the fan plus the half-turn contact spin"
        );
        assert_eq!(clone.entity.move_speed_current, 100 - 0x3C);
        assert_eq!(game.entities[0].health, 98);
        assert_eq!(game.entities[0].action_behavior, 100);
        assert_eq!(game.entities[0].is_being_attacked, 1);
    }

    #[test]
    fn enter_room_clears_the_arena_and_the_registry() {
        let mut game = game_with_parent();
        spawn(&mut game, 1, 4);
        game.web_joint_registry[0][0] = 5;
        let id = crate::state::RoomId::parse("101").unwrap();
        game.enter_room(id, &room());
        assert!(game.web_clones.iter().all(Option::is_none));
        assert_eq!(game.entities[1].clone_head, None);
        assert_eq!(game.entities[1].clone_count, 0);
        assert_eq!(game.web_joint_registry, [[0; 8]; 2]);
    }

    #[test]
    fn tick_entities_clears_the_live_marks() {
        let mut game = game_with_parent();
        let head = spawn(&mut game, 1, 1).unwrap();
        let slot = usize::from(head);
        update(&mut game, &room(), 1, 1);
        assert!(game.web_clones[slot].as_ref().unwrap().live);
        let mut models = crate::enemy::EntityModelCache::default();
        let pack =
            crate::pack::Pack::from_bytes(crate::pack::PackWriter::new().to_bytes().unwrap())
                .unwrap();
        game.tick_entities(&room(), &mut models, &pack, None);
        assert!(!game.web_clones[slot].as_ref().unwrap().live);
    }
}
