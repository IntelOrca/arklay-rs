//! The HD texture-pack filename hash.
//!
//! The TeamX HD loader for RE1 (`bio1hd.asi`, loaded by Classic REbirth)
//! names every replacement `hires/<class>/<hash>.webp`, where `<hash>` is the
//! CRC-32 (IEEE, the zlib polynomial and initial/final value) of the texture
//! as the game is about to upload it, written as 8 uppercase hex digits.
//! Hashing the uploaded texels rather than the file means a replacement keeps
//! working whatever path or format the original ships in.
//!
//! The loader hooks the game after it has converted the texture to 32-bit
//! BGRA, so reproducing a name means reproducing that conversion:
//!
//! - A 15-bit texel widens each channel to 8 bits. Item, enemy and room-object
//!   textures and the `DATA/*.PIX` screens are shifted (`v << 3`); room
//!   backgrounds (`STAGE*/RC*.pak`) and room masks (`objspr/OSP*.pak`) are
//!   bit-replicated (`v << 3 | v >> 2`).
//! - Pure black becomes `0x000001` (blue 1), so it stays distinct from the
//!   transparent `0x000000`. The alpha byte is still 0 at this point.
//! - A mask's colour 0 is transparent (`0x00000000`); every other mask texel
//!   is opaque.
//!
//! Each class then hashes its own window ([`Class`]): `bgd` takes the left
//! 256 x 240 texels with every alpha forced opaque; `mask` takes the left
//! 256 x 256 texels; the other classes take the whole texture with every
//! texel but `0` and `1` made opaque. A texture smaller than the window is
//! padded with zero texels.
//!
//! ```
//! # use arklay::hash::{crc32, format_hash};
//! assert_eq!(format_hash(crc32(b"123456789")), "CBF43926");
//! ```

use anyhow::{Result, bail};

use crate::state::Image;

/// The replacement directory a texture is looked up in, which also selects
/// the loader's hash window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A full-screen background: room cameras and menu screens.
    Bgd,
    /// A room mask page.
    Mask,
    /// Menu, font and effect textures.
    Misc,
    /// Item and enemy model textures.
    Skin,
    /// Room-object model textures.
    Skin2,
}

impl Class {
    pub const ALL: [Class; 5] = [
        Class::Bgd,
        Class::Mask,
        Class::Misc,
        Class::Skin,
        Class::Skin2,
    ];

    /// The pack directory name, `hires/<name>/`.
    pub fn name(self) -> &'static str {
        match self {
            Class::Bgd => "bgd",
            Class::Mask => "mask",
            Class::Misc => "misc",
            Class::Skin => "skin",
            Class::Skin2 => "skin2",
        }
    }

    pub fn parse(name: &str) -> Result<Class> {
        Class::ALL
            .into_iter()
            .find(|class| class.name().eq_ignore_ascii_case(name))
            .ok_or_else(|| anyhow::anyhow!("unknown HD texture class {name:?}"))
    }
}

/// How the game widens a 5-bit channel to 8 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expand {
    /// `v << 3`: model textures, menu textures and `DATA/*.PIX` screens.
    Shift,
    /// `v << 3 | v >> 2`: room backgrounds and room masks.
    Replicate,
}

/// A texture as the game uploads it: 32-bit `0xAARRGGBB` texels with the
/// alpha byte still clear, black remapped to `0x000001` and transparent
/// texels `0`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Surface {
    pub width: usize,
    pub height: usize,
    pub texels: Vec<u32>,
}

impl Surface {
    /// Convert 15-bit PSX colours (`0bBBBBBGGGGGRRRRR`, the top bit ignored).
    pub fn from_bgr555(width: usize, height: usize, colours: &[u16], expand: Expand) -> Self {
        Surface {
            width,
            height,
            texels: colours.iter().map(|&c| texel(c, expand)).collect(),
        }
    }

