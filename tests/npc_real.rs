//! Real-asset NPC capture tests: the scripted characters must actually reach
//! the rendered frame.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test npc_real -- --ignored`

mod common;

use arklay::anim;
use arklay::engine::{simulate_new_game, simulate_room};
use arklay::game::Entity;
use arklay::model::Emd;
use arklay::pack::Pack;
use arklay::player;
use arklay::render::{Camera, EntityMesh, Framebuffer, Lighting};
use arklay::state::{Image, RoomId};

/// Number of pixels two same-shape frames disagree on.
fn changed_pixels(a: &Image, b: &Image) -> usize {
    assert_eq!((a.width, a.height), (b.width, b.height));
    a.rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.rgba.as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count()
}

/// The character entity with `id`, its slot and its parsed model.
fn character(pack: &Pack, game: &arklay::game::GameState, id: u8) -> Option<(usize, Entity, Emd)> {
    let (slot, entity) = game
        .entities
        .iter()
        .enumerate()
        .find(|(_, entity)| entity.id == id && entity.active())?;
    let path = arklay::npc::model_path(id)?;
    let model = arklay::emd::parse(pack.read(path).ok()?).ok()?;
    Some((slot, *entity, model))
}

/// Render `meshes` over the cut background and return the frame.
fn render_cut(
    room: &arklay::state::RoomState,
    cut_index: usize,
    meshes: &[EntityMesh<'_>],
) -> Image {
    let cut = &room.cuts[cut_index];
    let camera = Camera::from_cut(cut);
    let lighting = Lighting::from_room(room);
    let mut framebuffer = Framebuffer::new();
    arklay::render::draw_gameplay_scene(
        &mut framebuffer,
        cut.background.as_ref(),
        meshes,
        None,
        &camera,
        &lighting,
        None,
    );
    Image {
        width: framebuffer.width,
        height: framebuffer.height,
        rgba: framebuffer.rgba,
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room20d0_characters_paint_in_the_spawn_region() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("20D0").unwrap();

    let first = simulate_room(&pack, id, 2, player::Input::default()).unwrap();
    let second = simulate_room(&pack, id, 2, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the capture differs between runs"
    );

    let (richard_slot, richard, richard_emd) =
        character(&pack, &first.game, 0x27).expect("ROOM20D0 spawns Richard");
    let (rebecca_slot, rebecca, rebecca_emd) =
        character(&pack, &first.game, 0x23).expect("ROOM20D0 spawns Rebecca");
    assert_eq!(richard.state(), 8, "the script moved Richard to state 8");
    assert_eq!(rebecca.state(), 1, "Rebecca is idle");

    // The engine's spawn camera (cut 0) does not frame the two spawn points;
    // pick the room cut that owns their region so the capture shows them.
    let cut_index = (0..first.room.cuts.len())
        .find(|&index| {
            let camera = Camera::from_cut(&first.room.cuts[index]);
            let on_screen = |pos: [i32; 3]| {
                camera
                    .project(pos)
                    .is_some_and(|[x, y]| (16..304).contains(&x) && (16..224).contains(&y))
            };
            on_screen(richard.pos) && on_screen(rebecca.pos)
        })
        .expect("a room cut frames both spawns");

    // Pose each model at the frame the driver's clock applied last.
    let mut models: Vec<Emd> = Vec::new();
    let mut joint_sets: Vec<Vec<anim::Mat4x3>> = Vec::new();
    for (slot, entity, emd) in [
        (richard_slot, richard, richard_emd),
        (rebecca_slot, rebecca, rebecca_emd),
    ] {
        let keyframe = first.game.entity_anims[slot].keyframe_index(&entity, &emd.clips);
        let matrix = anim::entity_matrix(entity.pos, entity.angle);
        joint_sets.push(anim::joint_matrices(
            &emd.skeleton,
            &emd.keyframes[keyframe],
            &matrix,
        ));
        models.push(emd);
    }
    let meshes: Vec<EntityMesh<'_>> = models
        .iter()
        .zip(&joint_sets)
        .map(|(model, joints)| EntityMesh {
            mesh: &model.mesh,
            texture: &model.texture,
            joints,
        })
        .collect();

    let with = render_cut(&first.room, cut_index, &meshes);
    let without = render_cut(&first.room, cut_index, &[]);
    let changed = changed_pixels(&with, &without);
    assert!(
        changed > 500,
        "the characters only repainted {changed} pixels"
    );

    // Both spawn points must own changed pixels.
    let camera = Camera::from_cut(&first.room.cuts[cut_index]);
    for entity in [&richard, &rebecca] {
        let [cx, cy] = camera.project(entity.pos).unwrap();
        let near = with
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(without.rgba.as_chunks::<4>().0)
            .enumerate()
            .filter(|(index, (a, b))| {
                a != b && {
                    let x = (index % 320) as i32;
                    let y = (index / 320) as i32;
                    (x - cx).abs() <= 80 && (y - cy).abs() <= 120
                }
            })
            .count();
        assert!(
            near > 50,
            "no character pixels near the spawn at ({cx}, {cy})"
        );
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1000_new_game_capture_includes_rebecca() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();

    // The main-hall scene needs the intro event to run; 30 ticks bring the
    // player into the camera that frames Rebecca.
    let first = simulate_new_game(&pack, 0, 30, player::Input::default()).unwrap();
    let second = simulate_new_game(&pack, 0, 30, player::Input::default()).unwrap();
    assert_eq!(
        first.frame.rgba, second.frame.rgba,
        "the new-game capture differs between runs"
    );
    assert!(
        first
            .game
            .entities
            .iter()
            .skip(1)
            .any(|entity| entity.id == 0x23 && entity.active()),
        "ROOM1000 init spawns Rebecca"
    );

    let changed = changed_pixels(&first.frame, &first.baseline);
    assert!(changed > 500, "Rebecca only repainted {changed} pixels");
}
