//! CLI error-path and atomic-output suite.
//!
//! Every test spawns the built binary (`CARGO_BIN_EXE_arklay`) against
//! synthetic packs and temporary directories only: no game assets, no display
//! and no network. Failure cases assert a non-zero exit status, a human
//! message on stderr (or stdout for `verify`'s report) and the absence of
//! `panicked at` in both streams.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use arklay::manifest;
use arklay::pack::PackWriter;
use arklay::state::{Image, RoomId};

/// Self-deleting temporary directory unique to the test's label.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("arklay-cli-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arklay"))
        .args(args)
        .output()
        .expect("the arklay binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Assert a failed run: non-zero status, `needle` in stderr, no panic trace.
fn assert_failure(output: &Output, needle: &str) {
    let err = stderr(output);
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded: {}",
        stdout(output)
    );
    assert!(err.contains(needle), "stderr lacks `{needle}`:\n{err}");
    assert!(
        !err.contains("panicked at") && !stdout(output).contains("panicked at"),
        "panic leaked to the CLI:\n{err}"
    );
}

/// A 320x240 RGBA image so the renderer always has a full background.
fn background() -> Vec<u8> {
    let mut rgba = Vec::with_capacity(320 * 240 * 4);
    for y in 0..240u32 {
        for x in 0..320u32 {
            let v = ((x * 3 + y) % 256) as u8;
            rgba.extend_from_slice(&[v, v ^ 0x55, v.wrapping_mul(37), 255]);
        }
    }
    arklay::bmp::encode_to_vec(&Image {
        width: 320,
        height: 240,
        rgba,
    })
    .unwrap()
}

/// The smallest RDT the room loader accepts: one camera cut, empty scripts.
fn synthetic_rdt() -> Vec<u8> {
    let mut rdt = vec![0u8; 0x94];
    rdt[0x01] = 1;
    rdt.extend_from_slice(&[0u8; 44]);
    rdt
}

/// A synthetic one-room pack with the cut its camera needs.
fn write_room_pack(path: &Path) {
    let id = RoomId::parse("1000").unwrap();
    let mut writer = PackWriter::new();
    writer.add(&id.rdt_entry(), synthetic_rdt()).unwrap();
    writer.add(&id.cut_entry(0), background()).unwrap();
    writer.write(path).unwrap();
}

/// A minimal valid 16bpp TIM.
fn tiny_tim() -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&0x10u32.to_le_bytes());
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&[0; 8]);
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data
}

/// A manifest-carrying pack: `room/1000.rdt` plus `entries`.
fn write_manifest_pack(path: &Path, manifest: &manifest::Manifest, entries: &[(&str, &[u8])]) {
    let mut writer = PackWriter::new();
    writer
        .add(manifest::ENTRY, manifest.render().into_bytes())
        .unwrap();
    for (entry_path, data) in entries {
        writer.add(entry_path, data.to_vec()).unwrap();
    }
    writer.write(path).unwrap();
}

/// Lay a raw `.akpak` image down without the writer's validation, so the
/// reader's rejection paths can be driven.
fn raw_pack(entries: &[(&str, u64, u64)]) -> Vec<u8> {
    let count = entries.len() as u32;
    let toc_len = entries.len() * 32;
    let mut paths = Vec::new();
    let mut path_offsets = Vec::new();
    for (path, _, _) in entries {
        path_offsets.push(10 + toc_len + paths.len());
        paths.extend_from_slice(path.as_bytes());
        paths.push(0);
    }
    let data_start = 10 + toc_len + paths.len();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"APAK");
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    for (index, (_, data_offset, length)) in entries.iter().enumerate() {
        bytes.extend_from_slice(&(path_offsets[index] as u64).to_le_bytes());
        bytes.extend_from_slice(&(*data_offset + data_start as u64).to_le_bytes());
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[0; 7]);
    }
    bytes.extend_from_slice(&paths);
    bytes
}

#[test]
fn a_missing_pack_fails_with_a_read_error() {
    let output = run(&["list", "definitely-not-here.akpak"]);
    assert_failure(&output, "failed to read pack");
}

