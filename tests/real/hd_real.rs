//! Replacement-format acceptance: the checked-in `mods/hd` source swaps a room
//! background (a 2x PNG), a UI page (a PNG at the shipped page's size) and a
//! BGM track (an OGG Vorbis file), and every path proves the swap reaches its
//! consumer:
//!
//! - the room's settled camera (cut 0) draws the PNG's stretched pixels;
//! - the item box draws the replacement page;
//! - the BGM cache resolves `bgm/013` to the OGG, decodes it to 22.05 kHz mono
//!   PCM, and the shipped WAV stays reachable when the layer is dropped.
//!
//! The mod source is built through the `arklay mod build` binary, exactly like
//! `tests/real/m15_real.rs` builds the demo mod, so the checked-in files are
//! the ones under test.

use std::fs;
use std::path::{Path, PathBuf};

use arklay::bgm::BgmCache;
use arklay::engine::{ScriptSource, simulate_room};
use arklay::pack::{AssetClass, Pack};
use arklay::player;
use arklay::state::RoomId;

use super::common;

/// Self-deleting temporary directory unique to this process and label.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("arklay-hd-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Build the checked-in `mods/hd` sources through the `arklay mod build`
/// binary.
fn build_hd_pack(dir: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("mods/hd");
    let out = dir.join("hd.akpak");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_arklay"))
        .arg("mod")
        .arg("build")
        .arg(&source)
        .arg("--out")
        .arg(&out)
        .status()
        .expect("failed to run arklay mod build");
    assert!(status.success(), "arklay mod build failed");
    out
}

/// Run the capture CLI and return the written BMP bytes.
fn capture(pack: &Path, mods: &[&Path], args: &[&str], out: &Path) -> Vec<u8> {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arklay"));
    command.arg(pack);
    for layer in mods {
        command.arg("--mod").arg(layer);
    }
    command.args(args).arg("--capture").arg(out);
    let status = command.status().expect("failed to run the capture CLI");
    assert!(status.success(), "capture failed");
    fs::read(out).unwrap()
}

