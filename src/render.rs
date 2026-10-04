//! Deterministic software rasterizer for the player model.
//!
//! [`Framebuffer::draw_model`] transforms a TMD mesh by the per-joint 4.12
//! matrices, shades the vertices with the room lighting, sorts the triangles
//! back-to-front and rasterizes them into the RGBA8 buffer. Nothing here uses
//! SDL; the engine owns presentation and only uploads [`Framebuffer::rgba`].

use crate::anim;
use crate::model::{Texture8, Tmd};
use crate::state::{Cut, Image, Light, RoomState};

/// Default framebuffer width in pixels.
const WIDTH: u32 = 320;
/// Default framebuffer height in pixels.
const HEIGHT: u32 = 240;
/// Fractional bits of the 4.12 fixed-point matrices.
const FIXED_BITS: u32 = 12;
/// 1.0 in the 4.12 fixed-point format.
const FIXED_ONE: f64 = 4096.0;
/// Horizontal screen centre in pixels.
const CENTER_X: f64 = 160.0;
/// Vertical screen centre in pixels.
const CENTER_Y: f64 = 120.0;
/// Divisor that scales a 12-bit ambient channel to 0..255.
const AMBIENT_DIVISOR: f64 = 16.0;
/// Full scale of one colour channel.
const CHANNEL_MAX: f64 = 255.0;

