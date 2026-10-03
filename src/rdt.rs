//! RDT parser (M0: stub, implemented next).

use anyhow::Result;

use crate::state::{RoomId, RoomState};

/// Parse an RDT file into internal room state.
pub fn parse(_data: &[u8], _id: RoomId) -> Result<RoomState> {
    anyhow::bail!("rdt::parse is not implemented yet (M0)")
}
