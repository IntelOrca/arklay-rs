//! Shared player-model data types.
//!
//! These are plain parsed values: the parsers in [`crate::tmd`], [`crate::emd`]
//! and [`crate::tim`] produce them and the renderer consumes them. No file
//! format details leak into this module.

/// Number of palette entries in one row of a decoded texture; every decoder
/// normalizes its CLUT rows to this stride.
pub const PALETTE_ROW_LEN: usize = 256;

/// One triangle packet from a TMD primitive list.
///
/// Vertex and normal fields are indices into the owning [`TmdObject`]'s
/// `vertices` and `normals`. `uv[i]` is `[u, v]` for vertex `i`. The embedded
/// RDT models use six packet commands; a textured gouraud quad is split into
/// two of these triangles at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmdPrim {
    pub vertices: [u16; 3],
    pub normals: [u16; 3],
    pub uv: [[u8; 2]; 3],
    pub clut: u16,
    pub tsb: u16,
    /// Whether the primitive samples the texture page. An untextured packet
    /// (`0x30000406`) paints [`TmdPrim::flat_color`] instead.
    pub textured: bool,
    /// The packet's semi-transparency command bit (`0x36000609`). The
    /// renderer's fallback is a flat half blend.
    pub blend: bool,
    /// The packet stores its vertices with an unnegated Y (`0x25010607`). The
    /// render path skips the standard Y conjugation for these vertices; every
    /// other form negates Y like the PSX vertex reader does.
    pub raw_y: bool,
    /// The packet colour of an untextured packet, `None` when textured.
    pub flat_color: Option<[u8; 3]>,
}

/// A TMD object: one vertex pool, one normal pool and the primitives using them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TmdObject {
    pub vertices: Vec<[i16; 3]>,
    pub normals: Vec<[i16; 3]>,
    pub prims: Vec<TmdPrim>,
}

/// A parsed TMD mesh, usually one animated body part per object.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tmd {
    pub objects: Vec<TmdObject>,
}

/// An 8bpp indexed texture with its palettes expanded to RGBA8.
///
/// `indices` is row-major, `width * height` bytes. `palettes` is row-major too:
/// `height`-independent CLUT rows of 256 entries each (`h` rows in file order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture8 {
    pub width: u32,
    pub height: u32,
    pub indices: Vec<u8>,
    pub palettes: Vec<[u8; 4]>,
}

impl Texture8 {
    /// Look up one palette entry; out-of-range lookups are black.
    pub fn palette(&self, row: usize, index: u8) -> [u8; 4] {
        self.palettes
            .get(row * PALETTE_ROW_LEN + index as usize)
            .copied()
            .unwrap_or([0, 0, 0, 0])
    }
}

/// The joint hierarchy: relative joint positions and the child lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Skeleton {
    pub relative: Vec<[i16; 3]>,
    pub children: Vec<Vec<u8>>,
}

/// One animation keyframe: a root translation plus a rotation per joint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Keyframe {
    pub offset: [i16; 3],
    pub rotations: Vec<[i16; 3]>,
}

/// One entry in an animation clip: which keyframe to show and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipFrame {
    pub keyframe: u16,
    pub timing: u16,
}

/// An animation clip: an ordered list of keyframe/timing pairs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Clip {
    pub frames: Vec<ClipFrame>,
}

/// A complete EMD player/enemy model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emd {
    pub skeleton: Skeleton,
    pub keyframes: Vec<Keyframe>,
    pub clips: Vec<Clip>,
    pub mesh: Tmd,
    pub texture: Texture8,
}

/// The RDT-embedded player animation pair (pointer slots 9 and 10): an EMR
/// armature/keyframe header plus an EDD clip table. The clips are the
/// room-specific push, vault and ladder motions the original drives with
/// `Joint_move` through `jointMoveData2`/`jointMoveData3`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoomAnim {
    pub skeleton: Skeleton,
    pub keyframes: Vec<Keyframe>,
    pub clips: Vec<Clip>,
}

/// A no-weapon EMW animation + mesh file. The EMW carries its own armature and
/// keyframes; its locomotion clips drive the same 15-joint skeleton as the EMD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emw {
    pub skeleton: Skeleton,
    pub keyframes: Vec<Keyframe>,
    pub clips: Vec<Clip>,
    pub mesh: Tmd,
}