/// The RGB of one pixel of a 24-bit bottom-up BMP capture.
fn pixel(bmp: &[u8], x: usize, y: usize) -> [u8; 3] {
    let width = i32::from_le_bytes(bmp[18..22].try_into().unwrap()) as usize;
    let height = i32::from_le_bytes(bmp[22..26].try_into().unwrap()) as usize;
    let offset = u32::from_le_bytes(bmp[10..14].try_into().unwrap()) as usize;
    let stride = (width * 3).div_ceil(4) * 4;
    let row = height - 1 - y;
    let at = offset + row * stride + x * 3;
    [bmp[at + 2], bmp[at + 1], bmp[at]]
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn hd_pack_swaps_a_background_a_ui_page_and_a_bgm_track() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let base = Pack::open(&pack_path).unwrap();
    let temp = TempDir::new("acceptance");
    let hd_pack = build_hd_pack(&temp.path);

    // The pack carries the manifest and the four replacements (the UI page
    // ships as both PNG and WebP, which is how the example proves the
    // extension priority), and every entry verifies with its own decoder.
    let built = Pack::open(&hd_pack).unwrap();
    assert_eq!(built.len(), 5, "manifest, background, UI page pair and BGM");
    assert_eq!(built.manifest().unwrap().id, "hd");
    assert_eq!(built.manifest().unwrap().base.as_deref(), Some("re1"));
    let report = arklay::verify::verify_pack(&built, false);
    assert!(report.valid(), "{:?}", report.failures);
    assert_eq!(report.formats[&arklay::verify::Format::Png].ok, 1);
    assert_eq!(report.formats[&arklay::verify::Format::Webp].ok, 1);
    assert_eq!(report.formats[&arklay::verify::Format::Cut].ok, 1);
    assert_eq!(report.formats[&arklay::verify::Format::Ogg].ok, 1);

    // Every stem resolves to the replacement, and the shadowed shipped copy is
    // still reachable for the geometry check.
    let layered = Pack::open_layered(&pack_path, std::slice::from_ref(&hd_pack)).unwrap();
    let background = layered
        .read_asset("roomcut/100_000", AssetClass::Image)
        .unwrap()
        .unwrap();
    assert_eq!(background.path(), "roomcut/100_000.png");
    let shipped = background.shadowed().unwrap().unwrap();
    assert_eq!(shipped.path(), "roomcut/100_000.bmp");
    // The UI page ships as both PNG and WebP: the layer's own extension
    // priority picks the WebP, and the shadowed PNG stays reachable.
    let page = layered
        .read_asset("ui/itemboxn", AssetClass::Image)
        .unwrap()
        .unwrap();
    assert_eq!(page.path(), "ui/itemboxn.webp");
    assert_eq!(page.shadowed().unwrap().unwrap().path(), "ui/itemboxn.png");
    let audio = layered
        .read_asset("bgm/013", AssetClass::Audio)
        .unwrap()
        .unwrap();
    assert_eq!(audio.path(), "bgm/013.ogg");

    // The room's settled camera (cut 0) draws the 2x background stretched over
    // the frame: its green border lands in the first screen pixels and the red
    // checkerboard covers the middle.
    let id = RoomId::parse("1000").unwrap();
    let run = simulate_room(&layered, id, 120, player::Input::default()).unwrap();
    assert_eq!(run.script_source, ScriptSource::Rdt);
    let vanilla = simulate_room(&base, id, 120, player::Input::default()).unwrap();
    assert_ne!(run.frame.rgba, vanilla.frame.rgba);
    let repeat = simulate_room(&layered, id, 120, player::Input::default()).unwrap();
    assert_eq!(
        run.frame.rgba, repeat.frame.rgba,
        "the run is deterministic"
    );
    assert_eq!(&run.frame.rgba[..4], &[0, 200, 0, 255], "the PNG border");
    let red = ((60 * 320 + 60) * 4) as usize;
    assert_eq!(&run.frame.rgba[red..red + 4], &[200, 0, 0, 255]);

    // The CLI capture agrees, pixel for pixel, and `--no-mods` gives the
    // vanilla frame byte for byte.
    let modded = capture(
        &pack_path,
        &[&hd_pack],
        &["--room", "100", "--ticks", "120"],
        &temp.path.join("modded.bmp"),
    );
    assert_eq!(pixel(&modded, 2, 2), [0, 200, 0]);
    assert_eq!(pixel(&modded, 60, 60), [200, 0, 0]);
    let plain = capture(
        &pack_path,
        &[],
        &["--room", "100", "--ticks", "120"],
        &temp.path.join("plain.bmp"),
    );
    assert_ne!(modded, plain, "the background override changed the frame");

    // The item box draws the replacement page: the shipped sheet's dark frame
    // becomes the mod's blue field with a magenta border.
    let item_box = capture(
        &pack_path,
        &[&hd_pack],
        &["--ui", "box"],
        &temp.path.join("box.bmp"),
    );
    let vanilla_box = capture(
        &pack_path,
        &[],
        &["--ui", "box"],
        &temp.path.join("vanilla-box.bmp"),
    );
    assert_ne!(
        item_box, vanilla_box,
        "the UI page override changed the frame"
    );
    assert_eq!(pixel(&item_box, 100, 100), [0, 0, 200]);

    // The BGM stem resolves to the OGG and decodes to the stream's own 22.05
    // kHz mono 16-bit PCM; dropping the layer gives the shipped WAV back.
    let mut cache = BgmCache::default();
    let wav = cache.load(&layered, "Bgm_13").expect("the OGG decodes");
    assert_eq!(wav.sample_rate, 22050);
    assert_eq!(wav.channels, 1);
    assert_eq!(wav.format, arklay::audio::WavFormat::S16Le);
    let samples = wav.data.len() / 2;
    assert!(
        (10_000..=12_000).contains(&samples),
        "half a second of audio: {samples} samples"
    );
    let mut vanilla_cache = BgmCache::default();
    let vanilla_wav = vanilla_cache.load(&base, "Bgm_13").expect("the WAV loads");
    assert_ne!(vanilla_wav.data, wav.data, "the OGG replaced the WAV");

    // `arklay verify` on the layered view classifies the replacements by their
    // own formats and validates them with their own decoders. (The pack's 28
    // documented four-byte stub rooms still fail as they always have, so the
    // assertion is scoped to the replacement entries.)
    let merged = arklay::verify::verify_pack(&layered, false);
    assert_eq!(merged.formats[&arklay::verify::Format::Png].ok, 1);
    assert_eq!(merged.formats[&arklay::verify::Format::Webp].ok, 1);
    assert_eq!(merged.formats[&arklay::verify::Format::Ogg].ok, 1);
    for failure in &merged.failures {
        assert!(
            !failure.path.contains("100_000.png")
                && !failure.path.contains("itemboxn")
                && !failure.path.contains(".ogg"),
            "{failure:?}"
        );
    }
}
