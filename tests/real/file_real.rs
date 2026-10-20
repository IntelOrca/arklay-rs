//! Real-asset FILE tab tests.
//!
//! Run with:
//! `ARKLAY_RE1_ROOT=assets/re1 ARKLAY_RE1_PACK=re1.akpak cargo test --profile test-release --test real file_real:: -- --ignored --nocapture`
//!
//! Both `ARKLAY_RE1_ROOT` and `ARKLAY_RE1_PACK` must be set together; with
//! neither the tests skip, and with only one they fail. The pack must carry
//! the `file/*.tim` entries the `ui/file.lua` reader draws (two covers, 14
//! backdrops, 43 page packs) plus the menu sheets and font. The first test
//! loads every TIM and checks the sizes the JPN reader indexes; the reader's
//! paging is covered by the Lua screen's unit tests. The second renders the
//! `--ui file` capture twice and requires it to be deterministic and to draw
//! its own layer over the menu.

use crate::common;

use arklay::bmp;
use arklay::engine;
use arklay::pack::Pack;
use arklay::state::Image;

/// The 14 backdrops and 43 page packs `ui/file.lua` names.
const FILEI_NAMES: [&str; 14] = [
    "filei03", "filei04", "filei06", "filei07", "filei08", "filei09", "filei10", "filei11",
    "filei12", "filei13", "filei14", "filei15", "filei16", "filei17",
];
const TEXTM_NAMES: [&str; 43] = [
    "textm_l0", "textm_l1", "textm_m0", "textm_m1", "textm_m2", "textm_n0", "textm_n1", "textm_n2",
    "textm_n3", "textm_n4", "textm_n5", "textm_o0", "textm_o1", "textm_p0", "textm_q0", "textm_q1",
    "textm_q2", "textm_r0", "textm_r1", "textm_r2", "textm_r3", "textm_s0", "textm_s1", "textm_s2",
    "textm_s3", "textm_t0", "textm_t1", "textm_t2", "textm_t3", "textm_u0", "textm_u1", "textm_u2",
    "textm_v0", "textm_v1", "textm_v2", "textm_w0", "textm_w1", "textm_x0", "textm_y0", "textm_z0",
    "textm_00", "textm_01", "textm_02",
];

fn pack() -> Option<Pack> {
    let (_, path) = common::asset_env()?;
    Some(Pack::open(&path).unwrap())
}

fn non_black(image: &Image) -> usize {
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
        .count()
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_file_art_has_the_sizes_the_reader_indexes() {
    let Some(pack) = pack() else {
        return;
    };
    let size = |stem: &str| {
        let art =
            arklay::ui::load_page(&pack, stem).unwrap_or_else(|err| panic!("{stem}: {err:#}"));
        (art.width(), art.height())
    };
    for cover in ["file/file000", "file/file001"] {
        assert_eq!(size(cover), (256, 256), "{cover}");
    }
    for name in FILEI_NAMES {
        assert_eq!(size(&format!("file/{name}")), (256, 120), "{name}");
    }
    for name in TEXTM_NAMES {
        assert_eq!(size(&format!("file/{name}")), (256, 256), "{name}");
    }
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_file_capture_is_deterministic_and_draws_the_list() {
    let Some((_root, path)) = common::asset_env() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("arklay-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let saves = dir.join("saves");
    let first = dir.join("file_a.bmp");
    let second = dir.join("file_b.bmp");
    let menu_path = dir.join("menu.bmp");

    engine::run_ui_with_options(&path, "file", Some(&first), &saves, 0).unwrap();
    engine::run_ui_with_options(&path, "file", Some(&second), &saves, 0).unwrap();
    // The same capture path without the FILE tab: the room, camera and panel
    // beneath are the same, so the delta is the file selector's own layer.
    engine::run_ui_with_options(&path, "menu", Some(&menu_path), &saves, 0).unwrap();

    let first_bytes = std::fs::read(&first).unwrap();
    let second_bytes = std::fs::read(&second).unwrap();
    assert_eq!(first_bytes, second_bytes, "two file captures differ");
    let image = bmp::decode(&first_bytes).unwrap();
    assert_eq!((image.width, image.height), (320, 240));
    assert!(
        non_black(&image) > 5000,
        "the file capture is mostly black: {} pixels",
        non_black(&image)
    );

    // A missing FILE layer would leave the menu over the same room; require a
    // known delta so that case cannot pass.
    let menu = bmp::decode(&std::fs::read(&menu_path).unwrap()).unwrap();
    let changed = image
        .rgba
        .iter()
        .zip(&menu.rgba)
        .filter(|(a, b)| a != b)
        .count();
    println!("file vs menu: {changed} changed pixels");
    // The selector draws into the panel's `0x28,0x18 0xf0x0xc0` screen area
    // (the original masks it with the panel's chrome), so its own layer can
    // only repaint that much.
    assert!(
        changed > 20000,
        "the FILE tab only repainted {changed} pixels over the menu"
    );
    println!("wrote {}", first.display());
}
