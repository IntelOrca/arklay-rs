//! A minimal ISO-BMFF (MP4) demuxer for HD film replacements.
//!
//! An HD mod ships its films as MP4 with one H.264 (`avc1`) video track and
//! an optional AAC (`mp4a`) audio track. Only the sample tables are read: the
//! `moov/trak/mdia` timescale and handler, and the `stbl` sample description,
//! timing, chunk and size boxes, which together give every sample's byte
//! range in the file. Fragmented files (`moof`), edit lists and other tracks
//! (a `tmcd` timecode, say) are ignored; the corpus starts both tracks at zero
//! and is constant frame rate.
//!
//! The result is a [`Media`] description shared with the H.264 AVI path, so
//! the film player does not care which container the file came in.

use std::ops::Range;

use anyhow::{Context, Result, bail};

use crate::budget;
use crate::media::{AacTrack, Media, NalFormat};

/// Whether `data` starts like an ISO-BMFF file (a leading `ftyp` box).
pub fn is_mp4(data: &[u8]) -> bool {
    data.get(4..8) == Some(b"ftyp")
}

/// Parse an MP4 image into its H.264 and AAC sample tables.
///
/// The file must carry one `avc1` video track; the first `mp4a` track, when
/// present, is the film's audio. The data is kept as the backing store.
pub fn parse(data: Vec<u8>) -> Result<Media> {
    let mut tracks = Vec::new();
    let moov = find_box(&data, 0..data.len(), b"moov")?.context("MP4 has no moov box")?;
    walk_children(&data, moov, |id, body| {
        if id == b"trak" {
            tracks.push(parse_track(&data, body)?);
        }
        Ok(())
    })?;

    let mut video = None;
    let mut audio = None;
    for track in tracks {
        match track.entry {
            SampleEntry::Avc {
                width,
                height,
                nal_length,
                ref parameter_sets,
            } if video.is_none() => {
                let delta = track
                    .typical_delta
                    .filter(|&delta| delta > 0)
                    .context("video track has no sample duration")?;
                if track.timescale == 0 {
                    bail!("video track has a zero timescale");
                }
                let (num, den) = reduce(track.timescale, delta);
                video = Some((
                    width,
                    height,
                    (num, den),
                    NalFormat::LengthPrefixed {
                        length: nal_length,
                        parameter_sets: parameter_sets.clone(),
                    },
                    track.samples,
                ));
            }
            SampleEntry::Aac {
                config,
                sample_rate,
            } if audio.is_none() => {
                audio = Some(AacTrack {
                    config,
                    sample_rate,
                    priming: (track.media_start, track.timescale.max(1)),
                    packets: track.samples,
                });
            }
            _ => {}
        }
    }
    let (width, height, frame_rate, nal, samples) =
        video.context("MP4 has no H.264 (avc1) video track")?;
    if samples.is_empty() {
        bail!("MP4 video track has no samples");
    }
    Ok(Media {
        data,
        width,
        height,
        frame_rate,
        nal,
        video: samples,
        audio,
    })
}

/// One parsed track: its media timescale, typical sample duration, sample
/// description and the byte range of every sample.
struct Track {
    timescale: u32,
    /// Where the edit list starts presenting the media, in `timescale`
    /// units: an AAC track's encoder priming.
    media_start: u64,
    typical_delta: Option<u32>,
    entry: SampleEntry,
    samples: Vec<Range<usize>>,
}

/// The sample descriptions the player understands.
enum SampleEntry {
    Avc {
        width: u32,
        height: u32,
        nal_length: u8,
        parameter_sets: Vec<Vec<u8>>,
    },
    Aac {
        config: Vec<u8>,
        sample_rate: u32,
    },
    Other,
}

