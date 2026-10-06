//! M15 real-asset tests: the layered pack and the conversion manifest.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=... ARKLAY_RE1_PACK=... \
//!  cargo test --test m15_real -- --ignored --nocapture`
//!
//! Only an unset environment skips; a partial configuration fails loudly. The
//! conversion test writes a full pack and takes minutes.

mod common;

use std::fs;
use std::path::PathBuf;

use arklay::convert;
use arklay::engine::{NEW_GAME_ROOM_ITEMS, ScriptSource, simulate_room, simulate_room_seeded};
use arklay::game::FlagBank;
use arklay::manifest;
use arklay::pack::{Pack, PackWriter};
use arklay::player;
use arklay::scd;
use arklay::state::RoomId;

/// Self-deleting temporary directory unique to this process and label.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("arklay-m15-{}-{label}", std::process::id()));
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

/// The rendered bytes of a mod manifest declaring `base`.
fn mod_manifest(base: &str) -> Vec<u8> {
    manifest::Manifest {
        kind: manifest::PackKind::Mod,
        base: Some(base.to_string()),
        load_order: 10,
        ..manifest::Manifest::base("layer")
    }
    .render()
    .into_bytes()
}

/// Collect every `.RDT` under `dir`, recursively.
fn collect_rdts(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rdts(&path, out);
        } else if path.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .to_ascii_uppercase()
                .ends_with(".RDT")
        }) {
            out.push(path);
        }
    }
}

/// One stream's body: `(kind, block sizes, instruction bytes, trailing)`.
type StreamBody = (String, Vec<u16>, Vec<u8>, Vec<u8>);

/// Every room identity the pack ships, sorted and deduplicated.
fn room_ids(pack: &Pack) -> Vec<RoomId> {
    let mut ids: Vec<RoomId> = pack
        .paths()
        .filter(|path| path.starts_with("room/") && path.ends_with(".rdt"))
        .filter_map(|path| RoomId::parse(&path[5..9]).ok())
        .collect();
    ids.sort_by_key(|id| (id.stage, id.room, id.player_flag));
    ids.dedup_by_key(|id| (id.stage, id.room, id.player_flag));
    ids
}

