//! CPU-side packing for the GPU renderer: the static per-mesh vertex
//! buffers, the per-draw uniform blocks and the frame's screen-space sprite
//! list. Nothing here touches SDL, so the maths is unit-tested without a GPU.
//!
//! The conventions mirror the software rasterizer in [`crate::render`]:
//!
//! * A mesh vertex is stored with the input Y conjugation already applied
//!   (`D v`, or `v` for a raw-Y packet), so a joint maps it with the rows
//!   `(R0, t0)`, `(-R1, t1)`, `(R2, t2)` exactly like `fixed_mul`.
//! * The cut camera projects `sx = vx*fov/vz + 160 + shake_x` and
//!   `sy = 120 + shake_y - vy*fov/vz`; the shader writes that as a clip-space
//!   position with `w = vz`, so the hardware divide reproduces it.
//! * Depth is the hyperbolic `far * (vz - near) / ((far - near) * vz)` with
//!   `near = 2 * fov`, the software near plane. Sprites without geometry
//!   (masks, effects, shadows) take the same mapping of their painter's key,
//!   which is a view-Z in the same units.

use crate::anim::Mat4x3;
use crate::model::{Texture8, Tmd};
use crate::render::{
    self, ABE_BIT, Camera, EntityLight, EntityMesh, GameplayScene, MirrorPass, apply_matrix_sv,
    mul_matrix0, stored_camera_matrix,
};
use crate::state::Image;

/// The logical frame the software rasterizer draws.
pub const LOGICAL_WIDTH: f32 = 320.0;
/// The logical frame the software rasterizer draws.
pub const LOGICAL_HEIGHT: f32 = 240.0;
/// The far plane of the depth mapping, in view-Z units; comfortably past any
/// room (the 16-bit world never reaches it).
pub const FAR_PLANE: f32 = 1_048_576.0;
/// The joints one draw can address: the software path's `hidden_joints` mask
/// is 32 bits, and no shipped model has more objects.
pub const MAX_JOINTS: usize = 32;

/// `in_packed` bit: the packet is semi-transparent (ABE or the TMD flag).
const PACKED_BLEND: u32 = 1 << 24;
/// `in_packed` bit: an untextured packet painting its colour word.
const PACKED_UNTEXTURED: u32 = 1 << 25;

/// One mesh corner in the static vertex buffer (48 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshVertex {
    /// Stored vertex with the input Y conjugation applied.
    pub position: [f32; 3],
    /// Stored normal with its Y negated (the reference's `PSXObjVtx` form).
    pub normal: [f32; 3],
    /// Texel coordinates including the packet's texture-page offset.
    pub uv: [f32; 2],
    /// An untextured packet's colour word / 1024; white otherwise.
    pub colour: [f32; 3],
    /// `joint | palette_row << 8 | blend << 24 | untextured << 25`.
    pub packed: u32,
}

/// A mesh's static vertex buffer contents: every packet expanded to
/// triangles, the semi-transparent packets last so the blend pass can draw
/// just `blend_start..vertices.len()`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeshVertices {
    pub vertices: Vec<MeshVertex>,
    pub blend_start: u32,
}

