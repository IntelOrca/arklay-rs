//! M16 slices 7, 8, 10 and 11 real-asset audit.
//!
//! Exercises the item-search/condition opcodes, the scripted room effects and
//! player ops, and the transition-following harness against the shipped room
//! corpus: every room simulates 300 ticks with zero placeholder hits for the
//! newly implemented opcodes, no *new* placeholder arm is reached anywhere
//! (only the one documented leftover), and the rooms whose scripts walk the
//! player through a door list their destinations. A companion test drives a
//! shipped door through the real transition-follow path, since no room walks
//! itself through a door under the audit's idle/action drive.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test m16_game_real -- --ignored`

mod common;

use std::collections::BTreeSet;

use arklay::engine::{simulate_room, simulate_room_prepared, simulate_room_with_input};
use arklay::pack::Pack;
use arklay::player;
use arklay::scd;
use arklay::state::RoomId;

/// The opcodes slices 7/8 implement: item searches and conditions (`0x10`,
/// `0x11`, `0x1A`, `0x22`, `0x38`, `0x3F`), the scripted room effects
/// (`0x1C`, `0x3A`, `0x46`) and the player/room ops (`0x2B`, `0x2D`, `0x33`).
/// Every entry must appear in the shipped scripts or its audit would be
/// vacuous.
const SLICE_OPCODES: [u8; 12] = [
    0x10, // testitem
    0x11, // testpickup
    0x1A, // item_ck
    0x1C, // timer_setup
    0x22, // ck_item_count
    0x2B, // plw_anim
    0x2D, // give_item
    0x33, // sys_multi
    0x38, // ck_bits
    0x3A, // msgnode_set
    0x3F, // ck_tween (player direction)
    0x46, // msg_list
];

/// The only placeholder arms the shipped corpus still dispatches, intentional
/// and named in `docs/m16-deviations.md`: `0x05` with an out-of-range flag
/// bank. Any other op reaching a placeholder arm fails the audit.
const KNOWN_PLACEHOLDERS: [u8; 1] = [0x05];

/// Every opcode byte that appears in one RDT's init, main or event scripts.
fn script_opcodes(bytes: &[u8]) -> BTreeSet<u8> {
    let Ok(scripts) = scd::reader::parse(bytes) else {
        return Default::default();
    };
    scripts
        .init
        .iter()
        .chain(&scripts.main)
        .flat_map(|block| block.insns.iter())
        .chain(scripts.events.iter().flat_map(|stream| stream.insns.iter()))
        .map(|insn| insn.op)
        .collect()
}

/// Inventory the pack's room entries.
fn room_ids(pack: &Pack) -> Vec<RoomId> {
    let mut ids: Vec<RoomId> = pack
        .paths()
        .filter(|path| path.starts_with("room/") && path.ends_with(".rdt"))
        .filter_map(|path| RoomId::parse(&path[5..9]).ok())
        .collect();
    ids.sort_by_key(|id| (id.stage, id.room, id.player_flag));
    ids.dedup_by_key(|id| (id.stage, id.room, id.player_flag));
    ids
}

fn slice_placeholder_hits(placeholders: &std::collections::BTreeMap<u8, u64>) -> Vec<(u8, u64)> {
    SLICE_OPCODES
        .iter()
        .filter_map(|op| {
            placeholders
                .get(op)
                .copied()
                .filter(|count| *count > 0)
                .map(|count| (*op, count))
        })
        .collect()
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_m16_game_corpus_has_no_slice_placeholders_and_lists_transitions() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let ids = room_ids(&pack);
    assert!(ids.len() > 300, "expected the shipped room corpus");

    let mut simulated = 0usize;
    let mut corpus_ops: BTreeSet<u8> = Default::default();
    let mut transitions: Vec<(RoomId, RoomId)> = Vec::new();
    let mut corpus_placeholders: std::collections::BTreeMap<u8, u64> = Default::default();
    for id in &ids {
        let Ok(sim) = simulate_room(&pack, *id, 300, player::Input::default()) else {
            // Stub rooms without camera cuts cannot load.
            continue;
        };
        simulated += 1;
        if let Some(destination) = sim.transitions.last() {
            transitions.push((*id, *destination));
        }
        if let Ok(bytes) = pack.read(&id.rdt_entry()) {
            corpus_ops.extend(script_opcodes(bytes));
        }
        for (&op, &count) in &sim.game.placeholders {
            *corpus_placeholders.entry(op).or_default() += count;
        }
        let hits = slice_placeholder_hits(&sim.game.placeholders);
        assert!(
            hits.is_empty(),
            "ROOM{id:?} dispatched slice placeholders on the idle pass: {hits:?}"
        );

        // Action-only entries live behind a 0x80 action-key probe: walk the
        // room while pressing action periodically so those scripts run too.
        let pressed = simulate_room_with_input(&pack, *id, 300, |tick| player::Input {
            up: true,
            action_pressed: tick % 15 == 0,
            ..player::Input::default()
        })
        .expect("the action-drive pass runs wherever the idle pass did");
        if let Some(destination) = pressed.transitions.last() {
            let entry = (*id, *destination);
            if !transitions.contains(&entry) {
                transitions.push(entry);
            }
        }
        for (&op, &count) in &pressed.game.placeholders {
            *corpus_placeholders.entry(op).or_default() += count;
        }
        let hits = slice_placeholder_hits(&pressed.game.placeholders);
        assert!(
            hits.is_empty(),
            "ROOM{id:?} dispatched slice placeholders on the action pass: {hits:?}"
        );
    }

    for op in SLICE_OPCODES {
        assert!(
            corpus_ops.contains(&op),
            "implemented opcode {op:#04x} never appears in a shipped script"
        );
    }

    transitions.sort_by_key(|(from, to)| (from.stage, from.room, to.room));
    let placeholders: Vec<(u8, u64)> = corpus_placeholders.iter().map(|(&o, &c)| (o, c)).collect();
    let new_placeholders: Vec<(u8, u64)> = corpus_placeholders
        .iter()
        .filter(|(op, _)| !KNOWN_PLACEHOLDERS.contains(op))
        .map(|(&op, &count)| (op, count))
        .collect();
    println!(
        "m16 corpus: {simulated} rooms simulated, zero slice placeholders; \
         rooms whose captures now transition: {transitions:?}"
    );
    println!("m16 corpus placeholders: {placeholders:02x?}");
    assert!(
        new_placeholders.is_empty(),
        "the corpus dispatched new placeholder ops: {new_placeholders:02x?}"
    );
    assert!(simulated > 300, "only {simulated} rooms simulated");
}

