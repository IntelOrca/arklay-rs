//! Real-asset door regression tests.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 cargo test --test doors_real -- --ignored`
//!
//! Every test loads a shipped RDT, runs its init script through the game state
//! and drives the door interaction the way the engine does: the original tests
//! a point 600 units in front of the player, so the helper below stands the
//! player short of the zone and lets the reach point land inside it.

mod common;

use std::path::PathBuf;

use arklay::game::{Door, GameState, RoomTransition, ScdGameHost};
use arklay::player;
use arklay::scd::ir::Scripts;
use arklay::scd::vm::{CommandVm, EventVm};
use arklay::state::{RoomId, RoomState};

fn asset_root() -> Option<PathBuf> {
    Some(common::asset_env()?.0)
}

/// Load `ROOM<name>.RDT` from the JPN install, run its init script and return
/// the state plus the parsed scripts.
fn load_room(root: &std::path::Path, name: &str) -> (RoomId, RoomState, Scripts, GameState) {
    let id = RoomId::parse(name).expect("valid room id");
    let path = root
        .join(format!("JPN/STAGE{}", id.stage))
        .join(format!("ROOM{:04X}.RDT", id.rdt_number()));
    let data = std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let room = arklay::rdt::parse(&data, id).expect("parse RDT");
    let scripts = arklay::scd::reader::parse(&data).expect("parse scripts");
    let mut state = GameState::new(id, &room);
    {
        let mut vm = CommandVm::new(&scripts);
        let mut host = ScdGameHost::new(&mut state);
        vm.run_init(&mut host);
    }
    (id, room, scripts, state)
}

/// The player position whose forward reach point lands in the door's zone.
fn reach_position(door: &Door) -> [i32; 3] {
    let center_x = i32::from(door.zone[0]) + i32::from(door.zone[2]) / 2;
    let center_z = i32::from(door.zone[1]) + i32::from(door.zone[3]) / 2;
    let (dx, dz) = player::reach_offset(0);
    [center_x - dx, 0, center_z - dz]
}

/// Probe the door in `slot` with the action key held.
fn press(state: &mut GameState, slot: u8) -> Option<RoomTransition> {
    let door = state.doors[usize::from(slot)].expect("door registered");
    let pos = reach_position(&door);
    state.interact(pos, 0, true);
    state.transition.take()
}

