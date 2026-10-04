//! Item-view model (`.IVM`) parser.
//!
//! An `.IVM` is the item's texture page (an 8bpp indexed TIM with one CLUT
//! row) followed immediately by a TMD mesh at `8 + clut_len + img_len`. The
//! shipped meshes use four polygon kinds: gouraud textured triangles
//! (`0x34000609`, with the `0x02000000` flag bit also seen as `0x36000609`),
//! gouraud textured quads (`0x3C00080C`), flat textured triangles
//! (`0x24000507`) and untextured flat-colour gouraud triangles (`0x30000406`).

use anyhow::{Context, Result, bail};

use crate::model::Texture8;
use crate::tim;

/// TMD magic byte.
const TMD_MAGIC: u32 = 0x41;
/// Tensor of the TMD header.
const TMD_HEADER_LEN: usize = 12;
/// Size of one TMD object descriptor.
const TMD_DESCRIPTOR_LEN: usize = 28;
/// Vertex and normal record size (x, y, z plus a padding word).
const VERTEX_LEN: usize = 8;
/// The flag bit the mesh decoder masks out of every polygon command.
const COMMAND_MASK: u32 = 0x3DFF_FFFF;

/// Gouraud textured triangle (`0x34000609` / `0x36000609`).
const GOURAUD_TEXTURED_TRIANGLE: u32 = 0x3400_0609;
/// Gouraud textured quad (`0x3C00080C`).
const GOURAUD_TEXTURED_QUAD: u32 = 0x3C00_080C;
/// Flat textured triangle (`0x24000507`).
const FLAT_TEXTURED_TRIANGLE: u32 = 0x2400_0507;
/// Flat-colour gouraud triangle (`0x30000406`).
const GOURAUD_TRIANGLE: u32 = 0x3000_0406;

/// A parsed item-view file: the texture page and its mesh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ivm {
    /// The item's 8bpp texture page.
    pub texture: Texture8,
    /// The TMD objects, in file order.
    pub objects: Vec<IvmObject>,
}

/// One TMD object: a vertex pool, a normal pool and the polygons using them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IvmObject {
    pub vertices: Vec<[i16; 3]>,
    pub normals: Vec<[i16; 3]>,
    pub prims: Vec<IvmPrim>,
}

/// Which mesh packet a polygon came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IvmPrimKind {
    /// Gouraud textured triangle: three UVs, per-vertex normals.
    TexturedGouraud,
    /// Gouraud textured quad: four UVs, per-vertex normals.
    TexturedGouraudQuad,
    /// Flat textured triangle: one normal and a flat shade colour.
    TexturedFlat,
    /// Untextured flat-colour gouraud triangle.
    Gouraud,
}

/// One polygon. Triangles use the first three vertices, quads all four;
/// `vertex_count` says how many. Textured polygons have a white `color`;
/// untextured ones have zero UVs and no `clut`/`tsb`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IvmPrim {
    pub kind: IvmPrimKind,
    pub vertex_count: u8,
    /// Vertex indices into the owning object's `vertices`.
    pub vertices: [u16; 4],
    /// Normal indices into the owning object's `normals`.
    pub normals: [u16; 4],
    /// Texture coordinates of each corner.
    pub uv: [[u8; 2]; 4],
    /// Flat shade colour (`255` per channel on textured polygons).
    pub color: [u8; 3],
    /// CLUT word from the packet's colour word.
    pub clut: u16,
    /// Texture-page word from the packet.
    pub tsb: u16,
}

/// Parse one `.IVM` file.
pub fn parse(data: &[u8]) -> Result<Ivm> {
    let tmd_offset = texture_end(data)?;
    let texture = tim::decode_8bpp(data).context("failed to decode the IVM texture")?;
    let tmd = data
        .get(tmd_offset..)
        .with_context(|| format!("IVM TMD offset 0x{tmd_offset:X} is out of bounds"))?;
    let objects = parse_tmd(tmd)?;
    Ok(Ivm { texture, objects })
}

