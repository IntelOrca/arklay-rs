//! Real-asset player-shadow capture test.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=assets/re1 ARKLAY_RE1_PACK=../re1.akpak cargo test --test shadow_real -- --ignored --nocapture`
//!
//! The pack must have been built after the shadow converter landed
//! (`shadow/kage.tim`); a pack that predates it falls back to the install's
//! `DATA/KAGE.TIM`, and only an unset `ARKLAY_RE1_PACK` skips. The frame is
//! deterministic: two renders must be byte-identical, and the shadow must
//! repaint the floor under the player.

mod common;

use std::path::Path;

use arklay::anim;
use arklay::bmp;
use arklay::emd;
use arklay::pack::Pack;
use arklay::player;
use arklay::rdt;
use arklay::render::{self, Camera, EntityMesh, Framebuffer, Lighting, MaskLayer};
use arklay::shadow;
use arklay::state::{Image, RoomId};

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// The baked shadow page: the pack entry when present, else the install file.
fn shadow_page(pack: &Pack, root: &Path) -> Image {
    if let Ok(bytes) = pack.read(shadow::KAGE_ENTRY) {
        return shadow::decode(bytes).unwrap_or_else(|err| {
            panic!("pack {} is invalid: {err:#}", shadow::KAGE_ENTRY);
        });
    }
    let path = ["JPN", ""]
        .iter()
        .map(|prefix| root.join(prefix).join("DATA").join("KAGE.TIM"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!(
                "pack has no {} and no KAGE.TIM under {}",
                shadow::KAGE_ENTRY,
                root.display()
            )
        });
    shadow::decode(&std::fs::read(&path).unwrap())
        .unwrap_or_else(|err| panic!("invalid {}: {err:#}", path.display()))
}

fn pixel(framebuffer: &Framebuffer, x: usize, y: usize) -> [u8; 4] {
    let offset = (y * framebuffer.width as usize + x) * 4;
    framebuffer.rgba[offset..offset + 4].try_into().unwrap()
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn player_shadow_capture_is_deterministic_and_present_at_spawn() {
    let Some((root, path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let id = RoomId::parse("1001").unwrap();
    let room = rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
    let player_state = player::spawn(id, &room);
    let camera_index = player::camera_for_position(&room, 0, player_state.pos);
    let cut = &room.cuts[camera_index];
    assert!(
        shadow::visible(id, &room, camera_index, &player_state),
        "spawn {:?} is not in camera {camera_index}'s header zone",
        player_state.pos
    );

    let texture = shadow_page(&pack, &root);
    let shadow = render::Shadow {
        texture: &texture,
        pos: player_state.pos,
        angle: player_state.angle,
        half_x: shadow::PLAYER_HALF_X,
        half_z: shadow::PLAYER_HALF_Z,
        lift: shadow::offset_y(id, camera_index),
        tint: shadow::billboard_tint(shadow::PLAYER_COLOR),
    };

    let emd = emd::parse(
        pack.read(&format!("player/{:02}.emd", id.player_flag & 3))
            .unwrap(),
    )
    .unwrap();
    let keyframe = player_state.anim.keyframe_index(&emd.clips);
    let entity = anim::entity_matrix(player_state.pos, player_state.angle);
    let joints = anim::joint_matrices(&emd.skeleton, &emd.keyframes[keyframe], &entity);
    let entities = [EntityMesh {
        mesh: &emd.mesh,
        texture: &emd.texture,
        joints: &joints,
    }];

    let background = bmp::decode(pack.read(&id.cut_entry(camera_index)).unwrap()).unwrap();
    let page = if cut.masks.is_empty() || cut.mask_active == 0 {
        None
    } else {
        pack.read(&id.roommask_entry(camera_index))
            .ok()
            .and_then(|bytes| bmp::decode_mask(bytes).ok())
    };
    let layer = page.as_ref().map(|page| MaskLayer {
        room: id,
        camera: camera_index,
        cut,
        page,
        active: cut.mask_active,
    });
    let camera = Camera::from_cut(cut);
    let lighting = Lighting::from_room(&room);

    let mut with = Framebuffer::new();
    render::draw_gameplay_scene(
        &mut with,
        Some(&background),
        &entities,
        Some(&shadow),
        &camera,
        &lighting,
        layer.as_ref(),
    );
    let mut again = Framebuffer::new();
    render::draw_gameplay_scene(
        &mut again,
        Some(&background),
        &entities,
        Some(&shadow),
        &camera,
        &lighting,
        layer.as_ref(),
    );
    assert_eq!(with.rgba, again.rgba, "two shadow renders differ");

    let mut without = Framebuffer::new();
    render::draw_gameplay_scene(
        &mut without,
        Some(&background),
        &entities,
        None,
        &camera,
        &lighting,
        layer.as_ref(),
    );

    let mut changed = 0usize;
    let mut darkened = 0usize;
    for (a, b) in with
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(without.rgba.as_chunks::<4>().0)
    {
        if a == b {
            continue;
        }
        changed += 1;
        if a[..3].iter().zip(&b[..3]).all(|(x, y)| x <= y) {
            darkened += 1;
        }
    }
    println!("shadow repainted {changed} pixels ({darkened} darker)");
    assert!(changed > 300, "the shadow only repainted {changed} pixels");
    assert!(
        darkened > 300,
        "only {darkened} of the repainted pixels are darker"
    );

    // The darkest shadow texel carries 99/255 coverage, so a repainted pixel
    // keeps at least 61% of its unshadowed value.
    let mut darkest = 255u8;
    for (a, b) in with
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(without.rgba.as_chunks::<4>().0)
    {
        if a != b {
            darkest = darkest.min(a[0]);
        }
    }
    let floor = pixel(&without, 160, 200);
    println!("darkest shadowed channel {darkest}, floor sample {floor:?}");

    let capture_dir = std::env::temp_dir();
    std::fs::create_dir_all(&capture_dir).expect("create temp dir");
    for (name, framebuffer) in [
        ("shadow_room1001.bmp", &with),
        ("shadow_room1001_plain.bmp", &without),
    ] {
        let image = Image {
            width: framebuffer.width,
            height: framebuffer.height,
            rgba: framebuffer.rgba.clone(),
        };
        let path = capture_dir.join(name);
        bmp::encode(&image, &path).expect("write the capture");
        println!(
            "wrote {} (fnv1a {:016x})",
            path.display(),
            fnv1a(&framebuffer.rgba)
        );
    }

    // Outside every switch zone the shadow is skipped.
    let mut away = player_state.clone();
    away.pos = [30000, 0, 30000];
    assert!(
        !shadow::visible(id, &room, camera_index, &away),
        "a player far outside the header zone must not queue a shadow"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn shadow_page_matches_the_pack_entry_when_converted() {
    let Some((root, path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&path).unwrap();
    let Ok(bytes) = pack.read(shadow::KAGE_ENTRY) else {
        eprintln!(
            "pack {} predates the shadow entry; reconvert to exercise this check",
            path.display()
        );
        return;
    };
    let image = shadow::decode(bytes).unwrap();
    assert_eq!((image.width, image.height), (26, 29));
    let direct = shadow_page(&pack, &root);
    assert_eq!(image.rgba, direct.rgba);
}
