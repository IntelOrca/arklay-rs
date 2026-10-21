//! Deterministic software rasterizer for the player model and room masks.
//!
//! [`Framebuffer::draw_model`] transforms a TMD mesh by the per-joint 4.12
//! matrices, shades every vertex normal against the entity's latched room
//! lights ([`Lighting::latched`]), sorts the triangles back-to-front and
//! rasterizes them into the RGBA8 buffer. ABE packets blend with the per-texel
//! STP selector the TIM CLUTs carry. Nothing here uses SDL; the engine owns
//! presentation and only uploads [`Framebuffer::rgba`].
//!
//! [`draw_gameplay_scene`] is the full gameplay path. It draws the camera's
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
use crate::assets::Artwork;
use crate::mask;
use crate::model::{Texture8, Tmd, TmdObject};
use crate::state::{Cut, Image, Light, RoomId, RoomState};

/// Default framebuffer width in pixels.
pub const WIDTH: u32 = 320;
/// Default framebuffer height in pixels.
pub const HEIGHT: u32 = 240;
/// Fractional bits of the 4.12 fixed-point matrices.
const FIXED_BITS: u32 = 12;
/// Horizontal screen centre in pixels.
const CENTER_X: i32 = 160;
/// Vertical screen centre in pixels.
const CENTER_Y: i32 = 120;
/// Full scale of one colour channel.
const CHANNEL_MAX: f64 = 255.0;

/// A 320x240 RGBA8 framebuffer.
#[derive(Debug, Clone)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Optional `[x, y, w, h]` clip applied to sprite draws. The pause menu's
    /// tab screens sit behind the panel's masking rects and frame, so their
    /// sliding art is clipped to the panel's screen area instead of spilling
    /// over the chrome.
    pub clip: Option<[i32; 4]>,
    /// The rows a wider aspect presents ([`crate::video::View`]); the whole
    /// frame at 4:3. Drawing ignores it: the scene stays in 4:3 space, and
    /// screen-space layers draw through [`Framebuffer::with_view`].
    pub view: crate::video::View,
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
    /// An explicit per-channel multiplier, for a caller that has a colour of
    /// its own (the Lua screen API's `#RRGGBB`).
    Rgb(u8, u8, u8),
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
            Self::Rgb(red, green, blue) => [red, green, blue],
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