/// Expand a TMD into its static vertex buffer.
///
/// Matches `collect_triangles` on the gameplay path: the page offsets come
/// from the texpage word, the palette row from the CLUT word, a zero or
/// missing corner normal borrows the packet's first real one and a packet with
/// no usable normal is dropped, as is one with an out-of-range vertex.
pub fn mesh_vertices(mesh: &Tmd) -> MeshVertices {
    let mut opaque = Vec::new();
    let mut blended = Vec::new();
    for (joint, object) in mesh.objects.iter().enumerate() {
        let joint = joint.min(0xFF) as u32;
        for prim in &object.prims {
            let corners = prim
                .vertices
                .map(|index| object.vertices.get(usize::from(index)));
            let [Some(v0), Some(v1), Some(v2)] = corners else {
                continue;
            };
            let mut normals = prim
                .normals
                .map(|index| object.normals.get(usize::from(index)).copied());
            let Some(substitute) = normals
                .iter()
                .flatten()
                .copied()
                .find(|normal| normal != &[0; 3])
            else {
                continue;
            };
            for normal in &mut normals {
                if normal.is_none() || normal == &Some([0; 3]) {
                    *normal = Some(substitute);
                }
            }
            let blend = prim.blend || prim.clut & ABE_BIT != 0;
            let page_x = f32::from(prim.tsb & 0xF) * 128.0;
            let page_y = f32::from((prim.tsb >> 4) & 1) * 256.0;
            let row = u32::from(((prim.clut >> 6) & 0x01FF).saturating_sub(480));
            let colour = prim
                .flat_color
                .map_or([1.0; 3], |colour| colour.map(|c| f32::from(c) / 1024.0));
            let mut packed = joint | (row & 0xFFFF) << 8;
            if blend {
                packed |= PACKED_BLEND;
            }
            if prim.flat_color.is_some() {
                packed |= PACKED_UNTEXTURED;
            }
            let target = if blend { &mut blended } else { &mut opaque };
            for (corner, vertex) in [v0, v1, v2].into_iter().enumerate() {
                let y = if prim.raw_y { vertex[1] } else { -vertex[1] };
                let normal = normals[corner].unwrap_or(substitute);
                target.push(MeshVertex {
                    position: [f32::from(vertex[0]), f32::from(y), f32::from(vertex[2])],
                    normal: [
                        f32::from(normal[0]),
                        -f32::from(normal[1]),
                        f32::from(normal[2]),
                    ],
                    uv: [
                        f32::from(prim.uv[corner][0]) + page_x,
                        f32::from(prim.uv[corner][1]) + page_y,
                    ],
                    colour,
                    packed,
                });
            }
        }
    }
    let blend_start = opaque.len() as u32;
    opaque.extend(blended);
    MeshVertices {
        vertices: opaque,
        blend_start,
    }
}

/// The vertex-stage uniform block of one mesh draw (`mesh.vert`'s `Draw`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawUniforms {
    pub view: [[f32; 4]; 3],
    /// `fov, shake_x, shake_y, near`.
    pub projection: [f32; 4],
    /// `far, 0, 0, 0`.
    pub depth: [f32; 4],
    pub ambient: [f32; 4],
    pub light_colour: [[f32; 4]; 3],
    pub tint: [f32; 4],
    /// `hidden_joints, 0, 0, 0`.
    pub flags: [u32; 4],
    /// Per joint: the three world rows, then the three light vectors.
    pub joints: [[f32; 4]; MAX_JOINTS * 6],
}

