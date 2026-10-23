//! Real-asset weapon tests: every packed `W*.EMW`/`WS*.TMD` parses and
//! resolves, and a shipped room's monster dies through the real weapon input
//! path with the held weapon painted.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/path/to/re1 ARKLAY_RE1_PACK=/path/to/re1.akpak cargo test --profile test-release --test real weapons_real:: -- --ignored`

use crate::common;

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
            let emw = arklay::emd::parse_emw(&bytes).unwrap_or_else(|err| {
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

    // Every armed character id's held-weapon mesh resolves and parses. The
    // ids in between (0x25..=0x29) are the unarmed characters and carry no
    // held-weapon TMD; `character_weapon_id` returns None for them.
    for id in (0x20..=0x24).chain(0x2A..=0x2E) {
        for behavior in 1..=6u8 {
            let entry = weapons::character_weapon_entry(id, behavior).unwrap();
            let bytes = pack
                .read(&entry)
                .unwrap_or_else(|_| panic!("missing {entry}"));
            let tmd = arklay::tmd::parse(&bytes).unwrap_or_else(|err| {
                panic!("{entry} does not parse: {err:#}");
            });
            assert_eq!(tmd.objects.len(), 1, "{entry} is the held mesh");
        }
    }
    for id in 0x25..=0x29 {
        for behavior in 1..=6u8 {
            assert_eq!(
                weapons::character_weapon_entry(id, behavior),
                None,
                "unarmed character {id:#04x} behavior {behavior}"
            );
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
    // Move the collision mirror with the placed position (the door and
    // scripted-motion paths do the same); a stale mirror resolves the first
    // movement back toward the spawn.
    run.player.collision_mirror = [run.player.pos[0], run.player.pos[2]];
    run.game.entities[0].pos = run.player.pos;

    let emd_bytes = pack.read("player/00.emd").unwrap();
    let emd = arklay::emd::parse(&emd_bytes).unwrap();
    let emw = arklay::emd::parse_emw(&pack.read("player/00.emw").unwrap()).unwrap();
    let weapon = arklay::emd::parse_emw(&pack.read("player/w01.emw").unwrap()).unwrap();
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
        // Hold the stance: the weapon-joint translation adjustment creeps the
        // player while the weapon machine drives the pose (the hand stays put
        // while the body settles), so a standing player is re-anchored every
        // tick, collision mirror included. The angle is the machine's own (its
        // turn-to-target steers it and needs to converge).
        run.player.pos = [web.pos[0] + 300, web.pos[1], web.pos[2]];
        run.player.collision_mirror = [run.player.pos[0], run.player.pos[2]];
        run.game.entities[0].pos = run.player.pos;
        run.game.entities[0].angle = run.player.angle;
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

/// The beretta's ejected magazine (issue #124): the real player EMD carries
/// the empty clip as a 16th mesh object, and a real tactical reload arms the
/// joint-15 physics, which falls to the floor and switches itself off.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn a_real_beretta_reload_ejects_the_clip_mesh() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();

    // The player EMD is 15 animated joints plus the clip's own 16th object;
    // the renderer only draws the latter while the physics is live.
    let emd = arklay::emd::parse(&pack.read("player/00.emd").unwrap()).unwrap();
    assert_eq!(emd.skeleton.relative.len(), 15, "the animated joints");
    assert_eq!(emd.mesh.objects.len(), 16, "15 body objects plus the clip");
    assert!(
        !emd.mesh.objects[15].prims.is_empty(),
        "the 16th object is the clip's own mesh"
    );

    // Room 101's script never latches the controls lock, so the aim + RUN
    // input reaches the weapon machine.
    let id = RoomId::parse("1010").unwrap();
    let reload = |ticks: usize| {
        arklay::engine::simulate_room_prepared(
            &pack,
            id,
            &[],
            ticks,
            |game, _player| {
                game.player_options.tactical_reload = true;
                game.inventory.push(arklay::game::InventoryItem {
                    id: 2,
                    quantity: 10,
                });
                game.inventory.push(arklay::game::InventoryItem {
                    id: 0x0B,
                    quantity: 15,
                });
                game.set_equipped(Some(2));
            },
            |tick| Input {
                aim: true,
                run: tick >= 3,
                ..Input::default()
            },
        )
        .unwrap()
    };

    // Mid-reload: the clip is in the air and drew this frame's pixels.
    let airborne = reload(40);
    let clip = airborne.player.detached_clip;
    assert!(clip.active(), "the reload left the clip in the air");
    assert!(
        i32::from(clip.vel_y) < i32::from(clip.floor_y),
        "the clip is still above the floor: {clip:?}"
    );
    assert!(
        airborne.player.pos[1] == i32::from(clip.floor_y),
        "the floor reference is the player's own Y"
    );

    // Late: the bounce has switched the clip off.
    let landed = reload(80);
    assert!(
        !landed.player.detached_clip.active(),
        "the clip switched itself off after the bounce"
    );
}

/// A handgun trigger cycle through the real packed clips queues the
/// weapon-bank fire sample (`Gun01`, the reference's sfx id 7), and every
/// queued name resolves in the pack (which must carry the weapon bank).
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn handgun_fire_queues_weapon_bank_sounds_that_are_packed() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse(WEB_ROOM).unwrap();
    let mut run = arklay::engine::simulate_room(&pack, id, 4, Input::default()).unwrap();

    run.game.inventory.push(arklay::game::InventoryItem {
        id: 2,
        quantity: 15,
    });
    run.game.set_equipped(Some(2));

    let character = run.game.id.player_flag & 3;
    let emd =
        arklay::emd::parse(&pack.read(&format!("player/{character:02}.emd")).unwrap()).unwrap();
    let emw =
        arklay::emd::parse_emw(&pack.read(&format!("player/{character:02}.emw")).unwrap()).unwrap();
    let weapon_entry = weapons::weapon_emw_entry(character, 2).unwrap();
    let weapon = arklay::emd::parse_emw(&pack.read(&weapon_entry).unwrap()).unwrap();
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

    let mut queued: Vec<&str> = Vec::new();
    for tick in 0..70 {
        let input = Input {
            aim: true,
            fire: tick < 30,
            fire_pressed: tick == 0,
            ..Input::default()
        };
        let before = run.game.entity_sounds.len();
        weapons::update(&mut run.game, &mut run.player, &run.room, &clips, input);
        queued.extend(
            run.game.entity_sounds[before..]
                .iter()
                .map(|sound| sound.name),
        );
    }

    assert!(
        queued.contains(&"Gun01"),
        "the fire frame queued the handgun sample: {queued:?}"
    );
    for name in queued {
        let path = format!("se/{}.wav", name.to_ascii_lowercase());
        assert!(pack.contains(&path), "{path} is missing from the pack");
    }
}

/// The equipped weapon is the hand joint's mesh in every bank: an idle player
/// with a weapon equipped paints different pixels than an unarmed one.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn an_idle_player_paints_the_equipped_weapon() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("1010").unwrap();

    let unarmed = arklay::engine::simulate_room(&pack, id, 8, Input::default()).unwrap();
    let armed = arklay::engine::simulate_room_prepared(
        &pack,
        id,
        &[],
        8,
        |game, _player| {
            game.inventory.push(arklay::game::InventoryItem {
                id: 2,
                quantity: 15,
            });
            game.set_equipped(Some(2));
        },
        |_| Input::default(),
    )
    .unwrap();
    if let Ok(dir) = std::env::var("ARKLAY_DUMP_DIR") {
        for (name, run) in [("unarmed", &unarmed), ("armed", &armed)] {
            let mut png = Vec::new();
            let mut enc = png::Encoder::new(&mut png, run.frame.width, run.frame.height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header()
                .unwrap()
                .write_image_data(&run.frame.rgba)
                .unwrap();
            std::fs::write(format!("{dir}/{name}.png"), png).unwrap();
        }
    }
    let changed = unarmed
        .frame
        .rgba
        .chunks(4)
        .zip(armed.frame.rgba.chunks(4))
        .filter(|(a, b)| a != b)
        .count();
    assert!(changed > 0, "the idle weapon painted no pixels");
}

/// The tactical reload through the real packed clips: aim + RUN on a
/// partially loaded Beretta refills its clip from the 9mm stack and fires no
/// round.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_tactical_reload_refills_the_beretta_from_its_ammo_stack() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse(WEB_ROOM).unwrap();
    let mut run = arklay::engine::simulate_room(&pack, id, 4, Input::default()).unwrap();

    run.game.inventory.push(arklay::game::InventoryItem {
        id: 2,
        quantity: 10,
    });
    run.game.set_equipped(Some(2));
    run.game.inventory.push(arklay::game::InventoryItem {
        id: 0x0B,
        quantity: 15,
    });
    run.game.player_options.tactical_reload = true;

    let character = run.game.id.player_flag & 3;
    let emd =
        arklay::emd::parse(&pack.read(&format!("player/{character:02}.emd")).unwrap()).unwrap();
    let emw =
        arklay::emd::parse_emw(&pack.read(&format!("player/{character:02}.emw")).unwrap()).unwrap();
    let weapon_entry = weapons::weapon_emw_entry(character, 2).unwrap();
    let weapon = arklay::emd::parse_emw(&pack.read(&weapon_entry).unwrap()).unwrap();
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

    let mut reached_reload = false;
    let mut lowest = 10;
    for _ in 0..240 {
        let input = Input {
            aim: true,
            run: true,
            ..Input::default()
        };
        weapons::update(&mut run.game, &mut run.player, &run.room, &clips, input);
        reached_reload |= run.player.weapon.behavior == 0x18;
        lowest = lowest.min(run.game.item_count(2) as u8);
        if run.game.item_count(2) == 15 {
            break;
        }
    }
    assert!(reached_reload, "aim + RUN started the reload");
    assert_eq!(lowest, 10, "the tactical reload fired no round");
    assert_eq!(run.game.item_count(2), 15, "the clip was refilled");
    assert_eq!(run.game.item_count(0x0B), 0, "the spare was consumed");
}
