//! BMP encode/decode.
//!
//! Encoding picks 8-bit indexed when the image has at most 256 unique RGB
//! colors and 24-bit BGR otherwise. Rows are stored bottom-up, padded to a
//! 4-byte boundary, and alpha is discarded.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;

use anyhow::{Result, anyhow, bail};

use crate::model::Texture8;
use crate::state::Image;

const FILE_HEADER_LEN: usize = 14;
const DIB_HEADER_LEN: usize = 40;

/// Encode an RGBA8 image as a BMP file at `path`.
pub fn encode(image: &Image, path: &Path) -> Result<()> {
    let data = encode_to_vec(image)?;
    std::fs::write(path, data)?;
    Ok(())
}

/// Decode a bottom-up 8-bit indexed or 24-bit BMP into an RGBA8 image.
///
/// Alpha is set to 255. Top-down images, compressed data, and other bit
/// depths are rejected.
pub fn decode(data: &[u8]) -> Result<Image> {
    if data.len() < 2 || &data[0..2] != b"BM" {
        bail!("not a BMP file: bad magic");
    }
    if data.len() < FILE_HEADER_LEN {
        bail!(
            "truncated BMP: file header is {} bytes (expected {FILE_HEADER_LEN})",
            data.len()
        );
    }
    let pixel_offset = u32_at(data, 10) as usize;

    if data.len() < FILE_HEADER_LEN + 4 {
        bail!("truncated BMP: missing DIB header");
    }
    let dib_size = u32_at(data, 14) as usize;
    if dib_size < DIB_HEADER_LEN {
        bail!("unsupported BMP DIB header size {dib_size} (expected at least {DIB_HEADER_LEN})");
    }
    if data.len() < FILE_HEADER_LEN + dib_size {
        bail!(
            "truncated BMP: DIB header needs {dib_size} bytes, have {}",
            data.len() - FILE_HEADER_LEN
        );
    }

    let width = i32_at(data, 18);
    let height = i32_at(data, 22);
    let planes = u16_at(data, 26);
    let bpp = u16_at(data, 28);
    let compression = u32_at(data, 30);
    let colors_used = u32_at(data, 46);

    if width <= 0 || height <= 0 {
        bail!("unsupported BMP dimensions {width}x{height} (must be positive, bottom-up)");
    }
    if planes != 1 {
        bail!("unsupported BMP planes {planes} (expected 1)");
    }
    if compression != 0 {
        bail!("unsupported BMP compression {compression} (expected 0)");
    }
    if bpp != 8 && bpp != 24 {
        bail!("unsupported BMP bit depth {bpp} (expected 8 or 24)");
    }

    let dib_end = FILE_HEADER_LEN + dib_size;
    let palette_len = if colors_used > 0 {
        colors_used as usize
    } else if pixel_offset > dib_end {
        (pixel_offset - dib_end) / 4
    } else {
        0
    };

    let mut palette = Vec::new();
    if bpp == 8 {
        for entry in data
            .get(dib_end..)
            .unwrap_or(&[])
            .as_chunks::<4>()
            .0
            .iter()
            .take(palette_len.min(256))
        {
            palette.push([entry[2], entry[1], entry[0]]);
        }
    }

    let width = width as usize;
    let height = height as usize;
    let row_bytes = width * bpp as usize / 8;
    let row_stride = (row_bytes + 3) & !3;
    let image_size = row_stride
        .checked_mul(height)
        .ok_or_else(|| anyhow!("BMP dimensions {width}x{height} are too large"))?;
    let end = pixel_offset
        .checked_add(image_size)
        .ok_or_else(|| anyhow!("BMP pixel data offset overflows"))?;
    if end > data.len() {
        bail!(
            "truncated BMP: pixel data ends at {end}, file has {} bytes",
            data.len()
        );
    }

    let pixels = &data[pixel_offset..end];
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        let src = &pixels[(height - 1 - y) * row_stride..];
        let dst = &mut rgba[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            let (r, g, b) = if bpp == 8 {
                let index = src[x] as usize;
                let color = palette.get(index).ok_or_else(|| {
                    anyhow!(
                        "BMP palette index {index} out of range (palette has {} entries)",
                        palette.len()
                    )
                })?;
                (color[0], color[1], color[2])
            } else {
                (src[x * 3 + 2], src[x * 3 + 1], src[x * 3])
            };
            dst[x * 4] = r;
            dst[x * 4 + 1] = g;
            dst[x * 4 + 2] = b;
            dst[x * 4 + 3] = 255;
        }
    }

    Ok(Image {
        width: width as u32,
        height: height as u32,
        rgba,
    })
}