/// Parse one `trak` body.
fn parse_track(data: &[u8], trak: Range<usize>) -> Result<Track> {
    let media_start = match find_box(data, trak.clone(), b"edts")? {
        Some(edts) => match find_box(data, edts, b"elst")? {
            Some(elst) => parse_elst(&data[elst])?,
            None => 0,
        },
        None => 0,
    };
    let mdia = find_box(data, trak, b"mdia")?.context("trak has no mdia")?;
    let mdhd = find_box(data, mdia.clone(), b"mdhd")?.context("mdia has no mdhd")?;
    let timescale = parse_mdhd(&data[mdhd])?;
    let hdlr = find_box(data, mdia.clone(), b"hdlr")?.context("mdia has no hdlr")?;
    let handler = data
        .get(hdlr.start + 8..hdlr.start + 12)
        .context("truncated hdlr")?;
    let minf = find_box(data, mdia, b"minf")?.context("mdia has no minf")?;
    let stbl = find_box(data, minf, b"stbl")?.context("minf has no stbl")?;

    let mut stsd = None;
    let mut stts = None;
    let mut stsc = None;
    let mut stsz = None;
    let mut stco = None;
    let mut co64 = None;
    walk_children(data, stbl, |id, body| {
        match id {
            b"stsd" => stsd = Some(body),
            b"stts" => stts = Some(body),
            b"stsc" => stsc = Some(body),
            b"stsz" => stsz = Some(body),
            b"stco" => stco = Some(body),
            b"co64" => co64 = Some(body),
            _ => {}
        }
        Ok(())
    })?;

    let entry = match handler {
        b"vide" | b"soun" => parse_stsd(data, stsd.context("stbl has no stsd")?)?,
        _ => SampleEntry::Other,
    };
    if matches!(entry, SampleEntry::Other) {
        return Ok(Track {
            timescale,
            media_start,
            typical_delta: None,
            entry,
            samples: Vec::new(),
        });
    }

    let sizes = parse_stsz(&data[stsz.context("stbl has no stsz")?])?;
    let chunks = match (stco, co64) {
        (Some(stco), _) => parse_chunk_offsets(&data[stco], false)?,
        (None, Some(co64)) => parse_chunk_offsets(&data[co64], true)?,
        (None, None) => bail!("stbl has neither stco nor co64"),
    };
    let runs = parse_stsc(&data[stsc.context("stbl has no stsc")?])?;
    let samples = sample_ranges(&sizes, &chunks, &runs, data.len())?;
    let typical_delta = match stts {
        Some(stts) => typical_delta(&data[stts])?,
        None => None,
    };
    Ok(Track {
        timescale,
        media_start,
        typical_delta,
        entry,
        samples,
    })
}

/// The media time an `elst` (version 0 or 1) starts presenting at: its first
/// non-empty edit's. Empty edits (a presentation delay) are ignored.
fn parse_elst(body: &[u8]) -> Result<u64> {
    let version = *body.first().context("empty elst")?;
    let count = read_u32(body, 4).context("truncated elst")?;
    let (entry_size, time_offset) = if version == 1 { (20, 8) } else { (12, 4) };
    for index in 0..count as usize {
        let entry = 8 + index * entry_size;
        let media_time = if version == 1 {
            read_u64(body, entry + time_offset).context("truncated elst")? as i64
        } else {
            i64::from(read_u32(body, entry + time_offset).context("truncated elst")? as i32)
        };
        if media_time >= 0 {
            return Ok(media_time as u64);
        }
    }
    Ok(0)
}

/// The `mdhd` timescale (version 0 or 1).
fn parse_mdhd(body: &[u8]) -> Result<u32> {
    let version = *body.first().context("empty mdhd")?;
    let offset = if version == 1 { 20 } else { 12 };
    read_u32(body, offset).context("truncated mdhd")
}

/// The first sample entry of an `stsd`.
fn parse_stsd(data: &[u8], stsd: Range<usize>) -> Result<SampleEntry> {
    let body = &data[stsd.clone()];
    let count = read_u32(body, 4).context("truncated stsd")?;
    if count == 0 {
        bail!("stsd has no sample entries");
    }
    let entries = stsd.start + 8..stsd.end;
    let mut found = None;
    walk_children(data, entries, |id, body| {
        if found.is_none() {
            found = Some((*id, body));
        }
        Ok(())
    })?;
    let (kind, body) = found.context("stsd has no sample entry box")?;
    match &kind {
        b"avc1" | b"avc3" => parse_avc1(data, body),
        b"mp4a" => parse_mp4a(data, body),
        _ => Ok(SampleEntry::Other),
    }
}

