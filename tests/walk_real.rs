//! Real-asset follow/escort tests for the state-9 walk layer.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test walk_real -- --ignored`

mod common;

use arklay::audio;
use arklay::enemy::walk::{xz_distance_to, zone_path_find};
use arklay::engine::{SimulatedRoom, simulate_room_seeded};
use arklay::game::GameState;
use arklay::pack::Pack;
use arklay::player::{self, Input};
use arklay::rdt;
use arklay::sfx;
use arklay::state::RoomId;

/// `FG_SCENARIO` bit 55 and `FG_COMMON` bit 75: the lab follow scene's story
/// gates. `FG_COMMON` bit 192 stays clear so ROOM5001 also spawns Barry.
const LAB_FOLLOW_FLAGS: [(u8, u8); 2] = [(0, 55), (1, 75)];

const FOLLOW_TICKS: usize = 120;

/// The first active scripted-character entity slot.
fn character_slot(game: &GameState) -> Option<usize> {
    game.entities
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, entity)| entity.active() && (0x20..=0x2E).contains(&entity.id))
        .map(|(slot, _)| slot)
}

fn distance(sim: &SimulatedRoom, slot: usize) -> i32 {
    xz_distance_to(&sim.game.entities[slot], sim.player.pos)
}

fn walking() -> Input {
    Input {
        up: true,
        ..Input::default()
    }
}

/// Assert the lab room spawns a state-9 follower, closes the distance while
/// the player walks, and never leaves the walk-zone grid or enters a wall.
fn follow_run(name: &str) {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse(name).unwrap();
    let input = walking();

    let first = simulate_room_seeded(&pack, id, &LAB_FOLLOW_FLAGS, 1, input).unwrap();
    let slot = character_slot(&first.game).unwrap_or_else(|| panic!("{name} spawns a follower"));
    assert_eq!(
        first.game.entities[slot].state(),
        9,
        "the script sets the follow state"
    );
    let start = distance(&first, slot);

    let run = simulate_room_seeded(&pack, id, &LAB_FOLLOW_FLAGS, FOLLOW_TICKS, input).unwrap();
    let slot = character_slot(&run.game).unwrap_or_else(|| panic!("{name} keeps its follower"));
    let entity = run.game.entities[slot];
    let end = distance(&run, slot);
    assert!(
        end < start,
        "{name}: the character did not close distance ({start} -> {end})"
    );
    assert!(
        run.room
            .walk_zones
            .iter()
            .any(|zone| zone.contains(entity.pos[0], entity.pos[2])),
        "{name}: the character left the walk zones at {:?}",
        entity.pos
    );
    assert!(
        !player::position_blocked(
            &run.room,
            entity.pos,
            i32::from(entity.sca_radius),
            entity.collision_flags,
        ),
        "{name}: the character stands in a wall at {:?}",
        entity.pos
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room5000_follow_closes_distance_in_the_walk_zones() {
    follow_run("5000");
}

/// Every shipped room's zone graph is walked from every zone to every zone
/// index. The deep-chain rooms (thirteen-plus zones in a row) drove the ring
/// walk's best-path copy past the end of its scratch array, so this sweep is
/// the corpus-level guard for that path shape.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_corpus_zone_paths_never_panic() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let mut rooms = 0usize;
    let mut paths = 0usize;
    for entry in pack.entries() {
        let path = entry.path();
        if !path.starts_with("room/") || !path.ends_with(".rdt") {
            continue;
        }
        let Ok(id) = RoomId::parse(&path[5..9]) else {
            continue;
        };
        let Ok(data) = pack.read(path) else {
            continue;
        };
        let Ok(room) = rdt::parse(data, id) else {
            continue;
        };
        if room.walk_zones.is_empty() {
            continue;
        }
        rooms += 1;
        for from_index in 0..room.walk_zones.len() {
            let zone = &room.walk_zones[from_index];
            let from = [
                (i32::from(zone.x1) + i32::from(zone.x2)) / 2,
                0,
                (i32::from(zone.z1) + i32::from(zone.z2)) / 2,
            ];
            for to_index in 0..room.walk_zones.len().min(0x100) {
                let _ = zone_path_find(&room, from, [to_index as i32, 0, 0]);
                paths += 1;
            }
        }
    }
    assert!(
        rooms >= 300,
        "only {rooms} rooms with walk zones ran; the pack looks wrong"
    );
    assert!(paths > 0, "no zone path was walked");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room5001_follow_closes_distance_in_the_walk_zones() {
    follow_run("5001");
}

/// The first tick that queues a character footstep, found by binary search
/// over the cumulative sound queue (the simulation is deterministic, so the
/// prefix of a longer run matches a shorter one).
fn first_footstep_tick(pack: &Pack, id: RoomId) -> SimulatedRoom {
    let input = walking();
    let last = simulate_room_seeded(pack, id, &LAB_FOLLOW_FLAGS, FOLLOW_TICKS, input).unwrap();
    assert!(
        !last.game.entity_sounds.is_empty(),
        "no character footstep within {FOLLOW_TICKS} ticks"
    );

    let mut lo = 1;
    let mut hi = FOLLOW_TICKS;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let sim = simulate_room_seeded(pack, id, &LAB_FOLLOW_FLAGS, mid, input).unwrap();
        if sim.game.entity_sounds.is_empty() {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    simulate_room_seeded(pack, id, &LAB_FOLLOW_FLAGS, lo, input).unwrap()
}

/// A queued footstep name must be a shipped SE whose WAV parses from the pack.
fn assert_real_footstep(pack: &Pack, name: &str) {
    assert!(
        sfx::SE_NAMES.contains(&name),
        "{name} is not a shipped sound effect"
    );
    let path = format!("se/{}.wav", name.to_ascii_lowercase());
    let bytes = pack
        .read(&path)
        .unwrap_or_else(|err| panic!("pack has no {path}: {err}"));
    audio::parse_wav(bytes).unwrap_or_else(|err| panic!("{path} does not parse: {err:#}"));
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room5000_footstep_plays_on_the_contact_frame() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("5000").unwrap();

    // The tick the first footstep is queued is the tick the contact frame was
    // published, so the returned entity words name the contact exactly.
    let sim = first_footstep_tick(&pack, id);
    let slot = character_slot(&sim.game).expect("ROOM5000 spawns a follower");
    let entity = sim.game.entities[slot];
    let sound = *sim
        .game
        .entity_sounds
        .last()
        .expect("the contact tick queued a sound");

    let (sound_type, contacts): (u8, &[u8]) = match entity.animation_id {
        3 | 7 => (0, &[8, 0x16]),
        8 => (1, &[0, 0x0A]),
        other => panic!("unexpected walk clip {other} on the contact tick"),
    };
    assert!(
        contacts.contains(&entity.animation_frame_id),
        "clip {} frame {} is not a contact",
        entity.animation_id,
        entity.animation_frame_id
    );
    let variant = if sound_type == 0 { "A" } else { "B" };
    assert!(
        sound.name.ends_with(variant),
        "{} is not the {variant} footstep of clip {}",
        sound.name,
        entity.animation_id
    );
    assert_real_footstep(&pack, sound.name);
    assert_eq!(sound.pos, entity.pos, "the cue carries the entity position");
}