fn assert_destination_exists(root: &std::path::Path, transition: &RoomTransition) {
    let path = root
        .join(format!("JPN/STAGE{}", transition.target.stage))
        .join(format!("ROOM{:04X}.RDT", transition.target.rdt_number()));
    assert!(
        path.is_file(),
        "transition target {} is missing",
        path.display()
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn save_room_door_survives_main_ticks_and_transitions() {
    let Some(root) = asset_root() else {
        return;
    };
    let (_, _, scripts, mut state) = load_room(&root, "1001");
    let door = state.doors[0].expect("save-room door");
    assert_eq!(door.next_room, 1);
    assert_eq!(door.sub_type, 0x81);

    // Re-running the per-frame script must not disturb the registration.
    let mut vm = CommandVm::new(&scripts);
    for _ in 0..10 {
        let mut host = ScdGameHost::new(&mut state);
        vm.run_main(&mut host);
        state.advance_frame();
    }
    assert!(state.doors[0].is_some(), "main must not drop the door");

    let transition = press(&mut state, 0).expect("door transition");
    assert_eq!(
        transition.target,
        RoomId {
            stage: 1,
            room: 1,
            player_flag: 1
        }
    );
    assert_eq!(transition.pos, [8700, 0, 7900]);
    assert_eq!(transition.angle, 1024);
    assert_destination_exists(&root, &transition);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn stairwell_doors_cross_stage_with_exact_placement() {
    let Some(root) = asset_root() else {
        return;
    };

    // ROOM1060: the stairwell door uses a stair animation (type 0x16) and
    // leads to stage 2.
    let (_, _, _, mut state) = load_room(&root, "1060");
    let stair = state.doors[4].expect("stairwell door");
    assert_eq!(stair.door_type, 0x16, "stair animation type");
    let transition = press(&mut state, 4).expect("stair transition");
    assert_eq!(
        transition.target,
        RoomId {
            stage: 2,
            room: 3,
            player_flag: 0
        }
    );
    assert_eq!(transition.pos, [17100, 0, 25300]);
    assert_eq!(transition.angle, 3072);
    assert_destination_exists(&root, &transition);

    // ROOM1010: a second stair type (0x18) into stage 2 room 1.
    let (_, _, _, mut state) = load_room(&root, "1010");
    let stair = state.doors[3].expect("stair door");
    assert_eq!(stair.door_type, 0x18);
    let transition = press(&mut state, 3).expect("stair transition");
    assert_eq!(
        transition.target,
        RoomId {
            stage: 2,
            room: 1,
            player_flag: 0
        }
    );
    assert_eq!(transition.pos, [14100, 0, 11500]);
    assert_eq!(transition.angle, 0);
    assert_destination_exists(&root, &transition);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn elevator_door_crosses_to_an_earlier_stage() {
    let Some(root) = asset_root() else {
        return;
    };
    // ROOM7010 slot 3: dest 0xC1 -> stage (0xC1 >> 5) - 1 = 5 -> stage digit 6.
    let (_, _, _, mut state) = load_room(&root, "7010");
    let elevator = state.doors[3].expect("elevator door");
    assert_eq!(elevator.next_room, 0xC1);
    let transition = press(&mut state, 3).expect("elevator transition");
    assert_eq!(
        transition.target,
        RoomId {
            stage: 6,
            room: 1,
            player_flag: 0
        }
    );
    assert_eq!(transition.pos, [4300, 0, 3100]);
    assert_eq!(transition.angle, 2048);
    assert_destination_exists(&root, &transition);

    // With the stage-variant scenario flag, dest 0x20 remaps stage 1 to the
    // return variant stage digit 6.
    let (_, _, _, mut state) = load_room(&root, "70E0");
    assert!(state.apply_flag(0, 0x00, 0));
    let transition = press(&mut state, 3).expect("variant transition");
    assert_eq!(transition.target.stage, 6);
    assert_eq!(transition.target.room, 0);
    assert_destination_exists(&root, &transition);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn key_locked_door_needs_and_consumes_its_key() {
    let Some(root) = asset_root() else {
        return;
    };
    // ROOM1010 slot 1: lock 0xCA (Chris only), key 0x34.
    let (_, _, _, mut state) = load_room(&root, "1010");
    let door = state.doors[1].expect("key door");
    assert_eq!(door.key, 0x34);
    assert_eq!(door.lock, 0xCA);

    // Without the key: the locked message and no transition.
    assert!(press(&mut state, 1).is_none());
    assert_eq!(state.message.id, Some(201));
    assert!(!state.flag_test(2, 0x0A, false));

    // A live window refuses the next prompt, so the message is read first.
    state.cancel_message();

    // With the key: the lock turns, the key is consumed and the flag is set.
    state.add_item(0x34, 1);
    assert!(
        press(&mut state, 1).is_none(),
        "key turn does not transition"
    );
    assert_eq!(state.message.id, Some(0xC3));
    assert!(!state.has_item(0x34));
    assert!(state.flag_test(2, 0x0A, false));

    // The next probe walks through.
    state.cancel_message();
    let transition = press(&mut state, 1).expect("unlocked transition");
    assert_eq!(transition.target.room, 2);
    assert_eq!(transition.pos, [3400, 0, 9200]);
    assert_eq!(transition.angle, 1024);
    assert_destination_exists(&root, &transition);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn character_restriction_bit_is_inert_for_pc_characters() {
    let Some(root) = asset_root() else {
        return;
    };
    // ROOM1031 is Jill's variant with lock 0x40 (bit 7 clear, so unlocked).
    // The original's wrong-character test compares `id & 3` against 3, which
    // is never true for Chris/Jill, so the bit does not bar her.
    let (_, _, _, mut state) = load_room(&root, "1031");
    let door = state.doors[2].expect("restricted door");
    assert_eq!(door.lock, 0x40);
    let transition = press(&mut state, 2).expect("Jill transition");
    assert_ne!(state.message.id, Some(0xD6), "the restriction is inert");
    assert_destination_exists(&root, &transition);

    // Chris's variant walks through the same way.
    let (_, _, _, mut state) = load_room(&root, "1030");
    let transition = press(&mut state, 2).expect("Chris transition");
    assert_eq!(transition.target.room, 12);
    assert_destination_exists(&root, &transition);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn other_side_key_door_unlocks_on_the_first_probe() {
    let Some(root) = asset_root() else {
        return;
    };
    // ROOM6100 slot 0: key 0xFE, lock flag 0x13.
    let (_, _, _, mut state) = load_room(&root, "6100");
    let door = state.doors[0].expect("other-side door");
    assert_eq!(door.key, 0xFE);

    assert!(press(&mut state, 0).is_none());
    assert_eq!(state.message.id, Some(0xD4));
    assert!(state.flag_test(2, 0x13, false));

    let transition = press(&mut state, 0).expect("unlocked transition");
    assert_eq!(transition.target.room, 4);
    assert_eq!(transition.pos, [31800, 0, 3300]);
    assert_eq!(transition.angle, 2048);
    assert_destination_exists(&root, &transition);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn event_payload_rearms_the_door_with_an_auto_probe() {
    let Some(root) = asset_root() else {
        return;
    };
    // ROOM4060: init registers slot 2 as a tiny action-key zone; action slot 5
    // starts event 4, which schedules event 5; that payload re-registers slot 2
    // with a large walk-in zone (probe 0x41) that leads to room 40D. The event
    // shows prompts and gates on the message byte (`cmpb 5`), so the player
    // dismissing each one is simulated by clearing the window each tick.
    let (_, _, scripts, mut state) = load_room(&root, "4060");
    let mut event_vm = EventVm::new(&scripts);
    event_vm.start(0, 4);
    for _ in 0..30 {
        {
            let mut host = ScdGameHost::new(&mut state);
            event_vm.step(&mut host);
        }
        for (slot, event) in std::mem::take(&mut state.pending_events) {
            event_vm.start(usize::from(slot), event);
        }
        state.cancel_message();
        state.advance_frame();
        if state.doors[2].is_some_and(|door| door.sub_type == 0x41) {
            break;
        }
    }

    let door = state.doors[2].expect("event-armed door");
    assert_eq!(door.sub_type, 0x41, "the event arms the auto probe");
    assert_eq!(door.next_room, 0x0D);

    // Walk in without the action key.
    let center = [
        i32::from(door.zone[0]) + i32::from(door.zone[2]) / 2,
        0,
        i32::from(door.zone[1]) + i32::from(door.zone[3]) / 2,
    ];
    state.interact(center, 0, false);
    let transition = state.transition.expect("auto transition");
    assert_eq!(transition.target.room, 0x0D);
    assert_eq!(transition.pos, [3000, 0, 27800]);
    assert_eq!(transition.angle, 1024);
    assert_destination_exists(&root, &transition);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn dead_lock_door_never_opens() {
    let Some(root) = asset_root() else {
        return;
    };
    // ROOM2010 slot 1: lock 0xFF, probe 0x00 (script-only, and locked for good).
    let (_, _, _, mut state) = load_room(&root, "2010");
    let door = state.doors[1].expect("dead door");
    assert_eq!(door.lock, 0xFF);
    assert_eq!(door.sub_type, 0x00);
    assert!(!state.try_door(1));
    assert_eq!(state.message.id, Some(210));
    assert!(!state.try_door(1));
}
