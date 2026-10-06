//! Cinepak (`cvid`) frame decoder.
//!
//! The decoder carries two codebooks per strip and the previous canvas across
//! frames. One frame is a 10-byte big-endian header, then one 12-byte header
//! per strip, then per strip a run of chunks: codebooks (`0x20`-`0x27`) and
//! vector data (`0x30`-`0x32`). A frame is decoded into a scratch copy of the
//! canvas and a cloned strip set, and only committed on success, so a
//! malformed frame leaves the previous picture and its codebooks intact.
//!
//! The shipped corpus uses exactly: intra frames whose first strip carries a
//! full `0x20`/`0x22` codebook and whose second strip inherits it, delta frames
//! whose strips carry selective `0x21`/`0x23` updates, `0x30`/`0x32` vectors on
//! intra strips and `0x31` vectors on delta strips. Unknown strip and chunk
//! ids are rejected.

use anyhow::{Context, Result, bail};

use crate::budget;

/// The frame header is a flags byte, a 24-bit size, width, height and strip
/// count.
const FRAME_HEADER: usize = 10;
/// One strip header: id, 24-bit size and four 16-bit coordinates.
const STRIP_HEADER: usize = 12;
/// A chunk header: id and a 24-bit size that includes the header.
const CHUNK_HEADER: usize = 4;
/// Codebook entries per table.
const ENTRY_COUNT: usize = 256;
/// One expanded entry: four RGB pixels (2x2).
const ENTRY_LEN: usize = 12;
/// The most strips a frame may declare, matching the codec's design limit.
const MAX_STRIPS: usize = 32;

/// Intra-coded strip id.
const INTRA_STRIP: u8 = 0x10;
/// Inter-coded strip id.
const INTER_STRIP: u8 = 0x11;

/// One V1 entry per 4x4 block (the entry is stretched 2x2 to 4x4).
const VECTOR_V1: u8 = 0x32;
/// One flag bit per block: set picks V4, clear picks V1.
const VECTOR_FLAG: u8 = 0x30;
/// Two flag bits per block: `0` skips, `10` is V1, `11` is V4.
const VECTOR_SELECT: u8 = 0x31;

/// One strip's live codebooks, expanded to RGB entries.
#[derive(Debug, Clone)]
struct Strip {
    /// V4 entries: four 2x2 vectors tile each 4x4 block.
    v4: [[u8; ENTRY_LEN]; ENTRY_COUNT],
    /// V1 entries: one 2x2 vector stretched over each 4x4 block.
    v1: [[u8; ENTRY_LEN]; ENTRY_COUNT],
}

impl Default for Strip {
    fn default() -> Self {
        Strip {
            v4: [[0; ENTRY_LEN]; ENTRY_COUNT],
            v1: [[0; ENTRY_LEN]; ENTRY_COUNT],
        }
    }
}

/// A Cinepak decoder for one fixed frame size.
#[derive(Debug)]
pub struct Decoder {
    width: u16,
    height: u16,
    /// The presented canvas, packed RGB.
    rgb: Vec<u8>,
    /// The transaction copy a frame is decoded into.
    scratch: Vec<u8>,
    /// Per-strip codebooks, carried across frames.
    strips: Vec<Strip>,
}

impl Decoder {
    /// Create a decoder for a `width`x`height` canvas, initially black.
    ///
    /// The canvas is capped at [`budget::MAX_CINEPAK_PIXELS`]; an oversized
    /// request fails before the two canvases are allocated.
    pub fn new(width: u16, height: u16) -> Result<Self> {
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .context("cinepak canvas dimensions overflow")?;
        budget::check_len(pixels, budget::MAX_CINEPAK_PIXELS, "cinepak canvas pixels")?;
        let len = pixels
            .checked_mul(3)
            .context("cinepak canvas size overflows")?;
        let mut rgb = budget::alloc::<u8>(len, "cinepak canvas")?;
        let mut scratch = budget::alloc::<u8>(len, "cinepak scratch canvas")?;
        rgb.resize(len, 0);
        scratch.resize(len, 0);
        Ok(Decoder {
            width,
            height,
            rgb,
            scratch,
            strips: Vec::new(),
        })
    }

