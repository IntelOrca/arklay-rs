//! AVI demuxer for the shipped Cinepak films.
//!
//! One RIFF/AVI image is parsed into a format description and a frame index.
//! The corpus is homogeneous: a `LIST hdrl` with one `vids`/`cvid` stream and
//! one `auds` PCM stream, a `LIST movi` whose `LIST rec ` wrappers hold the
//! `00dc` video and `01wb` audio chunks, and an `idx1` index. The index is
//! preferred when every entry validates against the chunk it names (the
//! shipped `rec ` container and `JUNK` entries included); the `movi` walk is
//! the fallback. Video and audio chunks are paired by their
//! ordinal position, which is the timestamp order: the corpus muxer writes a
//! fixed lead of audio chunks before the first video chunk, so a rec-local
//! pairing would drop the film's opening audio.
//!
//! Chunk bodies are never copied: the file bytes stay in one allocation and
//! [`Avi::video`]/[`Avi::audio`] hand out bounded slices of it.

use std::ops::Range;

use anyhow::{Context, Result, bail};

use crate::budget;

/// Microseconds per second, the unit of `avih`'s frame duration.
const MICROS_PER_SECOND: u32 = 1_000_000;

/// A parsed AVI file and its frame index.
#[derive(Debug)]
pub struct Avi {
    data: Vec<u8>,
    format: AviFormat,
    frames: Vec<AviFrame>,
}

/// The stream description of an AVI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AviFormat {
    /// Frame width in pixels.
    pub width: u16,
    /// Frame height in pixels.
    pub height: u16,
    /// Frames per second as an integer ratio (the corpus is 10/1 or 15/1).
    pub frame_rate: (u32, u32),
    /// Video codec fourcc, `b"cvid"` for the shipped corpus.
    pub video_codec: [u8; 4],
    /// The single PCM audio stream.
    pub audio: AudioFormat,
    /// The video frame count declared by `avih`.
    pub total_frames: u32,
}

/// A PCM audio stream description (a `WAVEFORMATEX`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    /// `wFormatTag`; 1 is PCM.
    pub format_tag: u16,
    /// Channel count.
    pub channels: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Average bytes per second.
    pub avg_bytes_per_sec: u32,
    /// Block alignment in bytes.
    pub block_align: u16,
    /// Bits per sample.
    pub bits_per_sample: u16,
}

/// One video frame and its paired audio chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AviFrame {
    /// `(offset, len)` of the video chunk body.
    pub video: (usize, usize),
    /// `(offset, len)` of the audio chunk body, or `None` when the audio track
    /// ends before the video.
    pub audio: Option<(usize, usize)>,
}

/// A validated reference to one chunk body inside the file.
#[derive(Debug, Clone, Copy)]
struct ChunkRef {
    offset: usize,
    len: usize,
}