/// The stream bodies of `scripts`, ignoring the absolute offsets a container
/// lays the streams out at.
fn stream_bodies(scripts: &scd::ir::Scripts) -> Vec<StreamBody> {
    let mut out = Vec::new();
    for (index, block) in scripts.init.iter().enumerate() {
        let insns = block
            .insns
            .iter()
            .flat_map(|insn| insn.bytes.iter().copied())
            .collect();
        out.push((
            format!("init[{index}]"),
            vec![block.size],
            insns,
            block.trailing.clone(),
        ));
    }
    for (index, block) in scripts.main.iter().enumerate() {
        let insns = block
            .insns
            .iter()
            .flat_map(|insn| insn.bytes.iter().copied())
            .collect();
        out.push((
            format!("main[{index}]"),
            vec![block.size],
            insns,
            block.trailing.clone(),
        ));
    }
    for stream in &scripts.events {
        let insns = stream
            .insns
            .iter()
            .flat_map(|insn| insn.bytes.iter().copied())
            .collect();
        out.push((
            format!("{:?}", stream.kind),
            Vec::new(),
            insns,
            stream.trailing.clone(),
        ));
    }
    out
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn every_rdt_disassembles_reassembles_and_reparses() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let mut files = Vec::new();
    collect_rdts(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "no RDT files found under {}",
        root.display()
    );

    let mut rooms = 0usize;
    let mut events = 0usize;
    let mut insns = 0usize;
    for path in &files {
        let data = fs::read(path).unwrap();
        if data.len() <= 4 {
            continue;
        }
        let scripts = scd::reader::parse(&data)
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        let text = scd::disasm::render(&scripts, &data);
        let assembled = scd::asm::assemble(&text)
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        let container = assembled.to_container().unwrap();
        let reparsed = scd::reader::parse(&container)
            .unwrap_or_else(|error| panic!("{}: container: {error:#}", path.display()));

        assert_eq!(
            stream_bodies(&reparsed),
            stream_bodies(&scripts),
            "{}: stream bodies differ",
            path.display()
        );
        assert_eq!(
            scd::decomp::render(&reparsed, &container),
            scd::decomp::render(&scripts, &data),
            "{}: .bio differs",
            path.display()
        );
        events += reparsed.events.len();
        insns += reparsed
            .init
            .iter()
            .chain(&reparsed.main)
            .flat_map(|block| &block.insns)
            .count()
            + reparsed
                .events
                .iter()
                .flat_map(|stream| &stream.insns)
                .count();
        rooms += 1;
    }
    assert_eq!(rooms, 320, "expected the 320 shipped rooms");
    println!("round-tripped {rooms} rooms, {events} events, {insns} instructions");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn assembled_containers_load_in_the_engine() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    let base = Pack::open(&pack_path).unwrap();
    let dir = TempDir::new("engine-override");
    let mut files = Vec::new();
    collect_rdts(&root, &mut files);
    files.sort();

    let mut loaded = 0usize;
    for path in &files {
        let data = fs::read(path).unwrap();
        if data.len() <= 4 {
            continue;
        }
        // Map ROOM####.RDT to its RoomId.
        let stem = path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_ascii_uppercase();
        let Some(digits) = stem.strip_prefix("ROOM") else {
            continue;
        };
        let Ok(id) = RoomId::parse(digits) else {
            continue;
        };
        if !base.contains(&id.rdt_entry()) {
            continue;
        }
        let scripts = scd::reader::parse(&data).unwrap();
        let container = scd::asm::assemble(&scd::disasm::render(&scripts, &data))
            .unwrap()
            .to_container()
            .unwrap();

        let mod_path = dir.path.join(format!("{}.akpak", id.rdt_number()));
        let mut writer = PackWriter::new();
        writer.add(manifest::ENTRY, mod_manifest("re1")).unwrap();
        writer.add(&id.scd_entry(), container).unwrap();
        writer.write(&mod_path).unwrap();

        let layered = Pack::open_layered(&pack_path, std::slice::from_ref(&mod_path)).unwrap();
        let run = simulate_room(&layered, id, 1, player::Input::default())
            .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        assert_eq!(run.id, id);
        loaded += 1;
    }
    assert!(loaded > 0, "no rooms loaded from the real pack");
    println!("loaded {loaded} assembled containers through simulate_room");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn layer_shadows_real_entries_and_no_mod_is_identical() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let base = Pack::open(&pack_path).unwrap();

    let shadow = "room/1001.rdt";
    assert!(base.contains(shadow));
    let replacement = vec![0x5A; base.read(shadow).unwrap().len()];

    let dir = TempDir::new("layer");
    let mod_path = dir.path.join("layer.akpak");
    let mut writer = PackWriter::new();
    writer.add(manifest::ENTRY, mod_manifest("re1")).unwrap();
    writer.add(shadow, replacement.clone()).unwrap();
    writer.write(&mod_path).unwrap();

    // The shipped pack predates the manifest, so its stem id `re1` matches.
    let layered = Pack::open_layered(&pack_path, std::slice::from_ref(&mod_path)).unwrap();
    assert!(layered.is_layered());
    assert_eq!(layered.manifest().unwrap().id, "re1");
    assert_eq!(layered.read(shadow).unwrap(), replacement.as_slice());
    assert_eq!(
        layered.read(&shadow.to_ascii_uppercase()).unwrap(),
        replacement.as_slice()
    );
    assert_eq!(
        layered.read("bgm/013.wav").unwrap(),
        base.read("bgm/013.wav").unwrap()
    );
    // The shipped pack predates the manifest, so the one extra merged entry is
    // the layer's own `manifest.toml`.
    assert_eq!(layered.len(), base.len() + 1);
    assert!(layered.contains(manifest::ENTRY));
    assert_eq!(layered.layer_of(shadow), Some(mod_path.as_path()));

    // No mods at all: the layered view is the plain single-pack view, and the
    // deterministic headless seam sees exactly the same run.
    let plain = Pack::open_layered(&pack_path, &[]).unwrap();
    assert!(!plain.is_layered());
    assert_eq!(plain.len(), base.len());
    assert_eq!(
        plain.paths().collect::<Vec<_>>(),
        base.paths().collect::<Vec<_>>()
    );
    for path in base.paths() {
        assert_eq!(
            plain.read(path).unwrap(),
            base.read(path).unwrap(),
            "{path}"
        );
    }

    let id = RoomId::parse("1010").unwrap();
    let plain_run = simulate_room(&base, id, 30, player::Input::default()).unwrap();
    let layered_run = simulate_room(&plain, id, 30, player::Input::default()).unwrap();
    assert_eq!(plain_run.frame.rgba, layered_run.frame.rgba);
    assert_eq!(plain_run.baseline.rgba, layered_run.baseline.rgba);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn rider_opcodes_execute_without_placeholders() {
    use std::collections::{BTreeMap, BTreeSet};

    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();

    // List the rooms whose scripts carry each rider opcode, so any capture
    // hash that changes can be scoped to the rooms that execute it.
    let mut files = Vec::new();
    collect_rdts(&root, &mut files);
    files.sort();
    let riders = [0x2Cu8, 0x44, 0x4C];
    let mut carrying: BTreeMap<u8, BTreeSet<String>> = BTreeMap::new();
    for path in &files {
        let data = fs::read(path).unwrap();
        if data.len() <= 4 {
            continue;
        }
        let scripts = scd::reader::parse(&data).unwrap();
        let mut opcodes = BTreeSet::new();
        for insn in scripts
            .init
            .iter()
            .chain(&scripts.main)
            .flat_map(|block| &block.insns)
            .chain(scripts.events.iter().flat_map(|stream| &stream.insns))
        {
            opcodes.insert(insn.op);
        }
        for op in riders {
            if opcodes.contains(&op) {
                carrying
                    .entry(op)
                    .or_default()
                    .insert(path.file_stem().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    for op in riders {
        println!(
            "{op:#04x} carried by: {:?}",
            carrying
                .get(&op)
                .map(|rooms| rooms.iter().collect::<Vec<_>>())
        );
    }
    assert!(carrying.contains_key(&0x2C), "item_remove ships");
    assert!(carrying.contains_key(&0x44), "task_kill ships");
    let transfers = carrying.get(&0x4C).expect("item_record_transfer ships");
    for room in ["ROOM1160", "ROOM30B0", "ROOM3080"] {
        assert!(transfers.contains(room), "{room} uses item_record_transfer");
    }

    // Run the corpus with the new-game room-items bank and the
    // second-playthrough bit, so the gated scripts execute; a rider opcode
    // still implemented as a placeholder records a hit the moment it runs.
    let mut bank = FlagBank::new();
    bank.bytes_mut().copy_from_slice(&NEW_GAME_ROOM_ITEMS);
    let mut flags: Vec<(u8, u8)> = (0..=255u16)
        .filter(|bit| bank.bit(*bit as u8))
        .map(|bit| (7, bit as u8))
        .collect();
    flags.push((0, 0x7B));

    let mut simulated = 0usize;
    for id in room_ids(&pack) {
        let Ok(sim) = simulate_room_seeded(&pack, id, &flags, 120, player::Input::default()) else {
            continue;
        };
        simulated += 1;
        for op in riders {
            if let Some(count) = sim.game.placeholders.get(&op) {
                panic!("ROOM{id:?} dispatched rider {op:#04x} {count} times");
            }
        }
    }
    assert!(simulated > 300, "the corpus should run, saw {simulated}");
}

/// Build the checked-in demo sources through the `arklay mod build` binary.
fn build_demo_pack(dir: &std::path::Path) -> PathBuf {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mods/demo");
    let out = dir.join("demo.akpak");
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

/// Run the runtime capture CLI and return the written BMP bytes.
fn capture(
    pack: &std::path::Path,
    mods: &[&std::path::Path],
    no_mods: bool,
    out: &PathBuf,
) -> Vec<u8> {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arklay"));
    command.arg(pack);
    for layer in mods {
        command.arg("--mod").arg(layer);
    }
    if no_mods {
        command.arg("--no-mods");
    }
    command
        .args(["--room", "100", "--ticks", "40", "--capture"])
        .arg(out);
    let status = command.status().expect("failed to run the capture CLI");
    assert!(status.success(), "capture failed");
    fs::read(out).unwrap()
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn demo_pack_boots_room_1000_with_the_script_lua_and_background() {
    // `--room 100` is RDT 1000: the three-digit CLI id is stage 1, room 0x00.
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let base = Pack::open(&pack_path).unwrap();
    let temp = TempDir::new("demo");
    let demo_pack = build_demo_pack(&temp.path);
    let built = Pack::open(&demo_pack).unwrap();
    assert_eq!(built.len(), 4, "manifest, script, hooks and background");
    assert_eq!(built.manifest().unwrap().id, "demo");
    assert_eq!(built.manifest().unwrap().base.as_deref(), Some("re1"));

    // The replacement background is the demo's own art.
    assert_ne!(
        built.read("roomcut/100_000.bmp").unwrap(),
        base.read("roomcut/100_000.bmp").unwrap()
    );

    let layered = Pack::open_layered(&pack_path, std::slice::from_ref(&demo_pack)).unwrap();
    let id = RoomId::parse("1000").unwrap();
    let run = simulate_room(&layered, id, 40, player::Input::default()).unwrap();
    assert_eq!(run.script_source, ScriptSource::Override);
    assert!(run.game.flags[0].bit(1), "the demo init set its flag");
    assert!(run.game.flags[0].bit(2), "on_room_load set its flag");
    assert!(run.game.flags[0].bit(3), "the event ran");
    assert_eq!(
        run.game.message.id,
        Some(0),
        "the main script raised a message"
    );
    assert!(
        run.game.item_count(0x0F) >= 1,
        "on_tick granted the demo item"
    );
    for op in [0x2Cu8, 0x44, 0x4C] {
        assert!(
            !run.game.placeholders.contains_key(&op),
            "rider {op:#04x} is implemented"
        );
    }

    // The headless runs are deterministic, and the replacement background
    // reaches the frame: the demo frame differs from a vanilla one.
    let repeat = simulate_room(&layered, id, 40, player::Input::default()).unwrap();
    assert_eq!(run.frame.rgba, repeat.frame.rgba);
    let vanilla = simulate_room(&base, id, 40, player::Input::default()).unwrap();
    assert_ne!(run.frame.rgba, vanilla.frame.rgba);

    // The same acceptance through the CLI's `--capture`: deterministic with
    // `--mod`, and `--no-mods` gives today's vanilla capture byte-for-byte.
    let demo_first = temp.path.join("demo-first.bmp");
    let demo_second = temp.path.join("demo-second.bmp");
    let base_bmp = temp.path.join("base.bmp");
    let no_mods_bmp = temp.path.join("no-mods.bmp");
    let first = capture(&pack_path, &[&demo_pack], false, &demo_first);
    let second = capture(&pack_path, &[&demo_pack], false, &demo_second);
    let plain = capture(&pack_path, &[], false, &base_bmp);
    let ignored = capture(&pack_path, &[&demo_pack], true, &no_mods_bmp);
    assert_eq!(first, second, "the demo capture is deterministic");
    assert_ne!(first, plain, "the background override changed the frame");
    assert_eq!(ignored, plain, "--no-mods ignores every layer");
    assert_eq!(fs::read(&demo_first).unwrap(), first);
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn corpus_audit_runs_with_and_without_the_demo_layer() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let temp = TempDir::new("corpus-demo");
    let demo_pack = build_demo_pack(&temp.path);
    let layered = Pack::open_layered(&pack_path, std::slice::from_ref(&demo_pack)).unwrap();

    let mut plain_runs = 0usize;
    let mut layered_runs = 0usize;
    for id in room_ids(&pack) {
        if simulate_room(&pack, id, 5, player::Input::default()).is_ok() {
            plain_runs += 1;
        }
        if simulate_room(&layered, id, 5, player::Input::default()).is_ok() {
            layered_runs += 1;
        }
    }
    assert!(plain_runs > 300, "the corpus should run, saw {plain_runs}");
    assert_eq!(
        plain_runs, layered_runs,
        "the demo layer must not stop a room from running"
    );
    println!("corpus audit: {plain_runs} rooms with and without the demo layer");
}

#[test]
fn list_reports_layer_warnings_on_stderr_only() {
    let dir = TempDir::new("list-stderr");
    let base = dir.path.join("re1.akpak");
    let mut writer = PackWriter::new();
    writer.add("x.txt", b"base".to_vec()).unwrap();
    writer.write(&base).unwrap();

    let layer = dir.path.join("layer.akpak");
    let mut writer = PackWriter::new();
    writer.add(manifest::ENTRY, mod_manifest("re1")).unwrap();
    writer.add("x.txt", b"layer".to_vec()).unwrap();
    writer.write(&layer).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_arklay"))
        .args(["list"])
        .arg(&base)
        .arg("--mod")
        .arg(&layer)
        .output()
        .expect("failed to run arklay list");
    assert!(output.status.success());

    // The stem-derived base id warns; the warning must not corrupt the TOC.
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains("warning"),
        "warnings leaked into the TOC: {stdout}"
    );
    assert!(
        stdout.lines().last().unwrap().contains("entries, "),
        "{stdout}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("warning:"), "{stderr}");
    assert!(stderr.contains("manifest.toml"), "{stderr}");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn pack_info_cli_prints_the_real_pack() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_arklay"))
        .args(["pack", "info"])
        .arg(&pack_path)
        .output()
        .expect("failed to run arklay pack info");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with(&format!("pack: {}", pack_path.display())),
        "{text}"
    );
    let pack = Pack::open(&pack_path).unwrap();
    match pack.manifest() {
        Some(manifest) => assert!(
            text.contains(&format!("manifest: {} ({})", manifest.id, manifest.kind)),
            "{text}"
        ),
        None => assert!(text.contains("manifest: none"), "{text}"),
    }
    assert!(text.contains("layers: 0"), "{text}");
    assert!(
        text.contains(&format!("{} entries, ", pack.len())),
        "{text}"
    );
}

#[test]
#[ignore = "slow: builds the workspace without the default features"]
fn no_default_features_builds_with_noop_hooks() {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/no-default-check");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["check", "--no-default-features", "--lib"])
        .env("CARGO_TARGET_DIR", target)
        .status()
        .expect("failed to run cargo check");
    assert!(status.success(), "the lean build must compile");
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK; writes ~470 MiB"]
fn conversion_adds_exactly_the_manifest_entry() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    let previous = Pack::open(&pack_path).unwrap();

    let dir = TempDir::new("convert");
    let out = dir.path.join("re1.akpak");
    convert::convert_game(&root, &out).unwrap();
    let converted = Pack::open(&out).unwrap();

    assert!(converted.contains(manifest::ENTRY));
    let manifest = manifest::Manifest::parse(
        std::str::from_utf8(converted.read(manifest::ENTRY).unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest.id, convert::BASE_PACK_ID);
    assert_eq!(manifest.kind, manifest::PackKind::Base);
    assert_eq!(manifest.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));

    let previous_paths: Vec<&str> = previous.paths().collect();
    let mut converted_paths: Vec<&str> = converted.paths().collect();
    converted_paths.retain(|path| !path.eq_ignore_ascii_case(manifest::ENTRY));
    assert_eq!(converted_paths, previous_paths);
    assert_eq!(converted.len(), previous.len() + 1);
    for path in &previous_paths {
        assert_eq!(
            converted.read(path).unwrap(),
            previous.read(path).unwrap(),
            "{path}"
        );
    }
}
