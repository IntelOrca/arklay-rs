//! Container-independent description of an HD (H.264/AAC) film.
//!
//! An HD film arrives either as MP4 ([`crate::mp4`]) or as H.264/AAC in the
//! shipped RIFF/AVI layout ([`crate::avi`]); both are reduced to a [`Media`]:
//! the file bytes, the byte range of every video access unit in decode order
//! and of every AAC packet. This module also holds the decoder-independent
//! pieces of playback: access-unit framing, the colour description read from
//! the SPS, YUV to RGBA conversion and the box downscale to the 320x240
//! canvas the software renderer draws.

use std::ops::Range;

use anyhow::{Context, Result, bail};

use crate::avi::Avi;

/// `WAVEFORMATEX` tag for raw AAC in an AVI.
pub const WAVE_FORMAT_AAC: u16 = 0x00FF;

/// The largest HD frame accepted, in pixels per side.
pub const MAX_DIMENSION: u32 = 4096;
/// The highest frame rate a film may declare; it also bounds the audio a
/// single frame spans.
pub const MAX_FRAME_RATE: u32 = 240;

/// One HD film's elementary streams.
#[derive(Debug)]
pub struct Media {
    /// The whole file, the backing store of every range below.
    pub data: Vec<u8>,
    /// Coded width in pixels as the container declares it.
    pub width: u32,
    /// Coded height in pixels as the container declares it.
    pub height: u32,
    /// Frames per second as an exact ratio.
    pub frame_rate: (u32, u32),
    /// How the video samples frame their NAL units.
    pub nal: NalFormat,
    /// Every video access unit in decode order.
    pub video: Vec<Range<usize>>,
    /// The audio track, if the film has one.
    pub audio: Option<AacTrack>,
}

/// The NAL framing of the video samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NalFormat {
    /// Start-code delimited, parameter sets inline (an AVI).
    AnnexB,
    /// Big-endian length-prefixed units with out-of-band parameter sets (an
    /// MP4 `avcC`).
    LengthPrefixed {
        /// Bytes of each length prefix: 1, 2 or 4.
        length: u8,
        /// The SPS and PPS units, without start codes.
        parameter_sets: Vec<Vec<u8>>,
    },
}

/// One AAC track.
#[derive(Debug, Clone)]
pub struct AacTrack {
    /// The AudioSpecificConfig.
    pub config: Vec<u8>,
    /// The container's sample rate (the config is authoritative).
    pub sample_rate: u32,
    /// Decoded audio to drop from the start, as `(duration, timescale)`
    /// seconds: the encoder priming an MP4 edit list skips.
    pub priming: (u64, u32),
    /// Every raw AAC packet in order.
    pub packets: Vec<Range<usize>>,
}

/// Whether an AVI's video codec fourcc is H.264.
pub fn is_h264_fourcc(codec: &[u8; 4]) -> bool {
    [
        b"H264", b"h264", b"X264", b"x264", b"AVC1", b"avc1", b"DAVC",
    ]
    .contains(&codec)
}

