//! RE1 EMD (model archive) and EMW (no-weapon animation) parsers.
//!
//! The EMD container ends with a 20-byte directory of five u32 chunk offsets:
//! `[1]` the EMR armature/animation header, `[2]` the EDD animation table,
//! `[3]` the TMD mesh and `[4]` the embedded TIM texture. The EMW container
//! ends with an 8-byte directory of two u32 offsets: `[0]` the EDD table and
//! `[1]` the TMD mesh.

use anyhow::{Context, Result, bail};

use crate::budget;
use crate::model::{Clip, ClipFrame, Emd, Emw, Keyframe, Skeleton};
use crate::{tim, tmd};

const EMD_DIRECTORY_LEN: usize = 20;
const EMW_DIRECTORY_LEN: usize = 8;
const JOINT_POSITION_LEN: usize = 6;
const ARMATURE_ENTRY_LEN: usize = 4;
const KEYFRAME_HEADER_LEN: usize = 12;
const ANGLE_LEN: usize = 6;
const EDD_ENTRY_LEN: usize = 4;
const EDD_FRAME_LEN: usize = 4;
const KEYFRAME_INDEX_MASK: u16 = 0x0FFF;

/// Parse an EMD player/enemy model.
pub fn parse(data: &[u8]) -> Result<Emd> {
    if data.len() < EMD_DIRECTORY_LEN {
        bail!(
            "EMD is truncated: {} bytes, need at least {EMD_DIRECTORY_LEN} for the directory",
            data.len()
        );
    }
    let directory_start = data.len() - EMD_DIRECTORY_LEN;
    let mut directory = [0usize; 5];
    for (slot, value) in directory.iter_mut().enumerate() {
        *value = read_u32(data, directory_start + slot * 4)? as usize;
    }
    check_order(&directory, directory_start, "EMD")?;
    let emr_offset = directory[1];
    let edd_offset = directory[2];
    let tmd_offset = directory[3];
    let tim_offset = directory[4];

    let skeleton = parse_skeleton(data, emr_offset, edd_offset)?;
    let keyframes = parse_keyframes(data, emr_offset, edd_offset)?;
    let clips = parse_clips(data, edd_offset, tmd_offset)?;
    let mesh = tmd::parse(
        data.get(tmd_offset..)
            .context("EMD mesh offset is out of range")?,
    )?;
    let texture = tim::decode_8bpp(
        data.get(tim_offset..)
            .context("EMD texture offset is out of range")?,
    )?;

    Ok(Emd {
        skeleton,
        keyframes,
        clips,
        mesh,
        texture,
    })
}

/// Parse an EMW no-weapon animation + mesh archive.
pub fn parse_emw(data: &[u8]) -> Result<Emw> {
    if data.len() < EMW_DIRECTORY_LEN {
        bail!(
            "EMW is truncated: {} bytes, need at least {EMW_DIRECTORY_LEN} for the directory",
            data.len()
        );
    }
    let directory_start = data.len() - EMW_DIRECTORY_LEN;
    let edd_offset = read_u32(data, directory_start)? as usize;
    let mesh_offset = read_u32(data, directory_start + 4)? as usize;
    if edd_offset > mesh_offset || mesh_offset > directory_start {
        bail!(
            "EMW directory offsets are out of order: EDD 0x{edd_offset:X}, mesh 0x{mesh_offset:X}"
        );
    }

    let clips = parse_clips(data, edd_offset, mesh_offset)?;
    let skeleton = parse_skeleton(data, 0, edd_offset)?;
    let keyframes = parse_keyframes(data, 0, edd_offset)?;
    let mesh = tmd::parse(
        data.get(mesh_offset..)
            .context("EMW mesh offset is out of range")?,
    )?;

    Ok(Emw {
        skeleton,
        keyframes,
        clips,
        mesh,
    })
}

