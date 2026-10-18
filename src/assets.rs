//! General asset overrides: replacement formats and their decoding.
//!
//! A pack stores each class of art in the format the original shipped: room
//! backgrounds as BMP, textures and sprites as TIM, audio as WAV. A mod
//! author works in WebP/PNG and OGG Vorbis instead, so the loaders resolve an
//! asset by its *stem* through [`crate::pack::Pack::read_asset`] and this
//! module decodes whatever format won:
//!
//! | class | priority, highest first |
//! |---|---|
//! | image ([`decode_picture`], [`decode_page`], [`decode_texture`]) | `webp` > `png` > `bmp` > `tim` > `pix` |
//! | audio ([`decode_audio`]) | `ogg` > `wav` |
//!
//! The resolution rule itself lives in [`crate::pack`]: the winning layer is
//! decided by `(load_order, id)` first, so a higher layer's PNG beats a lower
//! layer's WebP, and only then does the extension priority pick within the
//! winning layer.
//!
//! # Decoders
//!
//! BMP, TIM and WAV stay the port's own hand-written decoders with their
//! budgets. PNG, WebP and OGG Vorbis come from the pure-Rust `png`,
//! `image-webp` and `lewton` crates behind the default-on `hd` feature, so a
//! `--no-default-features` build keeps its dependency list to `zstd` (and the
//! optional `mlua`). A build without `hd` still resolves replacements by stem,
//! but fails to decode a PNG/WebP/OGG entry with a message naming the feature.
//!
//! Every decoder is bound by the same [`crate::budget`] caps as the shipped
//! formats: an image is refused over [`budget::MAX_PIXELS`] and an audio
//! stream over [`budget::MAX_WAV_SAMPLES`] samples, before the allocation that
//! would hold it.
//!
//! # Geometry
//!
//! Art is drawn through one of two samplers, and [`Geometry`] states which
//! one:
//!
//! - A full-screen picture ([`Geometry::Free`]) is stretched over the frame,
//!   so a room background may be authored at any size, for example 4x for an
//!   HD pack.
//! - A page or sheet ([`Geometry::Shipped`]) is addressed by fixed UVs or
//!   sprite rectangles, so a replacement must keep the dimensions of the
//!   shipped entry it shadows; the decoder reads that entry's dimensions and
//!   refuses a mismatch naming both sizes. The same dimensions size a
//!   headerless `.pix`, whose format carries none.

#[cfg(feature = "hd")]
use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};

use crate::audio::{self, Wav};
use crate::bmp;
use crate::budget;
use crate::model::{PALETTE_ROW_LEN, Texture8};
use crate::pack::PackAsset;
use crate::state::Image;
use crate::tim;

/// A decoded image asset: the shipped indexed page, or a true-colour image.
///
/// The UI pages are stored in this form so a replacement author may ship an
/// indexed TIM, a true-colour PNG or a lossless WebP without any loader
/// caring which: both sprite paths exist, and an indexed page renders through
/// palette row 0 with texture entry zero transparent ([`Artwork::to_rgba`]).
#[derive(Debug, Clone)]
pub enum Artwork {
    /// An 8bpp/4bpp palette page.
    Indexed(Texture8),
    /// Direct RGBA pixels.
    Rgba(Image),
}

impl Artwork {
    /// The page width in pixels.
    pub fn width(&self) -> u32 {
        match self {
            Self::Indexed(texture) => texture.width,
            Self::Rgba(image) => image.width,
        }
    }

    /// The page height in pixels.
    pub fn height(&self) -> u32 {
        match self {
            Self::Indexed(texture) => texture.height,
            Self::Rgba(image) => image.height,
        }
    }

    /// The indexed page, when the artwork is one.
    pub fn as_indexed(&self) -> Option<&Texture8> {
        match self {
            Self::Indexed(texture) => Some(texture),
            Self::Rgba(_) => None,
        }
    }

    /// The RGBA image, when the artwork is one.
    pub fn as_rgba(&self) -> Option<&Image> {
        match self {
            Self::Indexed(_) => None,
            Self::Rgba(image) => Some(image),
        }
    }

    /// An RGBA view: an indexed page expands through palette row 0 with
    /// texture entry zero transparent, exactly like the effect pages.
    pub fn to_rgba(&self) -> Image {
        match self {
            Self::Indexed(texture) => texture_to_rgba(texture),
            Self::Rgba(image) => image.clone(),
        }
    }
}

/// How a replacement's geometry is constrained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Geometry {
    /// Any size: the caller stretches the image over its frame.
    Free,
    /// The shadowed copy of the same stem decides. Its dimensions are decoded
    /// and a replacement that differs is refused; a headerless `.pix` decodes
    /// at them.
    Shipped,
}

/// The stem of an entry path: its final component without its extension.
///
/// `assets::stem("bgm/013.wav")` is `"bgm/013"`, and a path whose final
/// component has no extension is returned unchanged. This is the key
/// [`crate::pack::Pack::read_asset`] resolves replacements by.
pub fn stem(path: &str) -> &str {
    let name_start = path.rfind('/').map_or(0, |slash| slash + 1);
    match path[name_start..].rfind('.') {
        Some(dot) if dot != 0 => &path[..name_start + dot],
        _ => path,
    }
}