/// An `avc1` visual sample entry and its `avcC` configuration.
fn parse_avc1(data: &[u8], entry: Range<usize>) -> Result<SampleEntry> {
    // SampleEntry (8) + VisualSampleEntry fields up to the compressor name and
    // depth: 78 bytes before the child boxes.
    let body = &data[entry.clone()];
    let width = u32::from(read_u16(body, 24).context("truncated avc1")?);
    let height = u32::from(read_u16(body, 26).context("truncated avc1")?);
    if body.len() < 78 {
        bail!("avc1 entry is {} bytes, shorter than 78", body.len());
    }
    let avcc = find_box(data, entry.start + 78..entry.end, b"avcC")?
        .context("avc1 has no avcC configuration")?;
    let (nal_length, parameter_sets) = parse_avcc(&data[avcc])?;
    Ok(SampleEntry::Avc {
        width,
        height,
        nal_length,
        parameter_sets,
    })
}

/// The NAL length size and SPS/PPS units of an `avcC` record.
fn parse_avcc(body: &[u8]) -> Result<(u8, Vec<Vec<u8>>)> {
    if body.len() < 6 {
        bail!("avcC is {} bytes, shorter than its header", body.len());
    }
    let nal_length = (body[4] & 3) + 1;
    if nal_length == 3 {
        bail!("avcC declares an invalid 3-byte NAL length");
    }
    let mut sets = Vec::new();
    let mut pos = 5;
    let sps_count = usize::from(body[pos] & 0x1F);
    pos += 1;
    for _ in 0..sps_count {
        let (unit, next) = length_prefixed(body, pos).context("truncated avcC SPS")?;
        sets.push(unit.to_vec());
        pos = next;
    }
    let pps_count = usize::from(*body.get(pos).context("truncated avcC PPS count")?);
    pos += 1;
    for _ in 0..pps_count {
        let (unit, next) = length_prefixed(body, pos).context("truncated avcC PPS")?;
        sets.push(unit.to_vec());
        pos = next;
    }
    Ok((nal_length, sets))
}

/// A 16-bit length-prefixed unit at `pos`, and the position after it.
fn length_prefixed(body: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    let len = usize::from(read_u16(body, pos)?);
    let unit = body.get(pos + 2..pos + 2 + len)?;
    Some((unit, pos + 2 + len))
}

/// An `mp4a` audio sample entry and its `esds` AudioSpecificConfig.
fn parse_mp4a(data: &[u8], entry: Range<usize>) -> Result<SampleEntry> {
    let body = &data[entry.clone()];
    // SampleEntry (8), then the QuickTime sound description version: 0 has
    // 20 more bytes, 1 adds 16 and 2 adds 36. Version 2 moves the rate to a
    // float64 at 32.
    let version = read_u16(body, 8).context("truncated mp4a")?;
    let header = match version {
        0 => 28,
        1 => 44,
        2 => 64,
        _ => bail!("mp4a has unknown sound description version {version}"),
    };
    if body.len() < header {
        bail!("mp4a entry is {} bytes, shorter than {header}", body.len());
    }
    let sample_rate = match version {
        2 => f64::from_bits(read_u64(body, 32).context("truncated mp4a")?) as u32,
        _ => read_u32(body, 24).context("truncated mp4a")? >> 16,
    };
    let esds =
        find_box(data, entry.start + header..entry.end, b"esds")?.context("mp4a has no esds")?;
    Ok(match parse_esds(&data[esds])? {
        Some(config) => SampleEntry::Aac {
            config,
            sample_rate,
        },
        None => SampleEntry::Other,
    })
}

