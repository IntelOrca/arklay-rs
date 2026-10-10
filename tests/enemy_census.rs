//! Corpus enemy census: every monster spawn record in the shipped rooms
//! allocates a slot, and its id either runs the pack's script or parks inert,
//! with no script error anywhere in the corpus.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test enemy_census -- --ignored`

mod common;

use std::collections::{BTreeMap, BTreeSet};

use arklay::enemy::{CHARACTER_ID_MIN, LuaEnemyHost};
use arklay::game::{ENTITY_COUNT, GameState, ScdGameHost};
use arklay::pack::Pack;
use arklay::scd;
use arklay::scd::vm::CommandVm;
use arklay::state::RoomId;

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn corpus_monster_records_run_or_park_without_script_errors() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let mut host = LuaEnemyHost::new();
    let mut scripted: BTreeMap<u8, usize> = BTreeMap::new();
    let mut parked: BTreeSet<u8> = BTreeSet::new();
    let mut eml_subs: BTreeSet<u8> = BTreeSet::new();
    let mut rooms = 0usize;
    let mut records = 0usize;

    for entry in pack.entries() {
        let path = entry.path();
        let Some(name) = path
            .strip_prefix("room/")
            .and_then(|p| p.strip_suffix(".rdt"))
        else {
            continue;
        };
        let Ok(id) = RoomId::parse(name) else {
            continue;
        };
        let Ok(data) = pack.read(path) else {
            continue;
        };
        // The stub rooms without camera cuts are skipped, like the soak.
        let Ok(room) = arklay::rdt::parse(data, id) else {
            continue;
        };
        let Ok(scripts) = scd::reader::parse(data) else {
            continue;
        };
        // Every `eml_state` (0x28) sub-command every corpus stream uses. The
        // reader only decodes a known sub-command's width, so this is also
        // the gate that keeps a new sub-command from silently truncating a
        // room's init or main stream.
        let streams = scripts
            .init
            .iter()
            .map(|block| block.insns.as_slice())
            .chain(scripts.main.iter().map(|block| block.insns.as_slice()))
            .chain(scripts.events.iter().map(|stream| stream.insns.as_slice()));
        for insns in streams {
            for insn in insns {
                if insn.op == 0x28
                    && let Some(operand) = insn.operands.get(2)
                {
                    eml_subs.insert(operand.value as u8);
                }
            }
        }
        let mut game = GameState::new(id, &room);
        {
            let mut vm = CommandVm::new(&scripts);
            let mut game_host = ScdGameHost::new(&mut game);
            vm.run_init(&mut game_host);
        }
        rooms += 1;

        let active_slots = (1..ENTITY_COUNT)
            .filter(|&slot| game.entities[slot].active())
            .count();
        assert_eq!(
            usize::from(game.enemy_count),
            active_slots,
            "room {id:?} counts {} allocations for {active_slots} live entities",
            game.enemy_count
        );

        for slot in 1..ENTITY_COUNT {
            let entity = game.entities[slot];
            if !entity.active() || entity.id >= CHARACTER_ID_MIN {
                continue;
            }
            records += 1;
            if host.update(&mut game, &room, &pack, slot, &[]) {
                *scripted.entry(entity.id).or_default() += 1;
            } else {
                parked.insert(entity.id);
            }
        }
    }

    assert!(rooms > 300, "only {rooms} rooms booted");
    assert!(
        records > 300,
        "only {records} monster records found in the room init scripts"
    );
    assert_eq!(
        scripted.keys().copied().collect::<Vec<_>>(),
        vec![
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15
        ],
        "the zombies, the hound, the WebSpinner, the Black Tiger, the crow, \
         the hunter, the wasp, Plant 42, the chimera, the adder, both Tyrants, \
         Yawn, the roots, the monster plant, the rematch Yawn, the computer \
         arms and the spider web are the scripted monster ids so far"
    );
    assert!(scripted[&0x00] > 0, "no room spawns a standard zombie");
    assert!(scripted[&0x01] > 0, "no room spawns a naked zombie");
    assert!(scripted[&0x11] > 0, "no room spawns a green zombie");
    assert!(scripted[&0x13] > 0, "no room spawns a spider web");
    assert!(scripted[&0x0e] > 0, "no room spawns Plant 42's roots");
    assert!(scripted[&0x0a] > 0, "no room spawns an adder");
    assert!(scripted[&0x07] > 0, "no room spawns a wasp");
    assert!(scripted[&0x05] > 0, "no room spawns a crow");
    assert!(scripted[&0x04] > 0, "no room spawns a Black Tiger");
    assert!(scripted[&0x03] > 0, "no room spawns a WebSpinner");
    assert!(scripted[&0x02] > 0, "no room spawns a hound");
    assert!(scripted[&0x09] > 0, "no room spawns a chimera");
    assert!(scripted[&0x06] > 0, "no room spawns a hunter");
    assert!(scripted[&0x08] > 0, "no room spawns Plant 42");
    assert!(scripted[&0x0f] > 0, "no room spawns a monster plant");
    assert!(scripted[&0x14] > 0, "no room spawns the right computer arm");
    assert!(scripted[&0x15] > 0, "no room spawns the left computer arm");
    assert!(scripted[&0x0d] > 0, "no room spawns Yawn");
    assert!(scripted[&0x12] > 0, "no room spawns the rematch Yawn");
    assert!(scripted[&0x0c] > 0, "no room spawns the lab Tyrant");
    assert!(scripted[&0x10] > 0, "no room spawns the heliport Tyrant");
    assert!(
        parked.iter().all(|id| *id <= 0x16),
        "unexpected monster ids parked: {parked:?}"
    );
    assert_eq!(
        eml_subs.into_iter().collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 8],
        "the corpus uses exactly these eml_state sub-commands; the port also \
         implements 5/6/9/10 for mods, but no shipped script reaches them"
    );
    assert_eq!(
        host.failed_scripts(),
        0,
        "a corpus spawn's script failed to load or run"
    );
}