impl DrawUniforms {
    /// The block as bytes for `SDL_PushGPUVertexUniformData`.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `DrawUniforms` is `repr(C)` and made only of 4-byte scalars,
        // so it has no padding and every byte is initialized.
        unsafe {
            std::slice::from_raw_parts(
                (self as *const Self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// The view rows and projection terms shared by every draw through `camera`.
fn camera_terms(camera: &Camera) -> ([[f32; 4]; 3], [f32; 4]) {
    let view = std::array::from_fn(|row| {
        let r = camera.view[row];
        [
            r[0] as f32 / 4096.0,
            r[1] as f32 / 4096.0,
            r[2] as f32 / 4096.0,
            camera.trans[row] as f32,
        ]
    });
    let fov = camera.fov as f32;
    (
        view,
        [
            fov,
            camera.screen[0] as f32,
            camera.screen[1] as f32,
            2.0 * fov,
        ],
    )
}

/// The per-joint light vector the shader dots with the stored normal.
///
/// `reference_vertex_shade` computes `d = -(n_view . l) / 4096` with
/// `n_view = C R n' / 4096^3` and `l = ApplyMatrixSV(C, dir)`; folding the
/// matrices onto the light gives `d = -dot(n', (C R)^T l / 4096^4)`, so the
/// GPU evaluates one dot product per light and corner.
fn light_vector(
    stored: &[[i32; 3]; 3],
    joint: &Mat4x3,
    dir: [i32; 3],
    room_object: bool,
) -> [f32; 4] {
    let vec = if room_object {
        mul_matrix0(&[dir, [0; 3], [0; 3]], &joint.r)[0]
    } else {
        dir
    };
    let l = apply_matrix_sv(stored, vec);
    let mut out = [0.0f32; 4];
    for (k, slot) in out.iter_mut().take(3).enumerate() {
        let mut sum = 0.0f64;
        for (row, &l_row) in l.iter().enumerate() {
            let m: f64 = (0..3)
                .map(|j| f64::from(stored[row][j]) * f64::from(joint.r[j][k]))
                .sum();
            sum += m * f64::from(l_row);
        }
        *slot = (sum / 4096f64.powi(4)) as f32;
    }
    out
}

/// The uniform block of one mesh draw through `camera`.
///
/// `room_object` selects the room pass's light rotation (the software
/// `room_object` flag; never set for the mirror copy). `mirror` hides the
/// joints the mirror probe rejects, on top of the mesh's own hidden joints.
pub fn draw_uniforms(
    camera: &Camera,
    mesh: &EntityMesh<'_>,
    light: &EntityLight,
    room_object: bool,
    mirror: Option<&MirrorPass>,
) -> DrawUniforms {
    let (view, projection) = camera_terms(camera);
    let stored = stored_camera_matrix(camera.from, camera.look_at);
    let mut joints = [[0.0f32; 4]; MAX_JOINTS * 6];
    let mut hidden = 0u32;
    for index in 0..MAX_JOINTS {
        let joint = mesh.joints.get(index);
        let visible = index < mesh.mesh.objects.len()
            && mesh.joint_visible(index)
            && joint.is_some_and(|joint| mirror.is_none_or(|mirror| mirror.joint_visible(joint.t)));
        let Some(joint) = joint.filter(|_| visible) else {
            hidden |= 1 << index;
            continue;
        };
        let base = index * 6;
        for row in 0..3 {
            let sign = if row == 1 { -1.0 } else { 1.0 };
            let r = joint.r[row];
            joints[base + row] = [
                sign * r[0] as f32 / 4096.0,
                sign * r[1] as f32 / 4096.0,
                sign * r[2] as f32 / 4096.0,
                joint.t[row] as f32,
            ];
        }
        for (slot, source) in light.sources.iter().enumerate() {
            joints[base + 3 + slot] = light_vector(&stored, joint, source.dir, room_object);
        }
    }
    let ambient = light.ambient.map(|channel| channel as f32);
    DrawUniforms {
        view,
        projection,
        depth: [FAR_PLANE, 0.0, 0.0, 0.0],
        ambient: [ambient[0], ambient[1], ambient[2], 0.0],
        light_colour: light.sources.map(|source| {
            let colour = source
                .color
                .map(|channel| f32::from(channel) * 255.0 / 128.0);
            [colour[0], colour[1], colour[2], 0.0]
        }),
        tint: [
            f32::from(mesh.tint[0]),
            f32::from(mesh.tint[1]),
            f32::from(mesh.tint[2]),
            0.0,
        ],
        flags: [hidden, 0, 0, 0],
        joints,
    }
}

/// The clip-space position `mesh.vert` computes for a view-space point, for
/// tests and the CPU-side sprite path.
pub fn clip_position(view: [f32; 3], fov: f32, shake: [f32; 2]) -> [f32; 4] {
    let near = 2.0 * fov;
    [
        (view[0] * fov + shake[0] * view[2]) / 160.0,
        (view[1] * fov - shake[1] * view[2]) / 120.0,
        FAR_PLANE * (view[2] - near) / (FAR_PLANE - near),
        view[2],
    ]
}

/// The normalized depth of a painter's key (a view-Z), clamped to the depth
/// range: the depth a mesh fragment at that view-Z would have.
pub fn key_depth(key: u32, fov: i32) -> f32 {
    let near = 2.0 * fov as f32;
    let z = key as f32;
    if z <= near {
        return 0.0;
    }
    (FAR_PLANE * (z - near) / ((FAR_PLANE - near) * z)).clamp(0.0, 1.0)
}

/// The fragment-stage uniform block of a mesh draw (`mesh.frag`'s
/// `Material`): `x` the background weight of a blended texel, `y` the pass
/// (0 opaque, 1 blend).
pub fn material(blend_weight: Option<u8>, blend_pass: bool) -> [f32; 4] {
    let weight = f32::from(blend_weight.unwrap_or(0x80)) / 256.0;
    [weight, if blend_pass { 1.0 } else { 0.0 }, 0.0, 0.0]
}

/// `sprite.frag` modes.
pub const SPRITE_TEXTURED: u32 = 0;
pub const SPRITE_SHADOW: u32 = 1;
pub const SPRITE_BACKGROUND: u32 = 2;

/// One screen-space sprite corner (44 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpriteVertex {
    pub clip: [f32; 4],
    pub uv: [f32; 2],
    pub colour: [f32; 4],
    pub mode: u32,
}

/// A contiguous run of sprite triangles sampling one image.
#[derive(Debug, Clone, Copy)]
pub struct SpriteRun<'a> {
    pub image: &'a Image,
    pub first: u32,
    pub count: u32,
    /// Masks write depth so the meshes behind them are hidden; everything else
    /// only tests it.
    pub depth_write: bool,
}

/// The frame's sprites: `before` draws ahead of the meshes (background,
/// masks, shadows), `after` behind them (the effect billboards).
#[derive(Debug, Default)]
pub struct Sprites<'a> {
    pub vertices: Vec<SpriteVertex>,
    pub before: Vec<SpriteRun<'a>>,
    pub after: Vec<SpriteRun<'a>>,
}

/// A logical pixel position to normalized device coordinates.
fn ndc(x: f32, y: f32) -> [f32; 2] {
    [
        x / (LOGICAL_WIDTH / 2.0) - 1.0,
        1.0 - y / (LOGICAL_HEIGHT / 2.0),
    ]
}

impl<'a> Sprites<'a> {
    /// Append one screen rectangle as two triangles.
    fn push_rect(&mut self, rect: [f32; 4], uv: [f32; 4], depth: f32, colour: [f32; 4], mode: u32) {
        let [x0, y0, x1, y1] = rect;
        let corner = |x: f32, y: f32, u: f32, v: f32| {
            let [nx, ny] = ndc(x, y);
            SpriteVertex {
                clip: [nx, ny, depth, 1.0],
                uv: [u, v],
                colour,
                mode,
            }
        };
        let a = corner(x0, y0, uv[0], uv[1]);
        let b = corner(x1, y0, uv[2], uv[1]);
        let c = corner(x0, y1, uv[0], uv[3]);
        let d = corner(x1, y1, uv[2], uv[3]);
        self.vertices.extend([a, b, c, c, b, d]);
    }