    /// Convert an 8bpp page through its palette; with `key_zero` colour 0 is
    /// transparent, as for a room mask.
    pub fn from_indexed(
        width: usize,
        height: usize,
        indices: &[u8],
        palette: &[u16],
        expand: Expand,
        key_zero: bool,
    ) -> Self {
        let lookup: Vec<u32> = (0..256)
            .map(|i| match palette.get(i) {
                _ if key_zero && i == 0 => 0,
                Some(&c) => texel(c, expand),
                None => 1,
            })
            .collect();
        Surface {
            width,
            height,
            texels: indices.iter().map(|&i| lookup[usize::from(i)]).collect(),
        }
    }

    /// Convert decoded RGBA artwork, e.g. Classic REbirth's own `type.png`
    /// screens: alpha 0 is transparent, every other pixel keeps its colour.
    pub fn from_rgba(image: &Image) -> Self {
        let texels = image
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&[r, g, b, a]| {
                if a == 0 {
                    0
                } else {
                    let rgb = u32::from_be_bytes([0, r, g, b]);
                    if rgb == 0 { 1 } else { rgb }
                }
            })
            .collect();
        Surface {
            width: image.width as usize,
            height: image.height as usize,
            texels,
        }
    }

    /// The loader's hash of this surface for `class`.
    pub fn hash(&self, class: Class) -> u32 {
        match class {
            Class::Bgd => self.window_crc(256, 240, |t| t | 0xff00_0000),
            Class::Mask => self.window_crc(256, 256, |t| if t == 0 { 0 } else { t | 0xff00_0000 }),
            Class::Misc | Class::Skin | Class::Skin2 => {
                self.window_crc(self.width, self.height, |t| {
                    if t <= 1 { t } else { t | 0xff00_0000 }
                })
            }
        }
    }

    /// CRC-32 over `width` x `height` BGRA texels from the top-left corner,
    /// zero texels outside the surface, each passed through `finish`.
    pub fn window_crc(&self, width: usize, height: usize, finish: impl Fn(u32) -> u32) -> u32 {
        let mut bytes = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let texel = if x < self.width && y < self.height {
                    self.texels[y * self.width + x]
                } else {
                    0
                };
                bytes.extend_from_slice(&finish(texel).to_le_bytes());
            }
        }
        crc32(&bytes)
    }
}

/// One 15-bit colour as the game's 32-bit upload, alpha still clear.
pub fn texel(colour: u16, expand: Expand) -> u32 {
    let widen = |v: u16| -> u32 {
        let v = u32::from(v & 31);
        match expand {
            Expand::Shift => v << 3,
            Expand::Replicate => (v << 3) | (v >> 2),
        }
    };
    let rgb = (widen(colour) << 16) | (widen(colour >> 5) << 8) | widen(colour >> 10);
    if rgb == 0 { 1 } else { rgb }
}

/// One texture found in a file, with where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Byte offset of the TIM inside the scanned data.
    pub offset: usize,
    /// The CLUT row an indexed TIM was converted with.
    pub clut: Option<usize>,
    pub surface: Surface,
}

/// Every TIM embedded in `data`, converted with each of its CLUT rows.
///
/// Models (`.EMD`, `.EMW`, `.IVM`), rooms (`.RDT`) and plain `.TIM` files all
/// carry their textures as complete TIMs, so a header scan finds them without
/// parsing each container. A candidate must have a consistent block layout
/// to count, which rejects the stray `10 00 00 00` words in other data.
pub fn scan_tims(data: &[u8], expand: Expand, key_zero: bool) -> Vec<Found> {
    let mut found = Vec::new();
    let mut offset = 0;
    while offset + 8 <= data.len() {
        if let Some(tims) = parse_tim(data, offset, expand, key_zero) {
            found.extend(tims);
        }
        offset += 4;
    }
    found
}