impl Media {
    /// Parse an HD film from either container, chosen by its magic.
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        if crate::mp4::is_mp4(&data) {
            crate::mp4::parse(data)
        } else if data.starts_with(b"RIFF") {
            Self::from_avi(Avi::parse_any(data)?)
        } else {
            bail!("film is neither MP4 nor AVI")
        }
    }

    /// Describe an H.264/AAC AVI.
    pub fn from_avi(avi: Avi) -> Result<Self> {
        let format = avi.format().clone();
        if !is_h264_fourcc(&format.video_codec) {
            bail!(
                "AVI video codec {:?} is not H.264",
                String::from_utf8_lossy(&format.video_codec)
            );
        }
        let audio = match format.audio.format_tag {
            _ if !avi.has_audio() => None,
            WAVE_FORMAT_AAC => Some(AacTrack {
                config: avi.audio_extra().to_vec(),
                sample_rate: format.audio.sample_rate,
                priming: (0, 1),
                packets: avi
                    .audio_chunks()
                    .iter()
                    .map(|&(offset, len)| offset..offset + len)
                    .collect(),
            }),
            tag => bail!("AVI audio format tag 0x{tag:04X} is not AAC"),
        };
        let video = avi
            .video_chunks()
            .map(|(offset, len)| offset..offset + len)
            .collect();
        Ok(Media {
            width: u32::from(format.width),
            height: u32::from(format.height),
            frame_rate: avi.stream_rate().unwrap_or(format.frame_rate),
            nal: NalFormat::AnnexB,
            video,
            audio,
            data: avi.into_data(),
        })
    }

    /// Check what playback depends on before a decoder is started.
    pub fn validate(&self) -> Result<()> {
        if self.video.is_empty() {
            bail!("film has no video frames");
        }
        let (num, den) = self.frame_rate;
        if den == 0 || num < den || u64::from(num) > u64::from(den) * u64::from(MAX_FRAME_RATE) {
            bail!("film frame rate {num}/{den} is outside 1 to {MAX_FRAME_RATE} fps");
        }
        if self.width == 0
            || self.height == 0
            || self.width > MAX_DIMENSION
            || self.height > MAX_DIMENSION
        {
            bail!("film size {}x{} is out of range", self.width, self.height);
        }
        if let Some(track) = &self.audio
            && track.config.len() < 2
        {
            bail!("AAC track has no AudioSpecificConfig");
        }
        Ok(())
    }

    /// Write access unit `index` to `out` as Annex B, with the out-of-band
    /// parameter sets ahead of every IDR so a decoder restarted there has
    /// them.
    pub fn access_unit(&self, index: usize, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        let range = self
            .video
            .get(index)
            .with_context(|| format!("video frame {index} is missing"))?;
        let bytes = self
            .data
            .get(range.clone())
            .with_context(|| format!("video frame {index} is out of bounds"))?;
        match &self.nal {
            NalFormat::AnnexB => out.extend_from_slice(bytes),
            NalFormat::LengthPrefixed {
                length,
                parameter_sets,
            } => {
                let units = length_prefixed_units(bytes, *length)
                    .with_context(|| format!("video frame {index} has a corrupt NAL length"))?;
                if units.iter().any(|unit| nal_type(unit) == Some(5)) {
                    for unit in parameter_sets {
                        out.extend_from_slice(&[0, 0, 0, 1]);
                        out.extend_from_slice(unit);
                    }
                }
                for unit in units {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(unit);
                }
            }
        }
        Ok(())
    }

    /// Whether access unit `index` holds an IDR slice, a point decoding can
    /// restart from with nothing before it.
    pub fn is_idr(&self, index: usize) -> bool {
        let Some(bytes) = self.video.get(index).and_then(|r| self.data.get(r.clone())) else {
            return false;
        };
        match &self.nal {
            NalFormat::AnnexB => annex_b_units(bytes).any(|unit| nal_type(unit) == Some(5)),
            NalFormat::LengthPrefixed { length, .. } => length_prefixed_units(bytes, *length)
                .is_some_and(|units| units.iter().any(|unit| nal_type(unit) == Some(5))),
        }
    }

    /// The last IDR access unit at or before `index` (0 when there is none:
    /// the stream's first unit is where a decode has to start anyway).
    pub fn idr_at_or_before(&self, index: usize) -> usize {
        (0..=index.min(self.video.len().saturating_sub(1)))
            .rev()
            .find(|&unit| self.is_idr(unit))
            .unwrap_or(0)
    }

    /// The film's colour description from its first SPS, or BT.601 limited
    /// range when none is found or the SPS carries no VUI colour. Untagged
    /// files are usually ffmpeg encodes, whose RGB to YUV conversion is
    /// BT.601 unless told otherwise, whatever the frame size.
    pub fn colour(&self) -> Colour {
        let fallback = Colour {
            matrix: Matrix::Bt601,
            full_range: false,
        };
        let parsed = match &self.nal {
            NalFormat::LengthPrefixed { parameter_sets, .. } => parameter_sets
                .iter()
                .find(|unit| nal_type(unit) == Some(7))
                .and_then(|sps| sps_colour(sps)),
            NalFormat::AnnexB => self.video.iter().take(4).find_map(|range| {
                annex_b_units(self.data.get(range.clone())?)
                    .find(|unit| nal_type(unit) == Some(7))
                    .and_then(sps_colour)
            }),
        };
        parsed.unwrap_or(fallback)
    }
}