    /// Decode one frame over the previous canvas.
    ///
    /// On error the presented canvas and the per-strip codebooks are both
    /// unchanged: the frame is decoded into a copy of the strip set and only
    /// committed on success, so the next decode still uses the previous
    /// picture and its codebooks.
    pub fn decode(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() < FRAME_HEADER {
            bail!(
                "cinepak frame is {} bytes, shorter than the 10-byte header",
                frame.len()
            );
        }
        let flags = frame[0];
        let encoded = be24(frame, 1) as usize;
        let width = be16(frame, 4);
        let height = be16(frame, 6);
        let strip_count = be16(frame, 8) as usize;
        if width != self.width || height != self.height {
            bail!(
                "cinepak frame is {width}x{height}, decoder is {}x{}",
                self.width,
                self.height
            );
        }
        if encoded < FRAME_HEADER {
            bail!("cinepak frame declares {encoded} bytes, shorter than its own header");
        }
        if encoded > frame.len() {
            bail!(
                "cinepak frame declares {encoded} bytes but only {} are supplied",
                frame.len()
            );
        }
        if strip_count > MAX_STRIPS {
            bail!("cinepak frame declares {strip_count} strips, more than {MAX_STRIPS}");
        }
        if strip_count == 0 {
            return Ok(());
        }
        if self.strips.len() < strip_count {
            self.strips.resize(strip_count, Strip::default());
        }
        let mut strips = self.strips.clone();
        self.scratch.copy_from_slice(&self.rgb);
        decode_into(
            &mut strips,
            &mut self.scratch,
            self.width,
            self.height,
            flags,
            strip_count,
            &frame[FRAME_HEADER..encoded],
        )?;
        self.strips = strips;
        std::mem::swap(&mut self.rgb, &mut self.scratch);
        Ok(())
    }

    /// The presented canvas as packed RGB (`width * height * 3` bytes).
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    /// Expand the canvas into RGBA8 with an opaque alpha.
    pub fn rgba_into(&self, out: &mut [u8]) {
        let pixels = self.rgb.len() / 3;
        assert!(
            out.len() >= pixels * 4,
            "rgba_into needs {} bytes, got {}",
            pixels * 4,
            out.len()
        );
        let (pixels, _) = self.rgb.as_chunks::<3>();
        let (out, _) = out.as_chunks_mut::<4>();
        for (pixel, rgba) in pixels.iter().zip(out.iter_mut()) {
            rgba[0..3].copy_from_slice(pixel);
            rgba[3] = 0xFF;
        }
    }
}

/// Decode every strip of one frame body (the bytes after the frame header).
#[allow(clippy::too_many_arguments)]
fn decode_into(
    strips: &mut [Strip],
    canvas: &mut [u8],
    width: u16,
    height: u16,
    flags: u8,
    strip_count: usize,
    body: &[u8],
) -> Result<()> {
    let mut offset = 0usize;
    let mut previous_bottom = 0u16;
    for index in 0..strip_count {
        if offset + STRIP_HEADER > body.len() {
            bail!("cinepak strip {index} header is truncated");
        }
        let id = body[offset];
        let size = be24(body, offset + 1) as usize;
        let top = be16(body, offset + 4);
        let left = be16(body, offset + 6);
        let bottom = be16(body, offset + 8);
        let right = be16(body, offset + 10);
        // A zero top is the codec's "continue the previous strip" form and
        // carries a height instead of an absolute bottom.
        let (top, bottom) = if top == 0 {
            (previous_bottom, previous_bottom.saturating_add(bottom))
        } else {
            (top, bottom)
        };
        previous_bottom = bottom;
        if id != INTRA_STRIP && id != INTER_STRIP {
            bail!("cinepak strip {index} has unknown id 0x{id:02X}");
        }
        if size < STRIP_HEADER {
            bail!("cinepak strip {index} size {size} is smaller than its 12-byte header");
        }
        if offset + size > body.len() {
            bail!("cinepak strip {index} overruns the frame");
        }
        if left >= right || top >= bottom {
            bail!("cinepak strip {index} has empty geometry {left},{top}..{right},{bottom}");
        }
        if right > width || bottom > height {
            bail!(
                "cinepak strip {index} {left},{top}..{right},{bottom} lies outside {width}x{height}"
            );
        }
        // Flag bit 0 clear means the strip starts from the previous strip's
        // codebooks; when set each strip starts from its own previous state
        // (which the selective chunks then update).
        if index > 0 && flags & 1 == 0 {
            strips[index] = strips[index - 1].clone();
        }
        let strip = &mut strips[index];
        let strip_end = offset + size;
        let mut pos = offset + STRIP_HEADER;
        while pos + CHUNK_HEADER <= strip_end {
            let chunk_id = body[pos];
            let chunk_size = be24(body, pos + 1) as usize;
            if chunk_size < CHUNK_HEADER {
                bail!(
                    "cinepak chunk 0x{chunk_id:02X} size {chunk_size} is smaller than its header"
                );
            }
            let payload_start = pos + CHUNK_HEADER;
            let payload_end = payload_start + (chunk_size - CHUNK_HEADER);
            if payload_end > strip_end {
                bail!("cinepak chunk 0x{chunk_id:02X} overruns strip {index}");
            }
            let payload = &body[payload_start..payload_end];
            match chunk_id {
                0x20..=0x27 => decode_codebook(strip, chunk_id, payload)?,
                0x30..=0x32 => decode_vectors(
                    strip, canvas, width, height, chunk_id, payload, left, top, right, bottom,
                )?,
                _ => bail!("cinepak strip {index} has unknown chunk id 0x{chunk_id:02X}"),
            }
            pos = payload_end;
        }
        if pos != strip_end {
            bail!("cinepak strip {index} has {} unread bytes", strip_end - pos);
        }
        offset = strip_end;
    }
    if offset != body.len() {
        bail!("cinepak frame has {} trailing bytes", body.len() - offset);
    }
    Ok(())
}