fn parse_tim(data: &[u8], offset: usize, expand: Expand, key_zero: bool) -> Option<Vec<Found>> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?));
    if u32_at(offset)? != 0x10 {
        return None;
    }
    let flags = u32_at(offset + 4)?;
    let mode = flags & 7;
    if flags & !0xf != 0 || mode > 2 || (mode < 2) != (flags & 8 != 0) {
        return None;
    }
    let colours = |at: usize, count: usize| -> Option<Vec<u16>> {
        (0..count).map(|i| u16_at(at + i * 2)).collect()
    };

    let mut at = offset + 8;
    let mut palettes = Vec::new();
    if flags & 8 != 0 {
        let length = u32_at(at)? as usize;
        let (columns, rows) = (usize::from(u16_at(at + 8)?), usize::from(u16_at(at + 10)?));
        if columns == 0 || rows == 0 || length != 12 + columns * rows * 2 {
            return None;
        }
        for row in 0..rows {
            palettes.push(colours(at + 12 + row * columns * 2, columns)?);
        }
        at += length;
    }

    let length = u32_at(at)? as usize;
    let (units, height) = (usize::from(u16_at(at + 8)?), usize::from(u16_at(at + 10)?));
    let bytes = units * height * 2;
    // Room backgrounds store the image block length without its 12-byte
    // header; every other TIM includes it.
    if units == 0
        || height == 0
        || units > 1024
        || height > 512
        || (length != bytes + 12 && length != bytes)
    {
        return None;
    }
    let pixels = data.get(at + 12..at + 12 + bytes)?;

    let surfaces = match mode {
        2 => {
            let colours: Vec<u16> = pixels
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_le_bytes(pair))
                .collect();
            vec![(None, Surface::from_bgr555(units, height, &colours, expand))]
        }
        _ => {
            let indices: Vec<u8> = if mode == 1 {
                pixels.to_vec()
            } else {
                pixels.iter().flat_map(|&b| [b & 15, b >> 4]).collect()
            };
            let width = units * if mode == 1 { 2 } else { 4 };
            palettes
                .iter()
                .enumerate()
                .map(|(row, palette)| {
                    let surface =
                        Surface::from_indexed(width, height, &indices, palette, expand, key_zero);
                    (Some(row), surface)
                })
                .collect()
        }
    };
    Some(
        surfaces
            .into_iter()
            .map(|(clut, surface)| Found {
                offset,
                clut,
                surface,
            })
            .collect(),
    )
}

/// A headerless `.PIX` screen: 320x240 15-bit texels.
pub fn pix_surface(data: &[u8], expand: Expand) -> Result<Surface> {
    const WIDTH: usize = 320;
    if data.is_empty() || !data.len().is_multiple_of(WIDTH * 2) {
        bail!(
            "PIX is {} bytes, not a whole number of 320-texel rows",
            data.len()
        );
    }
    let colours: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect();
    Ok(Surface::from_bgr555(
        WIDTH,
        colours.len() / WIDTH,
        &colours,
        expand,
    ))
}

/// The CRC-32 (IEEE 802.3) lookup tables for slicing by eight: row 0 is the
/// classic byte table, row `k` advances a byte through `k` more zero bytes.
const CRC_TABLES: [[u32; 256]; 8] = {
    let mut tables = [[0u32; 256]; 8];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][index] = crc;
        index += 1;
    }
    let mut row = 1;
    while row < 8 {
        let mut index = 0;
        while index < 256 {
            let previous = tables[row - 1][index];
            tables[row][index] = (previous >> 8) ^ tables[0][(previous & 0xff) as usize];
            index += 1;
        }
        row += 1;
    }
    tables
};

/// The CRC-32 (IEEE 802.3), matching zlib. Textures are hashed as they load,
/// so this slices by eight bytes rather than shifting a bit at a time.
pub fn crc32(bytes: &[u8]) -> u32 {
    !crc32_update(0xffff_ffff, bytes)
}

/// Feed `bytes` into a running (pre-inverted) CRC-32 register.
fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    let t = &CRC_TABLES;
    let (words, rest) = bytes.as_chunks::<8>();
    for word in words {
        let low = crc ^ u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        crc = t[7][(low & 0xff) as usize]
            ^ t[6][((low >> 8) & 0xff) as usize]
            ^ t[5][((low >> 16) & 0xff) as usize]
            ^ t[4][(low >> 24) as usize]
            ^ t[3][usize::from(word[4])]
            ^ t[2][usize::from(word[5])]
            ^ t[1][usize::from(word[6])]
            ^ t[0][usize::from(word[7])];
    }
    for &byte in rest {
        crc = (crc >> 8) ^ t[0][((crc ^ u32::from(byte)) & 0xff) as usize];
    }
    crc
}

