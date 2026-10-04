//! PSX `.dor` door-animation container parser.
//!
//! A `.dor` file is the whole 3D door/stairs/lift transition: a small bytecode
//! program per animation phase, a TMD mesh of the moving panels and an 8bpp
//! texture. The transition layer parses the file with [`parse`] and drives the
//! bytecode with [`vm::Vm`].
//!
//! Layout (all header fields are little-endian `u32` offsets into the file):
//!
//! ```text
//! 0x00  script_table  offset of the NULL-terminated script pointer table
//! 0x04  tmd           offset of the TMD mesh header
//! 0x08  tim           offset of the 8bpp TIM texture
//! ```
//!
//! The script table holds `i32` offsets relative to the table itself and ends
//! with a zero entry. Entry 0 is the main script; the VM activates the other
//! entries with its `ACTIVATE` opcode. The mesh is a normal PSX TMD parsed by
//! [`crate::tmd::parse`], and the texture is a 128x256 8bpp TIM with a single
//! 256-colour CLUT row, decoded by [`crate::tim::decode_8bpp`].

pub mod vm;

use anyhow::{Context, Result, bail};

use crate::model::{Texture8, Tmd};
use crate::{tim, tmd};

/// Number of animated panel/order slots in a door file.
pub const ORDER_COUNT: usize = 12;
/// Sanity bound on the number of scripts in the NULL-terminated table.
const MAX_SCRIPTS: usize = 0x100;

/// `.dor` file stem for each door-type byte, `0x00..=0x21`.
///
/// The record byte `+0x0A` selects the animation: plain doors `00`-`0E`,
/// the monitor and elevator cages, the four stairwells, the two ladders and
/// the key-hole door variants. Types `>= 0x22` fall back to `door00`.
static DOOR_TYPE_NAMES: [&str; 0x22] = [
    "door00", "door01", "door02", "door03", "door04", "door05", "door06", "door07", "door08",
    "door09", "door10", "door11", "door12", "door13", "door14", "mon", "ele03", "ele01", "ele01a",
    "ele01b", "ele02", "ele04", "kai01", "kai03", "kai02", "kai04", "lad00", "lad01", "door00k",
    "door01k", "door03k", "door05k", "door06k", "door15",
];

/// The pack entry stem (`door/{name}.dor`) for a door-type byte.
///
/// Values outside the shipped table use `door00`, matching the original's
/// default animation selection.
pub fn type_name(door_type: u8) -> &'static str {
    DOOR_TYPE_NAMES
        .get(usize::from(door_type))
        .copied()
        .unwrap_or(DOOR_TYPE_NAMES[0])
}

/// One script from the file's pointer table.
///
/// `bytes` is the raw bytecode, starting at the script's first opcode and
/// running to the next script in the table (or to the TMD header for the last
/// one). `offset` is the byte offset of the first opcode in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// File offset of the first byte of the script.
    pub offset: usize,
    /// Raw bytecode bytes.
    pub bytes: Vec<u8>,
}

/// One of the 12 order (panel) slots in its pristine, pre-script state.
///
/// The scripts overwrite these at runtime with `ORDER_SETUP`; the parser
/// exposes the zeroed initial table so callers can see the slot layout. The
/// live state lives in [`vm::Vm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Order {
    /// Draw/rotate flags written by `ORDER_SETUP`/`ORDER_FLAGS`. Bit `0x4000`
    /// rebuilds the rotation, bit `0x8000` draws, and a low nibble of 1, 2 or 3
    /// submits a draw.
    pub flags: u16,
    /// TMD object index used by this order.
    pub model: u8,
    /// Parent order index for the hierarchical matrix, `None` for a root.
    pub parent: Option<u8>,
}

/// A parsed `.dor` door animation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dor {
    /// Script table in file order; entry 0 is the main script.
    pub scripts: Vec<Script>,
    /// The door's TMD mesh (one object per model index used by the orders).
    pub mesh: Tmd,
    /// The door's 8bpp 128x256 texture.
    pub texture: Texture8,
    /// The 12 order slots in their initial (zeroed) state.
    pub orders: [Order; ORDER_COUNT],
}

impl Dor {
    /// Total number of triangles across every TMD object.
    pub fn triangle_count(&self) -> usize {
        self.mesh
            .objects
            .iter()
            .map(|object| object.prims.len())
            .sum()
    }
}