/// The decoder-specific info (the AAC AudioSpecificConfig) of an `esds`;
/// `None` for an `mp4a` track that is not AAC.
fn parse_esds(body: &[u8]) -> Result<Option<Vec<u8>>> {
    // Skip the full-box version/flags, then descend ES_Descriptor (3) ->
    // DecoderConfigDescriptor (4) -> DecoderSpecificInfo (5).
    let mut pos = 4;
    let (tag, len, start) = descriptor(body, pos).context("truncated esds")?;
    if tag != 3 {
        bail!("esds starts with descriptor tag {tag}, expected 3");
    }
    let end = (start + len).min(body.len());
    let flags = *body.get(start + 2).context("truncated ES_Descriptor")?;
    pos = start + 3;
    if flags & 0x80 != 0 {
        pos += 2;
    }
    if flags & 0x40 != 0 {
        pos += 1 + usize::from(*body.get(pos).context("truncated ES URL")?);
    }
    if flags & 0x20 != 0 {
        pos += 2;
    }
    while pos < end {
        let (tag, len, start) = descriptor(body, pos).context("truncated esds descriptor")?;
        if tag == 4 {
            let object_type = *body.get(start).context("truncated DecoderConfig")?;
            if object_type != 0x40 && object_type != 0x67 {
                return Ok(None);
            }
            // objectType, streamType, bufferSize (3), max and avg bitrate.
            let mut inner = start + 13;
            let inner_end = (start + len).min(end);
            while inner < inner_end {
                let (tag, len, start) =
                    descriptor(body, inner).context("truncated DecoderConfig child")?;
                if tag == 5 {
                    let config = body
                        .get(start..start + len)
                        .context("truncated DecoderSpecificInfo")?;
                    return Ok(Some(config.to_vec()));
                }
                inner = start + len;
            }
            bail!("esds has no AudioSpecificConfig");
        }
        pos = start + len;
    }
    bail!("esds has no DecoderConfigDescriptor")
}

/// One MPEG-4 descriptor header at `pos`: `(tag, length, body start)`.
fn descriptor(body: &[u8], pos: usize) -> Option<(u8, usize, usize)> {
    let tag = *body.get(pos)?;
    let mut len = 0usize;
    let mut cursor = pos + 1;
    for _ in 0..4 {
        let byte = *body.get(cursor)?;
        cursor += 1;
        len = (len << 7) | usize::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            return Some((tag, len, cursor));
        }
    }
    None
}

/// The `stsz` sample sizes.
fn parse_stsz(body: &[u8]) -> Result<Vec<u32>> {
    let fixed = read_u32(body, 4).context("truncated stsz")?;
    let count = read_u32(body, 8).context("truncated stsz")? as usize;
    if count > budget::MAX_AVI_FRAMES {
        bail!(
            "stsz declares {count} samples, over the {} limit",
            budget::MAX_AVI_FRAMES
        );
    }
    if fixed != 0 {
        return Ok(vec![fixed; count]);
    }
    (0..count)
        .map(|index| read_u32(body, 12 + index * 4).context("truncated stsz table"))
        .collect()
}

/// The `stco` (32-bit) or `co64` (64-bit) chunk offsets.
fn parse_chunk_offsets(body: &[u8], wide: bool) -> Result<Vec<u64>> {
    let count = read_u32(body, 4).context("truncated chunk offset box")? as usize;
    if count > budget::MAX_AVI_FRAMES {
        bail!("chunk offset box declares {count} chunks, over the limit");
    }
    (0..count)
        .map(|index| {
            if wide {
                read_u64(body, 8 + index * 8)
            } else {
                read_u32(body, 8 + index * 4).map(u64::from)
            }
            .context("truncated chunk offset table")
        })
        .collect()
}

/// The `stsc` runs: `(first chunk, samples per chunk)`, 1-based chunks.
fn parse_stsc(body: &[u8]) -> Result<Vec<(u32, u32)>> {
    let count = read_u32(body, 4).context("truncated stsc")? as usize;
    if count > budget::MAX_AVI_FRAMES {
        bail!("stsc declares {count} runs, over the limit");
    }
    (0..count)
        .map(|index| {
            let at = 8 + index * 12;
            Ok((
                read_u32(body, at).context("truncated stsc table")?,
                read_u32(body, at + 4).context("truncated stsc table")?,
            ))
        })
        .collect()
}