/// Expand an indexed page to RGBA, with texture entry zero transparent.
pub fn texture_to_rgba(texture: &Texture8) -> Image {
    let mut rgba = Vec::with_capacity(texture.indices.len() * 4);
    for &index in &texture.indices {
        if index == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let texel = texture.palette(0, index);
            rgba.extend_from_slice(&[texel[0], texel[1], texel[2], 255]);
        }
    }
    Image {
        width: texture.width,
        height: texture.height,
        rgba,
    }
}

/// Decode a full-frame or fixed-geometry picture into RGBA.
///
/// `key` is the class's transparency key (exact black for the room masks and
/// the item atlas): a texel of that colour becomes transparent in every
/// format, and a PNG/WebP replacement's own alpha is honoured as well. `None`
/// keeps every texel opaque unless the format carries alpha.
pub fn decode_picture(
    asset: &PackAsset<'_>,
    geometry: Geometry,
    key: Option<[u8; 3]>,
) -> Result<Image> {
    let extension = asset.extension();
    let image = if extension == "pix" {
        decode_pix(asset, geometry)?
    } else {
        decode_image_bytes(&extension, asset.data())?
    };
    let geometry = resolve_geometry(asset, geometry, |shipped| {
        picture_dimensions(&shipped.extension(), shipped.data())
    })?;
    check_geometry(image.width, image.height, geometry, asset)?;
    Ok(apply_key(image, key))
}

/// Decode one image entry's bytes by extension, with no pack or geometry.
///
/// This is the pure decoder behind every picture and page: `bmp`/`png`/`webp`
/// decode to RGBA, `tim` decodes either indexed mode through palette row 0 or
/// a mode-2 direct image, and `pix` is refused because it is headerless. The
/// pack-level [`decode_picture`], [`decode_page`] and [`decode_texture`] wrap
/// it with the geometry a slot needs; `verify` and the fuzz targets call it
/// directly.
pub fn decode_image_bytes(extension: &str, data: &[u8]) -> Result<Image> {
    match extension {
        "bmp" => bmp::decode(data).context("invalid BMP replacement"),
        "png" => decode_png(data),
        "webp" => decode_webp(data),
        "tim" => match tim_mode(data)? {
            2 => tim::decode(data).context("invalid TIM replacement"),
            _ => Ok(texture_to_rgba(&decode_indexed_tim(data)?)),
        },
        "pix" => bail!("PIX is headerless and cannot be decoded without its dimensions"),
        other => bail!("unsupported image replacement format .{other}"),
    }
}

/// Decode one audio entry's bytes by extension, with no pack.
///
/// A WAV is parsed as shipped; an OGG Vorbis stream is decoded to 16-bit PCM
/// in the same [`Wav`] form, so `verify` validates both without a mixer.
pub fn decode_audio_bytes(extension: &str, data: &[u8]) -> Result<Wav> {
    match extension {
        "wav" => audio::parse_wav(data),
        "ogg" => decode_ogg(data),
        other => bail!("unsupported audio replacement format .{other}"),
    }
}

/// Decode an artwork page: an indexed TIM or an RGBA replacement.
///
/// A page is addressed by fixed UVs or sprite rectangles, so
/// [`Geometry::Shipped`] refuses a replacement whose dimensions differ from
/// the copy it shadows.
pub fn decode_page(asset: &PackAsset<'_>, geometry: Geometry) -> Result<Artwork> {
    let extension = asset.extension();
    let artwork = if extension == "pix" {
        Artwork::Rgba(decode_pix(asset, geometry)?)
    } else if extension == "tim" {
        match tim_mode(asset.data())? {
            2 => Artwork::Rgba(tim::decode(asset.data()).context("invalid TIM replacement")?),
            _ => Artwork::Indexed(decode_indexed_tim(asset.data())?),
        }
    } else {
        Artwork::Rgba(decode_image_bytes(&extension, asset.data())?)
    };
    let geometry = resolve_geometry(asset, geometry, |shipped| {
        page_dimensions(&shipped.extension(), shipped.data())
    })?;
    check_geometry(artwork.width(), artwork.height(), geometry, asset)?;
    Ok(artwork)
}

/// Decode an indexed texture, re-indexing a true-colour replacement exactly.
///
/// The font sheets are the remaining consumer: the glyph grid is addressed by
/// indices, so a PNG/WebP replacement is re-indexed colour for colour when it
/// uses at most 255 opaque colours plus transparency, and refused naming the
/// colour count otherwise. The palette repeats across as many CLUT rows as
/// the shipped texture carries, so a font's row selection keeps working.
pub fn decode_texture(asset: &PackAsset<'_>, geometry: Geometry) -> Result<Texture8> {
    let extension = asset.extension();
    let texture = if extension == "tim" {
        decode_indexed_tim(asset.data())?
    } else {
        reindex(&decode_image_bytes(&extension, asset.data())?, 1)?
    };
    let geometry = resolve_geometry(asset, geometry, |shipped| {
        page_dimensions(&shipped.extension(), shipped.data())
    })?;
    check_geometry(texture.width, texture.height, geometry, asset)?;
    Ok(texture)
}

