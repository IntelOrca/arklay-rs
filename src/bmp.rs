//! BMP encode/decode.
//!
//! M0: stub, implemented next. Encoding picks 8-bit indexed when the image has
//! at most 256 unique colors, otherwise 24-bit BGR. Rows are bottom-up and
//! padded to 4 bytes.

use std::path::Path;

use anyhow::Result;

use crate::state::Image;

/// Encode an RGBA8 image to a BMP file.
pub fn encode(_image: &Image, _path: &Path) -> Result<()> {
    anyhow::bail!("bmp::encode is not implemented yet (M0)")
}

/// Decode a BMP (8-bit indexed or 24/32-bit) to RGBA8.
pub fn decode(_data: &[u8]) -> Result<Image> {
    anyhow::bail!("bmp::decode is not implemented yet (M0)")
}
