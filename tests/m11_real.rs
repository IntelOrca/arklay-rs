//! M11 integration pass: the corpus reachability audit for the implemented
//! world-interaction opcode set and the deterministic `--ticks` captures.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test m11_real -- --ignored`

mod common;

use std::path::Path;
use std::process::Command;

use arklay::engine::{simulate_room, simulate_room_seeded, simulate_room_with_input};
use arklay::pack::Pack;
use arklay::player;
use arklay::scd;
use arklay::state::{RoomId, RoomState};

/// The opcodes M11 implements. A placeholder hit for any of these would mean a
/// shipped script reached an opcode the milestone claims to handle, and every
/// entry must appear in the shipped scripts or the check would be vacuous.
const IMPLEMENTED_OPCODES: [u8; 13] = [
    0x0C, // set_stairs_zone
    0x0F, // scene_setup
    0x11, // stairs_height_update
    0x1F, // obj
    0x30, // inst_cfg
    0x34, // model_op
    0x35, // objtbl_b_set
    0x36, // ck_anim
    0x3B, // eml_rot
    0x3C, // ck_counter
    0x40, // obj_xfm
    0x47, // eml_pos
    0x4D, // objs_hide
];

/// Every opcode byte that appears in one RDT's init, main or event scripts.
fn script_opcodes(bytes: &[u8]) -> std::collections::BTreeSet<u8> {
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

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_m11_corpus_audit_has_no_implemented_placeholders() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let ids = room_ids(&pack);
    assert!(ids.len() > 300, "expected the shipped room corpus");

    let mut simulated = 0usize;
    let mut object_rooms = 0usize;
    let mut animation_rooms = 0usize;
    let mut corpus_ops: std::collections::BTreeSet<u8> = Default::default();
    for id in &ids {
        let Ok(sim) = simulate_room(&pack, *id, 300, player::Input::default()) else {
            // Stub rooms without camera cuts cannot load.
            continue;
        };
        simulated += 1;
        if sim.game.objects.built > 0 {
            object_rooms += 1;
        }
        if sim.room.room_anim.is_some() {
            animation_rooms += 1;
        }
        if let Ok(bytes) = pack.read(&id.rdt_entry()) {
            corpus_ops.extend(script_opcodes(bytes));
        }
        let hits: Vec<(u8, u64)> = IMPLEMENTED_OPCODES
            .iter()
            .filter_map(|op| {
                sim.game
                    .placeholders
                    .get(op)
                    .copied()
                    .map(|count| (*op, count))
            })
            .filter(|(_, count)| *count > 0)
            .collect();
        assert!(
            hits.is_empty(),
            "ROOM{id:?} dispatched implemented placeholders: {hits:?}"
        );

        // Action-only handlers live behind a 0x80 action-key probe; walk the
        // room while pressing action periodically so those scripts run too.
        let pressed = simulate_room_with_input(&pack, *id, 300, |tick| player::Input {
            up: true,
            action_pressed: tick % 15 == 0,
            ..player::Input::default()
        })
        .expect("the action-drive pass runs wherever the idle pass did");
        let hits: Vec<(u8, u64)> = IMPLEMENTED_OPCODES
            .iter()
            .filter_map(|op| {
                pressed
                    .game
                    .placeholders
                    .get(op)
                    .copied()
                    .map(|count| (*op, count))
            })
            .filter(|(_, count)| *count > 0)
            .collect();
        assert!(
            hits.is_empty(),
            "ROOM{id:?} dispatched implemented placeholders on the action pass: {hits:?}"
        );

        // An `obj` operand is zero-extended on X/Z, so a built record must
        // stay inside the 16-bit room coordinate space; the collision margin
        // then catches wild placements while allowing the shipped decorative
        // records that legitimately sit past the last boundary (ROOM113).
        let bounds = collision_bounds(&sim.room);
        for (slot, record) in sim.game.objects.records.iter().enumerate() {
            if !record.active() {
                continue;
            }
            assert!(
                (0..=0xFFFF).contains(&record.pos[0]) && (0..=0xFFFF).contains(&record.pos[2]),
                "ROOM{id:?} object {slot} at {:?} left the 16-bit room space",
                record.pos
            );
            if let Some(([min_x, min_z], [max_x, max_z])) = bounds {
                let margin = 0x4000;
                assert!(
                    record.pos[0] >= min_x - margin
                        && record.pos[0] <= max_x + margin
                        && record.pos[2] >= min_z - margin
                        && record.pos[2] <= max_z + margin,
                    "ROOM{id:?} object {slot} at {:?} is far outside the room",
                    record.pos
                );
            }
        }
    }

    println!(
        "corpus: {simulated} rooms simulated, {object_rooms} built objects, \
         {animation_rooms} carry a room animation pair, zero implemented placeholders"
    );
    assert!(simulated > 300, "only {simulated} rooms simulated");
    assert!(
        object_rooms > 100,
        "only {object_rooms} rooms built objects"
    );
    assert!(
        animation_rooms > 300,
        "only {animation_rooms} rooms carry the player-animation pair"
    );

    // Every implemented opcode must really appear in the shipped scripts; an
    // entry no room ever decodes would make its placeholder check vacuous.
    for op in IMPLEMENTED_OPCODES {
        assert!(
            corpus_ops.contains(&op),
            "implemented opcode {op:#04x} never appears in a shipped script; \
             remove it from IMPLEMENTED_OPCODES"
        );
    }

    // A room change clears the mirror the outgoing room armed (ROOM112
    // enables it; the destination must not inherit the pass or its geometry).
    let mirrored = simulate_room(
        &pack,
        RoomId::parse("1120").unwrap(),
        1,
        player::Input::default(),
    )
    .expect("ROOM112 loads");
    assert!(mirrored.game.mirror_enabled(), "ROOM112 arms the mirror");
    let mut carried = mirrored.game;
    carried.enter_room(RoomId::parse("1140").unwrap(), &RoomState::default());
    assert!(
        !carried.mirror_enabled() && carried.mirror.plane == 0,
        "the mirror leaked through the room change"
    );

    // Flag 47 enables ROOM3010's ladder zones; the audit must still run clean.
    let id = RoomId::parse("3010").unwrap();
    let seeded = simulate_room_seeded(&pack, id, &[(0, 47)], 120, player::Input::default())
        .expect("ROOM3010 runs with the lab powered");
    assert!(
        seeded
            .game
            .room_actions
            .iter()
            .flatten()
            .any(|action| { action.kind == arklay::game::RoomActionKind::StairsZone })
    );
    for op in IMPLEMENTED_OPCODES {
        assert!(
            seeded.game.placeholders.get(&op).copied().unwrap_or(0) == 0,
            "ROOM3010 flagged dispatched placeholder op {op:#x}"
        );
    }
}