/// How a pending sprite mixes with what is already in the framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpriteBlend {
    /// Overwrite the destination.
    Opaque,
    /// `(source + destination) / 2`, the PSX half blend.
    Half,
    /// The original's variant-2 semi-transparency: `127/256` of the (already
    /// colour-modulated) source over the destination, rounded - the reference's
    /// GPU chain, which the FILE reader's backdrop pieces use.
    HalfVariant,
    /// Mix the destination towards black by `alpha`/255 wherever a texel is
    /// opaque; the text shadow's pass. The source colour is ignored.
    BlackAlpha(u8),
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
    /// How the sprite mixes with the framebuffer.
    pub blend: SpriteBlend,
    /// Mirror the sampled source rectangle horizontally (`[0]`) or vertically
    /// (`[1]`).
    pub mirror: [bool; 2],
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
            blend: SpriteBlend::Opaque,
            mirror: [false; 2],
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
            blend: SpriteBlend::Opaque,
            mirror: [false; 2],
        }
    }

    /// Override the sprite's blend mode.
    pub fn with_blend(mut self, blend: SpriteBlend) -> Self {
        self.blend = blend;
        self
    }

    /// Override the sprite's U/V mirror flags.
    pub fn with_mirror(mut self, mirror: [bool; 2]) -> Self {
        self.mirror = mirror;
        self
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
            clip: None,
            view: crate::video::View::default(),
        }
    }

    /// A black `width` x `height` framebuffer.
    pub fn with_size(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; width as usize * height as usize * 4],
            clip: None,
            view: crate::video::View {
                top: 0,
                height,
                pan: 0,
            },
        }
    }

    /// Run `draw` on the visible band as its own frame: a 320 x
    /// `view.height` framebuffer whose row 0 is the band's top, so a
    /// screen-space layer (a menu, the message window) lays itself out in
    /// the space the player sees. The band's pixels are copied in and back,
    /// and a clip rect `draw` leaves is lifted. A full-frame view draws in
    /// place, so 4:3 frames are drawn exactly as before.
    pub fn with_view<R>(&mut self, draw: impl FnOnce(&mut Framebuffer) -> R) -> R {
        let view = self.view;
        if view.is_full() || self.width != WIDTH || self.height != HEIGHT {
            return draw(self);
        }
        let stride = self.width as usize * 4;
        let start = view.top as usize * stride;
        let end = start + view.height as usize * stride;
        let mut band = Framebuffer::with_size(self.width, view.height);
        band.rgba.copy_from_slice(&self.rgba[start..end]);
        let result = draw(&mut band);
        self.rgba[start..end].copy_from_slice(&band.rgba);
        result
    }

    /// Run `draw` on the 320 x `rows` frame a fixed 4:3 layout draws in:
    /// [`Framebuffer::with_view`]'s band on a 320x240 frame, the centred
    /// 320x240 columns of a wider aspect's canvas. Rust's screen-space
    /// overlays (the message window, the debug menu) draw through this.
    pub fn with_screen<R>(&mut self, draw: impl FnOnce(&mut Framebuffer) -> R) -> R {
        if self.width <= WIDTH {
            return self.with_view(draw);
        }
        let left = (self.width - WIDTH) as usize / 2;
        let stride = self.width as usize * 4;
        let rows = self.height as usize;
        let mut screen = Framebuffer::with_size(WIDTH, self.height);
        let span = WIDTH as usize * 4;
        for row in 0..rows {
            let start = row * stride + left * 4;
            screen.rgba[row * span..(row + 1) * span]
                .copy_from_slice(&self.rgba[start..start + span]);
        }
        let result = draw(&mut screen);
        for row in 0..rows {
            let start = row * stride + left * 4;
            self.rgba[start..start + span]
                .copy_from_slice(&screen.rgba[row * span..(row + 1) * span]);
        }
        result
    }

    /// Fill the whole framebuffer black.
    pub fn clear(&mut self) {
        self.rgba.fill(0);
    }

    /// Clip subsequent sprite draws to `rect` (`[x, y, w, h]`); `None` clears
    /// the clip.
    pub fn set_clip_rect(&mut self, rect: Option<[i32; 4]>) {
        self.clip = rect;
    }

    /// The clip rect [`Framebuffer::set_clip_rect`] installed.
    pub fn clip_rect(&self) -> Option<[i32; 4]> {
        self.clip
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

    /// Draw a full-screen background through `origin` and a global colour.
    ///
    /// This is the cut's display image: the sampled window is shifted by the
    /// display origin (screen panning and shake) with a clamp sampler, so a
    /// positive X origin moves the image right by sampling one pixel further
    /// left, and the edge row/column repeats. `colour` is the global colour
    /// multiplier (`255` per channel is identity), applied with a truncating
    /// integer scale. The destination is always opaque.
    ///
    /// The image stretches over the logical 320x240 quad centred in the
    /// frame, so a wider aspect's shorter band crops the picture rather than
    /// squashing it; a wider canvas repeats the edge columns.
    pub fn draw_background(&mut self, image: &Image, origin: [i32; 2], colour: [u8; 3]) {
        if image.width == 0 || image.height == 0 {
            return;
        }
        let width = self.width as usize;
        let origin = [
            origin[0] + (self.width as i32 - WIDTH as i32) / 2,
            origin[1] + (self.height as i32 - HEIGHT as i32) / 2,
        ];
        // The reference uploads the background as a texture and stretches it
        // over the full logical 320x240 quad (see the display-image pass in
        // `OT_InsertPrimitive`), so a 316x236 screen-border background covers
        // the frame. The origin is the display-image origin in logical pixels,
        // applied before the texture-space sampling, and the fragment centre
        // (`(2x + 1) / 2`) is the reference's GL rasterisation.
        let scale_x = i64::from(image.width);
        let scale_y = i64::from(image.height);
        let frame_w = WIDTH as i64;
        let frame_h = HEIGHT as i64;
        for y in 0..self.height {
            let source_y = (((i64::from(y) - i64::from(origin[1])) * 2 + 1) * scale_y
                / (2 * frame_h))
                .clamp(0, scale_y - 1) as u32;
            for x in 0..self.width {
                let source_x = (((i64::from(x) - i64::from(origin[0])) * 2 + 1) * scale_x
                    / (2 * frame_w))
                    .clamp(0, scale_x - 1) as u32;
                let source = ((source_y * image.width + source_x) * 4) as usize;
                let Some(texel) = image.rgba.get(source..source + 4) else {
                    continue;
                };
                let offset = (y as usize * width + x as usize) * 4;
                let Some(slot) = self.rgba.get_mut(offset..offset + 4) else {
                    continue;
                };
                for channel in 0..3 {
                    slot[channel] =
                        (u32::from(texel[channel]) * u32::from(colour[channel]) / 255) as u8;
                }
                slot[3] = 255;
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
        self.draw_indexed_sprite_full(
            texture,
            src,
            dst,
            clut_row,
            brightness_scale(brightness),
            tint,
            SpriteBlend::Opaque,
            [false; 2],
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
        self.draw_indexed_sprite_full(
            texture,
            src,
            dst,
            clut_row,
            scale,
            tint,
            SpriteBlend::Opaque,
            [false; 2],
        );
    }

    /// Draw one indexed sprite the way the FILE reader's backdrop pieces are:
    /// the texel is modulated by `2 * ramp` (the original's `DAT_004af48c`
    /// colour field) and then blended over the destination with the
    /// descriptor's variant-2 half alpha (`127/256`), rounding at both stages
    /// exactly like the reference's GPU chain.
    pub fn draw_indexed_sprite_ramp(
        &mut self,
        texture: &Texture8,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        ramp: u8,
    ) {
        if texture.width == 0 || texture.height == 0 {
            return;
        }
        // `2 * ramp` is deliberately NOT clamped to 255: the reference keeps
        // the modulated colour as a float, so `2 * 0x80` rounds an art value of
        // 148 up to 149 and the drawn pixel lands on 74 - the value the fully
        // ramped backdrop actually shows.
        let colour = u32::from(ramp) * 2;
        self.draw_sprite(src, dst, [false; 2], SpriteBlend::HalfVariant, |u, v| {
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
            Some([
                ((u32::from(texel[0]) * colour + 127) / 255) as u8,
                ((u32::from(texel[1]) * colour + 127) / 255) as u8,
                ((u32::from(texel[2]) * colour + 127) / 255) as u8,
                255,
            ])
        });
    }

    /// Draw one indexed sprite with explicit U/V mirror flags and the font's
    /// `0..=30` brightness scale.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_indexed_sprite_mirrored(
        &mut self,
        texture: &Texture8,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        brightness: u8,
        tint: Tint,
        mirror: [bool; 2],
    ) {
        self.draw_indexed_sprite_full(
            texture,
            src,
            dst,
            clut_row,
            brightness_scale(brightness),
            tint,
            SpriteBlend::Opaque,
            mirror,
        );
    }

    /// Draw one indexed sprite through a per-index colour table: texel index
    /// `i` writes the opaque `lut[i]`, or nothing when it is `None`. Unlike
    /// the CLUT paths index 0 is not implicitly transparent.
    pub fn draw_indexed_sprite_lut(
        &mut self,
        texture: &Texture8,
        src: [i32; 4],
        dst: [i32; 4],
        lut: &[Option<[u8; 3]>; 256],
    ) {
        if texture.width == 0 || texture.height == 0 {
            return;
        }
        self.draw_sprite(src, dst, [false; 2], SpriteBlend::Opaque, |u, v| {
            if u < 0 || v < 0 || u >= texture.width as i32 || v >= texture.height as i32 {
                return None;
            }
            let index = *texture
                .indices
                .get(v as usize * texture.width as usize + u as usize)?;
            lut[usize::from(index)].map(|[r, g, b]| [r, g, b, 255])
        });
    }

    /// [`Framebuffer::draw_indexed_sprite_scaled`] with a blend mode and U/V
    /// mirror flags.
    #[allow(clippy::too_many_arguments)]
    fn draw_indexed_sprite_full(
        &mut self,
        texture: &Texture8,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        scale: u8,
        tint: Tint,
        blend: SpriteBlend,
        mirror: [bool; 2],
    ) {
        if texture.width == 0 || texture.height == 0 {
            return;
        }
        let scale = u32::from(scale);
        let tint = tint.rgb();
        self.draw_sprite(src, dst, mirror, blend, |u, v| {
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

    /// Draw one artwork page: an indexed page samples its palette row, an
    /// RGBA replacement samples its own pixels.
    ///
    /// This is the shared entry point for pack art that a mod may replace with
    /// any image format; `clut_row` is ignored by an RGBA page, and `tint`
    /// multiplies its colour channels exactly like an indexed page's.
    pub fn draw_page_sprite(
        &mut self,
        art: &Artwork,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        brightness: u8,
        tint: Tint,
    ) {
        match art {
            Artwork::Indexed(texture) => {
                self.draw_indexed_sprite(texture, src, dst, clut_row, brightness, tint);
            }
            Artwork::Rgba(image) => self.draw_rgba_sprite_full(
                image,
                src,
                dst,
                brightness_scale(brightness),
                tint,
                SpriteBlend::Opaque,
                [false; 2],
            ),
        }
    }

    /// [`Framebuffer::draw_page_sprite`] with a direct `0..=255` colour
    /// multiplier, the fade path [`Framebuffer::draw_indexed_sprite_scaled`]
    /// serves for indexed pages.
    pub fn draw_page_sprite_scaled(
        &mut self,
        art: &Artwork,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        scale: u8,
        tint: Tint,
    ) {
        match art {
            Artwork::Indexed(texture) => {
                self.draw_indexed_sprite_scaled(texture, src, dst, clut_row, scale, tint);
            }
            Artwork::Rgba(image) => self.draw_rgba_sprite_full(
                image,
                src,
                dst,
                scale,
                tint,
                SpriteBlend::Opaque,
                [false; 2],
            ),
        }
    }

    /// [`Framebuffer::draw_page_sprite`] with explicit U/V mirror flags.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_page_sprite_mirrored(
        &mut self,
        art: &Artwork,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        brightness: u8,
        tint: Tint,
        mirror: [bool; 2],
    ) {
        match art {
            Artwork::Indexed(texture) => {
                self.draw_indexed_sprite_mirrored(
                    texture, src, dst, clut_row, brightness, tint, mirror,
                );
            }
            Artwork::Rgba(image) => self.draw_rgba_sprite_full(
                image,
                src,
                dst,
                brightness_scale(brightness),
                tint,
                SpriteBlend::Opaque,
                mirror,
            ),
        }
    }

    /// [`Framebuffer::draw_page_sprite`] with the FILE reader's `2 * ramp`
    /// modulation and half-alpha blend.
    pub fn draw_page_sprite_ramp(
        &mut self,
        art: &Artwork,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        ramp: u8,
    ) {
        match art {
            Artwork::Indexed(texture) => {
                self.draw_indexed_sprite_ramp(texture, src, dst, clut_row, ramp);
            }
            Artwork::Rgba(image) => {
                if image.width == 0 || image.height == 0 {
                    return;
                }
                let colour = u32::from(ramp) * 2;
                self.draw_sprite(src, dst, [false; 2], SpriteBlend::HalfVariant, |u, v| {
                    if u < 0 || v < 0 || u >= image.width as i32 || v >= image.height as i32 {
                        return None;
                    }
                    let offset = (v as usize * image.width as usize + u as usize) * 4;
                    let texel = image.rgba.get(offset..offset + 4)?;
                    if texel[3] == 0 {
                        return None;
                    }
                    Some([
                        ((u32::from(texel[0]) * colour + 127) / 255) as u8,
                        ((u32::from(texel[1]) * colour + 127) / 255) as u8,
                        ((u32::from(texel[2]) * colour + 127) / 255) as u8,
                        255,
                    ])
                });
            }
        }
    }

    /// [`Framebuffer::draw_indexed_sprite`] with an explicit blend mode.
    ///
    /// The Lua screen API's `blend` option needs the modes the sprite queue
    /// supports without going through a `SpriteDraw`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_indexed_sprite_blended(
        &mut self,
        texture: &Texture8,
        src: [i32; 4],
        dst: [i32; 4],
        clut_row: usize,
        brightness: u8,
        tint: Tint,
        blend: SpriteBlend,
    ) {
        self.draw_indexed_sprite_full(
            texture,
            src,
            dst,
            clut_row,
            brightness_scale(brightness),
            tint,
            blend,
            [false, false],
        );
    }

    /// [`Framebuffer::draw_rgba_sprite`] with an explicit blend mode.
    pub fn draw_rgba_sprite_blended(
        &mut self,
        image: &Image,
        src: [i32; 4],
        dst: [i32; 4],
        brightness: u8,
        blend: SpriteBlend,
    ) {
        self.draw_rgba_sprite_full(
            image,
            src,
            dst,
            brightness_scale(brightness),
            Tint::White,
            blend,
            [false, false],
        );
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
        self.draw_rgba_sprite_full(
            image,
            src,
            dst,
            brightness_scale(brightness),
            Tint::White,
            SpriteBlend::Opaque,
            [false; 2],
        );
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
        self.draw_rgba_sprite_full(
            image,
            src,
            dst,
            scale,
            Tint::White,
            SpriteBlend::Opaque,
            [false; 2],
        );
    }

    /// [`Framebuffer::draw_rgba_sprite_scaled`] with a tint, blend mode and
    /// U/V mirror flags.
    #[allow(clippy::too_many_arguments)] // the indexed path's parameter set
    fn draw_rgba_sprite_full(
        &mut self,
        image: &Image,
        src: [i32; 4],
        dst: [i32; 4],
        scale: u8,
        tint: Tint,
        blend: SpriteBlend,
        mirror: [bool; 2],
    ) {
        if image.width == 0 || image.height == 0 {
            return;
        }
        let scale = u32::from(scale);
        let tint = tint.rgb();
        self.draw_sprite(src, dst, mirror, blend, |u, v| {
            if u < 0 || v < 0 || u >= image.width as i32 || v >= image.height as i32 {
                return None;
            }
            let offset = (v as usize * image.width as usize + u as usize) * 4;
            let texel = image.rgba.get(offset..offset + 4)?;
            if texel[3] == 0 {
                return None;
            }
            // A white tint is a no-op multiplier, so the existing RGBA
            // callers keep their exact `channel * scale / 255` truncation.
            let mix = |channel: u8, tint: u8| {
                (u32::from(channel) * u32::from(tint) / 255 * scale / 255) as u8
            };
            Some([
                mix(texel[0], tint[0]),
                mix(texel[1], tint[1]),
                mix(texel[2], tint[2]),
                texel[3],
            ])
        });
    }

    /// Copy another 320x240 framebuffer over this one.
    pub fn copy_from(&mut self, other: &Framebuffer) {
        // A wider aspect's canvas and the 320x240 frame trade places as
        // screens open and close.
        if (self.width, self.height) != (other.width, other.height) {
            self.resize(other.width, other.height);
        }
        if self.rgba.len() == other.rgba.len() {
            self.rgba.copy_from_slice(&other.rgba);
            self.view = other.view;
        }
    }

    /// Make this a black `width` x `height` frame unless it already has that
    /// size; its view becomes the whole frame either way.
    pub fn resize(&mut self, width: u32, height: u32) {
        if (self.width, self.height) != (width, height) {
            *self = Self::with_size(width, height);
        }
        self.view = crate::video::View::default();
    }

    /// Blend the whole frame towards `color` by `alpha`/255.
    ///
    /// This is the screens' fade overlay: `0` leaves the frame untouched and
    /// `255` paints it the overlay colour. Alpha is preserved.
    pub fn fade_to_color(&mut self, color: crate::transition::FadeColor, alpha: u8) {
        if color == crate::transition::FadeColor::Black {
            self.fade_to_black_weighted(alpha);
            return;
        }
        if alpha == 0 {
            return;
        }
        let target: u8 = 255;
        if alpha == 255 {
            for pixel in self.rgba.as_chunks_mut::<4>().0 {
                pixel[..3].fill(target);
            }
            return;
        }
        let keep = u32::from(255 - alpha);
        let add = u32::from(target) * u32::from(alpha);
        for pixel in self.rgba.as_chunks_mut::<4>().0 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * keep + add) / 255) as u8;
            }
        }
    }

    /// Blend the whole frame towards black by `alpha`/255 with the reference's
    /// per-texel weight.
    ///
    /// The reference draws the fade through the GPU as an alpha blend, so each
    /// texel is `round(channel * (255 - alpha) / 255)` (verified exact against
    /// the oracle's fade frames). Rounding, not truncating, is what matches;
    /// truncation diverges ~60% of a fading screen by one LSB.
    pub fn fade_to_black_weighted(&mut self, alpha: u8) {
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
                *channel = ((u32::from(*channel) * keep + 127) / 255) as u8;
            }
        }
    }

    /// Blend the whole frame towards black by `alpha`/255.
    ///
    /// This is the screens' black fade overlay: `0` leaves the frame untouched
    /// and `255` paints it black. Alpha is preserved.
    pub fn fade_to_black(&mut self, alpha: u8) {
        self.fade_to_black_weighted(alpha);
    }

    /// Blend a screen's full-frame overlay over the frame.
    pub fn fade_overlay(&mut self, overlay: Option<(crate::transition::FadeColor, u8)>) {
        if let Some((color, alpha)) = overlay {
            self.fade_to_color(color, alpha);
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

    /// [`Self::blend_black_rect`] with the executable's `draw_rect` rounding.
    ///
    /// The original stores the variant-3 ("fade to black") primitive's
    /// destination weight as `(0x100 - brightness)` and its sprite blend
    /// rounds at the store, the same `(... * keep + 127) / 255` the full-frame
    /// fade uses (`draw_rect`, Rendering.cpp 0x00470350); the letterbox bars
    /// therefore have to round like [`Self::fade_to_black_weighted`], not
    /// truncate. The truncating form shifted every affected bar pixel down by
    /// up to one unit, which the oracle's frame diff showed as a whole
    /// top/bottom band of one-unit differences.
    pub fn blend_black_rect_weighted(&mut self, dst: [i32; 4], alpha: u8) {
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
                        *channel = ((u32::from(*channel) * keep + 127) / 255) as u8;
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

    /// Rasterize a 1-pixel Bresenham line from `(x0, y0)` to `(x1, y1)`
    /// inclusive, clipped to the framebuffer (like [`Self::fill_rect`], it
    /// ignores the sprite clip).
    pub fn draw_line(&mut self, from: [i32; 2], to: [i32; 2], color: [u8; 3]) {
        let [x0, y0] = from;
        let [x1, y1] = to;
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut error = dx + dy;
        let (mut x, mut y) = (x0, y0);
        loop {
            if x >= 0 && y >= 0 && x < self.width as i32 && y < self.height as i32 {
                let offset = (y as usize * self.width as usize + x as usize) * 4;
                if let Some(pixel) = self.rgba.get_mut(offset..offset + 4) {
                    pixel.copy_from_slice(&[color[0], color[1], color[2], 255]);
                }
            }
            if x == x1 && y == y1 {
                break;
            }
            let doubled = 2 * error;
            if doubled >= dy {
                error += dy;
                x += sx;
            }
            if doubled <= dx {
                error += dx;
                y += sy;
            }
        }
    }

    /// Alpha-blend a solid rectangle over the framebuffer.
    ///
    /// `alpha` is the source weight: 0 leaves the destination untouched and
    /// 255 paints the colour exactly. Out-of-bounds parts are clipped.
    pub fn blend_rect(&mut self, dst: [i32; 4], color: [u8; 3], alpha: u8) {
        let [x, y, width, height] = dst;
        if width <= 0 || height <= 0 || alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.fill_rect(dst, [color[0], color[1], color[2], 255]);
            return;
        }
        let start_x = x.max(0) as u32;
        let start_y = y.max(0) as u32;
        let end_x = x.saturating_add(width).min(self.width as i32).max(0) as u32;
        let end_y = y.saturating_add(height).min(self.height as i32).max(0) as u32;
        let source = u32::from(alpha);
        let keep = u32::from(255 - alpha);
        for row in start_y..end_y {
            for column in start_x..end_x {
                let offset = (row as usize * self.width as usize + column as usize) * 4;
                if let Some(pixel) = self.rgba.get_mut(offset..offset + 4) {
                    for (channel, &value) in pixel[..3].iter_mut().zip(&color) {
                        // The reference blends through the GL pipeline in
                        // normalized floats, so the result rounds to nearest
                        // rather than truncating.
                        *channel = ((u32::from(value) * source + u32::from(*channel) * keep + 127)
                            / 255) as u8;
                    }
                }
            }
        }
    }

    /// Draw a pending list far-to-near.
    ///
    /// The sort is stable on descending [`SpriteDraw::depth`] (higher depths
    /// are farther behind and paint first), so equal-depth sprites keep their
    /// submission order, exactly like the original's pending-sprite queue.
    /// Each sprite's blend mode and mirror flags are honoured.
    pub fn draw_sprites(&mut self, sprites: &mut [SpriteDraw<'_>]) {
        sprites.sort_by_key(|sprite| std::cmp::Reverse(sprite.depth));
        for sprite in sprites.iter() {
            match sprite.source {
                SpriteSource::Indexed {
                    texture,
                    clut_row,
                    tint,
                } => self.draw_indexed_sprite_full(
                    texture,
                    sprite.src,
                    sprite.dst,
                    clut_row,
                    brightness_scale(sprite.brightness),
                    tint,
                    sprite.blend,
                    sprite.mirror,
                ),
                SpriteSource::Rgba(image) => self.draw_rgba_sprite_full(
                    image,
                    sprite.src,
                    sprite.dst,
                    brightness_scale(sprite.brightness),
                    Tint::White,
                    sprite.blend,
                    sprite.mirror,
                ),
            }
        }
    }

    /// Visit the framebuffer-visible pixels of a sprite: clip `dst` and map
    /// each destination pixel back to `src` with nearest sampling.
    ///
    /// `mirror` flips the sampled source rectangle around its own origin, and
    /// `blend` mixes the sampled pixel with the framebuffer before it is
    /// written.
    fn draw_sprite(
        &mut self,
        src: [i32; 4],
        dst: [i32; 4],
        mirror: [bool; 2],
        blend: SpriteBlend,
        mut sample: impl FnMut(i32, i32) -> Option<[u8; 4]>,
    ) {
        let [src_x, src_y, src_w, src_h] = src;
        let [dst_x, dst_y, dst_w, dst_h] = dst;
        if src_w <= 0 || src_h <= 0 || dst_w <= 0 || dst_h <= 0 {
            return;
        }
        let mut start_x = dst_x.max(0);
        let mut start_y = dst_y.max(0);
        let mut end_x = dst_x.saturating_add(dst_w).min(self.width as i32);
        let mut end_y = dst_y.saturating_add(dst_h).min(self.height as i32);
        if let Some([clip_x, clip_y, clip_w, clip_h]) = self.clip {
            start_x = start_x.max(clip_x);
            start_y = start_y.max(clip_y);
            end_x = end_x.min(clip_x.saturating_add(clip_w));
            end_y = end_y.min(clip_y.saturating_add(clip_h));
        }
        if start_x >= end_x || start_y >= end_y {
            return;
        }
        let width = self.width as usize;
        for y in start_y..end_y {
            let Ok(offset_y) = i32::try_from(
                (i64::from(y) - i64::from(dst_y)) * i64::from(src_h) / i64::from(dst_h),
            ) else {
                continue;
            };
            let tex_y = if mirror[1] {
                src_y + src_h - 1 - offset_y
            } else {
                src_y + offset_y
            };
            for x in start_x..end_x {
                let Ok(offset_x) = i32::try_from(
                    (i64::from(x) - i64::from(dst_x)) * i64::from(src_w) / i64::from(dst_w),
                ) else {
                    continue;
                };
                let tex_x = if mirror[0] {
                    src_x + src_w - 1 - offset_x
                } else {
                    src_x + offset_x
                };
                let Some(mut pixel) = sample(tex_x, tex_y) else {
                    continue;
                };
                let offset = (y as usize * width + x as usize) * 4;
                let Some(slot) = self.rgba.get_mut(offset..offset + 4) else {
                    continue;
                };
                match blend {
                    SpriteBlend::Opaque => {}
                    SpriteBlend::Half => {
                        for (channel, &destination) in pixel[..3].iter_mut().zip(slot.iter()) {
                            *channel = ((u16::from(*channel) + u16::from(destination)) / 2) as u8;
                        }
                    }
                    SpriteBlend::HalfVariant => {
                        for (channel, &destination) in pixel[..3].iter_mut().zip(slot.iter()) {
                            *channel =
                                ((u32::from(*channel) * 127 + u32::from(destination) * 129 + 128)
                                    >> 8) as u8;
                        }
                    }
                    SpriteBlend::BlackAlpha(alpha) => {
                        let keep = u32::from(255 - alpha);
                        for (channel, &destination) in pixel[..3].iter_mut().zip(slot.iter()) {
                            *channel = (u32::from(destination) * keep / 255) as u8;
                        }
                    }
                }
                slot.copy_from_slice(&pixel);
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
        let light = lighting.latched(root_position(joints));
        for (object, joint) in mesh.objects.iter().zip(joints) {
            collect_triangles(
                object,
                joint,
                camera,
                Some(lighting),
                Some(light),
                true,
                0,
                [255; 3],
                None,
                None,
                false,
                &mut triangles,
            );
        }
        self.rasterize_triangles(texture, triangles);
    }

    /// [`Framebuffer::draw_model`] with a joint visibility mask: an object
    /// whose joint's bit is set is skipped entirely. The DIED screen hides
    /// the player's head joint through this path.
    pub fn draw_model_hidden(
        &mut self,
        mesh: &Tmd,
        texture: &Texture8,
        joints: &[anim::Mat4x3],
        camera: &Camera,
        lighting: &Lighting,
        hidden: u32,
    ) {
        let mut triangles: Vec<Triangle> = Vec::new();
        let light = lighting.latched(root_position(joints));
        for (index, (object, joint)) in mesh.objects.iter().zip(joints).enumerate() {
            if hidden & (1 << index) != 0 {
                continue;
            }
            collect_triangles(
                object,
                joint,
                camera,
                Some(lighting),
                Some(light),
                true,
                0,
                [255; 3],
                None,
                None,
                false,
                &mut triangles,
            );
        }
        self.rasterize_triangles(texture, triangles);
    }

    /// Draw a TMD mesh full-bright, with backface culling.
    ///
    /// This is the door animation's path: panels are drawn over black with the
    /// door texture's single 256-colour CLUT row and direct (unpaged) UVs.
    /// Every packet backface-culls like the original's TMD pass; triangles are
    /// painter-sorted back-to-front exactly like
    /// [`Framebuffer::draw_model`].
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
            collect_triangles(
                object,
                joint,
                camera,
                None,
                None,
                true,
                0,
                [255; 3],
                None,
                None,
                false,
                &mut triangles,
            );
        }
        self.rasterize_triangles(texture, triangles);
    }

    /// Draw an item-view (`.ivm`) model full-bright: the item viewer's path.
    ///
    /// The item mesh keeps its own texture page (one 256-colour CLUT row, so
    /// every polygon samples row 0 with direct UVs) and every packet
    /// backface-culls like the door and room TMD paths. Quads split into two
    /// fan triangles but drop whole when any corner is inside the near plane;
    /// untextured gouraud polygons interpolate their three corner colours.
    /// One matrix per object, in object order, supplies the viewer's turntable
    /// rotation and distance.
    pub fn draw_ivm_unlit(
        &mut self,
        ivm: &crate::ivm::Ivm,
        joints: &[anim::Mat4x3],
        camera: &Camera,
    ) {
        self.draw_ivm(ivm, joints, camera, None);
    }

    /// Draw an item-view (`.ivm`) model, lit when `light` is given.
    ///
    /// The examine screen lights its model like an entity
    /// (`TmdComputeLight`): every corner normal is evaluated against the
    /// viewer's three lights over the ambient, the sum clamps at full scale and
    /// an untextured polygon then multiplies in its packet colour. The
    /// viewer's light rows are pre-multiplied by the camera
    /// (`MulMatrix0(&g_lightMatrix, &g_RoomCameraData)`) before the usual
    /// camera transform. `None` is the full-bright path.
    pub fn draw_ivm(
        &mut self,
        ivm: &crate::ivm::Ivm,
        joints: &[anim::Mat4x3],
        camera: &Camera,
        light: Option<&EntityLight>,
    ) {
        let mut triangles: Vec<Triangle> = Vec::new();
        for (object, joint) in ivm.objects.iter().zip(joints) {
            collect_ivm_triangles(object, joint, camera, light, &mut triangles);
        }
        self.rasterize_triangles(&ivm.texture, triangles);
    }

    fn rasterize_triangles(&mut self, texture: &Texture8, mut triangles: Vec<Triangle>) {
        triangles.sort_by(|a, b| b.depth.total_cmp(&a.depth));
        let mut depth = self.depth_buffer();
        let mut twins = KnockoutTwins::default();
        for triangle in &triangles {
            let twin = twins.get(texture, triangle);
            self.rasterize(texture, triangle, twin, &mut depth);
        }
    }

    /// A cleared 24-bit depth buffer covering the framebuffer.
    fn depth_buffer(&self) -> Vec<u32> {
        vec![crate::raster::DEPTH_CLEAR; self.width as usize * self.height as usize]
    }

    /// Draw one model triangle the way the reference's GL path does on
    /// llvmpipe: `DrawTriangles3D` under `GL_SRC_ALPHA` blending, a
    /// `GL_LEQUAL` test against the 24-bit depth buffer and a depth write for
    /// opaque triangles only.
    ///
    /// A blending triangle blends every texel at its alpha. When the page has
    /// a knock-out twin (`twin`), the reference draws the same triangle again
    /// through it at full alpha, so the non-STP texels come out opaque.
    fn rasterize(
        &mut self,
        texture: &Texture8,
        triangle: &Triangle,
        twin: bool,
        depth: &mut [u32],
    ) {
        if texture.width == 0 || texture.height == 0 {
            return;
        }
        let texture_width = f64::from(texture.width);
        let texture_height = f64::from(texture.height);
        // A blending packet's vertex alpha is the record's foreground weight;
        // `None` keeps the M16 per-packet half blend.
        let alpha = if triangle.blend {
            match triangle.blend_weight {
                // `CreateTmdObjectInternal`'s 0.2f background weight.
                Some(0x33) => 1.0 - 0.2f32,
                weight => 1.0 - f32::from(weight.unwrap_or(0x80)) / 256.0,
            }
        } else {
            1.0
        };
        let vertices = std::array::from_fn(|corner| {
            let vertex = &triangle.raster[corner];
            let nudge = triangle.uv_nudge[corner];
            // `PSXObject_Store` stores normalised coordinates and nudges a
            // degenerate corner by 0.003 on the stored float.
            let mut u = (f64::from(vertex.u) / texture_width) as f32;
            let mut v = (f64::from(vertex.v) / texture_height) as f32;
            let step = f64::from(if nudge & 4 != 0 { 0.0001f32 } else { 0.003f32 });
            if nudge & 1 != 0 {
                u = (f64::from(u) + step) as f32;
            }
            if nudge & 2 != 0 {
                v = (f64::from(v) + step) as f32;
            }
            crate::raster::GlVertex {
                x: vertex.position[0],
                y: vertex.position[1],
                depth: depth_ndc(vertex.w),
                w: vertex.w,
                u,
                v,
                color: [vertex.shade[0], vertex.shade[1], vertex.shade[2], alpha],
            }
        });
        let width = self.width as usize;
        let rgba = &mut self.rgba;
        crate::raster::draw_triangle(&vertices, self.width, self.height, |fragment| {
            let pixel = fragment.y * width + fragment.x;
            let Some(stored) = depth.get_mut(pixel) else {
                return;
            };
            if fragment.depth > *stored {
                return;
            }
            // The blended pass and its knockout twin leave depth alone; an
            // opaque triangle writes it even where its texel is transparent,
            // since the shader never discards.
            if !triangle.blend {
                *stored = fragment.depth;
            }
            // An untextured packet samples the white texture; every other
            // packet samples its page through the palette row. Palette entry
            // zero is the PSX colour key (alpha 0), which leaves the pixel.
            let (texel, blend) = match triangle.flat {
                Some(_) => ([255, 255, 255], false),
                None => {
                    let texel_u = clamp_texel(fragment.u, texture.width);
                    let texel_v = clamp_texel(fragment.v, texture.height);
                    let index = texel_v * texture.width as usize + texel_u;
                    let Some(&palette_index) = texture.indices.get(index) else {
                        return;
                    };
                    if palette_index == 0 {
                        return;
                    }
                    let color = texture.palette(triangle.palette_row, palette_index);
                    // A blending triangle blends every texel unless the
                    // knock-out twin paints its non-STP texels back opaque.
                    let blend = triangle.blend
                        && (!twin || texture.palette_stp(triangle.palette_row, palette_index));
                    ([color[0], color[1], color[2]], blend)
                }
            };
            let offset = pixel * 4;
            let Some(slot) = rgba.get_mut(offset..offset + 4) else {
                return;
            };
            let source_alpha = if blend {
                unorm8(fragment.color[3])
            } else {
                255
            };
            for (index, destination) in slot[..3].iter_mut().enumerate() {
                let source =
                    unorm8(f32::from(texel[index]) * (1.0 / 255.0) * fragment.color[index]);
                *destination = blend_unorm8(source, *destination, source_alpha);
            }
            slot[3] = 255;
        });
    }

    /// Paint a sorted scene list: each triangle selects its own texture page,
    /// mask sprites the camera's mask page, shadows the baked shadow mask and
    /// effects the room's decoded effect pages.
    fn draw_scene(
        &mut self,
        textures: &[&Texture8],
        page: Option<&Image>,
        effect_pages: &[Option<&Image>],
        items: &[SceneItem<'_>],
    ) {
        let mut depth = self.depth_buffer();
        let mut twins = KnockoutTwins::default();
        for item in items {
            match item {
                SceneItem::Shadow(poly) => {
                    self.rasterize_shadow(poly, &depth);
                }
                SceneItem::Triangle(triangle) => {
                    if let Some(texture) = textures.get(triangle.texture) {
                        let twin = twins.get(texture, triangle);
                        self.rasterize(texture, triangle, twin, &mut depth);
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

    /// Rasterize one ground shadow (`MarniDrawTrianglesPersp`): the model
    /// pipeline with bilinear sampling, depth tested but never written, and
    /// alpha-blended by the texel's coverage times the vertex colour.
    fn rasterize_shadow(&mut self, poly: &ShadowPoly, depth: &[u32]) {
        let texture = poly.texture;
        if texture.width == 0 || texture.height == 0 {
            return;
        }
        let width = self.width as usize;
        let rgba = &mut self.rgba;
        for triangle in &poly.triangles {
            crate::raster::draw_triangle(triangle, self.width, self.height, |fragment| {
                let pixel = fragment.y * width + fragment.x;
                if depth
                    .get(pixel)
                    .is_none_or(|&stored| fragment.depth > stored)
                {
                    return;
                }
                let texel = sample_bilinear(texture, fragment.u, fragment.v);
                let Some(slot) = rgba.get_mut(pixel * 4..pixel * 4 + 4) else {
                    return;
                };
                let alpha = unorm8(f32::from(texel[3]) * (1.0 / 255.0) * fragment.color[3]);
                for (index, destination) in slot[..3].iter_mut().enumerate() {
                    let source =
                        unorm8(f32::from(texel[index]) * (1.0 / 255.0) * fragment.color[index]);
                    *destination = blend_unorm8(source, *destination, alpha);
                }
                slot[3] = 255;
            });
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

        let page_width = page.width as usize;
        // `SubmitEffectSprite`: the tint is scaled by 0x80 and
        // `g_ColorScaleFactor / 255` and truncated to a byte, and the blend
        // level becomes `blend / 256` (opaque when zero), truncated to a byte.
        let tint_scale = (2.0f32 / 255.0) * (1.0f32 / 255.0);
        let color = quad.tint.map(|channel| {
            let value = (f32::from(channel) * 128.0 * tint_scale * 255.0) as i32;
            value.clamp(0, 255) as f32 / 255.0
        });
        let level = f32::from(quad.blend) * 0.003_906_25;
        let level = if level == 0.0 { 1.0 } else { level };
        let alpha = ((level * 255.0) as i32).clamp(0, 255) as f32 / 255.0;
        let target_stride = self.width as usize;

        // `MarniDX::DrawSprite`: two unprojected triangles, `(x, y) (x1, y)
        // (x, y1)` and `(x, y1) (x1, y) (x1, y1)`, through the same llvmpipe
        // setup as the models. The vertex order fixes the plane rounding that
        // decides which texel a pixel centre on a texel edge lands on.
        let [x, y, w, h] = [quad.pos[0], quad.pos[1], quad.size[0], quad.size[1]].map(|v| v as f32);
        let (x1, y1) = (x + w, y + h);
        let [top, sheet_height] = quad.sheet.map(u32::from);
        let sheet_height = sheet_height.clamp(1, page.height.saturating_sub(top).max(1));
        let page_w = page.width as f32;
        let sheet_v = f32::from(quad.uv[1]) - top as f32;
        let u0 = f32::from(quad.uv[0]) / page_w;
        let v0 = sheet_v / sheet_height as f32;
        let u1 = (f32::from(quad.uv[0]) + f32::from(quad.source[0].max(1))) / page_w;
        let v1 = (sheet_v + f32::from(quad.source[1].max(1))) / sheet_height as f32;
        let corner = |x: f32, y: f32, u: f32, v: f32| crate::raster::GlVertex {
            x,
            y,
            depth: 0.0,
            w: 1.0,
            u,
            v,
            color: [1.0; 4],
        };
        let triangles = [
            [
                corner(x, y, u0, v0),
                corner(x1, y, u1, v0),
                corner(x, y1, u0, v1),
            ],
            [
                corner(x, y1, u0, v1),
                corner(x1, y, u1, v0),
                corner(x1, y1, u1, v1),
            ],
        ];
        let rgba = &mut self.rgba;
        for triangle in &triangles {
            crate::raster::draw_triangle(triangle, self.width, self.height, |fragment| {
                let texel_x = clamp_texel(fragment.u, page.width);
                let texel_y = top as usize + clamp_texel(fragment.v, sheet_height);
                let source = texel_y * page_width * 4 + texel_x * 4;
                let target = fragment.y * target_stride * 4 + fragment.x * 4;
                let (Some(texel), Some(slot)) = (
                    page.rgba.get(source..source + 4),
                    rgba.get_mut(target..target + 4),
                ) else {
                    return;
                };
                if texel[3] == 0 {
                    return;
                }
                // `FlushSpriteCommands` hands the GPU an 8-bit vertex colour
                // and alpha. llvmpipe stores the fragment's texel-times-colour
                // as bytes and blends them in 8-bit fixed point, rounding
                // each `x * y / 255` product.
                let unorm = |value: f32| (value * 255.0).round().clamp(0.0, 255.0) as u32;
                let alpha = unorm(f32::from(texel[3]) / 255.0 * alpha);
                for ((channel, &source), &color) in
                    slot[..3].iter_mut().zip(&texel[..3]).zip(&color)
                {
                    let source = unorm(f32::from(source) / 255.0 * color);
                    let blended =
                        unorm_mul(source, alpha) + unorm_mul(u32::from(*channel), 255 - alpha);
                    *channel = blended.min(255) as u8;
                }
                slot[3] = 255;
            });
        }
    }
}

/// llvmpipe's `lp_build_mul` on 8-bit normalised values: `a * b / 255`,
/// rounded to nearest.
fn unorm_mul(a: u32, b: u32) -> u32 {
    let product = a * b + 0x80;
    (product + (product >> 8)) >> 8
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
    /// Per-channel RGB multiplier applied to every shaded triangle (default
    /// white); the shared plumbing for `model_op` and `objs_hide`.
    pub tint: [u8; 3],
    /// The record's background blend weight (the original's `+0x68` field),
    /// from `record_blend_weight` and the record's `DAT_004d2be0` override.
    /// `None` keeps the M16 per-packet half blend; `Some(weight)` mixes a
    /// blended texel as `source * (256 - weight) + background * weight`, so
    /// `Some(0)` draws opaque and `Some(0x30)` keeps 18.75% background.
    pub blend_weight: Option<u8>,
    /// Joint draw gate: bit `i` set means joint `i` is hidden and never draws
    /// (not even in the mirror copy), the port's stand-in for the joint flag
    /// byte's bit 0.
    pub hidden_joints: u32,
    /// Where the room lights latch (`update_entity_lighting`): an entity's
    /// own matrix translation (`ent + 0x34`, its feet), not its root joint.
    /// `None` latches at the first joint, which for a room object or item
    /// record is the record position the original passes (`obj + 0x34`).
    pub light_pos: Option<[i32; 3]>,
    /// Yawn (ids `0x0D`/`0x12`): `render_entity` re-latches the lights at
    /// every joint's world translation before drawing it.
    pub per_joint_light: bool,
    /// The mesh object the reference draws through its complex pool
    /// (`ComplexTmdObjectSetup` / `FUN_00486df0`) instead of the TMD path:
    /// Plant 42's root ball. See [`collect_complex_pool`].
    pub complex_object: Option<usize>,
    /// The Plant 42 body's light matrix: `MulMatrix0(g_lightMatrix, &m, &m)`
    /// with `m` the identity, run in place. See
    /// [`EntityLight::aliased_identity_product`].
    pub aliased_light: bool,
    /// Per-joint `JointSetColorTint` colours (1/128 multipliers); a non-zero
    /// entry draws the joint unlit in that colour.
    pub joint_colours: [[u8; 3]; 32],
}

/// No joint carries a `JointSetColorTint` colour.
pub const NO_JOINT_COLOURS: [[u8; 3]; 32] = [[0; 3]; 32];

/// The original's `GetTmdBlendMode` plus `CreateTmdObjectInternal`'s remap:
/// the background weight of the first semi-transparent primitive of `model`,
/// or `None` when the TMD carries none.
///
/// `GetTmdBlendMode` returns `blend_override` when it is set, otherwise the
/// primitive texpage ABR field's raw table value `{0x80, 0x00, 0x80, 0xD0}`.
/// `CreateTmdObjectInternal` then keeps only that value's zero/non-zero
/// distinction: `0x00` leaves the record opaque and every non-zero value
/// collapses to the `0x80` half blend, except the `cmd_omodel_set` `0x30`
/// override, which becomes the `0x33` (0.2) background weight. One primitive
/// marks every record of the TMD.
pub(crate) fn record_blend_weight(model: &Tmd, blend_override: Option<u8>) -> Option<u8> {
    const ABR_MODE: [u8; 4] = [0x80, 0x00, 0x80, 0xD0];
    let prim = model
        .objects
        .iter()
        .flat_map(|object| &object.prims)
        .find(|prim| prim.blend)?;
    let mode = blend_override.unwrap_or(ABR_MODE[usize::from((prim.tsb >> 5) & 3)]);
    Some(match mode {
        0 => 0x00,
        0x30 => 0x33,
        _ => 0x80,
    })
}

impl EntityMesh<'_> {
    /// Whether joint `index` draws.
    pub(crate) fn joint_visible(&self, index: usize) -> bool {
        index < 32 && self.hidden_joints & (1 << index) == 0
    }
}

/// A ground shadow ready to be interleaved with the masks and the model.
///
/// The quad spans `[-half_x, +half_x] x [-half_z, +half_z]` around `shift`
/// in the entity's local frame, is yawed by `angle` and placed at `pos` (Y is
/// the floor height). `texture` is the baked coverage page: white texels whose alpha is
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
    /// The quad's centre in the entity's local X/Z: an uneven
    /// `BillboardSetRect` (the player's stairway quad) sits off the entity.
    pub shift: [i32; 2],
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

/// Every input of one gameplay frame, bundled so the software rasterizer
/// ([`GameplayScene::draw`]) and the optional GPU renderer (`crate::gpu`) draw
/// from the same description. The fields are the arguments of
/// [`draw_gameplay_scene_with_effects`].
#[derive(Debug, Clone, Copy)]
pub struct GameplayScene<'a> {
    pub background: Option<&'a Image>,
    pub origin: [i32; 2],
    pub colour: [u8; 3],
    pub meshes: &'a [EntityMesh<'a>],
    pub shadows: &'a [Shadow<'a>],
    pub camera: &'a Camera,
    pub lighting: &'a Lighting,
    pub mask_layer: Option<&'a MaskLayer<'a>>,
    pub effect_layer: Option<&'a EffectLayer<'a>>,
    pub mirror_from: usize,
    pub mirror: Option<&'a MirrorPass>,
}

impl GameplayScene<'_> {
    /// Rasterize the scene in software; see [`draw_gameplay_scene_with_effects`].
    pub fn draw(&self, framebuffer: &mut Framebuffer) {
        draw_gameplay_scene_with_effects(
            framebuffer,
            self.background,
            self.origin,
            self.colour,
            self.meshes,
            self.shadows,
            self.camera,
            self.lighting,
            self.mask_layer,
            self.effect_layer,
            self.mirror_from,
            self.mirror,
        );
    }
}

/// Draw one gameplay frame: the background, then the depth-sorted scene list.
///
/// The ground shadows are submitted first, then the meshes' triangles collected
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
    shadows: &[Shadow<'_>],
    camera: &Camera,
    lighting: &Lighting,
    mask_layer: Option<&MaskLayer<'_>>,
) {
    draw_gameplay_scene_with_effects(
        framebuffer,
        background,
        [0, 0],
        [255; 3],
        meshes,
        shadows,
        camera,
        lighting,
        mask_layer,
        None,
        0,
        None,
    );
}

/// [`draw_gameplay_scene`] with the room's effect billboards interleaved.
///
/// `origin` is the display origin the background samples through (screen
/// panning and shake; the sampled window shifts the opposite way, so a
/// positive X moves the image right) and `colour` its global colour
/// multiplier. The effect quads are submitted after the masks (the original's
/// `update_2d_effects` runs after the entity pass), so at an exact key tie an
/// effect paints over a mask or triangle submitted earlier in the frame.
/// `mirror_from` is the first mesh index eligible for the mirror pass: the
/// room object and item meshes come first and are never reflected.
///
/// `mirror` appends the entity meshes' mirrored triangles, drawn through the
/// reflected camera and visibility-tested joint by joint, after the primary
/// pass (the original's second submission). Hidden joints never draw and never
/// reach the mirror copy.
#[allow(clippy::too_many_arguments)]
pub fn draw_gameplay_scene_with_effects<'a>(
    framebuffer: &mut Framebuffer,
    background: Option<&Image>,
    origin: [i32; 2],
    colour: [u8; 3],
    meshes: &[EntityMesh<'_>],
    shadows: &[Shadow<'a>],
    camera: &Camera,
    lighting: &Lighting,
    mask_layer: Option<&MaskLayer<'_>>,
    effect_layer: Option<&EffectLayer<'_>>,
    mirror_from: usize,
    mirror: Option<&MirrorPass>,
) {
    framebuffer.clear();
    if let Some(background) = background {
        framebuffer.draw_background(background, origin, colour);
    }

    let mut items = Vec::new();

    // The shadows are submitted before the models, so a mask or triangle whose
    // rounded key ties one wins the tie the way the original's later
    // submission into the shared ordering table does.
    items.extend(
        shadow_polys(shadows, camera)
            .into_iter()
            .map(SceneItem::Shadow),
    );

    let mut triangles = Vec::new();
    for (texture, mesh) in meshes.iter().enumerate() {
        let light = lighting.latched(mesh.light_pos.unwrap_or(root_position(mesh.joints)));
        // The room's own object and item models come first in the list (the
        // original's room pass, which uses its own D3D light setup); every
        // later mesh is an entity and uses the world-space dot.
        let room_object = texture < mirror_from;
        for (index, (object, joint)) in mesh.mesh.objects.iter().zip(mesh.joints).enumerate() {
            if !mesh.joint_visible(index) {
                continue;
            }
            if mesh.complex_object == Some(index) {
                collect_complex_pool(object, joint, camera, texture, &mut triangles);
                continue;
            }
            let light = if mesh.per_joint_light {
                lighting.latched(joint.t)
            } else {
                light
            };
            let light = if mesh.aliased_light {
                light.aliased_identity_product()
            } else {
                light
            };
            collect_triangles(
                object,
                joint,
                camera,
                Some(lighting),
                Some(light),
                true,
                texture,
                mesh.tint,
                mesh.joint_colours.get(index).copied(),
                mesh.blend_weight,
                room_object,
                &mut triangles,
            );
        }
    }
    // The mirror copy is drawn through the reflected camera and submitted
    // after the primary pass; the stable sort keeps it after the live entity
    // at equal depth. Only the entity meshes reflect: the original's mirror
    // pass re-runs the character joints, never the room's own objects.
    if let Some(mirror) = mirror {
        for (texture, mesh) in meshes.iter().enumerate().skip(mirror_from) {
            let light = lighting.latched(mesh.light_pos.unwrap_or(root_position(mesh.joints)));
            for (index, (object, joint)) in mesh.mesh.objects.iter().zip(mesh.joints).enumerate() {
                if !mesh.joint_visible(index) || !mirror.joint_visible(joint.t) {
                    continue;
                }
                let light = if mesh.per_joint_light {
                    lighting.latched(joint.t)
                } else {
                    light
                };
                // The mirrored X scale reverses screen-space winding, so the
                // usual back-face test would drop every front face; the
                // reflection is submitted unculled (the original culls with
                // the flipped scale baked into its projection).
                collect_triangles(
                    object,
                    joint,
                    &mirror.camera,
                    Some(lighting),
                    Some(light),
                    false,
                    texture,
                    mesh.tint,
                    mesh.joint_colours.get(index).copied(),
                    mesh.blend_weight,
                    false,
                    &mut triangles,
                );
            }
        }
    }
    // The integer scene key can tie triangles that are less than a unit apart;
    // the stable sort keeps this submission order for those ties.
    triangles.sort_by(|a, b| b.depth.total_cmp(&a.depth));
    items.extend(triangles.into_iter().map(SceneItem::Triangle));

    if let Some(layer) = mask_layer {
        collect_masks(layer, camera.screen, &mut items);
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
        &effect_pages,
        &items,
    );
}

/// The world position a mesh's light is latched from: its root joint's
/// translation, which the entity matrix sets to the entity's own position.
pub(crate) fn root_position(joints: &[anim::Mat4x3]) -> [i32; 3] {
    joints.first().map_or([0; 3], |joint| joint.t)
}

/// Append a layer's active mask sprites to `items`, farthest submission
/// first.
///
/// [`mask::mask_sprite_key`] folds together the shared per-(room, camera) bias
/// record, the room's per-entry overrides and the zero-depth rule (a computed
/// brightness depth of exactly zero sorts at the fixed key 550). `offset` is
/// the frame's screen offset (the shake), which the original adds to every
/// mask sprite's screen position.
fn collect_masks(layer: &MaskLayer<'_>, offset: [i32; 2], items: &mut Vec<SceneItem<'_>>) {
    items.extend(mask_quads(layer, offset).into_iter().map(SceneItem::Mask));
}

/// A layer's active mask sprites in submission order, the list
/// [`collect_masks`] feeds the painter's sort.
pub(crate) fn mask_quads(layer: &MaskLayer<'_>, offset: [i32; 2]) -> Vec<MaskQuad> {
    let mut quads = Vec::new();
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
        quads.push(MaskQuad {
            key,
            uv: (u32::from(sprite.uv.0), u32::from(sprite.uv.1)),
            pos: (sprite.pos.0 + offset[0], sprite.pos.1 + offset[1]),
            size: (u32::from(sprite.size.0), u32::from(sprite.size.1)),
        });
    }
    quads
}

/// Every ground shadow's projected commands in `DrawFadeSpr`'s queue order,
/// the items [`collect_shadow`] submits to the painter's sort.
pub(crate) fn shadow_polys<'a>(shadows: &[Shadow<'a>], camera: &Camera) -> Vec<ShadowPoly<'a>> {
    let mut items = Vec::new();
    for (layer, &index) in shadow_order(shadows, camera).iter().enumerate() {
        collect_shadow(&shadows[index], layer as i32, camera, &mut items);
    }
    items
        .into_iter()
        .filter_map(|item| match item {
            SceneItem::Shadow(poly) => Some(poly),
            _ => None,
        })
        .collect()
}

/// `DrawFadeSpr`'s queue order: a selection pass over the shadows' mean
/// entity-corner depths (`RotAverage4`, clamped like the queue's sort key),
/// swapping whole records.
fn shadow_order(shadows: &[Shadow<'_>], camera: &Camera) -> Vec<usize> {
    let keys: Vec<u32> = shadows
        .iter()
        .map(|shadow| {
            let (rotation, t) = shadow_matrix(shadow, 0, camera);
            let (left, right) = (
                shadow.shift[0] - shadow.half_x,
                shadow.shift[0] + shadow.half_x,
            );
            let (back, front) = (
                shadow.shift[1] - shadow.half_z,
                shadow.shift[1] + shadow.half_z,
            );
            let sum: i32 = [(left, front), (right, front), (left, back), (right, back)]
                .iter()
                .map(|&(x, z)| {
                    let r = i32::from(rotation[2][0]) * x + i32::from(rotation[2][2]) * z;
                    (((r + ((r >> 31) & 0xFFF)) >> 12) + t[2]) >> 2
                })
                .sum();
            let depth = ((sum + ((sum >> 31) & 3)) >> 2) as u32;
            if depth > 0xFEF { 0xFF0 } else { depth }
        })
        .collect();
    let mut order: Vec<usize> = (0..shadows.len()).collect();
    for a in 0..order.len() {
        for b in a + 1..order.len() {
            if keys[order[b]] < keys[order[a]] {
                order.swap(a, b);
            }
        }
    }
    order
}

/// The shadow's composed matrix: `CompMatrix(g_RoomCameraData, local)` with
/// `local` the identity yawed by the entity angle and translated to the
/// ground point, lifted by `layer` steps of 2 so queued shadows never share a
/// plane.
fn shadow_matrix(shadow: &Shadow<'_>, layer: i32, camera: &Camera) -> ([[i16; 3]; 3], [i32; 3]) {
    let local = anim::Mat4x3 {
        r: crate::player::rot_matrix_y(shadow.angle),
        t: [shadow.pos[0], shadow.pos[1] + 2 * layer, shadow.pos[2]],
    };
    ref_composed(camera, &local)
}

/// Project and clip one ground shadow into the scene list (`DrawFadeSpr`
/// and `AddFadePoly`).
///
/// The drawn quad spans `+-halfW x +-halfH` from the record's corner 1, in
/// the composed matrix's integer 4.12 arithmetic. It is clipped against view
/// Z 128 along the edge ring 0->1->3->2 and projected with truncating integer
/// divides. A pentagon becomes two four-corner commands, each drawn as the
/// fan (0,1,2) + (0,2,3). The painter's key is the truncating mean of the four
/// *unclipped* corner view Zs.
fn collect_shadow<'a>(
    shadow: &Shadow<'a>,
    layer: i32,
    camera: &Camera,
    items: &mut Vec<SceneItem<'a>>,
) {
    const FADE_NEAR: i32 = 128;
    if shadow.texture.width == 0 || shadow.texture.height == 0 {
        return;
    }
    let half_w = shadow.shift[0] + shadow.half_x;
    let half_h = shadow.shift[1] + shadow.half_z;
    if half_w <= 0 || half_h <= 0 {
        return;
    }
    let (rotation, t) = shadow_matrix(shadow, layer, camera);
    let mut cvx = [0i32; 4];
    let mut cvy = [0i32; 4];
    let mut cvz = [0i32; 4];
    let mut cu = [0i32; 4];
    let mut cv = [0i32; 4];
    for corner in 0..4 {
        let cx = if corner == 0 || corner == 2 {
            -half_w
        } else {
            half_w
        };
        let cz = if corner == 0 || corner == 1 {
            half_h
        } else {
            -half_h
        };
        let rotate = |row: usize| {
            let r = i32::from(rotation[row][0]) * cx + i32::from(rotation[row][2]) * cz;
            ((r + ((r >> 31) & 0xFFF)) >> 12) + t[row]
        };
        cvx[corner] = rotate(0);
        cvy[corner] = rotate(1);
        cvz[corner] = rotate(2);
        cu[corner] = ((cx + half_w) << 12) / (2 * half_w);
        cv[corner] = ((half_h - cz) << 12) / (2 * half_h);
    }

    let edge = [0usize, 1, 3, 2];
    // (x, y, z, u, v) per clipped corner.
    let mut ring: Vec<[i32; 5]> = Vec::with_capacity(5);
    for step in 0..4 {
        let (a, b) = (edge[step], edge[(step + 1) & 3]);
        let (za, zb) = (cvz[a], cvz[b]);
        let a_in = za >= FADE_NEAR;
        if a_in {
            ring.push([cvx[a], cvy[a], za, cu[a], cv[a]]);
        }
        if a_in != (zb >= FADE_NEAR) {
            let t = ((FADE_NEAR - za) << 12) / (zb - za);
            let lerp = |from: i32, to: i32| from + (((to - from) * t) >> 12);
            ring.push([
                lerp(cvx[a], cvx[b]),
                lerp(cvy[a], cvy[b]),
                FADE_NEAR,
                lerp(cu[a], cu[b]),
                lerp(cv[a], cv[b]),
            ]);
        }
    }
    if ring.is_empty() {
        return;
    }

    let focal = camera.fov;
    let lift = shadow.lift;
    let t1 = t[1];
    let color = shadow.tint.map(|byte| f32::from(byte) * (1.0 / 255.0));
    let corner = |k: usize| {
        let [vx, vy, vz, u, v] = ring[k];
        // `DrawFadeSpr` projects about the fixed 160/120 centre: the shadow
        // never follows the screen offset or shake the models and masks do.
        let px = (vx * focal) / vz + CENTER_X;
        let py = ((t1 + lift - (vy - t1)) * focal) / vz + CENTER_Y;
        let wz = f32::from(vz.min(30000) as i16);
        crate::raster::GlVertex {
            x: f32::from(px as i16),
            y: f32::from(py as i16),
            depth: depth_ndc(wz),
            w: wz,
            u: f32::from(u as i16) * (1.0 / 4096.0),
            v: f32::from(v as i16) * (1.0 / 4096.0),
            color: [color[0], color[1], color[2], 1.0],
        }
    };
    let last = ring.len() - 1;
    let commands: &[[usize; 4]] = if ring.len() > 4 {
        &[[0, 1, 2, 3], [0, 3, 4, 4]]
    } else {
        &[[0, 1, 2, 3]]
    };
    let mut triangles = Vec::with_capacity(4);
    for command in commands {
        let quad = command.map(|k| corner(k.min(last)));
        triangles.push([quad[0], quad[1], quad[2]]);
        triangles.push([quad[0], quad[2], quad[3]]);
    }

    let sort_z = (cvz[0] + cvz[1] + cvz[2] + cvz[3]) / 4;
    items.push(SceneItem::Shadow(ShadowPoly {
        key: triangle_depth_key(f64::from(sort_z.max(0))),
        triangles,
        texture: shadow.texture,
    }));
}

/// A cut camera: a 4.12 view rotation, a world-unit translation and the cut's
/// focal length in pixels.
///
/// `view` maps a world point to the screen's Y-down camera frame with a
/// truncating 4.12 multiply (see `Camera::view_position`); `trans` holds the
/// rotated eye. `roll` is the cut's 12-bit head roll and `screen` the frame's
/// screen-shake offset, both applied at projection time so the view matrix
/// stays the room's own. `from` and `look_at` are the source points the matrix
/// was built from; the mirror pass needs them to rebuild the view from
/// reflected points rather than composing a reflection into the matrix (the
/// game rebuilds the camera from a reflected record and only then flips its X
/// scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Camera {
    pub view: [[i32; 3]; 3],
    pub trans: [i32; 3],
    pub fov: i32,
    /// The cut's head-roll angle in 12-bit units, composed into the view.
    /// Every shipped RDT camera stores 0, so the path is synthetic-only.
    pub roll: i32,
    /// The frame's screen-shake offset in pixels, added at projection time.
    pub screen: [i32; 2],
    pub from: [i32; 3],
    pub look_at: [i32; 3],
    /// The camera was reflected about a mirror plane ([`Camera::mirrored`]);
    /// the reference negates `g_RoomCameraData`'s X row to match.
    pub mirrored: bool,
}

