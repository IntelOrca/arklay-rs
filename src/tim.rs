//! PSX TIM image decoder (M0: stub, implemented next).

use anyhow::Result;

use crate::state::Image;

/// Decode a PSX TIM image (16bpp direct color first; 4/8bpp CLUT later).
pub fn decode(_data: &[u8]) -> Result<Image> {
    anyhow::bail!("tim::decode is not implemented yet (M0)")
}
