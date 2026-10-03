//! RE1 `.pak` LZW decoder (M0: stub, implemented next).

use anyhow::Result;

/// Decode a RE1 `.pak` LZW stream.
pub fn decode(_input: &[u8]) -> Result<Vec<u8>> {
    anyhow::bail!("lzw::decode is not implemented yet (M0)")
}
