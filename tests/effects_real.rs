//! Real-asset effect tests: the RDT sprite pipeline and the room 100
//! chandelier spawn.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test effects_real -- --ignored`

mod common;

use arklay::effects;
use arklay::game::GameState;
use arklay::pack::Pack;
use arklay::state::RoomId;

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1000_chandelier_effect_spawns() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    let data = std::fs::read(root.join("JPN/STAGE1/ROOM1000.RDT")).unwrap();
    let state = arklay::rdt::parse(&data, RoomId::parse("1000").unwrap()).unwrap();

    // The room declares its single sprite (type 38); the chandelier script
    // spawns the global weapon type 9 (smoke) with depth group 7.
    assert_eq!(state.effects.index[0], 38);
    assert!(state.effects.sprite(38).is_some());

    let pack = Pack::open(&pack_path).unwrap();
    let weapon = effects::WeaponEffects::load(&pack);
    assert!(weapon.warnings.is_empty(), "{:?}", weapon.warnings);
    assert_eq!(weapon.slot_of(9), Some(1));

    let mut game = GameState::default();
    game.weapon_effects = weapon;
    let slot = effects::create(
        &mut game,
        &state.effects,
        9,
        7,
        0,
        [4420, -2500, 3800],
        1536,
        0,
    )
    .expect("the chandelier smoke spawns");

    assert_eq!(slot, 63);
    let effect = game.effects.slot(usize::from(slot)).unwrap();
    assert_eq!(effect.effect_type, 9);
    assert_eq!(effect.sprite, Some(9));
    assert_eq!(effect.depth_group, 7);
    assert_eq!(effect.local_offset, [4420, -2500, 3800]);
    assert_eq!(effect.spawn_pos, [4420, -2500, 3800]);
    assert_eq!(effect.yaw, 1536);
    assert_eq!(effect.anim_id, 2);
    assert_eq!(effect.attach, effects::Attach::Identity);
    assert_eq!(game.effects.active_count(), 1);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room_effect_pages_pack_inside_the_loaded_page_count() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    // ROOM1010 declares three sprites against esp000 + esp201; they must pack
    // onto pages the runtime loads (0-3) and their UVs must stay inside.
    let data = std::fs::read(root.join("JPN/STAGE1/ROOM1010.RDT")).unwrap();
    let state = arklay::rdt::parse(&data, RoomId::parse("1010").unwrap()).unwrap();
    assert_eq!(state.effects.index[..3], [3, 4, 32][..]);
    assert_eq!(state.effects.sprites.len(), 3);

    let weapon = effects::WeaponEffects::load(&Pack::open(&pack_path).unwrap());
    let mut room = state.effects.clone();
    effects::pages::pack(&weapon, &mut room);
    for sprite in &room.sprites {
        assert!(
            sprite.info.page_index() < 4,
            "sprite {} packed page {}",
            sprite.index,
            sprite.info.page_index()
        );
        assert!(
            u16::from(sprite.info.page_v) + sprite.geometry.height <= 256,
            "sprite {} art region runs past the page",
            sprite.index
        );
    }
}