/// The most common `stts` sample duration, the constant-rate frame length.
fn typical_delta(body: &[u8]) -> Result<Option<u32>> {
    let count = read_u32(body, 4).context("truncated stts")? as usize;
    let mut best: Option<(u32, u32)> = None;
    for index in 0..count.min(budget::MAX_AVI_FRAMES) {
        let at = 8 + index * 8;
        let samples = read_u32(body, at).context("truncated stts table")?;
        let delta = read_u32(body, at + 4).context("truncated stts table")?;
        if best.is_none_or(|(most, _)| samples > most) {
            best = Some((samples, delta));
        }
    }
    Ok(best.map(|(_, delta)| delta))
}

/// Resolve every sample's byte range from the size, chunk and run tables.
fn sample_ranges(
    sizes: &[u32],
    chunks: &[u64],
    runs: &[(u32, u32)],
    file_len: usize,
) -> Result<Vec<Range<usize>>> {
    let mut samples = Vec::with_capacity(sizes.len());
    let mut next = 0usize;
    for (run, &(first, per_chunk)) in runs.iter().enumerate() {
        let first = first.max(1) as usize - 1;
        let last = runs.get(run + 1).map_or(chunks.len(), |&(next_first, _)| {
            (next_first.max(1) as usize - 1).min(chunks.len())
        });
        for &chunk_offset in chunks.get(first..last).unwrap_or(&[]) {
            let mut offset = usize::try_from(chunk_offset).context("chunk offset overflows")?;
            for _ in 0..per_chunk {
                let Some(&size) = sizes.get(next) else {
                    return Ok(samples);
                };
                let end = offset
                    .checked_add(size as usize)
                    .filter(|&end| end <= file_len)
                    .with_context(|| format!("sample {next} overruns the file"))?;
                samples.push(offset..end);
                offset = end;
                next += 1;
            }
        }
    }
    if samples.len() < sizes.len() {
        bail!(
            "chunk tables cover {} of {} samples",
            samples.len(),
            sizes.len()
        );
    }
    Ok(samples)
}

/// The body of the first `kind` box directly inside `range`.
fn find_box(data: &[u8], range: Range<usize>, kind: &[u8; 4]) -> Result<Option<Range<usize>>> {
    let mut found = None;
    walk_children(data, range, |id, body| {
        if found.is_none() && id == kind {
            found = Some(body);
        }
        Ok(())
    })?;
    Ok(found)
}

/// Visit the direct child boxes of `range`: `(type, body range)`.
fn walk_children(
    data: &[u8],
    range: Range<usize>,
    mut visit: impl FnMut(&[u8; 4], Range<usize>) -> Result<()>,
) -> Result<()> {
    let end = range.end.min(data.len());
    let mut pos = range.start;
    while pos + 8 <= end {
        let size = u64::from(read_u32(data, pos).context("truncated box")?);
        let id: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        let (header, size) = match size {
            0 => (8, (end - pos) as u64),
            1 => (16, read_u64(data, pos + 8).context("truncated large box")?),
            size => (8, size),
        };
        let size = usize::try_from(size).context("box size overflows")?;
        let box_end = pos
            .checked_add(size)
            .filter(|&box_end| box_end <= end && size >= header)
            .with_context(|| format!("box {:?} at 0x{pos:X} overruns its parent", fourcc(&id)))?;
        visit(&id, pos + header..box_end)?;
        pos = box_end;
    }
    Ok(())
}