/// The transition-follow path on real data: press a shipped room's door,
/// run its `.dor` timeline to completion and assert the destination room was
/// loaded, placed and rendered. The corpus audit above only *records* rooms
/// whose scripts walk the player through a door (none do under its idle/action
/// drive), so this is the real-data counterpart of the synthetic follow test.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_door_transition_reboots_the_destination_room() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();

    // ROOM1001's first door leads to 1002; its record is a plain door with no
    // key requirement.
    let source = RoomId::parse("1001").unwrap();
    let sim = arklay::engine::simulate_door(&pack, source, 0, None).unwrap();
    assert_ne!(sim.target, source, "the door must cross into another room");
    assert_eq!(sim.room.stage, sim.target.stage);
    assert_eq!(sim.room.room, sim.target.room);
    assert!(sim.frame_count > 0, "the .dor timeline ran no frame");
    assert!(
        sim.gameplay_frame.rgba.iter().any(|&byte| byte != 0),
        "the destination's first gameplay frame is blank"
    );
    // The destination is a loadable shipped room, not a placeholder.
    assert!(pack.read(&sim.target.rdt_entry()).is_ok());
}

/// The room 506 console interaction raises the two flags the engine's
/// interactive-screen dispatch consumes: the room's init sets SysFlags bit
/// `0x1e` (the computer terminal selector) and the console's `aot_set` event
/// sets scenario flag `0x20` (the interactive-screen gate) once the player
/// presses the action key inside its zone.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room506_console_raises_the_interactive_screen_gate() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("506").unwrap();
    // The console's action zone (16000, 7600, 1000, 1200): stand short of the
    // centre so the forward reach point lands inside it.
    let zone = [16000i32, 7600, 1000, 1200];
    let (dx, dz) = player::reach_offset(0);
    let start = [zone[0] + zone[2] / 2 - dx, 0, zone[1] + zone[3] / 2 - dz];
    let sim = simulate_room_prepared(
        &pack,
        id,
        &[],
        40,
        move |_game, player_state| {
            player_state.pos = start;
            player_state.angle = 0;
        },
        |tick| player::Input {
            action_pressed: tick == 1,
            ..player::Input::default()
        },
    )
    .unwrap();
    // The room's init selected the computer terminal screen...
    assert!(
        sim.game.flag_test(4, 0x1e, false),
        "ROOM506's init must set the computer screen selector"
    );
    // ...and the console event raised the interactive-screen gate.
    assert!(
        sim.game.flag_test(0, 0x20, false),
        "pressing action at the console must raise the interactive-screen gate"
    );
}

/// The ROOM310 elevator ride stops at the lower floor. Its event pairs the
/// `spd_add` shadow-reference byte with a tween that moves the transform; the
/// port once applied `spd_add` to the position too, doubling the ride so the
/// lift sailed ~5500 units past the destination floor (the "never stops"
/// report). The tween alone carries the player from the top floor (Y=10000)
/// to the lower one (~4100-4500).
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room310_elevator_ride_stops_at_the_lower_floor() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("310").unwrap();
    let sim = simulate_room(&pack, id, 240, player::Input::default()).unwrap();
    let y = sim.player.pos[1];
    assert!(
        (3000..=6000).contains(&y),
        "the elevator must stop at the lower floor, not overshoot past it: y={y}"
    );
}
