//! Shared helpers for the ignored real-asset integration tests.

use std::path::{Path, PathBuf};

use arklay::manifest;
use arklay::pack::{Pack, PackWriter};

/// Resolve the asset environment the real-asset tests share.
///
/// Both `ARKLAY_RE1_ROOT` and `ARKLAY_RE1_PACK` must be configured together:
/// with neither set the ignored test skips, but a partial configuration fails
/// loudly instead of silently passing.
pub fn asset_env() -> Option<(PathBuf, PathBuf)> {
    let root = std::env::var("ARKLAY_RE1_ROOT").ok();
    let pack = std::env::var("ARKLAY_RE1_PACK").ok();
    match (root, pack) {
        (None, None) => None,
        (Some(_), None) => panic!(
            "ARKLAY_RE1_ROOT is set but ARKLAY_RE1_PACK is not; set both to run the real-asset tests"
        ),
        (None, Some(_)) => panic!(
            "ARKLAY_RE1_PACK is set but ARKLAY_RE1_ROOT is not; set both to run the real-asset tests"
        ),
        (Some(root), Some(pack)) => Some((PathBuf::from(root), PathBuf::from(pack))),
    }
}

/// Open the shipped pack with the checked-in enemy scripts layered over it.
///
/// The shared pack may have been converted from an older tree, so a test that
/// must exercise the repository's Lua shadows the pack's `enemy/*.lua` copies
/// with the `ENEMY_SCRIPTS` the build embeds. Rooms, models and combat data
/// still come from the real pack.
// Not every integration-test binary that includes this module calls it.
#[allow(dead_code)]
pub fn enemy_scripts_pack(pack_path: &Path) -> Pack {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "arklay-enemy-scripts-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the script overlay dir");
    let mod_path = dir.join("enemy-scripts.akpak");
    let mut writer = PackWriter::new();
    let manifest = manifest::Manifest {
        kind: manifest::PackKind::Mod,
        base: Some("re1".to_string()),
        load_order: 10,
        ..manifest::Manifest::base("enemy-scripts")
    }
    .render()
    .into_bytes();
    writer
        .add(manifest::ENTRY, manifest)
        .expect("write manifest");
    for (path, source) in arklay::enemy::ENEMY_SCRIPTS {
        writer
            .add(path, source.as_bytes().to_vec())
            .expect("write script entry");
    }
    writer.write(&mod_path).expect("write overlay pack");
    Pack::open_layered(pack_path, std::slice::from_ref(&mod_path)).expect("open layered pack")
}
