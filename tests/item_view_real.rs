//! Real-asset item viewer (examine screen) tests.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=assets/re1 ARKLAY_RE1_PACK=target/tmp-test/game.akpak cargo test --test item_view_real -- --ignored --nocapture`
//!
//! Both `ARKLAY_RE1_ROOT` and `ARKLAY_RE1_PACK` must be set together; with
//! neither the tests skip, and with only one they fail. The pack must carry
//! the converted `item/*.ivm` models, `font/font.tim`, the text tables and the
//! slice-5 UI art; a stale pack fails the test rather than silently skipping.
//!
//! The tests cover the four slice-7 acceptance paths: every shipped `.ivm`
//! parses, the combat knife renders a non-empty model (stable across two
//! draws, and different after a rotation), the description window draws
//! through the real font, and the `--ui view` capture is deterministic.

mod common;

use std::path::Path;

use arklay::engine;
use arklay::font::Font;
use arklay::ivm;
use arklay::pack::Pack;
use arklay::render::Framebuffer;
use arklay::text::Text;
use arklay::tim;
use arklay::ui::item_view::ItemViewScreen;
use arklay::ui::{Screen, ScreenResult, UiContext, UiInput};

fn pack() -> Option<Pack> {
    let (_, path) = common::asset_env()?;
    Some(Pack::open(&path).unwrap())
}

fn context<'a>(pack: &'a Pack, text: &'a Text, font: Option<&'a Font>) -> UiContext<'a> {
    UiContext {
        pack,
        save_dir: Path::new("."),
        font,
        text: Some(text),
        ticks: 0,
        cues: Default::default(),
    }
}