/// The type of one NAL unit (its header's low five bits).
pub fn nal_type(unit: &[u8]) -> Option<u8> {
    unit.first().map(|header| header & 0x1F)
}

/// Split a length-prefixed sample into its NAL units, or `None` when a length
/// overruns the sample.
fn length_prefixed_units(bytes: &[u8], length: u8) -> Option<Vec<&[u8]>> {
    let length = usize::from(length);
    if !(1..=4).contains(&length) {
        return None;
    }
    let mut units = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let prefix = bytes.get(pos..pos + length)?;
        let len = prefix
            .iter()
            .fold(0usize, |acc, &byte| (acc << 8) | usize::from(byte));
        pos += length;
        units.push(bytes.get(pos..pos + len)?);
        pos += len;
    }
    Some(units)
}

/// The NAL units of an Annex B byte stream, start codes removed.
pub fn annex_b_units(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let start = find_start_code(bytes, pos)?;
        let body = start + 3;
        let end = find_start_code(bytes, body).map_or(bytes.len(), |next| {
            // A four-byte start code's leading zero belongs to the next code.
            if next > body && bytes[next - 1] == 0 {
                next - 1
            } else {
                next
            }
        });
        pos = end;
        Some(&bytes[body..end.max(body)])
    })
}

/// The offset of the next `00 00 01` at or after `from`.
fn find_start_code(bytes: &[u8], from: usize) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(3)
        .position(|window| window == [0, 0, 1])
        .map(|offset| from + offset)
}

/// The YUV to RGB matrix of a film.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    /// ITU-R BT.601 (SD).
    Bt601,
    /// ITU-R BT.709 (HD).
    Bt709,
}

/// A film's colour description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    /// The matrix coefficients.
    pub matrix: Matrix,
    /// Whether luma spans 0-255 rather than 16-235.
    pub full_range: bool,
}

/// Read the VUI colour description out of an SPS NAL unit (with header).
///
/// Returns `None` when the SPS is malformed or carries no colour
/// description, so the caller's default applies. Matrix codes other than
/// BT.709 (1) are treated as BT.601, which is also what an unspecified matrix
/// decodes as in common players.
pub fn sps_colour(sps: &[u8]) -> Option<Colour> {
    let rbsp = unescape(sps.get(1..)?);
    let mut bits = Bits::new(&rbsp);
    let profile = bits.read(8)?;
    bits.skip(16)?;
    bits.ue()?;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format = bits.ue()?;
        if chroma_format == 3 {
            bits.skip(1)?;
        }
        bits.ue()?;
        bits.ue()?;
        bits.skip(1)?;
        if bits.read(1)? == 1 {
            let lists = if chroma_format == 3 { 12 } else { 8 };
            for list in 0..lists {
                if bits.read(1)? == 1 {
                    let size = if list < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + bits.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    bits.ue()?;
    let poc_type = bits.ue()?;
    if poc_type == 0 {
        bits.ue()?;
    } else if poc_type == 1 {
        bits.skip(1)?;
        bits.se()?;
        bits.se()?;
        let cycle = bits.ue()?;
        if cycle > 255 {
            return None;
        }
        for _ in 0..cycle {
            bits.se()?;
        }
    }
    bits.ue()?;
    bits.skip(1)?;
    bits.ue()?;
    bits.ue()?;
    if bits.read(1)? == 0 {
        bits.skip(1)?;
    }
    bits.skip(1)?;
    if bits.read(1)? == 1 {
        bits.ue()?;
        bits.ue()?;
        bits.ue()?;
        bits.ue()?;
    }
    if bits.read(1)? == 0 {
        return None;
    }
    // vui_parameters
    if bits.read(1)? == 1 && bits.read(8)? == 255 {
        bits.skip(32)?;
    }
    if bits.read(1)? == 1 {
        bits.skip(1)?;
    }
    if bits.read(1)? == 0 {
        return None;
    }
    bits.skip(3)?;
    let full_range = bits.read(1)? == 1;
    if bits.read(1)? == 0 {
        return Some(Colour {
            matrix: Matrix::Bt601,
            full_range,
        });
    }
    bits.skip(16)?;
    let matrix = match bits.read(8)? {
        1 => Matrix::Bt709,
        _ => Matrix::Bt601,
    };
    Some(Colour { matrix, full_range })
}

/// Remove emulation-prevention bytes (`00 00 03`).
fn unescape(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut zeros = 0;
    for &byte in bytes {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// An MSB-first bit reader with Exp-Golomb codes.
struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Bits { bytes, pos: 0 }
    }

    fn read(&mut self, count: u32) -> Option<u64> {
        let mut value = 0u64;
        for _ in 0..count {
            let byte = *self.bytes.get(self.pos / 8)?;
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            value = (value << 1) | u64::from(bit);
            self.pos += 1;
        }
        Some(value)
    }

    fn skip(&mut self, count: u32) -> Option<()> {
        self.read(count).map(|_| ())
    }

    fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u64 << zeros) - 1 + self.read(zeros)?)
    }

    fn se(&mut self) -> Option<i64> {
        let code = self.ue()? as i64;
        Some(if code & 1 == 1 {
            (code + 1) / 2
        } else {
            -(code / 2)
        })
    }
}

