//! M12 item-model real-asset tests: ROOM100's sword key build and capture,
//! the statue items riding omodel 0 in rooms 10D/60D, room 513's key on
//! omodel 6 and room 102's scripted `eml_rot`/`model_flag_set` writes.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test m12_items_real -- --ignored`

mod common;

use arklay::engine::{SimulatedRoom, render_game_frame, simulate_new_game, simulate_room_seeded};
use arklay::game::{GameState, RoomAction, RoomActionKind};
use arklay::objects;
use arklay::pack::Pack;
use arklay::player;
use arklay::render::Lighting;
use arklay::state::RoomId;

/// Open the converted pack, or skip when the asset environment is absent.
fn assets() -> Option<Pack> {
    let (_, pack_path) = common::asset_env()?;
    Some(Pack::open(&pack_path).unwrap())
}

/// The item action whose item id is `item`, when the room registered one.
fn item_action(game: &GameState, item: u8) -> Option<&RoomAction> {
    game.room_actions
        .iter()
        .flatten()
        .find(|action| action.kind == RoomActionKind::Item && action.item_id() == item)
}

/// The item record the action built (the model operand is the pair index).
fn item_record<'a>(game: &'a GameState, action: &RoomAction) -> Option<&'a objects::ItemRecord> {
    game.items.record(usize::from(action.item_model()))
}

/// The new-game room-items bank shape ("every bit still here") as `set`
/// operations, so a real room's `item_aot_set` sites register. The test
/// fixtures boot a room directly and have no save block to seed from.
fn new_game_shape() -> Vec<(u8, u8)> {
    (0..=255u16).map(|bit| (7, bit as u8)).collect()
}

/// Boot `room` with the room-items bank set, plus any extra flag bits, and run
/// it for a fixed number of idle ticks.
fn simulate_with_flags(pack: &Pack, room: &str, extra: &[(u8, u8)]) -> SimulatedRoom {
    let mut flags = new_game_shape();
    flags.extend_from_slice(extra);
    simulate_room_seeded(
        pack,
        RoomId::parse(room).unwrap(),
        &flags,
        60,
        player::Input::default(),
    )
    .unwrap_or_else(|error| panic!("ROOM{room} loads: {error:#}"))
}

