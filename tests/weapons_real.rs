//! Real-asset weapon tests: every packed `W*.EMW`/`WS*.TMD` parses and
//! resolves, and a shipped room's monster dies through the real weapon input
//! path with the held weapon painted.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test weapons_real -- --ignored`

mod common;

use arklay::game::BANK_ENEMIES;
use arklay::pack::Pack;
use arklay::player::Input;
use arklay::state::RoomId;
use arklay::weapons::{self, WeaponClips};

/// The web's room, shared with the combat tests.
const WEB_ROOM: &str = "30C0";

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn every_packed_weapon_asset_parses_and_resolves() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();

    // Both characters' full weapon rows resolve to packed EMWs.
    for character in 0..=1u8 {
        for weapon in 0..=0x0Du8 {
            let entry = weapons::weapon_emw_entry(character, weapon).unwrap();
            let bytes = pack
                .read(&entry)
                .unwrap_or_else(|_| panic!("missing {entry}"));
            let emw = arklay::emd::parse_emw(bytes).unwrap_or_else(|err| {
                panic!("{entry} does not parse: {err:#}");
            });
            assert!(!emw.clips.is_empty(), "{entry} carries no clips");
            assert_eq!(emw.mesh.objects.len(), 1, "{entry} is the held mesh");
            assert_eq!(emw.skeleton.relative.len(), 15, "{entry} joints");
        }
        // The two special weapons' rows.
        for weapon in [weapons::ITEM_INGRAM, weapons::ITEM_MINIMI] {
            let entry = weapons::weapon_emw_entry(character, weapon).unwrap();
            assert!(pack.contains(&entry), "missing special {entry}");
        }
    }
    assert!(pack.contains("player/00.emw"), "the locomotion pair");
    assert!(pack.contains("player/01.emw"));

    // Every character's held-weapon mesh resolves and parses.
    for id in 0x20..=0x2Eu8 {
        for behavior in 1..=6u8 {
            let entry = weapons::character_weapon_entry(id, behavior).unwrap();
            let bytes = pack
                .read(&entry)
                .unwrap_or_else(|_| panic!("missing {entry}"));
            let tmd = arklay::tmd::parse(bytes).unwrap_or_else(|err| {
                panic!("{entry} does not parse: {err:#}");
            });
            assert_eq!(tmd.objects.len(), 1, "{entry} is the held mesh");
        }
    }
    assert_eq!(weapons::character_weapon_entry(0x20, 0), None, "unarmed");
}

/// Boot the web room, equip the knife and swing through [`weapons::update`]
/// with the real packed clips: the web must die and the killing blow must
/// raise its room event.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room_web_dies_through_the_weapon_input_path() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse(WEB_ROOM).unwrap();
    let mut run = arklay::engine::simulate_room(&pack, id, 4, Input::default()).unwrap();

    let slot = (1..arklay::game::ENTITY_COUNT)
        .find(|&slot| {
            let entity = run.game.entities[slot];
            entity.active() && entity.id == 0x13
        })
        .expect("ROOM30C0 spawns the door-blocking web");
    let web = run.game.entities[slot];
    let death_bit = web.death_event_id;
    assert_ne!(death_bit, 0xFF, "the room script polls the death event");

    // The knife is the web's damaging weapon; stand close and face it.
    run.game.set_equipped(Some(weapons::ITEM_KNIFE));
    run.player.pos = [web.pos[0] + 300, web.pos[1], web.pos[2]];
    run.player.angle =
        arklay::sfx::angle_between_xz(run.player.pos[0], run.player.pos[2], web.pos[0], web.pos[2]);
    run.game.entities[0].pos = run.player.pos;

    let emd_bytes = pack.read("player/00.emd").unwrap();
    let emd = arklay::emd::parse(emd_bytes).unwrap();
    let emw = arklay::emd::parse_emw(pack.read("player/00.emw").unwrap()).unwrap();
    let weapon = arklay::emd::parse_emw(pack.read("player/w01.emw").unwrap()).unwrap();
    let clips = WeaponClips {
        emd: &emd.clips,
        emw: &emw.clips,
        room: &[],
        weapon: &weapon.clips,
        emd_keyframes: &emd.keyframes,
        emd_skeleton: &emd.skeleton,
        weapon_keyframes: &weapon.keyframes,
        weapon_skeleton: &weapon.skeleton,
    };
    // The web's own script clears its hit latch between swings, so interleave
    // it with the weapon tick the way the engine's entity pass does.
    let mut host = arklay::enemy::LuaEnemyHost::new();
    for tick in 0..300 {
        let input = Input {
            aim: true,
            fire: true,
            fire_pressed: tick == 0,
            ..Input::default()
        };
        weapons::update(&mut run.game, &mut run.player, &run.room, &clips, input);
        assert!(host.update(&mut run.game, &run.room, &pack, slot, &[]));
    }

    assert_eq!(
        run.game.entities[slot].state(),
        4,
        "the web ran its destroy state"
    );
    assert!(
        run.game.flags[usize::from(BANK_ENEMIES)].bit(death_bit),
        "the killing blow raised the room event"
    );
}

/// The held-weapon capture: an aim schedule with a packed weapon paints
/// different pixels than the weaponless idle in the same shipped room.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn a_held_weapon_changes_the_captured_frame() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    // Room 101 has no cutscene that latches the script's controls lock, so
    // the aim input reaches the weapon machine; the main hall's intro
    // (`set FG_6, 23, 1`) blanks the pad for its whole scene.
    let id = RoomId::parse("1010").unwrap();

    let idle = arklay::engine::simulate_room(&pack, id, 8, Input::default()).unwrap();
    let aiming = arklay::engine::simulate_room_prepared(
        &pack,
        id,
        &[],
        8,
        |game, _player| {
            game.inventory.push(arklay::game::InventoryItem {
                id: 10,
                quantity: 4,
            });
            game.set_equipped(Some(10));
        },
        |_| Input {
            aim: true,
            ..Input::default()
        },
    )
    .unwrap();
    // Room 1000 spawns no enemy, so the only difference is the raised weapon.
    let mut changed = 0usize;
    for (a, b) in idle.frame.rgba.chunks(4).zip(aiming.frame.rgba.chunks(4)) {
        if a != b {
            changed += 1;
        }
    }
    assert!(
        changed > 0,
        "the aim pose and held weapon painted no pixels"
    );
}