/// Decode an audio replacement into the mixer's WAV form.
///
/// A WAV is parsed as shipped; an OGG Vorbis stream is decoded to 16-bit PCM
/// and handed back as the same [`Wav`] type with the stream's own rate and
/// channel count, so the mixer's mono conversion and resampling apply
/// unchanged.
pub fn decode_audio(asset: &PackAsset<'_>) -> Result<Wav> {
    decode_audio_bytes(&asset.extension(), asset.data())
}

/// The TIM colour mode of `data` (the flags' low three bits).
fn tim_mode(data: &[u8]) -> Result<u32> {
    let flags = data
        .get(4..8)
        .context("TIM is truncated: missing its flags word")?;
    Ok(u32::from_le_bytes([flags[0], flags[1], flags[2], flags[3]]) & 7)
}

/// Decode a 4bpp or 8bpp indexed TIM by its own mode.
fn decode_indexed_tim(data: &[u8]) -> Result<Texture8> {
    match tim_mode(data)? {
        0 => tim::decode_4bpp(data),
        1 => tim::decode_8bpp(data),
        mode => bail!("TIM colour mode {mode} is not indexed art"),
    }
}

/// Resolve the geometry a decoder must enforce: `Shipped` decodes the
/// shadowed copy's dimensions through `dimensions`, `Free` is a no-op.
fn resolve_geometry(
    asset: &PackAsset<'_>,
    geometry: Geometry,
    dimensions: impl Fn(&crate::pack::AssetCopy<'_>) -> Result<(u32, u32)>,
) -> Result<Option<(u32, u32)>> {
    if geometry != Geometry::Shipped {
        return Ok(None);
    }
    let Some(shipped) = asset.shadowed()? else {
        return Ok(None);
    };
    Ok(Some(dimensions(&shipped).with_context(|| {
        format!("failed to size the shipped entry {}", shipped.path())
    })?))
}

/// Refuse a decoded image whose dimensions the slot cannot sample.
fn check_geometry(
    width: u32,
    height: u32,
    geometry: Option<(u32, u32)>,
    asset: &PackAsset<'_>,
) -> Result<()> {
    if let Some((want_w, want_h)) = geometry
        && (width, height) != (want_w, want_h)
    {
        bail!(
            "{} is {width}x{height} but the entry it replaces is {want_w}x{want_h}; \
             a page replacement must keep its dimensions",
            asset.path()
        );
    }
    Ok(())
}

/// A picture's dimensions without decoding its pixels, for the geometry check.
fn picture_dimensions(extension: &str, data: &[u8]) -> Result<(u32, u32)> {
    match extension {
        "bmp" => bmp_dimensions(data),
        "png" => png_dimensions(data),
        "webp" => webp_dimensions(data),
        "tim" => tim_dimensions(data),
        "pix" => bail!("a .pix entry is headerless and cannot size itself"),
        other => bail!("unsupported image replacement format .{other}"),
    }
}

/// A page's dimensions without decoding its pixels, for the geometry check.
fn page_dimensions(extension: &str, data: &[u8]) -> Result<(u32, u32)> {
    match extension {
        "tim" => tim_dimensions(data),
        other => picture_dimensions(other, data),
    }
}

/// The dimensions a TIM's image block declares.
fn tim_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    match tim_mode(data)? {
        2 => {
            let block = data
                .get(8..20)
                .context("TIM image block header is truncated")?;
            let width = u16::from_le_bytes([block[8], block[9]]) as usize;
            let height = u16::from_le_bytes([block[10], block[11]]) as usize;
            Ok((width as u32, height as u32))
        }
        0 | 1 => {
            // The image block sits after the CLUT block, whose length field
            // covers its own 12-byte header.
            let clut_length = data
                .get(8..12)
                .context("TIM CLUT block header is truncated")?;
            let clut_length = u32::from_le_bytes([
                clut_length[0],
                clut_length[1],
                clut_length[2],
                clut_length[3],
            ]) as usize;
            let block = data
                .get(8 + clut_length..20 + clut_length)
                .context("TIM image block header is truncated")?;
            let units = u16::from_le_bytes([block[8], block[9]]) as usize;
            let height = u16::from_le_bytes([block[10], block[11]]) as usize;
            let width = if tim_mode(data)? == 0 {
                units * 4
            } else {
                units * 2
            };
            Ok((width as u32, height as u32))
        }
        mode => bail!("unsupported TIM colour mode {mode}"),
    }
}

/// The dimensions a BMP's DIB header declares.
fn bmp_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    let header = data.get(18..26).context("BMP is truncated")?;
    let width = i32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let height = i32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if width <= 0 || height <= 0 {
        bail!("unsupported BMP dimensions {width}x{height}");
    }
    Ok((width as u32, height as u32))
}

/// The dimensions a PNG's IHDR chunk declares.
fn png_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    let header = data.get(..24).context("PNG is truncated")?;
    if &header[..8] != b"\x89PNG\r\n\x1A\n" {
        bail!("not a PNG file: bad signature");
    }
    if &header[12..16] != b"IHDR" {
        bail!("PNG is missing its IHDR chunk");
    }
    Ok((
        u32::from_be_bytes([header[16], header[17], header[18], header[19]]),
        u32::from_be_bytes([header[20], header[21], header[22], header[23]]),
    ))
}