/// A 320x240 RGBA8 framebuffer.
#[derive(Debug, Clone)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framebuffer {
    /// A black 320x240 framebuffer.
    pub fn new() -> Self {
        Self {
            width: WIDTH,
            height: HEIGHT,
            rgba: vec![0; (WIDTH * HEIGHT * 4) as usize],
        }
    }

    /// Fill the whole framebuffer black.
    pub fn clear(&mut self) {
        self.rgba.fill(0);
    }

    /// Copy `image` to the top-left corner, clipping to both images' bounds.
    pub fn blit(&mut self, image: &Image) {
        let rows = image.height.min(self.height) as usize;
        let columns = image.width.min(self.width) as usize;
        if rows == 0 || columns == 0 {
            return;
        }
        let source_stride = image.width as usize * 4;
        let target_stride = self.width as usize * 4;
        for row in 0..rows {
            let source_start = row * source_stride;
            let source_end = source_start + columns * 4;
            let Some(source) = image.rgba.get(source_start..source_end) else {
                break;
            };
            let target_start = row * target_stride;
            if let Some(target) = self.rgba.get_mut(target_start..target_start + source.len()) {
                target.copy_from_slice(source);
            }
        }
    }

    /// Draw a TMD mesh with the given joint matrices and view.
    ///
    /// Objects beyond `joints` are skipped. Triangles whose vertices cross the
    /// near plane or that face away from the camera are dropped, the rest are
    /// painted back-to-front by mean view-space Z.
    pub fn draw_model(
        &mut self,
        mesh: &Tmd,
        texture: &Texture8,
        joints: &[anim::Mat4x3],
        camera: &Camera,
        lighting: &Lighting,
    ) {
        let mut triangles: Vec<Triangle> = Vec::new();

        for (object_index, object) in mesh.objects.iter().enumerate() {
            let Some(joint) = joints.get(object_index) else {
                continue;
            };

            let vertices: Vec<[i32; 3]> = object
                .vertices
                .iter()
                .map(|vertex| fixed_mul(joint, *vertex))
                .collect();
            let normals: Vec<Option<[f64; 3]>> = object
                .normals
                .iter()
                .map(|normal| normalize(rotate(joint, *normal)))
                .collect();

            for prim in &object.prims {
                let (Some(vertex0), Some(vertex1), Some(vertex2)) = (
                    vertices.get(usize::from(prim.vertices[0])),
                    vertices.get(usize::from(prim.vertices[1])),
                    vertices.get(usize::from(prim.vertices[2])),
                ) else {
                    continue;
                };
                let (Some(normal0), Some(normal1), Some(normal2)) = (
                    normals.get(usize::from(prim.normals[0])),
                    normals.get(usize::from(prim.normals[1])),
                    normals.get(usize::from(prim.normals[2])),
                ) else {
                    continue;
                };
                let (Some(screen0), Some(screen1), Some(screen2)) = (
                    camera.project(*vertex0),
                    camera.project(*vertex1),
                    camera.project(*vertex2),
                ) else {
                    continue;
                };

                let position0 = [screen0[0] as f64, screen0[1] as f64];
                let position1 = [screen1[0] as f64, screen1[1] as f64];
                let position2 = [screen2[0] as f64, screen2[1] as f64];
                let area = (position1[0] - position0[0]) * (position2[1] - position0[1])
                    - (position2[0] - position0[0]) * (position1[1] - position0[1]);
                if !faces_camera(area) {
                    continue;
                }

                let depth0 = f64::from(camera.view_position(*vertex0)[2]);
                let depth1 = f64::from(camera.view_position(*vertex1)[2]);
                let depth2 = f64::from(camera.view_position(*vertex2)[2]);

                let page_x = f64::from(u32::from(prim.tsb & 0xF) * 128);
                let page_y = f64::from(u32::from((prim.tsb >> 4) & 1) * 256);

                let corners = [
                    (position0, depth0, &prim.uv[0], normal0, vertex0),
                    (position1, depth1, &prim.uv[1], normal1, vertex1),
                    (position2, depth2, &prim.uv[2], normal2, vertex2),
                ];
                let raster: [RasterVertex; 3] = std::array::from_fn(|corner| {
                    let (position, depth, uv, normal, vertex) = corners[corner];
                    RasterVertex {
                        position,
                        inv_z: 1.0 / depth,
                        u: f64::from(uv[0]) + page_x,
                        v: f64::from(uv[1]) + page_y,
                        shade: shade_vertex(normal, *vertex, lighting),
                    }
                });

                let palette_row = usize::from(((prim.clut >> 6) & 0x01FF).saturating_sub(480));
                triangles.push(Triangle {
                    raster,
                    depth: (depth0 + depth1 + depth2) / 3.0,
                    palette_row,
                });
            }
        }

        triangles.sort_by(|a, b| b.depth.total_cmp(&a.depth));
        for triangle in &triangles {
            self.rasterize(texture, triangle);
        }
    }

    fn rasterize(&mut self, texture: &Texture8, triangle: &Triangle) {
        if texture.width == 0 || texture.height == 0 {
            return;
        }
        let [a, b, c] = &triangle.raster;
        let area = (b.position[0] - a.position[0]) * (c.position[1] - a.position[1])
            - (c.position[0] - a.position[0]) * (b.position[1] - a.position[1]);
        if !faces_camera(area) {
            return;
        }

        let min_x = a.position[0].min(b.position[0]).min(c.position[0]).floor() as i64;
        let max_x = a.position[0].max(b.position[0]).max(c.position[0]).ceil() as i64;
        let min_y = a.position[1].min(b.position[1]).min(c.position[1]).floor() as i64;
        let max_y = a.position[1].max(b.position[1]).max(c.position[1]).ceil() as i64;
        let min_x = min_x.max(0);
        let min_y = min_y.max(0);
        let max_x = max_x.min(i64::from(self.width) - 1);
        let max_y = max_y.min(i64::from(self.height) - 1);
        if min_x > max_x || min_y > max_y {
            return;
        }

        let width = self.width as usize;
        for y in min_y..=max_y {
            let py = y as f64 + 0.5;
            for x in min_x..=max_x {
                let px = x as f64 + 0.5;
                let weight0 = ((b.position[0] - px) * (c.position[1] - py)
                    - (c.position[0] - px) * (b.position[1] - py))
                    / area;
                let weight1 = ((c.position[0] - px) * (a.position[1] - py)
                    - (a.position[0] - px) * (c.position[1] - py))
                    / area;
                let weight2 = 1.0 - weight0 - weight1;
                if weight0 < 0.0 || weight1 < 0.0 || weight2 < 0.0 {
                    continue;
                }

                let inv_z = weight0 * a.inv_z + weight1 * b.inv_z + weight2 * c.inv_z;
                if inv_z.is_nan() || inv_z <= 0.0 {
                    continue;
                }
                let u =
                    (weight0 * a.u * a.inv_z + weight1 * b.u * b.inv_z + weight2 * c.u * c.inv_z)
                        / inv_z;
                let v =
                    (weight0 * a.v * a.inv_z + weight1 * b.v * b.inv_z + weight2 * c.v * c.inv_z)
                        / inv_z;
                let Some(texel_u) = wrap_texel(u, texture.width) else {
                    continue;
                };
                let Some(texel_v) = wrap_texel(v, texture.height) else {
                    continue;
                };
                let index = texel_v as usize * texture.width as usize + texel_u as usize;
                let Some(&palette_index) = texture.indices.get(index) else {
                    continue;
                };
                let color = texture.palette(triangle.palette_row, palette_index);

                let shade = [
                    weight0 * a.shade[0] + weight1 * b.shade[0] + weight2 * c.shade[0],
                    weight0 * a.shade[1] + weight1 * b.shade[1] + weight2 * c.shade[1],
                    weight0 * a.shade[2] + weight1 * b.shade[2] + weight2 * c.shade[2],
                ];
                let pixel = [
                    channel(color[0], shade[0]),
                    channel(color[1], shade[1]),
                    channel(color[2], shade[2]),
                    255,
                ];
                let offset = (y as usize * width + x as usize) * 4;
                if let Some(slot) = self.rgba.get_mut(offset..offset + 4) {
                    slot.copy_from_slice(&pixel);
                }
            }
        }
    }
}