fn painted(framebuffer: &Framebuffer) -> usize {
    framebuffer
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[3] != 0)
        .count()
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn every_shipped_item_model_parses_from_the_pack() {
    let Some(pack) = pack() else {
        return;
    };
    let mut entries: Vec<&str> = pack
        .entries()
        .map(|entry| entry.path())
        .filter(|path| path.starts_with("item/") && path.ends_with(".ivm"))
        .collect();
    entries.sort();
    assert_eq!(entries.len(), 77, "the pack holds 77 item models");

    for entry in entries {
        let model = ivm::parse(pack.read(entry).unwrap())
            .unwrap_or_else(|error| panic!("{entry}: {error:#}"));
        assert_eq!((model.texture.width, model.texture.height), (256, 256));
        assert!(
            model.objects.iter().any(|object| !object.prims.is_empty()),
            "{entry} has no primitives"
        );
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn the_combat_knife_renders_a_non_empty_model() {
    let Some(pack) = pack() else {
        return;
    };
    let text = Text::load(&pack);
    let cx = context(&pack, &text, None);

    let mut screen = ItemViewScreen::new(1);
    screen.open_with(&pack, &text, &[0; 4]);
    assert!(screen.model().is_some(), "the knife's model loaded");

    let mut first = Framebuffer::new();
    screen.draw(&cx, &mut first);
    let first_painted = painted(&first);
    assert!(
        first_painted > 500,
        "the knife drew only {first_painted} pixels"
    );

    let mut second = Framebuffer::new();
    screen.draw(&cx, &mut second);
    assert_eq!(first.rgba, second.rgba, "two knife renders differ");

    // A half turn must move the model: spin right 0x20 * 0x40 = 0x800.
    for _ in 0..0x40 {
        assert_eq!(
            screen.update(
                &cx,
                UiInput {
                    right: true,
                    ..UiInput::default()
                },
            ),
            ScreenResult::Continue
        );
    }
    assert_eq!(screen.yaw, 0x800);
    let mut turned = Framebuffer::new();
    screen.draw(&cx, &mut turned);
    assert_ne!(first.rgba, turned.rgba, "the turntable angle is ignored");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn the_description_window_draws_over_the_model() {
    let Some(pack) = pack() else {
        return;
    };
    let text = Text::load(&pack);
    let font = Font::new(
        tim::decode_4bpp(pack.read("font/font.tim").unwrap()).expect("font/font.tim decodes"),
    );
    let cx = context(&pack, &text, Some(&font));

    let mut screen = ItemViewScreen::new(1);
    screen.open_with(&pack, &text, &[0; 4]);
    assert!(
        screen.description().is_some(),
        "the knife has a description"
    );

    let mut base = Framebuffer::new();
    screen.draw(&cx, &mut base);

    assert_eq!(
        screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
        ),
        ScreenResult::Continue
    );
    assert!(screen.message_active(), "confirm opened the description");
    for _ in 0..300 {
        screen.update(&cx, UiInput::default());
    }

    let mut described = Framebuffer::new();
    screen.draw(&cx, &mut described);
    let changed = base
        .rgba
        .iter()
        .zip(&described.rgba)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        changed > 100,
        "the description window drew only {changed} pixels"
    );
}

#[test]
#[ignore = "requires a converted pack via ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_view_capture_is_deterministic() {
    let Some((_root, path)) = common::asset_env() else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("arklay-view-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let saves = dir.join("saves");
    let first_path = dir.join("view_a.bmp");
    let second_path = dir.join("view_b.bmp");
    let menu_path = dir.join("menu.bmp");

    engine::run_ui_with_options(&path, "view", Some(&first_path), &saves, 0).unwrap();
    engine::run_ui_with_options(&path, "view", Some(&second_path), &saves, 0).unwrap();
    // The same capture path without the viewer: the menu and room underneath
    // are identical, so any further delta is the viewer's own model/name layer.
    engine::run_ui_with_options(&path, "menu", Some(&menu_path), &saves, 0).unwrap();

    let first = std::fs::read(&first_path).unwrap();
    let second = std::fs::read(&second_path).unwrap();
    assert_eq!(first, second, "two view captures differ");

    let decoded = arklay::bmp::decode(&first).unwrap();
    assert_eq!((decoded.width, decoded.height), (320, 240));
    // The viewport around the model keeps the original's pure-black screen
    // clear; only the model and the name line may paint over it.
    let viewport = |x: usize, y: usize| -> [u8; 4] {
        let offset = (y * decoded.width as usize + x) * 4;
        decoded.rgba[offset..offset + 4].try_into().unwrap()
    };
    assert_eq!(viewport(35, 60), [0, 0, 0, 255]);
    assert_eq!(viewport(170, 100), [0, 0, 0, 255]);
    let content = decoded
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
        .count();
    assert!(content > 5000, "the view capture is mostly black");

    // A stale pack without `item/*.ivm` still shows the menu underneath, so
    // non-black alone cannot prove the model layer drew. The viewer must add a
    // known delta over the menu capture: the model (centre region, away from
    // the inventory column and the name line) plus the examine name line.
    let menu = arklay::bmp::decode(&std::fs::read(&menu_path).unwrap()).unwrap();
    let changed = decoded
        .rgba
        .iter()
        .zip(&menu.rgba)
        .filter(|(a, b)| a != b)
        .count();
    let mut centre = 0usize;
    let mut name_line = 0usize;
    for y in 0..decoded.height as usize {
        for x in 0..decoded.width as usize {
            let offset = (y * 320 + x) * 4;
            if decoded.rgba[offset..offset + 4] == menu.rgba[offset..offset + 4] {
                continue;
            }
            if (40..200).contains(&x) && (20..170).contains(&y) {
                centre += 1;
            }
            if y >= 180 {
                name_line += 1;
            }
        }
    }
    println!("view vs menu: {changed} changed ({centre} centre, {name_line} name line)");
    assert!(
        changed > 2500,
        "the item viewer only repainted {changed} pixels over the menu"
    );
    assert!(
        centre > 250,
        "the model only repainted {centre} centre pixels over the menu"
    );
    assert!(
        name_line > 200,
        "the item name only repainted {name_line} line pixels"
    );
    println!("wrote {}", first_path.display());
}