/// The dimensions a WebP's header declares.
#[cfg(feature = "hd")]
fn webp_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    let decoder = webp_decoder(data)?;
    Ok(decoder.dimensions())
}

/// Read a WebP's dimensions when the build has no `hd` feature.
#[cfg(not(feature = "hd"))]
fn webp_dimensions(_data: &[u8]) -> Result<(u32, u32)> {
    bail!("{}", DISABLED_FORMATS)
}

/// The dimensions of a resolved image asset, from its header where it has one.
///
/// This is the cheap per-class test for the font sheets' script selection; a
/// `.pix` replacement is headerless and needs the geometry of the entry it
/// shadows, so it is decoded in full.
pub fn image_dimensions(asset: &PackAsset<'_>) -> Result<(u32, u32)> {
    match asset.extension().as_str() {
        "tim" => tim_dimensions(asset.data()),
        "bmp" => bmp_dimensions(asset.data()),
        "png" => png_dimensions(asset.data()),
        "webp" => webp_dimensions(asset.data()),
        "pix" => {
            decode_picture(asset, Geometry::Shipped, None).map(|image| (image.width, image.height))
        }
        other => bail!("unsupported image replacement format .{other}"),
    }
}

/// Decode a `.pix` replacement at the shipped geometry.
///
/// PIX is the headerless 16bpp RGB555 form of the original install, so the
/// entry it shadows supplies its dimensions: a `.pix` that shadows nothing has
/// no size and is refused.
fn decode_pix(asset: &PackAsset<'_>, geometry: Geometry) -> Result<Image> {
    if geometry != Geometry::Shipped {
        bail!(
            "{} is a .pix replacement and needs the shipped entry's dimensions; \
             a full-frame picture must be authored as a self-describing format",
            asset.path()
        );
    }
    let Some(shipped) = asset.shadowed()? else {
        bail!(
            "{} is a headerless .pix replacement and shadows no entry to take \
             its dimensions from",
            asset.path()
        );
    };
    let (width, height) = picture_dimensions(&shipped.extension(), shipped.data())?;
    decode_pix16(asset.data(), width, height)
        .with_context(|| format!("invalid {} at {width}x{height}", asset.path()))
}

/// Decode headerless 16bpp RGB555 pixels at a known size.
fn decode_pix16(data: &[u8], width: u32, height: u32) -> Result<Image> {
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .context("PIX dimensions overflow")?;
    budget::check_len(pixel_count, budget::MAX_PIXELS, "PIX pixel count")?;
    let need = pixel_count
        .checked_mul(2)
        .context("PIX dimensions overflow")?;
    if data.len() != need {
        bail!(
            "PIX is {} bytes but {width}x{height} needs {need}",
            data.len()
        );
    }
    let mut rgba = budget::alloc::<u8>(pixel_count * 4, "PIX RGBA buffer")?;
    for pixel in data.as_chunks::<2>().0 {
        let word = u16::from_le_bytes([pixel[0], pixel[1]]);
        let r = word & 31;
        let g = (word >> 5) & 31;
        let b = (word >> 10) & 31;
        rgba.extend_from_slice(&[
            (r * 255 / 31) as u8,
            (g * 255 / 31) as u8,
            (b * 255 / 31) as u8,
            255,
        ]);
    }
    Ok(Image {
        width,
        height,
        rgba,
    })
}

/// Give texels equal to `key` a zero alpha.
fn apply_key(mut image: Image, key: Option<[u8; 3]>) -> Image {
    let Some(key) = key else {
        return image;
    };
    for pixel in image.rgba.as_chunks_mut::<4>().0 {
        if pixel[0] == key[0] && pixel[1] == key[1] && pixel[2] == key[2] {
            pixel[3] = 0;
        }
    }
    image
}

/// Re-index an RGBA image as a texture with the palette repeated `rows` times.
///
/// Transparent texels (alpha zero) become index 0; every other colour is
/// exactly one palette entry in first-use order. More than 255 opaque colours
/// is refused naming the count.
fn reindex(image: &Image, rows: usize) -> Result<Texture8> {
    let width = image.width as usize;
    let height = image.height as usize;
    let pixel_count = width
        .checked_mul(height)
        .context("image dimensions overflow")?;
    if image.rgba.len() != pixel_count * 4 {
        bail!(
            "image is {} bytes but {}x{} needs {}",
            image.rgba.len(),
            image.width,
            image.height,
            pixel_count * 4
        );
    }
    let mut palette: Vec<[u8; 4]> = vec![[0, 0, 0, 0]];
    let mut lookup: std::collections::HashMap<[u8; 3], u8> = std::collections::HashMap::new();
    let mut indices = budget::alloc::<u8>(pixel_count, "re-indexed texture")?;
    for pixel in image.rgba.as_chunks::<4>().0 {
        if pixel[3] == 0 {
            indices.push(0);
            continue;
        }
        let rgb = [pixel[0], pixel[1], pixel[2]];
        let index = match lookup.get(&rgb) {
            Some(&index) => index,
            None => {
                let index = u8::try_from(palette.len()).map_err(|_| {
                    anyhow!(
                        "replacement uses more than 255 opaque colours; an indexed asset \
                         accepts at most 255"
                    )
                })?;
                palette.push([pixel[0], pixel[1], pixel[2], 255]);
                lookup.insert(rgb, index);
                index
            }
        };
        indices.push(index);
    }
    let rows = rows.max(1);
    let mut palettes = Vec::with_capacity(rows * PALETTE_ROW_LEN);
    for _ in 0..rows {
        palettes.extend(palette.iter().copied());
        palettes.resize(
            palettes.len() + PALETTE_ROW_LEN - palette.len(),
            [0, 0, 0, 0],
        );
    }
    Ok(Texture8 {
        width: image.width,
        height: image.height,
        indices,
        palettes,
        stp: vec![false; rows * PALETTE_ROW_LEN],
    })
}

