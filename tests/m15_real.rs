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
use arklay::engine::simulate_room;
use arklay::manifest;
use arklay::pack::{Pack, PackWriter};
use arklay::player;
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
