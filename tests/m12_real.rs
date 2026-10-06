//! M12 integration pass: the desk flow on the shipped rooms (401 and 102), the
//! corpus desk/item reachability audit and the deterministic `--ticks`
//! captures.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test m12_real -- --ignored`

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use arklay::engine::{
    NEW_GAME_ROOM_ITEMS, SimulatedRoom, render_game_frame, simulate_room, simulate_room_seeded,
};
use arklay::game::{FlagBank, GameState, RoomAction, RoomActionKind};
use arklay::message::MessageWindow;
use arklay::pack::Pack;
use arklay::player;
use arklay::state::{RoomId, RoomState};

/// Open the converted pack, or skip when the asset environment is absent.
fn assets() -> Option<Pack> {
    let (_, pack_path) = common::asset_env()?;
    Some(Pack::open(&pack_path).unwrap())
}

/// The selectors set in the shipped new-game room-items bank, decoded through
/// the same [`FlagBank`] the game uses. The pattern clears selectors 1 and
/// 172, so a fixture that needs those adds them explicitly.
fn new_game_room_item_bits() -> Vec<(u8, u8)> {
    let mut bank = FlagBank::new();
    bank.bytes_mut().copy_from_slice(&NEW_GAME_ROOM_ITEMS);
    (0..=255u16)
        .filter(|bit| bank.bit(*bit as u8))
        .map(|bit| (7, bit as u8))
        .collect()
}

/// The shipped room-items bank plus the second playthrough flag, so no item
/// build is conditionally skipped.
fn seeded_flags(extra: &[(u8, u8)]) -> Vec<(u8, u8)> {
    let mut flags = new_game_room_item_bits();
    flags.push((0, 0x7B));
    flags.extend_from_slice(extra);
    flags
}

/// Boot `room` with the new-game room-items bank and run it idle.
fn simulate(pack: &Pack, room: &str) -> SimulatedRoom {
    simulate_room_seeded(
        pack,
        RoomId::parse(room).unwrap(),
        &seeded_flags(&[]),
        30,
        player::Input::default(),
    )
    .unwrap_or_else(|error| panic!("ROOM{room} loads: {error:#}"))
}

/// The registered desk action slots of a room.
fn desk_slots(game: &GameState) -> Vec<u8> {
    game.room_actions
        .iter()
        .flatten()
        .filter(|action| action.kind == RoomActionKind::Desk)
        .map(|action| action.slot)
        .collect()
}

/// The item action a desk opens.
fn desk_item(game: &GameState, desk_slot: u8) -> RoomAction {
    let desk = game.room_actions[usize::from(desk_slot)].expect("desk action");
    let item_slot = desk.param_word(1) as u8;
    game.room_actions[usize::from(item_slot)].expect("desk item action")
}

/// Turn the locked desk's key: prompt, answer yes and check the key-turn
/// message. Leaves the action table with the lock bit raised and the SE queue
/// drained.
fn turn_desk_key(game: &mut GameState, desk_slot: u8) -> u8 {
    let desk = game.room_actions[usize::from(desk_slot)].expect("desk");
    let lock_bit = desk.param_word(0) as u8;
    let key = if game.flag_test(0, 0x7C, false) {
        0x31
    } else {
        0x3D
    };
    game.message = MessageWindow::default();
    assert!(game.check_desk(desk_slot));
    assert_eq!(game.desk.state, 1);
    game.check_desk_state();
    assert_eq!(game.message.id, Some(0xD9), "the key prompt plays");
    assert_eq!(game.selected_item, Some(key), "the prompt names the key");
    game.message.active = false;
    game.message.set_menu_choice_id(0);
    game.check_desk_state();
    assert!(game.flag_test(2, lock_bit, false), "the lock bit is set");
    assert_eq!(game.message.id, Some(0xC3), "the key-turn message plays");
    assert_eq!(game.sfx_requests, vec![0x26], "the key-turn SE is queued");
    game.cancel_message();
    game.sfx_requests.clear();
    key
}

