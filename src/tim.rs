//! PSX TIM image decoder.

use anyhow::{Context, Result, bail};

use crate::state::Image;

const TIM_MAGIC: u32 = 0x10;
const HEADER_LEN: usize = 8;
const IMAGE_BLOCK_HEADER_LEN: usize = 12;
const PIXEL_BYTES: usize = 2;

/// Decode a PSX TIM image. Only 16bpp direct color (mode 2) is supported.
pub fn decode(data: &[u8]) -> Result<Image> {
    if data.len() < HEADER_LEN {
        bail!(
            "TIM is truncated: {} bytes, need at least {HEADER_LEN}",
            data.len()
        );
    }
    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if magic != TIM_MAGIC {
        bail!("not a TIM file: magic 0x{magic:08X}, expected 0x{TIM_MAGIC:08X}");
    }

    let flags = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let mode = flags & 7;
    if mode != 2 {
        let name = match mode {
            0 => "4bpp indexed",
            1 => "8bpp indexed",
            3 => "24bpp direct",
            _ => "unknown",
        };
        bail!("unsupported TIM color mode {mode}: {name}; only mode 2 (16bpp direct) is supported");
    }

    let block = data
        .get(HEADER_LEN..HEADER_LEN + IMAGE_BLOCK_HEADER_LEN)
        .context("TIM image block header is truncated")?;
    let _length = u32::from_le_bytes(block[0..4].try_into().unwrap());
    let _x = i16::from_le_bytes(block[4..6].try_into().unwrap());
    let _y = i16::from_le_bytes(block[6..8].try_into().unwrap());
    let width = u16::from_le_bytes(block[8..10].try_into().unwrap());
    let height = u16::from_le_bytes(block[10..12].try_into().unwrap());

    let pixel_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(PIXEL_BYTES))
        .context("TIM image dimensions overflow")?;
    let start = HEADER_LEN + IMAGE_BLOCK_HEADER_LEN;
    let end = start
        .checked_add(pixel_bytes)
        .context("TIM size overflow")?;
    let raw = data.get(start..end).with_context(|| {
        format!(
            "truncated TIM pixel data: need {pixel_bytes} bytes, have {}",
            data.len().saturating_sub(start)
        )
    })?;

    let mut rgba = Vec::with_capacity(pixel_bytes / PIXEL_BYTES * 4);
    for px in raw.chunks_exact(PIXEL_BYTES) {
        let v = u16::from_le_bytes([px[0], px[1]]);
        let r = (v >> 10) & 31;
        let g = (v >> 5) & 31;
        let b = v & 31;
        rgba.extend_from_slice(&[
            (r * 255 / 31) as u8,
            (g * 255 / 31) as u8,
            (b * 255 / 31) as u8,
            255,
        ]);
    }

    Ok(Image {
        width: width as u32,
        height: height as u32,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(r: u16, g: u16, b: u16) -> u16 {
        (r << 10) | (g << 5) | b
    }

    fn tim(mode: u32, width: u16, height: u16, pixels: &[u16], length: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&TIM_MAGIC.to_le_bytes());
        out.extend_from_slice(&mode.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        for p in pixels {
            out.extend_from_slice(&p.to_le_bytes());
        }
        out
    }

    #[test]
    fn decodes_16bpp_direct_color() {
        let pixels = [
            pixel(31, 31, 31),
            pixel(0, 0, 0),
            pixel(31, 0, 0),
            pixel(0, 31, 0),
            pixel(0, 0, 31),
            pixel(0, 31, 31),
            pixel(16, 16, 16),
            0x8000,
        ];
        let data = tim(2, 4, 2, &pixels, 12 + 16);

        let image = decode(&data).unwrap();

        assert_eq!(image.width, 4);
        assert_eq!(image.height, 2);
        let expected: Vec<u8> = [
            [255, 255, 255, 255],
            [0, 0, 0, 255],
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [0, 255, 255, 255],
            [131, 131, 131, 255],
            [0, 0, 0, 255],
        ]
        .concat();
        assert_eq!(image.rgba, expected);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut data = tim(2, 1, 1, &[pixel(31, 31, 31)], 12 + 2);
        data[0..4].copy_from_slice(&0x11u32.to_le_bytes());

        let err = decode(&data).unwrap_err().to_string();

        assert!(err.contains("magic"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_unsupported_modes() {
        for (mode, name) in [(0, "4bpp"), (1, "8bpp"), (3, "24bpp")] {
            let data = tim(mode, 1, 1, &[pixel(31, 31, 31)], 12 + 2);
            let err = decode(&data).unwrap_err().to_string();
            assert!(err.contains(name), "mode {mode}: unexpected error: {err}");
        }
    }

    #[test]
    fn rejects_truncated_pixel_data() {
        let mut data = tim(2, 4, 2, &[pixel(31, 31, 31)], 12 + 16);
        data.truncate(data.len() - 14);

        let err = decode(&data).unwrap_err().to_string();

        assert!(err.contains("truncated"), "unexpected error: {err}");
    }

    #[test]
    fn decodes_320x240_with_short_length_field() {
        let pixels = vec![pixel(31, 31, 31); 320 * 240];
        let data = tim(2, 320, 240, &pixels, 153600);

        let image = decode(&data).unwrap();

        assert_eq!(image.width, 320);
        assert_eq!(image.height, 240);
        assert_eq!(image.rgba.len(), 320 * 240 * 4);

        let mut spec = data.clone();
        spec[12..16].copy_from_slice(&153612u32.to_le_bytes());
        assert!(decode(&spec).is_ok());
    }
}