/// Parse the RDT-embedded room player-animation pair.
///
/// `header` is RDT pointer slot 9 (the EMR armature/keyframe header) and `base`
/// slot 10 (the EDD clip table); `bound` is the next section's pointer, which
/// limits the clip table exactly the way the EMD directory does. The parser is
/// the same skeleton/keyframe/clip code the EMD/EMW paths use, so a room whose
/// clips are empty (most rooms) parses to an empty clip list and a caller can
/// fall back to the no-op stance.
pub fn parse_room_anim(
    data: &[u8],
    header: u32,
    base: u32,
    bound: u32,
) -> Result<crate::model::RoomAnim> {
    let header = header as usize;
    let base = base as usize;
    let bound = bound as usize;
    if header == 0 || base == 0 {
        bail!("room animation pair has a null pointer");
    }
    if header > base || base > bound || bound > data.len() {
        bail!(
            "room animation offsets are out of order: header 0x{header:X}, base 0x{base:X}, bound 0x{bound:X}"
        );
    }

    let skeleton = parse_skeleton(data, header, base)?;
    let keyframes = parse_keyframes(data, header, base)?;
    let clips = parse_clips(data, base, bound)?;
    Ok(crate::model::RoomAnim {
        skeleton,
        keyframes,
        clips,
    })
}

fn check_order(directory: &[usize], directory_start: usize, kind: &str) -> Result<()> {
    let mut previous = 0;
    for offset in directory.iter().chain(std::iter::once(&directory_start)) {
        if *offset < previous {
            bail!("{kind} directory offsets are out of order: {directory:X?}");
        }
        previous = *offset;
    }
    Ok(())
}

fn parse_skeleton(data: &[u8], emr_offset: usize, emr_end: usize) -> Result<Skeleton> {
    let armature_offset = read_u16(data, emr_offset)? as usize;
    let joint_count = read_u16(data, emr_offset + 4)? as usize;
    budget::check_len(joint_count, budget::MAX_EMD_JOINTS, "EMR joint count")?;

    let position_bytes = chunk_slice(
        data,
        emr_offset + 8,
        joint_count * JOINT_POSITION_LEN,
        emr_end,
        "EMR joint positions",
    )?;
    let relative = position_bytes
        .as_chunks::<JOINT_POSITION_LEN>()
        .0
        .iter()
        .map(|p| vec3(p))
        .collect();

    let armature_base = emr_offset
        .checked_add(armature_offset)
        .context("EMR armature offset overflows")?;
    let entries = chunk_slice(
        data,
        armature_base,
        joint_count * ARMATURE_ENTRY_LEN,
        emr_end,
        "EMR armature entries",
    )?;
    let mut children = budget::alloc(joint_count, "EMR joint children")?;
    for (index, entry) in entries
        .as_chunks::<ARMATURE_ENTRY_LEN>()
        .0
        .iter()
        .enumerate()
    {
        let child_count = i16::from_le_bytes([entry[0], entry[1]]);
        let child_list_offset = i16::from_le_bytes([entry[2], entry[3]]);
        if child_count < 0 || child_list_offset < 0 {
            bail!("EMR armature entry {index} has a negative child count or list offset");
        }
        let list_start = armature_base
            .checked_add(child_list_offset as usize)
            .context("EMR child list offset overflows")?;
        let list = chunk_slice(
            data,
            list_start,
            child_count as usize,
            emr_end,
            &format!("EMR child list {index}"),
        )?;
        children.push(list.to_vec());
    }

    Ok(Skeleton { relative, children })
}

fn parse_keyframes(data: &[u8], emr_offset: usize, emr_end: usize) -> Result<Vec<Keyframe>> {
    let keyframe_offset = read_u16(data, emr_offset + 2)? as usize;
    let joint_count = read_u16(data, emr_offset + 4)? as usize;
    let frame_stride = read_u16(data, emr_offset + 6)? as usize;
    if frame_stride == 0 {
        bail!("EMR frame stride is zero");
    }

    let start = emr_offset
        .checked_add(keyframe_offset)
        .context("EMR keyframe offset overflows")?;
    if start > emr_end {
        bail!("EMR keyframe offset 0x{keyframe_offset:X} is beyond the armature chunk");
    }
    let smallest_stride = KEYFRAME_HEADER_LEN + joint_count * ANGLE_LEN;
    if frame_stride < smallest_stride {
        bail!("EMR frame stride {frame_stride} is too small for {joint_count} joints");
    }

    let count = (emr_end - start) / frame_stride;
    budget::check_len(count, budget::MAX_EMD_KEYFRAMES, "EMR keyframe count")?;
    let mut keyframes = budget::alloc(count, "EMR keyframe list")?;
    for index in 0..count {
        let base = start + index * frame_stride;
        let offset = chunk_slice(data, base, ANGLE_LEN, emr_end, "EMR keyframe offset")?;
        let angles = chunk_slice(
            data,
            base + KEYFRAME_HEADER_LEN,
            joint_count * ANGLE_LEN,
            emr_end,
            "EMR keyframe rotations",
        )?;
        keyframes.push(Keyframe {
            offset: vec3(offset),
            rotations: angles
                .as_chunks::<ANGLE_LEN>()
                .0
                .iter()
                .map(|a| vec3(a))
                .collect(),
        });
    }

    Ok(keyframes)
}

