//! Real-asset actor and state-8 tests: the scripted actor run, the native
//! scripted walk against shipped room collision, and a corpus smoke run.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test actor_real -- --ignored`

mod common;

use arklay::emd;
use arklay::engine::simulate_room;
use arklay::game::{BANK_SYSTEM, ScdGameHost};
use arklay::npc;
use arklay::pack::Pack;
use arklay::player;
use arklay::scd::ir::Scripts;
use arklay::scd::vm::EventVm;
use arklay::state::{RoomId, RoomState};

/// Parse the SCD streams of the room's pack entry.
fn room_scripts(pack: &Pack, id: RoomId) -> Scripts {
    let data = pack.read(&id.rdt_entry()).expect("room entry");
    arklay::scd::reader::parse(data).expect("parse SCD")
}

/// The union of every collision record's extents, when the room has any.
fn collision_bounds(room: &RoomState) -> Option<([i32; 2], [i32; 2])> {
    let mut records = room.collision.quadrants.iter().flatten();
    let first = records.next()?;
    let mut min_x = i32::from(first.x_min);
    let mut max_x = i32::from(first.x_max);
    let mut min_z = i32::from(first.z_min);
    let mut max_z = i32::from(first.z_max);
    for record in records {
        min_x = min_x.min(i32::from(record.x_min));
        max_x = max_x.max(i32::from(record.x_max));
        min_z = min_z.min(i32::from(record.z_min));
        max_z = max_z.max(i32::from(record.z_max));
    }
    Some(([min_x, min_z], [max_x, max_z]))
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room20d0_actor_run_completes_and_records_the_waypoints() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("20D0").unwrap();
    let sim = simulate_room(&pack, id, 1, player::Input::default()).unwrap();
    let scripts = room_scripts(&pack, id);

    // Start the room's intro event and step once: the whole actor sub-ISA of
    // the first two blocks must run in that one tick and stop at the first
    // `evt_sleep`.
    let mut game = sim.game.clone();
    let mut vm = EventVm::new(&scripts);
    vm.start(0, 0);
    {
        let mut host = ScdGameHost::new(&mut game);
        vm.step(&mut host);
    }
    assert_eq!(vm.active_slots(), 1, "the event is parked on its sleep");

    // The player's block: motion records (9900, -800, 3050) and act_anim_flags
    // poses animation 0x20, behaviour 1.
    let player = game.entities[0];
    assert_eq!(player.state(), 8);
    assert_eq!(player.action_behavior, 1);
    assert_eq!(player.animation_id, 0x20);
    assert_eq!(player.look_at_flags, 0x13);
    assert_eq!(player.target, [9900, -800, 3050]);
    assert_eq!(player.look_at_yaw_step, 0xC0, "step 0 takes the default");
    assert_eq!(player.look_at_pitch_step, 0x40);

    // Rebecca's block (`evt_work_set WK_ENEMY, 1` is slot 2): the scripted
    // look-at waypoint and its 20-unit steps are recorded.
    let rebecca = game.entities[2];
    assert_eq!(rebecca.id, 0x23);
    assert_eq!(rebecca.target, [2300, 300, 1]);
    assert_eq!(rebecca.look_at_flags, 0x72);
    assert_eq!(rebecca.look_at_yaw_step, 20);
    assert_eq!(rebecca.look_at_pitch_step, 20);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room20d0_state8_walk_reaches_the_scripted_waypoint() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("20D0").unwrap();
    let sim = simulate_room(&pack, id, 1, player::Input::default()).unwrap();
    let slot = sim
        .game
        .entities
        .iter()
        .position(|entity| entity.id == 0x27 && entity.active())
        .expect("ROOM20D0 spawns Richard");

    let model = emd::parse(pack.read("npc/27.emd").unwrap()).unwrap();
    let mut game = arklay::game::GameState::default();
    let mut richard = sim.game.entities[slot];
    // ROOM20D0's own walk script: behaviour 3, target (7380, 2440), turn step
    // 0x20 and completion parameter 0x21.
    richard.action_behavior = 3;
    richard.action_state = 0;
    richard.unk_c6 = 7380;
    richard.unk_c8 = 2440;
    richard.scd_timer = 0x20;
    richard.scd_anim_param = 0x21;
    richard.collision_flags = 0;
    game.entities[slot] = richard;
    game.entities[slot].set_active(true);

    let target = [7380, 0, 2440];
    let mut min_distance = i32::MAX;
    let mut completed = false;
    for _ in 0..600 {
        npc::scd::update(&mut game, slot, &sim.room, &model.clips);
        let entity = game.entities[slot];
        assert!(
            !player::position_blocked(&sim.room, entity.pos, i32::from(entity.sca_radius)),
            "the walk crossed a collision rect at {:?}",
            entity.pos
        );
        min_distance = min_distance.min(npc::walk::xz_distance_to(&entity, target));
        if entity.action_behavior == 0 {
            completed = true;
            break;
        }
    }
    assert!(completed, "the scripted walk never finished");
    assert!(
        min_distance < 0xFA,
        "the walk only closed to {min_distance} units of the waypoint"
    );
    assert!(
        game.flags[usize::from(BANK_SYSTEM)].bit(0x21),
        "the completion flag the event script waits on was not raised"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room20d0_state8_animation_advances_through_the_engine() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("20D0").unwrap();

    // A fresh room runs its first-visit event, which poses Richard in state 8
    // with behaviour 1; the engine's entity tick then plays his clip.
    let first = simulate_room(&pack, id, 1, player::Input::default()).unwrap();
    let slot = first
        .game
        .entities
        .iter()
        .position(|entity| entity.id == 0x27 && entity.active())
        .expect("ROOM20D0 spawns Richard");
    assert_eq!(first.game.entities[slot].state(), 8);
    assert_eq!(first.game.entities[slot].action_behavior, 1);

    let later = simulate_room(&pack, id, 30, player::Input::default()).unwrap();
    assert!(
        later.game.entity_anims[slot].display_frame() > 0,
        "the state-8 clip never advanced"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_corpus_rooms_tick_and_keep_moved_characters_in_bounds() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();

    let mut rooms = 0usize;
    let mut moved = 0usize;
    for entry in pack.entries() {
        let path = entry.path();
        if !path.starts_with("room/") || !path.ends_with(".rdt") {
            continue;
        }
        let Ok(id) = RoomId::parse(&path[5..9]) else {
            continue;
        };
        // Stub rooms (no camera cuts) cannot load; everything else must run
        // its init and 60 fixed ticks without panicking.
        let Ok(spawn) = simulate_room(&pack, id, 1, player::Input::default()) else {
            continue;
        };
        let Ok(sim) = simulate_room(&pack, id, 60, player::Input::default()) else {
            continue;
        };
        rooms += 1;
        let bounds = collision_bounds(&sim.room);
        for (slot, entity) in sim.game.entities.iter().enumerate().skip(1) {
            if !entity.active() || !(0x20..=0x2E).contains(&entity.id) {
                continue;
            }
            if entity.pos == spawn.game.entities[slot].pos {
                continue;
            }
            moved += 1;
            let radius = i32::from(entity.sca_radius);
            assert!(
                !player::position_blocked(&sim.room, entity.pos, radius),
                "{id:?} slot {slot} moved into collision at {:?}",
                entity.pos
            );
            if let Some(([min_x, min_z], [max_x, max_z])) = bounds {
                let margin = radius + 0x200;
                assert!(
                    entity.pos[0] >= min_x - margin
                        && entity.pos[0] <= max_x + margin
                        && entity.pos[2] >= min_z - margin
                        && entity.pos[2] <= max_z + margin,
                    "{id:?} slot {slot} moved out of the room bounds to {:?}",
                    entity.pos
                );
            }
        }
    }
    assert!(rooms >= 300, "only {rooms} rooms ran; the pack looks wrong");
    assert!(moved > 0, "no character moved in the whole corpus");
}
