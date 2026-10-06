//! PSX TMD mesh parser.

use anyhow::{Context, Result, bail};

use crate::model::{Tmd, TmdObject, TmdPrim};

const HEADER_LEN: usize = 12;
const DESCRIPTOR_LEN: usize = 28;
const VERTEX_LEN: usize = 8;
const NORMAL_LEN: usize = 8;

/// Textured gouraud triangle (7 words); the packet every EMD/EMW uses.
pub const GOURAUD_TEXTURED_TRIANGLE: u32 = 0x3400_0609;
/// The same packet with the semi-transparency (ABE) command bit.
pub const GOURAUD_TEXTURED_TRIANGLE_BLEND: u32 = 0x3600_0609;
/// Textured flat triangle with one normal (6 words).
pub const FLAT_TEXTURED_TRIANGLE: u32 = 0x2400_0507;
/// Textured triangle with a `(0, 0, -1)` normal and raw Y vertices (7 words).
pub const TEXTURED_TRIANGLE_RAW_Y: u32 = 0x2501_0607;
/// Gouraud-shaded untextured triangle with a flat packet colour (5 words).
pub const GOURAUD_FLAT_UNTEXTURED_TRIANGLE: u32 = 0x3000_0406;
/// Textured gouraud quad (13 words), split into two triangles.
pub const GOURAUD_TEXTURED_QUAD: u32 = 0x3C00_080C;

/// The normal [`TEXTURED_TRIANGLE_RAW_Y`] packets carry implicitly:
/// `(0, 0, -1)` in the pool's 12-bit signed convention.
const RAW_Y_NORMAL: [i16; 3] = [0, 0, -4096];

/// Parse a TMD whose first byte is the TMD magic.
///
/// Every object descriptor has a vertex, normal and primitive list; all
/// offsets are relative to the descriptor table at `TMD + 0xC`. The six
/// packet commands the RDT-embedded models use are accepted, covering the
/// textured gouraud triangle the player/enemy models are made of as well as
/// the flat-textured, raw-Y, flat-colour and quad forms the object models
/// add. Anything else is an error.
pub fn parse(data: &[u8]) -> Result<Tmd> {
    if data.len() < HEADER_LEN {
        bail!(
            "TMD header is truncated: {} bytes, need at least {HEADER_LEN}",
            data.len()
        );
    }

    let object_count = read_u32(data, 8)? as usize;
    let table_len = object_count
        .checked_mul(DESCRIPTOR_LEN)
        .context("TMD object count overflows")?;
    let table_end = HEADER_LEN
        .checked_add(table_len)
        .context("TMD descriptor table overflows")?;
    let table = data.get(HEADER_LEN..table_end).with_context(|| {
        format!("TMD descriptor table for {object_count} object(s) is truncated")
    })?;

    let mut objects = Vec::with_capacity(object_count);
    for (index, descriptor) in table.as_chunks::<DESCRIPTOR_LEN>().0.iter().enumerate() {
        let field = |slot: usize| {
            i32::from_le_bytes(descriptor[slot * 4..slot * 4 + 4].try_into().unwrap())
        };
        let vertices = parse_vertices(data, field(0), field(1), index)?;
        let mut normals = parse_normals(data, field(2), field(3), index)?;
        let prims = parse_prims(data, field(4), field(5), &vertices, &mut normals, index)?;
        objects.push(TmdObject {
            vertices,
            normals,
            prims,
        });
    }
    Ok(Tmd { objects })
}

fn parse_vertices(data: &[u8], offset: i32, count: i32, object: usize) -> Result<Vec<[i16; 3]>> {
    let count = checked_count(count, &format!("object {object} vertex"))?;
    let start = relative(offset, &format!("object {object} vertex offset"))?;
    let bytes = checked_slice(
        data,
        start,
        count
            .checked_mul(VERTEX_LEN)
            .context("TMD vertex data overflows")?,
        &format!("object {object} vertex data"),
    )?;
    Ok(bytes
        .as_chunks::<VERTEX_LEN>()
        .0
        .iter()
        .map(|v| vec3(v))
        .collect())
}

