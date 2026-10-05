//! Real-asset room-mask render interleave test.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_PACK=target/tmp-test/game.akpak cargo test --test mask_render_real -- --ignored --nocapture`
//!
//! The pack must have been built after the room-mask converter landed
//! (`cargo run --release -- convert-game <install> --out target/tmp-test/game.akpak`);
//! a pack that is missing `roommask/100_000.bmp` fails the test, so a stale
//! pack cannot silently hide a regression. Only an unset `ARKLAY_RE1_PACK`
//! skips.
//!
//! The player is a world-space stand-in posed through the real camera: a
//! screen-facing triangle behind the group-1 pillar of ROOM1000 cut 0. Real
//! assets under test are the mask page, the parsed mask table and the
//! per-(room, camera) ordering records.

mod common;

use arklay::anim;
use arklay::bmp;
use arklay::mask;
use arklay::model::{Texture8, Tmd, TmdObject, TmdPrim};
use arklay::pack::Pack;
use arklay::rdt;
use arklay::render::{Camera, EntityMesh, Framebuffer, Lighting, MaskLayer, draw_gameplay_scene};
use arklay::state::{Light, RoomId};

/// The player stand-in's flat texture colour.
const PLAYER: [u8; 4] = [200, 40, 40, 255];
/// How far behind the pillar's key the stand-in is placed, in view units.
const BACK_INSET: f64 = 1500.0;

fn player_texture() -> Texture8 {
    Texture8 {
        width: 1,
        height: 1,
        indices: vec![0],
        palettes: vec![PLAYER],
    }
}

/// The world-space point that projects to `screen` at view-space Z `depth`.
///
/// Inverts the stored 4.12 camera (`view = R * world / 4096 + trans`), so
/// `world = R^T * (view - trans) / 4096`.
fn world_at(camera: &Camera, screen: (f64, f64), depth: f64) -> [i32; 3] {
    let focal = f64::from(camera.fov);
    let view = [
        (screen.0 - 160.0) * depth / focal,
        (120.0 - screen.1) * depth / focal,
        depth,
    ];
    let u = [
        view[0] - f64::from(camera.trans[0]),
        view[1] - f64::from(camera.trans[1]),
        view[2] - f64::from(camera.trans[2]),
    ];
    std::array::from_fn(|column| {
        let sum = u[0] * f64::from(camera.view[0][column])
            + u[1] * f64::from(camera.view[1][column])
            + u[2] * f64::from(camera.view[2][column]);
        (sum / 4096.0).round() as i32
    })
}

/// A screen-facing triangle whose corners project to `corners` at `depth`.
fn player_triangle(camera: &Camera, corners: [(f64, f64); 3], depth: f64) -> Tmd {
    let vertices: Vec<[i16; 3]> = corners
        .iter()
        .map(|&(x, y)| {
            world_at(camera, (x, y), depth).map(|value| {
                i16::try_from(value).unwrap_or_else(|_| panic!("vertex {value} does not fit a TMD"))
            })
        })
        .collect();
    Tmd {
        objects: vec![TmdObject {
            vertices,
            normals: vec![[0, 0, 4096]],
            prims: vec![TmdPrim {
                vertices: [0, 1, 2],
                normals: [0, 0, 0],
                uv: [[0, 0]; 3],
                clut: 0x7800,
                tsb: 0x80,
                textured: true,
                blend: false,
                raw_y: false,
                flat_color: None,
            }],
        }],
    }
}

fn pixel(framebuffer: &Framebuffer, x: usize, y: usize) -> [u8; 4] {
    let offset = (y * framebuffer.width as usize + x) * 4;
    framebuffer.rgba[offset..offset + 4].try_into().unwrap()
}

