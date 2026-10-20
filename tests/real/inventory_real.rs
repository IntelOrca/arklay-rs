//! Real-asset inventory/status screen tests.
//!
//! Run with:
//! `ARKLAY_RE1_ROOT=assets/re1 ARKLAY_RE1_PACK=re1.akpak cargo test --profile test-release --test real inventory_real:: -- --ignored --nocapture`
//!
//! Both `ARKLAY_RE1_ROOT` and `ARKLAY_RE1_PACK` must be set together; with
//! neither the tests skip, and with only one they fail. The pack must contain
//! the menu art (`ui/status.tim`, `ui/blue.tim`, `ui/statface.tim`,
//! `ui/staitem.tim`, `item/item_all.bmp`, `font/font.tim`); a stale pack fails
//! the test rather than silently skipping.
//!
//! The test renders the `--ui menu` capture (`ui/main_menu.lua` over the
//! room boot's inventory) twice and requires it to be deterministic and to
//! draw the panel art. The menu's rules are covered by the screen tests over
//! the checked-in script.

use crate::common;

use arklay::bmp;
use arklay::engine;

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn menu_capture_is_deterministic_and_draws_the_panel_art() {
    let Some((_root, path)) = common::asset_env() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("arklay-menu-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let saves = dir.join("saves");
    let first = dir.join("menu_a.bmp");
    let second = dir.join("menu_b.bmp");

    engine::run_ui_with_options(&path, "menu", Some(&first), &saves, 0).unwrap();
    engine::run_ui_with_options(&path, "menu", Some(&second), &saves, 0).unwrap();

    let first_bytes = std::fs::read(&first).unwrap();
    let second_bytes = std::fs::read(&second).unwrap();
    assert_eq!(first_bytes, second_bytes, "two menu captures differ");
    let image = bmp::decode(&first_bytes).unwrap();
    assert_eq!((image.width, image.height), (320, 240));

    // The centre viewport is the original's pure-black screen clear, not a
    // grey: sample points away from the panel art and the item slots.
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let offset = (y * image.width as usize + x) * 4;
        image.rgba[offset..offset + 4].try_into().unwrap()
    };
    assert_eq!(pixel(35, 60), [0, 0, 0, 255]);
    assert_eq!(pixel(170, 100), [0, 0, 0, 255]);

    let painted = image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[..3] != [0, 0, 0])
        .count();
    println!("pixels painted by the menu: {painted}");
    assert!(painted > 5000, "the menu only painted {painted} pixels");

    let _ = std::fs::remove_dir_all(&dir);
}