/// Decode one codebook chunk into the strip's V1 or V4 table.
///
/// Chunk bit 0x04 picks 4-byte (grey) entries over 6-byte (luma+chroma) ones,
/// bit 0x02 picks the V1 table over V4, and bit 0x01 marks a selective update
/// whose 32-bit flag words gate which entries are replaced.
fn decode_codebook(strip: &mut Strip, chunk_id: u8, data: &[u8]) -> Result<()> {
    let entry_size = if chunk_id & 0x04 != 0 { 4 } else { 6 };
    let selective = chunk_id & 0x01 != 0;
    let table = if chunk_id & 0x02 == 0 {
        &mut strip.v4
    } else {
        &mut strip.v1
    };
    let mut pos = 0usize;
    let mut flag = 0u32;
    let mut mask = 0u32;
    let mut index = 0usize;
    while index < ENTRY_COUNT {
        if selective {
            mask >>= 1;
            if mask == 0 {
                if pos + 4 > data.len() {
                    break;
                }
                flag = be32(data, pos);
                pos += 4;
                mask = 0x8000_0000;
            }
            if flag & mask == 0 {
                index += 1;
                continue;
            }
        }
        if pos + entry_size > data.len() {
            break;
        }
        table[index] = expand_entry(&data[pos..pos + entry_size]);
        pos += entry_size;
        index += 1;
    }
    Ok(())
}

/// Decode one vector chunk over the strip's rectangle.
#[allow(clippy::too_many_arguments)]
fn decode_vectors(
    strip: &Strip,
    canvas: &mut [u8],
    width: u16,
    height: u16,
    chunk_id: u8,
    data: &[u8],
    left: u16,
    top: u16,
    right: u16,
    bottom: u16,
) -> Result<()> {
    let mut bits = VectorBits::new(data);
    for y in (top..bottom).step_by(4) {
        for x in (left..right).step_by(4) {
            match chunk_id {
                VECTOR_SELECT => {
                    if !bits.bit()? {
                        continue;
                    }
                    if bits.bit()? {
                        let indices = bits.indices()?;
                        paint_v4(canvas, width, height, x, y, &v4_entries(strip, indices));
                    } else {
                        paint_v1(
                            canvas,
                            width,
                            height,
                            x,
                            y,
                            &strip.v1[usize::from(bits.byte()?)],
                        );
                    }
                }
                VECTOR_FLAG => {
                    if bits.bit()? {
                        let indices = bits.indices()?;
                        paint_v4(canvas, width, height, x, y, &v4_entries(strip, indices));
                    } else {
                        paint_v1(
                            canvas,
                            width,
                            height,
                            x,
                            y,
                            &strip.v1[usize::from(bits.byte()?)],
                        );
                    }
                }
                VECTOR_V1 => {
                    paint_v1(
                        canvas,
                        width,
                        height,
                        x,
                        y,
                        &strip.v1[usize::from(bits.byte()?)],
                    );
                }
                _ => unreachable!("caller filters the vector chunk ids"),
            }
        }
    }
    Ok(())
}

/// The four V4 entries a block references.
fn v4_entries(strip: &Strip, indices: [u8; 4]) -> [[u8; ENTRY_LEN]; 4] {
    [
        strip.v4[usize::from(indices[0])],
        strip.v4[usize::from(indices[1])],
        strip.v4[usize::from(indices[2])],
        strip.v4[usize::from(indices[3])],
    ]
}