fn parse_normals(data: &[u8], offset: i32, count: i32, object: usize) -> Result<Vec<[i16; 3]>> {
    let count = checked_count(count, &format!("object {object} normal"))?;
    let start = relative(offset, &format!("object {object} normal offset"))?;
    let bytes = checked_slice(
        data,
        start,
        count
            .checked_mul(NORMAL_LEN)
            .context("TMD normal data overflows")?,
        &format!("object {object} normal data"),
    )?;
    Ok(bytes
        .as_chunks::<NORMAL_LEN>()
        .0
        .iter()
        .map(|n| vec3(n))
        .collect())
}

fn parse_prims(
    data: &[u8],
    offset: i32,
    count: i32,
    vertices: &[[i16; 3]],
    normals: &mut Vec<[i16; 3]>,
    object: usize,
) -> Result<Vec<TmdPrim>> {
    let count = checked_count(count, &format!("object {object} primitive"))?;
    let start = relative(offset, &format!("object {object} primitive offset"))?;
    let mut prims = Vec::with_capacity(count);
    let mut cursor = start;
    for packet in 0..count {
        let (decoded, size) = decode_prim(data, cursor, vertices.len(), normals)
            .map_err(|error| anyhow::anyhow!("object {object} primitive {packet}: {error:#}"))?;
        cursor = cursor
            .checked_add(size)
            .context("TMD primitive cursor overflows")?;
        prims.extend(decoded);
    }
    Ok(prims)
}

