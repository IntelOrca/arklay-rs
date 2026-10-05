//! Real-asset NPC capture tests: the scripted characters must actually reach
//! the rendered frame.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test npc_real -- --ignored`

mod common;

use std::path::Path;
use std::process::Command;

use arklay::anim;
use arklay::engine::{SimulatedRoom, simulate_new_game, simulate_room};
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

/// Run the CLI's `--room ... --ticks ... --capture` path and return the BMP.
///
/// This is the end-to-end check of the flag: the spawned binary opens the pack,
/// runs the fixed ticks and writes the scaled capture.
fn cli_capture(pack: &Path, room: &str, ticks: u32, out: &Path) -> Vec<u8> {
    let status = Command::new(env!("CARGO_BIN_EXE_arklay"))
        .arg(pack)
        .arg("--room")
        .arg(room)
        .arg("--player")
        .arg("0")
        .arg("--ticks")
        .arg(ticks.to_string())
        .arg("--capture")
        .arg(out)
        .status()
        .expect("spawn arklay");
    assert!(status.success(), "the CLI capture failed");
    std::fs::read(out).expect("read the CLI capture")
}

/// A private capture directory for one test.
fn capture_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("arklay-npc-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

/// Render ROOM20D0's two spawned characters over the first cut that frames
/// their spawn region and return `(cut index, changed pixels, changed pixels
/// near each spawn)`.
///
/// At the boot camera (cut 0) both characters project off screen. The cuts
/// that frame them (1-2) are not the cuts whose switch zone contains them, so
/// the old whole-entity zone cull would have hidden them; the original draws
/// every active entity and only gates individual joints on the zone. This
/// renders the framing cut through the scene path so the pose and paint path
/// can be checked independently of the camera.
fn render_room20d0_spawns(pack: &Pack, sim: &SimulatedRoom) -> (usize, usize, [usize; 2]) {
    let characters: Vec<(usize, Entity, Emd)> = [0x27u8, 0x23]
        .iter()
        .map(|&id| character(pack, &sim.game, id).expect("ROOM20D0 spawns the character"))
        .collect();

    let cut_index = (0..sim.room.cuts.len())
        .find(|&index| {
            let camera = Camera::from_cut(&sim.room.cuts[index]);
            let on_screen = |pos: [i32; 3]| {
                camera
                    .project(pos)
                    .is_some_and(|[x, y]| (16..304).contains(&x) && (16..224).contains(&y))
            };
            characters
                .iter()
                .all(|(_, entity, _)| on_screen(entity.pos))
        })
        .expect("a room cut frames both spawns");
    assert!(
        characters
            .iter()
            .all(|(_, entity, _)| !arklay::npc::in_camera_zone(&sim.room, cut_index, entity.pos)),
        "the framing cut must not contain the spawns in its switch zone"
    );

    // Pose each model at the frame the driver's clock applied last.
    let mut models: Vec<&Emd> = Vec::new();
    let mut joint_sets: Vec<Vec<anim::Mat4x3>> = Vec::new();
    for (slot, entity, emd) in &characters {
        let keyframe = sim.game.entity_anims[*slot].keyframe_index(entity, &emd.clips);
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

    let with = render_cut(&sim.room, cut_index, &meshes);
    let without = render_cut(&sim.room, cut_index, &[]);
    let changed = changed_pixels(&with, &without);

    let camera = Camera::from_cut(&sim.room.cuts[cut_index]);
    let mut near = [0usize; 2];
    for (index, (_, entity, _)) in characters.iter().enumerate() {
        let [cx, cy] = camera.project(entity.pos).unwrap();
        near[index] = with
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
    }
    (cut_index, changed, near)
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

    let (_, richard, _) = character(&pack, &first.game, 0x27).expect("ROOM20D0 spawns Richard");
    assert_eq!(richard.state(), 8, "the script moved Richard to state 8");
    let (_, rebecca, _) = character(&pack, &first.game, 0x23).expect("ROOM20D0 spawns Rebecca");
    assert_eq!(rebecca.state(), 1, "Rebecca is idle");

    let (_, changed, near) = render_room20d0_spawns(&pack, &first);
    assert!(
        changed > 500,
        "the characters only repainted {changed} pixels"
    );
    for (index, count) in near.iter().enumerate() {
        assert!(*count > 50, "no character pixels near spawn {index}");
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

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room20d0_ticks_capture_is_stable_and_shows_the_characters() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("20D0").unwrap();
    let dir = capture_dir("room20d");

    // Two CLI runs with the same arguments are byte-identical.
    let first = cli_capture(&pack_path, "20D", 30, &dir.join("first.bmp"));
    let second = cli_capture(&pack_path, "20D", 30, &dir.join("second.bmp"));
    assert_eq!(first, second, "two --ticks captures differ");

    // The capture writer stores one sample per framebuffer pixel; the decoded
    // BMP must reproduce the library seam's frame exactly.
    let captured = arklay::bmp::decode(&first).unwrap();
    let sim = simulate_room(&pack, id, 30, player::Input::default()).unwrap();
    assert_eq!(
        captured.rgba, sim.frame.rgba,
        "the CLI capture does not match the simulated frame"
    );

    // The characters painted. At the boot camera (cut 0) they project off
    // screen, so the default capture cannot contain them; the delta is
    // measured at the first cut that frames the spawns (outside that cut's
    // switch zone, which the renderer no longer culls), over the exact
    // simulation the CLI capture ran.
    let (_, changed, _) = render_room20d0_spawns(&pack, &sim);
    assert!(
        changed > 500,
        "the characters only repainted {changed} pixels"
    );
}

/// A character outside the current camera's switch zone must still paint when
/// it projects on screen: the original's entity render loop draws every active
/// entity and only gates individual joints (flag byte 0x74) on the zone.
#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room40c1_off_zone_character_paints_on_screen() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("40C1").unwrap();
    let sim = simulate_room(&pack, id, 1, player::Input::default()).unwrap();

    // The Jill variant spawns id 0x2B at (10000, 0, 30000); cut 0's switch
    // zone does not contain it, but it projects to (178, 132) on screen.
    let (_, entity, _) = character(&pack, &sim.game, 0x2B).expect("ROOM40C1 spawns 0x2B");
    assert_eq!(entity.pos, [10000, 0, 30000]);
    assert!(
        !arklay::npc::in_camera_zone(&sim.room, sim.room.current_cut, entity.pos),
        "the fixture character must be outside the current switch zone"
    );
    let camera = Camera::from_cut(&sim.room.cuts[sim.room.current_cut]);
    let [x, y] = camera.project(entity.pos).expect("the fixture projects");
    assert!(
        (16..304).contains(&x) && (16..224).contains(&y),
        "the fixture must project on screen, got ({x}, {y})"
    );

    // The simulated frame runs engine::render_frame; the baseline hides every
    // character slot, so the delta is exactly the off-zone character.
    let changed = changed_pixels(&sim.frame, &sim.baseline);
    assert!(
        changed > 0,
        "the off-zone character never painted in the simulated frame"
    );
    let near = sim
        .frame
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(sim.baseline.rgba.as_chunks::<4>().0)
        .enumerate()
        .filter(|(index, (a, b))| {
            a != b && {
                let px = (index % 320) as i32;
                let py = (index / 320) as i32;
                (px - x).abs() <= 100 && (py - y).abs() <= 160
            }
        })
        .count();
    assert_eq!(
        near, changed,
        "the frame delta is not the off-zone character's pixels"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room1000_ticks_capture_contains_the_spawned_character() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("1000").unwrap();
    let dir = capture_dir("room1000");

    let bytes = cli_capture(&pack_path, "100", 30, &dir.join("room.bmp"));
    let captured = arklay::bmp::decode(&bytes).unwrap();
    let sim = simulate_room(&pack, id, 30, player::Input::default()).unwrap();
    assert_eq!(
        captured.rgba, sim.frame.rgba,
        "the CLI capture does not match the simulated frame"
    );
    assert!(
        sim.game
            .entities
            .iter()
            .skip(1)
            .any(|entity| entity.id == 0x23 && entity.active()),
        "ROOM1000 init spawns Rebecca"
    );

    let changed = changed_pixels(&sim.frame, &sim.baseline);
    assert!(changed > 500, "Rebecca only repainted {changed} pixels");
}
