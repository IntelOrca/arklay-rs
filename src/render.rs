//! Deterministic software rasterizer for the player model and room masks.
//!
//! [`Framebuffer::draw_model`] transforms a TMD mesh by the per-joint 4.12
//! matrices, shades the vertices with the room lighting, sorts the triangles
//! back-to-front and rasterizes them into the RGBA8 buffer. Nothing here uses
//! SDL; the engine owns presentation and only uploads [`Framebuffer::rgba`].
//!
//! [`draw_gameplay_scene`] is the full gameplay path. It blits the camera's
//! background and then paints one stable far-to-near list mixing the camera's
//! mask sprites (from a [`MaskLayer`]) with the player's triangles, so the
//! character passes behind foreground pillars:
//!
//! - A mask sprite's key is [`crate::mask::mask_sprite_key`]: the shared
//!   per-(room, camera) bias, the hand-tuned per-entry overrides and the
//!   `pos_data`-derived ordering-table key.
//! - A player triangle's key is its mean view-space Z. The original's
//!   ordering table takes an entity at `z >> 4` and stores a mask at
//!   `fade << 4`, so the two keys are directly comparable without any extra
//!   scaling. This engine sorts on integer keys, so the mean is rounded to the
//!   nearest unit; a triangle whose rounded key equals a mask's key ties with
//!   it, and the original flushes only *strictly* farther masks before a
//!   triangle, so equal-depth masks are submitted after player triangles here
//!   too and win the tie.
//!
//! A [`MaskLayer`] carries the camera's decoded `roommask/{room}_{cam}.bmp`
//! page and its [`Cut`]; a later engine step only has to load and decode that
//! page, then pass the layer to [`draw_gameplay_scene`]. [`Framebuffer::draw_model`]
//! remains the plain player-only path used by the tests and the non-mask case.

use crate::anim;
use crate::mask;
use crate::model::{Texture8, Tmd, TmdObject};
use crate::state::{Cut, Image, Light, RoomId, RoomState};

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

/// The original's pending-sprite ordering-table depth of the default text
/// brightness: `brightness * 16 + 0x1C2`.
pub const PENDING_SPRITE_DEPTH_BASE: u32 = 0x1C2;

/// The original's text tint table: the palette row (`clutY - 0x1E0`) picks a
/// colour multiplier over the sheet's grey ramp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    White,
    Green,
    Red,
    Grey,
    Yellow,
}

impl Tint {
    /// The tint a font CLUT row selects. Row 8 is the shadow row and behaves
    /// as green; anything outside 0..=3 is yellow.
    pub fn from_clut_row(row: i32) -> Self {
        match row {
            0 => Self::White,
            1 | 8 => Self::Green,
            2 => Self::Red,
            3 => Self::Grey,
            _ => Self::Yellow,
        }
    }

    /// The tint's per-channel multiplier.
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Self::White => [255, 255, 255],
            Self::Green => [0, 255, 0],
            Self::Red => [255, 0, 0],
            Self::Grey => [204, 204, 204],
            Self::Yellow => [255, 255, 0],
        }
    }
}

/// Where a queued 2D sprite samples its pixels.
#[derive(Debug, Clone, Copy)]
pub enum SpriteSource<'a> {
    /// An indexed texture with one palette row and a tint multiplier.
    Indexed {
        texture: &'a Texture8,
        clut_row: usize,
        tint: Tint,
    },
    /// A direct RGBA image.
    Rgba(&'a Image),
}

/// One pending 2D sprite, submitted to [`Framebuffer::draw_sprites`].
///
/// Both rectangles are `[x, y, width, height]`: `src` in texture pixels and
/// `dst` in framebuffer pixels, so unequal sizes scale with nearest sampling.
/// [`SpriteDraw::indexed`] and [`SpriteDraw::rgba`] derive `depth` from the
/// brightness the way the original's pending-sprite queue does.
#[derive(Debug, Clone, Copy)]
pub struct SpriteDraw<'a> {
    /// Ordering-table depth; higher depths are farther behind and paint first.
    pub depth: u32,
    pub source: SpriteSource<'a>,
    pub src: [i32; 4],
    pub dst: [i32; 4],
    /// Brightness in the original's 0..=30 range; 0 and 2 are both full.
    pub brightness: u8,
}

impl<'a> SpriteDraw<'a> {
    /// A queued indexed sprite.
    pub fn indexed(
        texture: &'a Texture8,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        brightness: u8,
        tint: Tint,
    ) -> Self {
        Self {
            depth: pending_sprite_depth(brightness),
            source: SpriteSource::Indexed {
                texture,
                clut_row,
                tint,
            },
            src,
            dst,
            brightness,
        }
    }

    /// A queued RGBA sprite.
    pub fn rgba(image: &'a Image, src: [i32; 4], dst: [i32; 4], brightness: u8) -> Self {
        Self {
            depth: pending_sprite_depth(brightness),
            source: SpriteSource::Rgba(image),
            src,
            dst,
            brightness,
        }
    }
}

/// The original's pending-sprite OT depth for a brightness.
pub fn pending_sprite_depth(brightness: u8) -> u32 {
    u32::from(brightness) * 16 + PENDING_SPRITE_DEPTH_BASE
}

