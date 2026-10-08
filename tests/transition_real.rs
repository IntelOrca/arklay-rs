//! Real-asset room-transition regression tests: destination placement, camera
//! cut selection and the first gameplay frame after teardown.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test transition_real -- --ignored`
//!
//! The transitions are driven through `engine::simulate_door`, the same
//! headless seam the engine's gameplay uses: the `.dor` timeline runs to
//! completion, the destination is swapped in and the player is placed. After
//! teardown the player is ticked against the destination room with no clips so
//! only locomotion and collision run.

mod common;

use std::path::PathBuf;

use arklay::engine::simulate_door;
use arklay::pack::Pack;
use arklay::player::{self, Input, PlayerState};
use arklay::state::{RoomId, RoomState};

fn pack_path() -> Option<PathBuf> {
    Some(common::asset_env()?.1)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// Tick `player` with `input`, ignoring the animation clips.
fn step(player: &mut PlayerState, room: &RoomState, input: Input, ticks: usize) {
    for _ in 0..ticks {
        player::update(player, room, &[], &[], input);
    }
}

fn squared_distance(a: [i32; 3], b: [i32; 3]) -> i64 {
    let dx = i64::from(a[0] - b[0]);
    let dz = i64::from(a[2] - b[2]);
    dx * dx + dz * dz
}

/// Assert the placed spawn settles free, stays put when idle and is walkable,
/// and that the camera cut is the zone scan from cut 0 at the stored spawn.
fn assert_spawn_is_playable(sim: &arklay::engine::SimulatedDoor) {
    assert_eq!(sim.player.pos[1], 0, "spawn height");

    let zone_cut = player::camera_for_position(&sim.room, 0, sim.player.pos);
    assert_eq!(
        sim.room.current_cut, zone_cut,
        "camera cut is not the zone-derived cut"
    );
    assert_eq!(sim.game.camera.current_cut, zone_cut);

    // The first collision passes settle the spawn. A raw arrival may legally
    // stay inside a record's grown bounds (the original's wedge accept keeps
    // the circle-pushed point), so only stability is required: idle ticks must
    // not keep moving it.
    let mut settled = sim.player.clone();
    step(&mut settled, &sim.room, Input::default(), 30);
    let settled_pos = settled.pos;
    step(&mut settled, &sim.room, Input::default(), 30);
    assert_eq!(
        settled.pos, settled_pos,
        "idle ticks moved a settled spawn (collision is still pushing)"
    );

    let mut forward = settled.clone();
    step(
        &mut forward,
        &sim.room,
        Input {
            up: true,
            ..Input::default()
        },
        15,
    );
    assert!(
        squared_distance(forward.pos, settled.pos) > 200 * 200,
        "forward input did not leave the spawn area: {:?} -> {:?}",
        settled.pos,
        forward.pos
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn stairs_101_to_201_places_the_raw_spawn() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let source = RoomId::parse("1010").unwrap();

    let dir = std::env::temp_dir().join("arklay-transition-real");
    std::fs::create_dir_all(&dir).unwrap();
    let sim = simulate_door(&pack, source, 3, Some(&dir)).unwrap();

    // ROOM1010 slot 3 is the west staircase to ROOM2010. The record's raw
    // arrival is placed untouched (the original does not resolve it on load);
    // the shape-5 staircase volume no longer applies to the player, so no
    // spawn hack and no warning fire.
    assert_eq!(sim.target, RoomId::parse("2010").unwrap());
    assert_eq!(sim.player.angle, 0);
    let raw = [14100, 0, 11500];
    assert_eq!(sim.player.pos, raw, "the raw arrival must be placed as-is");

    // The first idle tick's collision pass settles the arrival: the shape-1
    // record rolls back, the shape-3 circle nudges it and the wedge check
    // keeps the pushed point.
    let mut settled = sim.player.clone();
    step(&mut settled, &sim.room, Input::default(), 1);
    assert_eq!(settled.pos, [14098, 0, 11442]);

    assert_spawn_is_playable(&sim);

    // The destination's first gameplay frame is deterministic and rendered
    // from the zone-selected cut.
    assert!(sim.gameplay_frame.rgba.iter().any(|&byte| byte != 0));
    let repeat = simulate_door(&pack, source, 3, None).unwrap();
    assert_eq!(repeat.player.pos, sim.player.pos);
    assert_eq!(repeat.room.current_cut, sim.room.current_cut);
    assert_eq!(
        fnv1a(&repeat.gameplay_frame.rgba),
        fnv1a(&sim.gameplay_frame.rgba),
        "the first gameplay frame differs between runs"
    );

    let capture = dir.join("destination_frame0.bmp");
    assert!(
        capture.is_file(),
        "no gameplay capture at {}",
        capture.display()
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn stairs_201_to_101_return_door_fires() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    // Arrive in ROOM201 through the west staircase from ROOM101.
    let mut sim = simulate_door(&pack, RoomId::parse("1010").unwrap(), 3, None).unwrap();
    assert_eq!(sim.target, RoomId::parse("2010").unwrap());

    // Turn back to face west and walk into the stairwell. The shape-1 wall
    // stops the walk short of the return zone; the 600-unit reach probe still
    // lands inside it (x 11345..13950), so the action press fires the return.
    sim.player.angle = 0x800;
    let input = Input {
        up: true,
        ..Input::default()
    };
    for _ in 0..30 {
        player::update(&mut sim.player, &sim.room, &[], &[], input);
        sim.game.sync_entity_from_player(&sim.player);
        sim.game.interact(sim.player.pos, sim.player.angle, false);
        sim.game.apply_stair_state(&mut sim.player);
    }
    let (dx, dz) = player::reach_offset(sim.player.angle);
    let probe = [sim.player.pos[0] + dx, 0, sim.player.pos[2] + dz];
    assert!(
        (11345..=13950).contains(&probe[0]) && (10300..=12800).contains(&probe[2]),
        "the reach probe {probe:?} left the return zone from {:?}",
        sim.player.pos
    );

    sim.game.sync_entity_from_player(&sim.player);
    sim.game.interact(sim.player.pos, sim.player.angle, true);
    let transition = sim
        .game
        .transition
        .expect("the return stair door did not fire");
    assert_eq!(transition.target, RoomId::parse("1010").unwrap());
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn door_101_to_103_camera_follows_the_zone_not_the_entry_byte() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let source = RoomId::parse("1010").unwrap();
    // ROOM1010 slot 2 is a plain door to ROOM1030. Its record names entry
    // camera 1, while the spawn's switch zone selects cut 3.
    let sim = simulate_door(&pack, source, 2, None).unwrap();

    assert_eq!(sim.target, RoomId::parse("1030").unwrap());
    assert_eq!(sim.player.pos, [2400, 0, 24800]);
    assert_eq!(sim.player.angle, 0);
    assert_eq!(
        player::camera_for_position(&sim.room, 0, sim.player.pos),
        3,
        "the spawn zone should select cut 3"
    );
    assert_eq!(
        sim.room.current_cut, 3,
        "the entry camera byte must not seed the destination cut"
    );
    assert_spawn_is_playable(&sim);
}
