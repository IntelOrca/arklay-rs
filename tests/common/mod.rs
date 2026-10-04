//! Shared helpers for the ignored real-asset integration tests.

use std::path::PathBuf;

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