    /// Close the vertices pushed since `first` into a run.
    fn run(&self, image: &'a Image, first: usize, depth_write: bool) -> Option<SpriteRun<'a>> {
        let count = self.vertices.len() - first;
        (count > 0).then_some(SpriteRun {
            image,
            first: first as u32,
            count: count as u32,
            depth_write,
        })
    }
}

/// Build the frame's background, mask, shadow and effect sprites.
pub fn scene_sprites<'a>(scene: &GameplayScene<'a>) -> Sprites<'a> {
    let mut sprites = Sprites::default();
    let camera = scene.camera;

    // The background is stretched over the logical frame with its display
    // origin shifted in (`Framebuffer::draw_background`), at the far plane.
    if let Some(image) = scene
        .background
        .filter(|image| image.width > 0 && image.height > 0)
    {
        let first = sprites.vertices.len();
        let (w, h) = (image.width as f32, image.height as f32);
        let (ox, oy) = (scene.origin[0] as f32, scene.origin[1] as f32);
        let colour = scene.colour.map(|channel| f32::from(channel) / 255.0);
        sprites.push_rect(
            [0.0, 0.0, LOGICAL_WIDTH, LOGICAL_HEIGHT],
            [
                -ox * w / LOGICAL_WIDTH,
                -oy * h / LOGICAL_HEIGHT,
                (LOGICAL_WIDTH - ox) * w / LOGICAL_WIDTH,
                (LOGICAL_HEIGHT - oy) * h / LOGICAL_HEIGHT,
            ],
            1.0,
            [colour[0], colour[1], colour[2], 1.0],
            SPRITE_BACKGROUND,
        );
        sprites.before.extend(sprites.run(image, first, false));
    }

    // The masks are opaque cut-outs at their painter's key; they write depth
    // so the model fragments behind them fail the test.
    if let Some(layer) = scene.mask_layer {
        let first = sprites.vertices.len();
        for quad in render::mask_quads(layer, camera.screen) {
            let (x, y) = (quad.pos.0 as f32, quad.pos.1 as f32);
            let (w, h) = (quad.size.0 as f32, quad.size.1 as f32);
            let (u, v) = (quad.uv.0 as f32, quad.uv.1 as f32);
            sprites.push_rect(
                [x, y, x + w, y + h],
                [u, v, u + w, v + h],
                key_depth(quad.key, camera.fov),
                [1.0; 4],
                SPRITE_TEXTURED,
            );
        }
        sprites.before.extend(sprites.run(layer.page, first, true));
    }