/// The original's brightness scale: 0 and 2 are the same full-brightness
/// render, anything else is `brightness * 255 / 30` clamped to 255.
fn brightness_scale(brightness: u8) -> u32 {
    if brightness == 0 || brightness == 2 {
        255
    } else {
        (u32::from(brightness) * 255 / 30).min(255)
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

    /// Draw one indexed sprite with nearest sampling, clipped to the
    /// framebuffer.
    ///
    /// `src` and `dst` are `[x, y, width, height]`; the palette row and tint
    /// mix the texel colour and `brightness` scales it. Palette index 0 and
    /// zero-alpha palette entries are transparent. Out-of-range source pixels
    /// leave the framebuffer untouched.
    pub fn draw_indexed_sprite(
        &mut self,
        texture: &Texture8,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        brightness: u8,
        tint: Tint,
    ) {
        if texture.width == 0 || texture.height == 0 {
            return;
        }
        let scale = brightness_scale(brightness);
        let tint = tint.rgb();
        self.draw_sprite(src, dst, |u, v| {
            if u < 0 || v < 0 || u >= texture.width as i32 || v >= texture.height as i32 {
                return None;
            }
            let index = *texture
                .indices
                .get(v as usize * texture.width as usize + u as usize)?;
            if index == 0 {
                return None;
            }
            let texel = texture.palette(clut_row, index);
            if texel[3] == 0 {
                return None;
            }
            let mix = |channel: u8, tint: u8| {
                (u32::from(channel) * u32::from(tint) / 255 * scale / 255) as u8
            };
            Some([
                mix(texel[0], tint[0]),
                mix(texel[1], tint[1]),
                mix(texel[2], tint[2]),
                255,
            ])
        });
    }

    /// Draw one RGBA sprite with nearest sampling, clipped to the framebuffer.
    ///
    /// Zero-alpha texels are transparent; `brightness` scales the colour
    /// channels exactly like [`Framebuffer::draw_indexed_sprite`].
    pub fn draw_rgba_sprite(
        &mut self,
        image: &Image,
        src: [i32; 4],
        dst: [i32; 4],
        brightness: u8,
    ) {
        if image.width == 0 || image.height == 0 {
            return;
        }
        let scale = brightness_scale(brightness);
        self.draw_sprite(src, dst, |u, v| {
            if u < 0 || v < 0 || u >= image.width as i32 || v >= image.height as i32 {
                return None;
            }
            let offset = (v as usize * image.width as usize + u as usize) * 4;
            let texel = image.rgba.get(offset..offset + 4)?;
            if texel[3] == 0 {
                return None;
            }
            Some([
                (u32::from(texel[0]) * scale / 255) as u8,
                (u32::from(texel[1]) * scale / 255) as u8,
                (u32::from(texel[2]) * scale / 255) as u8,
                texel[3],
            ])
        });
    }

    /// Draw a pending list far-to-near.
    ///
    /// The sort is stable on descending [`SpriteDraw::depth`] (higher depths
    /// are farther behind and paint first), so equal-depth sprites keep their
    /// submission order, exactly like the original's pending-sprite queue.
    pub fn draw_sprites(&mut self, sprites: &mut [SpriteDraw<'_>]) {
        sprites.sort_by_key(|sprite| std::cmp::Reverse(sprite.depth));
        for sprite in sprites.iter() {
            match sprite.source {
                SpriteSource::Indexed {
                    texture,
                    clut_row,
                    tint,
                } => self.draw_indexed_sprite(
                    texture,
                    sprite.src,
                    sprite.dst,
                    clut_row,
                    sprite.brightness,
                    tint,
                ),
                SpriteSource::Rgba(image) => {
                    self.draw_rgba_sprite(image, sprite.src, sprite.dst, sprite.brightness);
                }
            }
        }
    }

    /// Visit the framebuffer-visible pixels of a sprite: clip `dst` and map
    /// each destination pixel back to `src` with nearest sampling.
    fn draw_sprite(
        &mut self,
        src: [i32; 4],
        dst: [i32; 4],
        mut sample: impl FnMut(i32, i32) -> Option<[u8; 4]>,
    ) {
        let [src_x, src_y, src_w, src_h] = src;
        let [dst_x, dst_y, dst_w, dst_h] = dst;
        if src_w <= 0 || src_h <= 0 || dst_w <= 0 || dst_h <= 0 {
            return;
        }
        let start_x = dst_x.max(0);
        let start_y = dst_y.max(0);
        let end_x = dst_x.saturating_add(dst_w).min(self.width as i32);
        let end_y = dst_y.saturating_add(dst_h).min(self.height as i32);
        if start_x >= end_x || start_y >= end_y {
            return;
        }
        let width = self.width as usize;
        for y in start_y..end_y {
            let Ok(tex_y) = i32::try_from(
                i64::from(src_y)
                    + (i64::from(y) - i64::from(dst_y)) * i64::from(src_h) / i64::from(dst_h),
            ) else {
                continue;
            };
            for x in start_x..end_x {
                let Ok(tex_x) = i32::try_from(
                    i64::from(src_x)
                        + (i64::from(x) - i64::from(dst_x)) * i64::from(src_w) / i64::from(dst_w),
                ) else {
                    continue;
                };
                let Some(pixel) = sample(tex_x, tex_y) else {
                    continue;
                };
                let offset = (y as usize * width + x as usize) * 4;
                if let Some(slot) = self.rgba.get_mut(offset..offset + 4) {
                    slot.copy_from_slice(&pixel);
                }
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
        for (object, joint) in mesh.objects.iter().zip(joints) {
            collect_triangles(object, joint, camera, Some(lighting), true, &mut triangles);
        }
        self.rasterize_triangles(texture, triangles);
    }

    /// Draw a TMD mesh full-bright, without lighting or backface culling.
    ///
    /// This is the door animation's path: panels are drawn over black with the
    /// door texture's single 256-colour CLUT row and direct (unpaged) UVs, and
    /// the original's TMD renderer runs with culling disabled. Triangles are
    /// painter-sorted back-to-front exactly like [`Framebuffer::draw_model`].
    pub fn draw_model_unlit(
        &mut self,
        mesh: &Tmd,
        texture: &Texture8,
        joints: &[anim::Mat4x3],
        camera: &Camera,
    ) {
        self.draw_objects_unlit(mesh.objects.iter().zip(joints), texture, camera);
    }

    /// Draw one or more objects full-bright with one matrix per object.
    ///
    /// Every object's triangles are collected first and painter-sorted
    /// together, so overlapping door panels interleave by depth the way the
    /// original's depth-keyed TMD queue does.
    pub fn draw_objects_unlit<'a>(
        &mut self,
        objects: impl IntoIterator<Item = (&'a TmdObject, &'a anim::Mat4x3)>,
        texture: &Texture8,
        camera: &Camera,
    ) {
        let mut triangles: Vec<Triangle> = Vec::new();
        for (object, joint) in objects {
            collect_triangles(object, joint, camera, None, false, &mut triangles);
        }
        self.rasterize_triangles(texture, triangles);
    }

    fn rasterize_triangles(&mut self, texture: &Texture8, mut triangles: Vec<Triangle>) {
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
        if triangle.cull && !faces_camera(area) {
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

    /// Paint a sorted scene list: triangles share one texture page, mask
    /// sprites sample the camera's decoded mask page.
    fn draw_scene(
        &mut self,
        texture: Option<&Texture8>,
        page: Option<&Image>,
        items: &[SceneItem],
    ) {
        for item in items {
            match item {
                SceneItem::Triangle(triangle) => {
                    if let Some(texture) = texture {
                        self.rasterize(texture, triangle);
                    }
                }
                SceneItem::Mask(quad) => {
                    if let Some(page) = page {
                        self.rasterize_mask(page, quad);
                    }
                }
            }
        }
    }

    /// Rasterize one mask sprite: opaque, unshaded, nearest-sampled, clipped
    /// to the framebuffer. The sprite's size equals its sampled region, so
    /// every screen pixel maps to exactly one page texel. A transparent texel
    /// (zero alpha, the page's black key colour) is a hole: it leaves the
    /// framebuffer pixel underneath untouched.
    fn rasterize_mask(&mut self, page: &Image, quad: &MaskQuad) {
        if page.width == 0 || page.height == 0 || quad.size.0 == 0 || quad.size.1 == 0 {
            return;
        }

        let x0 = i64::from(quad.pos.0);
        let y0 = i64::from(quad.pos.1);
        let start_x = x0.max(0);
        let start_y = y0.max(0);
        let end_x = (x0 + i64::from(quad.size.0)).min(i64::from(self.width));
        let end_y = (y0 + i64::from(quad.size.1)).min(i64::from(self.height));
        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let page_width = i64::from(page.width);
        let page_height = i64::from(page.height);
        let u0 = i64::from(quad.uv.0);
        let v0 = i64::from(quad.uv.1);
        let source_stride = page.width as usize * 4;
        let target_stride = self.width as usize * 4;
        for y in start_y..end_y {
            let texel_y = v0 + (y - y0);
            if !(0..page_height).contains(&texel_y) {
                continue;
            }
            for x in start_x..end_x {
                let texel_x = u0 + (x - x0);
                if !(0..page_width).contains(&texel_x) {
                    continue;
                }
                let source = texel_y as usize * source_stride + texel_x as usize * 4;
                let target = y as usize * target_stride + x as usize * 4;
                let (Some(texel), Some(slot)) = (
                    page.rgba.get(source..source + 4),
                    self.rgba.get_mut(target..target + 4),
                ) else {
                    continue;
                };
                if texel[3] == 0 {
                    continue;
                }
                slot[0] = texel[0];
                slot[1] = texel[1];
                slot[2] = texel[2];
                slot[3] = 255;
            }
        }
    }
}

/// Draw one frame of a parsed door animation over black.
///
/// Sets up the door VM camera (from/to points and focal length), draws every
/// order whose flags submit a draw (bit `0x8000` plus a low nibble of 1, 2 or
/// 3) with the door texture full-bright, then blends the frame's fade overlay.
/// The background stays black because the room behind the animation is hidden.
pub fn draw_door_scene(
    framebuffer: &mut Framebuffer,
    dor: &crate::door::Dor,
    frame: &crate::door::vm::Frame<'_>,
) {
    framebuffer.clear();
    let camera = Camera::from_points(frame.camera.from, frame.camera.to, frame.camera.focal);
    let objects = frame.orders.iter().filter_map(|order| {
        let draws = order.flags & 0x8000 != 0 && matches!(order.flags & 0xF, 1..=3);
        if !draws {
            return None;
        }
        order.mesh.map(|mesh| (mesh, &order.matrix))
    });
    framebuffer.draw_objects_unlit(objects, &dor.texture, &camera);
    draw_fade_overlay(framebuffer, &frame.fade);
}

/// Blend a full-screen fade overlay over the framebuffer.
///
/// `fade_type` 1 is a white flash and 2 (or anything else) is black. The
/// overlay alpha is `state >> 7` clamped to 0..=255, matching the original's
/// fading-rect brightness.
pub fn draw_fade_overlay(framebuffer: &mut Framebuffer, fade: &crate::door::vm::Fade) {
    let alpha = (fade.state >> 7).clamp(0, 255) as u32;
    if alpha == 0 {
        return;
    }
    let colour: [u8; 3] = if fade.fade_type == 1 {
        [255, 255, 255]
    } else {
        [0, 0, 0]
    };
    for pixel in framebuffer.rgba.as_chunks_mut::<4>().0 {
        for (channel, &target) in pixel.iter_mut().zip(&colour) {
            *channel =
                ((u32::from(*channel) * (255 - alpha) + u32::from(target) * alpha) / 255) as u8;
        }
    }
}

/// A posed player model ready to be interleaved with a room's mask layer.
#[derive(Debug, Clone, Copy)]
pub struct PlayerMesh<'a> {
    /// The TMD mesh to draw.
    pub mesh: &'a Tmd,
    /// The mesh's texture page.
    pub texture: &'a Texture8,
    /// One 4.12 joint matrix per mesh object, in object order.
    pub joints: &'a [anim::Mat4x3],
}

/// One camera's room-mask layer: its decoded page plus the ordering inputs.
///
/// The engine loads the camera's `roommask/{room}_{camera:03}.bmp` entry from
/// the pack, decodes it with [`crate::bmp::decode_mask`] (which cuts the
/// page's black key colour to transparent) and pairs it with the camera's
/// [`Cut`]. [`MaskLayer::new`] starts from the cut's own
/// `mask_active` bits; a script toggle can pass an updated `active` instead.
#[derive(Debug, Clone, Copy)]
pub struct MaskLayer<'a> {
    /// Room identity, used to select the per-(room, camera) ordering records.
    pub room: RoomId,
    /// Camera index within the room.
    pub camera: usize,
    /// The camera's parsed cut, carrying its mask sprites in file order.
    pub cut: &'a Cut,
    /// The decoded mask page; sprite UVs index it directly.
    pub page: &'a Image,
    /// Visibility bits, one per one-based group id.
    pub active: u32,
}

impl<'a> MaskLayer<'a> {
    /// A layer whose active bits start at the cut's own `mask_active`.
    pub fn new(room: RoomId, camera: usize, cut: &'a Cut, page: &'a Image) -> Self {
        Self {
            room,
            camera,
            cut,
            page,
            active: cut.mask_active,
        }
    }
}

/// Draw one gameplay frame: the background, then the depth-sorted scene list.
///
/// The player's triangles are submitted first, pre-sorted far-to-near so their
/// exact depth order survives the integer keys, then the masks in
/// [`mask::mask_submission_order`] (the order the original paints equal-key
/// sprites in). [`mask::order_far_to_near`] is the stable sort, so equal keys
/// keep this submission order: a mask whose key ties a triangle follows it and
/// paints over the player, which is how the original's strictly-farther mask
/// flush resolves the tie. Inactive groups and hidden entries never reach the
/// list.
pub fn draw_gameplay_scene(
    framebuffer: &mut Framebuffer,
    background: Option<&Image>,
    player: Option<&PlayerMesh<'_>>,
    camera: &Camera,
    lighting: &Lighting,
    mask_layer: Option<&MaskLayer<'_>>,
) {
    framebuffer.clear();
    if let Some(background) = background {
        framebuffer.blit(background);
    }

    let mut items = Vec::new();

    let mut triangles = Vec::new();
    if let Some(player) = player {
        for (object, joint) in player.mesh.objects.iter().zip(player.joints) {
            collect_triangles(object, joint, camera, Some(lighting), true, &mut triangles);
        }
    }
    // The integer scene key can tie triangles that are less than a unit apart;
    // the stable sort keeps this far-to-near order for those ties.
    triangles.sort_by(|a, b| b.depth.total_cmp(&a.depth));
    items.extend(triangles.into_iter().map(SceneItem::Triangle));

    if let Some(layer) = mask_layer {
        collect_masks(layer, &mut items);
    }

    mask::order_far_to_near(&mut items, SceneItem::key);

    framebuffer.draw_scene(
        player.map(|player| player.texture),
        mask_layer.map(|layer| layer.page),
        &items,
    );
}

/// Append a layer's active mask sprites to `items`, farthest submission
/// first.
///
/// [`mask::mask_sprite_key`] folds together the shared per-(room, camera) bias
/// record, the room's per-entry overrides and the zero-depth rule (a computed
/// brightness depth of exactly zero sorts at the fixed key 550).
fn collect_masks(layer: &MaskLayer<'_>, items: &mut Vec<SceneItem>) {
    for index in mask::mask_submission_order(layer.room, layer.camera, layer.cut.masks.len()) {
        let Some(sprite) = layer.cut.masks.get(index) else {
            continue;
        };
        if !mask::group_active(layer.active, sprite.group) {
            continue;
        }
        let Some(key) = mask::mask_sprite_key(layer.room, layer.camera, index, sprite) else {
            continue;
        };
        items.push(SceneItem::Mask(MaskQuad {
            key,
            uv: (u32::from(sprite.uv.0), u32::from(sprite.uv.1)),
            pos: sprite.pos,
            size: (u32::from(sprite.size.0), u32::from(sprite.size.1)),
        }));
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
    /// Build the view rotation and translation from a from/to point pair. The
    /// focal length is `fov`.
    ///
    /// This is the door animation's `CAM_MATRIX` camera as well as the room
    /// cut camera: the same from/look-at construction the original's
    /// `MatrixToCamera` performs, with the scene's focal length in pixels.
    pub fn from_points(from: [i32; 3], to: [i32; 3], fov: i32) -> Self {
        let from = [f64::from(from[0]), f64::from(from[1]), f64::from(from[2])];
        let to = [f64::from(to[0]), f64::from(to[1]), f64::from(to[2])];
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
        Self { view, trans, fov }
    }

    /// Build the view rotation and translation from the cut's position and
    /// look-at point. The focal length is `cut.fov`.
    pub fn from_cut(cut: &Cut) -> Self {
        Self::from_points(cut.pos, cut.look_at, cut.fov)
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
    /// Painter's key shared with the mask sprites; see [`triangle_depth_key`].
    key: u32,
    palette_row: usize,
    cull: bool,
}

/// One mask sprite of a camera's page, ready for rasterization.
#[derive(Debug, Clone, Copy)]
struct MaskQuad {
    /// Painter's key from [`mask::mask_sprite_key`].
    key: u32,
    /// Top-left texel of the sprite in the page bitmap.
    uv: (u32, u32),
    /// Top-left screen pixel.
    pos: (i32, i32),
    /// Sprite size in pixels; equals the sampled region, so sampling is 1:1.
    size: (u32, u32),
}

/// One item of a frame's painter's list.
enum SceneItem {
    Mask(MaskQuad),
    Triangle(Triangle),
}

impl SceneItem {
    /// The item's far-to-near key; larger keys are farther away.
    fn key(&self) -> u32 {
        match self {
            SceneItem::Mask(quad) => quad.key,
            SceneItem::Triangle(triangle) => triangle.key,
        }
    }
}

/// The painter's key of one player triangle: its mean view-space Z.
///
/// The key needs no scaling to sit next to [`mask::mask_sprite_key`]: the
/// original submits an overlay at `fade << 4` and a triangle enters the same
/// ordering table at `mean_z >> 4`, which is exactly the port's
/// `OT index * 16` depth-sort scale. This engine sorts on integer keys, so the
/// mean is rounded to the nearest unit; negative depths cannot reach here (the
/// near plane drops them), but clamp anyway so a key can never wrap.
fn triangle_depth_key(depth: f64) -> u32 {
    if depth <= 0.0 {
        0
    } else {
        depth.round() as u32
    }
}

/// Project and shade one TMD object into `triangles`.
///
/// `lighting` selects the gameplay path (room lights, the primitive's own
/// palette row and paged UVs); `None` is the full-bright door path (white
/// shade, palette row 0, direct UVs). `cull` drops back-facing triangles; the
/// door path disables it because the original renders with culling off.
fn collect_triangles(
    object: &TmdObject,
    joint: &anim::Mat4x3,
    camera: &Camera,
    lighting: Option<&Lighting>,
    cull: bool,
    triangles: &mut Vec<Triangle>,
) {
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
        let normal0 = normals.get(usize::from(prim.normals[0])).copied().flatten();
        let normal1 = normals.get(usize::from(prim.normals[1])).copied().flatten();
        let normal2 = normals.get(usize::from(prim.normals[2])).copied().flatten();
        if lighting.is_some() && (normal0.is_none() || normal1.is_none() || normal2.is_none()) {
            continue;
        }
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
        if cull && !faces_camera(area) {
            continue;
        }

        let depth0 = f64::from(camera.view_position(*vertex0)[2]);
        let depth1 = f64::from(camera.view_position(*vertex1)[2]);
        let depth2 = f64::from(camera.view_position(*vertex2)[2]);

        let (page_x, page_y, palette_row) = match lighting {
            Some(_) => (
                f64::from(u32::from(prim.tsb & 0xF) * 128),
                f64::from(u32::from((prim.tsb >> 4) & 1) * 256),
                usize::from(((prim.clut >> 6) & 0x01FF).saturating_sub(480)),
            ),
            // The door TIM is a single 128x256 page with one CLUT row and its
            // UVs are direct, so neither the primitive's page bits nor its
            // VRAM CLUT word offset the lookup.
            None => (0.0, 0.0, 0),
        };

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
                shade: match lighting {
                    Some(lighting) => shade_vertex(&normal, *vertex, lighting),
                    None => [CHANNEL_MAX; 3],
                },
            }
        });

        let depth = (depth0 + depth1 + depth2) / 3.0;
        triangles.push(Triangle {
            raster,
            depth,
            key: triangle_depth_key(depth),
            palette_row,
            cull,
        });
    }
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

    /// The standard screen-facing triangle, its plane at view-space Z `z`.
    fn mesh_at(z: i16, reversed: bool) -> Tmd {
        let mut vertices = vec![[0i16, 0, z], [0, 1000, z], [1000, 0, z]];
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

    fn mesh(reversed: bool) -> Tmd {
        mesh_at(1000, reversed)
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

    fn room() -> RoomId {
        RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        }
    }

    fn solid_image(width: u32, height: u32, color: [u8; 4]) -> Image {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            rgba.extend_from_slice(&color);
        }
        Image {
            width,
            height,
            rgba,
        }
    }

    fn mask_sprite(pos: (i32, i32), pos_data: u16, group: u8) -> crate::mask::MaskSprite {
        crate::mask::MaskSprite {
            uv: (0, 0),
            pos,
            size: (1, 1),
            pos_data,
            flags: 0,
            group,
        }
    }

    fn scene_triangle(key: u32) -> SceneItem {
        SceneItem::Triangle(Triangle {
            raster: std::array::from_fn(|_| RasterVertex {
                position: [0.0; 2],
                inv_z: 1.0,
                u: 0.0,
                v: 0.0,
                shade: [0.0; 3],
            }),
            depth: f64::from(key),
            key,
            palette_row: 0,
            cull: false,
        })
    }

    fn framebuffer_pixel(framebuffer: &Framebuffer, x: usize, y: usize) -> [u8; 4] {
        let offset = (y * framebuffer.width as usize + x) * 4;
        framebuffer.rgba[offset..offset + 4].try_into().unwrap()
    }

    #[test]
    fn triangle_keys_use_the_mask_ordering_table_scale() {
        assert_eq!(triangle_depth_key(-5.0), 0);
        assert_eq!(triangle_depth_key(0.0), 0);
        assert_eq!(triangle_depth_key(6080.0), 6080);
        assert_eq!(triangle_depth_key(6080.4), 6080);
        assert_eq!(triangle_depth_key(6080.5), 6081);
        assert_eq!(triangle_depth_key(f64::INFINITY), u32::MAX);
    }

    #[test]
    fn scene_list_is_stable_and_puts_equal_depth_masks_after_triangles() {
        // Room 100 walks its entries backwards, so the sprites are submitted
        // 2, 1, 0. Sprite 0 carries the far key; sprites 1 and 2 tie with the
        // triangle at 64. The triangle is submitted first, as the gameplay
        // path does, so on a tie the masks paint over the player.
        let cut = Cut {
            masks: vec![
                mask_sprite((0, 0), 100, 1),
                mask_sprite((1, 0), 4, 1),
                mask_sprite((2, 0), 7, 1),
            ],
            mask_active: 0b111,
            ..Cut::default()
        };
        let page = solid_image(1, 1, [1, 2, 3, 255]);
        let layer = MaskLayer::new(room(), 0, &cut, &page);

        let mut items = vec![scene_triangle(64)];
        collect_masks(&layer, &mut items);
        mask::order_far_to_near(&mut items, SceneItem::key);

        let order: Vec<(u32, Option<(i32, i32)>)> = items
            .iter()
            .map(|item| match item {
                SceneItem::Mask(quad) => (quad.key, Some(quad.pos)),
                SceneItem::Triangle(triangle) => (triangle.key, None),
            })
            .collect();
        assert_eq!(
            order,
            vec![
                (1600, Some((0, 0))),
                (64, None),
                (64, Some((2, 0))),
                (64, Some((1, 0))),
            ]
        );
    }

    #[test]
    fn mask_layer_skips_inactive_groups() {
        let cut = Cut {
            masks: vec![mask_sprite((0, 0), 100, 1), mask_sprite((1, 0), 200, 2)],
            mask_active: 0b01,
            ..Cut::default()
        };
        let page = solid_image(1, 1, [1, 2, 3, 255]);
        let mut items = Vec::new();
        collect_masks(&MaskLayer::new(room(), 0, &cut, &page), &mut items);
        assert!(matches!(&items[..], [SceneItem::Mask(quad)] if quad.pos == (0, 0)));

        // Activating group 2 adds its sprite; deactivating group 1 hides the
        // first one again.
        let mut active = cut.mask_active;
        mask::set_group_active(&mut active, 2, true);
        let mut items = Vec::new();
        collect_masks(
            &MaskLayer {
                active,
                ..MaskLayer::new(room(), 0, &cut, &page)
            },
            &mut items,
        );
        assert_eq!(items.len(), 2);

        mask::set_group_active(&mut active, 1, false);
        let mut items = Vec::new();
        collect_masks(
            &MaskLayer {
                active,
                ..MaskLayer::new(room(), 0, &cut, &page)
            },
            &mut items,
        );
        assert!(matches!(&items[..], [SceneItem::Mask(quad)] if quad.pos == (1, 0)));
    }

    /// Render the synthetic frame: a background, a 20x20 mask at (170, 90)
    /// and the standard triangle (mean depth 1000, covering pixel 180,100).
    /// The mask's page texel (10, 10) is distinct from the rest of the page.
    fn interleaved_frame(mask_pos_data: u16) -> Framebuffer {
        interleaved_frame_with(mask_pos_data, &mesh(false))
    }

    fn interleaved_frame_with(mask_pos_data: u16, mesh: &Tmd) -> Framebuffer {
        let cut = Cut {
            masks: vec![crate::mask::MaskSprite {
                uv: (0, 0),
                pos: (170, 90),
                size: (20, 20),
                pos_data: mask_pos_data,
                flags: 0,
                group: 1,
            }],
            mask_active: 1,
            ..Cut::default()
        };
        let mut page = solid_image(20, 20, [1, 2, 3, 255]);
        let texel = (10 * 20 + 10) * 4;
        page.rgba[texel..texel + 4].copy_from_slice(&[9, 8, 7, 255]);
        let layer = MaskLayer::new(room(), 0, &cut, &page);

        let background = solid_image(320, 240, [7, 8, 9, 255]);
        let joints = [identity()];
        let texture = solid_texture([10, 20, 30, 255]);
        let player = PlayerMesh {
            mesh,
            texture: &texture,
            joints: &joints,
        };
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };

        let mut framebuffer = Framebuffer::new();
        draw_gameplay_scene(
            &mut framebuffer,
            Some(&background),
            Some(&player),
            &straight_camera(),
            &lighting,
            Some(&layer),
        );
        framebuffer
    }

    #[test]
    fn near_player_paints_over_a_far_mask() {
        // Mask key 6080 is behind the triangle's key of 1000.
        let framebuffer = interleaved_frame(380);
        assert_eq!(framebuffer_pixel(&framebuffer, 180, 100), [10, 20, 30, 255]);
        // Neither item covers this background pixel.
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [7, 8, 9, 255]);
    }

    #[test]
    fn equal_depth_mask_paints_over_the_player() {
        // Mask pos_data 64 gives brightness 16, depth 16 and fade 64, so its
        // key is 1024. A player triangle with mean view-space Z 1024 produces
        // the same integer key; the original flushes only strictly-farther
        // masks before a triangle, so the mask is drawn after it and wins.
        let framebuffer = interleaved_frame_with(64, &mesh_at(1024, false));
        assert_eq!(framebuffer_pixel(&framebuffer, 180, 100), [9, 8, 7, 255]);
    }

    #[test]
    fn near_mask_occludes_the_player_and_samples_the_page_texel() {
        // Mask key 64 is in front of the triangle's key of 1000; the pixel
        // must sample page texel (10, 10), not the page's common colour.
        let framebuffer = interleaved_frame(4);
        assert_eq!(framebuffer_pixel(&framebuffer, 180, 100), [9, 8, 7, 255]);
        // A triangle pixel outside the 20x20 mask keeps the player colour.
        assert_eq!(framebuffer_pixel(&framebuffer, 220, 110), [10, 20, 30, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [7, 8, 9, 255]);
    }

    #[test]
    fn zero_key_masks_sort_at_the_550_quirk() {
        // pos_data 0 computes a zero key, which the original draws at the
        // fixed 550 slot: still in front of the triangle's 1000.
        let framebuffer = interleaved_frame(0);
        assert_eq!(framebuffer_pixel(&framebuffer, 180, 100), [9, 8, 7, 255]);

        let cut = Cut {
            masks: vec![mask_sprite((0, 0), 0, 1)],
            mask_active: 1,
            ..Cut::default()
        };
        let page = solid_image(1, 1, [1, 2, 3, 255]);
        let mut items = Vec::new();
        collect_masks(&MaskLayer::new(room(), 0, &cut, &page), &mut items);
        assert!(matches!(&items[0], SceneItem::Mask(quad) if quad.key == 550));
    }

    #[test]
    fn mask_sprites_clip_to_the_framebuffer() {
        let cut = Cut {
            masks: vec![crate::mask::MaskSprite {
                uv: (0, 0),
                pos: (-5, -5),
                size: (10, 10),
                pos_data: 4,
                flags: 0,
                group: 1,
            }],
            mask_active: 1,
            ..Cut::default()
        };
        let mut page = solid_image(10, 10, [1, 2, 3, 255]);
        let texel = (5 * 10 + 5) * 4;
        page.rgba[texel..texel + 4].copy_from_slice(&[9, 9, 9, 255]);
        let layer = MaskLayer::new(room(), 0, &cut, &page);
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [Light::default(); 3],
        };

        let mut framebuffer = Framebuffer::new();
        draw_gameplay_scene(
            &mut framebuffer,
            None,
            None,
            &straight_camera(),
            &lighting,
            Some(&layer),
        );

        // Screen (0, 0) is the sprite's top-left pixel and maps to page
        // texel (5, 5); the sprite ends at screen (4, 4).
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [9, 9, 9, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 4, 4), [1, 2, 3, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 5, 4), [0, 0, 0, 0]);
        assert_eq!(framebuffer_pixel(&framebuffer, 319, 239), [0, 0, 0, 0]);
    }

    #[test]
    fn transparent_mask_texels_leave_the_framebuffer_untouched() {
        let cut = Cut {
            masks: vec![crate::mask::MaskSprite {
                uv: (0, 0),
                pos: (10, 10),
                size: (2, 1),
                pos_data: 4,
                flags: 0,
                group: 1,
            }],
            mask_active: 1,
            ..Cut::default()
        };
        // One opaque page texel and one transparent (keyed) texel.
        let mut page = solid_image(2, 1, [0, 0, 0, 0]);
        page.rgba[0..4].copy_from_slice(&[9, 8, 7, 255]);
        let layer = MaskLayer::new(room(), 0, &cut, &page);

        let background = solid_image(320, 240, [7, 8, 9, 255]);
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [Light::default(); 3],
        };

        let mut framebuffer = Framebuffer::new();
        draw_gameplay_scene(
            &mut framebuffer,
            Some(&background),
            None,
            &straight_camera(),
            &lighting,
            Some(&layer),
        );

        // The opaque texel paints over the background; the transparent one is
        // a hole and keeps it.
        assert_eq!(framebuffer_pixel(&framebuffer, 10, 10), [9, 8, 7, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 11, 10), [7, 8, 9, 255]);
    }

    fn indexed_texture(indices: Vec<u8>, entries: &[[u8; 4]]) -> Texture8 {
        let mut palettes = vec![[0u8; 4]; crate::model::PALETTE_ROW_LEN];
        for (index, entry) in entries.iter().enumerate() {
            palettes[index] = *entry;
        }
        Texture8 {
            width: 2,
            height: 2,
            indices,
            palettes,
        }
    }

    fn checkered_texture() -> Texture8 {
        indexed_texture(
            vec![0, 1, 2, 3],
            &[
                [0, 0, 0, 0],
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
            ],
        )
    }

    #[test]
    fn indexed_sprite_scales_with_nearest_sampling() {
        let texture = checkered_texture();
        let mut framebuffer = Framebuffer::new();

        framebuffer.draw_indexed_sprite(&texture, [0, 0, 2, 2], [0, 0, 4, 4], 0, 2, Tint::White);

        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [0, 0, 0, 0]);
        assert_eq!(framebuffer_pixel(&framebuffer, 1, 0), [0, 0, 0, 0]);
        assert_eq!(framebuffer_pixel(&framebuffer, 2, 0), [255, 0, 0, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 3, 1), [255, 0, 0, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 2), [0, 255, 0, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 2, 2), [0, 0, 255, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 4, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn indexed_sprite_clips_to_the_framebuffer() {
        let texture = checkered_texture();
        let mut framebuffer = Framebuffer::new();

        framebuffer.draw_indexed_sprite(&texture, [0, 0, 2, 2], [-1, -1, 4, 4], 0, 2, Tint::White);

        // The top-left texel is transparent, so the clipped screen corner
        // stays untouched; the sprite ends at screen (3, 3) exclusive.
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [0, 0, 0, 0]);
        assert_eq!(framebuffer_pixel(&framebuffer, 3, 3), [0, 0, 0, 0]);
        // Everything outside the sprite is untouched too.
        assert_eq!(framebuffer_pixel(&framebuffer, 4, 4), [0, 0, 0, 0]);
    }

    #[test]
    fn indexed_sprite_applies_tint_and_brightness() {
        let texture = indexed_texture(vec![1], &[[0, 0, 0, 0], [255, 255, 255, 255]]);

        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_indexed_sprite(&texture, [0, 0, 1, 1], [0, 0, 1, 1], 0, 2, Tint::Green);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [0, 255, 0, 255]);

        framebuffer.draw_indexed_sprite(&texture, [0, 0, 1, 1], [1, 0, 1, 1], 0, 2, Tint::Grey);
        assert_eq!(framebuffer_pixel(&framebuffer, 1, 0), [204, 204, 204, 255]);

        // Brightness 0 is the same full-brightness render as 2; 15 is dimmed.
        framebuffer.draw_indexed_sprite(&texture, [0, 0, 1, 1], [2, 0, 1, 1], 0, 0, Tint::White);
        assert_eq!(framebuffer_pixel(&framebuffer, 2, 0), [255, 255, 255, 255]);
        framebuffer.draw_indexed_sprite(&texture, [0, 0, 1, 1], [3, 0, 1, 1], 0, 15, Tint::White);
        assert_eq!(framebuffer_pixel(&framebuffer, 3, 0), [127, 127, 127, 255]);
    }

    #[test]
    fn rgba_sprite_skips_zero_alpha_and_clips() {
        let mut image = solid_image(2, 2, [10, 20, 30, 255]);
        image.rgba[0..4].copy_from_slice(&[0, 0, 0, 0]);
        let mut framebuffer = Framebuffer::new();

        framebuffer.draw_rgba_sprite(&image, [0, 0, 2, 2], [5, 5, 2, 2], 2);

        assert_eq!(framebuffer_pixel(&framebuffer, 5, 5), [0, 0, 0, 0]);
        assert_eq!(framebuffer_pixel(&framebuffer, 6, 5), [10, 20, 30, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 7, 7), [0, 0, 0, 0]);

        // A negative destination clips instead of panicking; the second pass
        // paints over the transparent corner from the first.
        framebuffer.draw_rgba_sprite(&image, [0, 0, 2, 2], [-10, -10, 20, 20], 2);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [10, 20, 30, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 5, 5), [10, 20, 30, 255]);
    }

    #[test]
    fn pending_sprites_keep_submission_order_for_equal_depths() {
        let red = indexed_texture(vec![1], &[[0, 0, 0, 0], [255, 0, 0, 255]]);
        let green = indexed_texture(vec![1], &[[0, 0, 0, 0], [0, 255, 0, 255]]);
        let mut framebuffer = Framebuffer::new();
        let mut sprites = vec![
            SpriteDraw::indexed(&red, [0, 0, 1, 1], [0, 0, 1, 1], 0, 2, Tint::White),
            SpriteDraw::indexed(&green, [0, 0, 1, 1], [0, 0, 1, 1], 0, 2, Tint::White),
        ];
        assert_eq!(pending_sprite_depth(2), 482);

        framebuffer.draw_sprites(&mut sprites);

        // Equal depths keep submission order; the later sprite paints over.
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [0, 255, 0, 255]);
    }

    #[test]
    fn pending_sprites_paint_farther_depths_first() {
        let red = indexed_texture(vec![1], &[[0, 0, 0, 0], [255, 0, 0, 255]]);
        let green = indexed_texture(vec![1], &[[0, 0, 0, 0], [0, 255, 0, 255]]);
        let mut framebuffer = Framebuffer::new();
        // The far (brighter depth) sprite is submitted last but paints first.
        let mut sprites = vec![
            SpriteDraw::indexed(&red, [0, 0, 1, 1], [0, 0, 1, 1], 0, 2, Tint::White),
            SpriteDraw::indexed(&green, [0, 0, 1, 1], [0, 0, 1, 1], 0, 4, Tint::White),
        ];

        framebuffer.draw_sprites(&mut sprites);

        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pending_sprite_depth(30), 930);
    }

    #[test]
    fn tint_table_matches_the_original() {
        assert_eq!(Tint::from_clut_row(0), Tint::White);
        assert_eq!(Tint::from_clut_row(1), Tint::Green);
        assert_eq!(Tint::from_clut_row(2), Tint::Red);
        assert_eq!(Tint::from_clut_row(3), Tint::Grey);
        assert_eq!(Tint::from_clut_row(8), Tint::Green);
        assert_eq!(Tint::from_clut_row(4), Tint::Yellow);
        assert_eq!(Tint::from_clut_row(-1), Tint::Yellow);
        assert_eq!(Tint::Grey.rgb(), [204, 204, 204]);
        assert_eq!(Tint::Yellow.rgb(), [255, 255, 0]);
    }
}
