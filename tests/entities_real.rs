//! Real-asset entity spawn tests: the `enemy` record against shipped rooms.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test entities_real -- --ignored`

mod common;

use arklay::game::{BANK_ENEMIES, Entity, GameState, ScdGameHost};
use arklay::scd::host::ScdHost;
use arklay::scd::ir::Scripts;
use arklay::scd::opcode::command_op;
use arklay::scd::vm::CommandVm;
use arklay::state::{RoomId, RoomState};

/// Load `ROOM<name>.RDT` from the JPN install, run its init script and return
/// the parsed room, scripts and state.
fn load_room(root: &std::path::Path, name: &str) -> (RoomState, Scripts, GameState) {
    let id = RoomId::parse(name).expect("valid room id");
    let path = root
        .join(format!("JPN/STAGE{}", id.stage))
        .join(format!("ROOM{:04X}.RDT", id.rdt_number()));
    let data = std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let room = arklay::rdt::parse(&data, id).expect("parse RDT");
    let scripts = arklay::scd::reader::parse(&data).expect("parse scripts");
    let mut state = GameState::new(id, &room);
    run_init(&scripts, &mut state);
    (room, scripts, state)
}

fn run_init(scripts: &Scripts, state: &mut GameState) {
    let mut vm = CommandVm::new(scripts);
    let mut host = ScdGameHost::new(state);
    vm.run_init(&mut host);
}

fn active_rebecca(state: &GameState) -> Option<&Entity> {
    state.entities[1..]
        .iter()
        .find(|entity| entity.id == 0x23 && entity.active())
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1000_init_spawns_rebecca_and_the_guard_skips_the_record() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let (_room, scripts, mut state) = load_room(&root, "1000");

    let entity = active_rebecca(&state).expect("ROOM1000 init spawns Rebecca");
    assert_eq!(entity.pos, [7280, 0, 3920]);
    assert_eq!(entity.behavior_flags, 6);
    assert_eq!(entity.animation_id, 0x10);
    assert_eq!(entity.animation_frame_id, 0x0C);
    assert_eq!(entity.timing_control, 1);
    assert_eq!(state.enemy_count, 1);

    // The shipped record carries no guard (0xFF). Take its operands, arm a
    // guard bit in bank 3 and re-dispatch: the record must be skipped.
    let record = scripts
        .init
        .iter()
        .flat_map(|block| &block.insns)
        .find(|insn| insn.op == 0x1B && insn.operands[0].value == 0x23)
        .expect("the init script spawns Rebecca")
        .operands
        .clone();
    assert_eq!(record[2].value, 0xFF, "the shipped guard is disabled");

    let mut guarded = record.clone();
    guarded[2].value = 5;
    let slot = 1 + usize::from(guarded[11].value as u8 & 0x0F);

    // Without the guard the record re-initialises the occupied slot, because
    // its force-init byte is set.
    {
        let mut host = ScdGameHost::new(&mut state);
        host.state_mut().entities[slot].pos = [1, 2, 3];
        host.on_enemy(command_op(0x1B).unwrap(), &guarded);
    }
    assert_eq!(
        state.entities[slot].pos,
        [7280, 0, 3920],
        "force-init re-runs the record"
    );

    // With the guard bit set the whole record is skipped.
    state.flags[usize::from(BANK_ENEMIES)].apply(5, 0);
    state.entities[slot].pos = [1, 2, 3];
    let before = state.enemy_count;
    {
        let mut host = ScdGameHost::new(&mut state);
        host.on_enemy(command_op(0x1B).unwrap(), &guarded);
    }
    assert_eq!(
        state.entities[slot].pos,
        [1, 2, 3],
        "guard skips the record"
    );
    assert_eq!(state.enemy_count, before);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_monster_spawns_allocate_nothing() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    // ROOM1010's init spawns three zombies (id 0x00) with guard bits 0x09,
    // 0x0A and 0x0B; every one must parse but leave its slot free.
    let (_room, _scripts, state) = load_room(&root, "1010");
    assert_eq!(state.enemy_count, 0, "monster ids allocate no entity");
    assert!(
        state.entities[1..].iter().all(|entity| !entity.active()),
        "monster ids leave every enemy slot free"
    );
}