/// The 8-uppercase-hex-digit name of an HD replacement.
pub fn format_hash(hash: u32) -> String {
    format!("{hash:08X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgr(r: u16, g: u16, b: u16) -> u16 {
        r | (g << 5) | (b << 10)
    }

    #[test]
    fn crc32_matches_the_standard_vectors() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b"abc"), 0x3524_41c2);
    }

    #[test]
    fn texels_widen_and_remap_black() {
        assert_eq!(texel(bgr(31, 1, 0), Expand::Shift), 0x00f8_0800);
        assert_eq!(texel(bgr(31, 1, 0), Expand::Replicate), 0x00ff_0800);
        assert_eq!(texel(0, Expand::Shift), 1);
        assert_eq!(texel(0x8000, Expand::Replicate), 1);
    }

    #[test]
    fn the_model_window_is_the_whole_texture_with_black_left_clear() {
        let surface = Surface::from_bgr555(2, 1, &[bgr(1, 2, 3), 0], Expand::Shift);
        let expected = crc32(&[24, 16, 8, 0xff, 1, 0, 0, 0]);
        for class in [Class::Misc, Class::Skin, Class::Skin2] {
            assert_eq!(surface.hash(class), expected);
        }
    }

    #[test]
    fn the_background_window_is_256_by_240_and_opaque() {
        let colours: Vec<u16> = (0..320)
            .map(|x| if x == 0 { 0 } else { bgr(1, 0, 0) })
            .collect();
        let surface = Surface::from_bgr555(320, 1, &colours, Expand::Replicate);
        let mut expected = vec![1, 0, 0, 0xff];
        for _ in 1..256 {
            expected.extend_from_slice(&[0, 0, 8, 0xff]);
        }
        for _ in 0..239 * 256 {
            expected.extend_from_slice(&[0, 0, 0, 0xff]);
        }
        assert_eq!(surface.hash(Class::Bgd), crc32(&expected));
    }

    #[test]
    fn the_mask_window_keys_colour_zero() {
        let surface =
            Surface::from_indexed(2, 1, &[0, 1], &[bgr(1, 1, 1), 0], Expand::Replicate, true);
        let mut expected = vec![0, 0, 0, 0, 1, 0, 0, 0xff];
        expected.resize(256 * 256 * 4, 0);
        assert_eq!(surface.hash(Class::Mask), crc32(&expected));
    }

    #[test]
    fn rgba_artwork_maps_black_and_transparency_like_the_game() {
        let image = Image {
            width: 3,
            height: 1,
            rgba: vec![0, 0, 0, 255, 255, 0, 0, 0, 1, 2, 3, 255],
        };
        assert_eq!(Surface::from_rgba(&image).texels, vec![1, 0, 0x0001_0203]);
    }

    #[test]
    fn the_scanner_finds_an_embedded_indexed_tim_per_clut_row() {
        let mut data = vec![0xaa; 8];
        data.extend_from_slice(&0x10u32.to_le_bytes());
        data.extend_from_slice(&9u32.to_le_bytes());
        data.extend_from_slice(&(12u32 + 2 * 2 * 2).to_le_bytes());
        data.extend_from_slice(&[0, 0, 0, 0, 2, 0, 2, 0]);
        for colour in [0, bgr(31, 0, 0), bgr(0, 31, 0), bgr(0, 0, 31)] {
            data.extend_from_slice(&colour.to_le_bytes());
        }
        data.extend_from_slice(&(12u32 + 2).to_le_bytes());
        data.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
        data.extend_from_slice(&[0, 1]);

        let found = scan_tims(&data, Expand::Shift, false);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].offset, found[0].clut), (8, Some(0)));
        assert_eq!(found[0].surface.texels, vec![1, 0x00f8_0000]);
        assert_eq!(found[1].surface.texels, vec![0x0000_f800, 0x0000_00f8]);
    }

    #[test]
    fn format_hash_is_upper_case_and_padded() {
        assert_eq!(format_hash(0x0000_00ab), "000000AB");
        assert_eq!(format_hash(0xd1d1_44a8), "D1D144A8");
    }
}