/// Decode one variable-length primitive packet at `offset`.
///
/// The packet's length comes from the command word's bits 8-15, exactly like
/// the original's primitive walk: `((command & 0xFF00) >> 6) + 4` bytes. Most
/// commands produce one triangle; the textured gouraud quad produces two. A
/// raw-Y packet's implicit `(0, 0, -1)` normal is appended to the object's
/// normal pool once per packet. Returns the decoded primitives and the bytes
/// consumed.
fn decode_prim(
    data: &[u8],
    offset: usize,
    vertex_count: usize,
    normals: &mut Vec<[i16; 3]>,
) -> Result<(Vec<TmdPrim>, usize)> {
    let command = data
        .get(offset..offset + 4)
        .map(|raw| u32::from_le_bytes(raw.try_into().unwrap()))
        .with_context(|| format!("primitive command at 0x{offset:X} is truncated"))?;
    let size = (((command & 0xFF00) >> 6) as usize) + 4;
    let packet = data
        .get(offset..offset + size)
        .with_context(|| format!("primitive 0x{command:08X} at 0x{offset:X} is truncated"))?;
    let word = |slot: usize| u32::from_le_bytes(packet[slot * 4..slot * 4 + 4].try_into().unwrap());
    let w1 = word(1);
    let w2 = word(2);
    let w3 = word(3);
    let uv = [
        [(w1 & 0xFF) as u8, ((w1 >> 8) & 0xFF) as u8],
        [(w2 & 0xFF) as u8, ((w2 >> 8) & 0xFF) as u8],
        [(w3 & 0xFF) as u8, ((w3 >> 8) & 0xFF) as u8],
    ];

    let decoded = match command {
        GOURAUD_TEXTURED_TRIANGLE | GOURAUD_TEXTURED_TRIANGLE_BLEND => {
            let w4 = word(4);
            let w5 = word(5);
            let w6 = word(6);
            let vertices = [(w4 >> 16) as u16, (w5 >> 16) as u16, (w6 >> 16) as u16];
            let normal_indices = [
                (w4 & 0xFFFF) as u16,
                (w5 & 0xFFFF) as u16,
                (w6 & 0xFFFF) as u16,
            ];
            let prim = TmdPrim {
                vertices,
                normals: normal_indices,
                uv,
                clut: (w1 >> 16) as u16,
                tsb: (w2 >> 16) as u16,
                textured: true,
                blend: command == GOURAUD_TEXTURED_TRIANGLE_BLEND,
                raw_y: false,
                flat_color: None,
                quad: None,
            };
            check_prim(&prim, vertex_count, normals.len())?;
            vec![prim]
        }
        FLAT_TEXTURED_TRIANGLE => {
            let w4 = word(4);
            let w5 = word(5);
            let vertices = [(w4 >> 16) as u16, (w5 & 0xFFFF) as u16, (w5 >> 16) as u16];
            // The packet carries one normal that applies to the whole flat
            // triangle; the port replicates it across the three corners.
            let normal = (w4 & 0xFFFF) as u16;
            let prim = TmdPrim {
                vertices,
                normals: [normal; 3],
                uv,
                clut: (w1 >> 16) as u16,
                tsb: (w2 >> 16) as u16,
                textured: true,
                blend: false,
                raw_y: false,
                flat_color: None,
                quad: None,
            };
            check_prim(&prim, vertex_count, normals.len())?;
            vec![prim]
        }
        TEXTURED_TRIANGLE_RAW_Y => {
            let w5 = word(5);
            let w6 = word(6);
            let vertices = [
                (w5 & 0xFFFF) as u16,
                (w5 >> 16) as u16,
                (w6 & 0xFFFF) as u16,
            ];
            let normal =
                u16::try_from(normals.len()).context("TMD normal pool is larger than u16")?;
            normals.push(RAW_Y_NORMAL);
            let prim = TmdPrim {
                vertices,
                normals: [normal; 3],
                uv,
                clut: (w1 >> 16) as u16,
                tsb: (w2 >> 16) as u16,
                textured: true,
                blend: false,
                raw_y: true,
                flat_color: None,
                quad: None,
            };
            check_prim(&prim, vertex_count, normals.len())?;
            vec![prim]
        }
        GOURAUD_FLAT_UNTEXTURED_TRIANGLE => {
            let w4 = word(4);
            let vertices = [(w2 >> 16) as u16, (w3 >> 16) as u16, (w4 >> 16) as u16];
            let normal_indices = [
                (w2 & 0xFFFF) as u16,
                (w3 & 0xFFFF) as u16,
                (w4 & 0xFFFF) as u16,
            ];
            let colour = [
                (w1 & 0xFF) as u8,
                ((w1 >> 8) & 0xFF) as u8,
                ((w1 >> 16) & 0xFF) as u8,
            ];
            let prim = TmdPrim {
                vertices,
                normals: normal_indices,
                uv: [[0, 0]; 3],
                clut: 0,
                tsb: 0,
                textured: false,
                blend: false,
                raw_y: false,
                flat_color: Some(colour),
                quad: None,
            };
            check_prim(&prim, vertex_count, normals.len())?;
            vec![prim]
        }
        GOURAUD_TEXTURED_QUAD => {
            let w4 = word(4);
            let w5 = word(5);
            let w6 = word(6);
            let w7 = word(7);
            let w8 = word(8);
            let vertices = [
                (w5 >> 16) as u16,
                (w6 >> 16) as u16,
                (w7 >> 16) as u16,
                (w8 >> 16) as u16,
            ];
            let normal_indices = [
                (w5 & 0xFFFF) as u16,
                (w6 & 0xFFFF) as u16,
                (w7 & 0xFFFF) as u16,
                (w8 & 0xFFFF) as u16,
            ];
            let uvs = [
                uv[0],
                uv[1],
                uv[2],
                [(w4 & 0xFF) as u8, ((w4 >> 8) & 0xFF) as u8],
            ];
            let clut = (w1 >> 16) as u16;
            let tsb = (w2 >> 16) as u16;
            // The quad's corner ring is (0, 1, 3, 2); fan it into two
            // triangles in that order.
            let mut prims = Vec::with_capacity(2);
            for corners in [[0usize, 1, 3], [0, 3, 2]] {
                let prim = TmdPrim {
                    vertices: corners.map(|corner| vertices[corner]),
                    normals: corners.map(|corner| normal_indices[corner]),
                    uv: corners.map(|corner| uvs[corner]),
                    clut,
                    tsb,
                    textured: true,
                    blend: false,
                    raw_y: false,
                    flat_color: None,
                    quad: Some(vertices),
                };
                check_prim(&prim, vertex_count, normals.len())?;
                prims.push(prim);
            }
            prims
        }
        _ => bail!(
            "unsupported TMD primitive command 0x{command:08X}; only the six \
             0x34000609/0x36000609/0x24000507/0x25010607/0x30000406/0x3C00080C forms are supported"
        ),
    };
    Ok((decoded, size))
}

/// Bounds-check one decoded primitive's vertex and normal indices.
fn check_prim(prim: &TmdPrim, vertex_count: usize, normal_count: usize) -> Result<()> {
    for (corner, &index) in prim.vertices.iter().enumerate() {
        if index as usize >= vertex_count {
            bail!(
                "TMD primitive vertex index {index} (corner {corner}) is out of range ({vertex_count} vertices)"
            );
        }
    }
    for (corner, &index) in prim.normals.iter().enumerate() {
        if index as usize >= normal_count {
            bail!(
                "TMD primitive normal index {index} (corner {corner}) is out of range ({normal_count} normals)"
            );
        }
    }
    Ok(())
}