/// A cut camera: a 4.12 view rotation, a world-unit translation and the cut's
/// focal length in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Camera {
    pub view: [[i32; 3]; 3],
    pub trans: [i32; 3],
    pub fov: i32,
}

impl Camera {
    /// Build the view rotation and translation from the cut's position and
    /// look-at point. The focal length is `cut.fov`.
    pub fn from_cut(cut: &Cut) -> Self {
        let from = [
            f64::from(cut.pos[0]),
            f64::from(cut.pos[1]),
            f64::from(cut.pos[2]),
        ];
        let to = [
            f64::from(cut.look_at[0]),
            f64::from(cut.look_at[1]),
            f64::from(cut.look_at[2]),
        ];
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        let h = (d[0] * d[0] + d[2] * d[2]).sqrt();

        let (right, up, forward) = if len <= f64::EPSILON {
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0])
        } else if h <= f64::EPSILON {
            let forward = [d[0] / len, -d[1] / len, d[2] / len];
            let right = [1.0, 0.0, 0.0];
            let up = [
                forward[1] * right[2] - forward[2] * right[1],
                forward[2] * right[0] - forward[0] * right[2],
                forward[0] * right[1] - forward[1] * right[0],
            ];
            (right, up, forward)
        } else {
            (
                [d[2] / h, 0.0, -d[0] / h],
                [d[0] * d[1] / (h * len), h / len, d[1] * d[2] / (h * len)],
                [d[0] / len, -d[1] / len, d[2] / len],
            )
        };

        let rows = [right, up, forward];
        // The game projects a world point by negating its Y before the camera
        // rotation and using a Y-flipped translation (the world's Y axis grows
        // downwards while the camera basis is built for screen-down Y). Fold
        // that into the stored matrix: negate the Y column, then compute the
        // translation with the flipped rows so the camera's look-at point maps
        // to the screen centre. `project` then stays a plain `120 - vy`.
        let view = rows.map(|row| [fixed(row[0]), fixed(-row[1]), fixed(row[2])]);
        let trans = view.map(|row| {
            (-(f64::from(row[0]) * from[0]
                + f64::from(row[1]) * from[1]
                + f64::from(row[2]) * from[2])
                / 4096.0)
                .round() as i32
        });
        Self {
            view,
            trans,
            fov: cut.fov,
        }
    }

    /// Project a world-space point to screen pixels.
    ///
    /// `None` when the point is not in front of the near plane
    /// (`view_z < 2 * fov`).
    pub fn project(&self, world: [i32; 3]) -> Option<[i32; 2]> {
        let [vx, vy, vz] = self.view_position(world);
        let fov = i64::from(self.fov);
        let vz = i64::from(vz);
        if vz <= 0 || vz < 2 * fov {
            return None;
        }
        let focal = fov as f64;
        let inverse = focal / vz as f64;
        let sx = CENTER_X + vx as f64 * inverse;
        // `vy` is already in the screen-down frame (see `from_cut`).
        let sy = CENTER_Y - vy as f64 * inverse;
        Some([sx.round() as i32, sy.round() as i32])
    }

    /// The 4.12 fixed-point view-space position of a world point.
    fn view_position(&self, world: [i32; 3]) -> [i32; 3] {
        std::array::from_fn(|row| {
            let r = self.view[row];
            let sum = i128::from(r[0]) * i128::from(world[0])
                + i128::from(r[1]) * i128::from(world[1])
                + i128::from(r[2]) * i128::from(world[2]);
            clamp_i32((sum >> FIXED_BITS) + i128::from(self.trans[row]))
        })
    }
}

