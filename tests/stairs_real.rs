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
use arklay::render::{self, Camera, EntityMesh, Framebuffer, Lighting};
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

/// One engine-ordered tick with the real player clips and the object pass, in
/// the engine's order: player, room probe, objects, stair hand-off. The
/// action-key probe is skipped while a locked behaviour owns the tick, exactly
/// like `engine::tick_room`.
fn ladder_tick(
    player: &mut PlayerState,
    room: &RoomState,
    game: &mut GameState,
    emd_clips: &[arklay::model::Clip],
    emw_clips: &[arklay::model::Clip],
    input: Input,
    action: bool,
) {
    let room_clips = room
        .room_anim
        .as_ref()
        .map(|anim| anim.clips.as_slice())
        .unwrap_or(&[]);
    player::update_with_room(player, room, emd_clips, emw_clips, room_clips, input);
    game.sync_entity_from_player(player);
    let probe_action = action && player.locked == player::LockedAction::None;
    game.interact(player.pos, player.angle, probe_action);
    game.tick_objects(room, player);
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
    assert!(!player::position_blocked(
        &room,
        player.pos,
        player.radius,
        player.collision_flags,
    ));

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
    assert!(!player::position_blocked(
        &room,
        player.pos,
        player.radius,
        player.collision_flags,
    ));

    // Walking into the zone does not mark it: the entry is an action-key
    // probe, so only a press runs `set_stairs_zone`.
    tick(&mut player, &room, &mut game, Input::default(), false);
    assert!(game.stair_entry.is_none());
    assert_eq!(game.entities[0].zone_flags & 0x20, 0);

    // The press marks the zone and the object pass starts the climb.
    ladder_tick(
        &mut player,
        &room,
        &mut game,
        &[],
        &[],
        Input {
            action_pressed: true,
            ..Input::default()
        },
        true,
    );
    let latched = game.stair_entry.expect("ladder entry latched");
    assert_eq!(latched.slot, 2);
    assert!(latched.ladder);
    assert_eq!((latched.base_x, latched.base_z), (0x56EA, 0x2BD7));
    // The variant bit and the in-zone mark both survive: the object-side
    // mask-4 pass clears the object record's own zone bit, not the player's.
    assert_eq!(game.entities[0].zone_flags & 0x30, 0x30);
    assert_eq!(game.entities[0].unk_c6, 0x56EA);
    assert_eq!(game.entities[0].unk_c8, 0x2BD7);
    assert!(game.ladder_down(), "main_state_flags bit 4");
    assert!(!game.stair_climb, "the ladder climb is a player behaviour");
    assert_eq!(player.locked, player::LockedAction::Ladder);
    assert_eq!(player.stairs.base, [0x56EA, 0x2BD7]);
    // The entry's own low word toggled, so the next press flips the end.
    assert_eq!(game.room_actions[2].unwrap().param_word(0), 0);
}

/// The per-tick `(state, display frame, position)` log of one ladder leg.
type LadderLog = Vec<(u8, usize, [i32; 3])>;