/// Parse a `.dor` file.
///
/// Fails when the header is truncated, the script table is missing its
/// terminator or points outside the file, or the embedded TMD/TIM are invalid.
pub fn parse(data: &[u8]) -> Result<Dor> {
    if data.len() < 12 {
        bail!(
            "door header is truncated: {} bytes, need at least 12",
            data.len()
        );
    }
    let script_table = read_u32(data, 0)? as usize;
    let tmd_offset = read_u32(data, 4)? as usize;
    let tim_offset = read_u32(data, 8)? as usize;

    let scripts = parse_scripts(data, script_table, tmd_offset)?;
    if scripts.is_empty() {
        bail!("door script table is empty");
    }

    let mesh = tmd::parse(
        data.get(tmd_offset..)
            .context("door TMD offset is outside the file")?,
    )
    .context("door TMD is invalid")?;
    let texture = tim::decode_8bpp(
        data.get(tim_offset..)
            .context("door TIM offset is outside the file")?,
    )
    .context("door TIM is invalid")?;

    Ok(Dor {
        scripts,
        mesh,
        texture,
        orders: [Order::default(); ORDER_COUNT],
    })
}

/// Read the NULL-terminated script table and slice every script.
///
/// Entries are `i32` offsets relative to the table base; scripts must start in
/// file order and before the TMD header, so each script runs to the next one's
/// start (the last one to the TMD header).
fn parse_scripts(data: &[u8], table: usize, tmd_offset: usize) -> Result<Vec<Script>> {
    if table >= data.len() {
        bail!("door script table offset 0x{table:X} is outside the file");
    }
    if tmd_offset > data.len() || tmd_offset < table {
        bail!("door TMD offset 0x{tmd_offset:X} is outside the script region");
    }

    let mut starts = Vec::new();
    let mut cursor = table;
    loop {
        let entry = read_u32(data, cursor)
            .with_context(|| format!("door script table at 0x{table:X} has no NULL terminator"))?;
        if entry == 0 {
            break;
        }
        if starts.len() >= MAX_SCRIPTS {
            bail!("door script table at 0x{table:X} has too many entries");
        }
        let start = table
            .checked_add(entry as usize)
            .context("door script offset overflows")?;
        if start < table || start >= tmd_offset {
            bail!(
                "door script {start:#X} is outside the script region 0x{table:X}..0x{tmd_offset:X}"
            );
        }
        starts.push(start);
        cursor += 4;
    }

    if starts.windows(2).any(|pair| pair[0] >= pair[1]) {
        bail!("door script table at 0x{table:X} is not in file order");
    }

    let mut scripts = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(tmd_offset);
        scripts.push(Script {
            offset: start,
            bytes: data[start..end].to_vec(),
        });
    }
    Ok(scripts)
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .with_context(|| format!("truncated u32 at door offset 0x{offset:X}"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TMD_MAGIC: u32 = 0x41;
    const PRIM: u32 = 0x3400_0609;
    const DESCRIPTOR_LEN: usize = 28;

    /// A one-object, one-triangle TMD in the layout `crate::tmd::parse` reads.
    fn tmd_triangle() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&TMD_MAGIC.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        let descriptor_at = data.len();
        data.extend_from_slice(&[0u8; DESCRIPTOR_LEN]);

        let vertices = [[0i16, 0, 100], [100, 0, 100], [0, 100, 100]];
        let vertex_offset = data.len() - 12;
        for vertex in vertices {
            for channel in vertex {
                data.extend_from_slice(&channel.to_le_bytes());
            }
            data.extend_from_slice(&0i16.to_le_bytes());
        }
        let normals = [[0i16, 0, 4096], [4096, 0, 0], [0, 4096, 0]];
        let normal_offset = data.len() - 12;
        for normal in normals {
            for channel in normal {
                data.extend_from_slice(&channel.to_le_bytes());
            }
            data.extend_from_slice(&0i16.to_le_bytes());
        }

        let prim_offset = data.len() - 12;
        for word in [
            PRIM,
            (0x7800u32 << 16) | (10 << 8) | 1,
            (0x80u32 << 16) | (11 << 8) | 2,
            (12 << 8) | 3,
            0,
            (1 << 16) | 1,
            (2 << 16) | 2,
        ] {
            data.extend_from_slice(&word.to_le_bytes());
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

    /// A 2x1 8bpp TIM with a two-entry CLUT.
    fn tim_8bpp() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0x10u32.to_le_bytes());
        data.extend_from_slice(&(1u32 | (1 << 3)).to_le_bytes());
        data.extend_from_slice(&(12u32 + 4).to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&480i16.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&0x001Fu16.to_le_bytes());
        data.extend_from_slice(&0x7C00u16.to_le_bytes());
        data.extend_from_slice(&(12u32 + 2).to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&[0u8, 1u8]);
        data
    }

    /// Assemble a whole synthetic `.dor` from script bodies.
    fn build_dor(scripts: &[&[u8]]) -> Vec<u8> {
        let mut data = vec![0u8; 12];
        let table = data.len();
        data.extend_from_slice(&vec![0u8; (scripts.len() + 1) * 4]);

        let mut offsets = Vec::new();
        for script in scripts {
            offsets.push((data.len() - table) as i32);
            data.extend_from_slice(script);
        }
        let tmd_offset = data.len();
        data.extend_from_slice(&tmd_triangle());
        let tim_offset = data.len();
        data.extend_from_slice(&tim_8bpp());

        data[0..4].copy_from_slice(&(table as u32).to_le_bytes());
        data[4..8].copy_from_slice(&(tmd_offset as u32).to_le_bytes());
        data[8..12].copy_from_slice(&(tim_offset as u32).to_le_bytes());
        for (index, offset) in offsets.iter().enumerate() {
            let at = table + index * 4;
            data[at..at + 4].copy_from_slice(&offset.to_le_bytes());
        }
        data
    }

    #[test]
    fn door_type_table_maps_every_shipped_kind() {
        assert_eq!(type_name(0x00), "door00");
        assert_eq!(type_name(0x0E), "door14");
        assert_eq!(type_name(0x0F), "mon");
        for (byte, name) in [
            (0x10, "ele03"),
            (0x11, "ele01"),
            (0x12, "ele01a"),
            (0x13, "ele01b"),
            (0x14, "ele02"),
            (0x15, "ele04"),
            (0x16, "kai01"),
            (0x17, "kai03"),
            (0x18, "kai02"),
            (0x19, "kai04"),
            (0x1A, "lad00"),
            (0x1B, "lad01"),
            (0x1C, "door00k"),
            (0x1D, "door01k"),
            (0x1E, "door03k"),
            (0x1F, "door05k"),
            (0x20, "door06k"),
            (0x21, "door15"),
            (0x22, "door00"),
            (0xFF, "door00"),
        ] {
            assert_eq!(type_name(byte), name, "door type {byte:#04x}");
        }

        // The shipped table has one distinct stem per animated kind.
        let mut names: Vec<&str> = (0..=0x21).map(type_name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 34);
    }

    #[test]
    fn parses_scripts_mesh_texture_and_orders() {
        let data = build_dor(&[&[0x00, 0x00], &[0x03, 0x00, 0x00, 0x00]]);

        let dor = parse(&data).unwrap();

        assert_eq!(dor.scripts.len(), 2);
        assert_eq!(dor.scripts[0].bytes, [0x00, 0x00]);
        assert_eq!(dor.scripts[1].bytes, [0x03, 0x00, 0x00, 0x00]);
        assert_eq!(dor.mesh.objects.len(), 1);
        assert_eq!(dor.mesh.objects[0].prims.len(), 1);
        assert_eq!(dor.mesh.objects[0].vertices[1], [100, 0, 100]);
        assert_eq!(dor.texture.width, 2);
        assert_eq!(dor.texture.height, 1);
        assert_eq!(dor.texture.palette(0, 0), [255, 0, 0, 255]);
        assert_eq!(dor.triangle_count(), 1);
        assert_eq!(dor.orders, [Order::default(); ORDER_COUNT]);
        assert!(dor.scripts[0].offset >= 12);
    }

    #[test]
    fn reports_the_header_offsets_in_the_script_offsets() {
        let data = build_dor(&[&[0x00, 0x00]]);
        let dor = parse(&data).unwrap();
        assert_eq!(
            dor.scripts[0].offset,
            read_u32(&data, 0).unwrap() as usize + 8
        );
    }

    #[test]
    fn rejects_a_truncated_header() {
        let err = parse(&[0u8; 8]).unwrap_err().to_string();
        assert!(err.contains("header"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_an_out_of_range_script_table() {
        let mut data = build_dor(&[&[0x00, 0x00]]);
        data[0..4].copy_from_slice(&0xFFFFu32.to_le_bytes());
        let err = parse(&data).unwrap_err().to_string();
        assert!(err.contains("script table"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_an_unterminated_script_table() {
        let mut data = build_dor(&[&[0x00, 0x00]]);
        let table = read_u32(&data, 0).unwrap() as usize;
        // Overwrite the NULL terminator with an offset past the TMD.
        let at = table + 4;
        data[at..at + 4].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        let err = parse(&data).unwrap_err().to_string();
        assert!(err.contains("outside"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_out_of_order_scripts() {
        let mut data = build_dor(&[&[0x00, 0x00], &[0x03, 0x00, 0x00, 0x00]]);
        let table = read_u32(&data, 0).unwrap() as usize;
        let first = i32::from_le_bytes(data[table..table + 4].try_into().unwrap());
        let second_at = table + 4;
        data[second_at..second_at + 4].copy_from_slice(&(first - 4).to_le_bytes());
        let err = parse(&data).unwrap_err().to_string();
        assert!(err.contains("file order"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_an_out_of_range_tmd() {
        let mut data = build_dor(&[&[0x00, 0x00]]);
        data[4..8].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        let err = parse(&data).unwrap_err().to_string();
        assert!(err.contains("TMD"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_an_out_of_range_tim() {
        let mut data = build_dor(&[&[0x00, 0x00]]);
        data[8..12].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        let err = parse(&data).unwrap_err().to_string();
        assert!(err.contains("TIM"), "unexpected error: {err}");
    }
}
