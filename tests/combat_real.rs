//! Real-asset combat tests: the packed `data/combat.bin` must match a fresh
//! decode of the install's executable, and the shared fire entry must drive a
//! live spider web through the scripted damage -> death contract.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test combat_real -- --ignored`

mod common;

use std::fs;

use arklay::combat::CombatTables;
use arklay::enemy::LuaEnemyHost;
use arklay::game::BANK_ENEMIES;
use arklay::pack::Pack;
use arklay::player;
use arklay::state::RoomId;

/// The web's room: the spider blocks its doorway, and the room script polls
/// the spawn record's death event to open it.
const WEB_ROOM: &str = "30C0";

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn packed_combat_tables_match_a_fresh_executable_decode() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let packed = CombatTables::parse(
        pack.read(arklay::combat::COMBAT_ENTRY)
            .expect("the converted pack carries data/combat.bin"),
    )
    .unwrap();

    let exe = fs::read(root.join("Bio.exe")).expect("the install's executable");
    let fresh = arklay::convert::extract_combat_tables(&exe).unwrap();
    assert_eq!(
        packed, fresh,
        "the packed combat tables differ from a fresh decode"
    );

    // Structural invariants the pipeline depends on.
    assert!(
        packed
            .weapon_ranges
            .iter()
            .flatten()
            .all(|&range| range > 0)
    );
    assert!(
        packed
            .first_run
            .iter()
            .any(|record| record.damage > 0 && record.effect_type != 0)
    );
    assert!(packed.second_run.iter().any(|record| record.damage > 0));
    assert!(
        packed
            .hit_joints
            .iter()
            .all(|row| row.iter().all(|&joint| joint < 16))
    );
    assert_eq!(
        packed.hit_record(0, 0x00).unwrap().knockback,
        [100, -1800, 0]
    );
    assert_eq!(
        packed.weapon_range(1, 0),
        Some(1200),
        "Chris' handgun range"
    );
    assert_eq!(
        packed.weapon_range(1, 1),
        Some(1300),
        "Jill's handgun range"
    );
    assert_eq!(
        packed.model_entry(0x13, 0),
        Some("enemy/em1013.emd"),
        "the web's model path"
    );
    assert!(packed.hit_record(0, 0x14).is_none(), "NPCs have no rows");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room_web_dies_through_the_shared_fire_entry() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse(WEB_ROOM).unwrap();
    let mut run = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();

    let slot = (1..arklay::game::ENTITY_COUNT)
        .find(|&slot| {
            let entity = run.game.entities[slot];
            entity.active() && entity.id == 0x13
        })
        .expect("ROOM30C0 spawns the door-blocking web");
    let web = run.game.entities[slot];
    assert_eq!(web.state(), 1, "the script initialised the web");
    assert_eq!(web.health, 0x37);
    let death_bit = web.death_event_id;
    assert_ne!(death_bit, 0xFF, "the room script polls the death event");

    // Stand on the web and fire the rocket: the projectile class ignores the
    // aim cone and the line-of-sight rule, and the record's 900 damage kills
    // the 55-health web outright.
    run.game.player_flags = 0x40;
    run.game.entities[0].pos = web.pos;
    assert_eq!(run.game.apply_weapon_damage(&run.room, 10), 1);
    assert_eq!(run.game.entities[slot].state(), 3);
    assert!(
        run.game.flags[usize::from(BANK_ENEMIES)].bit(death_bit),
        "the killing shot raised the room event"
    );

    // The web's destroy state clears the last strands and parks.
    let mut host = LuaEnemyHost::new();
    assert!(host.update(&mut run.game, &run.room, &pack, slot, &[]));
    assert_eq!(run.game.entities[slot].state(), 4);
    assert_eq!(run.game.entities[slot].joint_flags, 0b0011_1111);
    assert_eq!(run.game.entities[slot].status_flags & 0x02, 0x02);
}

/// The pipeline is deterministic and the packed tables drive it: two fresh
/// boots of the web room resolve the same damage outcome after the same fire.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn the_packed_tables_drive_a_deterministic_hit() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse(WEB_ROOM).unwrap();
    let mut outcome = Vec::new();
    for _ in 0..2 {
        let mut run =
            arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
        let slot = (1..arklay::game::ENTITY_COUNT)
            .find(|&slot| {
                let entity = run.game.entities[slot];
                entity.active() && entity.id == 0x13
            })
            .unwrap();
        run.game.player_flags = 0x40;
        run.game.entities[0].pos = run.game.entities[slot].pos;
        let hit = run.game.apply_weapon_damage(&run.room, 8);
        outcome.push((
            hit,
            run.game.entities[slot].health,
            run.game.entities[slot].hit_state,
            run.game.entities[slot].state(),
        ));
    }
    assert_eq!(outcome[0], outcome[1], "the fire entry is deterministic");
}