fn parse_clips(data: &[u8], edd_offset: usize, chunk_end: usize) -> Result<Vec<Clip>> {
    let chunk = chunk_slice(
        data,
        edd_offset,
        chunk_end - edd_offset,
        chunk_end,
        "EDD animation chunk",
    )?;
    if chunk.len() < EDD_ENTRY_LEN {
        bail!(
            "EDD animation chunk is truncated: {} bytes, need at least {EDD_ENTRY_LEN}",
            chunk.len()
        );
    }

    let first_offset = u16::from_le_bytes([chunk[2], chunk[3]]) as usize;
    let clip_count = first_offset / EDD_ENTRY_LEN;
    budget::check_len(clip_count, budget::MAX_EMD_CLIPS, "EDD clip count")?;
    let table_len = clip_count * EDD_ENTRY_LEN;
    if table_len > chunk.len() {
        bail!(
            "EDD clip table for {clip_count} clip(s) does not fit in the {} byte chunk",
            chunk.len()
        );
    }

    let mut clips = budget::alloc(clip_count, "EDD clip list")?;
    let mut total_frames = 0usize;
    for (index, entry) in chunk[..table_len]
        .as_chunks::<EDD_ENTRY_LEN>()
        .0
        .iter()
        .enumerate()
    {
        let frame_count = u16::from_le_bytes([entry[0], entry[1]]) as usize;
        let frame_offset = u16::from_le_bytes([entry[2], entry[3]]) as usize;
        // Clip frame lists may overlap, so cap their total as well.
        total_frames = total_frames
            .checked_add(frame_count)
            .context("EDD frame total overflows")?;
        budget::check_len(total_frames, budget::MAX_RECORDS, "EDD clip frame total")?;
        let frames_len = frame_count
            .checked_mul(EDD_FRAME_LEN)
            .context("EDD frame list overflows")?;
        let frames_end = frame_offset
            .checked_add(frames_len)
            .context("EDD frame offset overflows")?;
        let frames = chunk.get(frame_offset..frames_end).with_context(|| {
            format!("EDD clip {index} frames at offset 0x{frame_offset:X} are out of bounds")
        })?;
        clips.push(Clip {
            frames: frames
                .as_chunks::<EDD_FRAME_LEN>()
                .0
                .iter()
                .map(|f| ClipFrame {
                    keyframe: u16::from_le_bytes([f[0], f[1]]) & KEYFRAME_INDEX_MASK,
                    timing: u16::from_le_bytes([f[2], f[3]]),
                })
                .collect(),
        });
    }

    Ok(clips)
}

fn chunk_slice<'a>(
    data: &'a [u8],
    start: usize,
    len: usize,
    chunk_end: usize,
    what: &str,
) -> Result<&'a [u8]> {
    let end = start
        .checked_add(len)
        .context("EMD/EMW chunk offset overflows")?;
    if end > chunk_end || end > data.len() {
        bail!("{what} at offset 0x{start:X} is out of bounds (chunk ends at 0x{chunk_end:X})");
    }
    Ok(&data[start..end])
}

