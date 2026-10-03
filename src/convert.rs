//! `convert-game`: migrate a game installation into an `.akpak` pack.
//!
//! M0 converts only room 1000 of RE1: the RDT plus every camera background
//! (`RC100<cam>.pak` -> `/roomcut/100_<cam:03>.bmp`). Implemented next.

use std::path::Path;

use anyhow::Result;

pub fn convert_game(_root: &Path, _out: &Path) -> Result<()> {
    anyhow::bail!("convert-game is not implemented yet (M0)")
}