    // The shadows keep their perspective-correct UVs: each projected corner
    // goes back to clip space with `w` = its view Z. They blend towards the
    // tint by the page's coverage alpha, far to near.
    let mut shadows: Vec<_> = scene
        .shadows
        .iter()
        .filter_map(|shadow| render::shadow_polygon(shadow, camera))
        .collect();
    shadows.sort_by_key(|poly| std::cmp::Reverse(poly.key));
    for poly in &shadows {
        let first = sprites.vertices.len();
        let depth = key_depth(poly.key, camera.fov);
        let colour = poly.tint.map(|channel| f32::from(channel) / 255.0);
        let corner = |index: usize| {
            let vertex = &poly.corners[index];
            let w = (1.0 / vertex.inv_z) as f32;
            let [nx, ny] = ndc(vertex.position[0] as f32, vertex.position[1] as f32);
            SpriteVertex {
                clip: [nx * w, ny * w, depth * w, w],
                uv: [vertex.u as f32, vertex.v as f32],
                colour: [colour[0], colour[1], colour[2], 1.0],
                mode: SPRITE_SHADOW,
            }
        };
        for index in 1..poly.corners.len() - 1 {
            sprites
                .vertices
                .extend([corner(0), corner(index), corner(index + 1)]);
        }
        sprites
            .before
            .extend(sprites.run(poly.texture, first, false));
    }

    // The effect billboards draw after the meshes, far to near, with the
    // tint record and an optional half blend.
    if let Some(layer) = scene.effect_layer {
        let mut quads: Vec<_> = layer.quads.to_vec();
        quads.sort_by_key(|quad| std::cmp::Reverse(quad.key));
        for quad in &quads {
            let Some(page) = layer.pages.get(quad.page).copied().flatten() else {
                continue;
            };
            if quad.size[0] <= 0 || quad.size[1] <= 0 || page.width == 0 || page.height == 0 {
                continue;
            }
            let first = sprites.vertices.len();
            let (x, y) = (quad.pos[0] as f32, quad.pos[1] as f32);
            let (u, v) = (f32::from(quad.uv[0]), f32::from(quad.uv[1]));
            let source = quad.source.map(|size| f32::from(size.max(1)));
            let tint = quad.tint.map(|channel| f32::from(channel) / 255.0);
            let alpha = if quad.blend & 0x80 != 0 { 0.5 } else { 1.0 };
            sprites.push_rect(
                [x, y, x + quad.size[0] as f32, y + quad.size[1] as f32],
                [u, v, u + source[0], v + source[1]],
                key_depth(quad.key, camera.fov),
                [tint[0], tint[1], tint[2], alpha],
                SPRITE_TEXTURED,
            );
            sprites.after.extend(sprites.run(page, first, false));
        }
    }
    sprites
}

/// The palette texture of an indexed page: 256 entries per row, the STP flag
/// in alpha. Rows past the end are padded black (the software lookup's
/// out-of-range colour).
pub fn palette_rgba(texture: &Texture8) -> (u32, Vec<u8>) {
    let rows = texture
        .palettes
        .len()
        .div_ceil(crate::model::PALETTE_ROW_LEN)
        .max(1);
    let mut rgba = vec![0u8; rows * crate::model::PALETTE_ROW_LEN * 4];
    for (index, entry) in texture.palettes.iter().enumerate() {
        let stp = texture.stp.get(index).copied().unwrap_or(false);
        rgba[index * 4..index * 4 + 4].copy_from_slice(&[
            entry[0],
            entry[1],
            entry[2],
            if stp { 255 } else { 0 },
        ]);
    }
    (rows as u32, rgba)
}

/// A cheap identity check for a cached upload: the sizes, the allocation and
/// a sparse sample of the contents. Residency is keyed on the asset's address,
/// so this catches an address reused by a different asset after a teardown.
pub fn fingerprint(parts: &[u64], bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    };
    for &part in parts {
        mix(part);
    }
    mix(bytes.len() as u64);
    mix(bytes.as_ptr() as u64);
    if !bytes.is_empty() {
        let step = (bytes.len() / 64).max(1);
        for index in (0..bytes.len()).step_by(step) {
            mix(u64::from(bytes[index]));
        }
    }
    hash
}

