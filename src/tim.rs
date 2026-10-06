//! PSX TIM image decoder.

use anyhow::{Context, Result, bail};

use crate::model::{PALETTE_ROW_LEN, Texture8};
use crate::state::Image;

const TIM_MAGIC: u32 = 0x10;
const HEADER_LEN: usize = 8;
const BLOCK_HEADER_LEN: usize = 12;
const IMAGE_BLOCK_HEADER_LEN: usize = 12;
const PIXEL_BYTES: usize = 2;
const CLUT_FLAG: u32 = 1 << 3;
const MODE_4BPP: u32 = 0;
const MODE_8BPP: u32 = 1;
/// Palette entries a 4bpp texel can index.
const CLUT_ROW_LEN: usize = 16;
/// The BGR555 word's semi-transparency (STP) bit.
const STP_BIT: u16 = 0x8000;

/// Decode a PSX TIM image. Only 16bpp direct color (mode 2) is supported.
///
/// The image block length field is ignored: the shipped paks store only the
/// pixel byte count there, so it cannot bound the pixel data.
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
    for px in raw.as_chunks::<PIXEL_BYTES>().0 {
        let v = u16::from_le_bytes([px[0], px[1]]);
        let r = v & 31;
        let g = (v >> 5) & 31;
        let b = (v >> 10) & 31;
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

/// Decode a TIM with an 8bpp indexed image and its CLUT.
///
/// The flags must select mode 1 (8bpp indexed) and set the CLUT bit. The CLUT
/// block's length field covers its 12-byte header and all BGR555 entries; the
/// image block's width field is in 16-bit units, so the pixel width is twice
/// the stored value. Palettes are flattened row-major with the 256 entries of
/// the player texture's CLUT row first, and each entry's bit 15 is kept in
/// [`Texture8::stp`] as the semi-transparency selector.
pub fn decode_8bpp(data: &[u8]) -> Result<Texture8> {
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
    if mode != MODE_8BPP {
        bail!("unsupported TIM color mode {mode}: expected mode {MODE_8BPP} (8bpp indexed)");
    }
    if flags & CLUT_FLAG == 0 {
        bail!("8bpp TIM has no CLUT block: flags 0x{flags:02X}");
    }

    let clut_block = block_header(data, HEADER_LEN, "CLUT")?;
    let clut_length = u32::from_le_bytes(clut_block[0..4].try_into().unwrap()) as usize;
    if clut_length < BLOCK_HEADER_LEN {
        bail!(
            "TIM CLUT block length {clut_length} is smaller than its {BLOCK_HEADER_LEN}-byte header"
        );
    }
    let clut_width = u16::from_le_bytes(clut_block[8..10].try_into().unwrap()) as usize;
    let clut_height = u16::from_le_bytes(clut_block[10..12].try_into().unwrap()) as usize;
    let clut_count = clut_width
        .checked_mul(clut_height)
        .context("TIM CLUT dimensions overflow")?;
    let clut_start = HEADER_LEN + BLOCK_HEADER_LEN;
    let clut_end = clut_start
        .checked_add(
            clut_count
                .checked_mul(PIXEL_BYTES)
                .context("TIM CLUT size overflows")?,
        )
        .context("TIM CLUT size overflows")?;
    if clut_end > HEADER_LEN + clut_length {
        bail!(
            "TIM CLUT block length {clut_length} does not cover its {clut_width}x{clut_height} palette"
        );
    }
    let clut_bytes = data.get(clut_start..clut_end).with_context(|| {
        format!(
            "truncated TIM CLUT data: need {} bytes, have {}",
            clut_count * PIXEL_BYTES,
            data.len().saturating_sub(clut_start)
        )
    })?;
    let clut_words: Vec<u16> = clut_bytes
        .as_chunks::<PIXEL_BYTES>()
        .0
        .iter()
        .map(|entry| u16::from_le_bytes([entry[0], entry[1]]))
        .collect();
    let palettes = clut_words.iter().map(|&word| expand_bgr555(word)).collect();
    let stp = clut_words.iter().map(|&word| word & STP_BIT != 0).collect();

    let image_start = HEADER_LEN
        .checked_add(clut_length)
        .context("TIM block offset overflows")?;
    let image_block = block_header(data, image_start, "image")?;
    let _image_length = u32::from_le_bytes(image_block[0..4].try_into().unwrap());
    let width_units = u16::from_le_bytes(image_block[8..10].try_into().unwrap()) as usize;
    let height = u16::from_le_bytes(image_block[10..12].try_into().unwrap()) as usize;
    let pixel_count = width_units
        .checked_mul(2)
        .and_then(|width| width.checked_mul(height))
        .context("TIM image dimensions overflow")?;
    let pixel_start = image_start + BLOCK_HEADER_LEN;
    let indices = data
        .get(pixel_start..pixel_start + pixel_count)
        .with_context(|| {
            format!(
                "truncated TIM 8bpp pixel data: need {pixel_count} bytes, have {}",
                data.len().saturating_sub(pixel_start)
            )
        })?
        .to_vec();

    Ok(Texture8 {
        width: (width_units * 2) as u32,
        height: height as u32,
        indices,
        palettes,
        stp,
    })
}

/// Decode a TIM with a 4bpp indexed image and its CLUT.
///
/// The image block's width field is in 16-bit units; each unit holds four
/// pixels, low nibble first, so the pixel width is four times the stored value.
/// Each CLUT row is normalized to its first 16 entries at the shared
/// [`PALETTE_ROW_LEN`]-entry stride, so [`Texture8::palette`] looks up a row by
/// its 4bpp texel index. The shipped 768x256 Japanese sheet stores 272 entries
/// per CLUT row; only the leading 16 are reachable, exactly as on the console.
/// Each entry's bit 15 becomes its [`Texture8::stp`] flag.
pub fn decode_4bpp(data: &[u8]) -> Result<Texture8> {
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
    if mode != MODE_4BPP {
        bail!("unsupported TIM color mode {mode}: expected mode {MODE_4BPP} (4bpp indexed)");
    }
    if flags & CLUT_FLAG == 0 {
        bail!("4bpp TIM has no CLUT block: flags 0x{flags:02X}");
    }

    let clut_block = block_header(data, HEADER_LEN, "CLUT")?;
    let clut_length = u32::from_le_bytes(clut_block[0..4].try_into().unwrap()) as usize;
    if clut_length < BLOCK_HEADER_LEN {
        bail!(
            "TIM CLUT block length {clut_length} is smaller than its {BLOCK_HEADER_LEN}-byte header"
        );
    }
    let clut_width = u16::from_le_bytes(clut_block[8..10].try_into().unwrap()) as usize;
    let clut_height = u16::from_le_bytes(clut_block[10..12].try_into().unwrap()) as usize;
    let clut_count = clut_width
        .checked_mul(clut_height)
        .context("TIM CLUT dimensions overflow")?;
    let clut_start = HEADER_LEN + BLOCK_HEADER_LEN;
    let clut_end = clut_start
        .checked_add(
            clut_count
                .checked_mul(PIXEL_BYTES)
                .context("TIM CLUT size overflows")?,
        )
        .context("TIM CLUT size overflows")?;
    if clut_end > HEADER_LEN + clut_length {
        bail!(
            "TIM CLUT block length {clut_length} does not cover its {clut_width}x{clut_height} palette"
        );
    }
    let clut_bytes = data.get(clut_start..clut_end).with_context(|| {
        format!(
            "truncated TIM CLUT data: need {} bytes, have {}",
            clut_count * PIXEL_BYTES,
            data.len().saturating_sub(clut_start)
        )
    })?;
    let mut palettes = vec![[0u8; 4]; clut_height * PALETTE_ROW_LEN];
    let mut stp = vec![false; clut_height * PALETTE_ROW_LEN];
    for row in 0..clut_height {
        for index in 0..clut_width.min(CLUT_ROW_LEN) {
            let offset = (row * clut_width + index) * PIXEL_BYTES;
            let entry = u16::from_le_bytes([clut_bytes[offset], clut_bytes[offset + 1]]);
            palettes[row * PALETTE_ROW_LEN + index] = expand_bgr555(entry);
            stp[row * PALETTE_ROW_LEN + index] = entry & STP_BIT != 0;
        }
    }

    let image_start = HEADER_LEN
        .checked_add(clut_length)
        .context("TIM block offset overflows")?;
    let image_block = block_header(data, image_start, "image")?;
    let _image_length = u32::from_le_bytes(image_block[0..4].try_into().unwrap());
    let width_units = u16::from_le_bytes(image_block[8..10].try_into().unwrap()) as usize;
    let height = u16::from_le_bytes(image_block[10..12].try_into().unwrap()) as usize;
    let byte_count = width_units
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_mul(height))
        .context("TIM image dimensions overflow")?;
    let pixel_start = image_start + BLOCK_HEADER_LEN;
    let raw = data
        .get(pixel_start..pixel_start + byte_count)
        .with_context(|| {
            format!(
                "truncated TIM 4bpp pixel data: need {byte_count} bytes, have {}",
                data.len().saturating_sub(pixel_start)
            )
        })?;
    let mut indices = Vec::with_capacity(byte_count * 2);
    for byte in raw {
        indices.push(byte & 0xF);
        indices.push(byte >> 4);
    }

    Ok(Texture8 {
        width: (width_units * 4) as u32,
        height: height as u32,
        indices,
        palettes,
        stp,
    })
}