/// Run the CLI capture for one room and return the written bytes.
fn cli_capture(pack: &Path, room: &str, ticks: u32, out: &Path) -> Vec<u8> {
    let status = Command::new(env!("CARGO_BIN_EXE_arklay"))
        .arg(pack)
        .arg("--room")
        .arg(room)
        .arg("--player")
        .arg("0")
        .arg("--ticks")
        .arg(ticks.to_string())
        .arg("--capture")
        .arg(out)
        .env("SDL_AUDIODRIVER", "dummy")
        .env("SDL_VIDEODRIVER", "dummy")
        .status()
        .expect("spawn arklay");
    assert!(status.success(), "the CLI capture failed");
    std::fs::read(out).expect("read the CLI capture")
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room_301_ticks_captures_are_deterministic() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("arklay-m11-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Two CLI runs with the same arguments are byte-identical.
    let first = cli_capture(&pack_path, "301", 30, &dir.join("first.bmp"));
    let second = cli_capture(&pack_path, "301", 30, &dir.join("second.bmp"));
    assert_eq!(first, second, "two --ticks captures differ");

    // The library seam reproduces the CLI frame exactly.
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("3010").unwrap();
    let captured = arklay::bmp::decode(&first).unwrap();
    let sim = simulate_room(&pack, id, 30, player::Input::default()).unwrap();
    assert_eq!(
        captured.rgba, sim.frame.rgba,
        "CLI capture != simulated frame"
    );
}