/// Drive one ladder leg from `start_angle` (`x`, `y`, `z`, `angle`): press
/// action on the first tick and tick until control returns. Returns the queued
/// sound ids, the recorded screen effects and the per-tick log.
fn drive_ladder(
    player: &mut PlayerState,
    room: &RoomState,
    game: &mut GameState,
    emd_clips: &[arklay::model::Clip],
    emw_clips: &[arklay::model::Clip],
    start_angle: [i32; 4],
    ticks: usize,
) -> (Vec<u16>, Vec<player::ScreenEffect>, LadderLog) {
    player.pos = [start_angle[0], start_angle[1], start_angle[2]];
    player.angle = start_angle[3] as u16;
    game.sync_entity_from_player(player);
    let mut sounds = Vec::new();
    let mut effects = Vec::new();
    let mut log = Vec::new();
    for tick in 0..ticks {
        let pressed = tick == 0;
        let input = Input {
            action_pressed: pressed,
            ..Input::default()
        };
        ladder_tick(player, room, game, emd_clips, emw_clips, input, pressed);
        sounds.extend(player.take_sounds().iter().map(|sound| sound.id));
        effects.extend(player.take_screen_effects());
        log.push((player.action_state, player.anim.display_frame, player.pos));
        if tick > 0 && player.locked == player::LockedAction::None {
            break;
        }
    }
    (sounds, effects, log)
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_301_ladder_climbs_the_shaft_both_ways() {
    let Some(path) = pack_path() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let id = RoomId::parse("3010").unwrap();
    let bytes = pack.read(&id.rdt_entry()).unwrap();
    let room = rdt::parse(bytes, id).unwrap();
    let scripts = arklay::scd::reader::parse(bytes).unwrap();
    let mut game = GameState::new(id, &room);
    assert!(game.apply_flag(0, 47, 0), "scenario flag 47");
    {
        let mut vm = CommandVm::new(&scripts);
        let mut host = ScdGameHost::new(&mut game);
        vm.run_init(&mut host);
    }
    assert!(room.room_anim.is_some(), "ROOM301 room animation pair");
    let emd = emd::parse(pack.read("player/00.emd").unwrap()).unwrap();
    let emw = emd::parse_emw(pack.read("player/00.emw").unwrap()).unwrap();
    let mut player = player::spawn(id, &room);

    // Bottom leg: slot 2's variant zone starts the 0x35 clip, which shifts the
    // player +0x708 in Z on frame 0x0F, raises the height to 0xA8C and grunts.
    let start = [22200, 0, 10250];
    let (sounds, effects, log) = drive_ladder(
        &mut player,
        &room,
        &mut game,
        &emd.clips,
        &emw.clips,
        [start[0], start[1], start[2], 0xC00],
        400,
    );
    assert!(log.iter().any(|&(state, ..)| state >= 4), "never climbed");
    assert!(log.iter().any(|&(_, frame, _)| frame == 0x0F));
    assert!(
        sounds.contains(&player::SE_LADDER_GRUNT),
        "variant grunt missing: {sounds:?}"
    );
    assert!(
        !sounds.contains(&player::SE_LADDER_STEP),
        "the variant must not play the plain step SE"
    );
    assert_eq!(effects.len(), 2, "two screen-effect writes per climb");
    assert_eq!(
        player.pos[1],
        player::LADDER_VARIANT_HEIGHT,
        "the variant ride holds the upper height"
    );
    assert!(
        player.pos[2] >= start[2] + player::LADDER_VARIANT_SLIDE + player::LADDER_STEP_OFF_VARIANT,
        "variant displacement: {:?}",
        player.pos
    );
    assert_eq!(player.locked, player::LockedAction::None);
    assert!(
        !player.ladder_release,
        "the object pass consumed the release"
    );
    assert!(!game.ladder_down(), "state 8 cleared the ladder mode");
    assert_eq!(game.entities[0].zone_flags & 0x10, 0);
    assert_eq!(
        game.room_actions[2].unwrap().param_word(0),
        0,
        "the entry word toggled"
    );
    println!(
        "ROOM301 bottom (variant 0x35): {start:?} -> {:?}, sounds {sounds:?}, effects {}",
        player.pos,
        effects.len()
    );

    // Top leg: slot 3's plain zone (variant word 0) plays 0x33 with the three
    // step SEs, the frame 0x32 end SE, the -1000 step-off and the walk-away.
    let top = [22200, 0, 26100];
    let (sounds, effects, log) = drive_ladder(
        &mut player,
        &room,
        &mut game,
        &emd.clips,
        &emw.clips,
        [top[0], top[1], top[2], 0x400],
        400,
    );
    assert!(log.iter().any(|&(state, ..)| state >= 4), "never climbed");
    assert!(log.iter().any(|&(_, frame, _)| frame == 0x32));
    let steps = sounds
        .iter()
        .filter(|&&id| id == player::SE_LADDER_STEP)
        .count();
    assert_eq!(steps, 3, "plain step SEs: {sounds:?}");
    assert!(
        sounds.contains(&player::SE_LADDER_END),
        "plain end SE missing: {sounds:?}"
    );
    assert!(
        sounds.contains(&player::SE_FOOTSTEP),
        "walk-away footstep missing: {sounds:?}"
    );
    assert_eq!(effects.len(), 2);
    assert_eq!(player.pos[1], 0, "the plain descent returns to the floor");
    assert!(
        player.pos[2] <= top[2] - player::LADDER_STEP_OFF,
        "plain step-off: {:?}",
        player.pos
    );
    assert_eq!(game.room_actions[3].unwrap().param_word(0), 1);
    assert!(!game.ladder_down());
    println!(
        "ROOM301 top (plain 0x33): {top:?} -> {:?}, sounds {sounds:?}, effects {}",
        player.pos,
        effects.len()
    );
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
        !player::position_blocked(
            &sim.room,
            sim.player.pos,
            sim.player.radius,
            sim.player.collision_flags,
        ),
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
        back.player.radius,
        back.player.collision_flags,
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

    // The destination placement is the raw arrival; the first collision pass
    // settles it (the shape-5 stairwell volume no longer applies to the
    // player, and the shape-3 circle nudges the point).
    let mut sim = simulate_door(&pack, id, 3, None).unwrap();
    assert_eq!(sim.target, RoomId::parse("2010").unwrap());
    assert_eq!(sim.player.pos, [14100, 0, 11500]);
    player::update(&mut sim.player, &sim.room, &[], &[], Input::default());
    assert_eq!(sim.player.pos, [14098, 0, 11442]);
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
        ClipSource::Emd | ClipSource::Room => (&emd.keyframes, &emd.clips),
        ClipSource::Emw => (&emw.keyframes, &emw.clips),
    };
    let keyframe = &keyframes[player.anim.keyframe_index(clips)];
    let joints = anim::joint_matrices(&emd.skeleton, keyframe, &entity);
    let entities = [EntityMesh {
        mesh: &emd.mesh,
        texture: &emd.texture,
        joints: &joints,
        tint: [255; 3],
        blend_weight: None,
        hidden_joints: 0,
    }];
    let mut framebuffer = Framebuffer::new();
    render::draw_gameplay_scene(
        &mut framebuffer,
        cut.background.as_ref(),
        &entities,
        &[],
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