/// Boot `room` with the room-items bank set and run it for 60 idle ticks.
fn simulate(pack: &Pack, room: &str) -> SimulatedRoom {
    simulate_with_flags(pack, room, &[])
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_100_sword_key_builds_and_changes_the_capture() {
    let Some(pack) = assets() else {
        return;
    };
    let id = RoomId::parse("1000").unwrap();
    let mut sim = simulate(&pack, "1000");

    let action = item_action(&sim.game, 0x33).expect("the sword key registers");
    assert_eq!(
        action.handler, 4,
        "an ordinary item uses the pickup handler"
    );
    assert_eq!(action.room_items_flag, 0x16);
    assert_eq!(action.item_model(), 0);
    let sword_key = *item_record(&sim.game, action).expect("the sword key record");
    assert_eq!(sword_key.flag, 1);
    assert_eq!(sword_key.model, 0);
    assert_eq!(sword_key.parent, 0xFF);
    assert_eq!(sword_key.pos, [5160, -930, 8690]);
    assert_eq!(sword_key.committed, [5160, -930, 8690]);
    assert_eq!(sword_key.rotation, [0, 0x0E42, 900]);
    assert_eq!(sword_key.asset, Some(0));
    // The 0x8700 flags word spawns the pick-up sparkle.
    assert_ne!(sword_key.sparkle, 0, "the sword key's sparkle is live");
    assert_eq!(
        sim.game
            .effects
            .slot(usize::from(sword_key.sparkle))
            .unwrap()
            .effect_type,
        0x0B
    );

    // The rendered frame differs from the same state with every item hidden,
    // and two renders of one state agree.
    let frame = render_game_frame(&pack, id, &sim.room, &sim.game, &sim.player).unwrap();
    let again = render_game_frame(&pack, id, &sim.room, &sim.game, &sim.player).unwrap();
    assert_eq!(frame.rgba, again.rgba, "the item pass is deterministic");
    for item in &mut sim.game.items.records {
        item.flag = 0;
    }
    let hidden = render_game_frame(&pack, id, &sim.room, &sim.game, &sim.player).unwrap();
    let changed = frame
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(hidden.rgba.as_chunks::<4>().0)
        .filter(|(visible, hidden)| visible != hidden)
        .count();
    assert!(changed > 0, "the item meshes painted no pixels");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_10d_and_60d_statue_items_ride_omodel_0() {
    let Some(pack) = assets() else {
        return;
    };
    for room in ["10D0", "60D0"] {
        let sim = simulate(&pack, room);
        for (item, model, local) in [
            (0x29u8, 0usize, [-450, -1550, 530]),
            (0x05, 1, [-450, -1500, -360]),
        ] {
            let action = item_action(&sim.game, item)
                .unwrap_or_else(|| panic!("ROOM{room} item {item:#04x} registers"));
            assert_eq!(usize::from(action.item_model()), model);
            let record = item_record(&sim.game, action).unwrap();
            assert_eq!(record.parent, 0x00, "the item rides omodel 0");
            assert_eq!(record.pos, local);
            assert!(record.active());

            let parent = sim.game.objects.record(0).expect("omodel 0");
            assert!(parent.active(), "the statue is built");
            let lighting = Lighting::from_room(&sim.room);
            let world = objects::item_world_matrix(
                &sim.game.items,
                &sim.game.objects,
                &lighting,
                model,
                sim.player.pos,
                sim.player.angle,
            );
            // The statue's zero yaw makes the composed translation its own
            // position plus the local offset, within the 4.12 truncation.
            for (axis, local_axis) in local.iter().enumerate() {
                let expected = parent.pos[axis] + local_axis;
                assert!(
                    (world.t[axis] - expected).abs() <= 1,
                    "ROOM{room} item {item:#04x} axis {axis}: {:?} is not the statue plus {local:?}",
                    world.t
                );
            }
        }
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_513_lab_key_rides_omodel_6() {
    let Some(pack) = assets() else {
        return;
    };
    // The key's build is gated on ScenarioFlags2 bit 0xC0.
    let sim = simulate_with_flags(&pack, "5130", &[(1, 0xC0)]);
    let action = item_action(&sim.game, 0x37).expect("the lab key registers");
    let model = usize::from(action.item_model());
    let record = *item_record(&sim.game, action).expect("the lab key record");
    assert_eq!(record.parent, 0x06);
    assert_eq!(record.pos, [0, 0, 0]);
    assert_eq!(record.asset, Some(0));
    // The 0x8511 flags word spawns the bias-1 sparkle on the same omodel.
    let sparkle = *sim
        .game
        .effects
        .slot(usize::from(record.sparkle))
        .expect("the parented sparkle is live");
    assert_eq!(sparkle.effect_type, 0x0B);
    assert_eq!(sparkle.depth_group, 0x0C);
    assert_eq!(sparkle.attach, arklay::effects::Attach::Item(0));
    assert_eq!(sparkle.local_offset, [0, -2, 0]);

    // The second declaration of omodel 6 from the other branch is the one the
    // parent carries; a zero local offset lands the key exactly on it.
    let parent = sim.game.objects.record(6).expect("omodel 6");
    let lighting = Lighting::from_room(&sim.room);
    let world = objects::item_world_matrix(
        &sim.game.items,
        &sim.game.objects,
        &lighting,
        model,
        sim.player.pos,
        sim.player.angle,
    );
    assert_eq!(world.t, objects::rebuild(parent, &lighting).t);

    // The room's other item is absolute at the far end of the corridor.
    let absolute = item_action(&sim.game, 0x0B).expect("the absolute item registers");
    let absolute_record = *item_record(&sim.game, absolute).unwrap();
    assert_eq!(absolute_record.parent, 0xFF);
    assert_eq!(absolute_record.pos, [6668, 0, 30866]);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_102_scripted_item_writes_reach_the_records() {
    let Some(pack) = assets() else {
        return;
    };
    let sim = simulate(&pack, "1020");
    assert_eq!(sim.game.items.built, 3, "three item declarations build");

    // The drawer's final item (0x0B) is active on model 2.
    let action = item_action(&sim.game, 0x0B).expect("the drawer item registers");
    assert_eq!(action.item_model(), 2);
    let drawer = *item_record(&sim.game, action).expect("the drawer record");
    assert_eq!(drawer.flag, 1);
    assert_eq!(drawer.pos, [5440, -2100, 9370]);

    // The scripted `eml_rot` wrote record 0's X/Z (Y stays the anim word);
    // record 1 was rotated and then had its drawn bit cleared by
    // `model_flag_set`.
    let first = *sim.game.items.record(0).expect("record 0");
    assert_eq!(first.pos, [6840, -2250, 9670]);
    assert_eq!(first.rotation, [-100, -2000, -1100]);
    assert_eq!(first.flag, 1);

    let rotated = *sim.game.items.record(1).expect("record 1");
    assert_eq!(rotated.rotation, [0, 1000, -1100]);
    assert_eq!(rotated.flag, 0);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_300_courtyard_map_raises_the_room_flag_without_inventory() {
    use std::rc::Rc;

    use arklay::game::ScdGameHost;
    use arklay::scd::vm::{CommandVm, EventVm};

    let Some(pack) = assets() else {
        return;
    };
    let id = RoomId::parse("3000").unwrap();
    let data = pack.read(&id.rdt_entry()).unwrap();
    let room = arklay::rdt::parse(data, id).unwrap();
    let scripts = Rc::new(arklay::scd::reader::parse(data).unwrap());
    let mut game = GameState::new(id, &room);
    game.set_weapon_effects(arklay::effects::WeaponEffects::load(&pack));
    game.resolve_room_effects(&room);
    game.flags[7]
        .bytes_mut()
        .copy_from_slice(&arklay::engine::NEW_GAME_ROOM_ITEMS);
    {
        let mut vm = CommandVm::new(&scripts);
        let mut host = ScdGameHost::new(&mut game);
        vm.run_init(&mut host);
    }
    // The init script re-arms slot 4 as the map's create_room_event action;
    // its original item operands survive the reset (the original's +8 record
    // pointer). Event 3 resets it to the key-pickup handler and runs it.
    let item = game.items.record(0).expect("the map record");
    assert_eq!(item.flag, 1, "the map model is built");
    let before_inventory = game.inventory.clone();
    let mut event_vm = EventVm::new(&scripts);
    event_vm.start(0, 3);
    for _ in 0..8 {
        {
            let mut host = ScdGameHost::new(&mut game);
            event_vm.step(&mut host);
        }
        if game.last_picked_item == Some(0x50) {
            break;
        }
    }

    assert_eq!(game.last_picked_item, Some(0x50));
    assert_eq!(
        game.inventory, before_inventory,
        "a map never enters the inventory"
    );
    // `0x7C + (0x50 - 0x4E)` = the courtyard map's owned bit.
    assert!(
        game.flags[8].bit(0x7C + 2),
        "the courtyard map's RoomFlags bit is raised"
    );
    assert!(!game.room_item_present(0x4F));
    assert_eq!(
        game.items.record(0).unwrap().flag,
        0,
        "the map model is cleared"
    );
    assert!(game.room_actions[4].is_none(), "the map action is consumed");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_406_guardhouse_map_raises_its_room_flag() {
    let Some(pack) = assets() else {
        return;
    };
    let mut sim = simulate(&pack, "4060");
    let action = *item_action(&sim.game, 0x52).expect("the guardhouse map registers");
    assert_eq!(action.handler, 0x0F);
    assert_eq!(action.item_model(), 2);
    let before_inventory = sim.game.inventory.clone();

    assert!(sim.game.run_room_action(action.slot, 0x0F));
    assert_eq!(sim.game.inventory, before_inventory);
    // `0x7C + (0x52 - 0x4E)` = the guardhouse map's owned bit.
    assert!(
        sim.game.flags[8].bit(0x7C + 4),
        "the guardhouse map's RoomFlags bit is raised"
    );
    assert_eq!(sim.game.last_picked_item, Some(0x52));
    assert!(sim.game.room_actions[usize::from(action.slot)].is_none());
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn simulate_new_game_in_room_100_registers_the_three_item_actions() {
    let Some(pack) = assets() else {
        return;
    };
    let sim = simulate_new_game(&pack, 0, 60, player::Input::default()).expect("ROOM1000 loads");
    let mut items: Vec<u8> = sim
        .game
        .room_actions
        .iter()
        .flatten()
        .filter(|action| action.kind == RoomActionKind::Item)
        .map(|action| action.item_id())
        .collect();
    items.sort_unstable();
    assert_eq!(
        items,
        vec![0x33, 0x42, 0x42],
        "the new-game room-items bank registers all three builds"
    );
    assert_eq!(sim.game.items.built, 3);
}