/// Packed RGB expansion of one codebook entry: grey for 4 bytes, luma plus
/// chroma for 6. The chroma weights are the codec's integer matrix,
/// `r = y + 2v`, `g = y - u/2 - v`, `b = y + 2u`, clamped per channel.
fn expand_entry(entry: &[u8]) -> [u8; ENTRY_LEN] {
    let mut out = [0u8; ENTRY_LEN];
    if entry.len() == 4 {
        for (pixel, &y) in entry.iter().enumerate() {
            out[pixel * 3] = y;
            out[pixel * 3 + 1] = y;
            out[pixel * 3 + 2] = y;
        }
    } else {
        let u = i32::from(entry[4] as i8);
        let v = i32::from(entry[5] as i8);
        for (pixel, &y) in entry[0..4].iter().enumerate() {
            let y = i32::from(y);
            out[pixel * 3] = clamp(y + 2 * v);
            out[pixel * 3 + 1] = clamp(y - u / 2 - v);
            out[pixel * 3 + 2] = clamp(y + 2 * u);
        }
    }
    out
}

fn clamp(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Paint a 4x4 block from one V1 entry, stretching each 2x2 entry pixel over
/// 2x2 output pixels and clipping the last block of a partial strip.
fn paint_v1(canvas: &mut [u8], width: u16, height: u16, x: u16, y: u16, entry: &[u8; ENTRY_LEN]) {
    let width = usize::from(width);
    let height = usize::from(height);
    for row in 0..4usize {
        let py = usize::from(y) + row;
        if py >= height {
            break;
        }
        let entry_row = (row / 2) * 6;
        for col in 0..4usize {
            let px = usize::from(x) + col;
            if px >= width {
                break;
            }
            let source = if col < 2 { entry_row } else { entry_row + 3 };
            let target = (py * width + px) * 3;
            canvas[target..target + 3].copy_from_slice(&entry[source..source + 3]);
        }
    }
}

/// Paint a 4x4 block from four V4 entries tiling it `e0 e1 / e2 e3`, clipping
/// the last block of a partial strip.
fn paint_v4(
    canvas: &mut [u8],
    width: u16,
    height: u16,
    x: u16,
    y: u16,
    entries: &[[u8; ENTRY_LEN]; 4],
) {
    let width = usize::from(width);
    let height = usize::from(height);
    for row in 0..4usize {
        let py = usize::from(y) + row;
        if py >= height {
            break;
        }
        let (left, right) = if row < 2 {
            (&entries[0], &entries[1])
        } else {
            (&entries[2], &entries[3])
        };
        let entry_row = if row % 2 == 0 { 0 } else { 6 };
        for col in 0..4usize {
            let px = usize::from(x) + col;
            if px >= width {
                break;
            }
            let (source, offset) = if col < 2 {
                (left, entry_row + if col == 0 { 0 } else { 3 })
            } else {
                (right, entry_row + if col == 2 { 0 } else { 3 })
            };
            let target = (py * width + px) * 3;
            canvas[target..target + 3].copy_from_slice(&source[offset..offset + 3]);
        }
    }
}

/// The lazy big-endian bit/byte reader the vector chunks share.
struct VectorBits<'a> {
    data: &'a [u8],
    pos: usize,
    flag: u32,
    mask: u32,
}

impl<'a> VectorBits<'a> {
    fn new(data: &'a [u8]) -> Self {
        VectorBits {
            data,
            pos: 0,
            flag: 0,
            mask: 0,
        }
    }

    /// The next flag bit, reading the next 32-bit word when one runs out.
    fn bit(&mut self) -> Result<bool> {
        self.mask >>= 1;
        if self.mask == 0 {
            let word = self
                .data
                .get(self.pos..self.pos + 4)
                .context("cinepak vector flag word is truncated")?;
            self.flag = be32(word, 0);
            self.pos += 4;
            self.mask = 0x8000_0000;
        }
        Ok(self.flag & self.mask != 0)
    }

    /// One V1 index byte.
    fn byte(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.pos)
            .context("cinepak V1 vector index is truncated")?;
        self.pos += 1;
        Ok(byte)
    }

    /// Four V4 index bytes.
    fn indices(&mut self) -> Result<[u8; 4]> {
        let bytes = self
            .data
            .get(self.pos..self.pos + 4)
            .context("cinepak V4 vector indices are truncated")?;
        self.pos += 4;
        Ok([bytes[0], bytes[1], bytes[2], bytes[3]])
    }
}

