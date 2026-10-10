//! Real-asset enemy tests: the scripted monsters must allocate, initialise and
//! run through the pack's Lua scripts.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test enemy_real -- --ignored`

mod common;

use arklay::game::Entity;
use arklay::pack::Pack;
use arklay::player;
use arklay::state::RoomId;

/// The first entity slot holding `id`, with its index.
fn entity_with_id(game: &arklay::game::GameState, id: u8) -> Option<(usize, Entity)> {
    game.entities
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, entity)| entity.active() && entity.id == id)
        .map(|(slot, entity)| (slot, *entity))
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room4080_wasp_nests_initialise_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("4080").unwrap();
    let model = pack
        .read(&format!("enemy/em07{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert!(
        model.skeleton.relative.len() >= 10,
        "the wasp model's ten joints are the effect anchors the death paths use"
    );

    // The nest room boots seven dormant wasps through the script's init.
    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let wasps: Vec<Entity> = first
        .game
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && entity.id == 0x07)
        .copied()
        .collect();
    assert_eq!(wasps.len(), 7, "ROOM4080 spawns seven wasp nests");
    for wasp in &wasps {
        assert_eq!(wasp.state(), 1, "init moved the nest to the run state");
        assert_eq!(wasp.behavior_flags, 0x10, "the dormant nest kind");
        assert!(
            (20..=62).contains(&wasp.health),
            "the script rolled the health: {}",
            wasp.health
        );
        assert_eq!(wasp.sca_radius, 0, "the wasp's SCA record has no radius");
        assert_eq!(wasp.sca_half_height, 0);
        assert_eq!(wasp.sca_offset, [0, 0, 0]);
        assert_eq!(
            wasp.shadow_half_x, wasp.shadow_half_z,
            "the run state sizes the quad from the root joint height"
        );
        assert!(wasp.shadow_half_x > 0);
        assert_eq!(wasp.shadow_tint, 0x0060_6060);
        assert_eq!(wasp.wasp_big, 0);
        assert_eq!(wasp.wasp_sound_latch, 0);
        assert_eq!(wasp.ignore(), 1, "the behaviour owns the dormant nest");
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room30c0_spider_web_initialises_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("30C0").unwrap();

    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let (slot, web) =
        entity_with_id(&first.game, 0x13).expect("ROOM30C0 spawns the door-blocking web");
    let model = pack
        .read(&format!("enemy/em13{}.emd", id.player_flag & 1))
        .unwrap();
    assert!(arklay::emd::parse(model).is_ok(), "the web model parses");

    assert_eq!(web.state(), 1, "the script's init moved the web to idle");
    assert_eq!(web.health, 0x37, "the script set the web health");
    assert_eq!(web.sca_radius, 500);
    assert_eq!(web.sca_offset, [-700, 0, 0], "Chris' scenario SCA offset");
    assert_eq!(web.shadow_half_x, 10, "the script built the ground quad");
    assert_eq!(web.shadow_half_z, 1000);
    assert_eq!(web.joint_scale, 0x1000);
    assert_eq!(
        first.game.entity_anims[slot].player.clip, 0,
        "the init clip advanced the clock once"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room3010_adders_initialise_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("3010").unwrap();
    let model = pack
        .read(&format!("enemy/em0a{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert_eq!(
        model.skeleton.relative.len(),
        11,
        "the adder model's eleven joints are the hierarchy the tail hiding walks"
    );

    // The water-gate room boots seven ceiling adders through the script.
    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let adders: Vec<Entity> = first
        .game
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && entity.id == 0x0a)
        .copied()
        .collect();
    assert_eq!(adders.len(), 7, "ROOM3010 spawns seven adders");
    for adder in &adders {
        assert_eq!(adder.state(), 1, "init moved the adder to run");
        assert_eq!(adder.behavior_flags, 1, "the water-gate spawn kind");
        assert!(
            (10..=52).contains(&adder.health),
            "the script rolled the health: {}",
            adder.health
        );
        assert_eq!(adder.sca_radius, 200);
        assert_eq!(adder.sca_half_height, 0);
        assert_eq!(adder.sca_offset, [0, 0, 0]);
        assert_eq!(adder.joint_scale, 0x3000);
        assert_eq!(adder.shadow_tint, 0x0020_2020);
        assert_eq!(adder.sca_active(), 1);
        assert_eq!(adder.hit_state, 0);
        assert_eq!(adder.roll, 0x800, "the ceiling spin");
        assert_eq!(adder.pitch, 0);
        assert!(
            adder.pos[1] <= -4000,
            "a ceiling adder starts high up: {}",
            adder.pos[1]
        );
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room40f0_plant42_roots_initialises_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("40F0").unwrap();
    let model = pack
        .read(&format!("enemy/em0e{}.emd", id.player_flag & 1))
        .unwrap();
    assert!(arklay::emd::parse(model).is_ok(), "the roots model parses");

    // A fresh boot takes the scenario script's nested dormant branch: the
    // mass initialises, pins to its spawn point and never moves.
    let first = arklay::engine::simulate_room(&pack, id, 6, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 6, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );
    let (slot, roots) =
        entity_with_id(&first.game, 0x0e).expect("ROOM40F0 spawns Plant 42's roots");
    assert_eq!(
        roots.state(),
        1,
        "the script's init moved the roots to idle"
    );
    assert_eq!(
        roots.behavior_flags, 0x80,
        "a fresh boot takes the nested dormant branch"
    );
    assert_eq!(roots.health, 1);
    assert_eq!(roots.joint_scale, 0x1000);
    assert_eq!(roots.status_flags & 0x02, 0, "dormant roots stay tangible");
    assert_eq!(roots.sca_radius, 0x07D0);
    assert_eq!(roots.sca_half_height, 0x1770);
    assert_eq!(roots.sca_offset, [0, 0, 0]);
    assert_eq!(roots.shadow_half_x, 0xC00);
    assert_eq!(roots.shadow_half_z, 0xC00);
    assert_eq!(roots.shadow_offset, [0, 0, -0x50]);
    assert_eq!(roots.shadow_tint, 0x0040_4040);
    assert_eq!(roots.stored_pos_x(), 6695, "the spawn point is frozen");
    assert_eq!(roots.stored_pos_z(), 2896);
    assert_eq!(roots.tint_queue, [0, 0, 0], "dormant roots stay untinted");
    assert!(!roots.tint_queue_armed);
    assert!(
        first.game.entity_sounds.is_empty(),
        "init and dormant idling queue no cue"
    );
    assert_eq!(
        first.game.entity_anims[slot].player.clip, 0,
        "the init clip advanced the clock once"
    );

    // Raising scenario bit 0 selects the retracted branch: the one-shot
    // collapse parks the scale and the tint, and shrinks the ground quad.
    let retracted =
        arklay::engine::simulate_room_seeded(&pack, id, &[(0, 0)], 6, player::Input::default())
            .unwrap();
    let (_, roots) =
        entity_with_id(&retracted.game, 0x0e).expect("the retracted branch also spawns");
    assert_eq!(roots.behavior_flags, 1);
    assert_eq!(roots.state(), 1);
    assert_eq!(roots.health, -1);
    assert_eq!(roots.joint_scale, 0x9C4);
    assert_eq!(roots.status_flags & 0x02, 0x02);
    assert_eq!(roots.shadow_half_x, 0xC00 - 2000);
    assert_eq!(roots.shadow_half_z, 0xC00 - 2000);
    assert_eq!(roots.tint_queue, [0, -6, -12]);
    assert!(roots.tint_queue_armed);
    assert_eq!(roots.pos, [6695, roots.pos[1], 2896], "the pin holds");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room30c0_black_tiger_initialises_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("30C0").unwrap();
    let model = pack
        .read(&format!("enemy/em04{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert_eq!(
        model.skeleton.relative.len(),
        20,
        "the Black Tiger's twenty joints are the leg and fang anchors"
    );

    let first = arklay::engine::simulate_room(&pack, id, 1, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 1, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let (slot, tiger) = entity_with_id(&first.game, 0x04).expect("ROOM30C0 spawns the Black Tiger");
    assert_eq!(tiger.state(), 1, "the script's init moved to the run state");
    assert_eq!(tiger.health, 0xCC, "the fixed 204 health");
    assert_eq!(tiger.sca_radius, 2500);
    assert_eq!(tiger.sca_half_height, 180);
    assert_eq!(tiger.sca_offset, [-500, -180, 0], "the idle rear box");
    assert_eq!(tiger.sca2, None, "the idle profile is a single box");
    assert_eq!(tiger.joint_scale, 0x1B33);
    assert_eq!(tiger.shadow_tint, 0x0040_4040);
    assert_eq!(tiger.shadow_half_x, 0x9C4);
    assert_eq!(tiger.shadow_half_z, 0x9C4);
    assert_eq!(tiger.sink_wobble(), 0x2D, "the post-attack cooldown");
    assert_eq!(
        first.game.entity_anims[slot].player.clip, 0,
        "the init clip advanced the clock once"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room4040_webspinners_initialise_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("4040").unwrap();
    let model = pack
        .read(&format!("enemy/em03{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert_eq!(
        model.skeleton.relative.len(),
        20,
        "the WebSpinner's twenty joints carry the leg and fang anchors"
    );

    let first = arklay::engine::simulate_room(&pack, id, 1, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 1, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let spiders: Vec<Entity> = first
        .game
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && entity.id == 0x03)
        .copied()
        .collect();
    assert_eq!(spiders.len(), 2, "ROOM4040 spawns two WebSpinners");
    for spider in &spiders {
        assert_eq!(
            spider.state(),
            1,
            "the script's init moved to the run state"
        );
        assert!(
            [0x63, 0x77, 0x59].contains(&spider.health),
            "the script rolled the health table: {}",
            spider.health
        );
        assert_eq!(spider.sca_radius, 1000, "the big SCA profile");
        assert_eq!(spider.sca_half_height, 180);
        assert_eq!(spider.sca_offset, [0, -180, 0]);
        assert_eq!(spider.joint_scale, 0);
        assert_eq!(spider.shadow_tint, 0x0080_8080);
        assert_eq!(spider.shadow_half_x, 0x4B0);
        assert_eq!(spider.shadow_half_z, 0x4B0);
        assert_eq!(spider.room_collision(), 0, "the delay starts clear");
        assert_eq!(spider.sca_active(), 0, "the splat flag starts clear");
        assert_eq!(spider.wasp_distance(), 0, "the chase count starts clear");
        assert_eq!(spider.wasp_collision(), 0, "the web index starts clear");
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room101_zombies_initialise_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("101").unwrap();

    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let zombies: Vec<Entity> = first
        .game
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && matches!(entity.id, 0x00 | 0x01 | 0x11))
        .copied()
        .collect();
    assert!(
        !zombies.is_empty(),
        "the 1F dining room spawns the acceptance zombies"
    );
    for zombie in &zombies {
        assert!(zombie.state() >= 1, "the script's init ran");
        assert!(
            (17..=99).contains(&zombie.health),
            "the script rolled 17..99 health: {}",
            zombie.health
        );
        assert!(
            (1..=4).contains(&zombie.hit_threshold),
            "the threshold table rolled 1..4: {}",
            zombie.hit_threshold
        );
        assert!(
            (3..=5).contains(&zombie.stagger_timer),
            "the stagger table rolled 3..5: {}",
            zombie.stagger_timer
        );
        let radius = if zombie.id == 0x01 { 322 } else { 422 };
        assert_eq!(zombie.sca_radius, radius, "the per-id SCA record");
        assert_eq!(zombie.sca_half_height, 1530);
        assert_eq!(zombie.sca_offset, [0, -1530, 0]);
        assert_eq!((zombie.shadow_half_x, zombie.shadow_half_z), (700, 900));
        assert_eq!(zombie.shadow_tint, 0x0080_8080);
        assert_eq!(zombie.move_speed_byte, 45);
        assert_eq!(zombie.turn_speed, 24);
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1140_hounds_initialise_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("1140").unwrap();
    let model = pack
        .read(&format!("enemy/em02{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert!(
        model.skeleton.relative.len() >= 5,
        "the hound model carries the reach/blood joint anchors"
    );

    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let hounds: Vec<Entity> = first
        .game
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && entity.id == 0x02)
        .copied()
        .collect();
    assert_eq!(hounds.len(), 2, "ROOM1140 spawns the window hounds");
    for hound in &hounds {
        assert_eq!(hound.state(), 1, "init moved the hound to the driver");
        assert!(
            (59..=122).contains(&hound.health),
            "the script rolled 59..122 health: {}",
            hound.health
        );
        assert_eq!(hound.sca_radius, 400);
        assert_eq!(hound.sca_half_height, 800);
        assert_eq!(hound.sca_offset, [500, -800, 0]);
        assert_eq!((hound.shadow_half_x, hound.shadow_half_z), (0x550, 0x200));
        assert_eq!(hound.shadow_tint, 0x0080_8080);
        assert_eq!(hound.cb_behflags(), 1, "the run bit is armed");
        assert_eq!(hound.cb_aiflags(), 0x80, "ACTIVE is armed");
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room50f0_chimeras_initialise_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("50F0").unwrap();
    let model = pack
        .read(&format!("enemy/em09{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert!(
        model.skeleton.relative.len() >= 11,
        "the chimera model carries the claw and torso anchors"
    );

    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let chimeras: Vec<Entity> = first
        .game
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && entity.id == 0x09)
        .copied()
        .collect();
    assert_eq!(chimeras.len(), 3, "ROOM50F0 spawns three ceiling chimeras");
    for chimera in &chimeras {
        assert_eq!(chimera.state(), 1, "init moved the chimera to the driver");
        assert_eq!(chimera.behavior_flags, 2, "the ceiling variant");
        assert_eq!(chimera.pos[1], -6008, "parked on the ceiling");
        assert_eq!(chimera.roll, 0x800, "the upside-down pose");
        assert!(
            (80..=122).contains(&chimera.health),
            "the script rolled 80..122 health: {}",
            chimera.health
        );
        assert_eq!(chimera.sca_radius, 500);
        assert_eq!(chimera.sca_half_height, 180);
        assert_eq!(chimera.sca_offset, [0, -180, 0]);
        assert_eq!(chimera.shadow_tint, 0x0080_8080);
        assert_eq!(chimera.c_fade_freeze(), 0, "the dissolve has not frozen it");
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room30a0_hunters_initialise_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("30A0").unwrap();
    let model = pack
        .read(&format!("enemy/em06{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert!(
        model.skeleton.relative.len() >= 16,
        "the hunter model carries the mouth and death chains"
    );

    // The Enrico room boots its two scripted-appearance hunters (the pair is
    // selected by the room's scenario flag): the walk-in kind with the
    // intro-parked action layer, and the skip kind.
    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let hunters: Vec<Entity> = first
        .game
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && entity.id == 0x06)
        .copied()
        .collect();
    assert_eq!(hunters.len(), 2, "ROOM30A0 spawns the scripted pair");
    for hunter in &hunters {
        assert_eq!(hunter.state(), 1, "init moved the hunter to the AI driver");
        assert!(
            (79..=111).contains(&hunter.health),
            "the script rolled 79..111 health: {}",
            hunter.health
        );
        assert_eq!(hunter.sca_radius, 900, "the mansion-stage record");
        assert_eq!(hunter.sca_half_height, 180);
        assert_eq!(hunter.sca_offset, [0, -180, 0]);
        assert_eq!(hunter.shadow_tint, 0x0080_8080);
        assert_eq!((hunter.shadow_half_x, hunter.shadow_half_z), (1000, 1000));
        assert_eq!(
            hunter.behavior_flags & 0x80,
            0x80,
            "both carry the scripted skip bit"
        );
    }
    let walk_in = hunters
        .iter()
        .find(|hunter| hunter.behavior_flags & 0x08 != 0)
        .expect("the scripted walk-in kind");
    assert_eq!(walk_in.ignore(), 1, "the intro parks the action layer");
    assert_eq!(walk_in.action_behavior, 0x0B);
    let skipped = hunters
        .iter()
        .find(|hunter| hunter.behavior_flags & 0x08 == 0)
        .expect("the skip kind");
    assert_eq!(
        skipped.ignore(),
        0,
        "the skip kind never reaches a behaviour"
    );
    assert_eq!(skipped.action_behavior, 0);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room10c0_monster_plants_initialise_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("10C0").unwrap();
    let model = pack
        .read(&format!("enemy/em0f{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert_eq!(
        model.skeleton.relative.len(),
        15,
        "the vine's fifteen segments are the reveal/hide table"
    );

    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let plants: Vec<Entity> = first
        .game
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && entity.id == 0x0f)
        .copied()
        .collect();
    assert_eq!(plants.len(), 6, "ROOM10C0 spawns the six-vine plant bed");
    for plant in &plants {
        assert_eq!(plant.state(), 1, "init moved the plant to the state check");
        assert_eq!(plant.health, 1, "the awake plant is immortal at one");
        assert_eq!(plant.sca_radius, 0x0190);
        assert_eq!(plant.sca_half_height, 0);
        assert_eq!(plant.sca_offset, [0, 0, 0]);
        assert_eq!(plant.shadow_half_x, 500);
        assert_eq!(plant.shadow_half_z, 100);
        assert_eq!(plant.shadow_tint, 0x0040_4040);
        assert_eq!(plant.mp_shadow(), 1, "the awake plant casts a shadow");
        assert!(
            plant.behavior_flags & 2 == 0,
            "ROOM10C0's vines are not the sprung spawn kind"
        );
        assert_eq!(plant.mp_hits(), 0);
        assert_eq!(plant.mp_alerted(), 1);
        assert_eq!(
            plant.joint_flags & 0x7FFF,
            0,
            "an awake vine starts fully revealed"
        );
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room5060_computer_arms_initialise_through_the_script() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("5060").unwrap();
    for arm_id in [0x14u8, 0x15] {
        let model = pack
            .read(&format!("enemy/em{arm_id:02x}{}.emd", id.player_flag & 1))
            .unwrap();
        assert!(arklay::emd::parse(model).is_ok(), "the arm model parses");
    }

    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let right = entity_with_id(&first.game, 0x14).expect("ROOM5060 spawns the right arm");
    let left = entity_with_id(&first.game, 0x15).expect("ROOM5060 spawns the left arm");
    assert_eq!(right.0, 2, "the right arm is enemy slot 1");
    assert_eq!(
        left.0, 1,
        "the left arm is slot 0, the one the terminal drives"
    );
    for (_, arm) in [right, left] {
        assert_eq!(
            arm.state(),
            1,
            "the one-shot init handed over to the driver"
        );
        assert_eq!(arm.health, 1);
        assert_ne!(arm.status_flags & 4, 0, "the init arms the intangible bit");
        assert_eq!(arm.behavior_flags & 0x80, 0x80, "both arms spawn parked");
        assert_eq!(arm.has_enter_switch_zone, 0, "a parked arm leaves the zone");
        assert_eq!(arm.arm_pos_x(), arm.pos[0] << 16, "the 16.16 home");
        assert_eq!(arm.arm_pos_z(), arm.pos[2] << 16);
        assert_eq!(arm.ignore(), 0);
        assert_eq!(arm.action_behavior, 0);
    }
    let right = first.game.entities[2];
    let left = first.game.entities[1];
    assert_eq!(
        right.target[1], 0,
        "the right arm does not record a spawn Y"
    );
    assert_eq!(
        left.target[1], -1242,
        "the left arm records the spawn height its command-7 drop falls to"
    );
}

/// Boot a cut-less stub room (the attic and lab entries the pack carries
/// without camera backgrounds) far enough to run the monster scripts: the
/// census's boot path plus a few ticks of every active monster slot.
fn boot_and_tick(pack: &Pack, id: RoomId, ticks: usize) -> arklay::game::GameState {
    use arklay::game::{ENTITY_COUNT, GameState, ScdGameHost};
    use arklay::scd;
    use arklay::scd::vm::CommandVm;

    let data = pack.read(&id.rdt_entry()).unwrap();
    let room = arklay::rdt::parse(data, id).unwrap();
    let scripts = scd::reader::parse(data).unwrap();
    let mut game = GameState::new(id, &room);
    {
        let mut vm = CommandVm::new(&scripts);
        let mut game_host = ScdGameHost::new(&mut game);
        vm.run_init(&mut game_host);
    }
    let mut models = arklay::enemy::EntityModelCache::default();
    let mut lua = arklay::enemy::LuaEnemyHost::new();
    let room_state = arklay::state::RoomState::default();
    for _ in 0..ticks {
        for slot in 1..ENTITY_COUNT {
            if !game.entities[slot].active() {
                continue;
            }
            let entity_id = game.entities[slot].id;
            if entity_id >= arklay::enemy::CHARACTER_ID_MIN {
                continue;
            }
            let player = id.player_flag & 1;
            let model = models.get(pack, entity_id, player);
            if let Some(keyframes) = models.keyframes(pack, entity_id, player) {
                game.entity_anims[slot].keyframes = Some(keyframes);
            }
            if let Some(skeleton) = models.skeleton(pack, entity_id, player) {
                game.entity_anims[slot].skeleton = Some(skeleton);
            }
            let clips: &[arklay::model::Clip] = model.as_ref().map_or(&[], |model| &model.clips);
            lua.update(&mut game, &room_state, pack, slot, clips);
        }
    }
    assert_eq!(lua.failed_scripts(), 0, "no script errored");
    game
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room210_yawn_initialises_with_its_twelve_segments() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("210").unwrap();
    let model = pack
        .read(&format!("enemy/em0d{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert_eq!(
        model.skeleton.relative.len(),
        15,
        "Yawn's fifteen joints are the body chain"
    );

    // The attic boots the parked scripted entrance: hidden, waiting for the
    // room's trigger to clear the park bit.
    let first = boot_and_tick(&pack, id, 4);
    let second = boot_and_tick(&pack, id, 4);
    assert_eq!(
        first.entities, second.entities,
        "the run differs between boots"
    );
    assert_eq!(first.rand_state, second.rand_state);

    let head = first
        .entities
        .iter()
        .skip(1)
        .find(|entity| entity.active() && entity.id == 0x0d && entity.behavior_flags != 1)
        .copied()
        .expect("ROOM210 spawns the Yawn head");
    assert!(head.state() >= 1, "the script's init ran");
    assert_eq!(head.health, 0x0BEA, "the first fight's health");
    assert_eq!(head.sca_radius, 800);
    assert_eq!(head.sca_half_height, 2000);
    assert_eq!((head.shadow_half_x, head.shadow_half_z), (1000, 1000));
    assert_eq!(head.shadow_tint, 0x0080_8080);
    assert_eq!(head.status_flags & 4, 4, "the scripted entrance is hidden");
    assert_eq!(head.yawn_form(), 0, "juvenile until the emerge");

    let segments = first
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && entity.id == 0x0d && entity.behavior_flags == 1)
        .count();
    assert_eq!(segments, 12, "the twelve body slots");
    assert_eq!(first.enemy_count, 13, "head plus body");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room70c_rematch_yawn_initialises() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("70c").unwrap();
    let model = pack
        .read(&format!("enemy/em12{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert_eq!(model.skeleton.relative.len(), 15);

    let first = boot_and_tick(&pack, id, 4);
    let second = boot_and_tick(&pack, id, 4);
    assert_eq!(
        first.entities, second.entities,
        "the run differs between boots"
    );

    let head = first
        .entities
        .iter()
        .skip(1)
        .find(|entity| entity.active() && entity.id == 0x12 && entity.behavior_flags != 1)
        .copied()
        .expect("ROOM70C spawns the rematch Yawn");
    assert!(head.state() >= 1);
    assert!(
        [0x012C, 0x0190].contains(&head.health),
        "the rematch health table: {:#x}",
        head.health
    );
    let segments = first
        .entities
        .iter()
        .skip(1)
        .filter(|entity| entity.active() && entity.id == 0x12 && entity.behavior_flags == 1)
        .count();
    assert_eq!(segments, 12);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room40c0_plant42_initialises_with_its_companions() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("40c0").unwrap();
    let model = pack
        .read(&format!("enemy/em08{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert!(
        model.skeleton.relative.len() >= 16,
        "the plant model carries its sixteen joints plus the companion objects"
    );
    assert!(
        model.mesh.objects.len() >= 18,
        "objects 16/17 are the flower body and the root ball"
    );

    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let plants: Vec<(usize, Entity)> = first
        .game
        .entities
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, entity)| entity.active() && entity.id == 0x08)
        .map(|(slot, entity)| (slot, *entity))
        .collect();
    assert!(!plants.is_empty(), "ROOM40C0 spawns Plant 42");
    let boss = plants
        .iter()
        .find(|(_, entity)| entity.behavior_flags & 1 == 0)
        .map(|(_, entity)| *entity)
        .expect("the boss record");
    assert_eq!(boss.health, 140, "the boss pool");
    assert_eq!(boss.sca_radius, 2000, "the full-height record");
    let body = boss.plant42_body.expect("the flower body spawned");
    let roots = boss.plant42_roots.expect("the root ball spawned");
    let body = first.game.companions[usize::from(body)]
        .as_ref()
        .expect("the body is live");
    assert_eq!(body.kind, arklay::enemy::companion::CompanionKind::Body);
    assert_eq!(body.vine_pool, 4, "the shared vine pool");
    assert_eq!(body.entity.shadow_tint, 0x0060_6060);
    let roots = first.game.companions[usize::from(roots)]
        .as_ref()
        .expect("the roots are live");
    assert_eq!(roots.kind, arklay::enemy::companion::CompanionKind::Roots);
    assert_eq!(first.game.plant42_shared_body, boss.plant42_body);

    for (_, split) in plants
        .iter()
        .filter(|(_, entity)| entity.behavior_flags & 1 != 0)
    {
        assert_eq!(split.sca_radius, 200, "the split-vine record");
        assert_eq!(
            split.plant42_body, None,
            "the split vine shares the boss body"
        );
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room5130_lab_tyrant_floats_in_the_pod() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("5130").unwrap();
    let model = pack
        .read(&format!("enemy/em0c{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert_eq!(
        model.skeleton.relative.len(),
        15,
        "the Tyrant's fifteen joints"
    );
    assert_eq!(
        model.mesh.objects.len(),
        16,
        "object 15 is the exposed heart the clone draws"
    );

    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let boss = first
        .game
        .entities
        .iter()
        .skip(1)
        .find(|entity| entity.active() && entity.id == 0x0c)
        .copied()
        .expect("ROOM5130 spawns the lab Tyrant");
    assert_eq!(boss.behavior_flags, 0x40, "the room hands it to the SCD");
    assert_eq!(boss.state(), 8, "the pod state");
    assert_eq!(boss.action_behavior, 0x0B, "the stasis-pod behaviour");
    assert_eq!(boss.health, 0xDC, "the lab pool");
    assert_eq!(boss.sca_radius, 800);
    assert_eq!(boss.sca_half_height, 2000);
    assert_eq!(boss.sca_offset, [0, -2000, 0]);
    assert_eq!((boss.shadow_half_x, boss.shadow_half_z), (1000, 1000));
    assert_eq!(boss.shadow_tint, 0x0080_8080);
    assert_eq!(boss.ty_cooldown(), 0xD2);
    assert_eq!(boss.look_at_flags, 2);
    assert_eq!(boss.look_at_joint, 2);
    assert_eq!(boss.pos[1], -200, "floating in the capsule");
    let heart = boss.tyrant_heart.expect("the exposed heart clone spawned");
    let heart = first.game.companions[usize::from(heart)]
        .as_ref()
        .expect("the heart is live");
    assert_eq!(heart.kind, arklay::enemy::companion::CompanionKind::Heart);

    // The paired variant's init is identical.
    let variant = RoomId::parse("5131").unwrap();
    let other = arklay::engine::simulate_room(&pack, variant, 4, player::Input::default()).unwrap();
    let other_boss = other
        .game
        .entities
        .iter()
        .skip(1)
        .find(|entity| entity.active() && entity.id == 0x0c)
        .copied()
        .expect("ROOM5131 spawns the same Tyrant");
    assert_eq!(other_boss.state(), 8);
    assert_eq!(other_boss.health, 0xDC);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room3030_heliport_tyrant_waits_suspended() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("3030").unwrap();
    let model = pack
        .read(&format!("enemy/em10{}.emd", id.player_flag & 1))
        .unwrap();
    let model = arklay::emd::parse(model).unwrap();
    assert_eq!(model.skeleton.relative.len(), 15);
    assert_eq!(model.mesh.objects.len(), 16);

    let first = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    let second = arklay::engine::simulate_room(&pack, id, 4, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let boss = first
        .game
        .entities
        .iter()
        .skip(1)
        .find(|entity| entity.active() && entity.id == 0x10)
        .copied()
        .expect("ROOM3030 spawns the heliport Tyrant");
    assert_eq!(
        boss.behavior_flags, 0x80,
        "parked until the script wakes it"
    );
    assert_eq!(boss.state(), 1);
    assert_eq!(boss.ignore(), 1);
    assert_eq!(boss.action_behavior, 9, "the eruption entrance is armed");
    assert_eq!(boss.health, 600, "the heliport pool");
    assert!(
        first.game.tyrant.trail.allocated,
        "the ribbon pool is reserved"
    );
    assert_eq!(boss.ty_repause(), 0x5A);
    let heart = boss.tyrant_heart.expect("the exposed heart clone spawned");
    assert!(first.game.companions[usize::from(heart)].is_some());
}