/// `a * b` for two 4.12 rotation matrices, each product truncated toward zero
/// at 12 fractional bits and summed, the game's `MulMatrix0` composition.
fn mul_fixed3(a: &[[i32; 3]; 3], b: &[[i32; 3]; 3]) -> [[i32; 3]; 3] {
    std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            let mut sum = 0i64;
            for k in 0..3 {
                let product = i64::from(a[i][k]) * i64::from(b[k][j]);
                sum += (product + ((product >> 63) & 0xFFF)) >> FIXED_BITS;
            }
            sum as i32
        })
    })
}

/// The GTE-style integer square root of a non-negative length squared,
/// clamped to at least 1 so a degenerate direction never divides by zero.
fn direction_length(squared: i128) -> i64 {
    let clamped = squared.min(i128::from(i32::MAX)) as i32;
    i64::from(crate::sfx::integer_sqrt(clamped)).max(1)
}

/// A 12-bit angle from a sine and cosine numerator pair.
fn angle12(sin_numerator: i64, cos_numerator: i64) -> i32 {
    let radians = (sin_numerator as f64).atan2(cos_numerator as f64);
    ((radians * (2048.0 / std::f64::consts::PI)) as i32) & 0x0FFF
}

impl Camera {
    /// Build the view rotation and translation from a from/to point pair. The
    /// focal length is `fov`; the roll is 0 and the shake offset is zero.
    ///
    /// This is the door animation's `CAM_MATRIX` camera as well as the room
    /// cut camera: the same from/look-at construction the original's
    /// `MatrixToCamera` performs, with the scene's focal length in pixels.
    pub fn from_points(from: [i32; 3], to: [i32; 3], fov: i32) -> Self {
        Self::from_points_rolled(from, to, fov, 0)
    }