/// Compute the TMD offset (`8 + clut_len + img_len`) after validating the TIM
/// header, so a malformed texture cannot send the TMD parser into the pixels.
fn texture_end(data: &[u8]) -> Result<usize> {
    let header = data
        .get(0..8)
        .context("IVM is too short to hold a TIM header")?;
    let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if magic != 0x10 {
        bail!("IVM texture is not a TIM: magic 0x{magic:08X}");
    }
    let flags = u32::from_le_bytes(header[4..8].try_into().unwrap());
    if flags & 7 != 1 || flags & 8 == 0 {
        bail!("IVM texture is not an indexed TIM with a CLUT: flags 0x{flags:02X}");
    }

    let clut_len = read_u32(data, 8).context("IVM TIM CLUT block is truncated")? as usize;
    let image = 8usize
        .checked_add(clut_len)
        .context("IVM TIM CLUT length overflows")?;
    let img_len = read_u32(data, image).context("IVM TIM image block is truncated")? as usize;
    image
        .checked_add(img_len)
        .filter(|&end| end <= data.len())
        .with_context(|| {
            format!(
                "IVM TMD offset 0x{:X} is past the {}-byte file",
                image + img_len,
                data.len()
            )
        })
}

/// Parse the TMD mesh that follows the texture.
fn parse_tmd(data: &[u8]) -> Result<Vec<IvmObject>> {
    let header = data
        .get(0..TMD_HEADER_LEN)
        .context("IVM TMD header is truncated")?;
    let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if magic != TMD_MAGIC {
        bail!("IVM TMD has magic 0x{magic:08X}, expected 0x{TMD_MAGIC:08X}");
    }
    let object_count = read_u32(data, 8)? as usize;
    let table_len = object_count
        .checked_mul(TMD_DESCRIPTOR_LEN)
        .context("IVM TMD object count overflows")?;
    let table = data
        .get(TMD_HEADER_LEN..TMD_HEADER_LEN + table_len)
        .with_context(|| {
            format!("IVM TMD descriptor table for {object_count} object(s) is truncated")
        })?;

    let mut objects = Vec::with_capacity(object_count);
    for (index, descriptor) in table.as_chunks::<TMD_DESCRIPTOR_LEN>().0.iter().enumerate() {
        let field = |slot: usize| {
            i32::from_le_bytes(descriptor[slot * 4..slot * 4 + 4].try_into().unwrap())
        };
        let vertices = parse_pool(data, field(0), field(1), index, "vertex")?;
        let normals = parse_pool(data, field(2), field(3), index, "normal")?;
        let prims = parse_prims(data, field(4), field(5), &vertices, &normals, index)?;
        objects.push(IvmObject {
            vertices,
            normals,
            prims,
        });
    }
    Ok(objects)
}

/// Parse one vertex or normal pool (both are 8-byte records).
fn parse_pool(
    data: &[u8],
    offset: i32,
    count: i32,
    object: usize,
    what: &str,
) -> Result<Vec<[i16; 3]>> {
    let count = usize::try_from(count)
        .with_context(|| format!("IVM TMD object {object} {what} count is negative: {count}"))?;
    let start = relative(data, offset, object, what)?;
    let bytes = data
        .get(start..)
        .with_context(|| format!("IVM TMD object {object} {what} pool is truncated"))?;
    let needed = count
        .checked_mul(VERTEX_LEN)
        .context("IVM TMD pool size overflows")?;
    let bytes = bytes
        .get(..needed)
        .with_context(|| format!("IVM TMD object {object} {what} pool needs {needed} bytes"))?;
    Ok(bytes
        .as_chunks::<VERTEX_LEN>()
        .0
        .iter()
        .map(|record| {
            [
                i16::from_le_bytes([record[0], record[1]]),
                i16::from_le_bytes([record[2], record[3]]),
                i16::from_le_bytes([record[4], record[5]]),
            ]
        })
        .collect())
}

/// Parse an object's primitive list, walking the variable-size packets by the
/// length encoded in each command word.
fn parse_prims(
    data: &[u8],
    offset: i32,
    count: i32,
    vertices: &[[i16; 3]],
    normals: &[[i16; 3]],
    object: usize,
) -> Result<Vec<IvmPrim>> {
    let count = usize::try_from(count)
        .with_context(|| format!("IVM TMD object {object} primitive count is negative: {count}"))?;
    let mut position = relative(data, offset, object, "primitive")?;
    // A primitive packet is at least 4 bytes, so the remaining mesh bytes cap
    // how many can possibly parse; never reserve an attacker-sized count.
    let capacity = count.min(data.len().saturating_sub(position) / 4);
    let mut prims = Vec::with_capacity(capacity);
    for index in 0..count {
        let packet = data
            .get(position..position + 4)
            .with_context(|| format!("IVM TMD object {object} primitive {index} is truncated"))?;
        let command = u32::from_le_bytes(packet.try_into().unwrap());
        let size = (((command & 0xFF00) >> 6) + 4) as usize;
        let packet = data.get(position..position + size).with_context(|| {
            format!("IVM TMD object {object} primitive {index} overruns the mesh")
        })?;
        let prim = decode_prim(
            command & COMMAND_MASK,
            packet,
            vertices.len(),
            normals.len(),
        )
        .with_context(|| format!("IVM TMD object {object} primitive {index}"))?;
        prims.push(prim);
        position += size;
    }
    Ok(prims)
}