/// Drive one desk through the full flow: refuse without the key, prompt, turn
/// the key, open the model, pan and award. Returns the awarded item id.
fn drive_desk(game: &mut GameState, desk_slot: u8) -> u8 {
    let item = desk_item(game, desk_slot);
    let item_slot = item.slot;
    let item_id = item.item_id();
    let model = usize::from(item.item_model());
    let (lock_bit, camera) = {
        let desk = game.room_actions[usize::from(desk_slot)].expect("desk");
        (desk.param_word(0) as u8, desk.param_word(2) as u8)
    };

    if !game.flag_test(2, lock_bit, false) {
        // Empty-handed: the desk refuses with the locked message.
        game.message = MessageWindow::default();
        assert!(game.check_desk(desk_slot), "the desk handles the probe");
        assert_eq!(game.message.id, Some(0xD8), "the locked message plays");
        assert_eq!(game.desk.state, 0);
        game.cancel_message();
        game.add_item(0x3D, 1);
        turn_desk_key(game, desk_slot);
    }

    // The next probe swings the desk open onto its model and camera.
    let saved = game.camera.current_cut;
    assert!(game.check_desk(desk_slot));
    assert_eq!(game.desk.state, 35);
    assert_eq!(game.camera.current_cut, usize::from(camera));
    assert_eq!(game.desk.saved_camera, Some(saved));
    assert_eq!(game.sfx_requests, vec![0x24], "the lid SE is queued");
    assert_eq!(
        game.items.record(model).expect("item record").flag & 1,
        1,
        "the desk model is drawn"
    );

    // The pan counts 35 -> 5, then state 5 awards and state 4 closes.
    for _ in 0..30 {
        game.check_desk_state();
    }
    assert_eq!(game.desk.state, 5);
    assert_eq!(game.last_picked_item, None, "no award before state 5");
    game.check_desk_state();
    assert_eq!(game.desk.state, 4);
    assert_eq!(game.last_picked_item, Some(item_id));
    assert!(
        !game.room_item_present(item.room_items_flag),
        "the room-items bit is cleared"
    );
    assert!(
        game.room_actions[usize::from(item_slot)].is_none(),
        "the desk consumes its item edge"
    );
    game.check_desk_state();
    assert_eq!(game.desk.state, 0);
    assert_eq!(game.camera.current_cut, saved, "the room camera returns");
    assert_eq!(game.items.record(model).unwrap().flag & 1, 0);
    item_id
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
fn room_401_desk_locked_refuses_then_opens_after_the_key_turns() {
    let Some(pack) = assets() else {
        return;
    };
    let mut sim = simulate(&pack, "4010");
    assert_eq!(desk_slots(&sim.game), vec![6], "ROOM4010's desk registers");
    let desk = sim.game.room_actions[6].unwrap();
    assert_eq!(desk.param_word(0), 0x21, "the desk lock bit");
    assert_eq!(desk.param_word(2), 3, "the desk camera");
    let item = desk_item(&sim.game, 6);
    let item_slot = desk.param_word(1) as u8;
    let model = usize::from(item.item_model());
    assert_eq!(item.item_id(), 0x0C, "the drawer hides shells");
    // The drawer model starts hidden (an init `eml_rot`/`model_flag_set` pair
    // stores it closed) and the desk is what opens it.
    assert_eq!(sim.game.items.record(model).unwrap().flag & 1, 0);

    // Without the key: message 0xD8, no state change, nothing awarded.
    let inventory = sim.game.inventory.clone();
    assert!(sim.game.check_desk(6));
    assert_eq!(sim.game.message.id, Some(0xD8));
    assert_eq!(sim.game.desk.state, 0);
    assert_eq!(sim.game.inventory, inventory, "the refusal awards nothing");
    sim.game.cancel_message();

    // With the desk key: prompt, key turn, then the opened desk camera.
    sim.game.add_item(0x3D, 1);
    assert_eq!(turn_desk_key(&mut sim.game, 6), 0x3D);
    let saved = sim.game.camera.current_cut;
    assert!(sim.game.check_desk(6));
    assert_eq!(sim.game.desk.state, 35);
    assert_eq!(sim.game.camera.current_cut, 3, "the desk cut is selected");
    assert_eq!(sim.game.desk.saved_camera, Some(saved));
    assert_eq!(sim.game.sfx_requests, vec![0x24]);
    assert_eq!(sim.game.items.record(model).unwrap().flag & 1, 1);

    // The opened drawer paints in the desk camera; hiding the model again
    // removes those pixels.
    sim.room.current_cut = 3;
    let opened = render_game_frame(&pack, sim.id, &sim.room, &mut sim.game, &sim.player).unwrap();
    sim.game.items.record_mut(model).unwrap().flag &= !1;
    let closed = render_game_frame(&pack, sim.id, &sim.room, &mut sim.game, &sim.player).unwrap();
    let changed = opened
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(closed.rgba.as_chunks::<4>().0)
        .filter(|(open, closed)| open != closed)
        .count();
    assert!(changed > 0, "the opened desk model painted no pixels");
    sim.game.items.record_mut(model).unwrap().flag |= 1;

    // Finish the pan: the award consumes the drawer edge and restores the
    // camera.
    for _ in 0..30 {
        sim.game.check_desk_state();
    }
    assert_eq!(sim.game.desk.state, 5);
    sim.game.check_desk_state();
    assert_eq!(sim.game.last_picked_item, Some(0x0C));
    assert!(
        sim.game.room_actions[usize::from(item_slot)].is_none(),
        "the drawer edge is consumed"
    );
    sim.game.check_desk_state();
    assert_eq!(sim.game.desk.state, 0);
    assert_eq!(sim.game.camera.current_cut, saved);
    assert_eq!(sim.game.items.record(model).unwrap().flag & 1, 0);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_102_scripted_desk_opens_the_edge_it_references() {
    let Some(pack) = assets() else {
        return;
    };
    let mut sim = simulate(&pack, "1020");
    assert_eq!(desk_slots(&sim.game), vec![4], "ROOM1020's desk registers");
    let desk = sim.game.room_actions[4].unwrap();
    assert_eq!(desk.param_word(0), 0x1E, "the desk lock bit");
    assert_eq!(desk.param_word(2), 3, "the desk camera");
    // The desk's second word is the room action slot of the item it opens:
    // slot 2, the shell box on model 1. The drawer item (0x0B on model 2) is
    // the separately scripted `eml_rot`/`model_flag_set` pair.
    let item = desk_item(&sim.game, 4);
    assert_eq!(item.slot, 2);
    assert_eq!(item.item_id(), 0x0C);
    assert_eq!(item.item_model(), 1);
    assert_eq!(item.room_items_flag, 0x31);

    // The scripted writes landed before the desk runs.
    assert_eq!(
        sim.game.items.record(0).unwrap().rotation,
        [-100, -2000, -1100]
    );
    assert_eq!(
        sim.game.items.record(1).unwrap().flag,
        0,
        "script-closed model"
    );
    assert_eq!(sim.game.items.record(2).unwrap().flag & 1, 1, "drawer item");

    // Drive the full flow; it must open model 1 and award slot 2's shells
    // without disturbing the drawer item.
    assert_eq!(drive_desk(&mut sim.game, 4), 0x0C);
    assert_eq!(
        sim.game.items.record(2).unwrap().flag & 1,
        1,
        "drawer intact"
    );
    assert_eq!(sim.game.items.record(1).unwrap().flag & 1, 0);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn corpus_desks_run_the_full_flow_and_items_stay_in_bounds() {
    let Some(pack) = assets() else {
        return;
    };
    let ids = room_ids(&pack);
    assert!(ids.len() > 300, "expected the shipped room corpus");

    // The M12 opcode set: a placeholder hit for any of these in a shipped
    // script would mean the milestone left an implemented build path inert.
    const M12_OPCODES: [u8; 4] = [0x18, 0x19, 0x35, 0x3B];

    let mut simulated = 0usize;
    let mut desks = 0usize;
    let mut desk_rooms = BTreeSet::new();
    let mut item_rooms = 0usize;
    let mut built_items = 0usize;
    let mut hidden_items = 0usize;
    for id in &ids {
        let Ok(mut sim) = simulate_room_seeded(
            &pack,
            *id,
            &seeded_flags(&[]),
            300,
            player::Input::default(),
        ) else {
            // Stub rooms without camera cuts cannot load.
            continue;
        };
        simulated += 1;
        if sim.game.items.built > 0 {
            item_rooms += 1;
            built_items += usize::from(sim.game.items.built);
        }
        let hits: Vec<(u8, u64)> = M12_OPCODES
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
            "ROOM{id:?} dispatched M12 placeholders: {hits:?}"
        );

        // Every live item record's composed world position stays inside the
        // collision extents (with the same margin the object audit allows).
        // Parented items carry local offsets (room 10D's statue items sit at
        // negative local X/Z), so the world matrix is what must be checked.
        let bounds = collision_bounds(&sim.room);
        let margin = 0x4000;
        for (slot, record) in sim.game.items.records.iter().enumerate() {
            if !record.active() {
                continue;
            }
            // The corpus ships four off-map declarations parked at the
            // (0x7530, 0x7530, 0x7530) sentinel: ROOM3071, both ROOM30A*
            // variants and ROOM5021. The original never frames them; the
            // other room bounds happen to be large enough that only ROOM5021
            // falls outside the collision extents.
            if record.pos == [30000, 30000, 30000] {
                hidden_items += 1;
                continue;
            }
            let world = arklay::objects::item_world_transform(
                &sim.game.items,
                &sim.game.objects,
                slot,
                sim.player.pos,
                sim.player.angle,
            );
            let (x, z) = (world.t[0], world.t[2]);
            if let Some(([min_x, min_z], [max_x, max_z])) = bounds {
                assert!(
                    x >= min_x - margin
                        && x <= max_x + margin
                        && z >= min_z - margin
                        && z <= max_z + margin,
                    "ROOM{id:?} item {slot} at {world:?} is far outside the room"
                );
            } else {
                assert!(
                    (-margin..=0xFFFF + margin).contains(&x)
                        && (-margin..=0xFFFF + margin).contains(&z),
                    "ROOM{id:?} item {slot} at {world:?} left the room space"
                );
            }
        }

        // Every registered desk runs the whole locked/key/open/award flow.
        let slots = desk_slots(&sim.game);
        if !slots.is_empty() {
            desk_rooms.insert((id.stage, id.room));
        }
        for slot in slots {
            drive_desk(&mut sim.game, slot);
            desks += 1;
        }
    }

    println!(
        "corpus: {simulated} rooms simulated, {item_rooms} built {built_items} item models, \
         {hidden_items} off-map sentinel(s), {desks} desks in {} room variants",
        desk_rooms.len()
    );
    assert!(simulated > 300, "only {simulated} rooms simulated");
    assert_eq!(
        item_rooms, 226,
        "the shipped seed must build items in 226 room variants"
    );
    assert_eq!(
        built_items, 519,
        "the shipped seed must build all 519 item declarations"
    );
    assert_eq!(
        hidden_items, 4,
        "the four off-map sentinels are the known set"
    );
    assert_eq!(desks, 16, "the corpus must register all 16 desk sites");
    assert_eq!(desk_rooms.len(), 8, "the desks span eight room variants");
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
fn real_room_401_ticks_captures_are_deterministic() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("arklay-m12-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Two CLI runs with the same arguments are byte-identical.
    let first = cli_capture(&pack_path, "401", 30, &dir.join("first.bmp"));
    let second = cli_capture(&pack_path, "401", 30, &dir.join("second.bmp"));
    assert_eq!(first, second, "two --ticks captures differ");

    // The library seam reproduces the CLI frame exactly.
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("4010").unwrap();
    let captured = arklay::bmp::decode(&first).unwrap();
    let mut sim = simulate_room(&pack, id, 30, player::Input::default()).unwrap();
    assert_eq!(
        captured.rgba, sim.frame.rgba,
        "CLI capture != simulated frame"
    );

    // The direct boot seeds the shipped room-items bank, so ROOM4010's three
    // declarations all build instead of reading as taken. Without this the
    // capture above would compare two item-less frames. The key's edge is
    // re-armed inert by the init `room_action_arm`, so two actions register.
    assert_eq!(sim.game.items.built, 3, "ROOM4010 builds its three items");
    let registered = sim
        .game
        .room_actions
        .iter()
        .flatten()
        .filter(|action| action.kind == RoomActionKind::Item)
        .count();
    assert_eq!(registered, 2, "the book and the drawer edge register");
    assert!(
        sim.game.items.records.iter().any(|record| record.active()),
        "at least one item model is drawn"
    );

    // Some cut frames the live items, and hiding them changes the pixels.
    let mut painted = 0usize;
    for cut in 0..sim.room.cuts.len() {
        sim.room.current_cut = cut;
        let shown = render_game_frame(&pack, id, &sim.room, &mut sim.game, &sim.player).unwrap();
        let mut hidden = sim.game.clone();
        for record in &mut hidden.items.records {
            record.flag = 0;
        }
        let blank = render_game_frame(&pack, id, &sim.room, &mut hidden, &sim.player).unwrap();
        painted = painted.max(
            shown
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .zip(blank.rgba.as_chunks::<4>().0)
                .filter(|(shown, blank)| shown != blank)
                .count(),
        );
    }
    assert!(painted > 0, "the item models painted no pixels in any cut");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_item_rooms_render_deterministically() {
    let Some(pack) = assets() else {
        return;
    };
    for room in ["1000", "1020", "10D0", "3030", "4010", "4060", "5130"] {
        let id = RoomId::parse(room).unwrap();
        let first = simulate_room_seeded(
            &pack,
            id,
            &seeded_flags(&[(1, 0xC0)]),
            30,
            player::Input::default(),
        )
        .unwrap_or_else(|error| panic!("ROOM{room} loads: {error:#}"));
        let second = simulate_room_seeded(
            &pack,
            id,
            &seeded_flags(&[(1, 0xC0)]),
            30,
            player::Input::default(),
        )
        .unwrap();
        assert_eq!(
            first.frame.rgba, second.frame.rgba,
            "ROOM{room} capture varies"
        );
    }
}