fn page_pixel(page: &arklay::state::Image, x: usize, y: usize) -> [u8; 4] {
    let offset = (y * page.width as usize + x) * 4;
    let texel: [u8; 4] = page.rgba[offset..offset + 4].try_into().unwrap();
    [texel[0], texel[1], texel[2], 255]
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn player_occluded_by_a_group_1_pillar_until_the_group_is_disabled() {
    let Some((_root, path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let id = RoomId::parse("1000").unwrap();
    let room = rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
    let cut = &room.cuts[0];
    let entry = id.roommask_entry(0);
    let page_bytes = pack.read(&entry).unwrap_or_else(|err| {
        panic!(
            "pack `{}` has no {entry}: {err} (reconvert the pack)",
            path.display()
        )
    });
    let page = bmp::decode_mask(page_bytes).unwrap();

    // The first group-1 sprite is the pillar top at screen (9, 73): 8x40,
    // page UV (0, 0), so pixel (12, 80) samples page texel (3, 7).
    let sprite = cut.masks[0];
    assert_eq!(sprite.group, 1);
    assert_eq!(sprite.uv, (0, 0));
    assert_eq!(sprite.pos, (9, 73));
    assert_eq!(sprite.size, (8, 40));
    let mask_key = mask::mask_sprite_key(id, 0, 0, &sprite).expect("the sprite is visible");
    let pillar = ((sprite.pos.0 + 3) as usize, (sprite.pos.1 + 7) as usize);
    let expected_mask = page_pixel(&page, 3, 7);
    assert_ne!(
        expected_mask, PLAYER,
        "pick a page texel distinct from the player colour"
    );

    let camera = Camera::from_cut(cut);
    let lighting = Lighting {
        ambient: [4095; 3],
        lights: [Light::default(); 3],
    };

    // Pose the stand-in past the pillar's key so the mask is drawn later and
    // covers the player. The triangle spans the left half of the frame and
    // covers both the pillar pixel and an unoccluded pixel at (60, 140).
    let depth = f64::from(mask_key) + BACK_INSET;
    let mesh = player_triangle(&camera, [(4.0, 40.0), (150.0, 40.0), (4.0, 225.0)], depth);
    let joints = [anim::Mat4x3 {
        r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
        t: [0, 0, 0],
    }];
    let texture = player_texture();
    let entities = [EntityMesh {
        mesh: &mesh,
        texture: &texture,
        joints: &joints,
        tint: [255; 3],
        hidden_joints: 0,
    }];

    // Only group 1 is active, so no other overlay interferes.
    let mut active = 0u32;
    mask::set_group_active(&mut active, 1, true);
    let layer = MaskLayer {
        room: id,
        camera: 0,
        cut,
        page: &page,
        active,
    };

    let mut framebuffer = Framebuffer::new();
    draw_gameplay_scene(
        &mut framebuffer,
        None,
        &entities,
        None,
        &camera,
        &lighting,
        Some(&layer),
    );

    assert_eq!(
        pixel(&framebuffer, pillar.0, pillar.1),
        expected_mask,
        "the pillar pixel must come from the mask page"
    );
    assert_eq!(
        pixel(&framebuffer, 60, 140),
        PLAYER,
        "an unoccluded player pixel must stay visible"
    );

    let capture = std::env::temp_dir().join("mask_room1000_frame.bmp");
    std::fs::create_dir_all(capture.parent().unwrap()).expect("create temp dir");
    let image = arklay::state::Image {
        width: framebuffer.width,
        height: framebuffer.height,
        rgba: framebuffer.rgba.clone(),
    };
    bmp::encode(&image, &capture).expect("write the capture");
    println!("wrote {}", capture.display());

    // Turning group 1 off removes the pillar: the player shows through.
    mask::set_group_active(&mut active, 1, false);
    let mut framebuffer = Framebuffer::new();
    draw_gameplay_scene(
        &mut framebuffer,
        None,
        &entities,
        None,
        &camera,
        &lighting,
        Some(&MaskLayer { active, ..layer }),
    );
    assert_eq!(
        pixel(&framebuffer, pillar.0, pillar.1),
        PLAYER,
        "disabling group 1 must reveal the player"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn room1000_player_frame_is_deterministic_and_mask_aware() {
    let Some((_root, path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let id = RoomId::parse("1000").unwrap();
    let room = rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
    let cut = &room.cuts[0];
    let mask_entry = id.roommask_entry(0);
    let page_bytes = pack.read(&mask_entry).unwrap_or_else(|err| {
        panic!(
            "pack `{}` has no {mask_entry}: {err} (reconvert the pack)",
            path.display()
        )
    });
    let background = bmp::decode(pack.read(&id.cut_entry(0)).unwrap()).unwrap();
    let page = bmp::decode_mask(page_bytes).unwrap();
    let emd = arklay::emd::parse(pack.read("player/00.emd").unwrap()).unwrap();

    // The same pose the engine's gameplay render would use at spawn.
    let player_state = arklay::player::spawn(id, &room);
    let keyframe = player_state.anim.keyframe_index(&emd.clips);
    let entity = anim::entity_matrix(player_state.pos, player_state.angle);
    let joints = anim::joint_matrices(&emd.skeleton, &emd.keyframes[keyframe], &entity);
    let texture = &emd.texture;
    let entities = [EntityMesh {
        mesh: &emd.mesh,
        texture,
        joints: &joints,
        tint: [255; 3],
        hidden_joints: 0,
    }];

    let camera = Camera::from_cut(cut);
    let lighting = Lighting::from_room(&room);
    let layer = MaskLayer::new(id, 0, cut, &page);

    let mut plain = Framebuffer::new();
    draw_gameplay_scene(
        &mut plain,
        Some(&background),
        &entities,
        None,
        &camera,
        &lighting,
        None,
    );
    let mut masked = Framebuffer::new();
    draw_gameplay_scene(
        &mut masked,
        Some(&background),
        &entities,
        None,
        &camera,
        &lighting,
        Some(&layer),
    );
    let mut again = Framebuffer::new();
    draw_gameplay_scene(
        &mut again,
        Some(&background),
        &entities,
        None,
        &camera,
        &lighting,
        Some(&layer),
    );

    assert_eq!(masked.rgba, again.rgba, "two masked renders differ");
    let changed = plain
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(masked.rgba.as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    println!("pixels repainted by the mask layer: {changed}");
    assert!(
        changed > 500,
        "the mask layer only repainted {changed} pixels"
    );

    let plain_capture = std::env::temp_dir().join("mask_room1000_plain.bmp");
    std::fs::create_dir_all(plain_capture.parent().unwrap()).expect("create temp dir");
    let image = arklay::state::Image {
        width: plain.width,
        height: plain.height,
        rgba: plain.rgba,
    };
    bmp::encode(&image, &plain_capture).expect("write the plain capture");

    let masked_capture = std::env::temp_dir().join("mask_room1000_player.bmp");
    std::fs::create_dir_all(masked_capture.parent().unwrap()).expect("create temp dir");
    let image = arklay::state::Image {
        width: masked.width,
        height: masked.height,
        rgba: masked.rgba,
    };
    bmp::encode(&image, &masked_capture).expect("write the masked capture");
    println!(
        "wrote {} and {}",
        plain_capture.display(),
        masked_capture.display()
    );
}