/// Decode a PNG into RGBA8, bounded by [`budget::MAX_PIXELS`].
#[cfg(feature = "hd")]
fn decode_png(data: &[u8]) -> Result<Image> {
    use png::{BitDepth, ColorType, Limits, Transformations};

    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(Transformations::normalize_to_color8());
    decoder.set_limits(Limits {
        bytes: budget::MAX_DECODE_ALLOC,
    });
    let mut reader = decoder
        .read_info()
        .map_err(|err| anyhow!("invalid PNG: {err}"))?;
    let (width, height) = reader.info().size();
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .context("PNG dimensions overflow")?;
    budget::check_len(pixel_count, budget::MAX_PIXELS, "PNG pixel count")?;
    let size = reader
        .output_buffer_size()
        .context("PNG output buffer overflows the address space")?;
    let mut buffer = budget::alloc::<u8>(size, "PNG pixel buffer")?;
    buffer.resize(size, 0);
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|err| anyhow!("invalid PNG: {err}"))?;
    let pixels = &buffer[..info.buffer_size()];
    let mut rgba = budget::alloc::<u8>(pixel_count * 4, "PNG RGBA buffer")?;
    let mut push = |r: u8, g: u8, b: u8, a: u8| rgba.extend_from_slice(&[r, g, b, a]);
    match (info.color_type, info.bit_depth) {
        (ColorType::Rgba, BitDepth::Eight) => {
            for pixel in pixels.as_chunks::<4>().0 {
                push(pixel[0], pixel[1], pixel[2], pixel[3]);
            }
        }
        (ColorType::Rgb, BitDepth::Eight) => {
            for pixel in pixels.as_chunks::<3>().0 {
                push(pixel[0], pixel[1], pixel[2], 255);
            }
        }
        (ColorType::Grayscale, BitDepth::Eight) => {
            for &value in pixels {
                push(value, value, value, 255);
            }
        }
        (ColorType::GrayscaleAlpha, BitDepth::Eight) => {
            for pixel in pixels.as_chunks::<2>().0 {
                push(pixel[0], pixel[0], pixel[0], pixel[1]);
            }
        }
        (color, depth) => bail!("unsupported PNG colour type {color:?} at {depth:?}"),
    }
    Ok(Image {
        width,
        height,
        rgba,
    })
}

/// Decode a PNG when the build has no `hd` feature: a named refusal.
#[cfg(not(feature = "hd"))]
fn decode_png(_data: &[u8]) -> Result<Image> {
    bail!("{}", DISABLED_FORMATS)
}

