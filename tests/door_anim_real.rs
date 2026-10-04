//! Real-asset `.dor` door animation tests.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 cargo test --test door_anim_real -- --ignored --nocapture`
//!
//! The tests parse the shipped door corpus, drive `DOOR00.DOR`'s main script
//! through the VM and render a mid-animation frame to a BMP.

use std::path::{Path, PathBuf};

use arklay::door::parse;
use arklay::door::vm::{DoorParams, Vm};
use arklay::render::{Framebuffer, draw_door_scene};
use arklay::state::Image;

fn asset_root() -> Option<PathBuf> {
    std::env::var("ARKLAY_RE1_ROOT").ok().map(PathBuf::from)
}

/// Every `.DOR` under `JPN/ITEM_M1`, matched case-insensitively and sorted.
fn door_paths(root: &Path) -> Vec<PathBuf> {
    let dir = root.join("JPN/ITEM_M1");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("dor"))
        })
        .collect();
    paths.sort();
    paths
}

fn door_named<'a>(paths: &'a [PathBuf], name: &str) -> &'a PathBuf {
    paths
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|file| file.to_str())
                .is_some_and(|file| file.eq_ignore_ascii_case(name))
        })
        .unwrap_or_else(|| panic!("{name} not found in the ITEM_M1 directory"))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn every_shipped_door_parses() {
    let Some(root) = asset_root() else {
        return;
    };
    let paths = door_paths(&root);
    assert_eq!(paths.len(), 34, "shipped door corpus");

    let mut total_objects = 0usize;
    let mut total_triangles = 0usize;
    for path in &paths {
        let data =
            std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let dor = parse(&data).unwrap_or_else(|err| panic!("parse {}: {err:#}", path.display()));
        assert!(!dor.scripts.is_empty(), "{} has no scripts", path.display());
        assert!(
            !dor.mesh.objects.is_empty(),
            "{} has no mesh",
            path.display()
        );
        assert_eq!(dor.texture.width, 128, "{} texture width", path.display());
        assert_eq!(dor.texture.height, 256, "{} texture height", path.display());
        total_objects += dor.mesh.objects.len();
        total_triangles += dor.triangle_count();
        println!(
            "{:>14}: {:>2} scripts, {:>2} objects, {:>3} triangles",
            path.file_name().unwrap().to_string_lossy(),
            dor.scripts.len(),
            dor.mesh.objects.len(),
            dor.triangle_count()
        );
    }
    println!("corpus totals: {total_objects} objects, {total_triangles} triangles");

    let door00 = parse(&std::fs::read(door_named(&paths, "door00.dor")).unwrap()).unwrap();
    assert_eq!(door00.scripts.len(), 35);
    assert_eq!(door00.mesh.objects.len(), 12);
    assert_eq!(door00.triangle_count(), 697);
    assert_eq!(door00.orders.len(), 12);
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn door00_main_script_moves_the_camera_and_sets_order_flags() {
    let Some(root) = asset_root() else {
        return;
    };
    let paths = door_paths(&root);
    let data = std::fs::read(door_named(&paths, "door00.dor")).unwrap();
    let dor = parse(&data).unwrap();

    let mut vm = Vm::new(
        &dor,
        DoorParams {
            direction: 0,
            ..DoorParams::default()
        },
    );
    let mut camera_x = Vec::new();
    let mut flags_seen = 0u16;
    let mut mesh_seen = false;
    for _ in 0..60 {
        let frame = vm.step();
        camera_x.push(frame.camera.from[0]);
        for order in &frame.orders {
            flags_seen |= order.flags;
            mesh_seen |= order.mesh.is_some();
        }
    }

    // CAM_MATRIX starts at x = 10800 and the script's 80-iteration loop pulls
    // the camera along -X one delta step per frame.
    assert_eq!(camera_x[0], 10800);
    assert!(camera_x[59] < 9000, "camera did not move: {}", camera_x[59]);
    assert!(camera_x[59] > 5000, "camera overshot: {}", camera_x[59]);
    assert!(
        flags_seen & 0x8000 != 0,
        "no order submitted a draw: {flags_seen:#06X}"
    );
    assert!(mesh_seen, "ORDER_SETUP created no mesh copy");
    assert!(!vm.is_done());
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn every_shipped_door_runs_frames_without_panicking() {
    let Some(root) = asset_root() else {
        return;
    };
    for path in door_paths(&root) {
        let data = std::fs::read(&path).unwrap();
        let dor = parse(&data).unwrap();
        let mut vm = Vm::new(
            &dor,
            DoorParams {
                direction: 0,
                ..DoorParams::default()
            },
        );
        for _ in 0..30 {
            let frame = vm.step();
            assert_eq!(frame.orders.len(), 12, "{}", path.display());
        }
    }
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn ele03_vertex_add_moves_cage_door_vertices() {
    let Some(root) = asset_root() else {
        return;
    };
    let paths = door_paths(&root);
    let data = std::fs::read(door_named(&paths, "ele03.dor")).unwrap();
    let dor = parse(&data).unwrap();
    let base = dor.mesh.objects[1].vertices.clone();

    // Direction 0 runs the lift script, whose 100-frame opening loop activates
    // the per-vertex cage-door scripts (other directions hit the bare END).
    let mut vm = Vm::new(
        &dor,
        DoorParams {
            direction: 0,
            ..DoorParams::default()
        },
    );
    let mut writes = 0usize;
    let mut order11 = None;
    for _ in 0..130 {
        let frame = vm.step();
        writes += frame
            .orders
            .iter()
            .map(|order| order.vertex_writes.len())
            .sum::<usize>();
        if let Some(mesh) = frame.orders[11].mesh {
            order11 = Some(mesh.vertices.clone());
        }
    }

    assert!(writes > 0, "no VERT_ADD writes reached the frame");
    let order11 = order11.expect("order 11 has a mesh copy");
    assert!(
        order11 != base,
        "VERT_ADD did not change order 11's mesh copy"
    );
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn door00_mid_animation_frame_renders_non_black_and_stable() {
    let Some(root) = asset_root() else {
        return;
    };
    let paths = door_paths(&root);
    let data = std::fs::read(door_named(&paths, "door00.dor")).unwrap();
    let dor = parse(&data).unwrap();

    let mut vm = Vm::new(
        &dor,
        DoorParams {
            direction: 0,
            ..DoorParams::default()
        },
    );
    let mut rendered = None;
    for _ in 0..20 {
        let frame = vm.step();
        let mut framebuffer = Framebuffer::new();
        draw_door_scene(&mut framebuffer, &dor, &frame);
        rendered = Some(framebuffer);
    }
    let first = rendered.expect("a rendered frame");

    let non_black = first
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
        .count();
    println!("mid-animation non-black pixels: {non_black}");
    assert!(non_black > 2000, "door panels are not visible: {non_black}");

    let hash = fnv1a(&first.rgba);
    println!("mid-animation frame hash: {hash:#018X}");

    // A second, independent run must produce the identical frame.
    let mut vm = Vm::new(
        &dor,
        DoorParams {
            direction: 0,
            ..DoorParams::default()
        },
    );
    let mut again = None;
    for _ in 0..20 {
        let frame = vm.step();
        let mut framebuffer = Framebuffer::new();
        draw_door_scene(&mut framebuffer, &dor, &frame);
        again = Some(framebuffer);
    }
    let second = again.expect("a rendered frame");
    assert_eq!(
        fnv1a(&second.rgba),
        hash,
        "two renders of the same frame differ"
    );

    let image = Image {
        width: first.width,
        height: first.height,
        rgba: first.rgba,
    };
    let path = std::env::temp_dir().join("door00_frame20.bmp");
    arklay::bmp::encode(&image, &path).expect("write BMP");
    println!("wrote {}", path.display());
    assert!(path.is_file());
}
