//! Real-asset stairs tests: the in-room `stairs_height_update` ramp, the
//! `set_stairs_zone` ladder entry and the stairwell doors that run the climb.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test stairs_real -- --ignored`
//!
//! Only an unset `ARKLAY_RE1_PACK` skips. The tick helper mirrors the engine's
//! gameplay order: movement first, then the room action probe and the stair
//! state hand-off, exactly as `engine::tick_room` does.

mod common;

use std::path::PathBuf;

use arklay::anim;
use arklay::bmp;
use arklay::door;
use arklay::emd;
use arklay::engine::simulate_door;
use arklay::game::{GameState, RoomActionKind, ScdGameHost};
use arklay::pack::Pack;
use arklay::player::{self, ClipSource, Input, PlayerState};
use arklay::rdt;
use arklay::render::{self, Camera, Framebuffer, Lighting, PlayerMesh};
use arklay::scd::vm::CommandVm;
use arklay::state::{Image, RoomId, RoomState};

fn pack_path() -> Option<PathBuf> {
    Some(common::asset_env()?.1)
}

/// Load `id`, run its init script and return the room and game state.
fn load(pack: &Pack, id: RoomId) -> (RoomState, GameState) {
    let bytes = pack.read(&id.rdt_entry()).unwrap();
    let room = rdt::parse(bytes, id).unwrap();
    let scripts = arklay::scd::reader::parse(bytes).unwrap();
    let mut game = GameState::new(id, &room);
    let mut vm = CommandVm::new(&scripts);
    let mut host = ScdGameHost::new(&mut game);
    vm.run_init(&mut host);
    (room, game)
}