impl Avi {
    /// Parse one RIFF/AVI image.
    ///
    /// The whole file is kept as the backing store; only the chunk index is
    /// built up front. A file that is not RIFF/AVI, has no `cvid` video
    /// stream, is not 320x240 or has a non-PCM audio stream is rejected with
    /// the offending field named.
    pub fn parse(bytes: Vec<u8>) -> Result<Self> {
        let data = bytes;
        if data.len() < 12 {
            bail!("AVI is {} bytes, shorter than a RIFF header", data.len());
        }
        if &data[0..4] != b"RIFF" {
            bail!("not a RIFF file: magic {:?}", fourcc(&data[0..4]));
        }
        if &data[8..12] != b"AVI " {
            bail!("RIFF type is {:?}, expected \"AVI \"", fourcc(&data[8..12]));
        }
        let declared = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        let riff_end = declared
            .checked_add(8)
            .map_or(data.len(), |end| end.min(data.len()));

        let mut avih_us = None;
        let mut avih_total = None;
        let mut video: Option<([u8; 4], u16, u16)> = None;
        let mut video_strh = None;
        let mut audio = None;
        let mut movi = None;
        let mut idx1 = None;

        walk_chunks(&data, 12, riff_end, |id, body, size| match id {
            b"LIST" => {
                let kind = data.get(body..body + 4).context("truncated LIST type")?;
                match kind {
                    b"hdrl" => walk_chunks(&data, body + 4, body + size, |id, body, size| {
                        match id {
                            b"avih" => {
                                let header = slice(&data, body, size).context("truncated avih")?;
                                if header.len() < 40 {
                                    bail!("avih is only {} bytes, shorter than 40", header.len());
                                }
                                avih_us = Some(read_u32(header, 0));
                                avih_total = Some(read_u32(header, 16));
                            }
                            b"LIST" => {
                                let kind =
                                    data.get(body..body + 4).context("truncated strl type")?;
                                if kind == b"strl" {
                                    let stream = parse_stream(&data, body + 4, body + size)?;
                                    match &stream.kind {
                                        b"vids" if video.is_none() => {
                                            let (codec, width, height) =
                                                parse_video_format(&data, &stream.strf)?;
                                            video = Some((codec, width, height));
                                            video_strh = Some(stream.strh);
                                        }
                                        b"auds" if audio.is_none() => {
                                            audio = Some(parse_audio_format(&data, &stream.strf)?);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                        Ok(())
                    }),
                    b"movi" => {
                        movi = Some(body + 4..body + size);
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            b"idx1" => {
                idx1 = Some(body..body + size);
                Ok(())
            }
            _ => Ok(()),
        })?;

        let (codec, width, height) = video.context("AVI has no video stream")?;
        if !codec.eq_ignore_ascii_case(b"cvid") {
            bail!(
                "unsupported video codec {:?}, expected \"cvid\"",
                fourcc(&codec)
            );
        }
        if width != 320 || height != 240 {
            bail!("unsupported video size {width}x{height}, expected 320x240");
        }
        let audio = audio.context("AVI has no audio stream")?;
        if audio.format_tag != 1 {
            bail!(
                "unsupported audio format tag {}, expected PCM (1)",
                audio.format_tag
            );
        }
        let strh_us = match video_strh {
            Some(strh) => strh_frame_duration(&data, strh)?,
            None => None,
        };
        let frame_rate = frame_rate(avih_us, strh_us)?;

        let (videos, audios) = match (idx1, movi) {
            (Some(index), Some(movi)) => {
                let movi_type = movi.start - 4;
                match index_entries(&data, index, movi_type, budget::MAX_AVI_FRAMES) {
                    Some(entries) => entries,
                    None => walk_movi(&data, movi, budget::MAX_AVI_FRAMES)?,
                }
            }
            (None, Some(movi)) => walk_movi(&data, movi, budget::MAX_AVI_FRAMES)?,
            (_, None) => bail!("AVI has no LIST movi chunk"),
        };
        if videos.is_empty() {
            bail!("AVI has no video chunks");
        }

        let total_frames = match avih_total {
            Some(total) if total > 0 => total,
            _ => videos.len() as u32,
        };
        let frames: Vec<AviFrame> = videos
            .iter()
            .enumerate()
            .map(|(index, chunk)| AviFrame {
                video: (chunk.offset, chunk.len),
                audio: audios.get(index).map(|chunk| (chunk.offset, chunk.len)),
            })
            .collect();
        Ok(Avi {
            data,
            format: AviFormat {
                width,
                height,
                frame_rate,
                video_codec: codec,
                audio,
                total_frames,
            },
            frames,
        })
    }

    /// The stream description.
    pub fn format(&self) -> &AviFormat {
        &self.format
    }

    /// The number of video frames in the index.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// The encoded bytes of one video frame.
    pub fn video(&self, index: usize) -> Option<&[u8]> {
        let (offset, len) = self.frames.get(index)?.video;
        self.data.get(offset..offset + len)
    }

    /// The PCM bytes of the audio chunk paired with one video frame. The
    /// corpus has three films whose audio track ends before the video; their
    /// tail frames yield `None`.
    pub fn audio(&self, index: usize) -> Option<&[u8]> {
        let (offset, len) = self.frames.get(index)?.audio?;
        self.data.get(offset..offset + len)
    }

    /// Audio samples one video frame spans: 2205 at 10 fps and 22050 Hz,
    /// 1470 at 15 fps. (`samples_before` is measured in this unit.)
    pub fn samples_per_frame(&self) -> usize {
        let (num, den) = self.format.frame_rate;
        if num == 0 {
            return 0;
        }
        (u64::from(self.format.audio.sample_rate) * u64::from(den) / u64::from(num)) as usize
    }

    /// Audio samples presented before `index` frames: the audio-led clock the
    /// playback session paces against.
    pub fn samples_before(&self, index: usize) -> u64 {
        (index as u64).saturating_mul(self.samples_per_frame() as u64)
    }
}

/// One `LIST strl` stream: the `strh` type and its `strf` format body.
struct RawStream {
    kind: [u8; 4],
    strh: Range<usize>,
    strf: Range<usize>,
}

/// Walk the direct children of a RIFF list body.
///
/// `handle` receives `(fourcc, body offset, body size)` for every chunk that
/// fits inside `start..end`; odd-sized chunks are followed by one pad byte.
/// An overrunning chunk is a hard error so a corrupt file is never read past.
fn walk_chunks(
    data: &[u8],
    start: usize,
    end: usize,
    mut handle: impl FnMut(&[u8; 4], usize, usize) -> Result<()>,
) -> Result<()> {
    let end = end.min(data.len());
    if start > end {
        bail!("chunk list starts at 0x{start:X} past its end 0x{end:X}");
    }
    let mut pos = start;
    while pos + 8 <= end {
        let id: [u8; 4] = data[pos..pos + 4].try_into().unwrap();
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        let chunk_end = body
            .checked_add(size)
            .filter(|&chunk_end| chunk_end <= end)
            .with_context(|| {
                format!(
                    "chunk {:?} at 0x{pos:X} overruns its container ending at 0x{end:X}",
                    fourcc(&id)
                )
            })?;
        handle(&id, body, size)?;
        pos = chunk_end + (size & 1);
    }
    Ok(())
}

/// Parse one `LIST strl` body (the nested chunks after the `strl` type).
fn parse_stream(data: &[u8], start: usize, end: usize) -> Result<RawStream> {
    let mut kind = [0u8; 4];
    let mut strh = None;
    let mut strf = None;
    walk_chunks(data, start, end, |id, body, size| {
        match id {
            b"strh" => {
                let header = slice(data, body, size).context("truncated strh")?;
                if header.len() < 4 {
                    bail!("strh is only {} bytes, shorter than a fourcc", header.len());
                }
                kind.copy_from_slice(&header[0..4]);
                strh = Some(body..body + size);
            }
            b"strf" => strf = Some(body..body + size),
            _ => {}
        }
        Ok(())
    })?;
    Ok(RawStream {
        kind,
        strh: strh.context("LIST strl has no strh")?,
        strf: strf.context("LIST strl has no strf")?,
    })
}

/// Read the `cvid` fourcc and size out of a video `strf` (a BITMAPINFOHEADER).
fn parse_video_format(data: &[u8], strf: &Range<usize>) -> Result<([u8; 4], u16, u16)> {
    let header = data
        .get(strf.clone())
        .context("video strf is out of bounds")?;
    if header.len() < 40 {
        bail!(
            "video strf is {} bytes, shorter than a BITMAPINFOHEADER",
            header.len()
        );
    }
    let width = i32::from_le_bytes(header[4..8].try_into().unwrap());
    let height = i32::from_le_bytes(header[8..12].try_into().unwrap());
    if !(1..=u16::MAX as i32).contains(&width) || !(1..=u16::MAX as i32).contains(&height) {
        bail!("video strf has invalid size {width}x{height}");
    }
    let mut codec = [0u8; 4];
    codec.copy_from_slice(&header[16..20]);
    Ok((codec, width as u16, height as u16))
}

/// Read a `WAVEFORMAT`/`WAVEFORMATEX` out of an audio `strf`.
fn parse_audio_format(data: &[u8], strf: &Range<usize>) -> Result<AudioFormat> {
    let bytes = data
        .get(strf.clone())
        .context("audio strf is out of bounds")?;
    if bytes.len() < 16 {
        bail!(
            "audio strf is {} bytes, shorter than a WAVEFORMAT",
            bytes.len()
        );
    }
    let format = AudioFormat {
        format_tag: read_u16(bytes, 0),
        channels: read_u16(bytes, 2),
        sample_rate: read_u32(bytes, 4),
        avg_bytes_per_sec: read_u32(bytes, 8),
        block_align: read_u16(bytes, 12),
        bits_per_sample: read_u16(bytes, 14),
    };
    if format.channels == 0 {
        bail!("audio strf has zero channels");
    }
    if format.sample_rate == 0 {
        bail!("audio strf has a zero sample rate");
    }
    Ok(format)
}

/// The `strh` frame duration in microseconds, when `avih` does not carry one.
///
/// The `dwScale` and `dwRate` fields sit at offsets 20 and 24 of the
/// `AVISTREAMHEADER`, so a truncated header is an error naming its size rather
/// than an out-of-bounds read.
fn strh_frame_duration(data: &[u8], strh: Range<usize>) -> Result<Option<u32>> {
    let header = data.get(strh).context("video strh is out of bounds")?;
    if header.len() < 28 {
        bail!(
            "video strh is only {} bytes, shorter than its 28-byte frame rate fields",
            header.len()
        );
    }
    let scale = read_u32(header, 20);
    let rate = read_u32(header, 24);
    if rate == 0 || scale == 0 {
        return Ok(None);
    }
    Ok(Some(
        (u64::from(scale) * u64::from(MICROS_PER_SECOND) / u64::from(rate)) as u32,
    ))
}

/// Frames per second, rounded to the nearest whole frame.
///
/// `avih`'s microseconds-per-frame is itself rounded (`66666` for the 15 fps
/// staff roll), so the corpus's exact integer rates are recovered by rounding;
/// the rate is stored as `(fps, 1)`.
fn frame_rate(avih_us: Option<u32>, strh_us: Option<u32>) -> Result<(u32, u32)> {
    let us = avih_us
        .filter(|&us| us > 0)
        .or(strh_us.filter(|&us| us > 0));
    let us = us.context("AVI declares neither avih nor strh frame duration")?;
    let fps = ((u64::from(MICROS_PER_SECOND) + u64::from(us) / 2) / u64::from(us)).max(1);
    if fps > u64::from(u32::MAX) {
        bail!("frame duration {us} us gives an impossible frame rate");
    }
    Ok((fps as u32, 1))
}

/// Build the frame index from `idx1`, or `None` when an entry does not point
/// at the chunk it names, in which case the caller falls back to `movi`.
///
/// Index offsets are relative to the `movi` fourcc (`movi_type`).
fn index_entries(
    data: &[u8],
    index: Range<usize>,
    movi_type: usize,
    max_frames: usize,
) -> Option<(Vec<ChunkRef>, Vec<ChunkRef>)> {
    let mut videos = Vec::new();
    let mut audios = Vec::new();
    let end = index.end.min(data.len());
    let mut pos = index.start;
    while pos + 16 <= end {
        let id: [u8; 4] = data.get(pos..pos + 4)?.try_into().ok()?;
        let offset = u32::from_le_bytes(data.get(pos + 8..pos + 12)?.try_into().ok()?) as usize;
        let len = u32::from_le_bytes(data.get(pos + 12..pos + 16)?.try_into().ok()?) as usize;
        let chunk = movi_type.checked_add(offset)?;
        let header = data.get(chunk..chunk + 8)?;
        // The shipped muxer indexes every `LIST rec ` wrapper under the
        // wrapper's type fourcc, so its chunk header reads `LIST`.
        let header_id: [u8; 4] = if id == *b"rec " { *b"LIST" } else { id };
        if header[0..4] != header_id {
            return None;
        }
        if u32::from_le_bytes(header[4..8].try_into().ok()?) as usize != len {
            return None;
        }
        data.get(chunk + 8..chunk + 8 + len)?;
        if videos.len() >= max_frames || audios.len() >= max_frames {
            return None;
        }
        match &id {
            b"00dc" | b"00db" => videos.push(ChunkRef {
                offset: chunk + 8,
                len,
            }),
            b"01wb" => audios.push(ChunkRef {
                offset: chunk + 8,
                len,
            }),
            b"rec " => {
                if data.get(chunk + 8..chunk + 12)? != b"rec " {
                    return None;
                }
            }
            b"JUNK" => {}
            _ => return None,
        }
        pos += 16;
    }
    if videos.is_empty() {
        return None;
    }
    Some((videos, audios))
}

/// Walk `LIST movi` recursively and collect the video and audio chunks in file
/// order. Used when there is no `idx1`, or when the index fails validation.
fn walk_movi(
    data: &[u8],
    movi: Range<usize>,
    max_frames: usize,
) -> Result<(Vec<ChunkRef>, Vec<ChunkRef>)> {
    let mut videos = Vec::new();
    let mut audios = Vec::new();
    collect_movi(data, movi, &mut videos, &mut audios, 0, max_frames)?;
    Ok((videos, audios))
}

/// Recursive `movi` walk; `rec` is one level, deeper lists are tolerated.
fn collect_movi(
    data: &[u8],
    range: Range<usize>,
    videos: &mut Vec<ChunkRef>,
    audios: &mut Vec<ChunkRef>,
    depth: usize,
    max_frames: usize,
) -> Result<()> {
    if depth > 4 {
        return Ok(());
    }
    let end = range.end.min(data.len());
    let mut pos = range.start;
    while pos + 8 <= end {
        let id: [u8; 4] = data[pos..pos + 4].try_into().unwrap();
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        let chunk_end = body
            .checked_add(size)
            .filter(|&chunk_end| chunk_end <= end)
            .with_context(|| format!("movi chunk {:?} overruns the list", fourcc(&id)))?;
        if videos.len() >= max_frames || audios.len() >= max_frames {
            bail!("AVI chunk index exceeds the {max_frames} frames per stream limit");
        }
        match &id {
            b"00dc" | b"00db" => videos.push(ChunkRef {
                offset: body,
                len: size,
            }),
            b"01wb" => audios.push(ChunkRef {
                offset: body,
                len: size,
            }),
            b"LIST" => collect_movi(
                data,
                body + 4..chunk_end,
                videos,
                audios,
                depth + 1,
                max_frames,
            )?,
            _ => {}
        }
        pos = chunk_end + (size & 1);
    }
    Ok(())
}

/// A human-readable fourcc for error messages.
fn fourcc(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&byte| {
            if byte.is_ascii_graphic() {
                char::from(byte)
            } else {
                '.'
            }
        })
        .collect()
}

/// A bounds-checked subslice.
fn slice(data: &[u8], offset: usize, len: usize) -> Option<&[u8]> {
    data.get(offset..offset.checked_add(len)?)
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AVIH_LEN: usize = 56;
    const STRH_LEN: usize = 56;

    /// One chunk: fourcc, LE size, body, pad byte when odd.
    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + body.len() + 1);
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    /// A LIST chunk whose body starts with its list type.
    fn list(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut inner = Vec::with_capacity(4 + body.len());
        inner.extend_from_slice(typ);
        inner.extend_from_slice(body);
        chunk(b"LIST", &inner)
    }

    /// A 56-byte `avih` with the given microseconds per frame and frame count.
    fn avih(us_per_frame: u32, total_frames: u32) -> Vec<u8> {
        let mut body = vec![0u8; AVIH_LEN];
        body[0..4].copy_from_slice(&us_per_frame.to_le_bytes());
        body[16..20].copy_from_slice(&total_frames.to_le_bytes());
        body[32..36].copy_from_slice(&320i32.to_le_bytes());
        body[36..40].copy_from_slice(&240i32.to_le_bytes());
        chunk(b"avih", &body)
    }

    /// A 56-byte `strh` with the given stream type and frame scale/rate.
    fn strh(kind: &[u8; 4], scale: u32, rate: u32, length: u32) -> Vec<u8> {
        let mut body = vec![0u8; STRH_LEN];
        body[0..4].copy_from_slice(kind);
        body[20..24].copy_from_slice(&scale.to_le_bytes());
        body[24..28].copy_from_slice(&rate.to_le_bytes());
        body[32..36].copy_from_slice(&length.to_le_bytes());
        chunk(b"strh", &body)
    }

    /// A 40-byte BITMAPINFOHEADER video `strf`.
    fn video_strf(codec: &[u8; 4], width: i32, height: i32) -> Vec<u8> {
        let mut body = vec![0u8; 40];
        body[0..4].copy_from_slice(&40u32.to_le_bytes());
        body[4..8].copy_from_slice(&width.to_le_bytes());
        body[8..12].copy_from_slice(&height.to_le_bytes());
        body[12..14].copy_from_slice(&1u16.to_le_bytes());
        body[14..16].copy_from_slice(&24u16.to_le_bytes());
        body[16..20].copy_from_slice(codec);
        chunk(b"strf", &body)
    }

    /// A 16-byte PCM `strf`.
    fn audio_strf(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let mut body = vec![0u8; 16];
        body[0..2].copy_from_slice(&tag.to_le_bytes());
        body[2..4].copy_from_slice(&channels.to_le_bytes());
        body[4..8].copy_from_slice(&rate.to_le_bytes());
        body[8..12]
            .copy_from_slice(&(rate * u32::from(channels) * u32::from(bits) / 8).to_le_bytes());
        body[12..14].copy_from_slice(&(channels * bits / 8).to_le_bytes());
        body[14..16].copy_from_slice(&bits.to_le_bytes());
        chunk(b"strf", &body)
    }

    /// A `LIST hdrl` with one video and one audio `LIST strl`.
    #[allow(clippy::too_many_arguments)]
    fn hdrl(
        us_per_frame: u32,
        total_frames: u32,
        codec: &[u8; 4],
        width: i32,
        height: i32,
        tag: u16,
        channels: u16,
        rate: u32,
        bits: u16,
    ) -> Vec<u8> {
        let mut body = avih(us_per_frame, total_frames);
        let video = [
            strh(b"vids", 100_000, 1_000_000, total_frames),
            video_strf(codec, width, height),
        ]
        .concat();
        body.extend_from_slice(&list(b"strl", &video));
        let audio = [
            strh(b"auds", 4, 88_200, total_frames),
            audio_strf(tag, channels, rate, bits),
        ]
        .concat();
        body.extend_from_slice(&list(b"strl", &audio));
        list(b"hdrl", &body)
    }

    /// A `LIST hdrl` with a caller-supplied video `strh` chunk.
    fn hdrl_with_video_strh(us_per_frame: u32, total_frames: u32, video_strh: Vec<u8>) -> Vec<u8> {
        let mut body = avih(us_per_frame, total_frames);
        let video = [video_strh, video_strf(b"cvid", 320, 240)].concat();
        body.extend_from_slice(&list(b"strl", &video));
        let audio = [
            strh(b"auds", 4, 88_200, total_frames),
            audio_strf(1, 2, 22_050, 16),
        ]
        .concat();
        body.extend_from_slice(&list(b"strl", &audio));
        list(b"hdrl", &body)
    }

    /// A built file plus the idx1 `(fourcc, offset, len)` of every chunk.
    struct Fixture {
        hdrl: Vec<u8>,
        movi_body: Vec<u8>,
        index: Vec<([u8; 4], usize, usize)>,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                hdrl: hdrl(100_000, 0, b"cvid", 320, 240, 1, 2, 22_050, 16),
                movi_body: Vec::new(),
                index: Vec::new(),
            }
        }

        fn with_hdrl(mut self, hdrl: Vec<u8>) -> Self {
            self.hdrl = hdrl;
            self
        }

        /// Append one chunk to the movi body and record its index entry. The
        /// index offset is relative to the `movi` fourcc: 4 for the first
        /// chunk header, plus where it sits in the body.
        fn push(&mut self, id: &[u8; 4], body: &[u8]) {
            let start = self.movi_body.len();
            self.movi_body.extend_from_slice(&chunk(id, body));
            self.index.push((*id, start + 4, body.len()));
        }

        /// Append a `LIST rec ` whose children were pushed as `(fourcc, body)`,
        /// recording the wrapper entry the shipped index carries too.
        fn push_rec(&mut self, children: &[(&[u8; 4], Vec<u8>)]) {
            let rec_start = self.movi_body.len();
            let mut inner = Vec::new();
            let mut child_entries = Vec::new();
            for (id, body) in children {
                let start = inner.len();
                inner.extend_from_slice(&chunk(id, body));
                // rec header 8 + type 4, then the child header.
                child_entries.push((**id, rec_start + 12 + start + 4, body.len()));
            }
            self.movi_body.extend_from_slice(&list(b"rec ", &inner));
            self.index.push((*b"rec ", rec_start + 4, 4 + inner.len()));
            self.index.extend(child_entries);
        }

        /// The `idx1` body for an explicit chunk order.
        fn index_body(&self, order: &[[u8; 4]]) -> Vec<u8> {
            let mut available = self.index.clone();
            let mut body = Vec::new();
            for id in order {
                let at = available
                    .iter()
                    .position(|entry| entry.0 == *id)
                    .expect("indexed chunk exists");
                let (_, offset, len) = available.remove(at);
                body.extend_from_slice(id);
                body.extend_from_slice(&0u32.to_le_bytes());
                body.extend_from_slice(&(offset as u32).to_le_bytes());
                body.extend_from_slice(&(len as u32).to_le_bytes());
            }
            body
        }

        /// The `idx1` body listing every recorded entry in order, including
        /// the `LIST rec ` wrappers the shipped muxer indexes.
        fn rec_index_body(&self) -> Vec<u8> {
            let mut body = Vec::new();
            for (id, offset, len) in &self.index {
                body.extend_from_slice(id);
                body.extend_from_slice(&0u32.to_le_bytes());
                body.extend_from_slice(&(*offset as u32).to_le_bytes());
                body.extend_from_slice(&(*len as u32).to_le_bytes());
            }
            body
        }

        /// The file bytes, with an `idx1` in the given chunk order when asked.
        fn build(&self, index_order: Option<&[[u8; 4]]>) -> Vec<u8> {
            let index = index_order.map(|order| self.index_body(order));
            self.assemble(index.as_deref())
        }

        /// The file bytes with a real `rec `-carrying `idx1`.
        fn build_rec_index(&self) -> Vec<u8> {
            self.assemble(Some(&self.rec_index_body()))
        }

        fn assemble(&self, index: Option<&[u8]>) -> Vec<u8> {
            let mut top = Vec::new();
            top.extend_from_slice(&self.hdrl);
            top.extend_from_slice(&list(b"movi", &self.movi_body));
            if let Some(body) = index {
                top.extend_from_slice(&chunk(b"idx1", body));
            }
            // Top-level odd-sized JUNK and an unknown chunk, both padded.
            top.extend_from_slice(&chunk(b"JUNK", &[1, 2, 3]));
            top.extend_from_slice(&chunk(b"XYZW", &[9, 9, 9, 9, 9]));
            let mut file = Vec::with_capacity(12 + top.len());
            file.extend_from_slice(b"RIFF");
            file.extend_from_slice(&((4 + top.len()) as u32).to_le_bytes());
            file.extend_from_slice(b"AVI ");
            file.extend_from_slice(&top);
            file
        }
    }

    /// Two audio chunks before three interleaved video frames with
    /// identifiable bytes; the second audio chunk sits by the first video.
    fn interleaved() -> Fixture {
        let mut fixture = Fixture::new();
        let (v0, v1, v2) = (vec![0x10], vec![0x11, 0x12], vec![0x13, 0x14, 0x15]);
        let (a0, a1) = (vec![0xA0, 0xA1], vec![0xA2, 0xA3]);
        fixture.push_rec(&[(b"01wb", a0)]);
        fixture.push_rec(&[(b"01wb", a1), (b"00dc", v0)]);
        fixture.push(b"JUNK", &[7, 7, 7]);
        fixture.push_rec(&[(b"00dc", v1)]);
        fixture.push_rec(&[(b"00db", v2)]);
        fixture
    }

    #[test]
    fn parses_interleaved_recs_with_junk_and_odd_padding() {
        let avi = Avi::parse(interleaved().build(None)).unwrap();
        assert_eq!(avi.frame_count(), 3);
        assert_eq!(avi.format().width, 320);
        assert_eq!(avi.format().height, 240);
        assert_eq!(avi.format().video_codec, *b"cvid");
        assert_eq!(avi.format().frame_rate, (10, 1));
        assert_eq!(avi.format().total_frames, 3);
        assert_eq!(avi.samples_per_frame(), 2205);
        assert_eq!(avi.samples_before(0), 0);
        assert_eq!(avi.samples_before(2), 4410);
        assert_eq!(avi.video(0), Some([0x10].as_slice()));
        assert_eq!(avi.video(1), Some([0x11, 0x12].as_slice()));
        assert_eq!(avi.video(2), Some([0x13, 0x14, 0x15].as_slice()));
        // The audio lead is paired by ordinal position, not by rec.
        assert_eq!(avi.audio(0), Some([0xA0, 0xA1].as_slice()));
        assert_eq!(avi.audio(1), Some([0xA2, 0xA3].as_slice()));
        assert_eq!(avi.audio(2), None);
        assert_eq!(avi.video(3), None);
        assert_eq!(avi.audio(3), None);
    }

    #[test]
    fn idx1_offsets_and_order_drive_the_index() {
        let fixture = interleaved();
        let file = fixture.build(Some(&[*b"00db", *b"00dc", *b"00dc", *b"01wb"]));
        let avi = Avi::parse(file).unwrap();
        assert_eq!(avi.frame_count(), 3);
        assert_eq!(avi.video(0), Some([0x13, 0x14, 0x15].as_slice()));
        assert_eq!(avi.video(1), Some([0x10].as_slice()));
        assert_eq!(avi.video(2), Some([0x11, 0x12].as_slice()));
        assert_eq!(avi.audio(0), Some([0xA0, 0xA1].as_slice()));
    }

    #[test]
    fn rec_index_entries_are_validated_and_skipped() {
        // The real corpus indexes each `LIST rec ` wrapper under `rec `; the
        // index must be accepted and its container/JUNK entries skipped.
        let avi = Avi::parse(interleaved().build_rec_index()).unwrap();
        assert_eq!(avi.frame_count(), 3);
        assert_eq!(avi.video(0), Some([0x10].as_slice()));
        assert_eq!(avi.video(1), Some([0x11, 0x12].as_slice()));
        assert_eq!(avi.video(2), Some([0x13, 0x14, 0x15].as_slice()));
        assert_eq!(avi.audio(0), Some([0xA0, 0xA1].as_slice()));
        assert_eq!(avi.audio(1), Some([0xA2, 0xA3].as_slice()));
        assert_eq!(avi.audio(2), None);
    }

    #[test]
    fn a_misdescribed_rec_index_falls_back_to_the_movi_walk() {
        let mut file = interleaved().build_rec_index();
        // Break the movi's first rec wrapper list type: the index entry then
        // fails validation and the movi walk supplies the frames.
        let movi = file
            .windows(4)
            .position(|window| window == b"movi")
            .unwrap();
        let list_type = file[movi..]
            .windows(4)
            .position(|window| window == b"rec ")
            .unwrap()
            + movi;
        file[list_type..list_type + 4].copy_from_slice(b"junk");
        let avi = Avi::parse(file).unwrap();
        assert_eq!(avi.frame_count(), 3);
        assert_eq!(avi.video(0), Some([0x10].as_slice()));
        assert_eq!(avi.audio(0), Some([0xA0, 0xA1].as_slice()));
    }

    #[test]
    fn truncated_strh_is_an_error_not_a_panic() {
        let mut short_strh = vec![0u8; 24];
        short_strh[0..4].copy_from_slice(b"vids");
        short_strh[20..24].copy_from_slice(&1000u32.to_le_bytes());
        let hdrl = hdrl_with_video_strh(100_000, 1, chunk(b"strh", &short_strh));
        let mut fixture = Fixture::new().with_hdrl(hdrl);
        fixture.push(b"00dc", &[1]);
        let err = Avi::parse(fixture.build(None)).unwrap_err().to_string();
        assert!(err.contains("strh"), "{err}");
        assert!(err.contains("24"), "{err}");
    }

    #[test]
    fn invalid_idx1_falls_back_to_the_movi_walk() {
        let mut file = interleaved().build(Some(&[*b"00db", *b"00dc", *b"00dc", *b"01wb"]));
        let idx = file
            .windows(4)
            .position(|window| window == b"idx1")
            .unwrap();
        // The first entry's length at idx1 + 8 + 12 no longer matches its chunk.
        file[idx + 20..idx + 24].copy_from_slice(&99u32.to_le_bytes());
        let avi = Avi::parse(file).unwrap();
        assert_eq!(avi.video(0), Some([0x10].as_slice()));
        assert_eq!(avi.video(2), Some([0x13, 0x14, 0x15].as_slice()));
    }

    #[test]
    fn empty_idx1_falls_back_to_the_movi_walk() {
        let avi = Avi::parse(interleaved().build(Some(&[]))).unwrap();
        assert_eq!(avi.frame_count(), 3);
        assert_eq!(avi.video(0), Some([0x10].as_slice()));
        assert_eq!(avi.audio(0), Some([0xA0, 0xA1].as_slice()));
    }

    #[test]
    fn avih_total_frames_is_the_format_description() {
        let mut fixture =
            Fixture::new().with_hdrl(hdrl(100_000, 7, b"cvid", 320, 240, 1, 2, 22_050, 16));
        fixture.push(b"00dc", &[1]);
        let avi = Avi::parse(fixture.build(None)).unwrap();
        assert_eq!(avi.format().total_frames, 7);
        assert_eq!(avi.frame_count(), 1);
    }

    #[test]
    fn frame_rate_rounds_the_15_fps_staff_roll() {
        let mut fixture =
            Fixture::new().with_hdrl(hdrl(66_666, 1, b"cvid", 320, 240, 1, 2, 22_050, 16));
        fixture.push(b"00dc", &[1]);
        let avi = Avi::parse(fixture.build(None)).unwrap();
        assert_eq!(avi.format().frame_rate, (15, 1));
        assert_eq!(avi.samples_per_frame(), 1470);
    }

    #[test]
    fn short_audio_tail_yields_none() {
        let mut fixture = Fixture::new();
        for frame in 0..3u8 {
            fixture.push(b"00dc", &[frame]);
        }
        fixture.push(b"01wb", &[0xAA, 0xBB]);
        let avi = Avi::parse(fixture.build(None)).unwrap();
        assert_eq!(avi.audio(0), Some([0xAA, 0xBB].as_slice()));
        assert_eq!(avi.audio(1), None);
        assert_eq!(avi.audio(2), None);
    }

    #[test]
    fn rejects_inputs_with_the_offending_field() {
        let err = Avi::parse(vec![0u8; 4]).unwrap_err().to_string();
        assert!(err.contains("shorter"), "{err}");

        let mut wave = Fixture::new().build(None);
        wave[8..12].copy_from_slice(b"WAVE");
        let err = Avi::parse(wave).unwrap_err().to_string();
        assert!(err.contains("RIFF type"), "{err}");
        assert!(err.contains("WAVE"), "{err}");

        let mut bad_codec =
            Fixture::new().with_hdrl(hdrl(100_000, 1, b"mjpg", 320, 240, 1, 2, 22_050, 16));
        bad_codec.push(b"00dc", &[1]);
        let err = Avi::parse(bad_codec.build(None)).unwrap_err().to_string();
        assert!(err.contains("cvid"), "{err}");
        assert!(err.contains("mjpg"), "{err}");

        let mut bad_size =
            Fixture::new().with_hdrl(hdrl(100_000, 1, b"cvid", 640, 480, 1, 2, 22_050, 16));
        bad_size.push(b"00dc", &[1]);
        let err = Avi::parse(bad_size.build(None)).unwrap_err().to_string();
        assert!(err.contains("640x480"), "{err}");

        let mut bad_audio =
            Fixture::new().with_hdrl(hdrl(100_000, 1, b"cvid", 320, 240, 0x50, 2, 22_050, 16));
        bad_audio.push(b"00dc", &[1]);
        let err = Avi::parse(bad_audio.build(None)).unwrap_err().to_string();
        assert!(err.contains("audio format tag"), "{err}");
        assert!(err.contains("80"), "{err}");
    }

    #[test]
    fn rejects_a_zero_frame_duration_instead_of_dividing_by_zero() {
        // A `strh` whose rate is far above its scale yields a zero
        // microseconds-per-frame; the frame rate must reject it rather than
        // divide by zero. Found by the avi fuzz target.
        let hdrl = hdrl_with_video_strh(0, 1, strh(b"vids", 1, 2_000_000, 1));
        let mut fixture = Fixture::new().with_hdrl(hdrl);
        fixture.push(b"00dc", &[1]);
        let err = Avi::parse(fixture.build(None)).unwrap_err().to_string();
        assert!(err.contains("frame duration"), "{err}");
    }

    #[test]
    fn rejects_streams_over_the_frame_cap() {
        let file = interleaved().build_rec_index();
        let movi = file.windows(4).position(|w| w == b"movi").unwrap();
        let movi_size = u32::from_le_bytes(file[movi - 4..movi].try_into().unwrap()) as usize;
        let movi_range = movi + 4..movi + movi_size;
        let idx = file.windows(4).position(|w| w == b"idx1").unwrap();
        let idx_size = u32::from_le_bytes(file[idx + 4..idx + 8].try_into().unwrap()) as usize;
        let idx_range = idx + 8..idx + 8 + idx_size;

        assert!(index_entries(&file, idx_range, movi, 2).is_none());
        let err = walk_movi(&file, movi_range, 2).unwrap_err().to_string();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn rejects_a_file_without_a_movi_list() {
        let mut bytes = Fixture::new().build(None);
        let movi = bytes
            .windows(4)
            .position(|window| window == b"movi")
            .unwrap();
        // Turn the LIST fourcc into an unknown chunk, hiding movi from the walk.
        bytes[movi - 8..movi - 4].copy_from_slice(b"JUNK");
        let err = Avi::parse(bytes).unwrap_err().to_string();
        assert!(err.contains("movi"), "{err}");
    }

    #[test]
    fn a_corrupt_chunk_overrun_is_an_error() {
        let mut bytes = Fixture::new().build(None);
        let avih = bytes.windows(4).position(|w| w == b"avih").unwrap();
        // Declare a chunk far larger than the file.
        bytes[avih + 4..avih + 8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let err = Avi::parse(bytes).unwrap_err().to_string();
        assert!(err.contains("overruns"), "{err}");
    }
}