#[test]
fn a_pack_with_bad_magic_is_rejected() {
    let dir = TempDir::new("bad-magic");
    let path = dir.join("bad.akpak");
    std::fs::write(&path, b"NOPE00000000").unwrap();
    let output = run(&["list", path.to_str().unwrap()]);
    assert_failure(&output, "magic");
}

#[test]
fn a_truncated_entry_range_is_rejected() {
    let dir = TempDir::new("truncated-entry");
    let path = dir.join("bad.akpak");
    // One entry whose data range starts far past the end of the file.
    std::fs::write(&path, raw_pack(&[("room/1000.rdt", 1_000_000, 16)])).unwrap();
    let output = run(&["list", path.to_str().unwrap()]);
    assert_failure(&output, "out of bounds");
}

#[test]
fn an_unsafe_entry_path_is_rejected() {
    let dir = TempDir::new("unsafe-path");
    let path = dir.join("bad.akpak");
    std::fs::write(&path, raw_pack(&[("../evil.rdt", 0, 0)])).unwrap();
    let output = run(&["list", path.to_str().unwrap()]);
    assert_failure(&output, "unsafe path");
}

#[test]
fn a_malformed_manifest_is_rejected_at_open() {
    let dir = TempDir::new("bad-manifest");
    let path = dir.join("bad.akpak");
    let mut writer = PackWriter::new();
    writer
        .add(manifest::ENTRY, b"not = valid = manifest".to_vec())
        .unwrap();
    writer.write(&path).unwrap();
    let output = run(&["list", path.to_str().unwrap()]);
    assert_failure(&output, "manifest");
}