    /// [`Camera::from_points`] with the cut's head roll composed into the view.
    ///
    /// The original derives the view's pitch and yaw from the from/to vector,
    /// builds the basis from its 14-bit trig tables and truncates the 4.12
    /// result; the port does the same (`anim::rotation_matrix` with the
    /// GTE-style integer lengths), then composes `rotation_matrix(0, 0, roll)`
    /// into the view and computes the translation with a truncating `>>12`.
    /// Every shipped camera stores roll 0, so the composition is exercised by
    /// synthetic tests only.
    pub fn from_points_rolled(from: [i32; 3], to: [i32; 3], fov: i32, roll: i32) -> Self {
        let dx = i64::from(to[0]) - i64::from(from[0]);
        let dy = i64::from(to[1]) - i64::from(from[1]);
        let dz = i64::from(to[2]) - i64::from(from[2]);
        // The original normalises the horizontal direction with the GTE's
        // integer square root: h = sqrt(dx^2 + dz^2). The pitch is then
        // atan2(dy, h), i.e. sin pitch = dy/len and cos pitch = h/len for the
        // full length len = sqrt(dx^2 + dy^2 + dz^2).
        let h = direction_length(i128::from(dx) * i128::from(dx) + i128::from(dz) * i128::from(dz));
        // The view's rows are rotation_matrix(pitch, yaw, 0):
        //   row 0 = (cos yaw, 0, sin yaw)
        //   row 1 = (-sin pitch sin yaw, cos pitch, sin pitch cos yaw)
        //   row 2 = (-cos pitch sin yaw, -sin pitch, cos pitch cos yaw)
        // with sin yaw = -dx/h and sin pitch = dy/len, the original's basis.
        let yaw = angle12(-dx, dz);
        let pitch = angle12(dy, h);
        let mut view = anim::rotation_matrix(pitch, yaw, 0);
        // The game projects a world point by negating its Y before the camera
        // rotation and using a Y-flipped translation (the world's Y axis grows
        // downwards while the camera basis is built for screen-down Y). Fold
        // that into the stored matrix: negate the Y column, then compute the
        // translation with the flipped rows so the camera's look-at point maps
        // to the screen centre. `project` then stays a plain `120 - vy`.
        view = view.map(|row| [row[0], -row[1], row[2]]);
        if roll != 0 {
            view = mul_fixed3(&anim::rotation_matrix(0, 0, roll), &view);
        }
        let trans = view.map(|row| {
            let sum = i64::from(row[0]) * i64::from(from[0])
                + i64::from(row[1]) * i64::from(from[1])
                + i64::from(row[2]) * i64::from(from[2]);
            // `trans = R * (-from)` with the sum truncated toward zero.
            (-((sum + ((sum >> 63) & 0xFFF)) >> FIXED_BITS)) as i32
        });
        Self {
            view,
            trans,
            fov,
            roll,
            screen: [0, 0],
            from,
            look_at: to,
            mirrored: false,
        }
    }

    /// Build the view rotation and translation from the cut's position and
    /// look-at point, composing its roll. The focal length is `cut.fov`.
    pub fn from_cut(cut: &Cut) -> Self {
        Self::from_points_rolled(cut.pos, cut.look_at, cut.fov, cut.roll)
    }

    /// The camera reflected about a room mirror plane, with the handedness
    /// flip the original applies after rebuilding the view.
    ///
    /// `axis_x` selects the plane `X = plane` (the X pair is folded) rather
    /// than `Z = plane` (the Z pair); `plane` is `g_mirrorPlaneCoord`. The view
    /// matrix's X scale is negated to undo the reflection's handedness. The
    /// roll and screen offset carry over from the live camera.
    pub fn mirrored(&self, axis_x: bool, plane: i32) -> Camera {
        let fold = plane.wrapping_mul(2);
        let mirror = |point: [i32; 3]| {
            if axis_x {
                [fold.wrapping_sub(point[0]), point[1], point[2]]
            } else {
                [point[0], point[1], fold.wrapping_sub(point[2])]
            }
        };
        let mut camera = Camera::from_points_rolled(
            mirror(self.from),
            mirror(self.look_at),
            self.fov,
            self.roll,
        );
        camera.screen = self.screen;
        camera.view[0] = camera.view[0].map(|entry| -entry);
        camera.trans[0] = -camera.trans[0];
        camera.mirrored = true;
        camera
    }

    /// Project a world-space point to screen pixels with truncating integer
    /// math: `sx = (vx*f)/vz + 160 + shake_x`, `sy = 120 + shake_y - (vy*f)/vz`,
    /// every quotient truncated toward zero.
    ///
    /// `None` when the point is behind or inside the near plane
    /// (`view_z < 2*fov`; the plane itself is accepted).
    pub fn project(&self, world: [i32; 3]) -> Option<[i32; 2]> {
        let [vx, vy, vz] = self.view_position(world);
        let fov = i64::from(self.fov);
        let vz = i64::from(vz);
        if vz <= 0 || vz < 2 * fov {
            return None;
        }
        let sx = i64::from(vx) * fov / vz + i64::from(CENTER_X) + i64::from(self.screen[0]);
        // `vy` is already in the screen-down frame (see `from_cut`).
        let sy = i64::from(CENTER_Y) + i64::from(self.screen[1]) - i64::from(vy) * fov / vz;
        Some([sx as i32, sy as i32])
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

/// The mirror visibility probe.
///
/// `axis_x` selects the plane `X = plane` (else `Z = plane`), `plane` is the
/// plane coordinate and the extents bound the crossing coordinate along the
/// other axis. Returns the crossing coordinate when the camera and the point
/// lie on the same side of the plane and the camera-to-point segment crosses
/// the plane inside the mirror's extent, `None` otherwise.
pub fn mirror_point_visible(
    axis_x: bool,
    plane: i32,
    extent_min: u16,
    extent_max: u16,
    camera: [i32; 3],
    point: [i32; 3],
) -> Option<i32> {
    let plane_coord = |p: [i32; 3]| {
        if axis_x { (p[0], p[2]) } else { (p[2], p[0]) }
    };
    let (point_plane, point_cross) = plane_coord(point);
    let (camera_plane, camera_cross) = plane_coord(camera);

    // The camera and the point must lie on the same side of the plane.
    if ((camera_plane - plane) ^ (point_plane - plane)) & i32::MIN != 0 {
        return None;
    }

    let denominator = point_plane
        .wrapping_sub(plane.wrapping_mul(2))
        .wrapping_add(camera_plane);
    if denominator == 0 {
        return None;
    }
    let product = (point_cross - camera_cross).wrapping_mul(camera_plane.wrapping_sub(plane));
    let crossing = camera_cross.wrapping_add(product / denominator);
    let span = u32::from(extent_max).wrapping_sub(u32::from(extent_min));
    if (crossing as u32).wrapping_sub(u32::from(extent_min)) < span {
        Some(crossing)
    } else {
        None
    }
}

/// One frame's mirror pass: the reflected camera plus the mirror geometry
/// needed to test each entity joint for visibility.
#[derive(Debug, Clone, Copy)]
pub struct MirrorPass {
    /// The plane is `X = plane` (else `Z = plane`).
    pub axis_x: bool,
    /// The plane coordinate.
    pub plane: i32,
    /// Extent minimum along the cross axis.
    pub extent_min: u16,
    /// Extent maximum along the cross axis.
    pub extent_max: u16,
    /// The camera's world position (the original tests against the camera
    /// record's eye).
    pub camera_pos: [i32; 3],
    /// The reflected camera the reflection is drawn through.
    pub camera: Camera,
}

impl MirrorPass {
    /// Whether a joint at `pos` is visible in this mirror.
    pub fn joint_visible(&self, pos: [i32; 3]) -> bool {
        mirror_point_visible(
            self.axis_x,
            self.plane,
            self.extent_min,
            self.extent_max,
            self.camera_pos,
            pos,
        )
        .is_some()
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

    /// Latch the room's lights for one entity at `entity_pos`.
    ///
    /// The original samples the lights once per entity and then evaluates each
    /// vertex normal against the latched directions. A point light's falloff is
    /// measured on X and Z only, `radius - sqrt(dx^2 + dz^2)` clamped at zero,
    /// and its colour is attenuated with a truncating integer division. The
    /// light's stored direction points from the light towards the entity; the
    /// latched `direction` is its negation, the direction from the entity
    /// towards the light, so a normal `N` gets `max(0, dot(N, direction))`.
    /// The per-channel colour caps at `0x80` before the `0..255` conversion.
    /// The 12-bit ambient truncates to eight bits with `(value * 255) / 4096`.
    pub fn latched(&self, entity_pos: [i32; 3]) -> EntityLight {
        let ambient = self.ambient.map(|channel| {
            let channel = i32::from(channel).clamp(0, 0x1000);
            f64::from((channel * 255) / 4096)
        });
        let sources = std::array::from_fn(|index| {
            let light = &self.lights[index];
            let (offset, color) = if light.kind == 0 {
                let offset = [
                    entity_pos[0] - light.pos[0],
                    entity_pos[1] - light.pos[1],
                    entity_pos[2] - light.pos[2],
                ];
                // The original reads the radius as unsigned: 0xFFFF is the
                // widest falloff, and only an exact zero disables the light.
                let radius = i64::from(light.radius);
                let attenuation = if radius == 0 {
                    0
                } else {
                    let dx = i64::from(offset[0]);
                    let dz = i64::from(offset[2]);
                    (radius - integer_sqrt(dx * dx + dz * dz)).max(0)
                };
                let color = if radius == 0 {
                    [0; 3]
                } else {
                    light.color.map(|channel| {
                        ((u64::from(channel) * attenuation as u64) / radius as u64) as u8
                    })
                };
                (offset, color)
            } else {
                (light.pos, light.color)
            };
            // The original's normalize clamps a near-zero length to 1.0, then
            // stores the 4.12 fixed-point vector with its Y negated
            // (`FUN_0040ac80`). The colour caps at `0x80` in the same record.
            let length = (f64::from(offset[0]).powi(2)
                + f64::from(offset[1]).powi(2)
                + f64::from(offset[2]).powi(2))
            .sqrt()
            .max(1.0);
            EntityLightSource {
                dir: [
                    (f64::from(offset[0]) / length * 4096.0) as i32,
                    (-f64::from(offset[1]) / length * 4096.0) as i32,
                    (f64::from(offset[2]) / length * 4096.0) as i32,
                ],
                color: color.map(|channel| channel.min(0x80)),
            }
        });
        EntityLight { ambient, sources }
    }
}

/// One entity's latched room lighting: the ambient colour and the three light
/// directions/colours, all in the rasterizer's `0..=255` colour scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntityLight {
    /// Ambient colour.
    pub ambient: [f64; 3],
    /// The room's three lights, latched at the entity's position.
    pub sources: [EntityLightSource; 3],
}

impl EntityLight {
    /// The light rows after the Plant 42 body's
    /// `MulMatrix0(g_lightMatrix, &m, &m)` over an identity `m`. The product
    /// is written element by element into its own right operand, so every
    /// row after the first reads columns the earlier rows already
    /// overwrote; only the first light keeps its direction.
    pub fn aliased_identity_product(mut self) -> Self {
        let rows = self.sources.map(|source| source.dir);
        let mut product = [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]];
        for (i, row) in rows.iter().enumerate() {
            // Indexed on purpose: each store must be visible to the next read.
            #[allow(clippy::needless_range_loop)]
            for j in 0..3 {
                let sum: i64 = (0..3)
                    .map(|k| {
                        let value = i64::from(row[k]) * i64::from(product[k][j]);
                        (value + ((value >> 63) & 0xFFF)) >> FIXED_BITS
                    })
                    .sum();
                // `MulMatrix0` stores shorts.
                product[i][j] = i32::from(sum as i16);
            }
        }
        for (source, row) in self.sources.iter_mut().zip(product) {
            source.dir = row;
        }
        self
    }
}

/// One latched light in the reference's stored form: the 4.12 direction
/// `F * normalize(entity - light)` (`FUN_0040ac80`) and its attenuated,
/// `0x80`-capped colour byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityLightSource {
    pub dir: [i32; 3],
    pub color: [u8; 3],
}

/// The ABE (semi-transparency) bit of a primitive's CLUT word.
pub(crate) const ABE_BIT: u16 = 0x8000;

/// Integer square root with truncation, as the original's `SquareRoot0` does.
fn integer_sqrt(value: i64) -> i64 {
    if value <= 0 {
        return 0;
    }
    let mut root = (value as f64).sqrt() as i64;
    while root * root > value {
        root -= 1;
    }
    while (root + 1) * (root + 1) <= value {
        root += 1;
    }
    root
}

/// One projected vertex ready for rasterization, in the reference's GL
/// vertex terms ([`crate::raster::GlVertex`]).
struct RasterVertex {
    /// Screen position (game pixels, Y down).
    position: [f32; 2],
    /// View-space Z.
    w: f32,
    /// Texture coordinates in texels; the rasterizer normalises them.
    u: f32,
    v: f32,
    /// Vertex colour, `1.0` = the texel as stored.
    shade: [f32; 3],
}

/// Per draw, which (page, palette row) pairs have an STP knock-out twin.
#[derive(Default)]
struct KnockoutTwins(std::collections::HashMap<(usize, usize), bool>);

impl KnockoutTwins {
    /// Whether `triangle` blends through a page with a knock-out twin.
    fn get(&mut self, texture: &Texture8, triangle: &Triangle) -> bool {
        if !triangle.blend || triangle.flat.is_some() {
            return false;
        }
        let key = (texture.indices.as_ptr() as usize, triangle.palette_row);
        *self
            .0
            .entry(key)
            .or_insert_with(|| texture.has_knockout_twin(triangle.palette_row))
    }
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
    /// A solid gouraud colour; `None` samples the texture instead.
    flat: Option<[u8; 3]>,
    /// The packet's semi-transparency bit: rasterize with a per-texel blend
    /// where the sampled CLUT entry carries the STP bit.
    blend: bool,
    /// The record's background blend weight in 256ths. `None` is the M16
    /// per-packet half blend; see [`EntityMesh::blend_weight`].
    blend_weight: Option<u8>,
    /// Per corner, `PSXObject_Store`'s degenerate-UV nudge: bit 0 adds
    /// `0.003` to the normalised u, bit 1 to v.
    uv_nudge: [u8; 3],
}

/// A ground shadow's projected commands, ready for rasterization.
#[derive(Debug, Clone)]
pub(crate) struct ShadowPoly<'a> {
    /// Painter's key from [`triangle_depth_key`].
    pub(crate) key: u32,
    /// The commands' triangles in draw order.
    pub(crate) triangles: Vec<[crate::raster::GlVertex; 3]>,
    /// The baked coverage page this polygon samples.
    pub(crate) texture: &'a Image,
}

/// One mask sprite of a camera's page, ready for rasterization.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MaskQuad {
    /// Painter's key from [`mask::mask_sprite_key`].
    pub(crate) key: u32,
    /// Top-left texel of the sprite in the page bitmap.
    pub(crate) uv: (u32, u32),
    /// Top-left screen pixel.
    pub(crate) pos: (i32, i32),
    /// Sprite size in pixels; equals the sampled region, so sampling is 1:1.
    pub(crate) size: (u32, u32),
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
    /// `[top, height]` of the texture the original samples, in page rows: a
    /// weapon sheet is its own `256 x height` texture cut from the page at
    /// `top`, so its UVs normalise against that height; anything else samples
    /// the whole page.
    pub sheet: [u16; 2],
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
enum SceneItem<'a> {
    Shadow(ShadowPoly<'a>),
    Mask(MaskQuad),
    Triangle(Triangle),
    Effect(EffectQuad),
}

impl SceneItem<'_> {
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
/// shade, palette row 0, direct UVs). `light` is the entity's latched lights,
/// evaluated per packet normal. `cull` drops back-facing packets. `texture` is
/// the page index every emitted triangle samples. `tint` is the per-channel
/// RGB multiplier applied when the triangle is shaded.
///
/// The original decides the near-plane and backface tests once per *packet*,
/// so a quad never loses only one of its halves; the parser carries the quad's
/// four vertex indices in [`TmdPrim::quad`] so this can test all of them.
#[allow(clippy::too_many_arguments)]
fn collect_triangles(
    object: &TmdObject,
    joint: &anim::Mat4x3,
    camera: &Camera,
    lighting: Option<&Lighting>,
    light: Option<EntityLight>,
    cull: bool,
    texture: usize,
    tint: [u8; 3],
    colour: Option<[u8; 3]>,
    blend_weight: Option<u8>,
    room_object: bool,
    triangles: &mut Vec<Triangle>,
) {
    // A `JointSetColorTint` colour turns the reference's unlit path on: the
    // colour (1/128 scale) replaces the lighting outright. An all-zero colour
    // is untinted.
    let colour = colour.filter(|colour| *colour != [0; 3]);
    // The reference transforms and projects every vertex in floating point
    // through `camera * joint` (see [`RefProjection`]). The vertex buffer
    // holds `(x, -y, z)`; the raw-Y form skips the negation, and its pool is
    // built lazily so the common path pays nothing.
    let projection = RefProjection::new(camera, joint);
    let project_pool = |sign: f64| -> Vec<Option<([f32; 2], f32)>> {
        object
            .vertices
            .iter()
            .map(|vertex| {
                projection.project([
                    f64::from(vertex[0]),
                    sign * f64::from(vertex[1]),
                    f64::from(vertex[2]),
                ])
            })
            .collect()
    };
    let vertices = project_pool(-1.0);
    let mut raw_vertices: Option<Vec<Option<Projected>>> = None;
    // The reference lights the normal through the object's draw matrix and
    // the light rows through `g_RoomCameraData`.
    let light_camera = ref_camera(camera).0;
    let normal_matrix = projection.rotation.map(|row| row.map(|entry| entry as f32));

    for prim in &object.prims {
        let pool = if prim.raw_y {
            raw_vertices.get_or_insert_with(|| project_pool(1.0))
        } else {
            &vertices
        };
        let corner = |index: u16| pool.get(usize::from(index)).copied().flatten();
        // The whole packet is dropped when any corner is inside the near
        // plane, and culled on its first three corners in index order - for a
        // quad, its first fan triangle - so a quad never loses only one half.
        let packet = match prim.quad {
            Some(quad) => {
                if quad.iter().any(|&index| corner(index).is_none()) {
                    continue;
                }
                [quad[0], quad[1], quad[3]]
            }
            None => prim.vertices,
        };
        let (Some(first), Some(second), Some(third)) =
            (corner(packet[0]), corner(packet[1]), corner(packet[2]))
        else {
            continue;
        };
        let [x0, y0] = first.0.map(f64::from);
        let [x1, y1] = second.0.map(f64::from);
        let [x2, y2] = third.0.map(f64::from);
        if cull && !faces_camera((x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0)) {
            continue;
        }
        let (Some((position0, depth0)), Some((position1, depth1)), Some((position2, depth2))) = (
            corner(prim.vertices[0]),
            corner(prim.vertices[1]),
            corner(prim.vertices[2]),
        ) else {
            continue;
        };

        // The packet's CLUT word carries the ABE bit; a packet with it blends
        // and is drawn full-bright, so the room lights do not apply. The TIM
        // decoder keeps each palette entry's STP flag for the per-texel blend.
        // A record with a blend weight is the reference's transparent TMD:
        // every primitive blends at the record weight (none at weight 0) and
        // draws full-bright.
        let blend = match blend_weight {
            Some(weight) => weight != 0,
            None => prim.blend || prim.clut & ABE_BIT != 0,
        };
        let full_bright = blend_weight.is_some() || blend;
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

        // Corner normals. `tmd::parse` already pointed every missing corner
        // (a flat packet's corners 1 and 2, a zero-length normal) at its
        // kind's first normal, the one the original substitutes; a zero
        // normal that remains lights at the ambient level.
        let corner_normals = [
            object.normals.get(usize::from(prim.normals[0])).copied(),
            object.normals.get(usize::from(prim.normals[1])).copied(),
            object.normals.get(usize::from(prim.normals[2])).copied(),
        ];

        // An untextured packet's colour word is a per-corner vertex colour in
        // the original's 1/1024 scale; the sampled flat colour is white and the
        // shade carries the packet colour.
        let vertex_scale = prim
            .flat_color
            .map(|colour| colour.map(|channel| f64::from(channel) / 1024.0));

        let corners = [
            (position0, depth0, &prim.uv[0], corner_normals[0]),
            (position1, depth1, &prim.uv[1], corner_normals[1]),
            (position2, depth2, &prim.uv[2], corner_normals[2]),
        ];
        let raster: [RasterVertex; 3] = std::array::from_fn(|corner| {
            let (position, depth, uv, normal) = corners[corner];
            let shade = match (lighting, light) {
                _ if let Some(colour) = colour => colour
                    .map(|channel| (f64::from(channel) / 128.0 * CHANNEL_MAX).min(CHANNEL_MAX)),
                // The gameplay path evaluates each corner normal against the
                // latched light directions, clamps the accumulated light, then
                // folds in the vertex colour and the mesh tint.
                (Some(_), Some(light)) if !full_bright => {
                    let mut shade = match normal {
                        Some(normal) => reference_vertex_shade(
                            &light_camera,
                            &normal_matrix,
                            normal,
                            &light,
                            room_object.then_some(&joint.r),
                        ),
                        None => light.ambient,
                    };
                    for (channel, &scale) in shade.iter_mut().zip(&tint) {
                        *channel = channel.min(CHANNEL_MAX) * f64::from(scale) / CHANNEL_MAX;
                    }
                    if let Some(scale) = vertex_scale {
                        for (channel, &scale) in shade.iter_mut().zip(&scale) {
                            *channel *= scale;
                        }
                    }
                    shade
                }
                // A blended packet on a lit mesh and every door/item polygon
                // are full-bright: the mesh tint alone.
                (Some(_), _) => tint.map(f64::from),
                (None, _) => [CHANNEL_MAX; 3],
            };
            RasterVertex {
                position,
                w: depth,
                u: (f64::from(uv[0]) + page_x) as f32,
                v: (f64::from(uv[1]) + page_y) as f32,
                shade: shade.map(|channel| (channel / CHANNEL_MAX) as f32),
            }
        });

        // The sort key is the mean view Z, stored as a float.
        let depth = f64::from(
            ((f64::from(depth0) + f64::from(depth1) + f64::from(depth2)) * f64::from(1.0f32 / 3.0))
                as f32,
        );
        triangles.push(Triangle {
            raster,
            depth,
            key: triangle_depth_key(depth),
            texture,
            palette_row,
            // An untextured packet paints its per-corner colour through the
            // shade; the sampled colour is white.
            flat: prim.flat_color.map(|_| [255, 255, 255]),
            blend,
            blend_weight,
            uv_nudge: prim.uv_nudge,
        });
    }
}

