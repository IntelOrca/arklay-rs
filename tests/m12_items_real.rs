//! M12 item-model real-asset tests: ROOM100's sword key build and capture,
//! the statue items riding omodel 0 in rooms 10D/60D, room 513's key on
//! omodel 6 and room 102's scripted `eml_rot`/`model_flag_set` writes.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test m12_items_real -- --ignored`

mod common;

use arklay::engine::{
    NEW_GAME_ROOM_ITEMS, SimulatedRoom, render_game_frame, simulate_new_game, simulate_room_seeded,
};
use arklay::game::{FlagBank, GameState, RoomAction, RoomActionKind};
use arklay::objects;
use arklay::pack::Pack;
use arklay::player;
use arklay::rdt;
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

/// The selectors set in the shipped new-game room-items bank ("bit still
/// here") as `set` operations, decoded through the same [`FlagBank`] the game
/// uses. The pattern clears selectors 1 and 172, so a fixture that needs one
/// adds it explicitly.
fn new_game_shape() -> Vec<(u8, u8)> {
    let mut bank = FlagBank::new();
    bank.bytes_mut().copy_from_slice(&NEW_GAME_ROOM_ITEMS);
    (0..=255u16)
        .filter(|bit| bank.bit(*bit as u8))
        .map(|bit| (7, bit as u8))
        .collect()
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
    let frame = render_game_frame(&pack, id, &sim.room, &mut sim.game, &sim.player).unwrap();
    let again = render_game_frame(&pack, id, &sim.room, &mut sim.game, &sim.player).unwrap();
    assert_eq!(frame.rgba, again.rgba, "the item pass is deterministic");
    for item in &mut sim.game.items.records {
        item.flag = 0;
    }
    let hidden = render_game_frame(&pack, id, &sim.room, &mut sim.game, &sim.player).unwrap();
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
    let mut sim = simulate_with_flags(&pack, "5130", &[(1, 0xC0)]);
    let action = item_action(&sim.game, 0x37).expect("the lab key registers");
    let model = usize::from(action.item_model());
    let record = *item_record(&sim.game, action).expect("the lab key record");
    assert_eq!(record.parent, 0x06);
    assert_eq!(record.pos, [0, 0, 0]);
    assert_eq!(record.asset, Some(0));
    // The 0x8511 flags word's bias byte 0x10 is -32 on the same omodel.
    let handle = usize::from(record.sparkle);
    let sparkle = *sim
        .game
        .effects
        .slot(handle)
        .expect("the parented sparkle is live");
    assert_eq!(sparkle.effect_type, 0x0B);
    assert_eq!(sparkle.depth_group, 0x0C);
    assert_eq!(sparkle.attach, arklay::effects::Attach::Item(0));
    assert_eq!(sparkle.local_offset, [0, -32, 0]);

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

    // Once the behaviour has armed the billboard, its world Y is the item's
    // world Y minus the 32-unit bias, not the old 2-unit nibble reading.
    sim.game.tick_effects(&sim.room);
    let effect = *sim.game.effects.slot(handle).unwrap();
    for (axis, expected) in [world.t[0], world.t[1] - 32, world.t[2]].iter().enumerate() {
        assert!(
            (i32::from(effect.pos[axis]) - expected).abs() <= 1,
            "the 513 sparkle axis {axis} is {:?}, not {expected}",
            effect.pos
        );
    }

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

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_303_flare_follows_the_player_when_they_move() {
    let Some(pack) = assets() else {
        return;
    };
    let mut sim = simulate(&pack, "3030");
    let action = item_action(&sim.game, 0x2A).expect("the flare registers");
    let model = usize::from(action.item_model());
    let record = *item_record(&sim.game, action).expect("the flare record");
    assert_eq!(record.parent, 0xFE, "the flare is parented to the player");

    let before = objects::item_world_transform(
        &sim.game.items,
        &sim.game.objects,
        model,
        sim.player.pos,
        sim.player.angle,
    );
    // Move the player: the flare is re-resolved against the moved frame.
    sim.player.pos[0] += 1000;
    sim.player.pos[2] -= 500;
    sim.game.sync_entity_from_player(&sim.player);
    let after = objects::item_world_transform(
        &sim.game.items,
        &sim.game.objects,
        model,
        sim.player.pos,
        sim.player.angle,
    );
    assert_eq!(
        after.t[0] - before.t[0],
        1000,
        "the flare did not follow the player's X move"
    );
    assert_eq!(
        after.t[2] - before.t[2],
        -500,
        "the flare did not follow the player's Z move"
    );
    assert_eq!(after.t[1], before.t[1], "Y is unchanged");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_30e_jill_skips_the_ink_ribbon_until_the_second_playthrough() {
    let Some(pack) = assets() else {
        return;
    };
    // Jill's variant is the player digit 1. The first playthrough clears the
    // ribbon site's room-items bit without building the model.
    let first = simulate_with_flags(&pack, "30E1", &[]);
    assert!(
        !first.game.room_item_present(0xF0),
        "the ribbon site is marked taken"
    );
    assert_eq!(
        first.game.items.built, 2,
        "only the two non-ribbon items build"
    );
    assert_eq!(
        first.game.items.record(2).unwrap(),
        &objects::ItemRecord::default(),
        "the ribbon model is not built"
    );
    assert!(item_action(&first.game, 0x2F).is_none());

    // The second-playthrough flag lets the ribbon site build normally.
    let second = simulate_with_flags(&pack, "30E1", &[(0, 0x7B)]);
    assert!(second.game.room_item_present(0xF0));
    assert_eq!(second.game.items.built, 3);
    let action = item_action(&second.game, 0x2F).expect("the ribbon registers");
    assert_eq!(action.item_model(), 2);
    let record = item_record(&second.game, action).unwrap();
    assert_eq!(record.flag, 1);
    assert_ne!(record.sparkle, 0, "the 0x8700 ribbon sparkle is live");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_715_second_trophy_item_sits_0x96_lower() {
    let Some(pack) = assets() else {
        return;
    };
    let sim = simulate(&pack, "7150");
    assert_eq!(sim.game.items.built, 4, "all four declarations build");
    // The RDT declares model 1 (the second build) at z=8180; the override
    // drops 0x96 so the document sits on the lower trophy.
    let lower = sim.game.items.record(1).expect("the second build");
    assert_eq!(lower.pos[2], 8180 - 0x96);
    assert_eq!(lower.committed[2], (8180 - 0x96) as i16);
    // The first build keeps its declared Z.
    assert_eq!(sim.game.items.record(0).unwrap().pos[2], 10020);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_406_map_palette_is_darkened_in_the_render() {
    let Some(pack) = assets() else {
        return;
    };
    let id = RoomId::parse("4060").unwrap();
    let mut sim = simulate(&pack, "4060");
    let map = item_action(&sim.game, 0x52).expect("the guardhouse map registers");
    let pair = usize::from(map.item_model());
    assert!(item_record(&sim.game, map).unwrap().active());

    // Decode the shipped, undarkened pair straight from the RDT.
    let data = pack.read(&id.rdt_entry()).unwrap();
    let stock = rdt::parse(data, id).unwrap();
    let stock_texture = stock
        .item_models
        .iter()
        .find(|asset| asset.pair_index == pair)
        .expect("the map pair")
        .texture
        .clone();
    let darkened_texture = sim
        .room
        .item_models
        .iter()
        .find(|asset| asset.pair_index == pair)
        .expect("the loaded map pair")
        .texture
        .clone();
    assert_ne!(
        darkened_texture.palettes, stock_texture.palettes,
        "the map pair's palette was not rewritten"
    );

    // Replacing only the texture reverts the darkening; some cut frames the
    // map and must change pixels.
    let mut stock_room = sim.room.clone();
    stock_room
        .item_models
        .iter_mut()
        .find(|asset| asset.pair_index == pair)
        .expect("the loaded map pair")
        .texture = stock_texture;
    let mut painted = 0usize;
    for cut in 0..sim.room.cuts.len() {
        stock_room.current_cut = cut;
        let mut darkened_room = stock_room.clone();
        darkened_room.current_cut = cut;
        darkened_room
            .item_models
            .iter_mut()
            .find(|asset| asset.pair_index == pair)
            .expect("the loaded map pair")
            .texture = darkened_texture.clone();
        let darkened =
            render_game_frame(&pack, id, &darkened_room, &mut sim.game, &sim.player).unwrap();
        let stock = render_game_frame(&pack, id, &stock_room, &mut sim.game, &sim.player).unwrap();
        painted = painted.max(
            darkened
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .zip(stock.rgba.as_chunks::<4>().0)
                .filter(|(darkened, stock)| darkened != stock)
                .count(),
        );
    }
    assert!(painted > 0, "the darkened map palette painted no pixels");
}
