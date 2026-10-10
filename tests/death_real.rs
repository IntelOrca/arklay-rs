//! Real-asset death and game-over tests: the shipped `ui/died.tim` decodes,
//! the headless `--ui death` capture is deterministic and draws the screen,
//! and a shipped room's player death runs the fall → fade → DIED → title
//! flow.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak cargo test --test death_real -- --ignored`

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use arklay::bmp;
use arklay::pack::Pack;
use arklay::state::RoomId;

/// Self-deleting temporary directory unique to this process and label.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("arklay-death-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Run the capture CLI with `args` and return the written BMP bytes.
fn cli_capture(pack: &Path, args: &[&str], out: &Path) -> Vec<u8> {
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_arklay"))
        .arg(pack)
        .args(args)
        .arg("--capture")
        .arg(out)
        .status()
        .expect("failed to run the capture CLI");
    assert!(status.success(), "capture {args:?} failed");
    fs::read(out).unwrap()
}

/// Pixels whose red channel dominates both others: the wavy strip's red text.
fn red_pixels(image: &arklay::state::Image) -> usize {
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] > 60 && pixel[0] > pixel[1] + 30 && pixel[0] > pixel[2] + 30)
        .count()
}

/// Pixels that are not near-black.
fn lit_pixels(image: &arklay::state::Image) -> usize {
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32 > 24)
        .count()
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn the_shipped_died_page_decodes() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let bytes = pack
        .read("ui/died.tim")
        .expect("the pack carries ui/died.tim; re-convert the install");
    assert_eq!(bytes.len(), 66_080, "the shipped page is 66,080 bytes");
    let texture = arklay::tim::decode_8bpp(bytes).expect("ui/died.tim is an 8bpp TIM");
    assert_eq!((texture.width, texture.height), (256, 256));
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_ui_death_capture_is_deterministic_and_draws_the_died_screen() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let dir = TempDir::new("capture");
    let first = dir.0.join("death-a.bmp");
    let second = dir.0.join("death-b.bmp");
    let a = cli_capture(&pack_path, &["--ui", "death"], &first);
    let b = cli_capture(&pack_path, &["--ui", "death"], &second);
    assert_eq!(a, b, "--ui death is deterministic");
    // The character digit selects the corpse model.
    let jill = cli_capture(
        &pack_path,
        &["--ui", "death", "--character", "1"],
        &dir.0.join("death-jill.bmp"),
    );
    assert_ne!(a, jill, "--character selects the corpse model");

    let image = bmp::decode(&a).expect("the capture is a BMP");
    assert_eq!((image.width, image.height), (320, 240));
    assert!(
        red_pixels(&image) > 200,
        "the red YOU DIED strip painted ({} red pixels)",
        red_pixels(&image)
    );
    assert!(
        lit_pixels(&image) < 40_000,
        "the settled screen is mostly dark ({} lit pixels)",
        lit_pixels(&image)
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_variant_room_deaths_fade_immediately() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    // The attic (the first Yawn) skips the 90-frame delay: the screen opens
    // after only the 128-frame fade.
    let attic = arklay::engine::simulate_death(&pack, RoomId::parse("210").unwrap(), 200, i16::MAX)
        .unwrap();
    assert!(attic.screen_open, "the attic fades immediately");
    // An ordinary room is still fading at the same tick.
    let ordinary =
        arklay::engine::simulate_death(&pack, RoomId::parse("100").unwrap(), 200, i16::MAX)
            .unwrap();
    assert!(
        !ordinary.screen_open,
        "the ordinary room is still in its delay/fade"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_room_death_reaches_the_died_screen() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let id = RoomId::parse("100").unwrap();
    let run = arklay::engine::simulate_death(&pack, id, 900, i16::MAX).unwrap();

    assert!(run.screen_open, "the DIED screen opened");
    assert!(run.finished, "the sequence returned to the title");
    assert!(
        run.game.entities[0].health < 0,
        "the killing blow left the player dead"
    );
    assert!(
        run.game.flags[5].bit(arklay::game::MSF_PLAYER_DEAD),
        "the dead flag was raised"
    );
    let frame = run.screen_frame.expect("a DIED frame was captured");
    assert!(
        red_pixels(&frame) > 100,
        "the shipped page's red strip painted ({} red pixels)",
        red_pixels(&frame)
    );
    assert!(
        lit_pixels(&frame) > 1_000,
        "the shipped page rendered ({} lit pixels)",
        lit_pixels(&frame)
    );
}