/// [`fingerprint`] of a TMD's geometry.
pub fn mesh_fingerprint(mesh: &Tmd) -> u64 {
    let mut parts = vec![mesh.objects.len() as u64, mesh.objects.as_ptr() as u64];
    for object in &mesh.objects {
        parts.push(object.vertices.len() as u64);
        parts.push(object.prims.len() as u64);
        parts.push(object.vertices.as_ptr() as u64);
        if let Some(first) = object.vertices.first() {
            parts.push(u64::from(first[0] as u16) << 32 | u64::from(first[2] as u16));
        }
    }
    fingerprint(&parts, &[])
}

/// [`fingerprint`] of an indexed texture page.
pub fn texture_fingerprint(texture: &Texture8) -> u64 {
    fingerprint(
        &[
            u64::from(texture.width),
            u64::from(texture.height),
            texture.palettes.len() as u64,
            texture.palettes.as_ptr() as u64,
            texture
                .palettes
                .first()
                .map_or(0, |entry| u64::from(u32::from_le_bytes(*entry))),
        ],
        &texture.indices,
    )
}

/// [`fingerprint`] of an RGBA image.
pub fn image_fingerprint(image: &Image) -> u64 {
    fingerprint(
        &[u64::from(image.width), u64::from(image.height)],
        &image.rgba,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TmdObject, TmdPrim};
    use crate::render::{EntityLightSource, Lighting};

    fn identity() -> Mat4x3 {
        Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        }
    }

    fn prim(vertices: [u16; 3]) -> TmdPrim {
        TmdPrim {
            vertices,
            normals: [0, 0, 0],
            uv: [[1, 2], [3, 4], [5, 6]],
            clut: (481 << 6) as u16,
            tsb: 0x11,
            textured: true,
            blend: false,
            raw_y: false,
            flat_color: None,
            quad: None,
        }
    }

    fn mesh() -> Tmd {
        let mut blended = prim([0, 1, 2]);
        blended.blend = true;
        let mut raw = prim([0, 1, 2]);
        raw.raw_y = true;
        Tmd {
            objects: vec![TmdObject {
                vertices: vec![[10, 20, 30], [40, 50, 60], [70, 80, 90]],
                normals: vec![[0, -4096, 0]],
                prims: vec![blended, prim([0, 1, 2]), raw, prim([0, 1, 9])],
            }],
        }
    }

    #[test]
    fn mesh_vertices_put_blend_packets_last_and_drop_bad_indices() {
        let built = mesh_vertices(&mesh());
        // Two opaque packets and one blended; the out-of-range one is dropped.
        assert_eq!(built.vertices.len(), 9);
        assert_eq!(built.blend_start, 6);
        let first = built.vertices[0];
        assert_eq!(first.position, [10.0, -20.0, 30.0]);
        assert_eq!(first.normal, [0.0, 4096.0, 0.0]);
        // Page 1 of the low row: +128 on u, +256 on v; palette row 1.
        assert_eq!(first.uv, [129.0, 258.0]);
        assert_eq!((first.packed >> 8) & 0xFFFF, 1);
        assert_eq!(first.packed & PACKED_BLEND, 0);
        // The raw-Y packet keeps the stored Y.
        assert_eq!(built.vertices[3].position, [10.0, 20.0, 30.0]);
        assert_ne!(built.vertices[6].packed & PACKED_BLEND, 0);
    }

    #[test]
    fn clip_position_reproduces_the_software_projection() {
        let mut camera = Camera::from_points([1200, -1800, 6000], [0, -900, 0], 600);
        camera.screen = [2, -1];
        let (view, projection) = camera_terms(&camera);
        for world in [[0, -900, 0], [300, -1200, -500], [-700, 0, 900]] {
            let expected = camera.project(world).unwrap();
            let world_f = world.map(|value| value as f32);
            let v: [f32; 3] = std::array::from_fn(|row| {
                view[row][0] * world_f[0]
                    + view[row][1] * world_f[1]
                    + view[row][2] * world_f[2]
                    + view[row][3]
            });
            let clip = clip_position(v, projection[0], [projection[1], projection[2]]);
            let sx = (clip[0] / clip[3] + 1.0) * 160.0;
            let sy = (1.0 - clip[1] / clip[3]) * 120.0;
            assert!(
                (sx - expected[0] as f32).abs() <= 1.0,
                "{sx} vs {expected:?}"
            );
            assert!(
                (sy - expected[1] as f32).abs() <= 1.0,
                "{sy} vs {expected:?}"
            );
            let depth = clip[2] / clip[3];
            assert!((0.0..=1.0).contains(&depth));
            // A painter's key at the same view-Z lands at the same depth.
            assert!((key_depth(v[2] as u32, camera.fov) - depth).abs() < 1e-4);
        }
    }

    #[test]
    fn light_vectors_match_the_reference_shade() {
        let camera = Camera::from_points([1200, -1800, 6000], [0, -900, 0], 600);
        let lighting = Lighting {
            ambient: [512, 1024, 2048],
            lights: Default::default(),
        };
        let mut light = lighting.latched([0, 0, 0]);
        light.sources[0] = EntityLightSource {
            dir: [1200, -3000, 2400],
            color: [0x80, 0x40, 0x20],
        };
        light.sources[1] = EntityLightSource {
            dir: [-2000, 1000, -3000],
            color: [0x10, 0x80, 0x40],
        };
        let texture = Texture8 {
            width: 1,
            height: 1,
            indices: vec![0],
            palettes: vec![],
            stp: vec![],
        };
        let tmd = mesh();
        let mut joint = identity();
        joint.r = crate::anim::rotation_matrix(300, 1200, 50);
        let joints = [joint];
        let entity = EntityMesh {
            mesh: &tmd,
            texture: &texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
        };
        let stored = stored_camera_matrix(camera.from, camera.look_at);
        for room_object in [false, true] {
            let uniforms = draw_uniforms(&camera, &entity, &light, room_object, None);
            for normal in [[0i16, -4096, 0], [2896, 0, 2896], [-1000, 3000, -2000]] {
                let reference = render::reference_vertex_shade(
                    &stored,
                    &joint.r,
                    normal,
                    &light,
                    room_object.then_some(&joint.r),
                );
                let n = [
                    f32::from(normal[0]),
                    -f32::from(normal[1]),
                    f32::from(normal[2]),
                ];
                let mut shade = uniforms.ambient;
                for slot in 0..3 {
                    let l = uniforms.joints[3 + slot];
                    let d = -(n[0] * l[0] + n[1] * l[1] + n[2] * l[2]);
                    if d > 0.0 {
                        for (value, colour) in shade.iter_mut().zip(uniforms.light_colour[slot]) {
                            *value += d * colour;
                        }
                    }
                }
                for channel in 0..3 {
                    assert!(
                        (f64::from(shade[channel]) - reference[channel]).abs() < 0.05,
                        "{shade:?} vs {reference:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn hidden_and_missing_joints_are_masked() {
        let tmd = Tmd {
            objects: vec![TmdObject::default(), TmdObject::default()],
        };
        let texture = Texture8 {
            width: 1,
            height: 1,
            indices: vec![0],
            palettes: vec![],
            stp: vec![],
        };
        let joints = [identity()];
        let entity = EntityMesh {
            mesh: &tmd,
            texture: &texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
        };
        let camera = Camera::from_points([0, 0, -5000], [0, 0, 0], 500);
        let light = Lighting {
            ambient: [0; 3],
            lights: Default::default(),
        }
        .latched([0; 3]);
        let uniforms = draw_uniforms(&camera, &entity, &light, false, None);
        // Joint 0 draws; joint 1 has no matrix; joints past the mesh never draw.
        assert_eq!(uniforms.flags[0], !1);
        let entity = EntityMesh {
            hidden_joints: 1,
            ..entity
        };
        assert_eq!(
            draw_uniforms(&camera, &entity, &light, false, None).flags[0],
            !0
        );
    }

    #[test]
    fn palette_carries_stp_in_alpha() {
        let texture = Texture8 {
            width: 1,
            height: 1,
            indices: vec![1],
            palettes: vec![[10, 20, 30, 255], [40, 50, 60, 255]],
            stp: vec![false, true],
        };
        let (rows, rgba) = palette_rgba(&texture);
        assert_eq!(rows, 1);
        assert_eq!(&rgba[..8], &[10, 20, 30, 0, 40, 50, 60, 255]);
        assert_eq!(rgba.len(), 256 * 4);
    }
}