/// Fixed-point (16.16) YUV to RGB coefficients for one colour description.
struct Coefficients {
    luma: i32,
    luma_offset: i32,
    red_v: i32,
    green_u: i32,
    green_v: i32,
    blue_u: i32,
}

impl Coefficients {
    fn new(colour: Colour) -> Self {
        let (kr, kb) = match colour.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
        };
        let kg = 1.0 - kr - kb;
        let (luma, chroma, luma_offset) = if colour.full_range {
            (1.0, 1.0, 0)
        } else {
            (255.0 / 219.0, 255.0 / 224.0, 16)
        };
        let fixed = |value: f64| (value * 65536.0).round() as i32;
        Coefficients {
            luma: fixed(luma),
            luma_offset,
            red_v: fixed(2.0 * (1.0 - kr) * chroma),
            green_u: fixed(2.0 * (1.0 - kb) * kb / kg * chroma),
            green_v: fixed(2.0 * (1.0 - kr) * kr / kg * chroma),
            blue_u: fixed(2.0 * (1.0 - kb) * chroma),
        }
    }
}

/// One decoded 4:2:0 picture's planes.
pub struct Planes<'a> {
    pub width: usize,
    pub height: usize,
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub u_stride: usize,
    pub v_stride: usize,
}

/// Convert a 4:2:0 picture to RGBA8 (`width * height * 4` bytes in `out`).
pub fn yuv420_to_rgba(planes: &Planes<'_>, colour: Colour, out: &mut Vec<u8>) {
    let coefficients = Coefficients::new(colour);
    let (width, height) = (planes.width, planes.height);
    out.resize(width * height * 4, 0);
    let clamp = |value: i32| ((value + 32768) >> 16).clamp(0, 255) as u8;
    for (row, line) in out.chunks_exact_mut(width * 4).enumerate().take(height) {
        let y_row = &planes.y[row * planes.y_stride..][..width];
        let u_row = &planes.u[(row / 2) * planes.u_stride..];
        let v_row = &planes.v[(row / 2) * planes.v_stride..];
        for (column, pixel) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let luma = (i32::from(y_row[column]) - coefficients.luma_offset) * coefficients.luma;
            let u = i32::from(u_row[column / 2]) - 128;
            let v = i32::from(v_row[column / 2]) - 128;
            pixel[0] = clamp(luma + coefficients.red_v * v);
            pixel[1] = clamp(luma - coefficients.green_u * u - coefficients.green_v * v);
            pixel[2] = clamp(luma + coefficients.blue_u * u);
            pixel[3] = 255;
        }
    }
}

