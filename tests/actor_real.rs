//! Real-asset actor and state-8 tests: the scripted actor run, the native
//! scripted walk against shipped room collision, and a corpus smoke run.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test actor_real -- --ignored`

mod common;

use arklay::emd;
use arklay::enemy;
use arklay::engine::simulate_room;
use arklay::game::{BANK_SYSTEM, ScdGameHost};
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

    let model = emd::parse(pack.read("enemy/em27.emd").unwrap()).unwrap();
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
        enemy::scd::update(&mut game, slot, &sim.room, &model.clips);
        let entity = game.entities[slot];
        // State 8 has no end-of-frame resolve, so the port keeps the walk
        // layer's pre-check + rollback for this driver: the scripted walk
        // slides against walls instead of crossing them.
        assert!(
            !player::position_blocked(
                &sim.room,
                entity.pos,
                i32::from(entity.sca_radius),
                entity.collision_flags,
            ),
            "the walk crossed a collision rect at {:?}",
            entity.pos
        );
        min_distance = min_distance.min(enemy::walk::xz_distance_to(&entity, target));
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
                !player::position_blocked(&sim.room, entity.pos, radius, entity.collision_flags),
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

/// XZ distance between a player position and a scripted waypoint.
fn waypoint_distance(pos: [i32; 3], waypoint: [i32; 2]) -> i32 {
    let dx = i64::from(pos[0] - waypoint[0]);
    let dz = i64::from(pos[2] - waypoint[1]);
    ((dx * dx + dz * dz) as f64).sqrt() as i32
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room_106_jill_cutscene_steers_to_the_scripted_waypoints() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("1061").unwrap();

    // The opening cutscene first aims Jill at (20900, 8500), retargets her at
    // (24500, 12800) before she arrives, and her walk hands control back to
    // state 1 on arrival with the script's system bit 0x20 raised. The
    // completion bit lives for one frame only (the per-frame room-state reset
    // clears it after the script pass), so the arrival is sampled on every
    // tick: at the second waypoint it is raised on the completing tick.
    let first = [20900, 8500];
    let second = [24500, 12800];
    let mut first_closest = i32::MAX;
    let mut second_closest = i32::MAX;
    let mut arrival: Option<usize> = None;
    for ticks in 60..=200 {
        let Ok(sim) = simulate_room(&pack, id, ticks, player::Input::default()) else {
            return;
        };
        first_closest = first_closest.min(waypoint_distance(sim.player.pos, first));
        let distance = waypoint_distance(sim.player.pos, second);
        second_closest = second_closest.min(distance);
        if distance < 0x96
            && sim.game.entities[0].state() == 1
            && sim.game.flags[usize::from(BANK_SYSTEM)].bit(0x20)
        {
            arrival = Some(ticks);
        }
    }
    assert!(
        first_closest < 0x96,
        "the walk only closed to {first_closest} units of (20900, 8500)"
    );
    assert!(
        second_closest < 0x96,
        "the walk only closed to {second_closest} units of (24500, 12800)"
    );
    let arrival = arrival.expect("the arrival never handed control back with bit 0x20");
    let sim = simulate_room(&pack, id, arrival, player::Input::default()).unwrap();
    assert_eq!(sim.game.entities[0].state(), 1, "state 1 on arrival");
    assert_eq!(sim.game.entities[0].action_behavior, 0);
    assert!(
        sim.game.flags[usize::from(BANK_SYSTEM)].bit(0x20),
        "the script's completion bit is raised"
    );

    // The cutscene's final leg is a scripted run to (2338, 17850) that carries
    // Jill into door AOT 2's zone. The un-collided step plus the driver's
    // collision pass must keep her moving: sample the run and assert it closes
    // on the target.
    let final_target = [2338, 17850];
    let mut final_closest = i32::MAX;
    for ticks in (1100..=1200).step_by(2) {
        let Ok(sim) = simulate_room(&pack, id, ticks, player::Input::default()) else {
            return;
        };
        final_closest = final_closest.min(waypoint_distance(sim.player.pos, final_target));
    }
    assert!(
        final_closest < 0x96,
        "the final run only closed to {final_closest} units of (2338, 17850)"
    );

    // Behavior 3's deceleration hands control back with the script's system bit
    // 0x20 raised; the run's target stays latched on the entity. The hand-back
    // raises the bit in the same tick the room-state reset has already passed,
    // so the completion tick still ends with it set.
    let sim = simulate_room(&pack, id, 1178, player::Input::default()).unwrap();
    assert_eq!(
        sim.game.entities[0].state(),
        1,
        "state 1 when the run completes"
    );
    assert_eq!(sim.game.entities[0].action_behavior, 0);
    assert!(
        sim.game.flags[usize::from(BANK_SYSTEM)].bit(0x20),
        "the final run raises the script's completion bit"
    );
    let entity = sim.game.entities[0];
    assert_eq!([entity.unk_c6, entity.unk_c8], [2338, 17850]);

    // With control back, the walk-in door fires and swaps in Jill's room 105.
    // The door load resets the player animation, so no scripted state survives
    // into the destination.
    let sim = simulate_room(&pack, id, 1500, player::Input::default()).unwrap();
    assert_eq!(
        sim.transitions,
        vec![RoomId::from_room_and_player("105", 1).unwrap()],
        "door AOT 2 enters room 105"
    );
    assert_eq!(
        sim.game.entities[0].state(),
        1,
        "the door load re-initialises pad control"
    );
    assert_eq!(sim.game.entities[0].action_behavior, 0);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room_1060_chris_cutscene_drops_the_stale_walk_after_the_door() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("1060").unwrap();

    // Chris's opening cutscene ends in the same walk into door AOT 2. Unlike
    // Jill's run, the walk hands pad control back before the door fires (the
    // per-frame reset shortens the window in which the completion bit is
    // visible, so the return is sampled at its completing tick).
    let sim = simulate_room(&pack, id, 848, player::Input::default()).unwrap();
    assert_eq!(
        sim.game.entities[0].state(),
        1,
        "the walk returns pad control"
    );
    assert_eq!(sim.game.entities[0].action_behavior, 0);

    // The door load resets the player animation before the destination boots,
    // so the source room's scripted walk (target 2388,17600) is gone after the
    // transition instead of continuing into room 105's geometry.
    let sim = simulate_room(&pack, id, 1150, player::Input::default()).unwrap();
    assert_eq!(
        sim.transitions,
        vec![RoomId::from_room_and_player("105", 0).unwrap()],
        "door AOT 2 enters room 105"
    );
    assert_eq!(
        sim.game.entities[0].state(),
        1,
        "the door load re-initialises pad control"
    );
    assert_eq!(
        sim.game.entities[0].action_behavior, 0,
        "the stale scripted walk is gone after the transition"
    );
}