/// One engine-ordered gameplay tick: move, probe the room actions, apply the
/// stair state.
fn tick(
    player: &mut PlayerState,
    room: &RoomState,
    game: &mut GameState,
    input: Input,
    action: bool,
) {
    player::update(player, room, &[], &[], input);
    game.sync_entity_from_player(player);
    game.interact(player.pos, player.angle, action);
    game.apply_stair_state(player);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn lab_stairway_ramps_height_up_and_down() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let id = RoomId::parse("40D0").unwrap();
    let (room, mut game) = load(&pack, id);

    // ROOM40D0's `aot_set` slot 4: zone [17700, 2000, 7000, 3800], own-position
    // probe, high-X edge, length 175, step 36.
    let ramp = game
        .room_actions
        .iter()
        .flatten()
        .find(|action| action.kind == RoomActionKind::StairsHeight)
        .expect("stairs_height_update zone");
    assert_eq!(ramp.zone, [17700, 2000, 7000, 3800]);
    assert_eq!(ramp.param_word(0), 1, "high-X edge");
    assert_eq!(ramp.param_word(1), 175);
    assert_eq!(ramp.param_word(2) as i16, 36);

    let mut player = player::spawn(id, &room);
    player.pos = [24000, 0, 3600];
    player.angle = 0x800; // west, up the ramp
    game.sync_entity_from_player(&player);
    assert!(!player::position_blocked(&room, player.pos, player.radius));

    let start = player.pos;
    let mut last = start;
    for _ in 0..40 {
        tick(
            &mut player,
            &room,
            &mut game,
            Input {
                up: true,
                ..Input::default()
            },
            false,
        );
        assert!(
            player.pos[0] < last[0],
            "walking west must progress: {last:?} -> {:?}",
            player.pos
        );
        assert!(
            player.pos[1] >= last[1],
            "height must not drop while climbing: {last:?} -> {:?}",
            player.pos
        );
        assert_eq!(player.stairs.height, game.stair_height);
        last = player.pos;
    }
    assert!(
        last[1] > start[1] + 400,
        "expected a real climb: {start:?} -> {last:?}"
    );
    assert!(game.stair_height.is_some());
    let top = last;

    // Walk back down: the ramp lowers the player as XZ progresses east.
    player.angle = 0;
    let mut last = top;
    for _ in 0..40 {
        tick(
            &mut player,
            &room,
            &mut game,
            Input {
                up: true,
                ..Input::default()
            },
            false,
        );
        assert!(
            player.pos[0] > last[0],
            "walking east must progress: {last:?} -> {:?}",
            player.pos
        );
        assert!(
            player.pos[1] <= last[1],
            "height must not rise while descending: {last:?} -> {:?}",
            player.pos
        );
        last = player.pos;
    }
    assert!(
        last[1] < top[1] - 400,
        "expected a real descent: {top:?} -> {last:?}"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn lab_ladder_zone_latches_flags_and_base() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let id = RoomId::parse("3010").unwrap();
    // The lab ladders only exist while scenario flag 47 (the powered lab) is
    // set; with it clear the init rewrites both slots into messages.
    let bytes = pack.read(&id.rdt_entry()).unwrap();
    let room = rdt::parse(bytes, id).unwrap();
    let scripts = arklay::scd::reader::parse(bytes).unwrap();
    let mut game = GameState::new(id, &room);
    assert!(game.apply_flag(0, 47, 0), "scenario flag 47");
    let mut vm = CommandVm::new(&scripts);
    let mut host = ScdGameHost::new(&mut game);
    vm.run_init(&mut host);

    // ROOM3010's `aot_set` slot 2: the first ladder, variant word 1, base
    // (0x56EA, 0x2BD7) = (22250, 11223).
    let entry = game
        .room_actions
        .iter()
        .flatten()
        .find(|action| action.kind == RoomActionKind::StairsZone)
        .expect("set_stairs_zone entry");
    assert_eq!(entry.zone, [21500, 10323, 1500, 1800]);
    assert_eq!(entry.flags, 0x81, "action-key probe");
    assert_eq!(entry.param_word(0), 1, "ladder variant");
    assert_eq!(entry.param_word(1), 0x56EA);
    assert_eq!(entry.param_word(2), 0x2BD7);

    // The ladder shaft itself sits inside the room collision volume, so the
    // player reaches into it from the free ledge to the north: stand at
    // (22200, 10250) facing south and let the 600-unit probe enter the zone.
    let mut player = player::spawn(id, &room);
    player.pos = [22200, 0, 10250];
    player.angle = 0xC00;
    game.sync_entity_from_player(&player);
    assert!(!player::position_blocked(&room, player.pos, player.radius));
    tick(&mut player, &room, &mut game, Input::default(), true);

    let latched = game.stair_entry.expect("ladder entry latched");
    assert_eq!(latched.slot, 2);
    assert!(latched.ladder);
    assert_eq!((latched.base_x, latched.base_z), (0x56EA, 0x2BD7));
    assert_eq!(game.entities[0].zone_flags & 0x30, 0x30);
    assert_eq!(game.entities[0].unk_c6, 0x56EA);
    assert_eq!(game.entities[0].unk_c8, 0x2BD7);
    assert!(game.ladder_down, "main_state_flags bit 4");
    assert!(player.stairs.climbing);
    assert!(player.stairs.in_zone);
    assert_eq!(player.stairs.base, [0x56EA, 0x2BD7]);

    // Reaching the base releases the climb. The 8-state ladder animation is a
    // later milestone, so only the state hand-off is asserted here.
    player.pos = [22250, 0, 11223];
    game.sync_entity_from_player(&player);
    tick(&mut player, &room, &mut game, Input::default(), false);
    assert!(!game.stair_climb);
    assert!(!player.stairs.climbing);
    assert!(player.stairs.in_zone, "the zone flag itself persists");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn main_hall_stairwell_door_runs_the_climb_and_lands_in_106() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let id = RoomId::parse("2030").unwrap();
    let (room, mut game) = load(&pack, id);

    // ROOM2030 slot 0 is the main-hall staircase, `kai01` (type 0x16).
    let door = game.doors[0].expect("main hall stair door");
    assert_eq!(door.door_type, 0x16);
    assert!(door::is_stair_type(door.door_type));

    let mut player = player::spawn(id, &room);
    player.pos = [17000, 0, 24000];
    player.angle = 0x400; // north, facing the staircase
    game.sync_entity_from_player(&player);
    tick(&mut player, &room, &mut game, Input::default(), true);

    let transition = game.transition.expect("stair transition");
    assert_eq!(transition.target, RoomId::parse("1060").unwrap());
    let entry = game.stair_entry.expect("climb entry");
    assert!(!entry.ladder);
    assert!(game.stair_climb);
    assert_eq!(game.entities[0].zone_flags & 0x20, 0x20);

    // The full animation reaches the 1F main hall.
    let sim = simulate_door(&pack, id, 0, None).unwrap();
    assert_eq!(sim.target, RoomId::parse("1060").unwrap());
    assert!(
        !player::position_blocked(&sim.room, sim.player.pos, sim.player.radius),
        "the 106 arrival must be free: {:?}",
        sim.player.pos
    );
    assert!(sim.gameplay_frame.rgba.iter().any(|&byte| byte != 0));

    // The return stair from 106 lands in 203 at the record's entry point.
    let back = simulate_door(&pack, RoomId::parse("1060").unwrap(), 4, None).unwrap();
    assert_eq!(back.target, RoomId::parse("2030").unwrap());
    assert_eq!(back.player.pos, [17100, 0, 25300]);
    assert!(!player::position_blocked(
        &back.room,
        back.player.pos,
        back.player.radius
    ));
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn west_stairwell_101_to_201_marks_the_climb() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let id = RoomId::parse("1010").unwrap();
    let (room, mut game) = load(&pack, id);

    // ROOM1010 slot 3 is the west staircase, `kai02` (type 0x18).
    let door = game.doors[3].expect("west stair door");
    assert!(
        door::is_stair_type(door.door_type),
        "type {:#x}",
        door.door_type
    );

    let mut player = player::spawn(id, &room);
    let center_x = i32::from(door.zone[0]) + i32::from(door.zone[2]) / 2;
    let center_z = i32::from(door.zone[1]) + i32::from(door.zone[3]) / 2;
    let (dx, dz) = player::reach_offset(0);
    player.pos = [center_x - dx, 0, center_z - dz];
    player.angle = 0;
    game.sync_entity_from_player(&player);
    tick(&mut player, &room, &mut game, Input::default(), true);
    assert!(game.transition.is_some());
    assert!(game.stair_climb);

    // The destination placement still settles free (the raw arrival sits
    // inside the stairwell collision, so `free_spawn` remains the fallback).
    let sim = simulate_door(&pack, id, 3, None).unwrap();
    assert_eq!(sim.target, RoomId::parse("2010").unwrap());
    assert!(!player::position_blocked(
        &sim.room,
        sim.player.pos,
        sim.player.radius
    ));
    assert!(sim.gameplay_frame.rgba.iter().any(|&byte| byte != 0));
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn capture_player_partway_up_the_lab_stairway() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let id = RoomId::parse("40D0").unwrap();
    let (mut room, mut game) = load(&pack, id);
    for cut in &mut room.cuts {
        let entry = id.cut_entry(cut.index);
        cut.background = Some(bmp::decode(pack.read(&entry).unwrap()).unwrap());
    }

    let emd = emd::parse(pack.read("player/00.emd").unwrap()).unwrap();
    let emw = emd::parse_emw(pack.read("player/00.emw").unwrap()).unwrap();
    let mut player = player::spawn(id, &room);
    // Inside the camera-switch zone that selects the stairway cut (7) and on
    // the ramp's lane.
    player.pos = [19500, 0, 3600];
    player.angle = 0x800; // west, up the ramp
    game.sync_entity_from_player(&player);
    for _ in 0..12 {
        player::update(
            &mut player,
            &room,
            &emd.clips,
            &emw.clips,
            Input {
                up: true,
                ..Input::default()
            },
        );
        game.sync_entity_from_player(&player);
        game.interact(player.pos, player.angle, false);
        game.apply_stair_state(&mut player);
    }
    assert!(
        player.pos[1] > 300,
        "expected the player partway up the stairway: {:?}",
        player.pos
    );

    // Render with the real player model on the zone-selected cut.
    let cut_index = player::camera_for_position(&room, 0, player.pos);
    let cut = &room.cuts[cut_index];
    let camera = Camera::from_cut(cut);
    let lighting = Lighting::from_room(&room);
    let entity = anim::entity_matrix(player.pos, player.angle);
    let (keyframes, clips) = match player.clip_source {
        ClipSource::Emd => (&emd.keyframes, &emd.clips),
        ClipSource::Emw => (&emw.keyframes, &emw.clips),
    };
    let keyframe = &keyframes[player.anim.keyframe_index(clips)];
    let joints = anim::joint_matrices(&emd.skeleton, keyframe, &entity);
    let mesh = PlayerMesh {
        mesh: &emd.mesh,
        texture: &emd.texture,
        joints: &joints,
    };
    let mut framebuffer = Framebuffer::new();
    render::draw_gameplay_scene(
        &mut framebuffer,
        cut.background.as_ref(),
        Some(&mesh),
        &camera,
        &lighting,
        None,
    );

    let background = cut.background.as_ref().expect("background");
    let changed = framebuffer
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(background.rgba.as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    assert!(changed > 200, "player model covered only {changed} pixels");

    let dir = std::env::temp_dir().join("arklay-stairs-real");
    std::fs::create_dir_all(&dir).unwrap();
    let image = Image {
        width: framebuffer.width,
        height: framebuffer.height,
        rgba: framebuffer.rgba.clone(),
    };
    let capture = dir.join("lab_stairway_partway.bmp");
    bmp::encode(&image, &capture).unwrap();
    assert!(capture.is_file(), "no capture at {}", capture.display());

    // Deterministic: the same drive reaches the same position.
    let (repeat_room, mut repeat_game) = load(&pack, id);
    let mut repeat_player = player::spawn(id, &repeat_room);
    repeat_player.pos = [19500, 0, 3600];
    repeat_player.angle = 0x800;
    repeat_game.sync_entity_from_player(&repeat_player);
    for _ in 0..12 {
        player::update(
            &mut repeat_player,
            &repeat_room,
            &emd.clips,
            &emw.clips,
            Input {
                up: true,
                ..Input::default()
            },
        );
        repeat_game.sync_entity_from_player(&repeat_player);
        repeat_game.interact(repeat_player.pos, repeat_player.angle, false);
        repeat_game.apply_stair_state(&mut repeat_player);
    }
    assert_eq!(
        repeat_player.pos, player.pos,
        "the stair drive is deterministic"
    );
}