/// Box-filter an RGBA image into a `dst_width x dst_height` canvas,
/// letterboxed (black bars) to keep the source aspect.
pub fn downscale_letterboxed(
    src: &[u8],
    src_width: usize,
    src_height: usize,
    dst_width: usize,
    dst_height: usize,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.resize(dst_width * dst_height * 4, 0);
    for alpha in out.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    if src_width == 0 || src_height == 0 || src.len() < src_width * src_height * 4 {
        return;
    }
    // The fitted rectangle: the full width or the full height, whichever
    // the source aspect allows.
    let (fit_width, fit_height) = if src_width * dst_height >= src_height * dst_width {
        (dst_width, (src_height * dst_width / src_width).max(1))
    } else {
        ((src_width * dst_height / src_height).max(1), dst_height)
    };
    let left = (dst_width - fit_width) / 2;
    let top = (dst_height - fit_height) / 2;
    for row in 0..fit_height {
        let y0 = row * src_height / fit_height;
        let y1 = ((row + 1) * src_height / fit_height).max(y0 + 1);
        for column in 0..fit_width {
            let x0 = column * src_width / fit_width;
            let x1 = ((column + 1) * src_width / fit_width).max(x0 + 1);
            let mut sum = [0u32; 3];
            for y in y0..y1 {
                let line = &src[(y * src_width + x0) * 4..(y * src_width + x1) * 4];
                for pixel in line.as_chunks::<4>().0 {
                    sum[0] += u32::from(pixel[0]);
                    sum[1] += u32::from(pixel[1]);
                    sum[2] += u32::from(pixel[2]);
                }
            }
            let count = ((y1 - y0) * (x1 - x0)) as u32;
            let at = ((top + row) * dst_width + left + column) * 4;
            for channel in 0..3 {
                out[at + channel] = ((sum[channel] + count / 2) / count) as u8;
            }
        }
    }
}

/// A streaming stereo resampler to the mixer rate.
///
/// Downsampling (a 48 kHz AAC track to 22050 Hz) area-averages the source
/// frames each output frame covers, a box low-pass that keeps the fold-back
/// of the removed band small without a FIR; upsampling interpolates
/// linearly. Equal rates pass the samples through unchanged.
#[derive(Debug, Clone)]
pub struct StereoResampler {
    from: u64,
    to: u64,
    /// Buffered interleaved input from frame `input_start` on.
    input: Vec<f32>,
    input_start: u64,
    /// The next output frame index.
    next: u64,
}

impl StereoResampler {
    /// A resampler from `from` Hz to `to` Hz (both non-zero).
    pub fn new(from: u32, to: u32) -> Self {
        StereoResampler {
            from: u64::from(from.max(1)),
            to: u64::from(to.max(1)),
            input: Vec::new(),
            input_start: 0,
            next: 0,
        }
    }

    /// The input rate in Hz.
    pub fn input_rate(&self) -> u32 {
        self.from as u32
    }