fn be16(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(data[offset..offset + 2].try_into().unwrap())
}

fn be24(data: &[u8], offset: usize) -> u32 {
    (u32::from(data[offset]) << 16)
        | (u32::from(data[offset + 1]) << 8)
        | u32::from(data[offset + 2])
}

fn be32(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 24-bit big-endian value.
    fn be24v(value: u32) -> [u8; 3] {
        [(value >> 16) as u8, (value >> 8) as u8, value as u8]
    }

    /// One chunk: id, 24-bit size including the header, payload.
    fn chunk(id: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![id];
        out.extend_from_slice(&be24v((CHUNK_HEADER + payload.len()) as u32));
        out.extend_from_slice(payload);
        out
    }

    /// One strip: header plus the concatenated chunks.
    fn strip(id: u8, x0: u16, y0: u16, x1: u16, y1: u16, chunks: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = chunks.concat();
        let mut out = vec![id];
        out.extend_from_slice(&be24v((STRIP_HEADER + payload.len()) as u32));
        out.extend_from_slice(&y0.to_be_bytes());
        out.extend_from_slice(&x0.to_be_bytes());
        out.extend_from_slice(&y1.to_be_bytes());
        out.extend_from_slice(&x1.to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// One frame: flags, 24-bit size, size, strip count, strips.
    fn frame(flags: u8, width: u16, height: u16, strips: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = strips.concat();
        let mut out = vec![flags];
        out.extend_from_slice(&be24v((FRAME_HEADER + body.len()) as u32));
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&(strips.len() as u16).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// A grey (`4` bytes/entry) or colour (`6` bytes/entry) codebook payload.
    fn codebook(entries: &[[u8; 6]], colour: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in entries {
            if colour {
                out.extend_from_slice(entry);
            } else {
                out.extend_from_slice(&entry[0..4]);
            }
        }
        out
    }

    /// A V4 vector chunk with one flag bit and four indices per block.
    fn vectors_v4(flags: u32, indices: &[u8]) -> Vec<u8> {
        let mut out = flags.to_be_bytes().to_vec();
        out.extend_from_slice(indices);
        out
    }

    fn grey(value: u8) -> [u8; 6] {
        [value, value, value, value, 0, 0]
    }

    /// The red channel of one pixel of the grey test frames.
    fn at(decoder: &Decoder, x: usize, y: usize) -> u8 {
        decoder.rgb()[(y * usize::from(decoder.width) + x) * 3]
    }

    fn rgb_at(decoder: &Decoder, x: usize, y: usize) -> [u8; 3] {
        let offset = (y * usize::from(decoder.width) + x) * 3;
        decoder.rgb()[offset..offset + 3].try_into().unwrap()
    }

    #[test]
    fn rejects_canvases_over_the_pixel_cap() {
        let message = budget::assert_cap_error(Decoder::new(0xFFFF, 0xFFFF));
        assert!(message.contains("canvas pixels"), "{message}");
        assert!(Decoder::new(320, 240).is_ok());
    }

    #[test]
    fn v1_codebook_and_v1_vectors_paint_stretched_blocks() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let entries = [[10, 20, 30, 40, 0, 0], [50, 60, 70, 80, 0, 0]];
        let codebooks = chunk(0x26, &codebook(&entries, false));
        let vectors = chunk(VECTOR_V1, &[0, 1, 1, 0]);
        let frame = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 8, 8, &[codebooks, vectors])],
        );
        decoder.decode(&frame).unwrap();
        // Entry [10,20,30,40] is 2x2 and stretches to the 4x4 block.
        assert_eq!(at(&decoder, 0, 0), 10);
        assert_eq!(at(&decoder, 2, 0), 20);
        assert_eq!(at(&decoder, 0, 2), 30);
        assert_eq!(at(&decoder, 2, 2), 40);
        assert_eq!(at(&decoder, 4, 0), 50);
        assert_eq!(at(&decoder, 6, 0), 60);
        assert_eq!(at(&decoder, 4, 2), 70);
        assert_eq!(at(&decoder, 6, 2), 80);
        assert_eq!(at(&decoder, 0, 4), 50);
        assert_eq!(at(&decoder, 4, 4), 10);
    }

    #[test]
    fn v4_codebook_and_v4_vectors_tile_four_entries() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let entries: Vec<[u8; 6]> = (0..8)
            .map(|i| [1 + i * 4, 2 + i * 4, 3 + i * 4, 4 + i * 4, 0, 0])
            .collect();
        let codebook = chunk(0x24, &codebook(&entries, false));
        let vectors = chunk(
            VECTOR_FLAG,
            &vectors_v4(
                0xF000_0000,
                &[0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 0, 2, 1, 3],
            ),
        );
        let frame = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 8, 8, &[codebook, vectors])],
        );
        decoder.decode(&frame).unwrap();
        assert_eq!(at(&decoder, 0, 0), 1);
        assert_eq!(at(&decoder, 1, 0), 2);
        assert_eq!(at(&decoder, 2, 0), 5);
        assert_eq!(at(&decoder, 0, 1), 3);
        assert_eq!(at(&decoder, 0, 2), 9);
        assert_eq!(at(&decoder, 2, 2), 13);
        assert_eq!(at(&decoder, 3, 3), 16);
        assert_eq!(at(&decoder, 4, 0), 17);
    }

    #[test]
    fn flag_vectors_mix_v4_and_v1_per_block() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let v4 = chunk(0x20, &codebook(&[grey(1), grey(2), grey(3), grey(4)], true));
        let v1 = chunk(0x26, &codebook(&[grey(90)], false));
        // Block 0 V4, block 1 V1, block 2 V4, block 3 V1.
        let vectors = chunk(
            VECTOR_FLAG,
            &vectors_v4(0xA000_0000, &[0, 1, 2, 3, 0, 0, 1, 2, 3, 0]),
        );
        let frame = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 8, 8, &[v4, v1, vectors])],
        );
        decoder.decode(&frame).unwrap();
        assert_eq!(at(&decoder, 0, 0), 1);
        assert_eq!(at(&decoder, 3, 3), 4);
        assert_eq!(at(&decoder, 4, 0), 90);
        assert_eq!(at(&decoder, 4, 4), 90);
    }

    #[test]
    fn selective_vectors_skip_unchanged_blocks() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let codebook = chunk(0x26, &codebook(&[grey(100), grey(200)], false));
        let paint = chunk(VECTOR_V1, &[0, 0, 0, 0]);
        let first = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 8, 8, &[codebook, paint])],
        );
        decoder.decode(&first).unwrap();
        assert_eq!(at(&decoder, 0, 0), 100);
        // Coded V1 block 0, skip block 1, coded V1 block 2, skip block 3.
        let select = chunk(0x31, &vectors_v4(0x9000_0000, &[1, 1]));
        let second = frame(1, 8, 8, &[strip(INTER_STRIP, 0, 0, 8, 8, &[select])]);
        decoder.decode(&second).unwrap();
        assert_eq!(at(&decoder, 0, 0), 200);
        assert_eq!(at(&decoder, 4, 0), 100);
        assert_eq!(at(&decoder, 0, 4), 200);
        assert_eq!(at(&decoder, 4, 4), 100);
    }

    #[test]
    fn touch_up_updates_selected_entries_only() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let full = chunk(0x26, &codebook(&[grey(10), grey(20)], false));
        let paint = chunk(VECTOR_V1, &[0, 1, 0, 1]);
        let first = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 8, 8, &[full, paint.clone()])],
        );
        decoder.decode(&first).unwrap();
        assert_eq!(at(&decoder, 0, 0), 10);
        assert_eq!(at(&decoder, 4, 0), 20);
        let mut payload = 0x8000_0000u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&[99, 99, 99, 99]);
        let touch = chunk(0x27, &payload);
        let second = frame(1, 8, 8, &[strip(INTER_STRIP, 0, 0, 8, 8, &[touch, paint])]);
        decoder.decode(&second).unwrap();
        assert_eq!(at(&decoder, 0, 0), 99);
        assert_eq!(at(&decoder, 4, 0), 20);
    }

    #[test]
    fn second_strip_inherits_the_first_codebook() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let codebook = chunk(0x26, &codebook(&[grey(77)], false));
        let paint = chunk(VECTOR_V1, &[0, 0]);
        let first = strip(INTRA_STRIP, 0, 0, 8, 4, &[codebook, paint]);
        let inherited = strip(INTRA_STRIP, 0, 4, 8, 8, &[chunk(VECTOR_V1, &[0, 0])]);
        decoder
            .decode(&frame(0, 8, 8, &[first, inherited]))
            .unwrap();
        assert_eq!(at(&decoder, 0, 0), 77);
        assert_eq!(at(&decoder, 0, 4), 77);
    }

    #[test]
    fn zero_top_continues_the_previous_strip() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let codebook = chunk(0x26, &codebook(&[grey(40)], false));
        let paint = chunk(VECTOR_V1, &[0, 0]);
        let first = strip(INTRA_STRIP, 0, 0, 8, 4, &[codebook, paint]);
        // A zero top plus the bottom field as a height: rows 4..8.
        let continued = strip(INTRA_STRIP, 0, 0, 8, 4, &[chunk(VECTOR_V1, &[0, 0])]);
        decoder
            .decode(&frame(0, 8, 8, &[first, continued]))
            .unwrap();
        assert_eq!(at(&decoder, 0, 0), 40);
        assert_eq!(at(&decoder, 0, 4), 40);
        assert_eq!(at(&decoder, 7, 7), 40);
    }

    #[test]
    fn partial_strips_clip_bottom_and_right_edges() {
        let mut decoder = Decoder::new(6, 6).unwrap();
        let codebook = chunk(0x26, &codebook(&[grey(10)], false));
        let paint = chunk(VECTOR_V1, &[0, 0]);
        let strip = strip(INTRA_STRIP, 0, 0, 6, 4, &[codebook, paint]);
        decoder.decode(&frame(0, 6, 6, &[strip])).unwrap();
        assert_eq!(at(&decoder, 0, 0), 10);
        assert_eq!(at(&decoder, 4, 0), 10);
        assert_eq!(at(&decoder, 5, 3), 10);
        assert_eq!(at(&decoder, 0, 4), 0);
        assert_eq!(at(&decoder, 5, 5), 0);
    }

    #[test]
    fn yuv_expansion_uses_the_codec_weights_and_clamps() {
        let mut decoder = Decoder::new(4, 4).unwrap();
        let warm = chunk(0x20, &codebook(&[[250, 250, 250, 250, 127, 127]], true));
        let cool = chunk(0x20, &codebook(&[[0, 0, 0, 0, 128, 128]], true));
        let paint = chunk(VECTOR_FLAG, &vectors_v4(0x8000_0000, &[0, 0, 0, 0]));
        let first = frame(
            0,
            4,
            4,
            &[strip(INTRA_STRIP, 0, 0, 4, 4, &[warm, paint.clone()])],
        );
        decoder.decode(&first).unwrap();
        assert_eq!(rgb_at(&decoder, 0, 0), [255, 60, 255]);
        let second = frame(0, 4, 4, &[strip(INTRA_STRIP, 0, 0, 4, 4, &[cool, paint])]);
        decoder.decode(&second).unwrap();
        assert_eq!(rgb_at(&decoder, 0, 0), [0, 192, 0]);
    }

    #[test]
    fn empty_frames_keep_the_canvas() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let codebook = chunk(0x26, &codebook(&[grey(33)], false));
        let paint = chunk(VECTOR_V1, &[0]);
        let strip = strip(INTRA_STRIP, 0, 0, 4, 4, &[codebook, paint]);
        decoder.decode(&frame(0, 8, 8, &[strip])).unwrap();
        assert_eq!(at(&decoder, 0, 0), 33);
        decoder.decode(&frame(1, 8, 8, &[])).unwrap();
        assert_eq!(at(&decoder, 0, 0), 33);
    }

    #[test]
    fn malformed_frames_error_and_keep_the_canvas() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let codebook = chunk(0x26, &codebook(&[grey(44)], false));
        let paint = chunk(VECTOR_V1, &[0, 0, 0, 0]);
        let good = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 8, 8, &[codebook, paint])],
        );
        decoder.decode(&good).unwrap();
        let before = decoder.rgb().to_vec();
        assert_eq!(at(&decoder, 0, 0), 44);

        let mut short = good.clone();
        short.truncate(FRAME_HEADER - 1);
        assert!(decoder.decode(&short).is_err());

        let mut declared_long = good.clone();
        declared_long[1..4].copy_from_slice(&be24v(0xFFFF));
        assert!(decoder.decode(&declared_long).is_err());

        let mut wrong_size = good.clone();
        wrong_size[4..6].copy_from_slice(&16u16.to_be_bytes());
        assert!(decoder.decode(&wrong_size).is_err());

        let bad_strip = frame(0, 8, 8, &[strip(0x12, 0, 0, 8, 8, &[])]);
        assert!(decoder.decode(&bad_strip).is_err());

        let bad_chunk = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 8, 8, &[chunk(0x99, &[0])])],
        );
        assert!(decoder.decode(&bad_chunk).is_err());

        let bad_geometry = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 16, 8, &[chunk(VECTOR_V1, &[0])])],
        );
        assert!(decoder.decode(&bad_geometry).is_err());

        // A 0x31 chunk whose flag words run out is truncated.
        let truncated_vectors = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 8, 8, &[chunk(0x31, &[])])],
        );
        assert!(decoder.decode(&truncated_vectors).is_err());

        assert_eq!(decoder.rgb(), before.as_slice());
    }

    #[test]
    fn a_failed_frame_rolls_back_the_codebooks() {
        let mut decoder = Decoder::new(8, 8).unwrap();
        let codebook = chunk(0x26, &codebook(&[grey(100)], false));
        let paint = chunk(VECTOR_V1, &[0, 0, 0, 0]);
        let first = frame(
            0,
            8,
            8,
            &[strip(INTRA_STRIP, 0, 0, 8, 8, &[codebook, paint])],
        );
        decoder.decode(&first).unwrap();
        assert_eq!(at(&decoder, 0, 0), 100);

        // A selective codebook update followed by a truncated vector chunk:
        // the codebook mutates before the frame fails.
        let mut update = 0x8000_0000u32.to_be_bytes().to_vec();
        update.extend_from_slice(&[55, 55, 55, 55, 0, 0]);
        let failing = frame(
            1,
            8,
            8,
            &[strip(
                INTER_STRIP,
                0,
                0,
                8,
                8,
                &[chunk(0x27, &update), chunk(VECTOR_V1, &[0])],
            )],
        );
        assert!(decoder.decode(&failing).is_err());
        assert_eq!(at(&decoder, 0, 0), 100, "the canvas changed on error");

        // A later delta frame without a codebook must still paint the
        // pre-error entry, not the failed frame's update.
        let reuse = frame(
            1,
            8,
            8,
            &[strip(
                INTER_STRIP,
                0,
                0,
                8,
                8,
                &[chunk(VECTOR_V1, &[0, 0, 0, 0])],
            )],
        );
        decoder.decode(&reuse).unwrap();
        assert_eq!(at(&decoder, 0, 0), 100, "the failed codebook was committed");
    }

    #[test]
    fn every_codebook_chunk_id_fills_the_right_table() {
        for id in [0x20u8, 0x21, 0x24, 0x25] {
            let mut strip = Strip::default();
            let entry: [u8; 6] = [11, 22, 33, 44, 0, 0];
            let payload = if id & 0x04 == 0 {
                entry.to_vec()
            } else {
                entry[0..4].to_vec()
            };
            let payload = if id & 0x01 == 0 {
                payload
            } else {
                let mut selective = 0x8000_0000u32.to_be_bytes().to_vec();
                selective.extend_from_slice(&payload);
                selective
            };
            decode_codebook(&mut strip, id, &payload).unwrap();
            assert_ne!(strip.v4[0], [0; ENTRY_LEN], "chunk {id:02X} V4");
            assert_eq!(strip.v1[0], [0; ENTRY_LEN], "chunk {id:02X} V4 only");
        }
        for id in [0x22u8, 0x23, 0x26, 0x27] {
            let mut strip = Strip::default();
            let entry: [u8; 6] = [55, 66, 77, 88, 0, 0];
            let payload = if id & 0x04 == 0 {
                entry.to_vec()
            } else {
                entry[0..4].to_vec()
            };
            let payload = if id & 0x01 == 0 {
                payload
            } else {
                let mut selective = 0x8000_0000u32.to_be_bytes().to_vec();
                selective.extend_from_slice(&payload);
                selective
            };
            decode_codebook(&mut strip, id, &payload).unwrap();
            assert_ne!(strip.v1[0], [0; ENTRY_LEN], "chunk {id:02X} V1");
            assert_eq!(strip.v4[0], [0; ENTRY_LEN], "chunk {id:02X} V1 only");
        }
    }

    #[test]
    fn rgba_expansion_is_opaque() {
        let mut decoder = Decoder::new(2, 2).unwrap();
        let codebook = chunk(0x26, &codebook(&[grey(7)], false));
        let paint = chunk(VECTOR_V1, &[0]);
        let strip = strip(INTRA_STRIP, 0, 0, 2, 2, &[codebook, paint]);
        decoder.decode(&frame(0, 2, 2, &[strip])).unwrap();
        let mut rgba = vec![0u8; 2 * 2 * 4];
        decoder.rgba_into(&mut rgba);
        assert_eq!(&rgba[0..4], &[7, 7, 7, 255]);
        assert_eq!(&rgba[12..16], &[7, 7, 7, 255]);
    }
}
