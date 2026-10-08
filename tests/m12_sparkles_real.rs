//! M12 sparkle real-asset tests: room 100's absolute sword-key sparkle, room
//! 513's parented lab-key sparkle and the corpus walk over all 43 shipped
//! `0x8000` builds.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test m12_sparkles_real -- --ignored`

mod common;

use arklay::effects;
use arklay::engine::{NEW_GAME_ROOM_ITEMS, SimulatedRoom, render_game_frame, simulate_room_seeded};
use arklay::game::{FlagBank, GameState, RoomActionKind, ScdGameHost};
use arklay::objects;
use arklay::pack::Pack;
use arklay::player;
use arklay::rdt;
use arklay::render::Camera;
use arklay::scd;
use arklay::scd::host::ScdHost;
use arklay::scd::ir::Operand;
use arklay::scd::opcode::command_op;
use arklay::state::{Image, RoomId};

fn operand_u8(operands: &[Operand], index: usize) -> u8 {
    operands.get(index).map_or(0, |operand| operand.value as u8)
}

fn operand_u16(operands: &[Operand], index: usize) -> u16 {
    operands
        .get(index)
        .map_or(0, |operand| operand.value as u16)
}

/// Open the converted pack, or skip when the asset environment is absent.
fn assets() -> Option<Pack> {
    let (_, pack_path) = common::asset_env()?;
    Some(Pack::open(&pack_path).unwrap())
}

/// The selectors set in the shipped new-game room-items bank plus the
/// second-playthrough flag, as `set` operations for `simulate_room_seeded`.
/// The pattern clears selectors 1 and 172.
fn seeded_flags(extra: &[(u8, u8)]) -> Vec<(u8, u8)> {
    let mut bank = FlagBank::new();
    bank.bytes_mut().copy_from_slice(&NEW_GAME_ROOM_ITEMS);
    let mut flags: Vec<(u8, u8)> = (0..=255u16)
        .filter(|bit| bank.bit(*bit as u8))
        .map(|bit| (7, bit as u8))
        .collect();
    flags.push((0, 0x7B));
    flags.extend_from_slice(extra);
    flags
}

/// The item action whose item id is `item`, with its model slot.
fn item_action(game: &GameState, item: u8) -> Option<(u8, usize)> {
    game.room_actions
        .iter()
        .flatten()
        .find(|action| action.kind == RoomActionKind::Item && action.item_id() == item)
        .map(|action| (action.slot, usize::from(action.item_model())))
}

fn render(pack: &Pack, id: RoomId, sim: &SimulatedRoom, game: &GameState) -> Image {
    let mut game = game.clone();
    render_game_frame(pack, id, &sim.room, &mut game, &sim.player).unwrap()
}

/// Pixels that differ between two frames within `radius` of `center`.
fn changed_near(a: &Image, b: &Image, center: [i32; 2], radius: i32) -> usize {
    let mut changed = 0;
    for y in (center[1] - radius)..=(center[1] + radius) {
        for x in (center[0] - radius)..=(center[0] + radius) {
            if x < 0 || y < 0 || x >= a.width as i32 || y >= a.height as i32 {
                continue;
            }
            let offset = ((y as u32 * a.width + x as u32) * 4) as usize;
            if a.rgba[offset..offset + 4] != b.rgba[offset..offset + 4] {
                changed += 1;
            }
        }
    }
    changed
}