/// The room's ambient colour and its three lights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lighting {
    pub ambient: [i16; 3],
    pub lights: [Light; 3],
}

impl Lighting {
    /// Take the ambient colour and lights straight from the loaded room.
    pub fn from_room(room: &RoomState) -> Self {
        Self {
            ambient: room.ambient,
            lights: room.lights,
        }
    }
}

/// One projected vertex ready for rasterization.
struct RasterVertex {
    position: [f64; 2],
    inv_z: f64,
    u: f64,
    v: f64,
    shade: [f64; 3],
}

/// One triangle ready for rasterization.
struct Triangle {
    raster: [RasterVertex; 3],
    depth: f64,
    palette_row: usize,
}

/// Rotate `vector` by `rotation` and negate the resulting Y.
///
/// This is the Y-sign conjugation `D * R * D` (with `D = diag(1, -1, 1)`) the
/// PS1 matrix pipeline builds into every product: the game's joint matrices
/// rotate Y-negated vectors and negate Y again on the way out, which flips the
/// apparent sign of pitch and roll. Applying the matrices without it mirrors
/// every limb that pitches or rolls.
fn conjugate_rotate(rotation: &[[i32; 3]; 3], vector: [i32; 3]) -> [i32; 3] {
    let v = [
        i128::from(vector[0]),
        -i128::from(vector[1]),
        i128::from(vector[2]),
    ];
    std::array::from_fn(|row| {
        let r = rotation[row];
        let mut sum = i128::from(r[0]) * v[0] + i128::from(r[1]) * v[1] + i128::from(r[2]) * v[2];
        if row == 1 {
            sum = -sum;
        }
        clamp_i32(sum)
    })
}

/// Transform a vertex by a joint's 4.12 rotation and translation.
///
/// The joint matrices follow the game's convention; a room-space point maps
/// through `D * R * D * v + t`.
fn fixed_mul(joint: &anim::Mat4x3, vertex: [i16; 3]) -> [i32; 3] {
    let v = [
        i128::from(vertex[0]),
        -i128::from(vertex[1]),
        i128::from(vertex[2]),
    ];
    std::array::from_fn(|row| {
        let r = joint.r[row];
        let sum = i128::from(r[0]) * v[0] + i128::from(r[1]) * v[1] + i128::from(r[2]) * v[2];
        let mut scaled = (sum + ((sum >> 127) & i128::from(0xFFF))) >> FIXED_BITS;
        if row == 1 {
            scaled = -scaled;
        }
        clamp_i32(scaled + i128::from(joint.t[row]))
    })
}

/// Rotate a normal by a joint's 4.12 rotation, ignoring the translation.
fn rotate(joint: &anim::Mat4x3, normal: [i16; 3]) -> [i32; 3] {
    conjugate_rotate(
        &joint.r,
        [
            i32::from(normal[0]),
            i32::from(normal[1]),
            i32::from(normal[2]),
        ],
    )
}

/// Round a real quantity to the 4.12 fixed-point representation.
fn fixed(value: f64) -> i32 {
    (value * FIXED_ONE).round() as i32
}

