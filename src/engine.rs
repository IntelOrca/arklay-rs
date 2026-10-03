//! Engine entry point: open a pack, load a room, display and cycle its cuts.
//!
//! M0: stub, implemented next.

use std::path::Path;

use anyhow::Result;

pub fn run(_pack: &Path, _room: u32, _player: u8, _capture: Option<&Path>) -> Result<()> {
    anyhow::bail!("engine::run is not implemented yet (M0)")
}