/// Whether any live effect is a `0x0B` sparkle.
fn any_sparkle(game: &GameState) -> bool {
    game.effects
        .active()
        .any(|(_, effect)| effect.effect_type == 0x0B)
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_100_sparkle_paints_over_the_item_and_vanishes_on_pickup() {
    let Some(pack) = assets() else {
        return;
    };
    let id = RoomId::parse("1000").unwrap();
    let mut sim = simulate_room_seeded(&pack, id, &seeded_flags(&[]), 0, player::Input::default())
        .expect("ROOM1000 loads");
    let (slot, model) = item_action(&sim.game, 0x33).expect("the sword key registers");
    let sparkle = sim.game.items.record(model).unwrap().sparkle;
    assert_ne!(sparkle, 0, "the sword key's 0x8700 spawns its sparkle");
    assert!(any_sparkle(&sim.game));

    let world = objects::item_world_transform(
        &sim.game.items,
        &sim.game.objects,
        model,
        sim.player.pos,
        sim.player.angle,
    );
    // One behaviour tick arms the sparkle's transform bit (the first frame's
    // behaviour writes the header flags), so later re-projections project it.
    sim.game.tick_effects(&sim.room);
    // Isolate the sparkle pixels: hide the item mesh (the sparkle still
    // resolves the item's frame), render with and without the `0x0B` slots,
    // and keep the cut where the sparkle paints the most, so the capture does
    // not depend on the init camera or mesh occlusion.
    let mut item_hidden = sim.game.clone();
    item_hidden.items.record_mut(model).unwrap().flag = 0;
    let mut no_sparkle = item_hidden.clone();
    for index in 0..effects::EFFECT_POOL_SIZE {
        if no_sparkle
            .effects
            .slot(index)
            .is_some_and(|effect| effect.effect_type == 0x0B)
        {
            no_sparkle.effects.release(index);
        }
    }
    let mut chosen = None;
    for cut in 0..sim.room.cuts.len() {
        sim.room.current_cut = cut;
        let mut sparkle_game = item_hidden.clone();
        sparkle_game.reproject_effects(&sim.room);
        let mut blank_game = no_sparkle.clone();
        blank_game.reproject_effects(&sim.room);
        let with = render(&pack, id, &sim, &sparkle_game);
        let without = render(&pack, id, &sim, &blank_game);
        let changed = changed_near(&with, &without, [160, 120], 320);
        if chosen
            .as_ref()
            .is_none_or(|(_, _, _, best)| changed > *best)
        {
            chosen = Some((cut, with, without, changed));
        }
    }
    let (cut, with, blank, sparkle_pixels) =
        chosen.expect("a cut frames the sword key and its sparkle");
    assert!(
        sparkle_pixels > 10,
        "the sparkle painted only {sparkle_pixels} pixels"
    );
    let center = Camera::from_cut(&sim.room.cuts[cut])
        .project(world.t)
        .expect("the sword key is on screen");
    assert!(
        changed_near(&with, &blank, center, 120) > 10,
        "the sparkle pixels are not over the item at {center:?}"
    );

    // Re-apply the chosen cut to the live state for the pickup comparison.
    sim.room.current_cut = cut;
    sim.game.camera.current_cut = cut;
    sim.game.reproject_effects(&sim.room);
    let before_pickup = render(&pack, id, &sim, &sim.game);

    // A simulated pickup consumes the action, clears the model and frees the
    // sparkle; the sprite's other frame slots run out too.
    assert!(sim.game.pick_up(slot));
    assert_eq!(sim.game.items.record(model).unwrap().flag, 0);
    assert!(sim.game.room_actions[usize::from(slot)].is_none());
    for _ in 0..400 {
        if !any_sparkle(&sim.game) {
            break;
        }
        sim.game.tick_effects(&sim.room);
    }
    assert!(!any_sparkle(&sim.game), "the sparkle did not vanish");
    let after = render(&pack, id, &sim, &sim.game);
    assert!(
        changed_near(&before_pickup, &after, center, 320) >= sparkle_pixels,
        "the pickup did not remove the sparkle's pixels"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_513_parented_sparkle_tracks_omodel_6() {
    let Some(pack) = assets() else {
        return;
    };
    let id = RoomId::parse("5130").unwrap();
    // The key's build is gated on ScenarioFlags2 bit 0xC0.
    let mut sim = simulate_room_seeded(
        &pack,
        id,
        &seeded_flags(&[(1, 0xC0)]),
        0,
        player::Input::default(),
    )
    .expect("ROOM5130 loads");
    let (_, model) = item_action(&sim.game, 0x37).expect("the lab key registers");
    let record = *sim.game.items.record(model).unwrap();
    assert_eq!(record.parent, 0x06);
    let handle = usize::from(record.sparkle);
    assert_ne!(handle, 0, "the key's 0x8511 spawns its sparkle");
    let effect = *sim.game.effects.slot(handle).unwrap();
    assert_eq!(effect.effect_type, 0x0B);
    assert_eq!(effect.depth_group, 0x0C, "the 0x500 nibble is effect 0x0C");
    assert_eq!(effect.attach, effects::Attach::Item(model as u8));
    assert_eq!(effect.local_offset, [0, -32, 0], "bias byte 0x10 is -32");

    sim.game.tick_effects(&sim.room);
    let before = sim.game.effects.slot(handle).unwrap().pos;

    // Move the parent: the sparkle's world position must follow.
    sim.game.objects.record_mut(6).unwrap().pos[0] += 250;
    sim.game.tick_effects(&sim.room);
    let after = sim.game.effects.slot(handle).unwrap().pos;
    assert_eq!(
        i32::from(after[0]) - i32::from(before[0]),
        250,
        "the sparkle did not track omodel 6"
    );
    assert_eq!(after[1], before[1], "the vertical bias is unchanged");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn shipped_sparkle_variety_rooms_pick_their_effect_and_bias() {
    let Some(pack) = assets() else {
        return;
    };
    // One room per shipped `0x0F00` nibble (plus the two nonzero bias bytes):
    // the room declares the site, the test reads the spawned billboard back.
    type Case = (&'static str, &'static [(u8, u8)], u8, u8, i16);
    let cases: [Case; 5] = [
        ("1000", &[], 0x33, 0x1C, 0),
        ("4091", &[], 0x13, 0x04, -32),
        ("40F0", &[], 0x3D, 0x14, 0),
        ("2100", &[], 0x2C, 0x03, -64),
        // The lab key's build is gated on ScenarioFlags2 bit 0xC0.
        ("5130", &[(1, 0xC0)], 0x37, 0x0C, -32),
    ];
    for (room, extra, item, effect_id, bias) in cases {
        let sim = simulate_room_seeded(
            &pack,
            RoomId::parse(room).unwrap(),
            &seeded_flags(extra),
            0,
            player::Input::default(),
        )
        .unwrap_or_else(|error| panic!("ROOM{room} loads: {error:#}"));
        let (_, model) = item_action(&sim.game, item)
            .unwrap_or_else(|| panic!("ROOM{room} item {item:#04x} registers"));
        let record = sim.game.items.record(model).expect("the item record");
        let sparkle = *sim
            .game
            .effects
            .slot(usize::from(record.sparkle))
            .unwrap_or_else(|| panic!("ROOM{room} spawned no sparkle"));
        assert_eq!(
            sparkle.depth_group, effect_id,
            "ROOM{room} item {item:#04x} effect id"
        );
        assert_eq!(
            sparkle.local_offset,
            [0, bias, 0],
            "ROOM{room} item {item:#04x} bias"
        );
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn corpus_walk_spawns_all_43_shipped_sparkles_and_frees_them() {
    let Some((_, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let mut rooms: Vec<String> = pack
        .paths()
        .filter(|path| path.starts_with("room/") && path.ends_with(".rdt"))
        .map(|path| path[5..path.len() - 4].to_string())
        .collect();
    rooms.sort();
    assert!(rooms.len() > 300, "expected the shipped room corpus");

    let mut spawned = 0usize;
    let mut rooms_with_sparkles = 0usize;
    for name in &rooms {
        let id = RoomId::parse(name).unwrap();
        let Ok(data) = pack.read(&id.rdt_entry()) else {
            continue;
        };
        let Ok(room) = rdt::parse(data, id) else {
            continue;
        };
        if room.item_models.is_empty() {
            continue;
        }
        let Ok(scripts) = scd::reader::parse(data) else {
            continue;
        };
        // Every `item_aot_set` site whose flags word carries the sparkle bit,
        // dispatched directly so a conditional branch did not skip one.
        let mut sites = Vec::new();
        for block in &scripts.init {
            for insn in &block.insns {
                if insn.op == 0x18 && operand_u16(&insn.operands, 15) & 0x8000 != 0 {
                    sites.push(insn.operands.clone());
                }
            }
        }
        if sites.is_empty() {
            continue;
        }
        rooms_with_sparkles += 1;

        let mut game = GameState::new(id, &room);
        game.set_weapon_effects(effects::WeaponEffects::load(&pack));
        game.resolve_room_effects(&room);
        game.flags[7]
            .bytes_mut()
            .copy_from_slice(&NEW_GAME_ROOM_ITEMS);
        // Second playthrough: room 30E1's ribbon site must not be skipped.
        game.apply_flag(0, 0x7B, 0);
        for operands in sites {
            let slot = usize::from(operand_u8(&operands, 0) & 0x7F);
            let model = usize::from(operand_u8(&operands, 7));
            // Each site is an independent declaration; site pairs that share a
            // room flag live on opposite conditional branches, so re-seed the
            // bank to give every site the still-here bit in turn.
            game.flags[7]
                .bytes_mut()
                .copy_from_slice(&NEW_GAME_ROOM_ITEMS);
            {
                let mut host = ScdGameHost::new(&mut game);
                host.on_item(command_op(0x18).unwrap(), &operands);
            }
            spawned += 1;
            let record = *game.items.record(model).expect("the item record");
            assert_ne!(
                record.sparkle, 0,
                "ROOM{name}: the sparkle site spawned no billboard"
            );
            let claimed: Vec<usize> = game
                .effects
                .active()
                .filter(|(_, effect)| effect.effect_type == 0x0B)
                .map(|(index, _)| index)
                .collect();
            assert!(!claimed.is_empty());

            // Drive the handler the build derived, as the walk-up probe does,
            // then the award the viewer's prompt resolves to: the sparkle is
            // freed by the take, not by the probe.
            let action = game.room_actions[slot].expect("the item action");
            assert_eq!(action.item_model(), operand_u8(&operands, 7));
            game.run_room_action(slot as u8, action.handler);
            match action.handler {
                0x0D => {
                    assert!(game.take_document(slot as u8), "ROOM{name}: file it");
                }
                4 => {
                    assert!(game.take_message_item(), "ROOM{name}: take it");
                }
                // The map handler (0x0F) awards in the probe itself.
                _ => {}
            }
            assert_eq!(
                game.items.record(model).unwrap().sparkle,
                0,
                "ROOM{name}: the pickup did not clear the sparkle handle"
            );

            // Tick the remaining per-frame billboards out of the animation.
            for _ in 0..600 {
                if claimed.iter().all(|index| {
                    game.effects
                        .slot(*index)
                        .is_some_and(|effect| effect.anim_id == 0 && effect.update_id == 0)
                }) {
                    break;
                }
                game.tick_effects(&room);
            }
            for index in &claimed {
                let effect = game.effects.slot(*index).unwrap();
                assert!(
                    effect.anim_id == 0 && effect.update_id == 0,
                    "ROOM{name}: sparkle slot {index} did not free ({effect:?})"
                );
            }
        }
    }
    assert_eq!(
        spawned, 43,
        "the corpus must spawn all 43 shipped sparkle builds"
    );
    assert!(rooms_with_sparkles > 10, "too few sparkle rooms");
}