fn vec3(bytes: &[u8]) -> [i16; 3] {
    [
        i16::from_le_bytes([bytes[0], bytes[1]]),
        i16::from_le_bytes([bytes[2], bytes[3]]),
        i16::from_le_bytes([bytes[4], bytes[5]]),
    ]
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    let bytes = data
        .get(offset..offset + 2)
        .with_context(|| format!("truncated u16 at offset 0x{offset:X}"))?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .with_context(|| format!("truncated u32 at offset 0x{offset:X}"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn minimal_tmd() -> Vec<u8> {
        let command = 0x3400_0609u32;
        let vertices = [[1i16, 2, 3], [4, 5, 6], [7, 8, 9]];
        let normals = [[0i16, 0, 4096], [0, 4096, 0], [4096, 0, 0]];
        let prim = [
            command,
            (0x7800u32 << 16) | (10 << 8) | 1,
            (0x80u32 << 16) | (11 << 8) | 2,
            (12 << 8) | 3,
            0,
            (1 << 16) | 1,
            (2 << 16) | 2,
        ];

        let mut data = Vec::new();
        data.extend_from_slice(&0x41u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&[0u8; 28]);

        let vtx_offset = data.len() - 12;
        for v in vertices {
            for c in v {
                data.extend_from_slice(&c.to_le_bytes());
            }
            data.extend_from_slice(&0i16.to_le_bytes());
        }
        let nor_offset = data.len() - 12;
        for n in normals {
            for c in n {
                data.extend_from_slice(&c.to_le_bytes());
            }
            data.extend_from_slice(&0i16.to_le_bytes());
        }
        let pri_offset = data.len() - 12;
        for w in prim {
            data.extend_from_slice(&w.to_le_bytes());
        }

        let descriptor = [
            vtx_offset as i32,
            3,
            nor_offset as i32,
            3,
            pri_offset as i32,
            1,
            0,
        ];
        for (slot, value) in descriptor.iter().enumerate() {
            let at = 12 + slot * 4;
            data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        data
    }

    fn minimal_tim() -> Vec<u8> {
        let palette = [0x001Fu16, 0x03E0];
        let mut data = Vec::new();
        data.extend_from_slice(&0x10u32.to_le_bytes());
        data.extend_from_slice(&0x09u32.to_le_bytes());
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&480i16.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        for entry in palette {
            data.extend_from_slice(&entry.to_le_bytes());
        }
        data.extend_from_slice(&14u32.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&[0u8, 1]);
        data
    }

    fn minimal_emr() -> Vec<u8> {
        let mut data = vec![0u8; 0x60];
        data[0..2].copy_from_slice(&0x30u16.to_le_bytes());
        data[2..4].copy_from_slice(&0x40u16.to_le_bytes());
        data[4..6].copy_from_slice(&2u16.to_le_bytes());
        data[6..8].copy_from_slice(&24u16.to_le_bytes());
        for (slot, joint) in [[1i16, 2, 3], [4, 5, 6]].iter().enumerate() {
            let at = 8 + slot * JOINT_POSITION_LEN;
            for (axis, value) in joint.iter().enumerate() {
                data[at + axis * 2..at + axis * 2 + 2].copy_from_slice(&value.to_le_bytes());
            }
        }
        data[0x30..0x32].copy_from_slice(&1i16.to_le_bytes());
        data[0x32..0x34].copy_from_slice(&8i16.to_le_bytes());
        data[0x38] = 1;
        for (slot, value) in [7i16, 8, 9, 0, 0, 0, 10, 11, 12, 13, 14, 15]
            .iter()
            .enumerate()
        {
            let at = 0x40 + slot * 2;
            data[at..at + 2].copy_from_slice(&value.to_le_bytes());
        }
        data
    }

    fn minimal_edd() -> Vec<u8> {
        let mut data = vec![0u8; 0x20];
        data[0..2].copy_from_slice(&1u16.to_le_bytes());
        data[2..4].copy_from_slice(&4u16.to_le_bytes());
        data[4..6].copy_from_slice(&0u16.to_le_bytes());
        data[6..8].copy_from_slice(&7u16.to_le_bytes());
        data
    }

    fn minimal_emd() -> Vec<u8> {
        let mut data = minimal_emr();
        let edd_offset = data.len();
        data.extend_from_slice(&minimal_edd());
        let tmd_offset = data.len();
        data.extend_from_slice(&minimal_tmd());
        let tim_offset = data.len();
        data.extend_from_slice(&minimal_tim());
        for value in [
            0u32,
            0,
            edd_offset as u32,
            tmd_offset as u32,
            tim_offset as u32,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data
    }

    fn minimal_emw() -> Vec<u8> {
        let mut data = minimal_emr();
        let edd_offset = data.len();
        data.extend_from_slice(&minimal_edd());
        let mesh_offset = data.len();
        data.extend_from_slice(&minimal_tmd());
        for value in [edd_offset as u32, mesh_offset as u32] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data
    }

    fn real_file(root: &Path, subdir: &str, name: &str) -> Option<PathBuf> {
        for prefix in ["JPN", ""] {
            let path = root.join(prefix).join(subdir).join(name);
            if path.is_file() {
                return Some(path);
            }
        }
        None
    }

    #[test]
    fn parses_minimal_emd() {
        let data = minimal_emd();

        let emd = parse(&data).unwrap();

        assert_eq!(emd.skeleton.relative, [[1, 2, 3], [4, 5, 6]]);
        assert_eq!(emd.skeleton.children, [vec![1], vec![]]);
        assert_eq!(emd.keyframes.len(), 1);
        assert_eq!(emd.keyframes[0].offset, [7, 8, 9]);
        assert_eq!(emd.keyframes[0].rotations, [[10, 11, 12], [13, 14, 15]]);
        assert_eq!(emd.clips.len(), 1);
        assert_eq!(
            emd.clips[0].frames,
            [ClipFrame {
                keyframe: 0,
                timing: 7
            }]
        );
        assert_eq!(emd.mesh.objects.len(), 1);
        assert_eq!(emd.mesh.objects[0].prims.len(), 1);
        assert_eq!(emd.texture.width, 2);
        assert_eq!(emd.texture.height, 1);
        assert_eq!(emd.texture.indices, [0, 1]);
        assert_eq!(emd.texture.palette(0, 0), [255, 0, 0, 255]);
        assert_eq!(emd.texture.palette(0, 1), [0, 255, 0, 255]);
    }

    #[test]
    fn parses_minimal_emw() {
        let data = minimal_emw();

        let emw = parse_emw(&data).unwrap();

        assert_eq!(emw.skeleton.relative, [[1, 2, 3], [4, 5, 6]]);
        assert_eq!(emw.keyframes.len(), 1);
        assert_eq!(emw.keyframes[0].offset, [7, 8, 9]);
        assert_eq!(emw.clips.len(), 1);
        assert_eq!(
            emw.clips[0].frames,
            [ClipFrame {
                keyframe: 0,
                timing: 7
            }]
        );
        assert_eq!(emw.mesh.objects.len(), 1);
        assert_eq!(emw.mesh.objects[0].prims.len(), 1);
    }

    #[test]
    fn parses_a_room_animation_pair() {
        // A non-null header offset, then the EDD table, bounded by the file end.
        let mut data = vec![0u8; 0x10];
        let header = data.len();
        data.extend_from_slice(&minimal_emr());
        let base = data.len();
        data.extend_from_slice(&minimal_edd());

        let anim = parse_room_anim(&data, header as u32, base as u32, data.len() as u32).unwrap();
        assert_eq!(anim.skeleton.relative, [[1, 2, 3], [4, 5, 6]]);
        assert_eq!(anim.keyframes.len(), 1);
        assert_eq!(anim.clips.len(), 1);
        assert_eq!(
            anim.clips[0].frames,
            [ClipFrame {
                keyframe: 0,
                timing: 7
            }]
        );
    }

    #[test]
    fn rejects_a_room_animation_pair_out_of_bounds() {
        let mut data = vec![0u8; 0x10];
        let header = data.len();
        data.extend_from_slice(&minimal_emr());
        let base = data.len();
        data.extend_from_slice(&minimal_edd());
        // The bound runs past the file.
        assert!(parse_room_anim(&data, header as u32, base as u32, 0x1000).is_err());
        // A null half.
        assert!(parse_room_anim(&data, 0, base as u32, data.len() as u32).is_err());
        // A base before the header is refused.
        assert!(parse_room_anim(&data, base as u32, header as u32, data.len() as u32).is_err());
    }

    #[test]
    fn rejects_truncated_emd_directory() {
        let err = parse(&[0u8; 19]).unwrap_err().to_string();
        assert!(err.contains("directory"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_truncated_emw_directory() {
        let err = parse_emw(&[0u8; 4]).unwrap_err().to_string();
        assert!(err.contains("directory"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_out_of_range_emd_offsets() {
        let mut data = minimal_emd();
        let len = data.len();
        data[len - 8..len - 4].copy_from_slice(&u32::MAX.to_le_bytes());

        let err = parse(&data).unwrap_err().to_string();

        assert!(err.contains("out of order"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_out_of_range_emw_mesh() {
        let mut data = minimal_emw();
        let len = data.len();
        data[len - 4..].copy_from_slice(&u32::MAX.to_le_bytes());

        let err = parse_emw(&data).unwrap_err().to_string();

        assert!(err.contains("out of order"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_clip_frames_out_of_range() {
        let mut data = minimal_emd();
        let len = data.len();
        let edd_offset = u32::from_le_bytes(data[len - 12..len - 8].try_into().unwrap()) as usize;
        data[edd_offset + 2..edd_offset + 4].copy_from_slice(&8u16.to_le_bytes());
        data[edd_offset + 6..edd_offset + 8].copy_from_slice(&1000u16.to_le_bytes());

        let err = parse(&data).unwrap_err().to_string();

        assert!(err.contains("clip 1"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_joint_and_clip_counts_over_the_caps() {
        let mut data = minimal_emd();
        data[4..6].copy_from_slice(&0xFFFFu16.to_le_bytes());
        let message = budget::assert_cap_error(parse(&data));
        assert!(message.contains("joint count"), "{message}");

        let mut data = minimal_emd();
        let len = data.len();
        let edd_offset = u32::from_le_bytes(data[len - 12..len - 8].try_into().unwrap()) as usize;
        let over = u16::try_from((budget::MAX_EMD_CLIPS + 1) * EDD_ENTRY_LEN).unwrap();
        data[edd_offset + 2..edd_offset + 4].copy_from_slice(&over.to_le_bytes());
        let message = budget::assert_cap_error(parse(&data));
        assert!(message.contains("clip count"), "{message}");
    }

    #[test]
    fn rejects_zero_frame_stride() {
        let mut data = minimal_emd();
        data[6..8].copy_from_slice(&0u16.to_le_bytes());

        let err = parse(&data).unwrap_err().to_string();

        assert!(err.contains("stride"), "unexpected error: {err}");
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn parses_real_player_models() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let cases = [
            ("Char10.emd", 16, 689),
            ("CHAR11.EMD", 16, 685),
            ("CHAR12.EMD", 15, 681),
            ("CHAR13.EMD", 16, 697),
        ];
        for (name, objects, triangles) in cases {
            let path = real_file(&root, "ENEMY", name)
                .unwrap_or_else(|| panic!("{name} not found under {}", root.display()));
            let data = std::fs::read(&path).unwrap();
            let emd = parse(&data).unwrap();
            let prims: usize = emd.mesh.objects.iter().map(|o| o.prims.len()).sum();
            println!(
                "{name}: {} object(s), {} joint(s), {} keyframe(s), {} clip(s), {}x{} texture, {} palette row(s), {prims} triangle(s)",
                emd.mesh.objects.len(),
                emd.skeleton.relative.len(),
                emd.keyframes.len(),
                emd.clips.len(),
                emd.texture.width,
                emd.texture.height,
                emd.texture.palettes.len() / 256,
            );
            assert_eq!(emd.mesh.objects.len(), objects, "{name}: object count");
            assert_eq!(emd.skeleton.relative.len(), 15, "{name}: joint count");
            assert_eq!(emd.skeleton.children.len(), 15, "{name}: child list count");
            assert_eq!(emd.keyframes.len(), 123, "{name}: keyframe count");
            assert_eq!(emd.clips.len(), 35, "{name}: clip count");
            assert_eq!(
                (emd.texture.width, emd.texture.height),
                (256, 256),
                "{name}: texture size"
            );
            assert_eq!(emd.texture.palettes.len(), 512, "{name}: palette size");
            assert_eq!(prims, triangles, "{name}: triangle count");
        }
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn parses_real_weapon_models() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        for name in ["W00.EMW", "W10.EMW"] {
            let path = real_file(&root, "PLAYERS", name)
                .unwrap_or_else(|| panic!("{name} not found under {}", root.display()));
            let data = std::fs::read(&path).unwrap();
            let emw = parse_emw(&data).unwrap();
            let prims: usize = emw.mesh.objects.iter().map(|o| o.prims.len()).sum();
            println!(
                "{name}: {} clip(s), {} object(s), {prims} triangle(s)",
                emw.clips.len(),
                emw.mesh.objects.len(),
            );
            assert_eq!(emw.clips.len(), 5, "{name}: clip count");
            assert!(!emw.mesh.objects.is_empty(), "{name}: empty mesh");
        }
    }
}