/// Reduce `num/den` by their greatest common divisor.
pub(crate) fn reduce(num: u32, den: u32) -> (u32, u32) {
    let (mut a, mut b) = (num, den);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    let gcd = a.max(1);
    (num / gcd, den / gcd)
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

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// Build a synthetic MP4 for tests: one `avc1` track whose samples are the
/// given length-prefixed access units at `rate` (timescale, delta), and an
/// optional `mp4a` track with the given packets.
#[cfg(test)]
pub(crate) fn synthetic(
    rate: (u32, u32),
    parameter_sets: &[&[u8]],
    video: &[Vec<u8>],
    audio: Option<(&[u8], &[Vec<u8>])>,
) -> Vec<u8> {
    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 8);
        out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }
    fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut inner = vec![0u8; 4];
        inner.extend_from_slice(body);
        boxed(kind, &inner)
    }
    fn u32s(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }
    fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag, body.len() as u8];
        out.extend_from_slice(body);
        out
    }
    // One chunk per sample keeps the table construction trivial; the mdat
    // sits after the ftyp, before the moov, like a non-faststart file.
    let ftyp = boxed(b"ftyp", b"isom\0\0\x02\0isomavc1");
    let mut mdat_body = Vec::new();
    let mut video_offsets = Vec::new();
    for sample in video {
        video_offsets.push(mdat_body.len());
        mdat_body.extend_from_slice(sample);
    }
    let mut audio_offsets = Vec::new();
    if let Some((_, packets)) = audio {
        for packet in packets {
            audio_offsets.push(mdat_body.len());
            mdat_body.extend_from_slice(packet);
        }
    }
    let base = ftyp.len() + 8;

    let track = |handler: &[u8; 4],
                 timescale: u32,
                 delta: u32,
                 entry: Vec<u8>,
                 samples: &[Vec<u8>],
                 offsets: &[usize]| {
        let mut mdhd = u32s(&[0, 0, timescale, delta * samples.len() as u32]);
        mdhd.extend_from_slice(&[0u8; 4]);
        let mut hdlr = u32s(&[0]);
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0u8; 13]);
        let mut stsd = u32s(&[1]);
        stsd.extend_from_slice(&entry);
        let stts = u32s(&[1, samples.len() as u32, delta]);
        let stsc = u32s(&[1, 1, 1, 1]);
        let mut stsz = u32s(&[0, samples.len() as u32]);
        stsz.extend(u32s(
            &samples.iter().map(|s| s.len() as u32).collect::<Vec<_>>(),
        ));
        let mut stco = u32s(&[offsets.len() as u32]);
        stco.extend(u32s(
            &offsets
                .iter()
                .map(|offset| (base + offset) as u32)
                .collect::<Vec<_>>(),
        ));
        let stbl = [
            full(b"stsd", &stsd),
            full(b"stts", &stts),
            full(b"stsc", &stsc),
            full(b"stsz", &stsz),
            full(b"stco", &stco),
        ]
        .concat();
        let minf = boxed(b"stbl", &stbl);
        let mdia = [
            full(b"mdhd", &mdhd),
            full(b"hdlr", &hdlr),
            boxed(b"minf", &minf),
        ]
        .concat();
        // The audio track's edit list starts with an empty edit, then
        // skips 1024 frames of encoder priming.
        let edts = if handler == b"soun" {
            let elst = u32s(&[2, 0, u32::MAX, 0x0001_0000, 100, 1024, 0x0001_0000]);
            boxed(b"edts", &full(b"elst", &elst))
        } else {
            Vec::new()
        };
        boxed(b"trak", &[edts, boxed(b"mdia", &mdia)].concat())
    };

    let mut avcc = vec![1, 100, 0, 40, 0xFF, 0xE0];
    let (sps, pps): (Vec<&[u8]>, Vec<&[u8]>) = parameter_sets
        .iter()
        .partition(|unit| unit.first().is_some_and(|byte| byte & 0x1F == 7));
    avcc[5] |= sps.len() as u8;
    for unit in &sps {
        avcc.extend_from_slice(&(unit.len() as u16).to_be_bytes());
        avcc.extend_from_slice(unit);
    }
    avcc.push(pps.len() as u8);
    for unit in &pps {
        avcc.extend_from_slice(&(unit.len() as u16).to_be_bytes());
        avcc.extend_from_slice(unit);
    }
    let mut avc1 = vec![0u8; 6];
    avc1.extend_from_slice(&1u16.to_be_bytes());
    avc1.extend_from_slice(&[0u8; 16]);
    avc1.extend_from_slice(&1280u16.to_be_bytes());
    avc1.extend_from_slice(&960u16.to_be_bytes());
    avc1.extend_from_slice(&[0u8; 50]);
    avc1.extend(boxed(b"avcC", &avcc));
    let mut moov = track(
        b"vide",
        rate.0,
        rate.1,
        boxed(b"avc1", &avc1),
        video,
        &video_offsets,
    );

    if let Some((config, packets)) = audio {
        let mut mp4a = vec![0u8; 6];
        mp4a.extend_from_slice(&1u16.to_be_bytes());
        mp4a.extend_from_slice(&[0u8; 8]);
        mp4a.extend_from_slice(&2u16.to_be_bytes());
        mp4a.extend_from_slice(&16u16.to_be_bytes());
        mp4a.extend_from_slice(&[0u8; 4]);
        mp4a.extend_from_slice(&(48_000u32 << 16).to_be_bytes());
        let mut decoder_config = vec![0x40, 0x15, 0, 0, 0];
        decoder_config.extend_from_slice(&[0u8; 8]);
        decoder_config.extend(descriptor(5, config));
        let mut es = vec![0, 1, 0];
        es.extend(descriptor(4, &decoder_config));
        mp4a.extend(full(b"esds", &descriptor(3, &es)));
        moov.extend(track(
            b"soun",
            48_000,
            1024,
            boxed(b"mp4a", &mp4a),
            packets,
            &audio_offsets,
        ));
    }

    [ftyp, boxed(b"mdat", &mdat_body), boxed(b"moov", &moov)].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 100, 0, 40, 0xAC];
    const PPS: &[u8] = &[0x68, 0xEE, 0x3C, 0x80];

    fn sample(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
            out.extend_from_slice(unit);
        }
        out
    }

    #[test]
    fn parses_video_and_audio_sample_tables() {
        let video = vec![
            sample(&[&[0x65, 1, 2, 3]]),
            sample(&[&[0x41, 4, 5]]),
            sample(&[&[0x41, 6]]),
        ];
        let audio = vec![vec![0x21, 0x10], vec![0x21, 0x20, 0x30]];
        let file = synthetic(
            (24_000, 1_000),
            &[SPS, PPS],
            &video,
            Some((&[0x11, 0x90], &audio)),
        );
        assert!(is_mp4(&file));
        let media = parse(file).unwrap();
        assert_eq!((media.width, media.height), (1280, 960));
        assert_eq!(media.frame_rate, (24, 1));
        assert_eq!(media.video.len(), 3);
        assert_eq!(&media.data[media.video[1].clone()], video[1].as_slice());
        match &media.nal {
            NalFormat::LengthPrefixed {
                length,
                parameter_sets,
            } => {
                assert_eq!(*length, 4);
                assert_eq!(parameter_sets, &vec![SPS.to_vec(), PPS.to_vec()]);
            }
            NalFormat::AnnexB => panic!("MP4 is length-prefixed"),
        }
        let track = media.audio.as_ref().unwrap();
        assert_eq!(track.config, vec![0x11, 0x90]);
        assert_eq!(track.sample_rate, 48_000);
        assert_eq!(track.priming, (1024, 48_000));
        assert_eq!(track.packets.len(), 2);
        assert_eq!(&media.data[track.packets[1].clone()], audio[1].as_slice());
    }

    #[test]
    fn ntsc_rate_reduces_to_the_exact_ratio() {
        let video = vec![sample(&[&[0x65, 1]])];
        let media = parse(synthetic((30_000, 1_001), &[SPS, PPS], &video, None)).unwrap();
        assert_eq!(media.frame_rate, (30_000, 1_001));
        assert!(media.audio.is_none());
    }

    #[test]
    fn rejects_a_file_without_a_video_track() {
        let err = parse(b"\0\0\0\x10ftypisom\0\0\0\0".to_vec()).unwrap_err();
        assert!(format!("{err:#}").contains("moov"), "{err:#}");
    }

    #[test]
    fn truncated_tables_are_errors_not_panics() {
        let video = vec![sample(&[&[0x65, 1]]), sample(&[&[0x41, 2]])];
        let file = synthetic((24, 1), &[SPS, PPS], &video, None);
        for len in (0..file.len()).step_by(7) {
            let _ = parse(file[..len].to_vec());
        }
    }
}