/// Encode an image to BMP bytes.
pub fn encode_to_vec(image: &Image) -> Result<Vec<u8>> {
    let width = image.width as usize;
    let height = image.height as usize;
    let expected = width * height * 4;
    if image.rgba.len() != expected {
        bail!(
            "rgba buffer has {} bytes but a {}x{} image needs {expected}",
            image.rgba.len(),
            image.width,
            image.height
        );
    }

    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut indices: HashMap<[u8; 3], u8> = HashMap::new();
    let mut indexed = true;
    for pixel in image.rgba.as_chunks::<4>().0 {
        let rgb = [pixel[0], pixel[1], pixel[2]];
        if let Entry::Vacant(slot) = indices.entry(rgb) {
            if palette.len() == 256 {
                indexed = false;
                break;
            }
            slot.insert(palette.len() as u8);
            palette.push(rgb);
        }
    }

    let bpp: u16 = if indexed { 8 } else { 24 };
    let palette_len = if indexed { palette.len() } else { 0 };
    let row_bytes = width * bpp as usize / 8;
    let row_stride = (row_bytes + 3) & !3;
    let image_size = row_stride * height;
    let pixel_offset = FILE_HEADER_LEN + DIB_HEADER_LEN + palette_len * 4;
    let file_size = pixel_offset + image_size;

    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    out.extend_from_slice(&(DIB_HEADER_LEN as u32).to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&bpp.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(image_size as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&(palette_len as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    if indexed {
        for rgb in &palette {
            out.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 0]);
        }
    }

    for y in (0..height).rev() {
        let row = &image.rgba[y * width * 4..(y + 1) * width * 4];
        if indexed {
            for pixel in row.as_chunks::<4>().0 {
                out.push(indices[&[pixel[0], pixel[1], pixel[2]]]);
            }
        } else {
            for pixel in row.as_chunks::<4>().0 {
                out.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
        }
        out.resize(out.len() + (row_stride - row_bytes), 0);
    }

    Ok(out)
}

/// Encode an 8bpp indexed texture through its first CLUT row as a BMP.
///
/// Mask pages carry one 256-colour CLUT row, so row 0 is the whole palette.
pub fn encode_texture8_to_vec(texture: &Texture8) -> Result<Vec<u8>> {
    let width = texture.width as usize;
    let height = texture.height as usize;
    let expected = width * height;
    if texture.indices.len() != expected {
        bail!(
            "texture has {} indices but a {}x{} image needs {expected}",
            texture.indices.len(),
            texture.width,
            texture.height
        );
    }

    let mut rgba = Vec::with_capacity(expected * 4);
    for &index in &texture.indices {
        rgba.extend_from_slice(&texture.palette(0, index));
    }
    encode_to_vec(&Image {
        width: texture.width,
        height: texture.height,
        rgba,
    })
}

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn i32_at(data: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn build(width: u32, height: u32, mut pixel: impl FnMut(u32, u32) -> [u8; 4]) -> Image {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&pixel(x, y));
            }
        }
        Image {
            width,
            height,
            rgba,
        }
    }

    fn few_colors(width: u32, height: u32) -> Image {
        const PALETTE: [[u8; 3]; 4] = [[10, 20, 30], [200, 40, 60], [7, 180, 90], [250, 250, 250]];
        build(width, height, |x, y| {
            let c = PALETTE[((x + y * 3) % 4) as usize];
            [c[0], c[1], c[2], 255]
        })
    }

    fn many_colors(width: u32, height: u32) -> Image {
        build(width, height, |x, y| {
            let i = y * width + x;
            [i as u8, (i >> 8) as u8, 0, 255]
        })
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("arklay-bmp-{}-{name}.bmp", std::process::id()))
    }

    fn pixel_offset(data: &[u8]) -> usize {
        u32_at(data, 10) as usize
    }

    #[test]
    fn indexed_roundtrip_with_padded_rows() {
        let image = few_colors(5, 3);
        let path = temp_path("indexed-padded");
        encode(&image, &path).unwrap();
        let data = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(u16_at(&data, 28), 8);
        let decoded = decode(&data).unwrap();
        assert_eq!(decoded.width, image.width);
        assert_eq!(decoded.height, image.height);
        assert_eq!(decoded.rgba, image.rgba);
    }

    #[test]
    fn truecolor_roundtrip_with_over_256_colors() {
        let image = many_colors(17, 16);
        let data = encode_to_vec(&image).unwrap();
        assert_eq!(u16_at(&data, 28), 24);
        let decoded = decode(&data).unwrap();
        assert_eq!(decoded.rgba, image.rgba);
    }

    #[test]
    fn rows_are_stored_bottom_up() {
        let image = many_colors(16, 17);
        let data = encode_to_vec(&image).unwrap();
        assert_eq!(u16_at(&data, 28), 24);

        let stride = 16 * 3;
        let first_file_row = pixel_offset(&data);
        let last_file_row = first_file_row + 16 * stride;
        let mut expected_first = Vec::new();
        let mut expected_last = Vec::new();
        for x in 0..16usize {
            let top = &image.rgba[x * 4..x * 4 + 3];
            let bottom = &image.rgba[16 * 16 * 4 + x * 4..16 * 16 * 4 + x * 4 + 3];
            expected_first.extend_from_slice(&[bottom[2], bottom[1], bottom[0]]);
            expected_last.extend_from_slice(&[top[2], top[1], top[0]]);
        }
        assert_eq!(
            &data[first_file_row..first_file_row + stride],
            expected_first.as_slice()
        );
        assert_eq!(
            &data[last_file_row..last_file_row + stride],
            expected_last.as_slice()
        );
    }

    #[test]
    fn palette_uses_first_occurrence_order() {
        let image = build(4, 1, |x, _| match x {
            0 => [0, 0, 255, 255],
            1 => [255, 0, 0, 255],
            2 => [0, 255, 0, 255],
            _ => [255, 0, 0, 255],
        });
        let data = encode_to_vec(&image).unwrap();
        assert_eq!(u16_at(&data, 28), 8);
        assert_eq!(u32_at(&data, 46), 3);

        let palette = FILE_HEADER_LEN + DIB_HEADER_LEN;
        assert_eq!(&data[palette..palette + 4], &[255, 0, 0, 0]);
        assert_eq!(&data[palette + 4..palette + 8], &[0, 0, 255, 0]);
        assert_eq!(&data[palette + 8..palette + 12], &[0, 255, 0, 0]);
    }

    #[test]
    fn decodes_foreign_24bpp_fixture() {
        let data: [u8; 70] = [
            0x42, 0x4D, 0x46, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x36, 0x00, 0x00, 0x00,
            0x28, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00,
            0x00, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00, 0x00,
        ];

        let image = decode(&data).unwrap();

        let expected: Vec<u8> = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ]
        .concat();
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.rgba, expected);
    }

    #[test]
    fn decodes_foreign_8bpp_fixture_with_inferred_palette() {
        let data: [u8; 70] = [
            0x42, 0x4D, 0x46, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3E, 0x00, 0x00, 0x00,
            0x28, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        ];

        let image = decode(&data).unwrap();

        let expected: Vec<u8> = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 255, 0, 255],
            [255, 0, 0, 255],
        ]
        .concat();
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.rgba, expected);
    }

    #[test]
    fn encodes_an_indexed_texture_through_its_first_palette_row() {
        let mut palettes = vec![[0u8; 4]; 256];
        palettes[0] = [10, 20, 30, 255];
        palettes[1] = [200, 100, 50, 255];
        let texture = Texture8 {
            width: 3,
            height: 1,
            indices: vec![0, 1, 0],
            palettes,
        };

        let data = encode_texture8_to_vec(&texture).unwrap();
        let image = decode(&data).unwrap();

        assert_eq!((image.width, image.height), (3, 1));
        let expected: Vec<u8> =
            [[10, 20, 30, 255], [200, 100, 50, 255], [10, 20, 30, 255]].concat();
        assert_eq!(image.rgba, expected);
    }

    #[test]
    fn encode_texture_rejects_wrong_index_len() {
        let texture = Texture8 {
            width: 2,
            height: 2,
            indices: vec![0; 3],
            palettes: vec![[0u8; 4]; 256],
        };
        assert!(encode_texture8_to_vec(&texture).is_err());
    }

    #[test]
    fn encode_rejects_wrong_rgba_len() {
        let image = Image {
            width: 2,
            height: 2,
            rgba: vec![0; 15],
        };
        assert!(encode(&image, &temp_path("bad-len")).is_err());
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut data = encode_to_vec(&few_colors(2, 2)).unwrap();
        data[0] = b'X';
        let err = decode(&data).unwrap_err();
        assert!(err.to_string().contains("magic"), "{err}");
    }

    #[test]
    fn decode_rejects_unsupported_bpp() {
        let mut data = encode_to_vec(&few_colors(2, 2)).unwrap();
        data[28..30].copy_from_slice(&4u16.to_le_bytes());
        let err = decode(&data).unwrap_err();
        assert!(err.to_string().contains("bit depth"), "{err}");
    }

    #[test]
    fn decode_rejects_unsupported_compression() {
        let mut data = encode_to_vec(&few_colors(2, 2)).unwrap();
        data[30..34].copy_from_slice(&1u32.to_le_bytes());
        let err = decode(&data).unwrap_err();
        assert!(err.to_string().contains("compression"), "{err}");
    }

    #[test]
    fn decode_rejects_truncated_data() {
        let data = encode_to_vec(&few_colors(5, 3)).unwrap();
        assert!(decode(&[]).is_err());
        assert!(decode(&data[..10]).is_err());
        assert!(decode(&data[..30]).is_err());
        let err = decode(&data[..data.len() - 1]).unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
    }
}