/// Project one object the way the reference's complex pool draws it.
///
/// `ComplexTmdObjectSetup` turns every textured triangle packet of the object
/// into its own double-sided, unlit object (vertex colour 1.0), keyed by the
/// view Z of its vertex centroid (`ApplyMatrixSV`). `FUN_00486df0` then
/// selection-sorts the keys near to far and writes each object the raw GTE
/// matrix - no `FUN_00486190` shake tilt - with its view Z pushed back by its
/// sorted rank, so the `i`-th nearest triangle projects through `t[2] + i`.
fn collect_complex_pool(
    object: &TmdObject,
    joint: &anim::Mat4x3,
    camera: &Camera,
    texture: usize,
    triangles: &mut Vec<Triangle>,
) {
    let (rotation, _) = ref_composed(camera, joint);
    let rotation = rotation.map(|row| row.map(i32::from));
    let prims: Vec<&crate::model::TmdPrim> = object
        .prims
        .iter()
        .filter(|prim| prim.textured && prim.quad.is_none() && !prim.raw_y)
        .collect();
    let mut keys: Vec<i32> = prims
        .iter()
        .map(|prim| {
            let positions = prim.vertices.map(|index| {
                object
                    .vertices
                    .get(usize::from(index))
                    .copied()
                    .unwrap_or_default()
            });
            // `(short)(a + b + c) / 3` per axis.
            let centroid: [i32; 3] = std::array::from_fn(|axis| {
                let sum: i32 = positions.iter().map(|p| i32::from(p[axis])).sum();
                i32::from(sum as i16) / 3
            });
            apply_matrix_sv(&rotation, centroid)[2] as i16 as i32
        })
        .collect();
    let mut ids: Vec<usize> = (0..prims.len()).collect();
    // The original's selection sort, verbatim (it is not stable).
    for i in 0..prims.len().saturating_sub(1) {
        for j in i..prims.len() {
            if keys[j] < keys[i] {
                keys.swap(i, j);
                ids.swap(i, j);
            }
        }
    }
    for (rank, &id) in ids.iter().enumerate() {
        let prim = prims[id];
        let projection = RefProjection::new_complex(camera, joint, rank);
        let corner = |index: u16| {
            object.vertices.get(usize::from(index)).and_then(|vertex| {
                projection.project([
                    f64::from(vertex[0]),
                    -f64::from(vertex[1]),
                    f64::from(vertex[2]),
                ])
            })
        };
        let (Some(first), Some(second), Some(third)) = (
            corner(prim.vertices[0]),
            corner(prim.vertices[1]),
            corner(prim.vertices[2]),
        ) else {
            continue;
        };
        let [x0, y0] = first.0.map(f64::from);
        let [x1, y1] = second.0.map(f64::from);
        let [x2, y2] = third.0.map(f64::from);
        // Both windings are listed, so exactly one survives the backface
        // test unless the triangle is edge-on.
        let area = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
        if area == 0.0 || area.is_nan() {
            continue;
        }
        let order = if faces_camera(area) {
            [0, 1, 2]
        } else {
            [0, 2, 1]
        };
        let corners = [first, second, third];
        let page_x = f64::from(u32::from(prim.tsb & 0xF) * 128);
        let page_y = f64::from(u32::from((prim.tsb >> 4) & 1) * 256);
        let raster: [RasterVertex; 3] = order.map(|corner| {
            let (position, depth) = corners[corner];
            let uv = prim.uv[corner];
            RasterVertex {
                position,
                w: depth,
                u: (f64::from(uv[0]) + page_x) as f32,
                v: (f64::from(uv[1]) + page_y) as f32,
                shade: [1.0; 3],
            }
        });
        let depth = f64::from(
            ((f64::from(first.1) + f64::from(second.1) + f64::from(third.1))
                * f64::from(1.0f32 / 3.0)) as f32,
        );
        triangles.push(Triangle {
            raster,
            depth,
            key: triangle_depth_key(depth),
            texture,
            palette_row: usize::from(((prim.clut >> 6) & 0x01FF).saturating_sub(480)),
            flat: None,
            blend: false,
            blend_weight: None,
            uv_nudge: [0; 3],
        });
    }
}

/// The reference's `g_RoomCameraData` view matrix (4.12) from the camera's
/// from/look-at points, `MatrixToCamera` (0x0040a680). Every lighting stage
/// uses this matrix, and it differs slightly from
/// [`Camera::from_points_rolled`]'s angle-built view, so the port rebuilds it
/// here rather than reusing the projection matrix. Roll is omitted exactly
/// like the reference (every shipped RDT camera stores zero).
pub(crate) fn stored_camera_matrix(from: [i32; 3], to: [i32; 3]) -> [[i32; 3]; 3] {
    let dx = f64::from(to[0] - from[0]);
    let dy = f64::from(to[1] - from[1]);
    let dz = f64::from(to[2] - from[2]);
    let len = (dx * dx + dy * dy + dz * dz).sqrt().max(1.0);
    let h = (dx * dx + dz * dz).sqrt().max(1.0);
    [
        [(dz / h * 4096.0) as i32, 0, (-dx / h * 4096.0) as i32],
        [
            (dx * dy / (h * len) * 4096.0) as i32,
            (h / len * 4096.0) as i32,
            (dy * dz / (h * len) * 4096.0) as i32,
        ],
        [
            (dx / len * 4096.0) as i32,
            (-dy / len * 4096.0) as i32,
            (dz / len * 4096.0) as i32,
        ],
    ]
}

/// `ApplyMatrixLV`: rotate a long vector by a 4.12 matrix with the GTE's Y
/// negation on input and output and per-product rounding toward zero.
fn apply_matrix_lv(m: &[[i32; 3]; 3], v: [i32; 3]) -> [i32; 3] {
    let source = [i64::from(v[0]), -i64::from(v[1]), i64::from(v[2])];
    let row = |row: usize| -> i64 {
        (0..3)
            .map(|k| {
                let value = i64::from(m[row][k]) * source[k];
                (value + ((value >> 63) & 0xFFF)) >> FIXED_BITS
            })
            .sum()
    };
    [row(0) as i32, -row(1) as i32, row(2) as i32]
}

/// The reference's float projection of one object (`FlushTmdObjects`).
///
/// The object's matrix is `CompMatrix(g_RoomCameraData, joint)` pushed
/// through `SetRotAndTransMatrix` (translation Y negated), and every vertex
/// is transformed and projected in floating point without ever rounding to
/// whole pixels: `sx = cx + vx * (f / vz)`, `sy = cy - vy * (f / vz)`. The
/// x87 build keeps the intermediates in extended precision, which doubles
/// stand in for; only the stored screen position and view Z round to `f32`.
struct RefProjection {
    rotation: [[f64; 3]; 3],
    translation: [f64; 3],
    center: [f64; 2],
    focal: f64,
    near: f64,
}

impl RefProjection {
    fn new(camera: &Camera, joint: &anim::Mat4x3) -> Self {
        let (rotation, mut translation) = ref_composed(camera, joint);
        // `SetRotAndTransMatrix` negates the translation's Y.
        translation[1] = translation[1].wrapping_neg();
        let focal = f64::from(camera.fov);
        let mut rotation = rotation.map(|row| row.map(|entry| f32::from(entry) / 4096.0));
        let mut translation = translation.map(|entry| entry as f32);
        if let Some(tilt) = shake_tilt(camera) {
            // `FUN_0048c730` then `FUN_0048c820`: the tilt rotates the
            // object's rotation and translation alike.
            rotation = std::array::from_fn(|row| {
                std::array::from_fn(|column| {
                    rotation[0][column] * tilt[row][0]
                        + rotation[1][column] * tilt[row][1]
                        + rotation[2][column] * tilt[row][2]
                })
            });
            translation = std::array::from_fn(|row| {
                tilt[row][0] * translation[0]
                    + tilt[row][1] * translation[1]
                    + tilt[row][2] * translation[2]
            });
        }
        Self {
            rotation: rotation.map(|row| row.map(f64::from)),
            translation: translation.map(f64::from),
            center: [
                f64::from(CENTER_X + camera.screen[0]),
                f64::from(CENTER_Y + camera.screen[1]),
            ],
            focal,
            near: (2.0 * focal).max(1.0),
        }
    }

    /// The complex pool's projection ([`collect_complex_pool`]): the raw GTE
    /// matrix with no shake tilt and the view Z pushed back by `rank` (the
    /// original's `M[14] = (float)i + (float)t[2]`).
    fn new_complex(camera: &Camera, joint: &anim::Mat4x3, rank: usize) -> Self {
        let (rotation, mut translation) = ref_composed(camera, joint);
        translation[1] = translation[1].wrapping_neg();
        let focal = f64::from(camera.fov);
        let rotation = rotation.map(|row| row.map(|entry| f32::from(entry) / 4096.0));
        let translation = [
            translation[0] as f32,
            translation[1] as f32,
            rank as f32 + translation[2] as f32,
        ];
        Self {
            rotation: rotation.map(|row| row.map(f64::from)),
            translation: translation.map(f64::from),
            center: [
                f64::from(CENTER_X + camera.screen[0]),
                f64::from(CENTER_Y + camera.screen[1]),
            ],
            focal,
            near: (2.0 * focal).max(1.0),
        }
    }

    /// Project one vertex-buffer position (`x, -y, z`, or the raw Y form):
    /// the screen position and view Z, or `None` inside the near plane.
    fn project(&self, vertex: [f64; 3]) -> Option<([f32; 2], f32)> {
        let [vx, vy, vz] = std::array::from_fn(|row| {
            let m = self.rotation[row];
            m[0] * vertex[0] + m[1] * vertex[1] + m[2] * vertex[2] + self.translation[row]
        });
        if vz.is_nan() || vz < self.near {
            return None;
        }
        let iz = self.focal / vz;
        Some((
            [
                (self.center[0] + vx * iz) as f32,
                (self.center[1] - vy * iz) as f32,
            ],
            vz as f32,
        ))
    }
}

/// `FUN_00486190`'s view rotation while the screen is shaken: the look-at from
/// `(0, 0, -f)` towards `(-shake_x, shake_y, 0)` (`FUN_0048cc50`), which the
/// reference folds into every TMD object's matrix on top of the shifted
/// projection centre. At rest it is the identity and is skipped.
fn shake_tilt(camera: &Camera) -> Option<[[f32; 3]; 3]> {
    if camera.screen == [0, 0] {
        return None;
    }
    let dx = -camera.screen[0] as f32;
    let dy = camera.screen[1] as f32;
    let dz = camera.fov as f32;
    let mut length = (dx * dx + dy * dy + dz * dz).sqrt();
    if length == 0.0 {
        length = 1.0;
    }
    let inverse = 1.0 / length;
    let ny = -(dy * inverse);
    let horizontal = (1.0 - ny * ny).sqrt();
    let (nx, nz) = if horizontal == 0.0 {
        (0.0, 1.0)
    } else {
        (-((dx * inverse) / horizontal), (dz * inverse) / horizontal)
    };
    Some([
        [nz, 0.0, nx],
        [-(nx * ny), horizontal, nz * ny],
        [-(nx * horizontal), -ny, nz * horizontal],
    ])
}

/// `g_RoomCameraData`: the stored camera rotation and translation.
pub(crate) fn ref_camera(camera: &Camera) -> ([[i32; 3]; 3], [i32; 3]) {
    let mut view = stored_camera_matrix(camera.from, camera.look_at);
    let mut view_t = apply_matrix_lv(&view, camera.from.map(i32::wrapping_neg));
    if camera.mirrored {
        // `entity_draw_mirror_reflection` composes a negated-X identity
        // into the reflected camera, rotation and translation alike.
        view[0] = view[0].map(|entry| -entry);
        view_t[0] = -view_t[0];
    }
    (view, view_t)
}

/// `CompMatrix(g_RoomCameraData, joint)`: the camera composed onto `joint`,
/// as the 4.12 short rotation and the integer translation.
fn ref_composed(camera: &Camera, joint: &anim::Mat4x3) -> ([[i16; 3]; 3], [i32; 3]) {
    let (view, view_t) = ref_camera(camera);
    // `MulMatrix0` stores shorts.
    let rotation = mul_matrix0(&view, &joint.r).map(|row| row.map(|entry| entry as i16));
    let moved = apply_matrix_lv(&view, joint.t);
    let translation = std::array::from_fn(|axis| moved[axis].wrapping_add(view_t[axis]));
    (rotation, translation)
}

/// A projected corner: screen position and view Z.
type Projected = ([f32; 2], f32);

/// `TmdDepthNdc`: view Z on the linear `[0, 1]` depth-buffer ramp.
fn depth_ndc(view_z: f32) -> f32 {
    const NEAR: f32 = 1.0;
    const FAR: f32 = 131_072.0;
    ((view_z - NEAR) * (1.0 / (FAR - NEAR))).clamp(0.0, 1.0)
}

/// `MulMatrix0`: `m2 = m0 * m1` over 4.12 matrices with the reference's
/// per-product rounding.
pub(crate) fn mul_matrix0(a: &[[i32; 3]; 3], b: &[[i32; 3]; 3]) -> [[i32; 3]; 3] {
    std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            let mut sum = 0i64;
            for k in 0..3 {
                let value = i64::from(a[i][k]) * i64::from(b[k][j]);
                sum += (value + ((value >> 63) & 0xFFF)) >> FIXED_BITS;
            }
            sum as i32
        })
    })
}

/// `ApplyMatrixSV`: the PS1 GTE convention, Y negated on input and output,
/// 4.12 in and out with the reference's rounding.
pub(crate) fn apply_matrix_sv(m: &[[i32; 3]; 3], v: [i32; 3]) -> [i32; 3] {
    let source = [v[0], -v[1], v[2]];
    let mut out = [0i32; 3];
    for row in 0..3 {
        let mut sum = 0i64;
        for k in 0..3 {
            let value = i64::from(m[row][k]) * i64::from(source[k]);
            sum += (value + ((value >> 63) & 0xFFF)) >> FIXED_BITS;
        }
        out[row] = sum as i32;
    }
    [out[0], -out[1], out[2]]
}

/// The reference's `TmdComputeLight` (TmdRenderer.cpp) for one vertex.
///
/// `camera` is `g_RoomCameraData`; `matrix` is the object's float draw matrix
/// ([`RefProjection`]'s rotation: `camera * rot` as 4.12 shorts, turned by the
/// shake tilt when the screen is shaken); `normal` is the raw TMD normal. Room objects multiply
/// the stored light row by the object's rotation before the camera transform
/// (`RoomObjectRender`), the item viewer by the camera itself
/// (`MulMatrix0(&g_lightMatrix, &g_RoomCameraData)`), both through
/// `light_rot`; entities use the stored row as-is. Every path then
/// rotate the normal (Y-negated and scaled like the vertex buffer's
/// `PSXObjVtx`) by the draw matrix, negate the transformed light direction,
/// and accumulate `max(0, -(n_view . l)) * colour` over the ambient. The light
/// rows never see the shake tilt. The scale of the dot is 4.12 (the light
/// vectors) against unit (the normal).
pub(crate) fn reference_vertex_shade(
    camera: &[[i32; 3]; 3],
    matrix: &[[f32; 3]; 3],
    normal: [i16; 3],
    light: &EntityLight,
    light_rot: Option<&[[i32; 3]; 3]>,
) -> [f64; 3] {
    // The reference accumulates in 32-bit floats (`TmdComputeLight`), so the
    // port matches its rounding exactly rather than using doubles.
    let normal = [
        f32::from(normal[0]) / 4096.0,
        -f32::from(normal[1]) / 4096.0,
        f32::from(normal[2]) / 4096.0,
    ];
    let n_view: [f32; 3] = std::array::from_fn(|row| {
        matrix[row][0] * normal[0] + matrix[row][1] * normal[1] + matrix[row][2] * normal[2]
    });
    let mut shade = light.ambient.map(|value| value as f32);
    for source in &light.sources {
        let vec = match light_rot {
            Some(light_rot) => mul_matrix0(&[source.dir, [0; 3], [0; 3]], light_rot)[0],
            None => source.dir,
        };
        let l = apply_matrix_sv(camera, vec);
        let d =
            -(n_view[0] * l[0] as f32 + n_view[1] * l[1] as f32 + n_view[2] * l[2] as f32) / 4096.0;
        if d > 0.0 {
            for (channel, &value) in shade.iter_mut().zip(&source.color) {
                *channel += d * value as f32 * 255.0 / 128.0;
            }
        }
    }
    shade.map(f64::from)
}