/// Decode a WebP into RGBA8, bounded by [`budget::MAX_PIXELS`].
#[cfg(feature = "hd")]
fn decode_webp(data: &[u8]) -> Result<Image> {
    let mut decoder = webp_decoder(data)?;
    let (width, height) = decoder.dimensions();
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .context("WebP dimensions overflow")?;
    budget::check_len(pixel_count, budget::MAX_PIXELS, "WebP pixel count")?;
    let size = decoder
        .output_buffer_size()
        .context("WebP output buffer overflows the address space")?;
    let mut buffer = budget::alloc::<u8>(size, "WebP pixel buffer")?;
    buffer.resize(size, 0);
    decoder
        .read_image(&mut buffer)
        .map_err(|err| anyhow!("invalid WebP: {err}"))?;
    let mut rgba = budget::alloc::<u8>(pixel_count * 4, "WebP RGBA buffer")?;
    if decoder.has_alpha() {
        rgba.extend_from_slice(&buffer);
    } else {
        for pixel in buffer.as_chunks::<3>().0 {
            rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
    }
    Ok(Image {
        width,
        height,
        rgba,
    })
}

/// Construct a WebP decoder bounded by [`budget::MAX_DECODE_ALLOC`].
#[cfg(feature = "hd")]
fn webp_decoder(data: &[u8]) -> Result<image_webp::WebPDecoder<Cursor<&[u8]>>> {
    let mut decoder = image_webp::WebPDecoder::new(Cursor::new(data))
        .map_err(|err| anyhow!("invalid WebP: {err}"))?;
    decoder.set_memory_limit(budget::MAX_DECODE_ALLOC);
    Ok(decoder)
}

/// Decode a WebP when the build has no `hd` feature: a named refusal.
#[cfg(not(feature = "hd"))]
fn decode_webp(_data: &[u8]) -> Result<Image> {
    bail!("{}", DISABLED_FORMATS)
}

/// Decode an OGG Vorbis stream to interleaved 16-bit PCM.
///
/// The stream's own sample rate and channel count are kept, exactly like a
/// WAV's, so the mixer's conversion and resampling apply unchanged. The
/// decoded sample count is capped by [`budget::MAX_WAV_SAMPLES`].
#[cfg(feature = "hd")]
fn decode_ogg(data: &[u8]) -> Result<Wav> {
    let mut reader = lewton::inside_ogg::OggStreamReader::new(Cursor::new(data))
        .map_err(|err| anyhow!("invalid OGG Vorbis: {err}"))?;
    let channels = reader.ident_hdr.audio_channels;
    let sample_rate = reader.ident_hdr.audio_sample_rate;
    if channels == 0 || sample_rate == 0 {
        bail!("OGG Vorbis stream has zero channels or sample rate");
    }
    let mut samples: Vec<i16> = Vec::new();
    while let Some(packet) = reader
        .read_dec_packet_itl()
        .map_err(|err| anyhow!("failed to decode OGG Vorbis: {err}"))?
    {
        if samples.len() + packet.len() > budget::MAX_WAV_SAMPLES {
            bail!(
                "OGG Vorbis decodes beyond the {} sample limit",
                budget::MAX_WAV_SAMPLES
            );
        }
        samples
            .try_reserve(packet.len())
            .map_err(|_| anyhow!("failed to reserve OGG Vorbis sample space"))?;
        samples.extend_from_slice(&packet);
    }
    Ok(Wav::from_s16(&samples, u16::from(channels), sample_rate))
}

/// Decode an OGG Vorbis stream when the build has no `hd` feature.
#[cfg(not(feature = "hd"))]
fn decode_ogg(_data: &[u8]) -> Result<Wav> {
    bail!("{}", DISABLED_FORMATS)
}

/// The refusal a build without the `hd` feature reports for its formats.
#[cfg(not(feature = "hd"))]
const DISABLED_FORMATS: &str =
    "PNG, WebP and OGG Vorbis replacements need the `hd` feature, which this build has disabled";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::extension_of;

    #[test]
    fn stems_strip_only_the_final_extension() {
        assert_eq!(stem("bgm/013.wav"), "bgm/013");
        assert_eq!(stem("roomcut/100_000.bmp"), "roomcut/100_000");
        assert_eq!(stem("se/40s&w.wav"), "se/40s&w");
        assert_eq!(stem("font/font.tim"), "font/font");
        assert_eq!(stem("no-extension"), "no-extension");
        assert_eq!(stem("dir.with.dots/file"), "dir.with.dots/file");
        assert_eq!(stem(".hidden"), ".hidden");
    }

    #[test]
    fn extensions_are_recognised_case_insensitively() {
        assert_eq!(extension_of("UI/Photo.WEBP"), "webp");
        assert_eq!(extension_of("dir.with.dots/file"), "");
        assert_eq!(extension_of(".hidden"), "");
    }

    #[test]
    fn reindexing_keeps_colours_exactly() {
        let image = Image {
            width: 3,
            height: 1,
            rgba: vec![
                255, 0, 0, 255, //
                0, 0, 0, 0, //
                255, 0, 0, 255,
            ],
        };
        let texture = reindex(&image, 2).unwrap();
        assert_eq!(texture.width, 3);
        assert_eq!(texture.height, 1);
        assert_eq!(texture.indices, [1, 0, 1]);
        assert_eq!(texture.palette(0, 1), [255, 0, 0, 255]);
        // The palette repeats across the requested CLUT rows.
        assert_eq!(texture.palette(1, 1), [255, 0, 0, 255]);
        assert_eq!(texture.palettes.len(), 2 * PALETTE_ROW_LEN);
        assert!(texture.stp.iter().all(|flag| !flag));
    }

    #[test]
    fn reindexing_refuses_more_colours_than_a_palette_holds() {
        let mut rgba = Vec::new();
        for index in 0..300u32 {
            rgba.extend_from_slice(&[(index & 0xFF) as u8, (index >> 8) as u8, 0, 255]);
        }
        let image = Image {
            width: 300,
            height: 1,
            rgba,
        };
        let err = reindex(&image, 1).unwrap_err().to_string();
        assert!(err.contains("255"), "{err}");
    }

    #[test]
    fn a_pix_replacement_needs_the_shipped_geometry() {
        // A lone `.pix` has no entry to take its dimensions from.
        let pack = pack_of(&[("roommask/100_000.pix", &[0x1F, 0x00, 0xE0, 0x03])]);
        let asset = pack
            .read_asset("roommask/100_000", crate::pack::AssetClass::Image)
            .unwrap()
            .unwrap();
        let err = decode_picture(&asset, Geometry::Shipped, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("headerless"), "{err}");
    }

    #[test]
    fn a_pix_replacement_decodes_at_the_shipped_size() {
        // A higher layer's headerless .pix takes its size from the base's BMP.
        let bmp = bmp_of(2, 1, &[[9, 9, 9, 255], [9, 9, 9, 255]]);
        let pack = layered_pack(
            &[("roommask/100_000.bmp", &bmp)],
            &[("roommask/100_000.pix", &[0x1F, 0x00, 0xE0, 0x03])],
        );
        let asset = pack
            .read_asset("roommask/100_000", crate::pack::AssetClass::Image)
            .unwrap()
            .unwrap();
        assert_eq!(asset.path(), "roommask/100_000.pix");
        let image = decode_picture(&asset, Geometry::Shipped, None).unwrap();
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(image.rgba, [255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn pix_decodes_rgb555_at_the_shipped_size() {
        // Two texels: red 0x001F and green 0x03E0.
        let data = [0x1F, 0x00, 0xE0, 0x03];
        let image = decode_pix16(&data, 2, 1).unwrap();
        assert_eq!(image.width, 2);
        assert_eq!(image.rgba, [255, 0, 0, 255, 0, 255, 0, 255]);
        let err = decode_pix16(&data, 3, 1).unwrap_err().to_string();
        assert!(err.contains("needs"), "{err}");
    }

    #[test]
    fn tim_dimensions_match_the_decoders() {
        let direct = direct_tim(4, 2);
        assert_eq!(tim_dimensions(&direct).unwrap(), (4, 2));
        assert_eq!(tim::decode(&direct).unwrap().width, 4);
        let indexed = indexed_tim_8bpp(6, 3);
        assert_eq!(tim_dimensions(&indexed).unwrap(), (6, 3));
        assert_eq!(tim::decode_8bpp(&indexed).unwrap().width, 6);
    }

    #[test]
    fn an_indexed_tim_page_is_not_re_indexed() {
        let pack = pack_of(&[("ui/died.tim", &indexed_tim_8bpp(4, 2))]);
        let asset = pack
            .read_asset("ui/died", crate::pack::AssetClass::Image)
            .unwrap()
            .unwrap();
        let artwork = decode_page(&asset, Geometry::Shipped).unwrap();
        assert_eq!(artwork.width(), 4);
        assert!(artwork.as_indexed().is_some());
    }

    #[test]
    fn a_page_replacement_must_keep_the_shipped_dimensions() {
        let bmp = bmp_of(4, 3, &[[7, 7, 7, 255]; 12]);
        let pack = pack_of(&[
            ("ui/died.tim", &indexed_tim_8bpp(4, 2)),
            ("ui/died.bmp", &bmp),
        ]);
        let asset = pack
            .read_asset("ui/died", crate::pack::AssetClass::Image)
            .unwrap()
            .unwrap();
        assert_eq!(asset.path(), "ui/died.bmp");
        let err = decode_page(&asset, Geometry::Shipped)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("4x3") && err.contains("4x2") && err.contains("keep its dimensions"),
            "{err}"
        );
        // A free-geometry slot takes the same replacement.
        assert!(decode_page(&asset, Geometry::Free).is_ok());
    }

    #[test]
    fn a_texture_reindexes_a_true_colour_font() {
        // A BMP carries no alpha, so every texel is opaque and the re-indexed
        // palette starts at index 1; the transparent case is
        // `reindexing_keeps_colours_exactly`.
        let sheet = bmp_of(4, 1, &[[9, 8, 7, 255]; 4]);
        let pack = pack_of(&[
            ("font/font.tim", &four_bpp_tim(4, 1)),
            ("font/font.bmp", &sheet),
        ]);
        let asset = pack
            .read_asset("font/font", crate::pack::AssetClass::Image)
            .unwrap()
            .unwrap();
        assert_eq!(asset.path(), "font/font.bmp");
        let texture = decode_texture(&asset, Geometry::Shipped).unwrap();
        assert_eq!((texture.width, texture.height), (4, 1));
        assert_eq!(texture.indices, [1, 1, 1, 1]);
        assert_eq!(texture.palette(0, 1), [9, 8, 7, 255]);
    }

    #[cfg(feature = "hd")]
    #[test]
    fn png_and_webp_dimensions_read_the_header() {
        let rgba = [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255];
        let png = png_of(2, 2, &rgba);
        assert_eq!(png_dimensions(&png).unwrap(), (2, 2));
        let webp = webp_of(2, 2, &rgba);
        assert_eq!(webp_dimensions(&webp).unwrap(), (2, 2));
        let err = png_dimensions(b"not a png").unwrap_err().to_string();
        assert!(err.contains("truncated"), "{err}");
        let err = png_dimensions(&[0u8; 32]).unwrap_err().to_string();
        assert!(err.contains("signature"), "{err}");
    }

    #[cfg(feature = "hd")]
    #[test]
    fn png_and_webp_decode_to_the_same_rgba() {
        let rgba = [
            255, 0, 0, 255, //
            0, 255, 0, 128, //
            0, 0, 255, 0, //
            9, 9, 9, 255,
        ];
        let png = png_of(2, 2, &rgba);
        let webp = webp_of(2, 2, &rgba);
        let from_png = decode_png(&png).unwrap();
        let from_webp = decode_webp(&webp).unwrap();
        assert_eq!((from_png.width, from_png.height), (2, 2));
        assert_eq!(from_png.rgba, rgba);
        assert_eq!(from_webp.rgba, rgba);
    }

    #[cfg(feature = "hd")]
    #[test]
    fn an_rgb_png_widens_to_rgba() {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[1, 2, 3]).unwrap();
        writer.finish().unwrap();
        let image = decode_png(&bytes).unwrap();
        assert_eq!(image.rgba, [1, 2, 3, 255]);
    }

    #[cfg(feature = "hd")]
    #[test]
    fn oversized_png_dimensions_are_refused_before_allocating() {
        // A 4096x4097 IHDR is one pixel over the cap.
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1A\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&4096u32.to_be_bytes());
        png.extend_from_slice(&4097u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        // The dimensions are refused before the CRC or any pixel is read.
        let err = decode_png(&png).unwrap_err().to_string();
        assert!(
            err.contains("limit") || err.contains("invalid PNG"),
            "{err}"
        );
    }

    /// A pack holding exactly these entries.
    fn pack_of(entries: &[(&str, &[u8])]) -> crate::pack::Pack {
        let mut writer = crate::pack::PackWriter::new();
        for (path, data) in entries {
            writer.add(path, data.to_vec()).unwrap();
        }
        crate::pack::Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    /// A pack holding `base`, layered with a mod pack holding `layer`.
    fn layered_pack(base: &[(&str, &[u8])], layer: &[(&str, &[u8])]) -> crate::pack::Pack {
        // A per-call label, not a pointer: a slice's `{:p}` renders as
        // `Pointer { addr: ..., metadata: ... }`, which no filesystem accepts
        // (Windows rejects the braces and colon outright).
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let label = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("arklay-assets-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base_path = dir.join("base.akpak");
        let layer_path = dir.join("layer.akpak");
        let mut writer = crate::pack::PackWriter::new();
        writer
            .add(
                "manifest.toml",
                crate::manifest::Manifest::base("re1").render().into_bytes(),
            )
            .unwrap();
        for (path, data) in base {
            writer.add(path, data.to_vec()).unwrap();
        }
        writer.write(&base_path).unwrap();
        let mut writer = crate::pack::PackWriter::new();
        writer.add("manifest.toml", mod_manifest()).unwrap();
        for (path, data) in layer {
            writer.add(path, data.to_vec()).unwrap();
        }
        writer.write(&layer_path).unwrap();
        let pack = crate::pack::Pack::open_layered(&base_path, &[layer_path]).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        pack
    }

    /// A mod manifest for the layered-pack helper.
    fn mod_manifest() -> Vec<u8> {
        let mut manifest = crate::manifest::Manifest::base("layer");
        manifest.kind = crate::manifest::PackKind::Mod;
        manifest.base = Some("re1".to_string());
        manifest.render().into_bytes()
    }

    /// A 24-bit BMP of `width`x`height` with the given RGBA pixels.
    fn bmp_of(width: u32, height: u32, rgba: &[[u8; 4]]) -> Vec<u8> {
        let image = Image {
            width,
            height,
            rgba: rgba.concat(),
        };
        bmp::encode_to_vec(&image).unwrap()
    }

    /// A PNG of `width`x`height` with the given RGBA pixels.
    #[cfg(feature = "hd")]
    fn png_of(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        writer.finish().unwrap();
        bytes
    }

    /// A lossless WebP of `width`x`height` with the given RGBA pixels.
    #[cfg(feature = "hd")]
    fn webp_of(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        image_webp::WebPEncoder::new(&mut bytes)
            .encode(rgba, width, height, image_webp::ColorType::Rgba8)
            .unwrap();
        bytes
    }

    /// A mode-2 (16bpp direct) TIM.
    fn direct_tim(width: u16, height: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x10u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&12u32.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        for _ in 0..width as usize * height as usize {
            out.extend_from_slice(&0x7FFFu16.to_le_bytes());
        }
        out
    }

    /// A mode-1 (8bpp indexed) TIM with a 256-entry CLUT.
    fn indexed_tim_8bpp(width: u16, height: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x10u32.to_le_bytes());
        out.extend_from_slice(&(1u32 | (1 << 3)).to_le_bytes());
        let clut_length = (12 + 256 * 2) as u32;
        out.extend_from_slice(&clut_length.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&256u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        for entry in 0..256u16 {
            out.extend_from_slice(&entry.to_le_bytes());
        }
        out.extend_from_slice(&12u32.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&(width / 2).to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend(std::iter::repeat_n(1u8, width as usize * height as usize));
        out
    }

    /// A mode-0 (4bpp indexed) TIM with a 16-entry CLUT.
    fn four_bpp_tim(width: u16, height: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x10u32.to_le_bytes());
        out.extend_from_slice(&(1u32 << 3).to_le_bytes());
        let clut_length = (12 + 16 * 2) as u32;
        out.extend_from_slice(&clut_length.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        for entry in 0..16u16 {
            out.extend_from_slice(&entry.to_le_bytes());
        }
        out.extend_from_slice(&12u32.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&(width / 4).to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend(std::iter::repeat_n(
            0u8,
            width as usize * height as usize / 2,
        ));
        out
    }
}