fn vec3(bytes: &[u8]) -> [i16; 3] {
    [
        i16::from_le_bytes([bytes[0], bytes[1]]),
        i16::from_le_bytes([bytes[2], bytes[3]]),
        i16::from_le_bytes([bytes[4], bytes[5]]),
    ]
}

fn checked_count(count: i32, what: &str) -> Result<usize> {
    usize::try_from(count).with_context(|| format!("{what} count is negative: {count}"))
}

fn relative(value: i32, what: &str) -> Result<usize> {
    let relative =
        usize::try_from(value).with_context(|| format!("{what} is negative: {value}"))?;
    HEADER_LEN
        .checked_add(relative)
        .context("TMD offset overflows")
}

fn checked_slice<'a>(data: &'a [u8], offset: usize, len: usize, what: &str) -> Result<&'a [u8]> {
    let end = offset.checked_add(len).context("TMD offset overflows")?;
    data.get(offset..end).with_context(|| {
        format!(
            "truncated TMD {what}: offset 0x{offset:X} plus {len} bytes does not fit in {} bytes",
            data.len()
        )
    })
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .with_context(|| format!("truncated u32 at TMD offset 0x{offset:X}"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::identity_op)] // the packet fixtures spell every word out

    use super::*;

    /// Build a one-object TMD around `prims` (each already its on-disk word
    /// count) with four vertices and four normals.
    fn sample_tmd(prims: &[&[u32]]) -> Vec<u8> {
        sample_tmd_with_vertex_count(prims, 4)
    }

    fn sample_tmd_with_vertex_count(prims: &[&[u32]], vertex_count: usize) -> Vec<u8> {
        let vertices: Vec<[i16; 3]> = (0..vertex_count)
            .map(|index| [index as i16, index as i16 + 1, index as i16 + 2])
            .collect();
        let normals = [[0i16, 0, 4096], [0, 4096, 0], [4096, 0, 0], [0, 0, -4096]];

        let mut data = Vec::new();
        data.extend_from_slice(&0x41u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        let descriptor_at = data.len();
        data.extend_from_slice(&[0u8; DESCRIPTOR_LEN]);

        let vtx_offset = data.len() - HEADER_LEN;
        for v in &vertices {
            for c in v {
                data.extend_from_slice(&c.to_le_bytes());
            }
            data.extend_from_slice(&0i16.to_le_bytes());
        }

        let nor_offset = data.len() - HEADER_LEN;
        for n in normals {
            for c in n {
                data.extend_from_slice(&c.to_le_bytes());
            }
            data.extend_from_slice(&0i16.to_le_bytes());
        }

        let pri_offset = data.len() - HEADER_LEN;
        for prim in prims {
            for w in *prim {
                data.extend_from_slice(&w.to_le_bytes());
            }
        }

        let descriptor = [
            vtx_offset as i32,
            vertices.len() as i32,
            nor_offset as i32,
            normals.len() as i32,
            pri_offset as i32,
            prims.len() as i32,
            0,
        ];
        for (slot, value) in descriptor.iter().enumerate() {
            let at = descriptor_at + slot * 4;
            data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        data
    }

    fn single_prim(tmd: &Tmd) -> &TmdPrim {
        assert_eq!(tmd.objects.len(), 1);
        assert_eq!(tmd.objects[0].prims.len(), 1);
        &tmd.objects[0].prims[0]
    }

    #[test]
    fn parses_gouraud_textured_triangle() {
        let prim = [
            GOURAUD_TEXTURED_TRIANGLE,
            (0x7800u32 << 16) | (10 << 8) | 1,
            (0x80u32 << 16) | (11 << 8) | 2,
            (12 << 8) | 3,
            0,
            (1 << 16) | 1,
            (2 << 16) | 2,
        ];
        let tmd = parse(&sample_tmd(&[&prim])).unwrap();

        let prim = single_prim(&tmd);
        assert_eq!(prim.vertices, [0, 1, 2]);
        assert_eq!(prim.normals, [0, 1, 2]);
        assert_eq!(prim.uv, [[1, 10], [2, 11], [3, 12]]);
        assert_eq!(prim.clut, 0x7800);
        assert_eq!(prim.tsb, 0x80);
        assert!(prim.textured);
        assert!(!prim.blend);
        assert_eq!(prim.flat_color, None);
        assert_eq!(prim.quad, None, "a triangle packet is no quad");
    }

    #[test]
    fn parses_blended_gouraud_textured_triangle() {
        let prim = [
            GOURAUD_TEXTURED_TRIANGLE_BLEND,
            (0x7800u32 << 16) | (1 << 8) | 1,
            (0x80u32 << 16) | (2 << 8) | 2,
            (3 << 8) | 3,
            0,
            (1 << 16) | 1,
            (2 << 16) | 2,
        ];
        let tmd = parse(&sample_tmd(&[&prim])).unwrap();

        let prim = single_prim(&tmd);
        assert!(prim.textured);
        assert!(prim.blend);
        assert_eq!(prim.vertices, [0, 1, 2]);
    }

    #[test]
    fn parses_flat_textured_triangle_with_one_normal() {
        // v0 = w4 hi, v1 = w5 lo, v2 = w5 hi; one normal in w4 lo.
        let prim = [
            FLAT_TEXTURED_TRIANGLE,
            (0x7800u32 << 16) | (0x40 << 8) | 5,
            (0x80u32 << 16) | (0x41 << 8) | 6,
            (0x42 << 8) | 7,
            (2 << 16) | 2,
            (1 << 16) | 0,
        ];
        let tmd = parse(&sample_tmd(&[&prim])).unwrap();

        let prim = single_prim(&tmd);
        assert_eq!(prim.vertices, [2, 0, 1]);
        assert_eq!(prim.normals, [2, 2, 2]);
        assert_eq!(prim.uv, [[5, 0x40], [6, 0x41], [7, 0x42]]);
        assert!(prim.textured);
        assert!(!prim.blend);
    }

    #[test]
    fn parses_raw_y_textured_triangle_with_the_implicit_normal() {
        // v0 = w5 lo, v1 = w5 hi, v2 = w6 lo; normal is (0, 0, -1).
        let prim = [
            TEXTURED_TRIANGLE_RAW_Y,
            (0x7800u32 << 16) | (9 << 8) | 8,
            (1 << 16) | (10 << 8) | 1,
            (2 << 16) | (11 << 8) | 2,
            0xDEAD_BEEF,
            (1 << 16) | 2,
            0,
            (3 << 16) | 1,
        ];
        let tmd = parse(&sample_tmd(&[&prim])).unwrap();

        assert_eq!(tmd.objects[0].normals.last(), Some(&RAW_Y_NORMAL));
        let prim = single_prim(&tmd);
        assert_eq!(prim.vertices, [2, 1, 0]);
        assert_eq!(prim.normals, [4, 4, 4]);
        assert_eq!(prim.uv, [[8, 9], [1, 10], [2, 11]]);
        assert!(prim.textured);
        assert!(prim.raw_y, "the 0x25010607 form carries unnegated Y");
    }

    #[test]
    fn parses_untextured_flat_colour_triangle() {
        // Colour in w1; vertices and normals share w2/w3/w4.
        let colour = 0x00_80_40u32;
        let prim = [
            GOURAUD_FLAT_UNTEXTURED_TRIANGLE,
            colour | (0x30 << 24),
            (1 << 16) | 0,
            (2 << 16) | 1,
            (0 << 16) | 2,
            0,
            0,
        ];
        let tmd = parse(&sample_tmd(&[&prim])).unwrap();

        let prim = single_prim(&tmd);
        assert_eq!(prim.vertices, [1, 2, 0]);
        assert_eq!(prim.normals, [0, 1, 2]);
        assert_eq!(prim.uv, [[0, 0]; 3]);
        assert!(!prim.textured);
        assert!(!prim.blend);
        assert_eq!(prim.flat_color, Some([0x40, 0x80, 0x00]));
    }

    #[test]
    fn parses_textured_gouraud_quad_into_two_triangles() {
        // Nine words: the command's bits 8-15 encode the 36-byte length.
        let prim = [
            GOURAUD_TEXTURED_QUAD,
            (0x7800u32 << 16) | (1 << 8) | 1,
            (0x80u32 << 16) | (2 << 8) | 2,
            (3 << 8) | 3,
            (4 << 8) | 4,
            (3 << 16) | 3,
            (2 << 16) | 2,
            (1 << 16) | 1,
            (0 << 16) | 0,
        ];
        let tmd = parse(&sample_tmd(&[&prim])).unwrap();

        assert_eq!(tmd.objects[0].prims.len(), 2);
        let first = &tmd.objects[0].prims[0];
        let second = &tmd.objects[0].prims[1];
        // The quad's corner ring is (0, 1, 3, 2).
        assert_eq!(first.vertices, [3, 2, 0]);
        assert_eq!(first.normals, [3, 2, 0]);
        assert_eq!(first.uv, [[1, 1], [2, 2], [4, 4]]);
        assert_eq!(second.vertices, [3, 0, 1]);
        assert_eq!(second.normals, [3, 0, 1]);
        assert_eq!(second.uv, [[1, 1], [4, 4], [3, 3]]);
        assert_eq!(first.clut, 0x7800);
        assert_eq!(first.tsb, 0x80);
        assert!(first.textured && second.textured);
        // Both halves carry the full ring so the renderer can drop the quad
        // whole when any corner is clipped.
        assert_eq!(first.quad, Some([3, 2, 1, 0]));
        assert_eq!(second.quad, Some([3, 2, 1, 0]));
    }

    #[test]
    fn parses_multi_packet_model() {
        let triangle = [
            GOURAUD_TEXTURED_TRIANGLE,
            0x7800_0000,
            0x80_0000,
            0,
            0,
            (1 << 16) | 1,
            (2 << 16) | 2,
        ];
        let flat = [
            GOURAUD_FLAT_UNTEXTURED_TRIANGLE,
            0x00_00_10,
            (1 << 16) | 0,
            (2 << 16) | 1,
            (0 << 16) | 2,
            0,
            0,
        ];
        let tmd = parse(&sample_tmd(&[&triangle, &flat])).unwrap();

        assert_eq!(tmd.objects[0].prims.len(), 2);
        assert!(tmd.objects[0].prims[0].textured);
        assert!(!tmd.objects[0].prims[1].textured);
    }

    #[test]
    fn parses_empty_mesh() {
        let mut data = vec![0u8; HEADER_LEN];
        data[0..4].copy_from_slice(&0x41u32.to_le_bytes());

        let tmd = parse(&data).unwrap();

        assert!(tmd.objects.is_empty());
    }

    #[test]
    fn rejects_truncated_header() {
        let err = parse(&[0u8; 8]).unwrap_err().to_string();
        assert!(err.contains("header"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_truncated_descriptor_table() {
        let data = sample_tmd(&[]);
        let err = parse(&data[..20]).unwrap_err().to_string();
        assert!(err.contains("descriptor"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_unknown_primitive_command() {
        let prim = [0x3400_0404, 0, 0, 0, 0, (1 << 16) | 1, (2 << 16) | 2];
        let err = parse(&sample_tmd(&[&prim])).unwrap_err().to_string();
        assert!(err.contains("0x34000404"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_out_of_range_vertex_offset() {
        let mut data = sample_tmd(&[]);
        data[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&1000i32.to_le_bytes());

        let err = parse(&data).unwrap_err().to_string();

        assert!(err.contains("truncated"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_out_of_range_vertex_index() {
        let prim = [
            GOURAUD_TEXTURED_TRIANGLE,
            0,
            0,
            0,
            0,
            (5u32 << 16) | 1,
            (2 << 16) | 2,
        ];
        let err = parse(&sample_tmd(&[&prim])).unwrap_err().to_string();

        assert!(err.contains("out of range"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_out_of_range_quad_corner() {
        // w1..w4 uv, w5..w8 vertices/normals; corner 3's vertex index is 9.
        let prim = [
            GOURAUD_TEXTURED_QUAD,
            0,
            0,
            0,
            0,
            (3u32 << 16) | 0,
            (2 << 16) | 1,
            (1 << 16) | 2,
            (9 << 16) | 0,
        ];
        let err = parse(&sample_tmd_with_vertex_count(&[&prim], 4))
            .unwrap_err()
            .to_string();

        assert!(err.contains("out of range"), "unexpected error: {err}");
    }
}