/// Project and full-bright-shade one `.ivm` object into `triangles`.
///
/// Item prims carry their own vertex and normal pools like TMD objects but a
/// richer packet set: textured triangles and quads (quads fan-split into two
/// triangles), flat textured triangles, and untextured gouraud triangles which
/// interpolate their three per-corner packet colours. Every packet
/// backface-culls and a quad drops whole when any of its four corners is inside
/// the near plane, exactly like the TMD path. The viewer uses a single palette
/// row, so a textured prim samples row 0 and shades white.
fn collect_ivm_triangles(
    object: &crate::ivm::IvmObject,
    joint: &anim::Mat4x3,
    camera: &Camera,
    light: Option<&EntityLight>,
    triangles: &mut Vec<Triangle>,
) {
    let stored_camera = stored_camera_matrix(camera.from, camera.look_at);
    // The viewer's normal transform: `camera * rot` in floats, unrounded.
    let draw_matrix: [[f32; 3]; 3] = std::array::from_fn(|row| {
        std::array::from_fn(|column| {
            (0..3)
                .map(|k| stored_camera[row][k] as f32 * joint.r[k][column] as f32)
                .sum::<f32>()
                / (4096.0 * 4096.0)
        })
    });
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
        // A quad loses both of its halves when any corner is inside the near
        // plane; a triangle's own three projections make the same test below.
        let mut quad_corners = [0usize; 4];
        for (slot, corner) in quad_corners.iter_mut().enumerate() {
            *corner = usize::from(prim.vertices[slot]);
        }
        if prim.vertex_count == 4
            && quad_corners.iter().any(|&index| {
                vertices
                    .get(index)
                    .is_none_or(|vertex| camera.project(*vertex).is_none())
            })
        {
            continue;
        }
        // An untextured gouraud polygon rasterizes white so the interpolated
        // per-corner colours come out exactly; a textured polygon samples its
        // page and keeps the white shade.
        let flat = match prim.kind {
            crate::ivm::IvmPrimKind::Gouraud => Some([255, 255, 255]),
            _ => None,
        };
        let gouraud = prim.kind == crate::ivm::IvmPrimKind::Gouraud;
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

            let position0 = [f64::from(screen0[0]), f64::from(screen0[1])];
            let position1 = [f64::from(screen1[0]), f64::from(screen1[1])];
            let position2 = [f64::from(screen2[0]), f64::from(screen2[1])];
            let area = (position1[0] - position0[0]) * (position2[1] - position0[1])
                - (position2[0] - position0[0]) * (position1[1] - position0[1]);
            if !faces_camera(area) {
                continue;
            }

            let depth0 = f64::from(camera.view_position(*vertex0)[2]);
            let depth1 = f64::from(camera.view_position(*vertex1)[2]);
            let depth2 = f64::from(camera.view_position(*vertex2)[2]);

            let corner_data = [
                (position0, depth0, i0),
                (position1, depth1, i1),
                (position2, depth2, i2),
            ];
            let raster: [RasterVertex; 3] = std::array::from_fn(|corner| {
                let (position, depth, index) = corner_data[corner];
                // The packet colour the lit sum multiplies: an untextured
                // polygon's own colour, white for a textured one.
                let colour = if gouraud {
                    prim.colors[index.min(2)].map(f64::from)
                } else {
                    [CHANNEL_MAX; 3]
                };
                let shade = match light {
                    Some(light) => {
                        // A flat packet carries one normal for every corner.
                        let normal_slot = if prim.kind == crate::ivm::IvmPrimKind::TexturedFlat {
                            0
                        } else {
                            index
                        };
                        let normal = object
                            .normals
                            .get(usize::from(prim.normals[normal_slot]))
                            .copied();
                        let mut shade = match normal {
                            Some(normal) => reference_vertex_shade(
                                &stored_camera,
                                &draw_matrix,
                                normal,
                                light,
                                Some(&stored_camera),
                            ),
                            None => light.ambient,
                        };
                        for (channel, &scale) in shade.iter_mut().zip(&colour) {
                            *channel = channel.min(CHANNEL_MAX) * scale / CHANNEL_MAX;
                        }
                        shade
                    }
                    None => colour,
                };
                RasterVertex {
                    position: position.map(|axis| axis as f32),
                    w: depth as f32,
                    u: f32::from(prim.uv[index][0]),
                    v: f32::from(prim.uv[index][1]),
                    shade: shade.map(|channel| (channel / CHANNEL_MAX) as f32),
                }
            });

            let depth = (depth0 + depth1 + depth2) / 3.0;
            triangles.push(Triangle {
                raster,
                depth,
                key: triangle_depth_key(depth),
                texture: 0,
                palette_row: 0,
                flat,
                blend: prim.blend,
                blend_weight: None,
                uv_nudge: [0; 3],
            });
        }
    }
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

/// GL_NEAREST under CLAMP_TO_EDGE: the texel under a normalised coordinate.
fn clamp_texel(coordinate: f32, size: u32) -> usize {
    ((coordinate * size as f32).floor().max(0.0) as usize).min(size as usize - 1)
}

/// llvmpipe's float to unorm8 conversion: clamp, scale by 255 and round to
/// nearest even.
fn unorm8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round_ties_even() as u8
}

/// llvmpipe's AoS bilinear fetch of an RGBA8 clamp-to-edge texture.
///
/// Each coordinate becomes 8.8 fixed point centred on the texels,
/// `round(c * size * 256) - 128`; the integer part picks the two clamped
/// neighbours and the low byte weights them. The lerps run in 16-bit lanes
/// with llvmpipe's rounding (`lp_build_lerp` on normalised 8-bit values).
fn sample_bilinear(texture: &Image, u: f32, v: f32) -> [u8; 4] {
    let axis = |coordinate: f32, size: u32| {
        let scaled = (coordinate * (size as f32 * 256.0)).round_ties_even();
        let fixed = scaled.clamp(-(1 << 24) as f32, (1 << 24) as f32) as i32 - 128;
        let whole = fixed >> 8;
        let last = size as i32 - 1;
        (
            whole.clamp(0, last) as usize,
            (whole + 1).clamp(0, last) as usize,
            (fixed & 0xFF) as u16,
        )
    };
    let (x0, x1, fx) = axis(u, texture.width);
    let (y0, y1, fy) = axis(v, texture.height);
    let width = texture.width as usize;
    let fetch = |x: usize, y: usize, channel: usize| {
        texture
            .rgba
            .get((y * width + x) * 4 + channel)
            .copied()
            .unwrap_or(0)
    };
    std::array::from_fn(|channel| {
        let top = lerp_unorm8(fx, fetch(x0, y0, channel), fetch(x1, y0, channel));
        let bottom = lerp_unorm8(fx, fetch(x0, y1, channel), fetch(x1, y1, channel));
        lerp_unorm8(fy, top, bottom)
    })
}

/// `lp_build_lerp` for normalised 8-bit values in 16-bit lanes: the weight
/// times the wrapped difference, rounded by `0x7F` plus the product's bit 8,
/// shifted down and added back modulo 256.
fn lerp_unorm8(weight: u16, from: u8, to: u8) -> u8 {
    let delta = u16::from(to).wrapping_sub(u16::from(from));
    let mut product = weight.wrapping_mul(delta);
    product = product.wrapping_add(0x7F + ((product & 0x100) >> 8));
    u16::from(from).wrapping_add(product >> 8) as u8
}

/// llvmpipe's 8-bit normalised multiply, `round(a * b / 255)` (the
/// `lp_build_mul_norm` double-blend trick).
fn mul_unorm8(a: u8, b: u8) -> u32 {
    let product = u32::from(a) * u32::from(b) + 128;
    (product + (product >> 8)) >> 8
}