/// Decode one polygon packet.
fn decode_prim(
    command: u32,
    packet: &[u8],
    vertex_count: usize,
    normal_count: usize,
) -> Result<IvmPrim> {
    let word = |slot: usize| u32::from_le_bytes(packet[slot * 4..slot * 4 + 4].try_into().unwrap());
    let prim = match command {
        GOURAUD_TEXTURED_TRIANGLE => {
            let color = word(1);
            IvmPrim {
                kind: IvmPrimKind::TexturedGouraud,
                vertex_count: 3,
                vertices: corners(&[word(4), word(5), word(6)], true, 3),
                normals: corners(&[word(4), word(5), word(6)], false, 3),
                uv: uvs(&[color, word(2), word(3)], 3),
                color: [255, 255, 255],
                clut: (color >> 16) as u16,
                tsb: (word(2) >> 16) as u16,
            }
        }
        GOURAUD_TEXTURED_QUAD => {
            let color = word(1);
            IvmPrim {
                kind: IvmPrimKind::TexturedGouraudQuad,
                vertex_count: 4,
                vertices: corners(&[word(5), word(6), word(7), word(8)], true, 4),
                normals: corners(&[word(5), word(6), word(7), word(8)], false, 4),
                uv: uvs(&[color, word(2), word(3), word(4)], 4),
                color: [255, 255, 255],
                clut: (color >> 16) as u16,
                tsb: (word(2) >> 16) as u16,
            }
        }
        FLAT_TEXTURED_TRIANGLE => {
            let color = word(1);
            let normal = (word(4) & 0xFFFF) as u16;
            IvmPrim {
                kind: IvmPrimKind::TexturedFlat,
                vertex_count: 3,
                vertices: [
                    (word(4) >> 16) as u16,
                    (word(5) & 0xFFFF) as u16,
                    (word(5) >> 16) as u16,
                    0,
                ],
                normals: [normal, 0, 0, 0],
                uv: uvs(&[color, word(2), word(3)], 3),
                color: shade(color),
                clut: (color >> 16) as u16,
                tsb: (word(2) >> 16) as u16,
            }
        }
        GOURAUD_TRIANGLE => {
            let color = word(1);
            IvmPrim {
                kind: IvmPrimKind::Gouraud,
                vertex_count: 3,
                vertices: corners(&[word(2), word(3), word(4)], true, 3),
                normals: corners(&[word(2), word(3), word(4)], false, 3),
                uv: [[0, 0]; 4],
                color: shade(color),
                clut: 0,
                tsb: 0,
            }
        }
        other => bail!("unsupported TMD polygon command 0x{other:08X}"),
    };

    for (corner, &index) in prim
        .vertices
        .iter()
        .enumerate()
        .take(usize::from(prim.vertex_count))
    {
        if usize::from(index) >= vertex_count {
            bail!(
                "vertex index {index} (corner {corner}) is out of range ({vertex_count} vertices)"
            );
        }
    }
    // The flat triangle's spare normal slots are zeros; only its first normal
    // is a real index.
    let normal_corners = if prim.kind == IvmPrimKind::TexturedFlat {
        1
    } else {
        usize::from(prim.vertex_count)
    };
    for (corner, &index) in prim.normals.iter().enumerate().take(normal_corners) {
        if usize::from(index) >= normal_count {
            bail!(
                "normal index {index} (corner {corner}) is out of range ({normal_count} normals)"
            );
        }
    }
    Ok(prim)
}

/// Extract vertex or normal indices from the high or low halves of `words`.
fn corners(words: &[u32], high: bool, count: usize) -> [u16; 4] {
    let mut out = [0u16; 4];
    for (slot, &word) in words.iter().enumerate().take(count) {
        out[slot] = if high {
            (word >> 16) as u16
        } else {
            (word & 0xFFFF) as u16
        };
    }
    out
}