fn block_header(data: &[u8], offset: usize, what: &str) -> Result<[u8; BLOCK_HEADER_LEN]> {
    data.get(offset..offset + BLOCK_HEADER_LEN)
        .with_context(|| format!("TIM {what} block header at offset 0x{offset:X} is truncated"))?
        .try_into()
        .context("TIM block header is truncated")
}

fn expand_bgr555(value: u16) -> [u8; 4] {
    let r = value & 31;
    let g = (value >> 5) & 31;
    let b = (value >> 10) & 31;
    [
        (r * 255 / 31) as u8,
        (g * 255 / 31) as u8,
        (b * 255 / 31) as u8,
        255,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(r: u16, g: u16, b: u16) -> u16 {
        r | (g << 5) | (b << 10)
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
    fn extracts_channels_in_psx_order() {
        let data = tim(2, 4, 1, &[0x001F, 0x03E0, 0x7C00, 0x7FFF], 12 + 8);

        let image = decode(&data).unwrap();

        let expected: Vec<u8> = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
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
    fn decodes_320x240_with_spec_length_field() {
        let pixels = vec![pixel(31, 31, 31); 320 * 240];
        let mut data = tim(2, 320, 240, &pixels, 153600);
        data[8..12].copy_from_slice(&153612u32.to_le_bytes());

        let image = decode(&data).unwrap();

        assert_eq!(image.width, 320);
        assert_eq!(image.height, 240);
        assert_eq!(image.rgba.len(), 320 * 240 * 4);
        assert_eq!(&image.rgba[image.rgba.len() - 4..], &[255, 255, 255, 255]);
    }

    fn tim_8bpp(
        palette_width: u16,
        palette_height: u16,
        palette: &[u16],
        image_width: u16,
        image_height: u16,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&TIM_MAGIC.to_le_bytes());
        out.extend_from_slice(&(CLUT_FLAG | MODE_8BPP).to_le_bytes());
        out.extend_from_slice(
            &((BLOCK_HEADER_LEN + palette.len() * PIXEL_BYTES) as u32).to_le_bytes(),
        );
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&480i16.to_le_bytes());
        out.extend_from_slice(&palette_width.to_le_bytes());
        out.extend_from_slice(&palette_height.to_le_bytes());
        for entry in palette {
            out.extend_from_slice(&entry.to_le_bytes());
        }
        out.extend_from_slice(&((BLOCK_HEADER_LEN + pixels.len()) as u32).to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&image_width.to_le_bytes());
        out.extend_from_slice(&image_height.to_le_bytes());
        out.extend_from_slice(pixels);
        out
    }

    #[test]
    fn decodes_8bpp_indexed_with_clut() {
        let palette = [pixel(31, 0, 0), pixel(0, 31, 0), 0x8000, pixel(0, 0, 31)];
        let pixels = [0u8, 1, 2, 3, 3, 2, 1, 0];
        let data = tim_8bpp(2, 2, &palette, 2, 2, &pixels);

        let texture = decode_8bpp(&data).unwrap();

        assert_eq!(texture.width, 4);
        assert_eq!(texture.height, 2);
        assert_eq!(texture.indices, pixels);
        assert_eq!(texture.palettes.len(), 4);
        assert_eq!(texture.palettes[0], [255, 0, 0, 255]);
        assert_eq!(texture.palettes[1], [0, 255, 0, 255]);
        assert_eq!(texture.palettes[2], [0, 0, 0, 255]);
        assert_eq!(texture.palettes[3], [0, 0, 255, 255]);
        assert_eq!(texture.palette(0, 3), [0, 0, 255, 255]);
    }

    #[test]
    fn flattens_palette_rows_in_file_order() {
        let mut palette = vec![pixel(0, 0, 0); 256];
        palette.push(pixel(31, 31, 31));
        palette.resize(512, pixel(0, 0, 0));
        let data = tim_8bpp(256, 2, &palette, 1, 1, &[0, 1]);

        let texture = decode_8bpp(&data).unwrap();

        assert_eq!(texture.palette(0, 0), [0, 0, 0, 255]);
        assert_eq!(texture.palette(1, 0), [255, 255, 255, 255]);
        assert_eq!(texture.palette(1, 1), [0, 0, 0, 255]);
        assert_eq!(texture.palette(2, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn keeps_the_clut_stp_bit_per_palette_entry() {
        // Bit 15 of each BGR555 CLUT word is the STP selector; the colour
        // channels are unchanged.
        let opaque = pixel(0, 0, 0);
        let stp_white = 0x8000 | pixel(31, 31, 31);
        let data = tim_8bpp(
            2,
            2,
            &[opaque, stp_white, stp_white, opaque],
            1,
            1,
            &[0, 1, 2, 3],
        );

        let texture = decode_8bpp(&data).unwrap();

        assert_eq!(texture.palettes[1], [255, 255, 255, 255]);
        assert_eq!(
            texture.stp,
            vec![false, true, true, false],
            "STP flags must follow the CLUT words"
        );
        assert!(texture.row_has_stp(0));
        assert!(texture.palette_stp(0, 1));
        assert!(!texture.palette_stp(0, 0));
    }

    #[test]
    fn normalizes_4bpp_stp_flags_to_the_row_stride() {
        let mut palette = vec![0u16; 32];
        palette[3] = 0x8000 | pixel(31, 0, 0);
        palette[16 + 4] = 0x8000 | pixel(0, 31, 0); // Second row, entry 4.
        let data = tim_4bpp(16, 2, &palette, 1, 1, &[0x30, 0x00]);

        let texture = decode_4bpp(&data).unwrap();

        assert_eq!(texture.palette(0, 3), [255, 0, 0, 255]);
        assert!(texture.palette_stp(0, 3));
        assert!(!texture.palette_stp(0, 0));
        assert!(texture.palette_stp(1, 4));
        assert!(!texture.palette_stp(1, 0));
    }

    #[test]
    fn rejects_8bpp_without_clut() {
        let mut data = tim_8bpp(1, 1, &[pixel(31, 31, 31)], 1, 1, &[0]);
        data[4..8].copy_from_slice(&MODE_8BPP.to_le_bytes());

        let err = decode_8bpp(&data).unwrap_err().to_string();

        assert!(err.contains("CLUT"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_non_8bpp_mode() {
        let mut data = tim_8bpp(1, 1, &[pixel(31, 31, 31)], 1, 1, &[0]);
        data[4..8].copy_from_slice(&(2 | CLUT_FLAG).to_le_bytes());

        let err = decode_8bpp(&data).unwrap_err().to_string();

        assert!(err.contains("mode 2"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_truncated_clut() {
        let mut data = tim_8bpp(
            2,
            2,
            &[pixel(31, 0, 0), pixel(0, 31, 0), pixel(0, 0, 31), 0x8000],
            1,
            1,
            &[0],
        );
        data.truncate(HEADER_LEN + BLOCK_HEADER_LEN + 4);

        let err = decode_8bpp(&data).unwrap_err().to_string();

        assert!(err.contains("truncated"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_truncated_8bpp_pixels() {
        let mut data = tim_8bpp(1, 1, &[pixel(31, 31, 31)], 2, 2, &[0, 1, 2, 3]);
        data.truncate(data.len() - 1);

        let err = decode_8bpp(&data).unwrap_err().to_string();

        assert!(err.contains("truncated"), "unexpected error: {err}");
    }

    /// A mode-0 TIM; `palette` is the flat CLUT and `pixels` the packed
    /// low-nibble-first image bytes.
    fn tim_4bpp(
        palette_width: u16,
        palette_height: u16,
        palette: &[u16],
        image_width_units: u16,
        image_height: u16,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&TIM_MAGIC.to_le_bytes());
        out.extend_from_slice(&(CLUT_FLAG | MODE_4BPP).to_le_bytes());
        out.extend_from_slice(
            &((BLOCK_HEADER_LEN + palette.len() * PIXEL_BYTES) as u32).to_le_bytes(),
        );
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&480i16.to_le_bytes());
        out.extend_from_slice(&palette_width.to_le_bytes());
        out.extend_from_slice(&palette_height.to_le_bytes());
        for entry in palette {
            out.extend_from_slice(&entry.to_le_bytes());
        }
        out.extend_from_slice(&((BLOCK_HEADER_LEN + pixels.len()) as u32).to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&image_width_units.to_le_bytes());
        out.extend_from_slice(&image_height.to_le_bytes());
        out.extend_from_slice(pixels);
        out
    }

    #[test]
    fn decodes_4bpp_indexed_with_normalized_palette() {
        let mut palette = vec![0u16; 16];
        palette[1] = pixel(31, 31, 31);
        palette[2] = pixel(31, 0, 0);
        palette.extend_from_slice(&[pixel(0, 31, 0); 16]); // A second CLUT row.
        let pixels = [0x10u8, 0x32, 0x23, 0x01, 0xF0, 0xAA, 0x55, 0x0F];
        let data = tim_4bpp(16, 2, &palette, 2, 2, &pixels);

        let texture = decode_4bpp(&data).unwrap();

        assert_eq!(texture.width, 8);
        assert_eq!(texture.height, 2);
        assert_eq!(
            texture.indices,
            [0u8, 1, 2, 3, 3, 2, 1, 0, 0, 15, 10, 10, 5, 5, 15, 0]
        );
        assert_eq!(texture.palettes.len(), 2 * PALETTE_ROW_LEN);
        assert_eq!(texture.palette(0, 0), [0, 0, 0, 255]);
        assert_eq!(texture.palette(0, 1), [255, 255, 255, 255]);
        assert_eq!(texture.palette(0, 2), [255, 0, 0, 255]);
        assert_eq!(texture.palette(1, 1), [0, 255, 0, 255]);
        assert_eq!(texture.palette(0, 15), [0, 0, 0, 255]);
    }

    #[test]
    fn normalizes_wide_4bpp_clut_rows_and_ignores_extra_entries() {
        // The shipped Japanese font stores 272 entries per CLUT row of which
        // only the leading 16 are reachable by a 4bpp texel.
        let mut palette = vec![0u16; 272 * 2];
        palette[0] = pixel(31, 0, 0);
        palette[16] = pixel(31, 31, 31); // Beyond the 4bpp window.
        palette[272] = pixel(0, 31, 0); // First entry of the next row.
        let data = tim_4bpp(272, 2, &palette, 1, 1, &[0x10, 0x00]);

        let texture = decode_4bpp(&data).unwrap();

        assert_eq!(texture.palettes.len(), 2 * PALETTE_ROW_LEN);
        assert_eq!(texture.palette(0, 0), [255, 0, 0, 255]);
        assert_eq!(texture.palette(0, 1), [0, 0, 0, 255]);
        assert_eq!(texture.palette(0, 15), [0, 0, 0, 255]);
        assert_eq!(texture.palette(1, 0), [0, 255, 0, 255]);
    }

    #[test]
    fn counts_4bpp_clut_rows() {
        let mut palette = vec![0u16; 16 * 3];
        palette[32] = pixel(0, 0, 31);
        let data = tim_4bpp(16, 3, &palette, 1, 1, &[0x00, 0x00]);

        let texture = decode_4bpp(&data).unwrap();

        assert_eq!(texture.palettes.len(), 3 * PALETTE_ROW_LEN);
        assert_eq!(texture.palette(1, 0), [0, 0, 0, 255]);
        assert_eq!(texture.palette(2, 0), [0, 0, 255, 255]);
        assert_eq!(texture.palette(3, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn rejects_non_4bpp_mode() {
        let mut data = tim_4bpp(16, 1, &[0u16; 16], 1, 1, &[0]);
        data[4..8].copy_from_slice(&(MODE_8BPP | CLUT_FLAG).to_le_bytes());

        let err = decode_4bpp(&data).unwrap_err().to_string();

        assert!(err.contains("mode 1"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_4bpp_without_clut() {
        let mut data = tim_4bpp(16, 1, &[0u16; 16], 1, 1, &[0]);
        data[4..8].copy_from_slice(&MODE_4BPP.to_le_bytes());

        let err = decode_4bpp(&data).unwrap_err().to_string();

        assert!(err.contains("CLUT"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_4bpp_clut_shorter_than_its_block() {
        let mut data = tim_4bpp(16, 2, &[0u16; 32], 1, 1, &[0]);
        let length = (BLOCK_HEADER_LEN + 16 * PIXEL_BYTES) as u32;
        data[8..12].copy_from_slice(&length.to_le_bytes());

        let err = decode_4bpp(&data).unwrap_err().to_string();

        assert!(err.contains("does not cover"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_truncated_4bpp_clut() {
        let mut data = tim_4bpp(16, 2, &[0u16; 32], 1, 1, &[0]);
        data.truncate(HEADER_LEN + BLOCK_HEADER_LEN + 4);

        let err = decode_4bpp(&data).unwrap_err().to_string();

        assert!(err.contains("truncated"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_truncated_4bpp_pixels() {
        let mut data = tim_4bpp(
            16,
            1,
            &[0u16; 16],
            2,
            2,
            &[0x10, 0x32, 0x23, 0x01, 0xF0, 0xAA, 0x55, 0x0F],
        );
        data.truncate(data.len() - 1);

        let err = decode_4bpp(&data).unwrap_err().to_string();

        assert!(err.contains("truncated"), "unexpected error: {err}");
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn decodes_the_real_font_sheet() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let path = std::path::PathBuf::from(root).join("JPN/DATA/FONT.TIM");
        let data = std::fs::read(&path).unwrap();

        let texture = decode_4bpp(&data).unwrap();

        assert_eq!((texture.width, texture.height), (768, 256));
        assert_eq!(texture.palettes.len(), 3 * PALETTE_ROW_LEN);
        assert_eq!(texture.palette(0, 0), [0, 0, 0, 255]);
        let ramp = texture.palette(0, 1);
        assert!(ramp[..3].iter().all(|&channel| channel > 200));
        assert_eq!(texture.palette(3, 0), [0, 0, 0, 0]);
        assert_eq!(texture.indices.len(), 768 * 256);
    }
}
