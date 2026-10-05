//! Real-asset tests for the object-model class: parse every shipped RDT's
//! embedded TMD/TIM pairs, build ROOM107's objects from its init script, and
//! follow an effect attached to an object through its transform.

use std::path::{Path, PathBuf};

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
            })
            .collect();
        let mut framebuffer = Framebuffer::new();
        arklay::render::draw_gameplay_scene(
            &mut framebuffer,
            None,
            &meshes,
            None,
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
