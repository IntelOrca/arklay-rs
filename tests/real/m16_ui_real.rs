//! M16 slices 9 and 11 real-asset checks: the UI captures (including the map
//! tab), the built-in map tables, and the golden-frame harness.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/path/to/re1 \
//!  ARKLAY_RE1_PACK=<a pack converted by this tree> \
//!  cargo test --profile test-release --test real m16_ui_real:: -- --ignored --nocapture`
//!
//! The map checks need a pack carrying `map/`; a pack converted before M16
//! fails loudly instead of skipping.
//!
//! # Golden frames
//!
//! Set `ARKLAY_RE1_GOLDEN` to a directory with a `hashes_fnv.txt` file (one
//! `scenario <hex fnv1a>` line each) and optionally `<scenario>.bmp` frames.
//! The goldens were generated locally from the original game by the project
//! author and are never committed: this harness is a regression and
//! cross-implementation check, **not** an independently produced oracle. Only
//! an unset variable skips; a set variable with a missing or malformed golden
//! set fails.

use crate::common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use arklay::bmp;
use arklay::pack::Pack;

/// Self-deleting temporary directory unique to this process and label.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("arklay-m16ui-{}-{label}", std::process::id()));
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

/// FNV-1a over the decoded frame's RGB bytes in pixel order.
fn fnv1a_rgb(bmp_bytes: &[u8]) -> u64 {
    let image = bmp::decode(bmp_bytes).expect("the capture is a BMP");
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for pixel in image.rgba.as_chunks::<4>().0 {
        for &channel in &pixel[..3] {
            hash ^= u64::from(channel);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    hash
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_ui_captures_are_deterministic_and_the_map_draws() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    assert!(
        pack.contains("map/map0e.tim"),
        "ARKLAY_RE1_PACK predates M16's map entries; re-convert the install"
    );

    let dir = TempDir::new("ui-captures");
    let mut captures = HashMap::new();
    for screen in ["select", "menu", "map", "view"] {
        let first = dir.0.join(format!("{screen}-a.bmp"));
        let second = dir.0.join(format!("{screen}-b.bmp"));
        let a = cli_capture(&pack_path, &["--ui", screen], &first);
        let b = cli_capture(&pack_path, &["--ui", screen], &second);
        assert_eq!(a, b, "--ui {screen} is deterministic");
        captures.insert(screen, a);
    }
    assert_ne!(
        captures["menu"], captures["map"],
        "the map tab draws its floor plan over the frozen menu"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn converted_packs_carry_no_map_tables() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    assert!(
        !pack.contains("map/tables.bin"),
        "a converted pack carries no map/tables.bin; the map tab uses the built-ins"
    );
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_item_examine_combos_gate_the_description() {
    use arklay::text::Text;
    use arklay::ui::item_view::{
        ConfirmOutcome, ExamineContext, ExamineEvent, ExamineInput, ItemViewScreen,
    };

    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let text = Text::load(&pack, Some("en-US"));
    let open = |item| {
        let mut view = ItemViewScreen::new(item);
        view.open_with(&pack, &text);
        for _ in 0..0x100 {
            view.step_examine(ExamineInput::default());
            if view.turntable_ready() {
                return view;
            }
        }
        panic!("the viewer intro never settled");
    };

    // The sword key (0x33) is one of the rotation-gated items: record 1 wants
    // yaw near 0 and the roll window at 0x5D0..0xA30. Aligned, it shows its
    // class message and marks the class examined.
    let mut view = open(0x33);
    view.pitch = 0x0F00;
    view.roll = 0x800;
    assert!(matches!(
        view.confirm(ExamineContext::default()),
        ConfirmOutcome::Message {
            mark_examined: true,
            ..
        }
    ));

    // The resting pose is outside the window: the plain description.
    let mut view = open(0x33);
    assert_eq!(
        view.confirm(ExamineContext::default()),
        ConfirmOutcome::Description
    );

    // The red book's matched pose takes the zoom, which ends on its message.
    let mut view = open(0x3E);
    view.yaw = 0x400;
    assert_eq!(
        view.confirm(ExamineContext::default()),
        ConfirmOutcome::Zoom
    );
    let mut ticks = 0;
    let event = loop {
        ticks += 1;
        assert!(ticks < 200, "the red book zoom never finished");
        match view.step_examine(ExamineInput::default()) {
            ExamineEvent::None => {}
            event => break event,
        }
    };
    assert!(matches!(event, ExamineEvent::Message(_)));
}

/// The sampled golden-frame scenarios, in `hashes_fnv.txt` order.
const GOLDEN_SCENARIOS: &[(&str, &[&str])] = &[
    ("room100", &["--room", "100"]),
    ("room107", &["--room", "107"]),
    ("room108", &["--room", "108"]),
    ("room300", &["--room", "300"]),
    ("room406", &["--room", "406"]),
    ("room506", &["--room", "506"]),
    ("npc_20d", &["--room", "20D", "--ticks", "30"]),
    ("message_107", &["--room", "107", "--ticks", "30"]),
    ("ui_title", &["--ui", "title"]),
    ("ui_select", &["--ui", "select"]),
    ("ui_menu", &["--ui", "menu"]),
    ("ui_map", &["--ui", "map"]),
    ("ui_view", &["--ui", "view"]),
];

/// The configured golden directory, or `None` when the variable is unset. A
/// set variable with a missing set fails loudly.
fn golden_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("ARKLAY_RE1_GOLDEN").ok()?);
    assert!(
        dir.is_dir(),
        "ARKLAY_RE1_GOLDEN is set but {} is not a directory",
        dir.display()
    );
    assert!(
        dir.join("hashes_fnv.txt").is_file(),
        "ARKLAY_RE1_GOLDEN is set but {} is missing",
        dir.join("hashes_fnv.txt").display()
    );
    Some(dir)
}

#[test]
#[ignore = "requires ARKLAY_RE1_GOLDEN (and both asset variables)"]
fn golden_frames_match_the_sampled_set() {
    let Some(golden) = golden_dir() else {
        println!(
            "note: ARKLAY_RE1_GOLDEN is unset; skipping the golden comparison. The goldens \
             are generated locally from the original game by the project author and are never \
             committed: this harness is a regression/cross-check, not an independent oracle."
        );
        return;
    };
    let Some((_root, pack_path)) = common::asset_env() else {
        panic!("ARKLAY_RE1_GOLDEN is set but ARKLAY_RE1_ROOT/ARKLAY_RE1_PACK are not");
    };
    let _pack = Pack::open(&pack_path).unwrap();
    let hashes: HashMap<String, u64> = fs::read_to_string(golden.join("hashes_fnv.txt"))
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next().expect("a scenario name").to_string();
            let hash = u64::from_str_radix(fields.next().expect("a hex hash"), 16)
                .unwrap_or_else(|_| panic!("golden hash for {name} is not hex"));
            (name, hash)
        })
        .collect();

    println!(
        "golden frames: {0} scenarios against {1} (provenance: locally generated from the \
         original game by the project author; not an independent oracle)",
        GOLDEN_SCENARIOS.len(),
        golden.display()
    );
    let dir = TempDir::new("golden");
    for (name, args) in GOLDEN_SCENARIOS {
        let out = dir.0.join(format!("{name}.bmp"));
        let bytes = cli_capture(&pack_path, args, &out);
        let expected = hashes
            .get(*name)
            .unwrap_or_else(|| panic!("the golden set has no hash for {name}"));
        assert_eq!(fnv1a_rgb(&bytes), *expected, "{name} frame hash ({args:?})");
        // An optional BMP golden is compared pixel for pixel as well.
        let golden_bmp = golden.join(format!("{name}.bmp"));
        if golden_bmp.is_file() {
            let golden_bytes = fs::read(&golden_bmp).unwrap();
            assert_eq!(
                bmp::decode(&bytes).unwrap().rgba,
                bmp::decode(&golden_bytes).unwrap().rgba,
                "{name} frame pixels"
            );
        }
    }
}
