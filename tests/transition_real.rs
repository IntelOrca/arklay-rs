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

use std::path::PathBuf;

use arklay::engine::simulate_door;
use arklay::pack::Pack;
use arklay::player::{self, Input, PlayerState};
use arklay::state::{RoomId, RoomState};

fn pack_path() -> Option<PathBuf> {
    std::env::var("ARKLAY_RE1_PACK").ok().map(PathBuf::from)
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

    // The first collision passes either leave the spawn alone (already free)
    // or push it clear; it must never stay wedged.
    let mut settled = sim.player.clone();
    step(&mut settled, &sim.room, Input::default(), 30);
    assert!(
        !player::position_blocked(&sim.room, settled.pos, settled.radius),
        "spawn {:?} is still inside a blocking collision record after 30 idle ticks",
        settled.pos
    );

    let settled_pos = settled.pos;
    step(&mut settled, &sim.room, Input::default(), 30);
    assert_eq!(
        settled.pos, settled_pos,
        "idle ticks moved a free spawn (collision is pushing)"
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
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn stairs_101_to_201_spawns_free_with_the_zone_camera() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let source = RoomId::parse("1010").unwrap();

    let dir = std::env::temp_dir().join("arklay-transition-real");
    std::fs::create_dir_all(&dir).unwrap();
    let sim = simulate_door(&pack, source, 3, Some(&dir)).unwrap();

    // ROOM1010 slot 3 is the west staircase to ROOM2010.
    assert_eq!(sim.target, RoomId::parse("2010").unwrap());
    assert_eq!(sim.player.angle, 0);
    // The record's raw arrival sits inside the stairwell collision; the
    // placement moves it clear but keeps it nearby.
    let raw = [14100, 0, 11500];
    assert!(
        player::position_blocked(&sim.room, raw, sim.player.radius),
        "the raw arrival is expected to be inside collision"
    );
    assert!(
        squared_distance(sim.player.pos, raw) <= 4096 * 4096,
        "spawn moved too far from the arrival: {:?}",
        sim.player.pos
    );
    assert!(
        !player::position_blocked(&sim.room, sim.player.pos, sim.player.radius),
        "the wedged arrival was not moved clear: {:?}",
        sim.player.pos
    );
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
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn stairs_201_to_101_spawns_free_and_walks_back_down() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let source = RoomId::parse("2010").unwrap();
    let sim = simulate_door(&pack, source, 3, None).unwrap();

    assert_eq!(sim.target, RoomId::parse("1010").unwrap());
    assert_eq!(sim.player.angle, 2048, "facing back down the stairs");
    let raw = [4300, 0, 3100];
    assert!(
        player::position_blocked(&sim.room, raw, sim.player.radius),
        "the raw arrival is expected to be inside collision"
    );
    assert!(squared_distance(sim.player.pos, raw) <= 4096 * 4096);
    // The reverse arrival is only lightly inside the stairwell volume: the
    // first collision pass clears it, so the placement stays at the record's
    // own point and the helper only requires it to settle free.
    assert_spawn_is_playable(&sim);
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
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