/// Clamp an intermediate to the `i32` range.
fn clamp_i32(value: i128) -> i32 {
    value.clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
}

/// Whether a projected triangle shows its outward side.
///
/// Screen-space signed area of an outward-facing triangle is positive under
/// the camera projection (verified against the model's stored corner normals).
fn faces_camera(area: f64) -> bool {
    area > 0.0
}

/// Normalize a fixed-point vector; zero-length vectors have no direction.
fn normalize(vector: [i32; 3]) -> Option<[f64; 3]> {
    let real = [
        f64::from(vector[0]),
        f64::from(vector[1]),
        f64::from(vector[2]),
    ];
    let length = (real[0] * real[0] + real[1] * real[1] + real[2] * real[2]).sqrt();
    if length <= f64::MIN_POSITIVE {
        return None;
    }
    Some([real[0] / length, real[1] / length, real[2] / length])
}

/// Shade one vertex: ambient plus the room lights, clamped to 0..255.
fn shade_vertex(normal: &Option<[f64; 3]>, world: [i32; 3], lighting: &Lighting) -> [f64; 3] {
    let mut shade = lighting
        .ambient
        .map(|channel| f64::from(channel) / AMBIENT_DIVISOR);
    let Some(normal) = normal else {
        return shade.map(|value| value.clamp(0.0, CHANNEL_MAX));
    };

    for light in &lighting.lights {
        let (direction, attenuation) = if light.kind == 0 {
            let offset = [
                f64::from(light.pos[0]) - f64::from(world[0]),
                f64::from(light.pos[1]) - f64::from(world[1]),
                f64::from(light.pos[2]) - f64::from(world[2]),
            ];
            let distance =
                (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]).sqrt();
            let attenuation = if light.radius > 0 {
                (1.0 - distance / f64::from(light.radius)).max(0.0)
            } else {
                0.0
            };
            (offset, attenuation)
        } else {
            (
                [
                    f64::from(light.pos[0]),
                    f64::from(light.pos[1]),
                    f64::from(light.pos[2]),
                ],
                1.0,
            )
        };
        if attenuation <= 0.0 {
            continue;
        }

        let length = (direction[0] * direction[0]
            + direction[1] * direction[1]
            + direction[2] * direction[2])
            .sqrt();
        if length <= f64::MIN_POSITIVE {
            continue;
        }
        let direction = [
            direction[0] / length,
            direction[1] / length,
            direction[2] / length,
        ];
        let diffuse =
            (normal[0] * direction[0] + normal[1] * direction[1] + normal[2] * direction[2])
                .max(0.0);
        if diffuse <= 0.0 {
            continue;
        }

        let contribution = diffuse * attenuation;
        for (channel_shade, color) in shade.iter_mut().zip(light.color) {
            *channel_shade += contribution * f64::from(color);
        }
    }

    shade.map(|value| value.clamp(0.0, CHANNEL_MAX))
}

/// Wrap a texture coordinate around one dimension.
fn wrap_texel(coordinate: f64, size: u32) -> Option<u32> {
    if size == 0 {
        return None;
    }
    Some(coordinate.rem_euclid(f64::from(size)) as u32)
}