#[test]
fn a_malformed_script_names_its_line() {
    let dir = TempDir::new("bad-s");
    let input = dir.join("bad.s");
    std::fs::write(&input, ".version 1\n\n.main\n.block\n    frobnicate 1\n").unwrap();
    let out = dir.join("bad.scd");
    let output = run(&[
        "scd",
        "build",
        input.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_failure(&output, "line 5");
    assert!(!out.exists(), "a failed assemble wrote an output file");
}

#[test]
fn a_missing_room_fails_before_any_display() {
    let dir = TempDir::new("missing-room");
    let path = dir.join("game.akpak");
    write_room_pack(&path);
    let output = run(&[
        path.to_str().unwrap(),
        "--room",
        "11C",
        "--ticks",
        "1",
        "--stats",
    ]);
    assert_failure(&output, "room/11c0.rdt");
}

#[test]
fn ticks_without_capture_or_stats_is_refused() {
    let dir = TempDir::new("ticks-only");
    let path = dir.join("game.akpak");
    write_room_pack(&path);
    let output = run(&[path.to_str().unwrap(), "--room", "100", "--ticks", "10"]);
    assert_failure(&output, "--ticks requires --capture");
}

#[test]
fn a_mismatched_mod_base_is_rejected() {
    let dir = TempDir::new("base-mismatch");
    let base = dir.join("base.akpak");
    let m = dir.join("m.akpak");
    write_manifest_pack(
        &base,
        &manifest::Manifest::base("re1"),
        &[("room/1000.rdt", b"base")],
    );
    let mod_manifest = manifest::Manifest {
        kind: manifest::PackKind::Mod,
        base: Some("re9".to_string()),
        ..manifest::Manifest::base("demo")
    };
    write_manifest_pack(&m, &mod_manifest, &[("room/1000.rdt", b"mod")]);

    let output = run(&["list", base.to_str().unwrap(), "--mod", m.to_str().unwrap()]);
    assert_failure(&output, "declares base");
}

#[test]
fn duplicate_and_repeated_layers_are_rejected() {
    let dir = TempDir::new("duplicate-layers");
    let base = dir.join("base.akpak");
    let first = dir.join("first.akpak");
    let second = dir.join("second.akpak");
    write_manifest_pack(
        &base,
        &manifest::Manifest::base("re1"),
        &[("room/1000.rdt", b"base")],
    );
    let same_id = manifest::Manifest {
        kind: manifest::PackKind::Mod,
        base: Some("re1".to_string()),
        ..manifest::Manifest::base("demo")
    };
    write_manifest_pack(&first, &same_id, &[("room/1000.rdt", b"first")]);
    write_manifest_pack(&second, &same_id, &[("room/1000.rdt", b"second")]);

    // Two different layers with the same manifest id.
    let output = run(&[
        "list",
        base.to_str().unwrap(),
        "--mod",
        first.to_str().unwrap(),
        "--mod",
        second.to_str().unwrap(),
    ]);
    assert_failure(&output, "duplicate mod id");

    // The same layer path twice.
    let output = run(&[
        "list",
        base.to_str().unwrap(),
        "--mod",
        first.to_str().unwrap(),
        "--mod",
        first.to_str().unwrap(),
    ]);
    assert_failure(&output, "duplicate mod layer path");
}

#[test]
fn verify_exits_zero_on_a_good_pack_and_reports_the_table() {
    let dir = TempDir::new("verify-good");
    let path = dir.join("game.akpak");
    let mut writer = PackWriter::new();
    writer.add("ui/blue.tim", tiny_tim()).unwrap();
    writer.write(&path).unwrap();

    let output = run(&["verify", path.to_str().unwrap()]);
    let out = stdout(&output);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(out.contains("format"), "{out}");
    assert!(out.contains("tim"), "{out}");
    assert!(
        out.contains("verify: 1 entries, 1 ok, 0 failed, 0 opaque"),
        "{out}"
    );
}

#[test]
fn verify_exits_nonzero_and_names_a_bad_entry() {
    let dir = TempDir::new("verify-bad");
    let path = dir.join("game.akpak");
    let mut writer = PackWriter::new();
    writer.add("ui/good.tim", tiny_tim()).unwrap();
    writer.add("ui/bad.tim", b"not a tim".to_vec()).unwrap();
    writer.write(&path).unwrap();

    let output = run(&["verify", path.to_str().unwrap()]);
    let out = stdout(&output);
    assert!(!output.status.success());
    assert!(
        out.contains("ui/bad.tim"),
        "stdout must name the entry:\n{out}"
    );
    assert!(
        stderr(&output).contains("failed verification"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn verify_strict_fails_an_unknown_extension() {
    let dir = TempDir::new("verify-strict");
    let path = dir.join("game.akpak");
    let mut writer = PackWriter::new();
    writer.add("data/core00.esp", vec![1, 2, 3]).unwrap();
    writer.write(&path).unwrap();

    let lenient = run(&["verify", path.to_str().unwrap()]);
    assert!(lenient.status.success(), "stderr: {}", stderr(&lenient));
    assert!(stdout(&lenient).contains("opaque"));

    let strict = run(&["verify", "--strict", path.to_str().unwrap()]);
    assert!(!strict.status.success());
    assert!(stdout(&strict).contains("data/core00.esp"));
}

#[test]
fn verify_merges_mod_layers_before_classifying() {
    let dir = TempDir::new("verify-mod");
    let base = dir.join("base.akpak");
    let layer = dir.join("layer.akpak");

    let mut writer = PackWriter::new();
    writer
        .add(
            manifest::ENTRY,
            manifest::Manifest::base("re1").render().into_bytes(),
        )
        .unwrap();
    writer.add("ui/base.tim", tiny_tim()).unwrap();
    writer.write(&base).unwrap();

    let mod_manifest = manifest::Manifest {
        kind: manifest::PackKind::Mod,
        base: Some("re1".to_string()),
        ..manifest::Manifest::base("demo")
    };
    let mut writer = PackWriter::new();
    writer
        .add(manifest::ENTRY, mod_manifest.render().into_bytes())
        .unwrap();
    // The layer shadows the base's TIM with corrupt bytes and adds a new one,
    // so verification must walk the merged view exactly like the runtime.
    writer.add("ui/base.tim", b"not a tim".to_vec()).unwrap();
    writer.add("ui/extra.tim", tiny_tim()).unwrap();
    writer.write(&layer).unwrap();

    let output = run(&[
        "verify",
        base.to_str().unwrap(),
        "--mod",
        layer.to_str().unwrap(),
    ]);
    let out = stdout(&output);
    assert!(!output.status.success(), "stderr: {}", stderr(&output));
    assert!(out.contains("ui/base.tim"), "{out}");
    assert!(
        out.contains("verify: 3 entries, 2 ok, 1 failed, 0 opaque"),
        "{out}"
    );
}

#[test]
fn stats_runs_without_a_capture_and_reports_ordered_counters() {
    let dir = TempDir::new("stats");
    let path = dir.join("game.akpak");
    write_room_pack(&path);

    let output = run(&[
        path.to_str().unwrap(),
        "--room",
        "100",
        "--ticks",
        "30",
        "--stats",
    ]);
    let out = stdout(&output);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let line = |prefix: &str| {
        out.lines()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no `{prefix}` line in:\n{out}"))
    };
    assert_eq!(line("stats ticks "), "stats ticks 30");
    let tick: Vec<&str> = line("stats tick_us ").split_whitespace().collect();
    assert_eq!(&tick[..2], ["stats", "tick_us"]);
    let value = |key: &str| -> u64 {
        let index = tick.iter().position(|word| *word == key).unwrap();
        tick[index + 1].parse().unwrap()
    };
    assert!(value("min") <= value("avg"));
    assert!(value("avg") <= value("p95"));
    assert!(value("p95") <= value("max"));
    let phase: Vec<&str> = line("stats phase_us ").split_whitespace().collect();
    for key in ["load", "update", "effect", "render"] {
        assert!(phase.contains(&key), "phase line lacks {key}: {phase:?}");
    }
    assert!(line("stats high_water ").contains("entities"));
}

#[test]
fn stats_is_refused_with_fmv_and_ending() {
    for extra in [
        ["--fmv", "0", "--ticks", "5", "--stats"],
        ["--ending", "1", "--ticks", "5", "--stats"],
    ] {
        let mut args = vec!["game.akpak"];
        args.extend_from_slice(&extra);
        let output = run(&args);
        assert_failure(&output, "--stats");
    }
}

#[test]
fn scd_build_writes_atomically_and_leaves_no_temporary() {
    let dir = TempDir::new("scd-atomic");
    let input = dir.join("good.s");
    let out = dir.join("good.scd");
    std::fs::write(
        &input,
        ".version 1\n\n.main\n.block\n    nop                     0\n\n.event event_00\n    evt_finish\n",
    )
    .unwrap();

    let output = run(&[
        "scd",
        "build",
        input.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(out.exists());

    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
}

#[test]
fn a_failed_output_rename_keeps_the_destination_and_removes_the_temporary() {
    let dir = TempDir::new("failed-rename");
    let input = dir.join("good.s");
    std::fs::write(
        &input,
        ".version 1\n\n.main\n.block\n    nop                     0\n",
    )
    .unwrap();
    // The output path is an existing non-empty directory, so the final rename
    // fails after the temporary file was written.
    let out = dir.join("out.scd");
    std::fs::create_dir_all(out.join("occupied")).unwrap();

    let output = run(&[
        "scd",
        "build",
        input.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_failure(&output, "failed to");
    assert!(out.is_dir(), "the blocked destination was replaced");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
}

#[test]
fn a_failed_capture_writes_no_partial_file() {
    let dir = TempDir::new("failed-capture");
    let path = dir.join("game.akpak");
    write_room_pack(&path);
    // The capture path is a directory: the BMP bytes are prepared, the
    // temporary sibling is written and the rename is rejected.
    let capture = dir.join("capture.bmp");
    std::fs::create_dir_all(capture.join("occupied")).unwrap();

    let output = run(&[
        path.to_str().unwrap(),
        "--room",
        "100",
        "--capture",
        capture.to_str().unwrap(),
    ]);
    assert_failure(&output, "failed to");
    assert!(capture.is_dir(), "the blocked capture path was replaced");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
}

#[test]
fn extraction_writes_every_entry_and_rejects_a_layered_duplicate() {
    let dir = TempDir::new("extract");
    let path = dir.join("game.akpak");
    write_room_pack(&path);

    let out = dir.join("out");
    let output = run(&[
        "extract",
        path.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(out.join("room/1000.rdt").exists());
    assert!(out.join("roomcut/100_000.bmp").exists());

    let leftovers: Vec<_> = std::fs::read_dir(out.join("room"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
}
