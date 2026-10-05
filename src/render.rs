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
//! page, then pass the layer to [`draw_gameplay_scene`]. An [`EffectLayer`]
//! carries the room's composited effect pages and this frame's
//! [`EffectQuad`]s, submitted after the masks so an exact key tie paints the
//! billboard over the model. [`Framebuffer::draw_model`] remains the plain
//! player-only path used by the tests and the non-mask case, and
//! [`Framebuffer::draw_ivm_unlit`] is the item viewer's full-bright path over
//! an `.ivm` mesh and its own texture page.

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
fn brightness_scale(brightness: u8) -> u8 {
    if brightness == 0 || brightness == 2 {
        255
    } else {
        (u32::from(brightness) * 255 / 30).min(255) as u8
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
        self.draw_indexed_sprite_scaled(
            texture,
            src,
            dst,
            clut_row,
            brightness_scale(brightness),
            tint,
        );
    }

    /// Draw one indexed sprite with a direct `0..=255` colour multiplier.
    ///
    /// Unlike [`Framebuffer::draw_indexed_sprite`] the scale is not the font's
    /// `0..=30` brightness: `0` is black and `255` is full, which is what the
    /// screens need for fades.
    pub fn draw_indexed_sprite_scaled(
        &mut self,
        texture: &Texture8,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        scale: u8,
        tint: Tint,
    ) {
        if texture.width == 0 || texture.height == 0 {
            return;
        }
        let scale = u32::from(scale);
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
        self.draw_rgba_sprite_scaled(image, src, dst, brightness_scale(brightness));
    }

    /// Draw one RGBA sprite with a direct `0..=255` colour multiplier.
    ///
    /// Like [`Framebuffer::draw_indexed_sprite_scaled`], `0` is black rather
    /// than the font's full-brightness zero.
    pub fn draw_rgba_sprite_scaled(
        &mut self,
        image: &Image,
        src: [i32; 4],
        dst: [i32; 4],
        scale: u8,
    ) {
        if image.width == 0 || image.height == 0 {
            return;
        }
        let scale = u32::from(scale);
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

    /// Copy another 320x240 framebuffer over this one.
    pub fn copy_from(&mut self, other: &Framebuffer) {
        if self.width == other.width
            && self.height == other.height
            && self.rgba.len() == other.rgba.len()
        {
            self.rgba.copy_from_slice(&other.rgba);
        }
    }

    /// Blend the whole frame towards black by `alpha`/255.
    ///
    /// This is the screens' fade overlay: `0` leaves the frame untouched and
    /// `255` paints it black. Alpha is preserved.
    pub fn fade_to_black(&mut self, alpha: u8) {
        if alpha == 0 {
            return;
        }
        if alpha == 255 {
            for pixel in self.rgba.as_chunks_mut::<4>().0 {
                pixel[..3].fill(0);
            }
            return;
        }
        let keep = u32::from(255 - alpha);
        for pixel in self.rgba.as_chunks_mut::<4>().0 {
            for channel in &mut pixel[..3] {
                *channel = (u32::from(*channel) * keep / 255) as u8;
            }
        }
    }

    /// Blend a rectangle towards black by `alpha`/255, clipped to the frame.
    pub fn blend_black_rect(&mut self, dst: [i32; 4], alpha: u8) {
        let [x, y, width, height] = dst;
        if width <= 0 || height <= 0 || alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.fill_rect(dst, [0, 0, 0, 255]);
            return;
        }
        let start_x = x.max(0) as u32;
        let start_y = y.max(0) as u32;
        let end_x = x.saturating_add(width).min(self.width as i32).max(0) as u32;
        let end_y = y.saturating_add(height).min(self.height as i32).max(0) as u32;
        let keep = u32::from(255 - alpha);
        for row in start_y..end_y {
            for column in start_x..end_x {
                let offset = (row as usize * self.width as usize + column as usize) * 4;
                if let Some(pixel) = self.rgba.get_mut(offset..offset + 4) {
                    for channel in &mut pixel[..3] {
                        *channel = (u32::from(*channel) * keep / 255) as u8;
                    }
                }
            }
        }
    }

    /// Fill a rectangle with `color`, clipped to the framebuffer.
    pub fn fill_rect(&mut self, dst: [i32; 4], color: [u8; 4]) {
        let [x, y, width, height] = dst;
        let start_x = x.max(0) as u32;
        let start_y = y.max(0) as u32;
        let end_x = x.saturating_add(width).min(self.width as i32).max(0) as u32;
        let end_y = y.saturating_add(height).min(self.height as i32).max(0) as u32;
        for row in start_y..end_y {
            for column in start_x..end_x {
                let offset = (row as usize * self.width as usize + column as usize) * 4;
                if let Some(pixel) = self.rgba.get_mut(offset..offset + 4) {
                    pixel.copy_from_slice(&color);
                }
            }
        }
    }

    /// Draw a pending list far-to-near.
    ///
    /// The sort is stable on descending [`SpriteDraw::depth`] (higher depths
    /// are farther behind and paint first), so equal-depth sprites keep their
    /// submission order, exactly like the original's pending-sprite queue.
    pub fn draw_sprites(&mut self, sprites: &mut [SpriteDraw<'_>]) {
        // TODO(parity): (visual) the original blends: the pending text shadow
        // is semi-transparent black with alpha = brightness*255/30, and sprite
        // descriptors carry a blend level (often 0.5) plus U/V mirror flags.
        // `SpriteDraw` has no alpha and no mirror, so every queued sprite
        // overwrites its destination.
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
            collect_triangles(
                object,
                joint,
                camera,
                Some(lighting),
                true,
                0,
                &mut triangles,
            );
        }
        self.rasterize_triangles(texture, triangles);
    }

    /// Draw a TMD mesh full-bright, without lighting or backface culling.
    ///
    /// This is the door animation's path: panels are drawn over black with the
    /// door texture's single 256-colour CLUT row and direct (unpaged) UVs.
    /// Triangles are painter-sorted back-to-front exactly like
    /// [`Framebuffer::draw_model`].
    pub fn draw_model_unlit(
        &mut self,
        mesh: &Tmd,
        texture: &Texture8,
        joints: &[anim::Mat4x3],
        camera: &Camera,
    ) {
        // TODO(parity): (visual) `cull` is false here and in the `.ivm` path,
        // but the original backface-culls every TMD packet, doors and item
        // meshes included; a panel that faces away still paints here.
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
            collect_triangles(object, joint, camera, None, false, 0, &mut triangles);
        }
        self.rasterize_triangles(texture, triangles);
    }

    /// Draw an item-view (`.ivm`) model full-bright: the item viewer's path.
    ///
    /// The item mesh keeps its own texture page (one 256-colour CLUT row, so
    /// every polygon samples row 0 with direct UVs) and is drawn with backface
    /// culling disabled, like the door path. Quads split into two fan
    /// triangles; untextured gouraud polygons paint their packet colour as a
    /// solid triangle. One matrix per object, in object order, supplies the
    /// viewer's turntable rotation and distance.
    pub fn draw_ivm_unlit(
        &mut self,
        ivm: &crate::ivm::Ivm,
        joints: &[anim::Mat4x3],
        camera: &Camera,
    ) {
        let mut triangles: Vec<Triangle> = Vec::new();
        for (object, joint) in ivm.objects.iter().zip(joints) {
            collect_ivm_triangles(object, joint, camera, &mut triangles);
        }
        self.rasterize_triangles(&ivm.texture, triangles);
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
                // An untextured gouraud polygon keeps its packet colour; every
                // other polygon samples the texture page through its row.
                let color = match triangle.flat {
                    Some(flat) => [flat[0], flat[1], flat[2], 255],
                    None => {
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
                        texture.palette(triangle.palette_row, palette_index)
                    }
                };

                let shade = [
                    weight0 * a.shade[0] + weight1 * b.shade[0] + weight2 * c.shade[0],
                    weight0 * a.shade[1] + weight1 * b.shade[1] + weight2 * c.shade[1],
                    weight0 * a.shade[2] + weight1 * b.shade[2] + weight2 * c.shade[2],
                ];
                let mut pixel = [
                    channel(color[0], shade[0]),
                    channel(color[1], shade[1]),
                    channel(color[2], shade[2]),
                    255,
                ];
                let offset = (y as usize * width + x as usize) * 4;
                // A packet carrying the ABE command bit uses the effect path's
                // flat half blend (the documented fallback for per-texel STP).
                if triangle.blend
                    && let Some(dst) = self.rgba.get(offset..offset + 4)
                {
                    for (channel, &destination) in pixel[..3].iter_mut().zip(dst) {
                        *channel = ((u16::from(*channel) + u16::from(destination)) / 2) as u8;
                    }
                }
                if let Some(slot) = self.rgba.get_mut(offset..offset + 4) {
                    slot.copy_from_slice(&pixel);
                }
            }
        }
    }

    /// Paint a sorted scene list: each triangle selects its own texture page,
    /// mask sprites the camera's mask page, shadows the baked shadow mask and
    /// effects the room's decoded effect pages.
    fn draw_scene(
        &mut self,
        textures: &[&Texture8],
        page: Option<&Image>,
        shadow_texture: Option<&Image>,
        effect_pages: &[Option<&Image>],
        items: &[SceneItem],
    ) {
        for item in items {
            match item {
                SceneItem::Shadow(poly) => {
                    if let Some(shadow_texture) = shadow_texture {
                        self.rasterize_shadow(shadow_texture, poly);
                    }
                }
                SceneItem::Triangle(triangle) => {
                    if let Some(texture) = textures.get(triangle.texture) {
                        self.rasterize(texture, triangle);
                    }
                }
                SceneItem::Mask(quad) => {
                    if let Some(page) = page {
                        self.rasterize_mask(page, quad);
                    }
                }
                SceneItem::Effect(quad) => {
                    if let Some(page) = effect_pages.get(quad.page).copied().flatten() {
                        self.rasterize_effect(page, quad);
                    }
                }
            }
        }
    }

    /// Rasterize one clipped ground-shadow polygon.
    ///
    /// The convex ring is fanned into triangles and sampled nearest with
    /// perspective-correct UVs, exactly like the model path. A texel's alpha
    /// multiplies the destination towards the polygon's tint; a zero-alpha
    /// texel is a hole and leaves the framebuffer untouched.
    fn rasterize_shadow(&mut self, texture: &Image, poly: &ShadowPoly) {
        if texture.width == 0 || texture.height == 0 || poly.corners.len() < 3 {
            return;
        }
        for corner in 1..poly.corners.len() - 1 {
            self.rasterize_shadow_triangle(texture, poly, [0, corner, corner + 1]);
        }
    }

    fn rasterize_shadow_triangle(
        &mut self,
        texture: &Image,
        poly: &ShadowPoly,
        indices: [usize; 3],
    ) {
        let (a, b, c) = (
            &poly.corners[indices[0]],
            &poly.corners[indices[1]],
            &poly.corners[indices[2]],
        );
        let area = (b.position[0] - a.position[0]) * (c.position[1] - a.position[1])
            - (c.position[0] - a.position[0]) * (b.position[1] - a.position[1]);
        if area == 0.0 {
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
                // The fan's triangles share their diagonals; the top-left
                // rule assigns a sample exactly on a shared edge to one of
                // them, so the translucent quad is not blended twice along
                // the seam.
                let positive = area > 0.0;
                let inside = |weight: f64, from: [f64; 2], to: [f64; 2]| {
                    if weight > 1e-9 {
                        true
                    } else if weight < -1e-9 {
                        false
                    } else {
                        top_left(from, to, positive)
                    }
                };
                if !inside(weight0, b.position, c.position)
                    || !inside(weight1, c.position, a.position)
                    || !inside(weight2, a.position, b.position)
                {
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
                let Some(texel_u) = wrap_texel(u * f64::from(texture.width), texture.width) else {
                    continue;
                };
                let Some(texel_v) = wrap_texel(v * f64::from(texture.height), texture.height)
                else {
                    continue;
                };
                let texel = (texel_v as usize * texture.width as usize + texel_u as usize) * 4;
                let Some(texel) = texture.rgba.get(texel..texel + 4) else {
                    continue;
                };
                let alpha = i64::from(texel[3]);
                if alpha == 0 {
                    continue;
                }
                let offset = (y as usize * width + x as usize) * 4;
                let Some(pixel) = self.rgba.get_mut(offset..offset + 4) else {
                    continue;
                };
                for (channel, &tint) in pixel[..3].iter_mut().zip(&poly.tint) {
                    *channel =
                        ((i64::from(*channel) * (255 - alpha) + i64::from(tint) * alpha + 127)
                            / 255) as u8;
                }
                pixel[3] = 255;
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

    /// Rasterize one effect billboard: nearest-sampled, clipped to the
    /// framebuffer, with texture-entry-zero transparent (the page's zero
    /// alpha), an RGB multiply from the spawn's tint record and a half blend
    /// when the blend record calls for one.
    fn rasterize_effect(&mut self, page: &Image, quad: &EffectQuad) {
        if page.width == 0 || page.height == 0 || quad.size[0] <= 0 || quad.size[1] <= 0 {
            return;
        }

        let x0 = i64::from(quad.pos[0]);
        let y0 = i64::from(quad.pos[1]);
        let start_x = x0.max(0);
        let start_y = y0.max(0);
        let end_x = (x0 + i64::from(quad.size[0])).min(i64::from(self.width));
        let end_y = (y0 + i64::from(quad.size[1])).min(i64::from(self.height));
        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let page_width = page.width as usize;
        let u0 = i64::from(quad.uv[0]);
        let v0 = i64::from(quad.uv[1]);
        let source_w = i64::from(quad.source[0].max(1));
        let source_h = i64::from(quad.source[1].max(1));
        let target_w = i64::from(quad.size[0]);
        let target_h = i64::from(quad.size[1]);
        let half = quad.blend & 0x80 != 0;
        let target_stride = self.width as usize;

        for y in start_y..end_y {
            let texel_y = v0 + (y - y0) * source_h / target_h;
            if !(0..i64::from(page.height)).contains(&texel_y) {
                continue;
            }
            for x in start_x..end_x {
                let texel_x = u0 + (x - x0) * source_w / target_w;
                if !(0..i64::from(page.width)).contains(&texel_x) {
                    continue;
                }
                let source = texel_y as usize * page_width * 4 + texel_x as usize * 4;
                let target = y as usize * target_stride * 4 + x as usize * 4;
                let (Some(texel), Some(slot)) = (
                    page.rgba.get(source..source + 4),
                    self.rgba.get_mut(target..target + 4),
                ) else {
                    continue;
                };
                if texel[3] == 0 {
                    continue;
                }
                let tinted = [
                    (u32::from(texel[0]) * u32::from(quad.tint[0]) / 255) as u8,
                    (u32::from(texel[1]) * u32::from(quad.tint[1]) / 255) as u8,
                    (u32::from(texel[2]) * u32::from(quad.tint[2]) / 255) as u8,
                ];
                if half {
                    for (channel, &source) in slot[..3].iter_mut().zip(&tinted) {
                        *channel = ((u32::from(*channel) + u32::from(source)) / 2) as u8;
                    }
                } else {
                    slot[..3].copy_from_slice(&tinted);
                }
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

/// A posed entity model ready to be interleaved with a room's mask layer.
#[derive(Debug, Clone, Copy)]
pub struct EntityMesh<'a> {
    /// The TMD mesh to draw.
    pub mesh: &'a Tmd,
    /// The mesh's texture page.
    pub texture: &'a Texture8,
    /// One 4.12 joint matrix per mesh object, in object order.
    pub joints: &'a [anim::Mat4x3],
}

/// A ground shadow ready to be interleaved with the masks and the model.
///
/// The quad spans `[-half_x, +half_x] x [-half_z, +half_z]` in the entity's
/// local frame, is yawed by `angle` and placed at `pos` (Y is the floor
/// height). `texture` is the baked coverage page: white texels whose alpha is
/// how far the destination is multiplied towards `tint`. `lift` is the
/// per-(room, camera) view-space Y offset of the placement record.
#[derive(Debug, Clone, Copy)]
pub struct Shadow<'a> {
    /// The baked shadow coverage page: white texels whose alpha is the
    /// darkening amount.
    pub texture: &'a Image,
    /// Ground point under the entity.
    pub pos: [i32; 3],
    /// Entity yaw; the quad rotates with it.
    pub angle: u16,
    /// Half-extent along the entity's local X.
    pub half_x: i32,
    /// Half-extent along the entity's local Z.
    pub half_z: i32,
    /// The placement record's view-space Y offset, added to the primitive's
    /// translation row. Positive values drop the quad down the screen.
    pub lift: i32,
    /// The colour the quad blends towards.
    pub tint: [u8; 3],
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
/// The ground shadow is submitted first, then the meshes' triangles collected
/// in slice order and pre-sorted far-to-near with a stable sort, then the
/// masks in [`mask::mask_submission_order`] (the order the original paints
/// equal-key sprites in). [`mask::order_far_to_near`] is the stable sort, so
/// equal keys keep this submission order: the caller places the NPC meshes in
/// entity-slot order and the player mesh last (the original queues the enemies
/// before the player), so at an exact tie the player paints over an NPC, and a
/// mask whose key ties a triangle follows it and paints over the model, which
/// is how the original's strictly-farther mask flush resolves the tie.
/// Inactive groups and hidden entries never reach the list. Each triangle
/// keeps the index of its mesh's texture page.
pub fn draw_gameplay_scene(
    framebuffer: &mut Framebuffer,
    background: Option<&Image>,
    meshes: &[EntityMesh<'_>],
    shadow: Option<&Shadow<'_>>,
    camera: &Camera,
    lighting: &Lighting,
    mask_layer: Option<&MaskLayer<'_>>,
) {
    draw_gameplay_scene_with_effects(
        framebuffer,
        background,
        meshes,
        shadow,
        camera,
        lighting,
        mask_layer,
        None,
    );
}

/// [`draw_gameplay_scene`] with the room's effect billboards interleaved.
///
/// The effect quads are submitted after the masks (the original's `update_2d_effects`
/// runs after the entity pass), so at an exact key tie an effect paints over a
/// mask or triangle submitted earlier in the frame.
#[allow(clippy::too_many_arguments)]
pub fn draw_gameplay_scene_with_effects(
    framebuffer: &mut Framebuffer,
    background: Option<&Image>,
    meshes: &[EntityMesh<'_>],
    shadow: Option<&Shadow<'_>>,
    camera: &Camera,
    lighting: &Lighting,
    mask_layer: Option<&MaskLayer<'_>>,
    effect_layer: Option<&EffectLayer<'_>>,
) {
    framebuffer.clear();
    if let Some(background) = background {
        // TODO(parity): (visual) the original draws the cut as the display
        // image through the pending-sprite queue, shifted by the display origin
        // (screen panning) and multiplied by the global colour; this blit is
        // 1:1 and untinted.
        framebuffer.blit(background);
    }

    let mut items = Vec::new();

    // The shadow is submitted before the models, so a mask or triangle whose
    // rounded key ties it wins the tie the way the original's later
    // submission into the shared ordering table does.
    if let Some(shadow) = shadow {
        collect_shadow(shadow, camera, &mut items);
    }

    let mut triangles = Vec::new();
    for (texture, mesh) in meshes.iter().enumerate() {
        for (object, joint) in mesh.mesh.objects.iter().zip(mesh.joints) {
            collect_triangles(
                object,
                joint,
                camera,
                Some(lighting),
                true,
                texture,
                &mut triangles,
            );
        }
    }
    // The integer scene key can tie triangles that are less than a unit apart;
    // the stable sort keeps this submission order for those ties.
    triangles.sort_by(|a, b| b.depth.total_cmp(&a.depth));
    items.extend(triangles.into_iter().map(SceneItem::Triangle));

    if let Some(layer) = mask_layer {
        collect_masks(layer, &mut items);
    }

    if let Some(layer) = effect_layer {
        items.extend(layer.quads.iter().copied().map(SceneItem::Effect));
    }

    mask::order_far_to_near(&mut items, SceneItem::key);

    let textures: Vec<&Texture8> = meshes.iter().map(|mesh| mesh.texture).collect();
    let effect_pages: Vec<Option<&Image>> = effect_layer
        .map(|layer| layer.pages.to_vec())
        .unwrap_or_default();
    framebuffer.draw_scene(
        &textures,
        mask_layer.map(|layer| layer.page),
        shadow.map(|shadow| shadow.texture),
        &effect_pages,
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

/// Project and clip one ground shadow into the scene list.
///
/// The four corners are yawed into world space exactly like the model's root
/// joint, clipped against the camera's near plane and projected. The painter's
/// key is the mean view-space Z of the four *unclipped* corners, the same
/// quantity and units a triangle's [`Triangle::depth`] uses. `None` when the
/// whole quad is behind the camera or the texture is empty.
fn collect_shadow(shadow: &Shadow<'_>, camera: &Camera, items: &mut Vec<SceneItem>) {
    if shadow.texture.width == 0 || shadow.texture.height == 0 {
        return;
    }
    let entity = anim::entity_matrix(shadow.pos, shadow.angle);
    let half_x = shadow.half_x as i16;
    let half_z = shadow.half_z as i16;
    let local = [
        [-half_x, 0, half_z],
        [half_x, 0, half_z],
        [-half_x, 0, -half_z],
        [half_x, 0, -half_z],
    ];
    let view: [[i32; 3]; 4] = local.map(|corner| camera.view_position(fixed_mul(&entity, corner)));

    // TODO(parity): (visual) the original's drawn key is the integer mean
    // (truncating `/ 4`) of the four unclipped corner view Zs, quantised to an
    // ordering-table bucket; this f64 mean rounds to the nearest unit, which
    // can flip a shadow's order against a mask or triangle it ties with.
    let mean_depth =
        view.iter().map(|vertex| f64::from(vertex[2])).sum::<f64>() / view.len() as f64;

    // The texture's v axis runs towards the entity's local -Z and u along +X,
    // matching the viewport quad's UV orientation.
    let uv = [[0.0, 0.0], [4096.0, 0.0], [0.0, 4096.0], [4096.0, 4096.0]];
    // The corners are laid out (-x,+z) (+x,+z) (-x,-z) (+x,-z), so the quad's
    // edges are 0->1->3->2. Walking 0->1->2->3 would be a bowtie whose
    // diagonals clip at the wrong places.
    let edge = [0usize, 1, 3, 2];
    let mut polygon: Vec<ShadowVertex> = Vec::with_capacity(5);
    let near = 2.0 * f64::from(camera.fov);
    for corner in 0..4 {
        let index = edge[corner];
        let next = edge[(corner + 1) & 3];
        let a = view[index];
        let b = view[next];
        let a_in = f64::from(a[2]) >= near;
        let b_in = f64::from(b[2]) >= near;
        if a_in {
            polygon.push(project_shadow_vertex(a, uv[index], shadow.lift, camera));
        }
        if a_in != b_in {
            let t = (near - f64::from(a[2])) / f64::from(b[2] - a[2]);
            let lerp =
                |axis: usize| f64::from(a[axis]) + (f64::from(b[axis]) - f64::from(a[axis])) * t;
            let clipped = [lerp(0) as i32, lerp(1) as i32, lerp(2) as i32];
            let uv_clipped = [
                uv[index][0] + (uv[next][0] - uv[index][0]) * t,
                uv[index][1] + (uv[next][1] - uv[index][1]) * t,
            ];
            polygon.push(project_shadow_vertex(
                clipped,
                uv_clipped,
                shadow.lift,
                camera,
            ));
        }
    }
    if polygon.len() < 3 {
        return;
    }

    items.push(SceneItem::Shadow(ShadowPoly {
        key: triangle_depth_key(mean_depth),
        corners: polygon,
        tint: shadow.tint,
    }));
}

/// Project one view-space shadow vertex to screen space.
///
/// `lift` joins the primitive's translation row, the same displacement the
/// original applies to the quad origin: the translation grows downwards, so a
/// positive lift drops the vertex down the screen. u/v stay in the texture's
/// 0..4096 space and are scaled to texels by the rasterizer.
fn project_shadow_vertex(view: [i32; 3], uv: [f64; 2], lift: i32, camera: &Camera) -> ShadowVertex {
    let focal = f64::from(camera.fov);
    let inv_z = 1.0 / f64::from(view[2]);
    ShadowVertex {
        position: [
            CENTER_X + f64::from(view[0]) * focal * inv_z,
            CENTER_Y - (f64::from(view[1]) - f64::from(lift)) * focal * inv_z,
        ],
        inv_z,
        u: uv[0] / 4096.0,
        v: uv[1] / 4096.0,
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
        // TODO(parity): (visual) the original camera is a 4.12 fixed-point
        // matrix projected with truncating integer math (`(vx*f)/vz + 160`),
        // applies the cut's roll and the subpixel screen-shake offset; this
        // f64, round-to-nearest camera ignores roll and shake, so projected
        // pixels can differ by a unit and shake is absent.
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
    pub(crate) fn view_position(&self, world: [i32; 3]) -> [i32; 3] {
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
    /// Index of the mesh's texture page in the frame's texture list.
    texture: usize,
    palette_row: usize,
    cull: bool,
    /// A solid gouraud colour; `None` samples the texture instead.
    flat: Option<[u8; 3]>,
    /// The packet's semi-transparency bit: rasterize with the flat half blend
    /// the effect path uses (the documented fallback for per-texel STP).
    blend: bool,
}

/// One projected shadow vertex: screen position, inverse view Z and the
/// normalized texture coordinates.
#[derive(Debug, Clone, Copy)]
struct ShadowVertex {
    position: [f64; 2],
    inv_z: f64,
    /// Texture u over `0..=1`; the rasterizer scales by the page width.
    u: f64,
    /// Texture v over `0..=1`; the rasterizer scales by the page height.
    v: f64,
}

/// A ground shadow's clipped convex polygon, ready for rasterization.
#[derive(Debug, Clone)]
struct ShadowPoly {
    /// Painter's key from [`triangle_depth_key`].
    key: u32,
    /// Convex ring of three to five vertices in edge order.
    corners: Vec<ShadowVertex>,
    /// The colour the quad blends towards.
    tint: [u8; 3],
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

/// One decoded effect billboard, ready for rasterization.
///
/// The page index selects an entry of [`EffectLayer::pages`]; the UV rect is in
/// page texels; `source` is the billboard's unscaled texel size and `size` its
/// projected screen size, so nearest sampling maps the frame across the quad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectQuad {
    /// Painter's key: `(proj_depth >> 4) * 0x40 - scale_y`.
    pub key: u32,
    /// Index into the frame's effect-page list.
    pub page: usize,
    /// `[u, v, pivot_x, pivot_y]` in page texels.
    pub uv: [u8; 4],
    /// Unscaled billboard texel size.
    pub source: [u8; 2],
    /// Top-left screen pixel after the pivot scale.
    pub pos: [i32; 2],
    /// Scaled screen size in pixels.
    pub size: [i32; 2],
    /// Per-channel tint record multiplier.
    pub tint: [u8; 3],
    /// `0x00` opaque or `0x80` half blend.
    pub blend: u8,
}

/// The effect pages of one frame plus the billboards that sample them.
///
/// `pages` holds the four base texture pages followed by the variant CLUT-row
/// pages (`4 + page * 3 + (row - 1)`); [`EffectQuad::page`] indexes it.
#[derive(Debug, Clone, Copy)]
pub struct EffectLayer<'a> {
    /// Decoded effect texture pages; a missing entry is a page this room does
    /// not have art for.
    pub pages: &'a [Option<&'a Image>],
    /// This frame's live billboards, in pool submission order.
    pub quads: &'a [EffectQuad],
}

/// One item of a frame's painter's list.
enum SceneItem {
    Shadow(ShadowPoly),
    Mask(MaskQuad),
    Triangle(Triangle),
    Effect(EffectQuad),
}

impl SceneItem {
    /// The item's far-to-near key; larger keys are farther away.
    fn key(&self) -> u32 {
        match self {
            SceneItem::Shadow(poly) => poly.key,
            SceneItem::Mask(quad) => quad.key,
            SceneItem::Triangle(triangle) => triangle.key,
            SceneItem::Effect(quad) => quad.key,
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
/// `texture` is the page index every emitted triangle samples.
fn collect_triangles(
    object: &TmdObject,
    joint: &anim::Mat4x3,
    camera: &Camera,
    lighting: Option<&Lighting>,
    cull: bool,
    texture: usize,
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
        // A corner whose stored normal is zero length borrows the primitive's
        // first real normal (flat shading) instead of dropping the packet; the
        // original substitutes and still draws it. A packet with no normal at
        // all is dropped.
        let mut corners_normals = [normal0, normal1, normal2];
        if lighting.is_some() {
            let substitute = corners_normals.iter().flatten().next().copied();
            let Some(substitute) = substitute else {
                continue;
            };
            for normal in &mut corners_normals {
                if normal.is_none() {
                    *normal = Some(substitute);
                }
            }
        }
        let [normal0, normal1, normal2] = corners_normals;
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

        // TODO(parity): (visual) `clut`/`tsb` are read only for the texture page
        // and palette row: the original also blends a primitive whose CLUT
        // carries the ABE bit (background/source halves, or a per-texel STP
        // knockout for palettes with STP entries) and marks some transparent
        // TMDs unlit. This port writes every triangle opaque, and the TIM
        // decoder drops the STP bit.
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
            texture,
            palette_row,
            cull,
            // An untextured packet paints its flat colour.
            flat: prim.flat_color,
            blend: prim.blend,
        });
    }
}

/// Project and full-bright-shade one `.ivm` object into `triangles`.
///
/// Item prims carry their own vertex and normal pools like TMD objects but a
/// richer packet set: textured triangles and quads (quads fan-split into two
/// triangles), flat textured triangles, and untextured gouraud triangles whose
/// packet colour is rasterized as a solid. The viewer draws with culling off
/// and a single palette row, so every corner shades white and samples row 0.
fn collect_ivm_triangles(
    object: &crate::ivm::IvmObject,
    joint: &anim::Mat4x3,
    camera: &Camera,
    triangles: &mut Vec<Triangle>,
) {
    let vertices: Vec<[i32; 3]> = object
        .vertices
        .iter()
        .map(|vertex| fixed_mul(joint, *vertex))
        .collect();

    for prim in &object.prims {
        let order: &[usize] = if prim.vertex_count == 4 {
            &[0, 1, 2, 0, 2, 3]
        } else {
            &[0, 1, 2]
        };
        // TODO(parity): (visual) an untextured gouraud primitive is painted
        // with one packet colour; the original interpolates the packet's three
        // per-vertex colours across the triangle.
        let flat = match prim.kind {
            crate::ivm::IvmPrimKind::Gouraud => Some(prim.color),
            _ => None,
        };
        // TODO(parity): (visual) the original drops the WHOLE packet when any
        // one of its 3/4 corners is inside the near plane; splitting a quad
        // here tests each half separately, so one clipped corner still draws
        // half the quad.
        for corners in order.as_chunks::<3>().0 {
            let (i0, i1, i2) = (corners[0], corners[1], corners[2]);
            let (Some(vertex0), Some(vertex1), Some(vertex2)) = (
                vertices.get(usize::from(prim.vertices[i0])),
                vertices.get(usize::from(prim.vertices[i1])),
                vertices.get(usize::from(prim.vertices[i2])),
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

            let depth0 = f64::from(camera.view_position(*vertex0)[2]);
            let depth1 = f64::from(camera.view_position(*vertex1)[2]);
            let depth2 = f64::from(camera.view_position(*vertex2)[2]);

            let corner_data = [
                (screen0, depth0, i0),
                (screen1, depth1, i1),
                (screen2, depth2, i2),
            ];
            let raster: [RasterVertex; 3] = std::array::from_fn(|corner| {
                let (screen, depth, index) = corner_data[corner];
                RasterVertex {
                    position: [f64::from(screen[0]), f64::from(screen[1])],
                    inv_z: 1.0 / depth,
                    u: f64::from(prim.uv[index][0]),
                    v: f64::from(prim.uv[index][1]),
                    shade: [CHANNEL_MAX; 3],
                }
            });

            let depth = (depth0 + depth1 + depth2) / 3.0;
            triangles.push(Triangle {
                raster,
                depth,
                key: triangle_depth_key(depth),
                texture: 0,
                palette_row: 0,
                cull: false,
                flat,
                blend: false,
            });
        }
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

/// Whether a boundary sample on the directed edge `from -> to` belongs to the
/// triangle under the top-left fill rule.
///
/// `positive` is the winding of the triangle's screen-space area. Two
/// triangles sharing an edge see it in opposite directions, so exactly one of
/// them keeps the samples lying on it.
fn top_left(from: [f64; 2], to: [f64; 2], positive: bool) -> bool {
    let dx = to[0] - from[0];
    let dy = to[1] - from[1];
    if positive {
        dy < 0.0 || (dy == 0.0 && dx > 0.0)
    } else {
        dy > 0.0 || (dy == 0.0 && dx < 0.0)
    }
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
    // TODO(parity): (visual) the original latches the room lights once per
    // entity from that entity's own position, measures point-light falloff from
    // X and Z only, truncates the attenuated colour to a byte capped at 0x80
    // per channel, and truncates the 12-bit ambient to 8 bits. This shader
    // evaluates the lights in world space per vertex with the full 3D distance
    // and unclamped float colour, so an entity's shade drifts from the original.
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
                    textured: true,
                    blend: false,
                    flat_color: None,
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
            texture: 0,
            palette_row: 0,
            cull: false,
            flat: None,
            blend: false,
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
    fn two_mesh_ties_keep_submission_order_and_masks_paint_last() {
        // Both meshes carry the same triangle at the same depth. The stable
        // far-to-near sort must keep the slice order, so the second mesh's
        // green texture paints over the first's red. `engine::render_frame`
        // relies on this: it submits the NPC meshes first and the player last,
        // so at an exact tie the player wins, exactly like the original's
        // entity-then-player render loop.
        let red = solid_texture([255, 0, 0, 255]);
        let green = solid_texture([0, 255, 0, 255]);
        let mesh0 = mesh_at(1024, false);
        let mesh1 = mesh_at(1024, false);
        let joints = [identity()];
        let meshes = [
            EntityMesh {
                mesh: &mesh0,
                texture: &red,
                joints: &joints,
            },
            EntityMesh {
                mesh: &mesh1,
                texture: &green,
                joints: &joints,
            },
        ];
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };

        let mut tied = Framebuffer::new();
        draw_gameplay_scene(
            &mut tied,
            None,
            &meshes,
            None,
            &straight_camera(),
            &lighting,
            None,
        );
        assert_eq!(framebuffer_pixel(&tied, 180, 100), [0, 255, 0, 255]);

        // A mask whose rounded key also ties both triangles is collected after
        // them, so it wins the tie and samples its own page.
        let cut = Cut {
            masks: vec![crate::mask::MaskSprite {
                uv: (0, 0),
                pos: (170, 90),
                size: (20, 20),
                pos_data: 64,
                flags: 0,
                group: 1,
            }],
            mask_active: 1,
            ..Cut::default()
        };
        let page = solid_image(20, 20, [9, 8, 7, 255]);
        let layer = MaskLayer::new(room(), 0, &cut, &page);
        let mut masked = Framebuffer::new();
        draw_gameplay_scene(
            &mut masked,
            None,
            &meshes,
            None,
            &straight_camera(),
            &lighting,
            Some(&layer),
        );
        assert_eq!(framebuffer_pixel(&masked, 180, 100), [9, 8, 7, 255]);
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
                SceneItem::Shadow(poly) => (poly.key, None),
                SceneItem::Effect(quad) => (quad.key, Some((quad.pos[0], quad.pos[1]))),
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
        let entities = [EntityMesh {
            mesh,
            texture: &texture,
            joints: &joints,
        }];
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };

        let mut framebuffer = Framebuffer::new();
        draw_gameplay_scene(
            &mut framebuffer,
            Some(&background),
            &entities,
            None,
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
            &[],
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
            &[],
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

    fn shadow_at<'a>(pos: [i32; 3], half_x: i32, half_z: i32, texture: &'a Image) -> Shadow<'a> {
        Shadow {
            texture,
            pos,
            angle: 0,
            half_x,
            half_z,
            lift: 0,
            tint: [0, 0, 1],
        }
    }

    #[test]
    fn shadow_projection_keeps_the_quad_edge_order_and_uvs() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        // Half the depth keeps every corner in front of the near plane
        // (z = 2 * fov = 400).
        let shadow = shadow_at([0, 0, 1000], 500, 300, &texture);
        let mut items = Vec::new();
        collect_shadow(&shadow, &straight_camera(), &mut items);

        let [SceneItem::Shadow(poly)] = &items[..] else {
            panic!("expected one shadow item, got {}", items.len());
        };
        assert_eq!(poly.key, 1000);
        assert_eq!(poly.corners.len(), 4);
        // Edge order 0->1->3->2, so the corners come out
        // (-x,+z) (+x,+z) (+x,-z) (-x,-z) with v running towards local -Z.
        // The near edge is wider than the far edge and every corner sits on
        // the horizon line of the unpitched camera.
        let expected = [
            (83.07692307692308, 0.0, 0.0),
            (236.92307692307693, 1.0, 0.0),
            (302.85714285714283, 1.0, 1.0),
            (17.142857142857142, 0.0, 1.0),
        ];
        for (corner, (x, u, v)) in poly.corners.iter().zip(expected) {
            assert!(
                (corner.position[0] - x).abs() < 1.0,
                "x {} != {x}",
                corner.position[0]
            );
            assert!((corner.position[1] - 120.0).abs() < 1e-9);
            assert!((corner.u - u).abs() < 1e-9, "u {} != {u}", corner.u);
            assert!((corner.v - v).abs() < 1e-9, "v {} != {v}", corner.v);
        }
        let far_width = poly.corners[1].position[0] - poly.corners[0].position[0];
        let near_width = poly.corners[2].position[0] - poly.corners[3].position[0];
        assert!(near_width > far_width, "{near_width} <= {far_width}");
    }

    #[test]
    fn shadow_projection_applies_the_placement_lift() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let shadow = shadow_at([0, 0, 1000], 500, 700, &texture);
        let mut unlifted = Vec::new();
        collect_shadow(&shadow, &straight_camera(), &mut unlifted);
        let mut lifted = Vec::new();
        collect_shadow(
            &Shadow {
                lift: 100,
                ..shadow
            },
            &straight_camera(),
            &mut lifted,
        );
        let (Some(SceneItem::Shadow(base)), Some(SceneItem::Shadow(raised))) =
            (unlifted.first(), lifted.first())
        else {
            panic!("expected shadow items");
        };
        // A positive placement lift drops the quad down the screen:
        // screen y = 120 - (view_y - lift) * f / z.
        let expected = base.corners[0].position[1]
            + 100.0 * f64::from(straight_camera().fov) * base.corners[0].inv_z;
        assert!((raised.corners[0].position[1] - expected).abs() < 1e-9);
        assert!(raised.corners[0].position[1] > base.corners[0].position[1]);
    }

    #[test]
    fn shadow_clips_against_the_near_plane() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let shadow = shadow_at([0, 0, 0], 500, 700, &texture);
        let mut items = Vec::new();
        collect_shadow(&shadow, &straight_camera(), &mut items);

        let [SceneItem::Shadow(poly)] = &items[..] else {
            panic!("expected one shadow item, got {}", items.len());
        };
        // Corners 1 and 3 are behind the near plane (z = 2 * fov = 400); each
        // clipped edge contributes one vertex on the plane.
        assert_eq!(poly.corners.len(), 4);
        for corner in &poly.corners {
            assert!(corner.inv_z > 0.0);
        }
        // The clipped vertices land on the plane at screen x = 160 +/- 250.
        assert!((poly.corners[2].position[0] - 410.0).abs() < 1.0);
        assert!((poly.corners[2].inv_z - 1.0 / 400.0).abs() < 1e-9);
        assert!((poly.corners[3].position[0] + 90.0).abs() < 1.0);
        assert!((poly.corners[3].inv_z - 1.0 / 400.0).abs() < 1e-9);
    }

    #[test]
    fn shadow_wholly_behind_the_camera_is_skipped() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let mut items = Vec::new();
        collect_shadow(
            &shadow_at([0, 0, -5000], 500, 700, &texture),
            &straight_camera(),
            &mut items,
        );
        assert!(items.is_empty());

        let empty = Image {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
        collect_shadow(
            &shadow_at([0, 0, 1000], 500, 700, &empty),
            &straight_camera(),
            &mut items,
        );
        assert!(items.is_empty());
    }

    /// A shadow item covering screen (10,10)..(20,20) with a 1x1 page.
    fn shadow_quad(alpha: u8) -> (Image, ShadowPoly) {
        let texture = solid_image(1, 1, [255, 255, 255, alpha]);
        let poly = ShadowPoly {
            key: 1000,
            corners: vec![
                ShadowVertex {
                    position: [10.0, 10.0],
                    inv_z: 1.0 / 1000.0,
                    u: 0.0,
                    v: 0.0,
                },
                ShadowVertex {
                    position: [20.0, 10.0],
                    inv_z: 1.0 / 1000.0,
                    u: 1.0,
                    v: 0.0,
                },
                ShadowVertex {
                    position: [20.0, 20.0],
                    inv_z: 1.0 / 1000.0,
                    u: 1.0,
                    v: 1.0,
                },
                ShadowVertex {
                    position: [10.0, 20.0],
                    inv_z: 1.0 / 1000.0,
                    u: 0.0,
                    v: 1.0,
                },
            ],
            tint: [0, 0, 1],
        };
        (texture, poly)
    }

    #[test]
    fn shadow_darkens_towards_the_tint_by_the_texel_alpha() {
        let (texture, poly) = shadow_quad(128);
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
        framebuffer.rasterize_shadow(&texture, &poly);
        // 200 * 127 / 255 + 1 * 128 / 255 rounds to 100 on every channel.
        assert_eq!(
            framebuffer_pixel(&framebuffer, 15, 15),
            [100, 100, 100, 255]
        );
        assert_eq!(framebuffer_pixel(&framebuffer, 5, 5), [200, 200, 200, 255]);

        // A zero-alpha texel is a hole and leaves the destination untouched.
        let (texture, poly) = shadow_quad(0);
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
        framebuffer.rasterize_shadow(&texture, &poly);
        assert_eq!(
            framebuffer_pixel(&framebuffer, 15, 15),
            [200, 200, 200, 255]
        );
    }

    #[test]
    fn shadow_seam_blends_once_for_both_windings() {
        // The fan's shared diagonal passes through (15, 15); a camera on the
        // other side of the quad reverses the ring and must not double-blend
        // the seam either.
        for reversed in [false, true] {
            let (texture, mut poly) = shadow_quad(128);
            if reversed {
                poly.corners.reverse();
            }
            let mut framebuffer = Framebuffer::new();
            framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
            framebuffer.rasterize_shadow(&texture, &poly);
            assert_eq!(
                framebuffer_pixel(&framebuffer, 15, 15),
                [100, 100, 100, 255],
                "reversed {reversed}"
            );
        }
    }

    #[test]
    fn shadow_samples_the_page_at_its_texel_size() {
        // A 2x1 page: the left texel is transparent, the right one darkens.
        // The quad's normalized u must scale by the page width, so each half
        // of the quad samples its own texel.
        let mut texture = solid_image(2, 1, [255, 255, 255, 128]);
        texture.rgba[3] = 0;
        let (_, poly) = shadow_quad(128);
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
        framebuffer.rasterize_shadow(&texture, &poly);
        assert_eq!(
            framebuffer_pixel(&framebuffer, 11, 15),
            [200, 200, 200, 255]
        );
        assert_eq!(
            framebuffer_pixel(&framebuffer, 18, 15),
            [100, 100, 100, 255]
        );
    }

    #[test]
    fn equal_key_masks_are_drawn_after_the_shadow() {
        // A mask pos_data 64 carries key 1024; the shadow's mean view-space Z
        // is 1024, so both land on the same slot. The shadow is submitted
        // first, so the stable sort leaves it under the mask.
        let cut = Cut {
            masks: vec![mask_sprite((100, 100), 64, 1)],
            mask_active: 1,
            ..Cut::default()
        };
        let page = solid_image(1, 1, [1, 2, 3, 255]);
        let layer = MaskLayer::new(room(), 0, &cut, &page);
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let shadow = shadow_at([0, 0, 1024], 500, 300, &texture);

        let mut items = Vec::new();
        collect_shadow(&shadow, &straight_camera(), &mut items);
        collect_masks(&layer, &mut items);
        mask::order_far_to_near(&mut items, SceneItem::key);
        assert!(matches!(items[0], SceneItem::Shadow(_)));
        assert!(matches!(items[1], SceneItem::Mask(_)));
    }

    #[test]
    fn shadow_lies_on_the_floor_under_a_pitched_camera() {
        let camera = Camera::from_cut(&Cut {
            pos: [0, -1500, -1500],
            look_at: [0, 0, 1000],
            fov: 200,
            ..Cut::default()
        });
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let shadow = shadow_at([0, 0, 1000], 500, 700, &texture);
        let background = solid_image(320, 240, [200, 200, 200, 255]);
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [Light::default(); 3],
        };

        let mut with = Framebuffer::new();
        draw_gameplay_scene(
            &mut with,
            Some(&background),
            &[],
            Some(&shadow),
            &camera,
            &lighting,
            None,
        );
        let mut without = Framebuffer::new();
        draw_gameplay_scene(
            &mut without,
            Some(&background),
            &[],
            None,
            &camera,
            &lighting,
            None,
        );

        let changed = with
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(without.rgba.as_chunks::<4>().0)
            .filter(|(a, b)| a != b)
            .count();
        assert!(changed > 1000, "the shadow only repainted {changed} pixels");
        let center = camera.project([0, 0, 1000]).unwrap();
        let pixel = framebuffer_pixel(&with, center[0] as usize, center[1] as usize);
        assert_eq!(pixel, [100, 100, 100, 255]);
    }
    fn effect_image(pixels: &[[u8; 4]]) -> Image {
        Image {
            width: 2,
            height: 2,
            rgba: pixels.concat(),
        }
    }

    fn effect_quad(
        page: usize,
        pos: [i32; 2],
        size: [i32; 2],
        tint: [u8; 3],
        blend: u8,
    ) -> EffectQuad {
        EffectQuad {
            key: 500,
            page,
            uv: [0, 0, 0, 0],
            source: [2, 2],
            pos,
            size,
            tint,
            blend,
        }
    }

    fn draw_effects(
        framebuffer: &mut Framebuffer,
        background: &Image,
        page: &Image,
        quads: &[EffectQuad],
    ) {
        let camera = straight_camera();
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [Light::default(); 3],
        };
        let pages = [Some(page)];
        let layer = EffectLayer {
            pages: &pages,
            quads,
        };
        draw_gameplay_scene_with_effects(
            framebuffer,
            Some(background),
            &[],
            None,
            &camera,
            &lighting,
            None,
            Some(&layer),
        );
    }

    #[test]
    fn effect_quads_are_opaque_and_entry_zero_is_transparent() {
        let page = effect_image(&[[255, 0, 0, 255], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]]);
        let background = solid_image(320, 240, [10, 20, 30, 255]);
        let quad = effect_quad(0, [10, 10], [4, 4], [255, 255, 255], 0);
        let mut framebuffer = Framebuffer::new();
        draw_effects(&mut framebuffer, &background, &page, &[quad]);

        assert_eq!(framebuffer_pixel(&framebuffer, 10, 10), [255, 0, 0, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 12, 10), [10, 20, 30, 255]);
    }

    #[test]
    fn effect_quads_half_blend_and_multiply_the_tint() {
        let page = effect_image(&[[255, 255, 255, 255]; 4]);
        let background = solid_image(320, 240, [100, 100, 100, 255]);
        let quad = effect_quad(0, [0, 0], [4, 4], [255, 255, 255], 0x80);
        let mut framebuffer = Framebuffer::new();
        draw_effects(&mut framebuffer, &background, &page, &[quad]);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [177, 177, 177, 255]);

        let quad = effect_quad(0, [0, 0], [4, 4], [128, 255, 0], 0);
        let mut framebuffer = Framebuffer::new();
        draw_effects(&mut framebuffer, &background, &page, &[quad]);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [128, 255, 0, 255]);
    }

    #[test]
    fn effect_quad_pivot_offsets_the_scaled_rectangle() {
        let page = effect_image(&[[9, 9, 9, 255]; 4]);
        let background = solid_image(320, 240, [0, 0, 0, 255]);
        // Pivot the 2x2 source at its bottom-right, scaled to 8x8: the quad
        // runs from (-8, -8) to (0, 0) of the pivot point.
        let mut quad = effect_quad(0, [0, 0], [8, 8], [255, 255, 255], 0);
        quad.uv = [0, 0, 2, 2];
        quad.pos = [100 - 8, 100 - 8];
        let mut framebuffer = Framebuffer::new();
        draw_effects(&mut framebuffer, &background, &page, &[quad]);
        assert_eq!(framebuffer_pixel(&framebuffer, 99, 99), [9, 9, 9, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 92, 92), [9, 9, 9, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 100, 100), [0, 0, 0, 255]);
    }

    #[test]
    fn effect_ties_paint_over_masks_and_entities() {
        // A mask and an effect with the same key keep their submission order
        // through the stable sort, and the effect was submitted last.
        let mask_page = solid_image(1, 1, [0, 0, 255, 255]);
        let effect_page = effect_image(&[[255, 0, 0, 255]; 4]);
        let mask = SceneItem::Mask(MaskQuad {
            key: 500,
            uv: (0, 0),
            pos: (10, 10),
            size: (1, 1),
        });
        let effect = SceneItem::Effect(effect_quad(0, [10, 10], [4, 4], [255, 255, 255], 0));
        let mut items = vec![mask, effect];
        mask::order_far_to_near(&mut items, SceneItem::key);
        assert!(matches!(items[0], SceneItem::Mask(_)));
        assert!(matches!(items[1], SceneItem::Effect(_)));

        let mut framebuffer = Framebuffer::new();
        let effect_pages = [Some(&effect_page)];
        framebuffer.draw_scene(&[], Some(&mask_page), None, &effect_pages, &items);
        assert_eq!(framebuffer_pixel(&framebuffer, 10, 10), [255, 0, 0, 255]);

        // The same effect against a real entity triangle at the same key.
        let texture = solid_texture([0, 0, 255, 255]);
        let mesh = mesh_at(1000, false);
        let joints = [identity()];
        let meshes = [EntityMesh {
            mesh: &mesh,
            texture: &texture,
            joints: &joints,
        }];
        let camera = straight_camera();
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };
        let pages = [Some(&effect_page)];
        let quads = [effect_quad(0, [160, 120], [200, 200], [255, 255, 255], 0)];
        let layer = EffectLayer {
            pages: &pages,
            quads: &quads,
        };
        let mut framebuffer = Framebuffer::new();
        draw_gameplay_scene_with_effects(
            &mut framebuffer,
            None,
            &meshes,
            None,
            &camera,
            &lighting,
            None,
            Some(&layer),
        );
        assert_eq!(framebuffer_pixel(&framebuffer, 160, 130), [255, 0, 0, 255]);
    }
}
