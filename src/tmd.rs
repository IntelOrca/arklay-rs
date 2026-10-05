//! PSX TMD mesh parser.

use anyhow::{Context, Result, bail};

use crate::model::{Tmd, TmdObject, TmdPrim};

const HEADER_LEN: usize = 12;
const DESCRIPTOR_LEN: usize = 28;
const VERTEX_LEN: usize = 8;
const NORMAL_LEN: usize = 8;
const PRIM_LEN: usize = 28;
const GOURAUD_TEXTURED_TRIANGLE: u32 = 0x3400_0609;

/// Parse a TMD whose first byte is the TMD magic.
///
/// Every object descriptor has a vertex, normal and primitive list; all
/// offsets are relative to the descriptor table at `TMD + 0xC`. Only the
/// gouraud textured triangle packet (`0x34000609`) used by the shipped player
/// models is accepted; anything else is an error.
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
        let normals = parse_normals(data, field(2), field(3), index)?;
        let prims = parse_prims(data, field(4), field(5), &vertices, &normals, index)?;
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
    normals: &[[i16; 3]],
    object: usize,
) -> Result<Vec<TmdPrim>> {
    let count = checked_count(count, &format!("object {object} primitive"))?;
    let start = relative(offset, &format!("object {object} primitive offset"))?;
    let bytes = checked_slice(
        data,
        start,
        count
            .checked_mul(PRIM_LEN)
            .context("TMD primitive data overflows")?,
        &format!("object {object} primitive data"),
    )?;
    bytes
        .as_chunks::<PRIM_LEN>()
        .0
        .iter()
        .map(|packet| decode_prim(packet, vertices.len(), normals.len()))
        .collect()
}

fn decode_prim(
    packet: &[u8; PRIM_LEN],
    vertex_count: usize,
    normal_count: usize,
) -> Result<TmdPrim> {
    let word = |slot: usize| u32::from_le_bytes(packet[slot * 4..slot * 4 + 4].try_into().unwrap());
    let command = word(0);
    // TODO(parity): (gameplay) the original's TMD path also accepts the other
    // packet types (flat-textured, flat/gouraud untextured, quads); this parser
    // fails the whole model on ANY other command, so such an EMD/EMW does not
    // load at all instead of drawing.
    if command != GOURAUD_TEXTURED_TRIANGLE {
        bail!(
            "unsupported TMD primitive command 0x{command:08X}; only 0x{GOURAUD_TEXTURED_TRIANGLE:08X} is supported"
        );
    }

    let w1 = word(1);
    let w2 = word(2);
    let w3 = word(3);
    let w4 = word(4);
    let w5 = word(5);
    let w6 = word(6);

    let vertices = [(w4 >> 16) as u16, (w5 >> 16) as u16, (w6 >> 16) as u16];
    let normals = [
        (w4 & 0xFFFF) as u16,
        (w5 & 0xFFFF) as u16,
        (w6 & 0xFFFF) as u16,
    ];
    for (corner, &index) in vertices.iter().enumerate() {
        if index as usize >= vertex_count {
            bail!(
                "TMD primitive vertex index {index} (corner {corner}) is out of range ({vertex_count} vertices)"
            );
        }
    }
    for (corner, &index) in normals.iter().enumerate() {
        if index as usize >= normal_count {
            bail!(
                "TMD primitive normal index {index} (corner {corner}) is out of range ({normal_count} normals)"
            );
        }
    }

    Ok(TmdPrim {
        vertices,
        normals,
        uv: [
            [(w1 & 0xFF) as u8, ((w1 >> 8) & 0xFF) as u8],
            [(w2 & 0xFF) as u8, ((w2 >> 8) & 0xFF) as u8],
            [(w3 & 0xFF) as u8, ((w3 >> 8) & 0xFF) as u8],
        ],
        clut: (w1 >> 16) as u16,
        tsb: (w2 >> 16) as u16,
    })
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
    use super::*;

    fn sample_tmd(command: u32) -> Vec<u8> {
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
        let descriptor_at = data.len();
        data.extend_from_slice(&[0u8; DESCRIPTOR_LEN]);

        let vtx_offset = data.len() - HEADER_LEN;
        for v in vertices {
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
        for w in prim {
            data.extend_from_slice(&w.to_le_bytes());
        }

        let descriptor = [
            vtx_offset as i32,
            vertices.len() as i32,
            nor_offset as i32,
            normals.len() as i32,
            pri_offset as i32,
            1,
            0,
        ];
        for (slot, value) in descriptor.iter().enumerate() {
            let at = descriptor_at + slot * 4;
            data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        data
    }

    #[test]
    fn parses_single_triangle() {
        let tmd = parse(&sample_tmd(GOURAUD_TEXTURED_TRIANGLE)).unwrap();

        assert_eq!(tmd.objects.len(), 1);
        let object = &tmd.objects[0];
        assert_eq!(object.vertices, [[1, 2, 3], [4, 5, 6], [7, 8, 9]]);
        assert_eq!(object.normals, [[0, 0, 4096], [0, 4096, 0], [4096, 0, 0]]);
        assert_eq!(object.prims.len(), 1);
        let prim = &object.prims[0];
        assert_eq!(prim.vertices, [0, 1, 2]);
        assert_eq!(prim.normals, [0, 1, 2]);
        assert_eq!(prim.uv, [[1, 10], [2, 11], [3, 12]]);
        assert_eq!(prim.clut, 0x7800);
        assert_eq!(prim.tsb, 0x80);
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
        let data = sample_tmd(GOURAUD_TEXTURED_TRIANGLE);
        let err = parse(&data[..20]).unwrap_err().to_string();
        assert!(err.contains("descriptor"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_unknown_primitive_command() {
        let err = parse(&sample_tmd(0x3400_0404)).unwrap_err().to_string();
        assert!(err.contains("0x34000404"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_out_of_range_vertex_offset() {
        let mut data = sample_tmd(GOURAUD_TEXTURED_TRIANGLE);
        data[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&1000i32.to_le_bytes());

        let err = parse(&data).unwrap_err().to_string();

        assert!(err.contains("truncated"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_out_of_range_vertex_index() {
        let mut data = sample_tmd(GOURAUD_TEXTURED_TRIANGLE);
        let prim_at = data.len() - PRIM_LEN;
        data[prim_at + 16..prim_at + 20].copy_from_slice(&(5u32 << 16).to_le_bytes());

        let err = parse(&data).unwrap_err().to_string();

        assert!(err.contains("out of range"), "unexpected error: {err}");
    }
}