/// The reference's `GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA` blend as llvmpipe
/// runs it on an RGBA8 target: both terms in unorm8, added with saturation.
fn blend_unorm8(source: u8, destination: u8, alpha: u8) -> u8 {
    (mul_unorm8(source, alpha) + mul_unorm8(destination, 255 - alpha)).min(255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TmdObject, TmdPrim};

    #[test]
    fn draw_line_marks_its_endpoints_and_skips_offscreen_pixels() {
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_line([10, 10], [13, 10], [255, 0, 0]);
        for x in 10..=13 {
            let offset = (10 * framebuffer.width as usize + x) * 4;
            assert_eq!(&framebuffer.rgba[offset..offset + 4], &[255, 0, 0, 255]);
        }
        framebuffer.draw_line([-5, -5], [-1, -1], [0, 255, 0]);
    }

    fn identity() -> anim::Mat4x3 {
        anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        }
    }

    /// A latched light with a black ambient and no sources: the lit path's
    /// zero shade.
    fn dark_entity_light() -> EntityLight {
        EntityLight {
            ambient: [0.0; 3],
            sources: std::array::from_fn(|_| EntityLightSource {
                dir: [0; 3],
                color: [0; 3],
            }),
        }
    }

    fn straight_camera() -> Camera {
        Camera {
            view: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            trans: [0, 0, 0],
            fov: 200,
            roll: 0,
            screen: [0, 0],
            from: [0, 0, 0],
            look_at: [0, 0, 100],
            mirrored: false,
        }
    }

    fn solid_texture(pixel: [u8; 4]) -> Texture8 {
        // Index 0 is the colour key, so the fixture samples entry 1.
        Texture8 {
            width: 1,
            height: 1,
            indices: vec![1],
            palettes: vec![[0, 0, 0, 255], pixel],
            stp: Vec::new(),
        }
    }

    /// The standard screen-facing triangle, its plane at view-space Z `z`.
    fn mesh_at(z: i16, reversed: bool) -> Tmd {
        let mut vertices = vec![[0i16, 0, z], [0, -1000, z], [1000, 0, z]];
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
                    raw_y: false,
                    flat_color: None,
                    quad: None,
                    uv_nudge: [0; 3],
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
        // The default ambient 4095 maps to 254, so each channel truncates.
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
        assert_eq!(fnv1a(&framebuffer.rgba), 9_570_522_492_068_598_149);
    }

    #[test]
    fn project_rejects_points_behind_the_near_plane() {
        let camera = straight_camera();
        assert_eq!(camera.project([0, 0, 399]), None);
        assert_eq!(camera.project([0, 0, 400]), Some([160, 120]));
        assert_eq!(camera.project([0, 0, -10]), None);
    }

    #[test]
    fn project_truncates_toward_zero_for_both_signs() {
        // With fov 200 and vz 1000 the focal term is `vx / 5`, so the quotient
        // is hand-computable. `8 / 5 = 1.6` truncates to 1 (round-to-nearest
        // would give 2) and `-8 / 5 = -1.6` truncates to -1 (flooring would
        // give -2).
        let camera = straight_camera();
        assert_eq!(camera.project([8, -8, 1000]), Some([161, 121]));
        assert_eq!(camera.project([-8, 8, 1000]), Some([159, 119]));
        assert_eq!(camera.project([7, -7, 1000]), Some([161, 121]));
        // `-14 / 5 = -2.8` truncates to -2 (flooring would give -3).
        assert_eq!(camera.project([-14, 14, 1000]), Some([158, 118]));

        // A non-zero screen-shake offset is added after the truncation.
        let mut shaken = straight_camera();
        shaken.screen = [1, -1];
        assert_eq!(shaken.project([8, -8, 1000]), Some([162, 120]));
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

        // A diagonal cut: the fixed-point basis still maps the look-at to the
        // exact centre.
        let cut = Cut {
            pos: [0, 0, 0],
            look_at: [1000, 0, 1000],
            fov: 200,
            ..Cut::default()
        };
        let camera = Camera::from_cut(&cut);
        assert_eq!(camera.project([1000, 0, 1000]), Some([160, 120]));
    }

    #[test]
    fn a_nineteen_degree_roll_maps_the_screen_axes() {
        // 19 degrees in the game's 12-bit angle units.
        let camera = Camera::from_points_rolled([0, 0, 0], [0, 0, 1000], 200, 216);
        assert_eq!(camera.roll, 216);
        // The unrolled camera puts the world +X axis on the screen's positive
        // X axis (0 degrees from the centre) and the world -Y axis straight
        // up (-90 degrees). The roll swings both by +19 degrees.
        let angle = |point: [i32; 2]| {
            f64::from(point[1] - CENTER_Y)
                .atan2(f64::from(point[0] - CENTER_X))
                .to_degrees()
        };
        let right = camera.project([10_000, 0, 4000]).unwrap();
        let up = camera.project([0, -10_000, 4000]).unwrap();
        assert!(
            (angle(right) - 19.0).abs() < 1.0,
            "+X axis at {right:?} ({} deg)",
            angle(right)
        );
        assert!(
            (angle(up) + 71.0).abs() < 1.0,
            "-Y axis at {up:?} ({} deg)",
            angle(up)
        );

        // No roll: the same points keep their axes.
        let plain = Camera::from_points([0, 0, 0], [0, 0, 1000], 200);
        assert!(angle(plain.project([10_000, 0, 4000]).unwrap()).abs() < 1e-9);
        assert!((angle(plain.project([0, -10_000, 4000]).unwrap()) + 90.0).abs() < 1e-9);
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

    fn scene_triangle(key: u32) -> SceneItem<'static> {
        SceneItem::Triangle(Triangle {
            raster: std::array::from_fn(|_| RasterVertex {
                position: [0.0; 2],
                w: 1.0,
                u: 0.0,
                v: 0.0,
                shade: [0.0; 3],
            }),
            depth: f64::from(key),
            key,
            texture: 0,
            palette_row: 0,
            flat: None,
            blend: false,
            blend_weight: None,
            uv_nudge: [0; 3],
        })
    }

    /// Draw one triangle against a freshly cleared depth buffer.
    fn rasterize_alone(framebuffer: &mut Framebuffer, texture: &Texture8, triangle: &Triangle) {
        let mut depth = framebuffer.depth_buffer();
        let twin = texture.has_knockout_twin(triangle.palette_row);
        framebuffer.rasterize(texture, triangle, twin, &mut depth);
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
                tint: [255; 3],
                blend_weight: None,
                hidden_joints: 0,
                light_pos: None,
                per_joint_light: false,
                complex_object: None,
                aliased_light: false,
                joint_colours: NO_JOINT_COLOURS,
            },
            EntityMesh {
                mesh: &mesh1,
                texture: &green,
                joints: &joints,
                tint: [255; 3],
                blend_weight: None,
                hidden_joints: 0,
                light_pos: None,
                per_joint_light: false,
                complex_object: None,
                aliased_light: false,
                joint_colours: NO_JOINT_COLOURS,
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
            &[],
            &straight_camera(),
            &lighting,
            None,
        );
        assert_eq!(framebuffer_pixel(&tied, 180, 100), [0, 254, 0, 255]);

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
            &[],
            &straight_camera(),
            &lighting,
            Some(&layer),
        );
        assert_eq!(framebuffer_pixel(&masked, 180, 100), [9, 8, 7, 255]);
    }

    #[test]
    fn equal_depth_object_item_character_meshes_keep_the_submission_order() {
        // `engine::render_frame` submits the room objects, then the room items,
        // then the NPC/player meshes; the stable far-to-near sort keeps an
        // exact tie in that order, so a character paints over an item and an
        // item over an object.
        let red = solid_texture([255, 0, 0, 255]);
        let green = solid_texture([0, 255, 0, 255]);
        let blue = solid_texture([0, 0, 255, 255]);
        let mesh0 = mesh_at(1024, false);
        let joints = [identity()];
        fn entity<'a>(
            mesh: &'a Tmd,
            texture: &'a Texture8,
            joints: &'a [anim::Mat4x3],
        ) -> EntityMesh<'a> {
            EntityMesh {
                mesh,
                texture,
                joints,
                tint: [255; 3],
                blend_weight: None,
                hidden_joints: 0,
                light_pos: None,
                per_joint_light: false,
                complex_object: None,
                aliased_light: false,
                joint_colours: NO_JOINT_COLOURS,
            }
        }
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };

        // Object then item: the item (later) paints over the object.
        let mut object_item = Framebuffer::new();
        let meshes = [
            entity(&mesh0, &red, &joints),
            entity(&mesh0, &green, &joints),
        ];
        draw_gameplay_scene(
            &mut object_item,
            None,
            &meshes,
            &[],
            &straight_camera(),
            &lighting,
            None,
        );
        assert_eq!(framebuffer_pixel(&object_item, 180, 100), [0, 254, 0, 255]);

        // Object, item then character: the character wins the tie.
        let mut all = Framebuffer::new();
        let meshes = [
            entity(&mesh0, &red, &joints),
            entity(&mesh0, &green, &joints),
            entity(&mesh0, &blue, &joints),
        ];
        draw_gameplay_scene(
            &mut all,
            None,
            &meshes,
            &[],
            &straight_camera(),
            &lighting,
            None,
        );
        assert_eq!(framebuffer_pixel(&all, 180, 100), [0, 0, 254, 255]);
    }

    #[test]
    fn screen_shake_tilts_the_object_matrix() {
        let mut camera = straight_camera();
        camera.fov = 207;
        assert_eq!(shake_tilt(&camera), None);
        // Offset (161, 121) with f = 207, as `FUN_0048cc50` builds it.
        camera.screen = [1, 1];
        let tilt = shake_tilt(&camera).expect("a shaken screen tilts");
        let expected = [
            [0.999_988, 0.0, 0.004_830_86],
            [0.000_023_337, 0.999_988, -0.004_830_75],
            [-0.004_830_81, 0.004_830_81, 0.999_977],
        ];
        for (row, expected) in tilt.iter().zip(&expected) {
            for (&value, &expected) in row.iter().zip(expected) {
                assert!((value - expected).abs() < 1e-6, "{tilt:?}");
            }
        }
    }

    #[test]
    fn screen_shake_shifts_masks_but_not_shadows() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let shadow = shadow_at([0, 0, 1000], 500, 300, &texture);
        let mut plain = Vec::new();
        collect_shadow(&shadow, 0, &straight_camera(), &mut plain);
        let mut camera = straight_camera();
        camera.screen = [4, -3];
        let mut shaken = Vec::new();
        collect_shadow(&shadow, 0, &camera, &mut shaken);
        let [SceneItem::Shadow(plain)] = &plain[..] else {
            panic!("expected one plain shadow");
        };
        let [SceneItem::Shadow(shifted)] = &shaken[..] else {
            panic!("expected one shaken shadow");
        };
        for (plain, shifted) in shadow_corners(plain).iter().zip(&shadow_corners(shifted)) {
            assert_eq!((shifted.x, shifted.y), (plain.x, plain.y));
        }

        let cut = Cut {
            masks: vec![mask_sprite((10, 20), 100, 1)],
            mask_active: 1,
            ..Cut::default()
        };
        let page = solid_image(1, 1, [1, 2, 3, 255]);
        let layer = MaskLayer::new(room(), 0, &cut, &page);
        let mut items = Vec::new();
        collect_masks(&layer, [4, -3], &mut items);
        assert!(matches!(&items[..], [SceneItem::Mask(quad)] if quad.pos == (14, 17)));
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
        collect_masks(&layer, [0, 0], &mut items);
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
        collect_masks(&MaskLayer::new(room(), 0, &cut, &page), [0, 0], &mut items);
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
            [0, 0],
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
            [0, 0],
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
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
            light_pos: None,
            per_joint_light: false,
            complex_object: None,
            aliased_light: false,
            joint_colours: NO_JOINT_COLOURS,
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
            &[],
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
        collect_masks(&MaskLayer::new(room(), 0, &cut, &page), [0, 0], &mut items);
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
            &[],
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
            &[],
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
            stp: Vec::new(),
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
    fn a_page_draw_dispatches_to_the_replacement_format() {
        // The same sprite drawn as an indexed page and as a true-colour RGBA
        // replacement lands on the same pixels, and an RGBA page honours its
        // own alpha and the shared tint.
        let indexed = checkered_texture();
        let rgba = Artwork::Rgba(Image {
            width: 2,
            height: 2,
            rgba: vec![
                0, 0, 0, 0, //
                255, 0, 0, 255, //
                0, 255, 0, 255, //
                0, 0, 255, 255,
            ],
        });

        let mut from_indexed = Framebuffer::new();
        from_indexed.draw_page_sprite(
            &Artwork::Indexed(indexed),
            [0, 0, 2, 2],
            [0, 0, 4, 4],
            0,
            2,
            Tint::White,
        );
        let mut from_rgba = Framebuffer::new();
        from_rgba.draw_page_sprite(&rgba, [0, 0, 2, 2], [0, 0, 4, 4], 0, 2, Tint::White);
        assert_eq!(from_indexed.rgba, from_rgba.rgba);

        // Brightness and tint apply to an RGBA page exactly like an indexed
        // one: brightness 15 dims, and a green tint keeps only green.
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_page_sprite(&rgba, [0, 0, 2, 2], [0, 0, 2, 2], 0, 15, Tint::Green);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [0, 0, 0, 0]);
        assert_eq!(framebuffer_pixel(&framebuffer, 1, 0), [0, 0, 0, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 1), [0, 127, 0, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 1, 1), [0, 0, 0, 255]);
    }

    #[test]
    fn a_page_ramp_draw_modulates_an_rgba_replacement() {
        // The FILE reader's `2 * ramp` modulation and half-alpha blend.
        let page = Artwork::Rgba(Image {
            width: 1,
            height: 1,
            rgba: vec![148, 148, 148, 255],
        });
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_page_sprite_ramp(&page, [0, 0, 1, 1], [0, 0, 1, 1], 0, 0x80);
        // `2 * 0x80` is deliberately not clamped: 148 rounds to 149, then the
        // variant-2 half blend lands on 74, the value the reference shows.
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [74, 74, 74, 255]);
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
        assert_eq!(Tint::Rgb(1, 2, 3).rgb(), [1, 2, 3]);
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
            shift: [0, 0],
            lift: 0,
            tint: [0, 0, 1],
        }
    }

    /// The ring corners of a single-command shadow: the fan (0,1,2) + (0,2,3).
    fn shadow_corners(poly: &ShadowPoly) -> Vec<crate::raster::GlVertex> {
        assert_eq!(poly.triangles.len(), 2);
        let [first, second] = [poly.triangles[0], poly.triangles[1]];
        assert_eq!(first[0], second[0]);
        assert_eq!(first[2], second[1]);
        vec![first[0], first[1], first[2], second[2]]
    }

    #[test]
    fn shadow_projection_keeps_the_quad_edge_order_and_uvs() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let shadow = shadow_at([0, 0, 1000], 500, 300, &texture);
        let mut items = Vec::new();
        collect_shadow(&shadow, 0, &straight_camera(), &mut items);

        let [SceneItem::Shadow(poly)] = &items[..] else {
            panic!("expected one shadow item, got {}", items.len());
        };
        assert_eq!(poly.key, 1000);
        // Edge order 0->1->3->2, so the corners come out
        // (-x,+z) (+x,+z) (+x,-z) (-x,-z) with v running towards local -Z.
        // The near edge is wider than the far edge and every corner sits on
        // the horizon line of the unpitched camera, at truncated integer
        // screen positions.
        let corners = shadow_corners(poly);
        let expected = [
            (84.0, 0.0, 0.0),
            (236.0, 1.0, 0.0),
            (302.0, 1.0, 1.0),
            (18.0, 0.0, 1.0),
        ];
        for (corner, (x, u, v)) in corners.iter().zip(expected) {
            assert_eq!((corner.x, corner.y, corner.u, corner.v), (x, 120.0, u, v));
            assert_eq!(corner.color, [0.0, 0.0, 1.0 / 255.0, 1.0]);
        }
    }

    #[test]
    fn shadow_projection_applies_the_placement_lift() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let shadow = shadow_at([0, 0, 1000], 500, 700, &texture);
        let mut unlifted = Vec::new();
        collect_shadow(&shadow, 0, &straight_camera(), &mut unlifted);
        let mut lifted = Vec::new();
        collect_shadow(
            &Shadow {
                lift: 100,
                ..shadow
            },
            0,
            &straight_camera(),
            &mut lifted,
        );
        let (Some(SceneItem::Shadow(base)), Some(SceneItem::Shadow(raised))) =
            (unlifted.first(), lifted.first())
        else {
            panic!("expected shadow items");
        };
        // A positive placement lift drops the quad down the screen by
        // lift * f / z: 100 * 200 / 1699 at the far corner (the yaw matrix's
        // 4095 diagonal truncates 700 to 699).
        let (base, raised) = (shadow_corners(base)[0], shadow_corners(raised)[0]);
        assert_eq!(base.w, 1699.0);
        assert_eq!(raised.y - base.y, 11.0);
    }

    #[test]
    fn shadow_clips_against_the_near_plane() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let shadow = shadow_at([0, 0, 0], 500, 700, &texture);
        let mut items = Vec::new();
        collect_shadow(&shadow, 0, &straight_camera(), &mut items);

        let [SceneItem::Shadow(poly)] = &items[..] else {
            panic!("expected one shadow item, got {}", items.len());
        };
        // Corners 2 and 3 are behind the near plane (view z 128); each
        // clipped edge contributes one vertex on the plane, at screen
        // x = 160 +/- 499 * 200 / 128 and v = (699 - 128) / 1398, all
        // truncated.
        let corners = shadow_corners(poly);
        assert_eq!([corners[2].x, corners[2].w], [939.0, 128.0]);
        assert_eq!([corners[3].x, corners[3].w], [-619.0, 128.0]);
        assert_eq!(corners[2].v, 1672.0 / 4096.0);
    }

    #[test]
    fn shadow_wholly_behind_the_camera_is_skipped() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let mut items = Vec::new();
        collect_shadow(
            &shadow_at([0, 0, -5000], 500, 700, &texture),
            0,
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
            0,
            &straight_camera(),
            &mut items,
        );
        assert!(items.is_empty());
    }

    /// A shadow polygon covering screen (10,10)..(20,20), sampling `texture`.
    fn shadow_poly(texture: &Image) -> ShadowPoly<'_> {
        let corner = |x: f32, y: f32, u: f32, v: f32| crate::raster::GlVertex {
            x,
            y,
            depth: depth_ndc(1000.0),
            w: 1000.0,
            u,
            v,
            color: [0.0, 0.0, 1.0 / 255.0, 1.0],
        };
        let quad = [
            corner(10.0, 10.0, 0.0, 0.0),
            corner(20.0, 10.0, 1.0, 0.0),
            corner(20.0, 20.0, 1.0, 1.0),
            corner(10.0, 20.0, 0.0, 1.0),
        ];
        ShadowPoly {
            key: 1000,
            triangles: vec![[quad[0], quad[1], quad[2]], [quad[0], quad[2], quad[3]]],
            texture,
        }
    }

    fn draw_shadow(framebuffer: &mut Framebuffer, poly: &ShadowPoly) {
        let depth = framebuffer.depth_buffer();
        framebuffer.rasterize_shadow(poly, &depth);
    }

    #[test]
    fn shadow_darkens_towards_the_tint_by_the_texel_alpha() {
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        let poly = shadow_poly(&texture);
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
        draw_shadow(&mut framebuffer, &poly);
        // 200 * 127 / 255 rounds to 100; the tint's 1/255 blue adds one.
        assert_eq!(
            framebuffer_pixel(&framebuffer, 15, 15),
            [100, 100, 101, 255]
        );
        assert_eq!(framebuffer_pixel(&framebuffer, 5, 5), [200, 200, 200, 255]);

        // A zero-alpha texel is a hole and leaves the destination untouched.
        let texture = solid_image(1, 1, [255, 255, 255, 0]);
        let poly = shadow_poly(&texture);
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
        draw_shadow(&mut framebuffer, &poly);
        assert_eq!(
            framebuffer_pixel(&framebuffer, 15, 15),
            [200, 200, 200, 255]
        );
    }

    #[test]
    fn shadow_seam_blends_once_for_both_windings() {
        // The fan's shared diagonal passes through (15, 15); the reversed
        // winding must not double-blend the seam either.
        let texture = solid_image(1, 1, [255, 255, 255, 128]);
        for reversed in [false, true] {
            let mut poly = shadow_poly(&texture);
            if reversed {
                for triangle in &mut poly.triangles {
                    triangle.reverse();
                }
            }
            let mut framebuffer = Framebuffer::new();
            framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
            draw_shadow(&mut framebuffer, &poly);
            assert_eq!(
                framebuffer_pixel(&framebuffer, 15, 15),
                [100, 100, 101, 255],
                "reversed {reversed}"
            );
        }
    }

    #[test]
    fn shadow_samples_the_page_bilinearly_with_clamped_edges() {
        // A 2x1 page: the left texel is transparent, the right one darkens.
        // Near either edge both bilinear taps clamp onto the same texel; the
        // middle column weights the two halves.
        let mut texture = solid_image(2, 1, [255, 255, 255, 128]);
        texture.rgba[3] = 0;
        let poly = shadow_poly(&texture);
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
        draw_shadow(&mut framebuffer, &poly);
        assert_eq!(
            framebuffer_pixel(&framebuffer, 11, 15),
            [200, 200, 200, 255]
        );
        assert_eq!(
            framebuffer_pixel(&framebuffer, 18, 15),
            [100, 100, 101, 255]
        );
        let middle = framebuffer_pixel(&framebuffer, 15, 15);
        assert!(middle[0] > 100 && middle[0] < 200, "{middle:?}");
    }

    #[test]
    fn bilinear_lerp_matches_llvmpipe_rounding() {
        assert_eq!(lerp_unorm8(0, 10, 200), 10);
        assert_eq!(lerp_unorm8(128, 0, 255), 128);
        assert_eq!(lerp_unorm8(128, 255, 0), 127);
        assert_eq!(lerp_unorm8(255, 0, 255), 254);
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
        collect_shadow(&shadow, 0, &straight_camera(), &mut items);
        collect_masks(&layer, [0, 0], &mut items);
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
            &[shadow],
            &camera,
            &lighting,
            None,
        );
        let mut without = Framebuffer::new();
        draw_gameplay_scene(
            &mut without,
            Some(&background),
            &[],
            &[],
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
        assert_eq!(pixel, [100, 100, 101, 255]);
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
            sheet: [0, 256],
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
            [0, 0],
            [255; 3],
            &[],
            &[],
            &camera,
            &lighting,
            None,
            Some(&layer),
            0,
            None,
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
        framebuffer.draw_scene(&[], Some(&mask_page), &effect_pages, &items);
        assert_eq!(framebuffer_pixel(&framebuffer, 10, 10), [255, 0, 0, 255]);

        // The same effect against a real entity triangle at the same key.
        let texture = solid_texture([0, 0, 255, 255]);
        let mesh = mesh_at(1000, false);
        let joints = [identity()];
        let meshes = [EntityMesh {
            mesh: &mesh,
            texture: &texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
            light_pos: None,
            per_joint_light: false,
            complex_object: None,
            aliased_light: false,
            joint_colours: NO_JOINT_COLOURS,
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
            [0, 0],
            [255; 3],
            &meshes,
            &[],
            &camera,
            &lighting,
            None,
            Some(&layer),
            0,
            None,
        );
        assert_eq!(framebuffer_pixel(&framebuffer, 160, 130), [255, 0, 0, 255]);
    }

    #[test]
    fn mirror_point_visible_checks_the_sides_and_the_extent() {
        // Plane Z = 100, camera at z = 200. A point on the camera's side
        // (z > 100) reflects; one behind the plane does not.
        let camera = [0, 0, 200];
        // crossing x = x_point * (camera_plane - plane) / ((point-camera planes))
        // = 15000 * 100 / 300 = 5000.
        assert_eq!(
            mirror_point_visible(false, 100, 4000, 6300, camera, [15000, 0, 300]),
            Some(5000)
        );
        assert!(mirror_point_visible(false, 100, 4000, 6300, camera, [15000, 0, 50]).is_none());
        // Crossings outside the span are culled, and the bounds are exclusive.
        assert!(mirror_point_visible(false, 100, 4000, 6300, camera, [3000, 0, 300]).is_none());
        assert!(mirror_point_visible(false, 100, 4000, 6300, camera, [18900, 0, 300]).is_none());
        assert_eq!(
            mirror_point_visible(false, 100, 4000, 6300, camera, [12000, 0, 300]),
            Some(4000)
        );

        // Plane X = 12500 (axis 1): the plane coordinate is x, the cross
        // coordinate z. The camera at x = 13000; crossings scale by 500/2000.
        let camera = [13000, 0, 0];
        assert_eq!(
            mirror_point_visible(true, 12500, 4000, 6300, camera, [14000, 0, 24000]),
            Some(6000)
        );
        assert!(mirror_point_visible(true, 12500, 4000, 6300, camera, [12000, 0, 24000]).is_none());
        assert!(mirror_point_visible(true, 12500, 4000, 6300, camera, [14000, 0, 4000]).is_none());
    }

    #[test]
    fn a_mirrored_camera_reflects_the_eye_and_negates_the_x_scale() {
        let cut = Cut {
            pos: [0, 0, 1000],
            look_at: [0, 0, 0],
            fov: 200,
            ..Cut::default()
        };
        let camera = Camera::from_cut(&cut);
        let mirrored = camera.mirrored(false, 0);
        // The eye and the look-at are folded about the plane.
        assert_eq!(mirrored.from, [0, 0, -1000]);
        assert_eq!(mirrored.look_at, [0, 0, 0]);
        // The rebuilt reflection already flips the right and forward axes; the
        // X-scale negation restores the right row and flips forward back, so
        // the reflection projects with the live camera's orientation.
        assert_eq!(mirrored.view[0], camera.view[0]);
        assert_eq!(mirrored.view[1], camera.view[1]);
        assert_eq!(mirrored.view[2], camera.view[2].map(|entry| -entry));

        // Axis X folds the X pair.
        let mirrored = camera.mirrored(true, 500);
        assert_eq!(mirrored.from, [1000, 0, 1000]);
        assert_eq!(mirrored.look_at, [1000, 0, 0]);
    }

    #[test]
    fn hidden_joints_never_draw_and_never_mirror() {
        let texture = solid_texture([10, 20, 30, 255]);
        let mesh = mesh_at(1000, false);
        let joints = [identity()];
        let camera = straight_camera();
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };

        // The triangle's only joint is hidden: the scene list is empty.
        let hidden = [EntityMesh {
            mesh: &mesh,
            texture: &texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0b1,
            light_pos: None,
            per_joint_light: false,
            complex_object: None,
            aliased_light: false,
            joint_colours: NO_JOINT_COLOURS,
        }];
        let mut framebuffer = Framebuffer::new();
        draw_gameplay_scene(
            &mut framebuffer,
            None,
            &hidden,
            &[],
            &camera,
            &lighting,
            None,
        );
        assert!(
            framebuffer
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[..3] == [0, 0, 0]),
            "a hidden joint drew"
        );

        // The same mesh with the gate clear paints.
        let visible = [EntityMesh {
            mesh: &mesh,
            texture: &texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
            light_pos: None,
            per_joint_light: false,
            complex_object: None,
            aliased_light: false,
            joint_colours: NO_JOINT_COLOURS,
        }];
        let mut framebuffer = Framebuffer::new();
        draw_gameplay_scene(
            &mut framebuffer,
            None,
            &visible,
            &[],
            &camera,
            &lighting,
            None,
        );
        assert_ne!(framebuffer_pixel(&framebuffer, 200, 100), [0, 0, 0, 255]);
    }

    #[test]
    fn the_mirror_pass_culls_a_joint_behind_the_camera_and_outside_the_extent() {
        let camera = [0, 0, 200];
        let pass = MirrorPass {
            axis_x: false,
            plane: 100,
            extent_min: 0,
            extent_max: 7000,
            camera_pos: camera,
            camera: straight_camera().mirrored(false, 100),
        };
        // Same side of the plane and inside the extent.
        assert!(pass.joint_visible([5000, 0, 150]));
        // Behind the plane: no reflection.
        assert!(!pass.joint_visible([5000, 0, 50]));
        // Outside the mirror's span (the crossing lands past extent_max).
        assert!(!pass.joint_visible([12000, 0, 150]));
    }

    #[test]
    fn the_mirror_pass_never_reflects_meshes_before_the_partition() {
        let texture = solid_texture([255, 0, 0, 255]);
        let mesh = mesh_at(1000, false);
        let joints = [identity()];
        let camera = straight_camera();
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };
        let pass = MirrorPass {
            axis_x: false,
            plane: 0,
            extent_min: 0,
            extent_max: 7000,
            camera_pos: [0, 0, 200],
            camera: straight_camera().mirrored(false, 0),
        };
        let meshes = [EntityMesh {
            mesh: &mesh,
            texture: &texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
            light_pos: None,
            per_joint_light: false,
            complex_object: None,
            aliased_light: false,
            joint_colours: NO_JOINT_COLOURS,
        }];

        let render = |mirror_from: usize, mirror: Option<&MirrorPass>| {
            let mut framebuffer = Framebuffer::new();
            draw_gameplay_scene_with_effects(
                &mut framebuffer,
                None,
                [0, 0],
                [255; 3],
                &meshes,
                &[],
                &camera,
                &lighting,
                None,
                None,
                mirror_from,
                mirror,
            );
            framebuffer.rgba
        };
        let primary = render(0, None);
        // The mesh sits before the partition, so the only candidate is skipped
        // and the frame matches the mirror-less one exactly; the real-asset
        // ROOM112 test proves the same pass does draw an eligible entity.
        assert_eq!(primary, render(1, Some(&pass)));
    }

    #[test]
    fn the_mesh_tint_multiplies_the_shaded_triangle() {
        // The standard white-shaded triangle with a half tint paints grey.
        let texture = solid_texture([255, 255, 255, 255]);
        let mesh = mesh_at(1000, false);
        let joints = [identity()];
        let camera = straight_camera();
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };
        let tinted = [EntityMesh {
            mesh: &mesh,
            texture: &texture,
            joints: &joints,
            tint: [128, 128, 128],
            blend_weight: None,
            hidden_joints: 0,
            light_pos: None,
            per_joint_light: false,
            complex_object: None,
            aliased_light: false,
            joint_colours: NO_JOINT_COLOURS,
        }];
        let mut framebuffer = Framebuffer::new();
        draw_gameplay_scene(
            &mut framebuffer,
            None,
            &tinted,
            &[],
            &camera,
            &lighting,
            None,
        );
        // (200, 40) lies inside mesh_at's screen triangle.
        let pixel = framebuffer_pixel(&framebuffer, 200, 40);
        assert!(pixel[0] > 100 && pixel[0] < 160, "{pixel:?}");
    }

    #[test]
    fn a_joint_colour_draws_the_joint_unlit_in_that_colour() {
        // Zero light would paint black; the 0x30 red joint colour (0.375)
        // replaces the lighting outright.
        let texture = solid_texture([255, 255, 255, 255]);
        let mesh = mesh_at(1000, false);
        let joints = [identity()];
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [Light::default(); 3],
        };
        let mut joint_colours = NO_JOINT_COLOURS;
        joint_colours[0] = [0x30, 0, 0];
        let tinted = [EntityMesh {
            mesh: &mesh,
            texture: &texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
            light_pos: None,
            per_joint_light: false,
            complex_object: None,
            aliased_light: false,
            joint_colours,
        }];
        let mut framebuffer = Framebuffer::new();
        draw_gameplay_scene(
            &mut framebuffer,
            None,
            &tinted,
            &[],
            &straight_camera(),
            &lighting,
            None,
        );
        let pixel = framebuffer_pixel(&framebuffer, 200, 40);
        assert!((94..=97).contains(&pixel[0]), "{pixel:?}");
        assert_eq!(&pixel[1..3], &[0, 0], "{pixel:?}");
    }

    #[test]
    fn the_abe_clut_bit_blends_and_forces_the_packet_unlit() {
        // A packet whose CLUT word carries bit 15 is semi-transparent and is
        // drawn full-bright: a zero ambient cannot darken it.
        let mut abe = mesh_at(1000, false);
        abe.objects[0].prims[0].clut = 0x8000;
        let plain = mesh_at(1000, false);
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [Light::default(); 3],
        };
        let mut triangles = Vec::new();
        collect_triangles(
            &abe.objects[0],
            &identity(),
            &straight_camera(),
            Some(&lighting),
            Some(dark_entity_light()),
            true,
            0,
            [255; 3],
            None,
            None,
            false,
            &mut triangles,
        );
        assert_eq!(triangles.len(), 1);
        assert!(triangles[0].blend, "the ABE bit must set the blend flag");
        assert_eq!(
            triangles[0].raster[0].shade, [1.0; 3],
            "a blended packet is full-bright"
        );

        // The same mesh without the bit is shaded black by the zero ambient.
        let mut triangles = Vec::new();
        collect_triangles(
            &plain.objects[0],
            &identity(),
            &straight_camera(),
            Some(&lighting),
            Some(dark_entity_light()),
            true,
            0,
            [255; 3],
            None,
            None,
            false,
            &mut triangles,
        );
        assert!(!triangles[0].blend);
        assert_eq!(triangles[0].raster[0].shade, [0.0; 3]);
    }

    #[test]
    fn the_abe_path_blends_half_over_the_destination() {
        let mut mesh = mesh_at(1000, false);
        mesh.objects[0].prims[0].clut = 0x8000;
        let mut texture = solid_texture([255, 255, 255, 255]);
        texture.stp = vec![false, true];
        let joints = [identity()];
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [Light::default(); 3],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [100, 100, 100, 255]));
        framebuffer.draw_model(&mesh, &texture, &joints, &straight_camera(), &lighting);
        // llvmpipe's unorm8 blend at alpha 128: 128 + 50 = 178.
        assert_eq!(
            framebuffer_pixel(&framebuffer, 200, 100),
            [178, 178, 178, 255]
        );
        // Untouched grey outside it.
        assert_eq!(
            framebuffer_pixel(&framebuffer, 10, 220),
            [100, 100, 100, 255]
        );
    }

    #[test]
    fn per_texel_stp_selects_which_texels_blend() {
        // A three-texel page: entry 1 is opaque, entry 2 carries the STP bit.
        // Opaque texels outnumber STP ones, so the page gets a knock-out twin.
        let mut texture = Texture8 {
            width: 3,
            height: 1,
            indices: vec![1, 1, 2],
            palettes: vec![[0u8; 4]; 2 * crate::model::PALETTE_ROW_LEN],
            stp: vec![false; 2 * crate::model::PALETTE_ROW_LEN],
        };
        texture.palettes[1] = [255, 255, 255, 255];
        texture.palettes[2] = [255, 255, 255, 255];
        texture.stp[2] = true;
        let raster_vertex = |position: [f32; 2], u: f32, v: f32| RasterVertex {
            position,
            w: 1000.0,
            u,
            v,
            shade: [1.0; 3],
        };
        let triangle = Triangle {
            raster: [
                raster_vertex([10.0, 10.0], 0.0, 0.0),
                raster_vertex([20.0, 10.0], 3.0, 0.0),
                raster_vertex([10.0, 20.0], 0.0, 0.0),
            ],
            depth: 1000.0,
            key: 1000,
            texture: 0,
            palette_row: 0,
            flat: None,
            blend: true,
            blend_weight: None,
            uv_nudge: [0; 3],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [100, 100, 100, 255]));
        rasterize_alone(&mut framebuffer, &texture, &triangle);
        // The twin paints the opaque texel back; the STP texel stays blended.
        assert_eq!(
            framebuffer_pixel(&framebuffer, 11, 11),
            [255, 255, 255, 255]
        );
        assert_eq!(
            framebuffer_pixel(&framebuffer, 17, 11),
            [178, 178, 178, 255]
        );

        // With no STP texels there is no twin, so every texel blends.
        texture.stp = vec![false; 2 * crate::model::PALETTE_ROW_LEN];
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [100, 100, 100, 255]));
        rasterize_alone(&mut framebuffer, &texture, &triangle);
        assert_eq!(
            framebuffer_pixel(&framebuffer, 11, 11),
            [178, 178, 178, 255]
        );

        // Nor when STP texels are the majority.
        texture.stp[1] = true;
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [100, 100, 100, 255]));
        rasterize_alone(&mut framebuffer, &texture, &triangle);
        assert_eq!(
            framebuffer_pixel(&framebuffer, 17, 11),
            [178, 178, 178, 255]
        );
    }

    #[test]
    fn record_blend_weight_remaps_the_abr_table() {
        let with_prim = |blend: bool, tsb: u16| Tmd {
            objects: vec![TmdObject {
                vertices: vec![[0; 3]; 3],
                normals: vec![[0, 0, 4096]],
                prims: vec![TmdPrim {
                    vertices: [0, 1, 2],
                    normals: [0, 0, 0],
                    uv: [[0, 0]; 3],
                    clut: 0,
                    tsb,
                    textured: true,
                    blend,
                    raw_y: false,
                    flat_color: None,
                    quad: None,
                    uv_nudge: [0; 3],
                }],
            }],
        };
        // No semi-transparent primitive means no record weight.
        assert_eq!(record_blend_weight(&with_prim(false, 0), None), None);
        // The ABR field is texpage bits 5-6. Its raw table value only carries
        // the zero/non-zero distinction: ABR 1 is opaque, 0, 2 and 3 all
        // collapse to the half blend.
        assert_eq!(
            record_blend_weight(&with_prim(true, 0 << 5), None),
            Some(0x80)
        );
        assert_eq!(
            record_blend_weight(&with_prim(true, 1 << 5), None),
            Some(0x00)
        );
        assert_eq!(
            record_blend_weight(&with_prim(true, 2 << 5), None),
            Some(0x80)
        );
        assert_eq!(
            record_blend_weight(&with_prim(true, 3 << 5), None),
            Some(0x80)
        );
        // The armed `cmd_omodel_set` override wins over the ABR and becomes
        // the 0.2 background weight.
        assert_eq!(
            record_blend_weight(&with_prim(true, 0 << 5), Some(0x30)),
            Some(0x33)
        );
        assert_eq!(
            record_blend_weight(&with_prim(true, 3 << 5), Some(0x30)),
            Some(0x33)
        );
    }

    #[test]
    fn a_record_blend_weight_scales_the_per_texel_mix() {
        let mut texture = solid_texture([255, 255, 255, 255]);
        texture.stp = vec![false, true];
        let raster_vertex = |position: [f32; 2], u: f32, v: f32| RasterVertex {
            position,
            w: 1000.0,
            u,
            v,
            shade: [1.0; 3],
        };
        let triangle = |blend_weight| Triangle {
            raster: [
                raster_vertex([10.0, 10.0], 0.0, 0.0),
                raster_vertex([20.0, 10.0], 2.0, 0.0),
                raster_vertex([10.0, 20.0], 0.0, 0.0),
            ],
            depth: 1000.0,
            key: 1000,
            texture: 0,
            palette_row: 0,
            flat: None,
            blend: true,
            blend_weight,
            uv_nudge: [0; 3],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [100, 100, 100, 255]));

        // The 0x33 override weight (0.2 background): (255 * 205 + 100 * 51) /
        // 256 = 224.
        rasterize_alone(&mut framebuffer, &texture, &triangle(Some(0x33)));
        assert_eq!(
            framebuffer_pixel(&framebuffer, 11, 11),
            [224, 224, 224, 255]
        );

        // The 0x80 half blend in unorm8: alpha 128 gives 128 + 50 = 178.
        framebuffer.blit(&solid_image(320, 240, [100, 100, 100, 255]));
        rasterize_alone(&mut framebuffer, &texture, &triangle(Some(0x80)));
        assert_eq!(
            framebuffer_pixel(&framebuffer, 11, 11),
            [178, 178, 178, 255]
        );

        // A zero background weight is opaque, the ABR 1 `B + F` rule.
        framebuffer.blit(&solid_image(320, 240, [100, 100, 100, 255]));
        rasterize_alone(&mut framebuffer, &texture, &triangle(Some(0)));
        assert_eq!(
            framebuffer_pixel(&framebuffer, 11, 11),
            [255, 255, 255, 255]
        );
    }

    #[test]
    fn a_back_facing_door_packet_is_culled() {
        let mesh = mesh_at(1000, true);
        let texture = solid_texture([10, 20, 30, 255]);
        let joints = [identity()];
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_model_unlit(&mesh, &texture, &joints, &straight_camera());
        assert!(
            framebuffer.rgba.iter().all(|&byte| byte == 0),
            "a door packet facing away must not paint"
        );

        // The same packet the other way round paints.
        let mut framebuffer = Framebuffer::new();
        let front = mesh_at(1000, false);
        framebuffer.draw_model_unlit(&front, &texture, &joints, &straight_camera());
        assert_ne!(framebuffer_pixel(&framebuffer, 200, 100), [0, 0, 0, 0]);
    }

    #[test]
    fn a_quad_drops_whole_when_one_corner_is_inside_the_near_plane() {
        // Ring (0,1,3,2); only the (0,3,2) half is supplied. With corner 1 in
        // front the half paints; with corner 1 behind the near plane the whole
        // quad must drop instead of leaving the half.
        let quad = |behind: bool| Tmd {
            objects: vec![TmdObject {
                vertices: vec![
                    [0, 0, 1000],
                    [1000, -1000, if behind { 0 } else { 1000 }],
                    [1000, 0, 1000],
                    [0, -1000, 1000],
                ],
                normals: vec![[0, 0, 4096]],
                prims: vec![TmdPrim {
                    vertices: [0, 3, 2],
                    normals: [0, 0, 0],
                    uv: [[0, 0]; 3],
                    clut: 0x7800,
                    tsb: 0x80,
                    textured: true,
                    blend: false,
                    raw_y: false,
                    flat_color: None,
                    quad: Some([0, 1, 3, 2]),
                    uv_nudge: [0; 3],
                }],
            }],
        };
        let texture = solid_texture([10, 20, 30, 255]);
        let joints = [identity()];
        let lighting = Lighting {
            ambient: [4095; 3],
            lights: [Light::default(); 3],
        };

        let mut visible = Framebuffer::new();
        visible.draw_model(
            &quad(false),
            &texture,
            &joints,
            &straight_camera(),
            &lighting,
        );
        assert_ne!(framebuffer_pixel(&visible, 200, 100), [0, 0, 0, 0]);

        let mut dropped = Framebuffer::new();
        dropped.draw_model(
            &quad(true),
            &texture,
            &joints,
            &straight_camera(),
            &lighting,
        );
        assert!(
            dropped.rgba.iter().all(|&byte| byte == 0),
            "half a quad survived a clipped corner"
        );
    }

    #[test]
    fn an_ivm_quad_drops_whole_and_gouraud_interpolates_corner_colours() {
        use crate::ivm::{Ivm, IvmObject, IvmPrim, IvmPrimKind};
        let texture = solid_texture([255, 255, 255, 255]);
        let camera = straight_camera();

        // A quad fan's corner 1 is behind the near plane: both halves drop.
        let mut quad = Ivm {
            texture: texture.clone(),
            objects: vec![IvmObject {
                vertices: vec![
                    [1000, -1000, 1000],
                    [0, 0, 0],
                    [-1000, -1000, 1000],
                    [0, 1000, 1000],
                ],
                normals: vec![[0, 0, 4096]],
                prims: vec![IvmPrim {
                    kind: IvmPrimKind::TexturedGouraud,
                    vertex_count: 4,
                    vertices: [0, 1, 2, 3],
                    normals: [0; 4],
                    uv: [[0, 0]; 4],
                    colors: [[255; 3]; 3],
                    clut: 0,
                    tsb: 0,
                    blend: false,
                }],
            }],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_ivm_unlit(&quad, &[identity()], &camera);
        assert!(
            framebuffer.rgba.iter().all(|&byte| byte == 0),
            "an IVM quad survived a clipped corner"
        );

        // With every corner in front the quad paints.
        quad.objects[0].vertices[1] = [0, 0, 1000];
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_ivm_unlit(&quad, &[identity()], &camera);
        assert!(
            framebuffer
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] != 0),
            "an unclipped IVM quad did not paint"
        );

        // An untextured gouraud triangle with three distinct corner colours
        // interpolates: each corner is dominated by its own channel and the
        // centre is a near-equal mix.
        let gouraud = Ivm {
            texture: solid_texture([0, 0, 0, 255]),
            objects: vec![IvmObject {
                vertices: vec![[-300, -200, 1000], [0, 200, 1000], [300, -200, 1000]],
                normals: vec![[0, 0, 4096]],
                prims: vec![IvmPrim {
                    kind: IvmPrimKind::Gouraud,
                    vertex_count: 3,
                    vertices: [0, 1, 2, 0],
                    normals: [0; 4],
                    uv: [[0, 0]; 4],
                    colors: [[255, 0, 0], [0, 0, 255], [0, 255, 0]],
                    clut: 0,
                    tsb: 0,
                    blend: false,
                }],
            }],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_ivm_unlit(&gouraud, &[identity()], &camera);
        let red = framebuffer_pixel(&framebuffer, 110, 155);
        let green = framebuffer_pixel(&framebuffer, 210, 155);
        let blue = framebuffer_pixel(&framebuffer, 160, 95);
        assert!(
            red[0] > 200 && red[1] < 60 && red[2] < 60,
            "red corner {red:?}"
        );
        assert!(
            green[1] > 200 && green[0] < 60 && green[2] < 60,
            "green corner {green:?}"
        );
        assert!(
            blue[2] > 200 && blue[0] < 60 && blue[1] < 60,
            "blue corner {blue:?}"
        );
        let centre = framebuffer_pixel(&framebuffer, 160, 140);
        for channel in centre[..3].iter() {
            assert!(*channel > 40 && *channel < 200, "centre {centre:?}");
        }
    }

    #[test]
    fn the_item_view_and_door_captures_stay_deterministic() {
        let texture = solid_texture([10, 20, 30, 255]);
        let joints = [identity()];
        let mut first = Framebuffer::new();
        first.draw_model_unlit(&mesh_at(1000, false), &texture, &joints, &straight_camera());
        let mut second = Framebuffer::new();
        second.draw_model_unlit(&mesh_at(1000, false), &texture, &joints, &straight_camera());
        assert_eq!(first.rgba, second.rgba, "door captures differ");

        let ivm = crate::ivm::Ivm {
            texture: solid_texture([1, 2, 3, 255]),
            objects: vec![crate::ivm::IvmObject {
                vertices: vec![[-1000, 1000, 1000], [1000, 1000, 1000], [0, -1000, 1000]],
                normals: vec![[0, 0, 4096]],
                prims: vec![crate::ivm::IvmPrim {
                    kind: crate::ivm::IvmPrimKind::Gouraud,
                    vertex_count: 3,
                    vertices: [0, 1, 2, 0],
                    normals: [0; 4],
                    uv: [[0, 0]; 4],
                    colors: [[10, 0, 0], [0, 10, 0], [0, 0, 10]],
                    clut: 0,
                    tsb: 0,
                    blend: false,
                }],
            }],
        };
        let mut first = Framebuffer::new();
        first.draw_ivm_unlit(&ivm, &[identity()], &straight_camera());
        let mut second = Framebuffer::new();
        second.draw_ivm_unlit(&ivm, &[identity()], &straight_camera());
        assert_eq!(first.rgba, second.rgba, "item-view captures differ");
    }

    #[test]
    fn latched_lighting_uses_xz_falloff_the_80_cap_and_truncated_ambient() {
        let ambient_only = Lighting {
            ambient: [4095, 1775, 15],
            lights: [Light::default(); 3],
        };
        // The 12-bit ambient converts with the original's `value * 255 / 4096`.
        let light = ambient_only.latched([0, 0, 0]);
        assert_eq!(light.ambient, [254.0, 110.0, 0.0]);
        for source in light.sources {
            assert_eq!(source.color, [0; 3]);
        }

        // A point light at the entity's own X/Z (a huge Y offset does not
        // count for falloff) reaches full attenuation; its 255 colour caps at
        // 0x80 before the 1/128 scale, so the latched colour is 255.
        let point = Lighting {
            ambient: [0; 3],
            lights: [
                Light {
                    pos: [0, 100_000, 0],
                    color: [255, 255, 255],
                    kind: 0,
                    radius: 1000,
                },
                Light::default(),
                Light::default(),
            ],
        };
        let light = point.latched([0, 0, 0]);
        assert_eq!(light.sources[0].color, [128, 128, 128]);
        // The stored direction is `F * normalize(entity - light)` in 4.12: the
        // entity sits at the light's X/Z, so only the Y term survives and the
        // negation flips it.
        assert_eq!(light.sources[0].dir, [0, 4096, 0]);

        // Half the radius out on X/Z truncates each channel, and the direction
        // is normalized over all three axes.
        let point = Lighting {
            ambient: [0; 3],
            lights: [
                Light {
                    pos: [0, 0, 0],
                    color: [201, 101, 3],
                    kind: 0,
                    radius: 1000,
                },
                Light::default(),
                Light::default(),
            ],
        };
        let light = point.latched([300, 0, 400]);
        assert_eq!(
            light.sources[0].color,
            [100, 50, 1],
            "attenuated bytes before the 0x80 cap"
        );
        // normalize(entity - light) = (0.6, 0, 0.8), Y negated (zero here),
        // truncated to 4.12.
        assert_eq!(light.sources[0].dir, [2457, 0, 3276]);

        // A light at or beyond the radius contributes nothing, and a zero
        // radius is black.
        assert_eq!(point.latched([0, 0, 1000]).sources[0].color, [0; 3]);
        let zero = Lighting {
            ambient: [0; 3],
            lights: [
                Light {
                    pos: [0; 3],
                    color: [255, 0, 0],
                    kind: 0,
                    radius: 0,
                },
                Light::default(),
                Light::default(),
            ],
        };
        let light = zero.latched([0, 0, 0]);
        assert_eq!(light.sources[0].color, [0; 3]);
        assert_eq!(light.sources[0].dir, [0, 0, 0]);

        // The radius is unsigned: 0xFFFF is the widest falloff, not -1, so a
        // room whose lights store it stays lit at any in-room distance.
        let wide = Lighting {
            ambient: [0; 3],
            lights: [
                Light {
                    pos: [0; 3],
                    color: [255, 255, 255],
                    kind: 0,
                    radius: 0xFFFF,
                },
                Light::default(),
                Light::default(),
            ],
        };
        let light = wide.latched([0, 0, 1000]);
        assert_eq!(light.sources[0].color, [128, 128, 128]);

        // A directional light's position is its direction vector: it is
        // normalized, capped at 0x80 and reversed (the stored direction points
        // towards the light).
        let directional = Lighting {
            ambient: [16, 16, 16],
            lights: [
                Light {
                    pos: [1, 0, 0],
                    color: [200, 200, 200],
                    kind: 1,
                    radius: 0,
                },
                Light::default(),
                Light::default(),
            ],
        };
        let light = directional.latched([0, 0, 0]);
        assert_eq!(light.sources[0].color, [128, 128, 128]);
        // `F * normalize((1,0,0))` in 4.12.
        assert_eq!(light.sources[0].dir, [4096, 0, 0]);
        assert!((light.ambient[0] - 0.0).abs() < 1e-9);
    }

    #[test]
    fn untextured_gouraud_folds_the_packet_colour_into_the_shade() {
        // An untextured packet's colour word is a vertex colour in the
        // original's 1/1024 units; the sampled flat colour is white, so a full
        // ambient and an 0xFF colour give 255 * 255 / 1024 = 63.
        let mut mesh = mesh_at(1000, false);
        mesh.objects[0].prims[0].textured = false;
        mesh.objects[0].prims[0].flat_color = Some([255, 255, 255]);
        let texture = solid_texture([200, 200, 200, 255]);
        let joints = [identity()];
        let lighting = Lighting {
            ambient: [4096; 3],
            lights: [Light::default(); 3],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_model(&mesh, &texture, &joints, &straight_camera(), &lighting);
        assert_eq!(framebuffer_pixel(&framebuffer, 200, 100), [64, 64, 64, 255]);
    }

    #[test]
    fn a_lit_scene_matches_hand_computed_pixels() {
        // The texture is 200 grey. A directional light [-1] points its
        // direction along +Z, so the triangle's +Z normal takes the full
        // diffuse; its 64 colour scales to 64 / 128 = 0.5 by the original's
        // byte conversion, and 200 * 127.5 / 255 = 100.
        let mesh = mesh_at(1000, false);
        let texture = solid_texture([200, 200, 200, 255]);
        let joints = [identity()];
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [
                Light {
                    pos: [0, 0, -1],
                    color: [64, 64, 64],
                    kind: 1,
                    radius: 0,
                },
                Light::default(),
                Light::default(),
            ],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_model(&mesh, &texture, &joints, &straight_camera(), &lighting);
        assert_eq!(
            framebuffer_pixel(&framebuffer, 200, 100),
            [100, 100, 100, 255]
        );

        // A point light on the normal's side with half the falloff radius:
        // the 500-unit X/Z distance halves the colour to 32 (63.75 after the
        // 1/128 scale), so the pixel is 200 * 63.75 / 255 = 50.
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [
                Light {
                    pos: [0, 0, 500],
                    color: [64, 64, 64],
                    kind: 0,
                    radius: 1000,
                },
                Light::default(),
                Light::default(),
            ],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_model(&mesh, &texture, &joints, &straight_camera(), &lighting);
        assert_eq!(framebuffer_pixel(&framebuffer, 200, 100), [50, 50, 50, 255]);

        // The same light on the other side of the surface leaves the +Z normal
        // unlit: the per-vertex normal is what selects diffuse.
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [
                Light {
                    pos: [0, 0, 1],
                    color: [64, 64, 64],
                    kind: 1,
                    radius: 0,
                },
                Light::default(),
                Light::default(),
            ],
        };
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_model(&mesh, &texture, &joints, &straight_camera(), &lighting);
        assert_eq!(framebuffer_pixel(&framebuffer, 200, 100), [0, 0, 0, 255]);
    }

    #[test]
    fn background_sampling_follows_the_origin_and_colour() {
        let mut image = Image {
            width: 320,
            height: 240,
            rgba: Vec::with_capacity(320 * 240 * 4),
        };
        for y in 0..240u32 {
            for x in 0..320u32 {
                image
                    .rgba
                    .extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 7, 255]);
            }
        }
        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_background(&image, [1, 2], [255, 255, 255]);
        // Origin +1 samples one pixel further left; +1 in Y samples one row
        // up. The trailing edge clamps to the image's own edge.
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [0, 0, 7, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 5, 5), [4, 3, 7, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 319, 239), [62, 237, 7, 255]);

        let mut framebuffer = Framebuffer::new();
        framebuffer.draw_background(&image, [0, 0], [128, 255, 0]);
        assert_eq!(
            framebuffer_pixel(&framebuffer, 4, 3),
            [(4u32 * 128 / 255) as u8, 3, 0, 255]
        );
    }

    #[test]
    fn sprite_blend_and_mirror_flags_change_the_pixels() {
        let texture = indexed_texture(
            vec![1, 2],
            &[[0, 0, 0, 0], [200, 40, 40, 255], [40, 200, 40, 255]],
        );
        // A 2x1 source drawn into a 2x1 destination.
        assert_eq!(texture.width, 2);
        assert_eq!(texture.height, 2);

        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [100, 100, 100, 255]));
        let mut sprites = vec![
            SpriteDraw::indexed(&texture, [0, 0, 2, 1], [0, 0, 2, 1], 0, 2, Tint::White)
                .with_mirror([true, false]),
        ];
        framebuffer.draw_sprites(&mut sprites);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [40, 200, 40, 255]);
        assert_eq!(framebuffer_pixel(&framebuffer, 1, 0), [200, 40, 40, 255]);

        // A half blend mixes with the grey destination.
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [100, 100, 100, 255]));
        let mut sprites = vec![
            SpriteDraw::indexed(&texture, [0, 0, 1, 1], [0, 0, 1, 1], 0, 2, Tint::White)
                .with_blend(SpriteBlend::Half),
        ];
        framebuffer.draw_sprites(&mut sprites);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [150, 70, 70, 255]);

        // A black-alpha blend mixes the destination towards zero.
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
        let mut sprites = vec![
            SpriteDraw::indexed(&texture, [0, 0, 1, 1], [0, 0, 1, 1], 0, 2, Tint::White)
                .with_blend(SpriteBlend::BlackAlpha(128)),
        ];
        framebuffer.draw_sprites(&mut sprites);
        assert_eq!(framebuffer_pixel(&framebuffer, 0, 0), [99, 99, 99, 255]);
    }

    #[test]
    fn fade_to_color_blends_towards_white_or_black() {
        use crate::transition::FadeColor;

        let mut frame = Framebuffer::new();
        frame.fill_rect([0, 0, 2, 1], [100, 50, 200, 255]);

        // Alpha 0 leaves the frame untouched.
        frame.fade_to_color(FadeColor::White, 0);
        assert_eq!(&frame.rgba[..4], &[100, 50, 200, 255]);

        // A mid white blends each channel towards 255.
        frame.fade_to_color(FadeColor::White, 128);
        assert_eq!(&frame.rgba[..4], &[177, 152, 227, 255]);

        // Full alpha paints the overlay colour and preserves alpha.
        frame.fade_to_color(FadeColor::White, 255);
        assert_eq!(&frame.rgba[..4], &[255, 255, 255, 255]);

        frame.fade_to_color(FadeColor::Black, 128);
        assert_eq!(&frame.rgba[..4], &[127, 127, 127, 255]);
        frame.fade_to_color(FadeColor::Black, 255);
        assert_eq!(&frame.rgba[..4], &[0, 0, 0, 255]);

        // The overlay helper maps `None` to no draw.
        let mut clear = Framebuffer::new();
        clear.fill_rect([0, 0, 1, 1], [1, 2, 3, 255]);
        clear.fade_overlay(None);
        assert_eq!(&clear.rgba[..4], &[1, 2, 3, 255]);
        clear.fade_overlay(Some((FadeColor::White, 255)));
        assert_eq!(&clear.rgba[..4], &[255, 255, 255, 255]);
    }
}
