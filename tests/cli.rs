//! CLI error-path and atomic-output suite.
//!
//! Every test spawns the built binary (`CARGO_BIN_EXE_arklay`) against
//! synthetic packs and temporary directories only: no game assets, no display
//! and no network. Failure cases assert a non-zero exit status, a human
//! message on stderr (or stdout for `verify`'s report) and the absence of
//! `panicked at` in both streams.

use std::fs;
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
    run_env(args, &[])
}

/// [`run`] with extra environment variables.
///
/// Every run points `ARKLAY_CONFIG_DIR` at a directory that does not exist, so
/// a developer's own `arklay.toml` cannot change a test's behaviour; a test
/// that wants a config names the variable itself.
fn run_env(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arklay"));
    command.args(args);
    command.env(
        "ARKLAY_CONFIG_DIR",
        std::env::temp_dir().join("arklay-cli-no-config"),
    );
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("the arklay binary runs")
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

/// A 2x2 RGBA PNG, hand-built so the suite needs no encoder.
fn tiny_png() -> Vec<u8> {
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72,
        0xB6, 0x0D, 0x24, 0x00, 0x00, 0x00, 0x14, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xF8,
        0xCF, 0xC0, 0xF0, 0x1F, 0x0C, 0x81, 0x34, 0x10, 0x30, 0xFC, 0x07, 0x00, 0x47, 0xCA, 0x08,
        0xF8, 0x5B, 0x9A, 0xA4, 0xBE, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42,
        0x60, 0x82,
    ];
    TINY_PNG.to_vec()
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
fn directory_packs_and_directory_mods_list_and_extract() {
    let dir = TempDir::new("directory-pack");
    let game = dir.join("game");
    std::fs::create_dir_all(game.join("room")).unwrap();
    std::fs::write(
        game.join(manifest::ENTRY),
        manifest::Manifest::base("re1").render(),
    )
    .unwrap();
    std::fs::write(game.join("room/1000.rdt"), b"base").unwrap();
    std::fs::write(game.join("notes.txt"), b"notes").unwrap();

    let output = run(&["list", game.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let listed = stdout(&output);
    assert!(listed.contains("room/1000.rdt"), "{listed}");
    assert!(listed.contains("manifest.toml"), "{listed}");

    let mod_dir = dir.join("moddir");
    std::fs::create_dir_all(mod_dir.join("room")).unwrap();
    let mod_manifest = manifest::Manifest {
        kind: manifest::PackKind::Mod,
        base: Some("re1".to_string()),
        ..manifest::Manifest::base("demo")
    };
    std::fs::write(mod_dir.join(manifest::ENTRY), mod_manifest.render()).unwrap();
    std::fs::write(mod_dir.join("room/1000.rdt"), b"mod").unwrap();

    let out = dir.join("out");
    let output = run(&[
        "extract",
        game.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--mod",
        mod_dir.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        std::fs::read(out.join("room/1000.rdt")).unwrap(),
        b"mod",
        "the directory mod layer did not win"
    );
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
fn verify_validates_the_checked_in_hd_mods_png_and_ogg_replacements() {
    // The `mods/hd` example ships a PNG background, a PNG UI page and an OGG
    // BGM track; `mod build` packs them verbatim and `verify` decodes each
    // with the replacement-format decoder.
    let dir = TempDir::new("verify-hd");
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mods/hd");
    let pack = dir.join("hd.akpak");
    let built = run(&[
        "mod",
        "build",
        source.to_str().unwrap(),
        "--out",
        pack.to_str().unwrap(),
    ]);
    assert!(built.status.success(), "stderr: {}", stderr(&built));

    let output = run(&["verify", pack.to_str().unwrap()]);
    let out = stdout(&output);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(out.contains("roomcut"), "{out}");
    assert!(out.contains("png"), "{out}");
    assert!(out.contains("webp"), "{out}");
    assert!(out.contains("ogg"), "{out}");
    assert!(
        out.contains("verify: 5 entries, 5 ok, 0 failed, 0 opaque"),
        "{out}"
    );

    // A replacement whose format is refused fails verification by name.
    let bad = dir.join("bad.akpak");
    let mut writer = PackWriter::new();
    writer
        .add(
            manifest::ENTRY,
            manifest::Manifest::base("bad").render().into_bytes(),
        )
        .unwrap();
    writer.add("bgm/013.ogg", b"not vorbis".to_vec()).unwrap();
    writer.write(&bad).unwrap();
    let output = run(&["verify", bad.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(
        stdout(&output).contains("bgm/013.ogg"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn mod_build_does_not_warn_about_a_format_override() {
    // A replacement whose stem matches a base entry is an override, not a new
    // file, whatever its extension.
    let dir = TempDir::new("override-warn");
    let base = dir.join("base.akpak");
    let source = dir.join("mod");
    fs::create_dir_all(source.join("roomcut")).unwrap();
    fs::create_dir_all(source.join("ui")).unwrap();
    fs::create_dir_all(source.join("extra")).unwrap();
    fs::create_dir_all(source.join("bgm")).unwrap();
    let mut writer = PackWriter::new();
    writer
        .add(
            manifest::ENTRY,
            manifest::Manifest::base("re1").render().into_bytes(),
        )
        .unwrap();
    writer.add("roomcut/100_000.bmp", background()).unwrap();
    writer.add("ui/itemboxn.tim", tiny_tim().to_vec()).unwrap();
    writer.add("bgm/013.wav", b"RIFF".to_vec()).unwrap();
    writer.write(&base).unwrap();

    fs::write(
        source.join("manifest.toml"),
        manifest::Manifest {
            kind: manifest::PackKind::Mod,
            base: Some("re1".to_string()),
            ..manifest::Manifest::base("overrides")
        }
        .render(),
    )
    .unwrap();
    // A PNG over the BMP background, an OGG over the WAV sample and a file the
    // base cannot serve.
    fs::write(source.join("roomcut/100_000.png"), tiny_png()).unwrap();
    fs::write(source.join("bgm/013.ogg"), b"not vorbis").unwrap();
    fs::write(source.join("extra/new.bin"), b"new").unwrap();

    let out = dir.join("overrides.akpak");
    let built = run(&[
        "mod",
        "build",
        source.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--base",
        base.to_str().unwrap(),
    ]);
    assert!(built.status.success(), "stderr: {}", stderr(&built));
    let warnings = stdout(&built);
    assert!(
        warnings.contains("1 override(s) are new files"),
        "only the extra file is new: {warnings}"
    );
    assert!(warnings.contains("extra/new.bin"), "{warnings}");
    assert!(!warnings.contains("100_000.png"), "{warnings}");
    assert!(!warnings.contains("013.ogg"), "{warnings}");
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
        "--frame-rate",
        "60",
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
    // Two presentation frames a tick at 60 fps, timed against 1/60 s.
    let render: Vec<&str> = line("stats render ").split_whitespace().collect();
    let field = |key: &str| render[render.iter().position(|word| *word == key).unwrap() + 1];
    assert_eq!(field("frame_rate"), "60");
    assert_eq!(field("frames"), "60");
    assert_eq!(field("budget_us"), "16666");
}

#[test]
fn a_configured_quick_turn_never_changes_a_trace() {
    let dir = TempDir::new("quick-turn-trace");
    let pack = dir.join("game.akpak");
    write_room_pack(&pack);
    let config = dir.join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("arklay.toml"), "[game]\nquick_turn = true\n").unwrap();
    let trace = |name: &str, extra: &[&str], env: &[(&str, &str)]| {
        let path = dir.join(name);
        let mut args = vec![
            pack.to_str().unwrap(),
            "--room",
            "100",
            "--ticks",
            "30",
            "--trace",
            path.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let output = run_env(&args, env);
        assert!(output.status.success(), "stderr: {}", stderr(&output));
        std::fs::read(&path).unwrap()
    };
    let plain = trace("plain.trace", &[], &[]);
    assert!(!plain.is_empty());
    let config_dir = config.to_str().unwrap();
    assert!(
        plain
            == trace(
                "configured.trace",
                &[],
                &[("ARKLAY_CONFIG_DIR", config_dir)]
            ),
        "a configured quick turn changed a --trace run without --quick-turn"
    );
    assert!(
        plain
            == trace(
                "forced_off.trace",
                &["--no-quick-turn"],
                &[("ARKLAY_CONFIG_DIR", config_dir)]
            ),
        "--no-quick-turn changed the trace"
    );
}

#[test]
fn the_frame_rate_never_changes_a_trace() {
    let dir = TempDir::new("frame-rate-trace");
    let pack = dir.join("game.akpak");
    write_room_pack(&pack);
    let trace = |name: &str, extra: &[&str]| {
        let path = dir.join(name);
        let mut args = vec![
            pack.to_str().unwrap(),
            "--room",
            "100",
            "--ticks",
            "30",
            "--trace",
            path.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let output = run(&args);
        assert!(output.status.success(), "stderr: {}", stderr(&output));
        (std::fs::read(&path).unwrap(), stdout(&output))
    };
    let (plain, _) = trace("plain.trace", &[]);
    let (tweened, out) = trace("tweened.trace", &["--frame-rate", "120", "--stats"]);
    assert!(!plain.is_empty());
    assert!(plain == tweened, "a 120 fps --stats run changed the trace");
    assert!(
        out.contains("stats render frame_rate 120 frames 120 "),
        "{out}"
    );
    // Without --frame-rate a headless report draws no presentation frames,
    // whatever the config says.
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("arklay.toml"), "[graphics]\nframe_rate = 240\n").unwrap();
    let output = run_env(
        &[
            pack.to_str().unwrap(),
            "--room",
            "100",
            "--ticks",
            "30",
            "--stats",
        ],
        &[("ARKLAY_CONFIG_DIR", config.to_str().unwrap())],
    );
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(
        !stdout(&output).contains("stats render"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn the_tactical_reload_never_changes_a_trace() {
    let dir = TempDir::new("tactical-reload-trace");
    let pack = dir.join("game.akpak");
    write_room_pack(&pack);
    let config = dir.join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("arklay.toml"),
        "[game]\ntactical_reload = true\n",
    )
    .unwrap();
    let trace = |name: &str, env: &[(&str, &str)]| {
        let path = dir.join(name);
        let output = run_env(
            &[
                pack.to_str().unwrap(),
                "--room",
                "100",
                "--ticks",
                "30",
                "--trace",
                path.to_str().unwrap(),
            ],
            env,
        );
        assert!(output.status.success(), "stderr: {}", stderr(&output));
        fs::read(&path).unwrap()
    };
    let plain = trace("plain.trace", &[]);
    let configured = trace(
        "configured.trace",
        &[("ARKLAY_CONFIG_DIR", config.to_str().unwrap())],
    );
    assert!(!plain.is_empty());
    assert!(
        plain == configured,
        "the config's tactical reload changed a trace"
    );
    // The synthetic room carries no player weapon bank, so the reload itself
    // cannot fire here; the flag resolution is unit-tested in `src/main.rs`.
    // This covers the config table being accepted while a trace stays
    // byte-identical, the end-to-end half of the trace-invariance guarantee.
}

#[test]
fn a_frame_rate_outside_30_to_1000_is_refused() {
    for rate in ["0", "29", "1001"] {
        let output = run(&["game.akpak", "--room", "100", "--frame-rate", rate]);
        assert_failure(&output, "--frame-rate");
    }
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
fn the_config_file_is_read_and_a_bad_value_is_a_warning() {
    let dir = TempDir::new("config");
    let path = dir.join("game.akpak");
    write_room_pack(&path);
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("arklay.toml");
    let capture = dir.join("capture.bmp");
    let saves = dir.join("cfg-saves");
    let config_env = config_dir.to_str().unwrap();

    // A config naming a save location: the load screen runs from it, and the
    // pack carries no Lua, so the ported original screen opens.
    std::fs::write(
        &config,
        // The literal string form keeps a Windows path valid without escaping.
        format!("save_dir = '{}'\n", saves.display()),
    )
    .unwrap();
    let output = run_env(
        &[
            path.to_str().unwrap(),
            "--ui",
            "load",
            "--capture",
            capture.to_str().unwrap(),
        ],
        &[("ARKLAY_CONFIG_DIR", config_env)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(capture.is_file());

    // A malformed value is reported as a warning and the run goes on with
    // its default.
    std::fs::write(&config, "save_dir = 3\n").unwrap();
    let output = run_env(
        &[
            path.to_str().unwrap(),
            "--ui",
            "load",
            "--capture",
            capture.to_str().unwrap(),
        ],
        &[("ARKLAY_CONFIG_DIR", config_env)],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("warning:") && stderr(&output).contains("expects a quoted string"),
        "{}",
        stderr(&output)
    );
}

/// A pack whose Lua hook registers a probe save screen: the screen logs
/// `saves=<count>`, the number of files in the save directory it was given.
fn write_lua_screen_pack(path: &Path) {
    let id = RoomId::parse("1000").unwrap();
    let rdt_entry = id.rdt_entry();
    let cut_entry = id.cut_entry(0);
    let rdt = synthetic_rdt();
    let cut = background();
    let manifest = manifest::Manifest {
        lua: vec!["lua/hook.lua".to_string()],
        ..manifest::Manifest::base("probe")
    };
    let hook = b"\
function on_init(api)
    api:register_save_screen(\"script/probe.lua\")
end
";
    let screen = b"\
function on_open(api)
    api:log(\"saves=\" .. #api:saves())
end
function update(api, input)
end
";
    write_manifest_pack(
        path,
        &manifest,
        &[
            (&rdt_entry, &rdt),
            (&cut_entry, &cut),
            ("lua/hook.lua", hook),
            ("script/probe.lua", screen),
        ],
    );
}

/// The config's `save_dir` is the directory the screen lists: the probe screen
/// logs the number of files it sees, so the count proves which directory was
/// read.
#[test]
fn the_config_save_dir_drives_a_registered_lua_screen() {
    let dir = TempDir::new("config-save-dir");
    let path = dir.join("game.akpak");
    write_lua_screen_pack(&path);
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("arklay.toml");
    let capture = dir.join("capture.bmp");
    let saves = dir.join("cfg-saves");
    let config_env = config_dir.to_str().unwrap();
    let load = || {
        run_env(
            &[
                path.to_str().unwrap(),
                "--ui",
                "load",
                "--capture",
                capture.to_str().unwrap(),
            ],
            &[("ARKLAY_CONFIG_DIR", config_env)],
        )
    };

    // The configured directory exists but is empty.
    std::fs::write(&config, format!("save_dir = '{}'\n", saves.display())).unwrap();
    let output = load();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(capture.is_file());
    assert!(stderr(&output).contains("saves=0"), "{}", stderr(&output));

    // A save written there is what the screen lists.
    let file = arklay::save::SaveFile::from_state(&arklay::game::GameState::default());
    std::fs::create_dir_all(&saves).unwrap();
    arklay::save::save_to_path(&saves.join("savedat1.dat"), &file).unwrap();
    let output = load();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("saves=1"), "{}", stderr(&output));

    // Without the key the screen reads the default location, not the
    // configured one, so the file that sits there is not listed.
    std::fs::remove_file(&config).unwrap();
    let output = load();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("saves=0"), "{}", stderr(&output));
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
