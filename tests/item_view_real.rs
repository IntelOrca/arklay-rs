//! Real-asset item viewer (examine screen) tests.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_PACK=target/tmp-test/game.akpak cargo test --test item_view_real -- --ignored --nocapture`
//!
//! The pack must carry the converted `item/*.ivm` models, `font/font.tim`,
//! the text tables and the slice-5 UI art; a stale pack fails the test rather
//! than silently skipping. Only an unset `ARKLAY_RE1_PACK` skips.
//!
//! The tests cover the four slice-7 acceptance paths: every shipped `.ivm`
//! parses, the combat knife renders a non-empty model (stable across two
//! draws, and different after a rotation), the description window draws
//! through the real font, and the `--ui view` capture is deterministic.

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
    let path = std::env::var("ARKLAY_RE1_PACK").ok()?;
    Some(Pack::open(Path::new(&path)).unwrap())
}

fn context<'a>(pack: &'a Pack, text: &'a Text, font: Option<&'a Font>) -> UiContext<'a> {
    UiContext {
        pack,
        save_dir: Path::new("."),
        font,
        text: Some(text),
        ticks: 0,
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
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
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
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn the_combat_knife_renders_a_non_empty_model() {
    let Some(pack) = pack() else {
        return;
    };
    let text = Text::load(&pack);
    let cx = context(&pack, &text, None);

    let mut screen = ItemViewScreen::new(1);
    screen.open_with(&pack, &text);
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
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
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
    screen.open_with(&pack, &text);
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
#[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
fn real_view_capture_is_deterministic() {
    let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("arklay-view-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let saves = dir.join("saves");
    let first_path = dir.join("view_a.bmp");
    let second_path = dir.join("view_b.bmp");

    engine::run_ui_with_options(Path::new(&path), "view", Some(&first_path), &saves, 0).unwrap();
    engine::run_ui_with_options(Path::new(&path), "view", Some(&second_path), &saves, 0).unwrap();

    let first = std::fs::read(&first_path).unwrap();
    let second = std::fs::read(&second_path).unwrap();
    assert_eq!(first, second, "two view captures differ");

    let decoded = arklay::bmp::decode(&first).unwrap();
    assert_eq!((decoded.width, decoded.height), (320, 240));
    let content = decoded
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
        .count();
    assert!(content > 5000, "the view capture is mostly black");
    println!("wrote {}", first_path.display());
}