/// Extract `count` texture coordinates from the low two bytes of `words`.
fn uvs(words: &[u32], count: usize) -> [[u8; 2]; 4] {
    let mut out = [[0u8; 2]; 4];
    for (slot, &word) in words.iter().enumerate().take(count) {
        out[slot] = [(word & 0xFF) as u8, ((word >> 8) & 0xFF) as u8];
    }
    out
}

/// Split a packet colour word into its three 8-bit shade channels.
fn shade(word: u32) -> [u8; 3] {
    [
        (word & 0xFF) as u8,
        ((word >> 8) & 0xFF) as u8,
        ((word >> 16) & 0xFF) as u8,
    ]
}

/// Resolve a TMD-relative pool offset.
fn relative(data: &[u8], offset: i32, object: usize, what: &str) -> Result<usize> {
    let relative = usize::try_from(offset)
        .with_context(|| format!("IVM TMD object {object} {what} offset is negative: {offset}"))?;
    let start = TMD_HEADER_LEN
        .checked_add(relative)
        .context("IVM TMD offset overflows")?;
    if start > data.len() {
        bail!(
            "IVM TMD object {object} {what} offset 0x{start:X} is past the {}-byte mesh",
            data.len()
        );
    }
    Ok(start)
}

/// Read a little-endian `u32`.
fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let raw = data
        .get(offset..offset + 4)
        .context("IVM read is out of bounds")?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::tim::decode_8bpp;

    /// A 256x256 8bpp TIM with one 256-entry CLUT row.
    fn sample_texture() -> Vec<u8> {
        let mut palette = Vec::new();
        for index in 0..256u16 {
            palette.push(index.wrapping_mul(0x0841));
        }
        let pixels: Vec<u8> = (0..256 * 256).map(|index| (index % 251) as u8).collect();
        let mut out = Vec::new();
        out.extend_from_slice(&0x10u32.to_le_bytes());
        out.extend_from_slice(&9u32.to_le_bytes());
        out.extend_from_slice(&(12u32 + palette.len() as u32 * 2).to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&480i16.to_le_bytes());
        out.extend_from_slice(&256u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        for entry in &palette {
            out.extend_from_slice(&entry.to_le_bytes());
        }
        out.extend_from_slice(&(12u32 + pixels.len() as u32).to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&128u16.to_le_bytes());
        out.extend_from_slice(&256u16.to_le_bytes());
        out.extend_from_slice(&pixels);
        out
    }

    /// One TMD object with a single gouraud textured triangle.
    fn sample_tmd() -> Vec<u8> {
        let vertices = [[1i16, 2, 3], [4, 5, 6], [7, 8, 9]];
        let normals = [[0i16, 0, 4096], [0, 4096, 0], [4096, 0, 0]];
        let prim = [
            GOURAUD_TEXTURED_TRIANGLE,
            (0x7800u32 << 16) | (10 << 8) | 1,
            (0x80u32 << 16) | (11 << 8) | 2,
            (12 << 8) | 3,
            0,
            (1 << 16) | 1,
            (2 << 16) | 2,
        ];

        let mut data = Vec::new();
        data.extend_from_slice(&TMD_MAGIC.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        let descriptor_at = data.len();
        data.extend_from_slice(&[0u8; TMD_DESCRIPTOR_LEN]);

        let vertex_offset = data.len() - TMD_HEADER_LEN;
        for vertex in vertices {
            for component in vertex {
                data.extend_from_slice(&component.to_le_bytes());
            }
            data.extend_from_slice(&0i16.to_le_bytes());
        }
        let normal_offset = data.len() - TMD_HEADER_LEN;
        for normal in normals {
            for component in normal {
                data.extend_from_slice(&component.to_le_bytes());
            }
            data.extend_from_slice(&0i16.to_le_bytes());
        }
        let prim_offset = data.len() - TMD_HEADER_LEN;
        for value in prim {
            data.extend_from_slice(&value.to_le_bytes());
        }

        let descriptor = [
            vertex_offset as i32,
            vertices.len() as i32,
            normal_offset as i32,
            normals.len() as i32,
            prim_offset as i32,
            1,
            0,
        ];
        for (slot, value) in descriptor.iter().enumerate() {
            let at = descriptor_at + slot * 4;
            data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        data
    }

    fn sample_ivm() -> Vec<u8> {
        let mut data = sample_texture();
        data.extend_from_slice(&sample_tmd());
        data
    }

    #[test]
    fn parses_texture_and_mesh() {
        let ivm = parse(&sample_ivm()).unwrap();

        assert_eq!(ivm.texture.width, 256);
        assert_eq!(ivm.texture.height, 256);
        assert_eq!(ivm.texture.indices.len(), 256 * 256);
        assert_eq!(ivm.objects.len(), 1);

        let object = &ivm.objects[0];
        assert_eq!(object.vertices, [[1, 2, 3], [4, 5, 6], [7, 8, 9]]);
        assert_eq!(object.normals, [[0, 0, 4096], [0, 4096, 0], [4096, 0, 0]]);
        assert_eq!(object.prims.len(), 1);

        let prim = &object.prims[0];
        assert_eq!(prim.kind, IvmPrimKind::TexturedGouraud);
        assert_eq!(prim.vertex_count, 3);
        assert_eq!(prim.vertices, [0, 1, 2, 0]);
        assert_eq!(prim.normals, [0, 1, 2, 0]);
        assert_eq!(prim.uv, [[1, 10], [2, 11], [3, 12], [0, 0]]);
        assert_eq!(prim.color, [255, 255, 255]);
        assert_eq!(prim.clut, 0x7800);
        assert_eq!(prim.tsb, 0x80);
    }

    #[test]
    fn parses_the_texture_at_the_header_offset() {
        let texture = sample_texture();
        let expected = decode_8bpp(&texture).unwrap();
        let ivm = parse(&sample_ivm()).unwrap();
        assert_eq!(ivm.texture, expected);
    }

    #[test]
    fn rejects_a_non_tim_file() {
        assert!(parse(&[0u8; 32]).is_err());
    }

    #[test]
    fn rejects_a_texture_without_a_clut() {
        let mut data = sample_ivm();
        data[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert!(parse(&data).is_err());
    }

    #[test]
    fn rejects_an_unknown_polygon_command() {
        let mut data = sample_ivm();
        let prim_at = data.len() - 28;
        data[prim_at..prim_at + 4].copy_from_slice(&0x3400_0404u32.to_le_bytes());
        let error = parse(&data).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("0x34000404"), "{message}");
    }

    #[test]
    fn rejects_an_absurd_primitive_count_without_reserving_it() {
        let mut data = sample_ivm();
        // Descriptor field 5 is the primitive count; overwrite it with a huge
        // attacker-controlled value. The parser must cap its reservation by the
        // remaining mesh bytes and fail on the truncated list instead of
        // trying to allocate 2 GiB.
        let tmd = texture_end(&data).unwrap();
        let count_at = tmd + TMD_HEADER_LEN + 5 * 4;
        data[count_at..count_at + 4].copy_from_slice(&0x7FFF_FFFFi32.to_le_bytes());
        let error = parse(&data).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("primitive"), "{message}");
    }

    #[test]
    fn rejects_an_out_of_range_vertex() {
        let mut data = sample_ivm();
        let prim_at = data.len() - 28;
        data[prim_at + 16..prim_at + 20].copy_from_slice(&(5u32 << 16).to_le_bytes());
        let error = parse(&data).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("out of range"), "{message}");
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn parses_all_shipped_ivms() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let dir = Path::new(&root).join("JPN/ITEM_M2");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("failed to list {}: {error}", dir.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ivm"))
            })
            .collect();
        files.sort();
        assert_eq!(files.len(), 77);

        let mut textured = 0usize;
        let mut quads = 0usize;
        let mut flat = 0usize;
        let mut gouraud = 0usize;
        let mut knife = false;
        for path in &files {
            let data = std::fs::read(path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            let ivm = parse(&data)
                .unwrap_or_else(|error| panic!("failed to parse {}: {error:#}", path.display()));
            assert!((ivm.texture.width, ivm.texture.height) == (256, 256));
            assert!(!ivm.objects.is_empty(), "{}", path.display());
            let name = path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_ascii_lowercase();
            if name == "i00v.ivm" {
                knife = ivm.objects.iter().any(|object| !object.prims.is_empty());
            }
            for object in &ivm.objects {
                for prim in &object.prims {
                    match prim.kind {
                        IvmPrimKind::TexturedGouraud => textured += 1,
                        IvmPrimKind::TexturedGouraudQuad => quads += 1,
                        IvmPrimKind::TexturedFlat => flat += 1,
                        IvmPrimKind::Gouraud => gouraud += 1,
                    }
                }
            }
        }

        assert!(knife, "the combat knife's I00V.IVM has no primitives");
        assert_eq!(textured, 26347);
        assert_eq!(quads, 26);
        assert_eq!(flat, 523);
        assert_eq!(gouraud, 39);
    }
}