/// Multiply a texture channel by a 0..255 shade.
fn channel(texel: u8, shade: f64) -> u8 {
    (f64::from(texel) * shade / CHANNEL_MAX).clamp(0.0, CHANNEL_MAX) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TmdObject, TmdPrim};

    fn identity() -> anim::Mat4x3 {
        anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        }
    }

    fn straight_camera() -> Camera {
        Camera {
            view: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            trans: [0, 0, 0],
            fov: 200,
        }
    }

    fn solid_texture(pixel: [u8; 4]) -> Texture8 {
        Texture8 {
            width: 1,
            height: 1,
            indices: vec![0],
            palettes: vec![pixel],
        }
    }

    fn mesh(reversed: bool) -> Tmd {
        let mut vertices = vec![[0i16, 0, 1000], [0, 1000, 1000], [1000, 0, 1000]];
        if reversed {
            vertices.swap(1, 2);
        }
        Tmd {
            objects: vec![TmdObject {
                vertices,
                normals: vec![[0, 0, 4096]],
                prims: vec![TmdPrim {
                    vertices: [0, 1, 2],
                    normals: [0, 0, 0],
                    uv: [[0, 0]; 3],
                    clut: 0x7800,
                    tsb: 0x80,
                }],
            }],
        }
    }

    fn render_triangle(reversed: bool) -> Framebuffer {
        let mut framebuffer = Framebuffer::new();
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };
        framebuffer.draw_model(
            &mesh(reversed),
            &solid_texture([10, 20, 30, 255]),
            &[identity()],
            &straight_camera(),
            &lighting,
        );
        framebuffer
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }

    #[test]
    fn rasterizes_a_front_facing_triangle() {
        let framebuffer = render_triangle(false);
        let center = (53 * 320 + 226) * 4;
        assert_eq!(&framebuffer.rgba[center..center + 4], &[10, 20, 30, 255]);
        assert_eq!(&framebuffer.rgba[0..4], &[0, 0, 0, 0]);
        let corner = (239 * 320 + 319) * 4;
        assert_eq!(&framebuffer.rgba[corner..corner + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn culls_a_back_facing_triangle() {
        let framebuffer = render_triangle(true);
        assert!(framebuffer.rgba.iter().all(|&byte| byte == 0));
    }

    #[test]
    fn synthetic_triangle_render_matches_golden_hash() {
        let framebuffer = render_triangle(false);
        assert_eq!(fnv1a(&framebuffer.rgba), 1_660_610_673_012_094_917);
    }

    #[test]
    fn project_rejects_points_behind_the_near_plane() {
        let camera = straight_camera();
        assert_eq!(camera.project([0, 0, 399]), None);
        assert_eq!(camera.project([0, 0, 400]), Some([160, 120]));
        assert_eq!(camera.project([0, 0, -10]), None);
    }

    #[test]
    fn from_cut_centres_the_look_at_point() {
        let cut = Cut {
            pos: [0, 0, 0],
            look_at: [0, 0, 1000],
            fov: 200,
            ..Cut::default()
        };
        let camera = Camera::from_cut(&cut);
        assert_eq!(camera.project([0, 0, 1000]), Some([160, 120]));

        let cut = Cut {
            pos: [0, 0, -1000],
            look_at: [0, 0, 0],
            fov: 200,
            ..Cut::default()
        };
        let camera = Camera::from_cut(&cut);
        assert_eq!(camera.project([0, 0, 0]), Some([160, 120]));
    }

    #[test]
    fn from_cut_survives_degenerate_look_at() {
        let cut = Cut {
            pos: [5, 5, 5],
            look_at: [5, 5, 5],
            fov: 200,
            ..Cut::default()
        };
        let camera = Camera::from_cut(&cut);
        assert_eq!(camera.project([5, 5, 500]), Some([160, 120]));
    }

    #[test]
    fn blit_clips_out_of_bounds_images() {
        let mut image = Image {
            width: 400,
            height: 300,
            rgba: Vec::with_capacity(400 * 300 * 4),
        };
        for y in 0..300u32 {
            for x in 0..400u32 {
                image
                    .rgba
                    .extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 7, 255]);
            }
        }

        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&image);

        assert_eq!((framebuffer.width, framebuffer.height), (320, 240));
        assert_eq!(&framebuffer.rgba[0..4], &[0, 0, 7, 255]);
        let corner = (239 * 320 + 319) * 4;
        assert_eq!(&framebuffer.rgba[corner..corner + 4], &[63, 239, 7, 255]);
        let source = (239 * 400 + 319) * 4;
        assert_eq!(
            &image.rgba[source..source + 4],
            &framebuffer.rgba[corner..corner + 4]
        );
    }

    #[test]
    fn blit_handles_truncated_images() {
        let image = Image {
            width: 320,
            height: 240,
            rgba: vec![9; 320 * 4 + 8],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&image);
        assert_eq!(&framebuffer.rgba[0..4], &[9, 9, 9, 9]);
        let second_row = 320 * 4;
        assert_eq!(&framebuffer.rgba[second_row..second_row + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn lighting_copies_the_room_state() {
        let room = RoomState {
            ambient: [1, 2, 3],
            lights: [Light {
                pos: [4, 5, 6],
                color: [7, 8, 9],
                kind: 1,
                radius: 10,
            }; 3],
            ..RoomState::default()
        };
        let lighting = Lighting::from_room(&room);
        assert_eq!(lighting.ambient, [1, 2, 3]);
        assert_eq!(lighting.lights[0].pos, [4, 5, 6]);
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn renders_real_player_model() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let path = ["JPN", ""]
            .iter()
            .map(|prefix| root.join(prefix).join("ENEMY").join("CHAR11.EMD"))
            .find(|candidate| candidate.is_file());
        let Some(path) = path else {
            panic!("CHAR11.EMD not found under {}", root.display());
        };

        let data = std::fs::read(&path).unwrap();
        let emd = crate::emd::parse(&data).unwrap();
        let joints = vec![identity(); 15];
        let camera = Camera::from_cut(&Cut {
            pos: [0, 0, -4000],
            look_at: [0, 0, 0],
            fov: 221,
            ..Cut::default()
        });
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };

        let mut first = Framebuffer::new();
        first.draw_model(&emd.mesh, &emd.texture, &joints, &camera, &lighting);
        let mut second = Framebuffer::new();
        second.draw_model(&emd.mesh, &emd.texture, &joints, &camera, &lighting);

        assert_eq!(first.rgba, second.rgba, "two renders differ");
        let visible = first
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
            .count();
        println!("CHAR11 non-black pixels: {visible}");
        assert!(visible > 1000, "model is not visible: {visible} pixels");
    }

    #[test]
    fn fixed_mul_uses_the_ps1_y_sign_conjugation() {
        // RotMatrix(0x400, 0, 0) is [[4095, 0, 0], [0, 0, 4095], [0, -4095, 0]].
        let joint = anim::Mat4x3 {
            r: [[4095, 0, 0], [0, 0, 4095], [0, -4095, 0]],
            t: [10, 20, 30],
        };

        // The vertex's Y is negated before the rotation and the rotation's Y
        // negated again before the translation is added, so the pitched joint
        // maps +Z to -Y (a plain product would map it to +Y).
        assert_eq!(fixed_mul(&joint, [0, 0, 100]), [10, -79, 30]);
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn real_walk_pose_matches_the_original_projection() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let data = std::fs::read(format!("{root}/JPN/ENEMY/CHAR11.EMD")).unwrap();
        let emd = crate::emd::parse(&data).unwrap();

        // CHAR11 keyframe 55 (a walk pose), entity at (5000, 0, 5000) facing
        // yaw 0, projected through ROOM1001 cut 0 (pos 3960,-3132,11538,
        // look-at 5274,-2430,5652, fov 221). The expected screen pixels are
        // the original's own pipeline output.
        let entity = anim::entity_matrix([5000, 0, 5000], 0);
        let joints = anim::joint_matrices(&emd.skeleton, &emd.keyframes[55], &entity);
        let camera = Camera::from_cut(&Cut {
            pos: [3960, -3132, 11538],
            look_at: [5274, -2430, 5652],
            fov: 221,
            ..Cut::default()
        });

        let expected = [
            (0usize, 0usize, 169i32, 137i32),
            (0, 1, 168, 138),
            (0, 2, 177, 138),
            (0, 3, 173, 127),
            (9, 0, 172, 137),
            (14, 3, 173, 143),
        ];
        for (object, vertex, expected_x, expected_y) in expected {
            let world = fixed_mul(&joints[object], emd.mesh.objects[object].vertices[vertex]);
            let screen = camera.project(world).unwrap();
            assert!(
                (screen[0] - expected_x).abs() <= 1 && (screen[1] - expected_y).abs() <= 1,
                "object {object} vertex {vertex}: got {screen:?}, expected ({expected_x}, {expected_y})"
            );
        }
    }
}
