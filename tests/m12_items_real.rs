//! M12 item-model real-asset tests: ROOM100's sword key build and capture,
//! the statue items riding omodel 0 in rooms 10D/60D, room 513's key on
//! omodel 6 and room 102's scripted `eml_rot`/`model_flag_set` writes.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test m12_items_real -- --ignored`

mod common;

use arklay::engine::{SimulatedRoom, render_game_frame, simulate_room, simulate_room_seeded};
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

/// Boot `room` and run it for a fixed number of idle ticks.
fn simulate(pack: &Pack, room: &str) -> SimulatedRoom {
    simulate_room(
        pack,
        RoomId::parse(room).unwrap(),
        60,
        player::Input::default(),
    )
    .unwrap_or_else(|error| panic!("ROOM{room} loads: {error:#}"))
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
    assert_eq!(sword_key.sparkle, 0);

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
    let sim = simulate_room_seeded(
        &pack,
        RoomId::parse("5130").unwrap(),
        &[(1, 0xC0)],
        60,
        player::Input::default(),
    )
    .expect("ROOM5130 loads");
    let action = item_action(&sim.game, 0x37).expect("the lab key registers");
    let model = usize::from(action.item_model());
    let record = *item_record(&sim.game, action).expect("the lab key record");
    assert_eq!(record.parent, 0x06);
    assert_eq!(record.pos, [0, 0, 0]);
    assert_eq!(record.asset, Some(0));

    // The second declaration of omodel 6 from the other branch is the one the
    // parent carries; a zero local offset lands the key exactly on it. The
    // scripted drawn-bit writes are masked by the port's inverted room-items
    // polarity until M12 slice 4, so only the transform is asserted here.
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
