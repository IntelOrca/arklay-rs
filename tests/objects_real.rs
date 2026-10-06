//! Real-asset tests for the object-model class: parse every shipped RDT's
//! embedded TMD/TIM pairs, build ROOM107's objects from its init script, and
//! follow an effect attached to an object through its transform.

mod common;

use std::path::{Path, PathBuf};

use arklay::pack::Pack;
use arklay::state::RoomId;
use arklay::{game, objects, rdt, scd};

fn root() -> Option<PathBuf> {
    std::env::var("ARKLAY_RE1_ROOT").ok().map(PathBuf::from)
}

fn collect_rdts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rdts(&path, out);
        } else if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".RDT"))
        {
            // The shipped room files are upper-case `ROOM####.RDT`; the
            // two mixed-case leftovers are not part of the audited corpus.
            out.push(path);
        }
    }
}

fn room_id(path: &Path) -> RoomId {
    let name = path
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_ascii_uppercase();
    RoomId::parse(name.trim_start_matches("ROOM")).unwrap()
}

fn jpn_rdt(root: &Path, relative: &str) -> Vec<u8> {
    let path = root.join(relative);
    std::fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn parses_every_embedded_model_pair() {
    let Some(root) = root() else {
        return;
    };
    let mut files = Vec::new();
    collect_rdts(&root.join("JPN"), &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "no RDT files found under {}",
        root.display()
    );

    let mut rooms = 0usize;
    let mut declared_omodels = 0usize;
    let mut declared_items = 0usize;
    let mut decoded_omodels = 0usize;
    let mut decoded_items = 0usize;
    let mut omodel_rooms = 0usize;
    let mut item_rooms = 0usize;
    let mut warnings = 0usize;
    let mut triangles = 0usize;

    for path in &files {
        let data = std::fs::read(path).unwrap();
        if data.len() <= 4 {
            continue;
        }
        let room = rdt::parse(&data, room_id(path))
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        rooms += 1;
        warnings += room.model_warnings.len();
        declared_omodels += usize::from(room.omodel_slot_count);
        declared_items += usize::from(room.item_count);
        decoded_omodels += room.object_models.len();
        decoded_items += room.item_models.len();
        omodel_rooms += usize::from(room.omodel_slot_count > 0);
        item_rooms += usize::from(room.item_count > 0);
        for asset in room.object_models.iter().chain(&room.item_models) {
            let label = format!("{} pair {}", path.display(), asset.pair_index);
            assert_eq!(
                asset.model.objects.len(),
                1,
                "{label}: expected exactly one TMD object"
            );
            let object = &asset.model.objects[0];
            assert!(!object.prims.is_empty(), "{label}: empty primitive list");
            triangles += object
                .prims
                .iter()
                .map(|prim| prim.vertices.len())
                .sum::<usize>()
                / 3;
            assert!(asset.texture.width > 0, "{label}: empty TIM width");
            assert!(asset.texture.height > 0, "{label}: empty TIM height");
            assert!(
                asset
                    .texture
                    .indices
                    .len()
                    .is_multiple_of(asset.texture.width as usize),
                "{label}: index buffer does not match the width"
            );
        }
    }

    println!(
        "parsed {rooms} RDTs: {decoded_omodels}/{declared_omodels} omodel pairs decoded in \
         {omodel_rooms} rooms, {decoded_items}/{declared_items} item pairs decoded in \
         {item_rooms} rooms, {triangles} triangles, {warnings} warnings"
    );
    assert_eq!(rooms, 318);
    assert_eq!(warnings, 0, "malformed embedded model pairs");
    // The declared table sizes from the asset audit. The difference between
    // the declared and decoded counts is the pairs with a null half, which
    // are declared-but-unbuilt rather than malformed.
    assert_eq!(declared_omodels, 394);
    assert_eq!(declared_items, 578);
    assert_eq!(omodel_rooms, 166);
    assert_eq!(item_rooms, 239);
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_107_objects_build_with_expected_fields() {
    let Some(root) = root() else {
        return;
    };
    let data = jpn_rdt(&root, "JPN/STAGE1/ROOM1070.RDT");
    let id = RoomId::parse("1070").unwrap();
    let room = rdt::parse(&data, id).unwrap();
    assert_eq!(room.omodel_slot_count, 2);
    assert!(room.model_warnings.is_empty(), "{:?}", room.model_warnings);

    // Slot 0 is the step ladder (active + climbable, 184 primitives) and slot
    // 1 the shelf (active, not pushable is clear, 84 primitives).
    assert_eq!(room.object_models.len(), 2);
    let ladder = room
        .object_models
        .iter()
        .find(|asset| asset.pair_index == 0)
        .unwrap();
    let shelf = room
        .object_models
        .iter()
        .find(|asset| asset.pair_index == 1)
        .unwrap();
    assert_eq!(ladder.model.objects[0].prims.len(), 184);
    assert_eq!(shelf.model.objects[0].prims.len(), 84);

    let scripts = scd::reader::parse(&data).unwrap();
    let mut state = game::GameState::new(id, &room);
    {
        let mut vm = scd::vm::CommandVm::new(&scripts);
        let mut host = game::ScdGameHost::new(&mut state);
        vm.run_init(&mut host);
    }

    assert_eq!(state.objects.built, 2);
    let ladder = state.objects.record(0).unwrap();
    assert_eq!(
        ladder.flag,
        objects::OBJECT_FLAG_ACTIVE | objects::OBJECT_FLAG_CLIMBABLE
    );
    assert_eq!(ladder.model & 0x3F, 0);
    assert_eq!(ladder.asset, Some(0));
    assert!(ladder.pos != [0, 0, 0]);

    let shelf = state.objects.record(1).unwrap();
    assert_eq!(shelf.flag, objects::OBJECT_FLAG_ACTIVE);
    assert_eq!(shelf.model & 0x3F, 1);
    assert_eq!(shelf.asset, Some(1));
    assert!(shelf.pos != [0, 0, 0]);
    // The ladder's collision half-extents come from the record's extents.
    assert!(ladder.half_extents != [0, 0, 0]);
    // Both objects sit inside the room's starting camera zone, so the render
    // pass submits them.
    let player = arklay::player::spawn(id, &room);
    let cut = arklay::player::camera_for_position(&room, room.current_cut, player.pos);
    println!(
        "start cut {cut}; ladder {:?} in zone: {}; shelf {:?} in zone: {}",
        ladder.pos,
        arklay::npc::in_camera_zone(&room, cut, ladder.pos),
        shelf.pos,
        arklay::npc::in_camera_zone(&room, cut, shelf.pos)
    );
    assert!(
        arklay::npc::in_camera_zone(&room, cut, ladder.pos),
        "ladder is outside the starting camera zone"
    );
    assert!(
        arklay::npc::in_camera_zone(&room, cut, shelf.pos),
        "shelf is outside the starting camera zone"
    );
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_107_objects_render() {
    use arklay::render::{Camera, EntityMesh, Framebuffer, Lighting};

    let Some(root) = root() else {
        return;
    };
    let data = jpn_rdt(&root, "JPN/STAGE1/ROOM1070.RDT");
    let id = RoomId::parse("1070").unwrap();
    let room = rdt::parse(&data, id).unwrap();
    let scripts = scd::reader::parse(&data).unwrap();
    let mut state = game::GameState::new(id, &room);
    {
        let mut vm = scd::vm::CommandVm::new(&scripts);
        let mut host = game::ScdGameHost::new(&mut state);
        vm.run_init(&mut host);
    }

    let mut drew_a_cut = false;
    for cut in &room.cuts {
        let camera = Camera::from_cut(cut);
        let visible: Vec<_> = state
            .objects
            .records
            .iter()
            .filter(|record| {
                record.active()
                    && record.asset.is_some_and(|slot| {
                        room.object_models
                            .iter()
                            .any(|asset| asset.pair_index == usize::from(slot))
                    })
                    && camera
                        .project(record.pos)
                        .is_some_and(|[x, y]| (0..320).contains(&x) && (0..240).contains(&y))
            })
            .collect();
        if visible.is_empty() {
            continue;
        }
        drew_a_cut = true;

        let lighting = Lighting::from_room(&room);
        let mut joints = Vec::new();
        let mut assets = Vec::new();
        for record in visible {
            assets.push(
                room.object_models
                    .iter()
                    .find(|asset| asset.pair_index == usize::from(record.asset.unwrap()))
                    .unwrap(),
            );
            joints.push(vec![objects::rebuild(record, &lighting)]);
        }
        let meshes: Vec<EntityMesh<'_>> = assets
            .iter()
            .zip(&joints)
            .map(|(asset, joints)| EntityMesh {
                mesh: &asset.model,
                texture: &asset.texture,
                joints,
                tint: [255; 3],
                hidden_joints: 0,
            })
            .collect();
        let mut framebuffer = Framebuffer::new();
        arklay::render::draw_gameplay_scene(
            &mut framebuffer,
            None,
            &meshes,
            &[],
            &camera,
            &lighting,
            None,
        );
        assert!(
            framebuffer.rgba.chunks(4).any(|pixel| pixel[3] != 0),
            "cut {} drew no object pixels",
            cut.index
        );
    }
    assert!(drew_a_cut, "no camera cut sees an object");
}

/// The eight compass offsets a push approach is tried from.
const APPROACHES: [[i32; 2]; 8] = [
    [1, 0],
    [1, -1],
    [0, -1],
    [-1, -1],
    [-1, 0],
    [-1, 1],
    [0, 1],
    [1, 1],
];

/// A player placed `distance` from `target` on approach `direction`, facing it.
fn player_facing(
    id: RoomId,
    room: &arklay::state::RoomState,
    target: [i32; 3],
    direction: [i32; 2],
    distance: i32,
) -> arklay::player::PlayerState {
    let mut player = arklay::player::spawn(id, room);
    player.pos = [
        target[0] - direction[0] * distance,
        0,
        target[2] - direction[1] * distance,
    ];
    player.angle =
        arklay::sfx::angle_between_xz(player.pos[0], player.pos[2], target[0], target[2]);
    player
}

/// One engine-ordered gameplay tick with the object pass.
fn object_tick(
    player: &mut arklay::player::PlayerState,
    room: &arklay::state::RoomState,
    game: &mut game::GameState,
    input: arklay::player::Input,
) {
    let room_clips = room
        .room_anim
        .as_ref()
        .map(|anim| anim.clips.as_slice())
        .unwrap_or(&[]);
    arklay::player::update_with_room(player, room, &[], &[], room_clips, input);
    game.sync_entity_from_player(player);
    game.tick_objects(room, player);
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_107_shelf_slides_and_wedges_against_the_wall() {
    let Some(root) = root() else {
        return;
    };
    let data = jpn_rdt(&root, "JPN/STAGE1/ROOM1070.RDT");
    let id = RoomId::parse("1070").unwrap();
    let room = rdt::parse(&data, id).unwrap();
    assert!(room.room_anim.is_some(), "ROOM107 room animation pair");
    let scripts = scd::reader::parse(&data).unwrap();
    let mut game = game::GameState::new(id, &room);
    {
        let mut vm = scd::vm::CommandVm::new(&scripts);
        let mut host = game::ScdGameHost::new(&mut game);
        vm.run_init(&mut host);
    }

    let shelf_slot = (0..game.objects.records.len())
        .find(|&slot| {
            let record = game.objects.record(slot).unwrap();
            record.active()
                && record.flag & objects::OBJECT_FLAG_NOT_PUSHABLE == 0
                && record.flag & objects::OBJECT_FLAG_CLIMBABLE == 0
        })
        .expect("ROOM107 declares a pushable shelf");
    let shelf = *game.objects.record(shelf_slot).unwrap();
    let radius = arklay::player::spawn(id, &room).radius;
    let distance = radius + i32::from(shelf.half_extents[0].max(shelf.half_extents[2])) + 80;
    let input = arklay::player::Input {
        up: true,
        ..arklay::player::Input::default()
    };

    // Try every approach: some sides face a wall or the object's own probe
    // veto, so only the open side can start a real push.
    let mut best: Option<([i32; 2], i32)> = None;
    for direction in APPROACHES {
        let mut player = player_facing(id, &room, shelf.pos, direction, distance);
        let mut state = game.clone();
        for _ in 0..180 {
            object_tick(&mut player, &room, &mut state, input);
        }
        let moved = state.objects.records[shelf_slot].pos[0] - shelf.pos[0];
        let moved_z = state.objects.records[shelf_slot].pos[2] - shelf.pos[2];
        let travelled = (moved * moved + moved_z * moved_z).abs();
        if best.is_none_or(|(_, best)| travelled > best) {
            best = Some((direction, travelled));
        }
    }
    let (direction, travelled) = best.unwrap();
    assert!(travelled > 0, "no approach moved the shelf");

    // Push from the open side for a long run: the shelf slides, then wedges
    // and stays put while the player keeps leaning in.
    let mut player = player_facing(id, &room, shelf.pos, direction, distance);
    let mut state = game.clone();
    let mut last = shelf.pos;
    let mut stopped_at = None;
    for tick in 0..3000 {
        object_tick(&mut player, &room, &mut state, input);
        let current = state.objects.records[shelf_slot].pos;
        if tick % 200 == 0 && tick > 400 && current == last {
            stopped_at = Some(tick);
            break;
        }
        if tick % 200 == 0 {
            last = current;
        }
    }
    let final_pos = state.objects.records[shelf_slot].pos;
    let dx = final_pos[0] - shelf.pos[0];
    let dz = final_pos[2] - shelf.pos[2];
    let total = ((dx * dx + dz * dz) as f64).sqrt();
    println!(
        "shelf {shelf_slot} pushed from {direction:?}: {shelf:?} -> {final_pos:?} ({total} units, stopped {stopped_at:?})"
    );
    assert!(total > 100.0, "the shelf barely moved: {total}");
    assert!(stopped_at.is_some(), "the shelf never wedged");

    // The player stays outside the shelf's box (the mode-0 resolve).
    let record = state.objects.records[shelf_slot];
    let ext = i32::from(record.half_extents[0]) + radius;
    assert!(
        (player.pos[0] - final_pos[0]).abs() > ext - 40
            || (player.pos[2] - final_pos[2]).abs() > ext - 40,
        "the player is inside the shelf: player {:?}, shelf {final_pos:?}",
        player.pos
    );
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_107_vault_lands_the_player_on_the_far_side() {
    let Some(root) = root() else {
        return;
    };
    let data = jpn_rdt(&root, "JPN/STAGE1/ROOM1070.RDT");
    let id = RoomId::parse("1070").unwrap();
    let room = rdt::parse(&data, id).unwrap();
    let scripts = scd::reader::parse(&data).unwrap();
    let mut game = game::GameState::new(id, &room);
    {
        let mut vm = scd::vm::CommandVm::new(&scripts);
        let mut host = game::ScdGameHost::new(&mut game);
        vm.run_init(&mut host);
    }
    let ladder_slot = (0..game.objects.records.len())
        .find(|&slot| game.objects.record(slot).unwrap().flag & objects::OBJECT_FLAG_CLIMBABLE != 0)
        .expect("ROOM107 declares a climbable ladder");
    let ladder = *game.objects.record(ladder_slot).unwrap();

    // The scan compares `(facing + 0x800)` with the object's yaw, so approach
    // from the side the record faces and stop inside the reach box.
    let angle = (ladder.rotation[1].wrapping_sub(0x800) as u16) & 0x0FFF;
    let radians = f64::from(angle) * std::f64::consts::TAU / 4096.0;
    let reach = 300 + i32::from(ladder.half_extents[0]);
    let mut player = arklay::player::spawn(id, &room);
    player.angle = angle;
    player.pos = [
        ladder.pos[0] - (radians.cos() * f64::from(reach)) as i32,
        0,
        ladder.pos[2] + (radians.sin() * f64::from(reach)) as i32,
    ];
    // Press action: the climb scan latches the vault.
    object_tick(
        &mut player,
        &room,
        &mut game,
        arklay::player::Input {
            action_pressed: true,
            action_held: true,
            ..arklay::player::Input::default()
        },
    );
    assert!(
        player.vault_bit,
        "the climb scan rejected slot {ladder_slot} at {:?} angle {angle:#x}",
        player.pos
    );
    assert_eq!(player.locked, arklay::player::LockedAction::Vault);
    let start = player.pos;

    for _ in 0..2000 {
        object_tick(
            &mut player,
            &room,
            &mut game,
            arklay::player::Input::default(),
        );
        if player.locked == arklay::player::LockedAction::None {
            break;
        }
    }
    assert_eq!(
        player.locked,
        arklay::player::LockedAction::None,
        "vault stuck"
    );
    let dx = player.pos[0] - start[0];
    let dz = player.pos[2] - start[2];
    println!("vault {:?} -> {:?}", start, player.pos);
    assert!(
        dx.abs().max(dz.abs()) >= 0x73A - 1,
        "the vault did not cross the object: {start:?} -> {:?}",
        player.pos
    );
    assert_eq!(player.pos[1], -0x708, "the vault's vertical warp");
}

/// Build the player's posed mesh from the shipped character model, or `None`
/// when the install does not carry it.
fn player_mesh(
    root: &Path,
    id: RoomId,
    room: &arklay::state::RoomState,
) -> Option<(arklay::model::Emd, Vec<arklay::anim::Mat4x3>)> {
    let path = ["JPN/ENEMY/Char10.emd", "JPN/ENEMY/CHAR10.EMD"]
        .iter()
        .map(|relative| root.join(relative))
        .find(|path| path.is_file())?;
    let emd = arklay::emd::parse(&std::fs::read(path).ok()?).ok()?;
    let player = arklay::player::spawn(id, room);
    let keyframe = emd.keyframes.get(player.anim.keyframe_index(&emd.clips))?;
    let entity = arklay::anim::entity_matrix(player.pos, player.angle);
    let joints = arklay::anim::joint_matrices(&emd.skeleton, keyframe, &entity);
    Some((emd, joints))
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_112_mirror_shows_the_player_and_culls_outside_the_extent() {
    use arklay::render::{Camera, Lighting, MirrorPass};

    let Some((root, _)) = common::asset_env() else {
        return;
    };
    let data = jpn_rdt(&root, "JPN/STAGE1/ROOM1120.RDT");
    let id = RoomId::parse("1120").unwrap();
    let room = rdt::parse(&data, id).unwrap();
    let scripts = scd::reader::parse(&data).unwrap();
    let mut state = game::GameState::new(id, &room);
    {
        let mut vm = scd::vm::CommandVm::new(&scripts);
        let mut host = game::ScdGameHost::new(&mut state);
        vm.run_init(&mut host);
    }
    assert!(state.mirror_enabled(), "ROOM112 enables the mirror");
    assert!(!state.mirror_axis_x(), "the ROOM112 mirror is plane Z");
    assert_eq!(state.mirror.plane, 5700);
    assert_eq!(state.mirror.extent_min, 4100);
    assert_eq!(state.mirror.extent_max, 10000);
    let _ = Lighting::from_room(&room);

    let Some((emd, _)) = player_mesh(&root, id, &room) else {
        return;
    };
    let keyframe = emd
        .keyframes
        .get(
            arklay::player::spawn(id, &room)
                .anim
                .keyframe_index(&emd.clips),
        )
        .unwrap();
    let cut = room.cuts.get(room.current_cut).unwrap();
    let camera = Camera::from_cut(cut);
    let mirror = MirrorPass {
        axis_x: state.mirror_axis_x(),
        plane: i32::from(state.mirror.plane),
        extent_min: state.mirror.extent_min,
        extent_max: state.mirror.extent_max,
        camera_pos: cut.pos,
        camera: camera.mirrored(state.mirror_axis_x(), i32::from(state.mirror.plane)),
    };

    // A joint close to the plane inside the span reflects and the mirrored
    // camera projects it; one whose crossing leaves the span is culled.
    let inside = [7000, 0, 5690];
    let inside_joints = arklay::anim::joint_matrices(
        &emd.skeleton,
        keyframe,
        &arklay::anim::entity_matrix(inside, 0),
    );
    assert!(
        mirror.joint_visible(inside_joints[0].t),
        "inside joint not visible"
    );
    assert!(
        mirror.camera.project(inside_joints[0].t).is_some(),
        "mirrored camera drops the joint"
    );

    let outside = [1000, 0, 3000];
    let outside_joints = arklay::anim::joint_matrices(
        &emd.skeleton,
        keyframe,
        &arklay::anim::entity_matrix(outside, 0),
    );
    assert!(
        !mirror.joint_visible(outside_joints[0].t),
        "outside joint reflected"
    );
    assert!(
        arklay::render::mirror_point_visible(
            false,
            5700,
            state.mirror.extent_min,
            state.mirror.extent_max,
            cut.pos,
            outside_joints[0].t,
        )
        .is_none()
    );
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_30b_objs_hide_darkens_the_player() {
    use std::rc::Rc;

    let Some(root) = root() else {
        return;
    };
    let data = jpn_rdt(&root, "JPN/STAGE3/ROOM30B0.RDT");
    let id = RoomId::parse("30B0").unwrap();
    let room = rdt::parse(&data, id).unwrap();
    let scripts = scd::reader::parse(&data).unwrap();
    let mut game = game::GameState::new(id, &room);
    let mut command_vm = scd::vm::CommandVm::new(&scripts);
    let mut event_vm = scd::vm::EventVm::from_scripts(Rc::new(scripts));
    {
        let mut host = game::ScdGameHost::new(&mut game);
        command_vm.run_init(&mut host);
    }
    // Event script 4 is the cutscene that calls `objs_hide`; start it on slot
    // 0 directly rather than waiting for the in-game trigger (the script kills
    // slots 1, 2, 5 and 6, so it must not run on one of those).
    event_vm.start(0, 4);

    for _ in 0..3000 {
        {
            let mut host = game::ScdGameHost::new(&mut game);
            command_vm.run_main(&mut host);
        }
        for request in game.pending_event_requests.drain(..) {
            event_vm.apply(request);
        }
        {
            let mut host = game::ScdGameHost::new(&mut game);
            event_vm.step(&mut host);
        }
        game.advance_frame();
        if game.player_tint != [255; 3] {
            break;
        }
    }
    assert_eq!(
        game.player_tint,
        [0x30, 0, 0],
        "ROOM30B's objs_hide did not run"
    );
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_1000_item_box_lid_swings_and_settles() {
    let Some(root) = root() else {
        return;
    };
    let data = jpn_rdt(&root, "JPN/STAGE1/ROOM1000.RDT");
    let id = RoomId::parse("1000").unwrap();
    let room = rdt::parse(&data, id).unwrap();
    let scripts = scd::reader::parse(&data).unwrap();
    let mut state = game::GameState::new(id, &room);
    {
        let mut vm = scd::vm::CommandVm::new(&scripts);
        let mut host = game::ScdGameHost::new(&mut state);
        vm.run_init(&mut host);
    }
    let slot = (0..state.room_actions.len())
        .find(|&slot| {
            state.room_actions[slot]
                .is_some_and(|action| action.kind == game::RoomActionKind::ItemBox)
        })
        .expect("ROOM100 declares an item-box action");

    assert!(state.open_itembox(slot as u8));
    let cover = state.itembox.cover.unwrap();
    let mut minimum = 0;
    let mut opened = false;
    for _ in 0..80 {
        state.check_itembox_state();
        minimum = minimum.min(state.objects.records[cover].rotation[2]);
        if state.take_itembox_open() {
            opened = true;
            break;
        }
    }
    println!("item-box lid {cover} reached {minimum}");
    assert!(minimum < -199, "the lid never passed -199");
    assert!(opened, "the box menu never became ready");

    state.reset_itembox();
    state.check_itembox_state();
    assert_eq!(state.objects.records[cover].rotation[2], 0);
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn effect_attached_to_an_omodel_follows_its_transform() {
    let Some(root) = root() else {
        return;
    };
    let data = jpn_rdt(&root, "JPN/STAGE2/ROOM20A0.RDT");
    let id = RoomId::parse("20A0").unwrap();
    let room = rdt::parse(&data, id).unwrap();
    let scripts = scd::reader::parse(&data).unwrap();
    let mut game = game::GameState::new(id, &room);
    {
        let mut vm = scd::vm::CommandVm::new(&scripts);
        let mut host = game::ScdGameHost::new(&mut game);
        vm.run_init(&mut host);
    }
    assert!(game.objects.built > 0, "ROOM20A declares no built objects");

    // Spawn the room's first declared effect attached to omodel 0.
    let sprite = room
        .effects
        .sprites
        .first()
        .expect("ROOM20A declares no effect sprites");
    let effect_type = sprite.index;
    let Some(slot) = arklay::effects::create(
        &mut game,
        &room.effects,
        effect_type,
        0,
        0x80,
        [0, 0, 0],
        0,
        0,
    ) else {
        panic!("effect type {effect_type} did not allocate a slot");
    };

    let object = *game.objects.record(0).unwrap();
    assert!(object.active(), "omodel 0 is not active");
    arklay::effects::behaviour::update(&mut game, &room);
    let effect = game.effects.slot(usize::from(slot)).unwrap();
    assert_eq!(
        effect.sprite_offset, object.pos,
        "effect did not adopt the object's world position"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_3031_obj_xfm_relights_the_room_and_the_capture() {
    use std::rc::Rc;

    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("3031").unwrap();
    let data = pack.read(&id.rdt_entry()).unwrap();
    let mut room = rdt::parse(data, id).unwrap();
    let scripts = scd::reader::parse(data).unwrap();
    let mut game = game::GameState::new(id, &room);
    let mut command_vm = scd::vm::CommandVm::new(&scripts);
    let mut event_vm = scd::vm::EventVm::from_scripts(Rc::new(scripts));
    {
        let mut host = game::ScdGameHost::new(&mut game);
        command_vm.run_init(&mut host);
    }
    let player = arklay::player::spawn(id, &room);
    game.sync_entity_from_player(&player);

    let before_lights = room.lights;
    let before = arklay::engine::render_game_frame(&pack, id, &room, &game, &player).unwrap();

    // Event 1A's first block rewrites all three lights; start it directly
    // rather than waiting for the in-game trigger.
    event_vm.start(0, 0x1A);
    for _ in 0..3000 {
        {
            let mut host = game::ScdGameHost::new(&mut game);
            event_vm.step(&mut host);
        }
        game.apply_room_edits(&mut room);
        if room.lights != before_lights {
            break;
        }
    }
    assert_ne!(room.lights, before_lights, "obj_xfm never rewrote a light");
    println!("ROOM3031 lights {:?} -> {:?}", before_lights, room.lights);

    let after = arklay::engine::render_game_frame(&pack, id, &room, &game, &player).unwrap();
    let changed = before
        .rgba
        .iter()
        .zip(&after.rgba)
        .filter(|(a, b)| a != b)
        .count();
    assert!(changed > 50, "the relit frame only changed {changed} bytes");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn parented_objects_compose_their_sca_chain() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("3100").unwrap();
    let data = pack.read(&id.rdt_entry()).unwrap();
    let room = rdt::parse(data, id).unwrap();
    let scripts = scd::reader::parse(data).unwrap();
    let mut game = game::GameState::new(id, &room);
    {
        let mut vm = scd::vm::CommandVm::new(&scripts);
        let mut host = game::ScdGameHost::new(&mut game);
        vm.run_init(&mut host);
    }

    // ROOM310 carries a display object parented to the player; its world
    // matrix must follow the player, not sit at the local offset.
    let (slot, record) = game
        .objects
        .records
        .iter()
        .enumerate()
        .find(|(_, record)| record.active() && record.parent == 0xFE)
        .expect("ROOM310's player-parented object");
    let local = record.pos;
    let player_pos = [12345, 0, 6789];
    let player_angle = 0;
    let world = objects::world_matrix(&game.objects, slot, player_pos, player_angle);
    let expected = [
        player_pos[0] + local[0] * 4095 / 4096,
        player_pos[1] + local[1] * 4095 / 4096,
        player_pos[2] + local[2] * 4095 / 4096,
    ];
    assert_eq!(
        world.t, expected,
        "the player-parent chain was not composed"
    );

    // Moving the player moves the child; its local record is unchanged.
    let moved = objects::world_matrix(&game.objects, slot, [20000, 0, 30000], player_angle);
    assert_ne!(world.t, moved.t);
    assert_eq!(game.objects.record(slot).unwrap().pos, local);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_305_inst_cfg_rewrites_the_collision() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("3050").unwrap();
    let data = pack.read(&id.rdt_entry()).unwrap();
    let before = rdt::parse(data, id).unwrap();
    let sim = arklay::engine::simulate_room(&pack, id, 120, arklay::player::Input::default())
        .expect("ROOM305 loads");

    let mut changed = 0usize;
    for (quadrant, (a, b)) in before
        .collision
        .quadrants
        .iter()
        .zip(sim.room.collision.quadrants.iter())
        .enumerate()
    {
        for (index, (ra, rb)) in a.iter().zip(b.iter()).enumerate() {
            if ra != rb {
                changed += 1;
                println!("q{quadrant} rec{index}: {ra:?} -> {rb:?}");
            }
        }
    }
    assert!(changed >= 5, "inst_cfg rewrote only {changed} records");
    assert_eq!(
        sim.room.collision.quadrants[0][4],
        arklay::state::CollisionRect {
            x_max: 2,
            z_max: 2,
            x_min: 0,
            z_min: 0,
            kind: 1,
            flags: 768,
        },
        "the east-wing corridor wall collapsed"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn pushed_object_capture_is_deterministic() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("1070").unwrap();
    let data = pack.read(&id.rdt_entry()).unwrap();
    let room = rdt::parse(data, id).unwrap();
    let scripts = scd::reader::parse(data).unwrap();

    let build = |_pack: &Pack| {
        let mut game = game::GameState::new(id, &room);
        {
            let mut vm = scd::vm::CommandVm::new(&scripts);
            let mut host = game::ScdGameHost::new(&mut game);
            vm.run_init(&mut host);
        }
        game
    };
    let game = build(&pack);
    let shelf_slot = (0..game.objects.records.len())
        .find(|&slot| {
            let record = game.objects.record(slot).unwrap();
            record.active()
                && record.flag & objects::OBJECT_FLAG_NOT_PUSHABLE == 0
                && record.flag & objects::OBJECT_FLAG_CLIMBABLE == 0
        })
        .expect("ROOM107 declares a pushable shelf");
    let shelf = *game.objects.record(shelf_slot).unwrap();
    let radius = arklay::player::spawn(id, &room).radius;
    let distance = radius + i32::from(shelf.half_extents[0].max(shelf.half_extents[2])) + 80;
    let input = arklay::player::Input {
        up: true,
        ..arklay::player::Input::default()
    };

    // Find the open approach, then push for 400 ticks.
    let mut best: Option<([i32; 2], i32)> = None;
    for direction in APPROACHES {
        let mut player = player_facing(id, &room, shelf.pos, direction, distance);
        let mut state = game.clone();
        for _ in 0..180 {
            object_tick(&mut player, &room, &mut state, input);
        }
        let moved = state.objects.records[shelf_slot].pos[0] - shelf.pos[0];
        let moved_z = state.objects.records[shelf_slot].pos[2] - shelf.pos[2];
        let travelled = (moved * moved + moved_z * moved_z).abs();
        if best.is_none_or(|(_, best)| travelled > best) {
            best = Some((direction, travelled));
        }
    }
    let (direction, _) = best.unwrap();
    let mut player = player_facing(id, &room, shelf.pos, direction, distance);
    let mut state = game.clone();
    for _ in 0..400 {
        object_tick(&mut player, &room, &mut state, input);
    }
    assert_ne!(
        state.objects.records[shelf_slot].pos, shelf.pos,
        "the shelf did not move"
    );

    let frame = arklay::engine::render_game_frame(&pack, id, &room, &state, &player).unwrap();
    assert!(
        frame.rgba.chunks(4).any(|pixel| pixel[3] != 0),
        "the pushed frame is blank"
    );

    // The same drive reaches the same pixels.
    let mut repeat_game = build(&pack);
    let mut repeat_player = player_facing(id, &room, shelf.pos, direction, distance);
    for _ in 0..400 {
        object_tick(&mut repeat_player, &room, &mut repeat_game, input);
    }
    let repeat =
        arklay::engine::render_game_frame(&pack, id, &room, &repeat_game, &repeat_player).unwrap();
    assert_eq!(
        frame.rgba, repeat.rgba,
        "the push capture is not deterministic"
    );
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_112_mirror_renders_reflection_pixels() {
    use arklay::render::{Camera, EntityMesh, Framebuffer, Lighting, MirrorPass};

    let Some((root, _)) = common::asset_env() else {
        return;
    };
    let data = jpn_rdt(&root, "JPN/STAGE1/ROOM1120.RDT");
    let id = RoomId::parse("1120").unwrap();
    let room = rdt::parse(&data, id).unwrap();
    let scripts = scd::reader::parse(&data).unwrap();
    let mut state = game::GameState::new(id, &room);
    {
        let mut vm = scd::vm::CommandVm::new(&scripts);
        let mut host = game::ScdGameHost::new(&mut state);
        vm.run_init(&mut host);
    }
    assert!(state.mirror_enabled());
    let Some((emd, _)) = player_mesh(&root, id, &room) else {
        return;
    };
    let keyframe = &emd.keyframes[0];
    let joints = arklay::anim::joint_matrices(
        &emd.skeleton,
        keyframe,
        &arklay::anim::entity_matrix([7000, 0, 5690], 0),
    );
    let meshes = [EntityMesh {
        mesh: &emd.mesh,
        texture: &emd.texture,
        joints: &joints,
        tint: [255; 3],
        hidden_joints: 0,
    }];
    // A stand-in room-object mesh with a flat blue page: it sits before the
    // entity meshes, so the mirror pass must never reflect it.
    let blue = arklay::model::Texture8 {
        width: 1,
        height: 1,
        indices: vec![0],
        palettes: vec![[0, 0, 255, 255]],
        stp: Vec::new(),
    };
    let object_meshes = [
        EntityMesh {
            mesh: &emd.mesh,
            texture: &blue,
            joints: &joints,
            tint: [255; 3],
            hidden_joints: 0,
        },
        EntityMesh {
            mesh: &emd.mesh,
            texture: &emd.texture,
            joints: &joints,
            tint: [255; 3],
            hidden_joints: 0,
        },
    ];

    let lighting = Lighting::from_room(&room);
    let mut found = false;
    let mut drawn = 0usize;
    for cut in &room.cuts {
        let camera = Camera::from_cut(cut);
        let mirror = MirrorPass {
            axis_x: state.mirror_axis_x(),
            plane: i32::from(state.mirror.plane),
            extent_min: state.mirror.extent_min,
            extent_max: state.mirror.extent_max,
            camera_pos: cut.pos,
            camera: camera.mirrored(state.mirror_axis_x(), i32::from(state.mirror.plane)),
        };
        let mut plain = Framebuffer::new();
        arklay::render::draw_gameplay_scene(
            &mut plain,
            None,
            &meshes,
            &[],
            &camera,
            &lighting,
            None,
        );
        let mut reflected = Framebuffer::new();
        arklay::render::draw_gameplay_scene_with_effects(
            &mut reflected,
            None,
            [0, 0],
            [255; 3],
            &meshes,
            &[],
            &camera,
            &lighting,
            None,
            None,
            0,
            Some(&mirror),
        );
        let changed = plain
            .rgba
            .iter()
            .zip(&reflected.rgba)
            .filter(|(a, b)| a != b)
            .count();

        // The object-first partition: enabling the mirror must not change a
        // single object pixel; only the entity copy may reflect.
        let count_blue = |rgba: &[u8]| {
            rgba.chunks(4)
                .filter(|pixel| pixel[2] > 128 && pixel[0] < 64 && pixel[1] < 64)
                .count()
        };
        let mut object_plain = Framebuffer::new();
        arklay::render::draw_gameplay_scene_with_effects(
            &mut object_plain,
            None,
            [0, 0],
            [255; 3],
            &object_meshes,
            &[],
            &camera,
            &lighting,
            None,
            None,
            1,
            None,
        );
        let mut object_reflected = Framebuffer::new();
        arklay::render::draw_gameplay_scene_with_effects(
            &mut object_reflected,
            None,
            [0, 0],
            [255; 3],
            &object_meshes,
            &[],
            &camera,
            &lighting,
            None,
            None,
            1,
            Some(&mirror),
        );
        assert_eq!(
            count_blue(&object_plain.rgba),
            count_blue(&object_reflected.rgba),
            "the mirror pass reflected a room object"
        );

        if changed > 100 {
            found = true;
            drawn = changed;
            break;
        }
    }
    println!("ROOM112 mirror adds {drawn} bytes");
    assert!(found, "no cut rendered the mirror reflection");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room_30b_objs_hide_changes_the_rendered_tint() {
    use std::rc::Rc;

    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("30B0").unwrap();
    let data = pack.read(&id.rdt_entry()).unwrap();
    let room = rdt::parse(data, id).unwrap();
    let scripts = scd::reader::parse(data).unwrap();
    let mut game = game::GameState::new(id, &room);
    let mut command_vm = scd::vm::CommandVm::new(&scripts);
    let mut event_vm = scd::vm::EventVm::from_scripts(Rc::new(scripts));
    {
        let mut host = game::ScdGameHost::new(&mut game);
        command_vm.run_init(&mut host);
    }
    event_vm.start(0, 4);
    for _ in 0..3000 {
        {
            let mut host = game::ScdGameHost::new(&mut game);
            command_vm.run_main(&mut host);
        }
        for request in game.pending_event_requests.drain(..) {
            event_vm.apply(request);
        }
        {
            let mut host = game::ScdGameHost::new(&mut game);
            event_vm.step(&mut host);
        }
        game.advance_frame();
        if game.player_tint != [255; 3] {
            break;
        }
    }
    assert_eq!(game.player_tint, [0x30, 0, 0], "objs_hide did not run");

    let player = arklay::player::spawn(id, &room);
    game.sync_entity_from_player(&player);

    // Find a cut that draws the player, then compare the tinted and untinted
    // frames there.
    let mut best: Option<(usize, usize, Vec<u8>, Vec<u8>)> = None;
    for cut in 0..room.cuts.len() {
        let mut cut_room = room.clone();
        cut_room.current_cut = cut;
        let tinted =
            arklay::engine::render_game_frame(&pack, id, &cut_room, &game, &player).unwrap();
        game.player_tint = [255; 3];
        let plain =
            arklay::engine::render_game_frame(&pack, id, &cut_room, &game, &player).unwrap();
        game.player_tint = [0x30, 0, 0];
        let changed = plain
            .rgba
            .iter()
            .zip(&tinted.rgba)
            .filter(|(a, b)| a != b)
            .count();
        if best
            .as_ref()
            .is_none_or(|(_, best_changed, ..)| changed > *best_changed)
        {
            best = Some((cut, changed, plain.rgba, tinted.rgba));
        }
    }
    let (cut, changed, plain, tinted) = best.expect("no camera cut");
    println!("ROOM30B cut {cut}: the objs_hide tint changes {changed} bytes");
    assert!(changed > 50, "the tint changed only {changed} bytes");
    let sum = |rgba: &[u8]| {
        rgba.chunks(4)
            .map(|pixel| u64::from(pixel[0]) + u64::from(pixel[1]) + u64::from(pixel[2]))
            .sum::<u64>()
    };
    assert!(
        sum(&tinted) < sum(&plain),
        "the objs_hide tint must darken the frame"
    );
}