    /// Feed interleaved stereo `samples` (-1.0..=1.0) and append every output
    /// frame they complete to `out` as interleaved s16.
    pub fn push(&mut self, samples: &[f32], out: &mut Vec<i16>) {
        self.input.extend_from_slice(samples);
        let available = self.input_start + (self.input.len() / 2) as u64;
        let to_i16 = |value: f32| (value * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
        loop {
            let frame = if self.from > self.to {
                let lo = self.next * self.from;
                let hi = lo + self.from;
                let first = lo / self.to;
                let last = hi.div_ceil(self.to);
                if last > available {
                    break;
                }
                let mut sum = [0f32; 2];
                for source in first..last {
                    let weight = (hi.min((source + 1) * self.to) - lo.max(source * self.to)) as f32;
                    let at = ((source - self.input_start) * 2) as usize;
                    sum[0] += self.input[at] * weight;
                    sum[1] += self.input[at + 1] * weight;
                }
                let scale = self.from as f32;
                [sum[0] / scale, sum[1] / scale]
            } else {
                let position = self.next * self.from;
                let source = position / self.to;
                let fraction = (position % self.to) as f32 / self.to as f32;
                let needed = if fraction > 0.0 {
                    source + 2
                } else {
                    source + 1
                };
                if needed > available {
                    break;
                }
                let at = ((source - self.input_start) * 2) as usize;
                if fraction > 0.0 {
                    let mix = |a: f32, b: f32| a + (b - a) * fraction;
                    [
                        mix(self.input[at], self.input[at + 2]),
                        mix(self.input[at + 1], self.input[at + 3]),
                    ]
                } else {
                    [self.input[at], self.input[at + 1]]
                }
            };
            out.push(to_i16(frame[0]));
            out.push(to_i16(frame[1]));
            self.next += 1;
        }
        // Keep only the source frames a later output frame can still reach.
        let keep = (self.next * self.from / self.to).clamp(self.input_start, available);
        let drop = ((keep - self.input_start) * 2) as usize;
        self.input.drain(..drop);
        self.input_start = keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_units_split_three_and_four_byte_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4,
        ];
        let units: Vec<&[u8]> = annex_b_units(&stream).collect();
        assert_eq!(units, vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4]]);
    }

    #[test]
    fn length_prefixed_samples_become_annex_b_with_sets_before_idr() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 2, 0x65, 9]);
        data.extend_from_slice(&[0, 0, 0, 2, 0x41, 8]);
        let media = Media {
            data,
            width: 16,
            height: 16,
            frame_rate: (24, 1),
            nal: NalFormat::LengthPrefixed {
                length: 4,
                parameter_sets: vec![vec![0x67, 1], vec![0x68, 2]],
            },
            video: vec![0..6, 6..12],
            audio: None,
        };
        let mut out = Vec::new();
        media.access_unit(0, &mut out).unwrap();
        assert_eq!(
            out,
            vec![
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 9
            ]
        );
        media.access_unit(1, &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x41, 8]);
        assert!(media.is_idr(0));
        assert!(!media.is_idr(1));
        assert_eq!(media.idr_at_or_before(1), 0);
    }

    #[test]
    fn validate_bounds_the_frame_rate() {
        let mut media = Media {
            data: vec![0, 0, 0, 1, 0x65],
            width: 16,
            height: 16,
            frame_rate: (24, 1),
            nal: NalFormat::AnnexB,
            video: std::iter::once(0..5).collect(),
            audio: None,
        };
        assert!(media.validate().is_ok());
        for rate in [(0, 1), (1, 0), (1, 3600), (241, 1), (u32::MAX, 1)] {
            media.frame_rate = rate;
            assert!(media.validate().is_err(), "{rate:?}");
        }
        media.frame_rate = (30_000, 1001);
        assert!(media.validate().is_ok());
    }

    #[test]
    fn a_corrupt_nal_length_is_an_error() {
        let media = Media {
            data: vec![0, 0, 0, 9, 0x65],
            width: 16,
            height: 16,
            frame_rate: (24, 1),
            nal: NalFormat::LengthPrefixed {
                length: 4,
                parameter_sets: Vec::new(),
            },
            video: std::iter::once(0..5).collect(),
            audio: None,
        };
        assert!(media.access_unit(0, &mut Vec::new()).is_err());
        assert!(!media.is_idr(0));
    }

    #[test]
    fn reads_the_vui_colour_of_the_hd_corpus() {
        // The High profile 1280x960 SPS of an HD mod's MP4 (BT.709, limited
        // range) and of its JPN AVI (no colour description).
        let mp4 = [
            0x67, 0x64, 0x00, 0x29, 0xAC, 0x2C, 0xA5, 0x01, 0x40, 0x1E, 0x6C, 0x05, 0xA8, 0x08,
            0x10, 0x0A, 0x00, 0x00, 0x07, 0xD0, 0x00, 0x01, 0x77, 0x01, 0x08,
        ];
        assert_eq!(
            sps_colour(&mp4),
            Some(Colour {
                matrix: Matrix::Bt709,
                full_range: false
            })
        );
        let avi = [
            0x67, 0x64, 0x00, 0x20, 0xAC, 0xD9, 0x40, 0x50, 0x07, 0x9A, 0x10, 0x00, 0x00, 0x03,
            0x00, 0x10, 0x00, 0x00, 0x03, 0x02, 0x80, 0xF1, 0x83, 0x19, 0x60,
        ];
        assert!(sps_colour(&avi).is_none_or(|colour| colour.matrix == Matrix::Bt601));
        assert_eq!(sps_colour(&[0x67]), None);
    }

    #[test]
    fn grey_and_primaries_convert_like_the_reference_matrices() {
        let colour = Colour {
            matrix: Matrix::Bt709,
            full_range: false,
        };
        let convert = |y: u8, u: u8, v: u8| {
            let (ys, us, vs) = ([y; 4], [u; 1], [v; 1]);
            let planes = Planes {
                width: 2,
                height: 2,
                y: &ys,
                u: &us,
                v: &vs,
                y_stride: 2,
                u_stride: 1,
                v_stride: 1,
            };
            let mut out = Vec::new();
            yuv420_to_rgba(&planes, colour, &mut out);
            [out[0], out[1], out[2], out[3]]
        };
        assert_eq!(convert(16, 128, 128), [0, 0, 0, 255]);
        assert_eq!(convert(235, 128, 128), [255, 255, 255, 255]);
        assert_eq!(convert(126, 128, 128), [128, 128, 128, 255]);
        // BT.709 limited-range pure red is (63, 102, 240).
        let red = convert(63, 102, 240);
        assert!(red[0] >= 253 && red[1] <= 2 && red[2] <= 2, "{red:?}");
    }

    #[test]
    fn downscale_averages_blocks_and_letterboxes_wide_sources() {
        // A 4x2 source onto a 2x2 canvas: the width fits, leaving one row
        // of bar below the picture.
        let mut src = Vec::new();
        for value in [0u8, 100, 200, 40, 0, 100, 200, 40] {
            src.extend_from_slice(&[value, value, value, 255]);
        }
        let mut out = Vec::new();
        downscale_letterboxed(&src, 4, 2, 2, 2, &mut out);
        assert_eq!(&out[0..8], &[50, 50, 50, 255, 120, 120, 120, 255]);
        assert_eq!(&out[8..16], &[0, 0, 0, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn resampler_passes_equal_rates_through() {
        let mut resampler = StereoResampler::new(22_050, 22_050);
        let mut out = Vec::new();
        resampler.push(&[0.5, -0.5, 0.25, -0.25], &mut out);
        resampler.push(&[1.0, -1.0], &mut out);
        assert_eq!(out, vec![16384, -16384, 8192, -8192, 32767, -32767]);
    }

    #[test]
    fn resampler_area_averages_a_halving() {
        let mut resampler = StereoResampler::new(44_100, 22_050);
        let mut out = Vec::new();
        // Split mid-pair: the second output frame needs the next push.
        resampler.push(&[0.0, 1.0, 0.5, 1.0, 1.0, 0.0], &mut out);
        assert_eq!(out, vec![8192, 32767]);
        resampler.push(&[0.0, 0.0], &mut out);
        assert_eq!(out, vec![8192, 32767, 16384, 0]);
    }

    #[test]
    fn resampler_48k_stream_keeps_the_rate_across_packets() {
        let mut resampler = StereoResampler::new(48_000, 22_050);
        let mut out = Vec::new();
        let packet = vec![0.25f32; 2048];
        for _ in 0..47 {
            resampler.push(&packet, &mut out);
        }
        // 47 packets of 1024 frames: 48128 source frames.
        let expected = 48_128 * 22_050 / 48_000;
        let produced = (out.len() / 2) as i64;
        assert!((produced - expected as i64).abs() <= 1, "{produced}");
        assert!(out.iter().all(|&sample| sample == 8192));
    }

    #[test]
    fn resampler_interpolates_an_upsampling() {
        let mut resampler = StereoResampler::new(11_025, 22_050);
        let mut out = Vec::new();
        resampler.push(&[0.0, 0.0, 0.5, -0.5], &mut out);
        assert_eq!(out, vec![0, 0, 8192, -8192, 16384, -16384]);
    }
}
