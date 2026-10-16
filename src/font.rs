//! The shared text font: metrics, glyph decoding and tinted drawing.
//!
//! One encoded byte stream is decoded left to right into [`Glyph`]s. Plain
//! bytes address the glyph grid directly, `0xF8`/`0xF9`/`0xFA` select the
//! extended glyph pages with a second byte, and a few control bytes move or
//! stop the cursor. [`Font::draw_text`] samples the sheet through the
//! original's CLUT-driven [`Tint`] table and the pending 2D sprite path.

use crate::model::Texture8;
use crate::render::{Framebuffer, SpriteBlend, SpriteDraw};

pub use crate::render::Tint;

/// Glyph height in pixels for both sheet widths.
pub const GLYPH_HEIGHT: i32 = 14;
/// Width at which a sheet selects the 14px Japanese metrics.
pub const JPN_SHEET_WIDTH: u32 = 768;

/// A font sheet's glyph metrics, selected by the sheet width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontMetrics {
    /// Glyph advance: 14 for the 768-wide Japanese sheet, 8 for the 256-wide
    /// USA sheet.
    pub glyph_w: i32,
    /// Left margin of a message box.
    pub left_margin: i32,
    /// Room-message y.
    pub text_y: i32,
    /// Menu and item-name/description y.
    pub menu_y: i32,
}

impl FontMetrics {
    /// Select the metrics from the sheet width: 768-wide selects 14px glyphs
    /// with margin `0x22`; anything narrower uses the USA 8px metrics with
    /// margin `0x30`. Both share the room-message y `181` and menu y `186`.
    pub fn from_sheet_width(width: u32) -> Self {
        if width >= JPN_SHEET_WIDTH {
            Self {
                glyph_w: 14,
                left_margin: 0x22,
                text_y: 181,
                menu_y: 186,
            }
        } else {
            Self {
                glyph_w: 8,
                left_margin: 0x30,
                text_y: 181,
                menu_y: 186,
            }
        }
    }
}

/// One decoded step of an encoded text stream.
///
/// Cursor-only steps (spaces, `0xFF` half-advances) and no-ops have a zero
/// size; [`Glyph::visible`] distinguishes them from drawable glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyph {
    /// Top-left texel of the glyph in the sheet.
    pub u: i32,
    pub v: i32,
    /// Drawable size; zero for a cursor-only step.
    pub width: i32,
    pub height: i32,
    /// Horizontal cursor advance after the step.
    pub advance: i32,
    /// Whether this is the reserved missing-glyph cell; the font draws these
    /// from its synthetic box rather than from the sheet.
    pub missing: bool,
}

impl Glyph {
    /// A cursor-only step that draws nothing.
    fn cursor(advance: i32) -> Self {
        Self {
            u: 0,
            v: 0,
            width: 0,
            height: 0,
            advance,
            missing: false,
        }
    }

    /// Whether this step draws a glyph.
    pub fn visible(self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// Iterator over the glyphs of one encoded byte stream.
///
/// `0x00` advances one cell, `0xFF` half a cell, `0xFB` is a no-op, and
/// `0x01`/`0x07` end the stream. A truncated `0xF8`/`0xF9`/`0xFA` sequence
/// also ends it without a glyph.
pub struct Glyphs<'a> {
    bytes: &'a [u8],
    position: usize,
    metrics: FontMetrics,
    remap_parentheses: bool,
}

impl<'a> Glyphs<'a> {
    /// Decode `bytes` with `metrics`, remapping `0x28`/`0x29` to the
    /// controller-symbol cells like the original's game-text renderer.
    pub fn new(bytes: &'a [u8], metrics: FontMetrics) -> Self {
        Self {
            bytes,
            position: 0,
            metrics,
            remap_parentheses: true,
        }
    }

    /// Decode `bytes` with `metrics` without the controller-symbol remap, so
    /// `0x28`/`0x29` are the ordinary letters L and M. The save screen's
    /// header uses the original's plain formatted path, which has no remap.
    pub fn plain(bytes: &'a [u8], metrics: FontMetrics) -> Self {
        Self {
            bytes,
            position: 0,
            metrics,
            remap_parentheses: false,
        }
    }

    fn glyph(&self, u: i32, v: i32) -> Glyph {
        Glyph {
            u,
            v,
            width: self.metrics.glyph_w,
            height: GLYPH_HEIGHT,
            advance: self.metrics.glyph_w,
            missing: false,
        }
    }

    /// The reserved missing-glyph cell: the synthetic box carries no sheet
    /// coordinate, so it samples from `u = v = 0` of the box texture.
    fn missing_glyph(&self) -> Glyph {
        Glyph {
            missing: true,
            ..self.glyph(0, 0)
        }
    }
}

impl Iterator for Glyphs<'_> {
    type Item = Glyph;

    fn next(&mut self) -> Option<Glyph> {
        loop {
            let byte = *self.bytes.get(self.position)?;
            match byte {
                0x01 | 0x07 => return None,
                0x00 => {
                    self.position += 1;
                    return Some(Glyph::cursor(self.metrics.glyph_w));
                }
                0xFB => self.position += 1,
                0xFF => {
                    self.position += 1;
                    return Some(Glyph::cursor(self.metrics.glyph_w / 2));
                }
                0xF8..=0xFA => {
                    let parameter = i32::from(*self.bytes.get(self.position + 1)?);
                    self.position += 2;
                    // The reserved missing-glyph pair draws the synthetic
                    // box, not a sheet cell; see `charset::MISSING`.
                    if [byte, parameter as u8] == crate::charset::MISSING {
                        return Some(self.missing_glyph());
                    }
                    let col = parameter % 18;
                    let row = parameter / 18;
                    let (page, v) = match byte {
                        0xF8 => (0, (row + 15) * GLYPH_HEIGHT),
                        0xF9 => (256, row * GLYPH_HEIGHT),
                        _ => (256, (row + 14) * GLYPH_HEIGHT),
                    };
                    return Some(self.glyph(page + col * self.metrics.glyph_w, v));
                }
                _ => {
                    self.position += 1;
                    // ASCII '(' and ')' are the controller-symbol cells in
                    // both sheet widths.
                    if self.remap_parentheses {
                        if byte == 0x28 {
                            return Some(self.glyph(56, 224));
                        }
                        if byte == 0x29 {
                            return Some(self.glyph(70, 224));
                        }
                    }
                    let byte = i32::from(byte);
                    let col = byte % 18;
                    let row = byte / 18;
                    return Some(self.glyph(col * self.metrics.glyph_w, 28 + row * GLYPH_HEIGHT));
                }
            }
        }
    }
}

/// A loaded font sheet with the metrics its width selects.
#[derive(Debug, Clone)]
pub struct Font {
    pub texture: Texture8,
    pub metrics: FontMetrics,
    /// The synthetic hollow box the reserved missing-glyph cell draws; see
    /// [`crate::charset::MISSING`]. Kept out of `texture` so the decoded sheet
    /// is never mutated.
    missing: Texture8,
}

impl Font {
    /// Pair `texture` with the metrics its width selects.
    ///
    /// The decoded sheet is left byte-identical; the missing-glyph placeholder
    /// is a separate synthetic texture, so no sheet cell is ever clobbered.
    pub fn new(texture: Texture8) -> Self {
        let metrics = FontMetrics::from_sheet_width(texture.width);
        let missing = missing_box_texture(&texture, metrics.glyph_w);
        Self {
            texture,
            metrics,
            missing,
        }
    }

    /// Draw an encoded stream left to right from `(x, y)`.
    ///
    /// Every step advances the cursor by its own advance; glyphs sample
    /// palette row 0 (the sheet's grey ramp) mixed by `tint` and scaled by
    /// `brightness`.
    pub fn draw_text(
        &self,
        framebuffer: &mut Framebuffer,
        x: i32,
        y: i32,
        tint: Tint,
        brightness: u8,
        bytes: &[u8],
    ) {
        self.draw_glyphs(framebuffer, x, y, tint, brightness, bytes, true);
    }

    /// Draw an encoded stream without the controller-symbol remap; see
    /// [`Glyphs::plain`].
    pub fn draw_text_plain(
        &self,
        framebuffer: &mut Framebuffer,
        x: i32,
        y: i32,
        tint: Tint,
        brightness: u8,
        bytes: &[u8],
    ) {
        self.draw_glyphs(framebuffer, x, y, tint, brightness, bytes, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_glyphs(
        &self,
        framebuffer: &mut Framebuffer,
        x: i32,
        y: i32,
        tint: Tint,
        brightness: u8,
        bytes: &[u8],
        remap_parentheses: bool,
    ) {
        let glyphs = if remap_parentheses {
            Glyphs::new(bytes, self.metrics)
        } else {
            Glyphs::plain(bytes, self.metrics)
        };
        // Every glyph queues its shadow pass first and its text pass second,
        // both at the pending-sprite depth the brightness selects; the stable
        // order keeps the shadow under the glyph. The shadow is one pixel down
        // and right, a semi-transparent black silhouette.
        let alpha = shadow_alpha(brightness);
        let mut sprites: Vec<SpriteDraw<'_>> = Vec::new();
        let mut cursor = x;
        for glyph in glyphs {
            if glyph.visible() {
                let src = [glyph.u, glyph.v, glyph.width, glyph.height];
                let texture = if glyph.missing {
                    &self.missing
                } else {
                    &self.texture
                };
                if alpha > 0 {
                    sprites.push(
                        SpriteDraw::indexed(
                            texture,
                            src,
                            [cursor + 1, y + 1, glyph.width, glyph.height],
                            0,
                            brightness,
                            Tint::White,
                        )
                        .with_blend(SpriteBlend::BlackAlpha(alpha)),
                    );
                }
                sprites.push(SpriteDraw::indexed(
                    texture,
                    src,
                    [cursor, y, glyph.width, glyph.height],
                    0,
                    brightness,
                    tint,
                ));
            }
            cursor += glyph.advance;
        }
        framebuffer.draw_sprites(&mut sprites);
    }
}

/// Build the synthetic hollow box the reserved missing-glyph cell draws.
///
/// It is one glyph cell wide and [`GLYPH_HEIGHT`] tall; the strokes use the
/// palette index the shipped glyph pixels do, and the palettes are cloned from
/// the sheet, so the box takes the same tint and brightness as the text around
/// it. The sheet itself is untouched.
fn missing_box_texture(texture: &Texture8, glyph_w: i32) -> Texture8 {
    /// The palette index the shipped glyph strokes use (a dark grey that the
    /// tint pass maps to the text colour).
    const INK: u8 = 8;
    let width = glyph_w.max(1) as u32;
    let height = GLYPH_HEIGHT as u32;
    let mut indices = vec![0u8; (width * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let edge = y == 0 || y == height - 1 || x == 0 || x == width - 1;
            indices[(y * width + x) as usize] = if edge { INK } else { 0 };
        }
    }
    Texture8 {
        width,
        height,
        indices,
        palettes: texture.palettes.clone(),
        stp: texture.stp.clone(),
    }
}

/// The text shadow's blend alpha: `brightness * 255 / 30`, capped at 255.
/// Unlike the text pass, the shadow pass has no 0/2 collapse.
pub fn shadow_alpha(brightness: u8) -> u8 {
    (u32::from(brightness) * 255 / 30).min(255) as u8
}

/// Split an encoded stream into rows of at most `max_cells` glyph cells,
/// breaking at the last space of a row when there is one.
///
/// The terminators end the stream; a `0xF8`/`0xF9`/`0xFA` pair counts as one
/// cell, `0xFB` as none.
pub fn wrap_cells(bytes: &[u8], max_cells: usize) -> Vec<&[u8]> {
    let max_cells = max_cells.max(1);
    let mut rows = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let mut index = 0;
        let mut cells = 0;
        let mut last_space = None;
        while index < rest.len() {
            let byte = rest[index];
            if byte == 0x01 || byte == 0x07 {
                // The terminator closes the stream; the rest is the last row.
                index = rest.len();
                break;
            }
            let step = match byte {
                0xF8..=0xFA => 2,
                _ => 1,
            };
            let width = usize::from(byte != 0xFB);
            if cells + width > max_cells {
                break;
            }
            if byte == 0x00 {
                last_space = Some(index);
            }
            cells += width;
            index = (index + step).min(rest.len());
        }
        if index >= rest.len() {
            rows.push(rest);
            break;
        }
        let cut = match last_space {
            Some(space) if space > 0 => space,
            _ => index.max(1),
        };
        rows.push(&rest[..cut]);
        rest = &rest[cut..];
        // The space the row broke at does not start the next one.
        if rest.first() == Some(&0x00) {
            rest = &rest[1..];
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_image(width: u32, height: u32, color: [u8; 4]) -> crate::state::Image {
        crate::state::Image {
            width,
            height,
            rgba: color.repeat((width * height) as usize),
        }
    }

    fn metrics_jpn() -> FontMetrics {
        FontMetrics::from_sheet_width(768)
    }

    fn metrics_usa() -> FontMetrics {
        FontMetrics::from_sheet_width(256)
    }

    fn steps(bytes: &[u8], metrics: FontMetrics) -> Vec<Glyph> {
        Glyphs::new(bytes, metrics).collect()
    }

    /// A 768x48 sheet with one white glyph cell filled at `(u, v)`.
    fn sheet_with_cell(u: i32, v: i32) -> Texture8 {
        let width = 768u32;
        let height = 48u32;
        let mut indices = vec![0u8; (width * height) as usize];
        for y in v..v + GLYPH_HEIGHT {
            for x in u..u + 14 {
                indices[(y as u32 * width + x as u32) as usize] = 1;
            }
        }
        let mut palettes = vec![[0u8; 4]; crate::model::PALETTE_ROW_LEN];
        palettes[1] = [255, 255, 255, 255];
        Texture8 {
            width,
            height,
            indices,
            palettes,
            stp: Vec::new(),
        }
    }

    /// A full 768x256 sheet with a white glyph cell at every `(u, v)`.
    fn sheet_with_cells(cells: &[(i32, i32)]) -> Texture8 {
        let width = 768u32;
        let height = 256u32;
        let mut indices = vec![0u8; (width * height) as usize];
        for &(u, v) in cells {
            for y in v..v + GLYPH_HEIGHT {
                for x in u..u + 14 {
                    indices[(y as u32 * width + x as u32) as usize] = 1;
                }
            }
        }
        let mut palettes = vec![[0u8; 4]; crate::model::PALETTE_ROW_LEN];
        palettes[1] = [255, 255, 255, 255];
        Texture8 {
            width,
            height,
            indices,
            palettes,
            stp: Vec::new(),
        }
    }

    #[test]
    fn metrics_follow_the_sheet_width() {
        let jpn = metrics_jpn();
        assert_eq!(jpn.glyph_w, 14);
        assert_eq!(jpn.left_margin, 0x22);
        assert_eq!(jpn.text_y, 181);
        assert_eq!(jpn.menu_y, 186);

        let usa = metrics_usa();
        assert_eq!(usa.glyph_w, 8);
        assert_eq!(usa.left_margin, 0x30);
        assert_eq!(usa.text_y, 181);
        assert_eq!(usa.menu_y, 186);
    }

    #[test]
    fn plain_bytes_address_the_left_page_grid() {
        let decoded = steps(&[0x1D], metrics_jpn());
        assert_eq!(decoded.len(), 1);
        assert_eq!(
            decoded[0],
            Glyph {
                u: 154,
                v: 42,
                width: 14,
                height: 14,
                advance: 14,
                missing: false,
            }
        );

        let decoded = steps(&[0x0C], metrics_jpn());
        assert_eq!(decoded[0].u, 168);
        assert_eq!(decoded[0].v, 28);
    }

    #[test]
    fn wrap_cells_breaks_at_spaces_and_cell_limits() {
        // `A B`: `A` = 0x1D, space = 0x00, `B` = 0x1E, terminator 0x01.
        let bytes = [0x1D, 0x00, 0x1E, 0x01];
        assert_eq!(
            wrap_cells(&bytes, 10),
            vec![[0x1D, 0x00, 0x1E, 0x01].as_slice()]
        );
        assert_eq!(
            wrap_cells(&bytes, 2),
            vec![[0x1D].as_slice(), [0x1E, 0x01].as_slice()]
        );

        // An extended glyph pair counts as one cell.
        let bytes = [0xF9, 0x09, 0x1D, 0x00, 0x1E];
        assert_eq!(
            wrap_cells(&bytes, 2),
            vec![[0xF9, 0x09, 0x1D].as_slice(), [0x1E].as_slice()]
        );

        // `0xFB` takes no cell.
        let bytes = [0x1D, 0xFB, 0x1E, 0x1D];
        assert_eq!(
            wrap_cells(&bytes, 2),
            vec![[0x1D, 0xFB, 0x1E].as_slice(), [0x1D].as_slice()]
        );
    }

    #[test]
    fn the_missing_glyph_cell_is_a_hollow_box() {
        let font = Font::new(sheet_with_cell(168, 28));
        let glyph = Glyphs::new(&crate::charset::MISSING, font.metrics)
            .next()
            .expect("the missing sequence decodes to a cell");
        assert!(glyph.visible());
        assert!(glyph.missing);
        let pixel = |x: i32, y: i32| {
            font.missing.indices[(y as u32 * font.missing.width + x as u32) as usize]
        };
        assert_ne!(pixel(glyph.u, glyph.v), 0, "top-left corner");
        assert_ne!(
            pixel(glyph.u + glyph.width - 1, glyph.v),
            0,
            "top-right corner"
        );
        assert_ne!(
            pixel(glyph.u, glyph.v + glyph.height - 1),
            0,
            "bottom-left corner"
        );
        assert_eq!(
            pixel(glyph.u + glyph.width / 2, glyph.v + glyph.height / 2),
            0,
            "the middle stays hollow"
        );
    }

    #[test]
    fn font_new_leaves_the_sheet_byte_identical() {
        // The shipped 0x0B cell is the real ▼ the message machine draws; the
        // constructor must not paint over it (or anywhere else).
        let sheet = sheet_with_cells(&[(154, 28)]);
        let before = sheet.clone();
        let font = Font::new(sheet);
        assert_eq!(font.texture, before, "Font::new mutated the decoded sheet");
    }

    #[test]
    fn item_54s_lead_in_pair_still_decodes_to_the_sheet_cell() {
        // The ja-JP table spells ITEM_54_NAME with `0xFA 0x3B` — the sheet's
        // one blank, unnamed cell — so that pair must stay an ordinary sheet
        // sample at (326, 238), not become the missing box.
        let decoded = steps(&[0xFA, 0x3B], metrics_jpn());
        assert_eq!(decoded.len(), 1);
        assert!(!decoded[0].missing);
        assert_eq!((decoded[0].u, decoded[0].v), (326, 238));
    }

    #[test]
    fn draw_text_samples_the_sheet_for_0x0b_and_the_box_for_missing() {
        // A sheet with the ▼ at the 0x0B cell (white) and a distinct ink for
        // the synthetic box's palette index.
        let mut sheet = sheet_with_cell(154, 28);
        sheet.palettes[8] = [9, 8, 7, 255];
        let font = Font::new(sheet);

        fn pixel(framebuffer: &Framebuffer, x: i32, y: i32) -> [u8; 4] {
            let offset = (y as usize * 320 + x as usize) * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        }

        // Byte 0x0B draws the sheet's arrow, not the box.
        let mut framebuffer = Framebuffer::new();
        font.draw_text(&mut framebuffer, 10, 10, Tint::White, 2, &[0x0B]);
        assert_eq!(pixel(&framebuffer, 10, 10), [255, 255, 255, 255]);

        // The reserved missing sequence draws the synthetic hollow box.
        let mut framebuffer = Framebuffer::new();
        font.draw_text(
            &mut framebuffer,
            10,
            10,
            Tint::White,
            2,
            &crate::charset::MISSING,
        );
        assert_eq!(pixel(&framebuffer, 10, 10), [9, 8, 7, 255], "box edge");
        assert_eq!(pixel(&framebuffer, 17, 17), [0, 0, 0, 0], "box hollow");
    }

    #[test]
    fn extended_pages_use_their_own_rows() {
        let f8 = steps(&[0xF8, 20], metrics_jpn());
        assert_eq!(f8[0].u, 28);
        assert_eq!(f8[0].v, 224);

        let f9 = steps(&[0xF9, 20], metrics_jpn());
        assert_eq!(f9[0].u, 256 + 28);
        assert_eq!(f9[0].v, 14);

        let fa = steps(&[0xFA, 20], metrics_jpn());
        assert_eq!(fa[0].u, 256 + 28);
        assert_eq!(fa[0].v, 210);
    }

    #[test]
    fn usa_extended_glyphs_use_eight_pixel_columns() {
        let f8 = steps(&[0xF8, 20], metrics_usa());
        assert_eq!(f8[0].u, 16);
        assert_eq!(f8[0].v, 224);
        assert_eq!(f8[0].width, 8);

        let plain = steps(&[0x1D], metrics_usa());
        assert_eq!(plain[0].u, 88);
        assert_eq!(plain[0].v, 42);
    }

    #[test]
    fn space_no_op_half_advance_and_end_bytes() {
        let space = steps(&[0x00], metrics_jpn());
        assert_eq!(space.len(), 1);
        assert!(!space[0].visible());
        assert_eq!(space[0].advance, 14);

        // 0xFB is a true no-op: it neither draws nor advances.
        let no_op = steps(&[0xFB, 0x0C], metrics_jpn());
        assert_eq!(no_op.len(), 1);
        assert!(no_op[0].visible());
        assert_eq!(no_op[0].u, 168);

        let half = steps(&[0xFF], metrics_jpn());
        assert_eq!(half.len(), 1);
        assert!(!half[0].visible());
        assert_eq!(half[0].advance, 7);
        assert_eq!(steps(&[0xFF], metrics_usa())[0].advance, 4);

        assert_eq!(steps(&[0x0C, 0x01], metrics_jpn()).len(), 1);
        assert_eq!(steps(&[0x0C, 0x07], metrics_jpn()).len(), 1);
        assert!(steps(&[0x01], metrics_jpn()).is_empty());
        assert!(steps(&[0x07], metrics_jpn()).is_empty());
    }

    #[test]
    fn parentheses_remap_to_the_controller_cells() {
        let open = steps(&[0x28], metrics_jpn());
        assert_eq!((open[0].u, open[0].v), (56, 224));
        let close = steps(&[0x29], metrics_jpn());
        assert_eq!((close[0].u, close[0].v), (70, 224));
    }

    #[test]
    fn the_plain_iterator_keeps_parentheses_as_letters() {
        let metrics = metrics_jpn();
        let open: Vec<Glyph> = Glyphs::plain(&[0x28], metrics).collect();
        assert_eq!((open[0].u, open[0].v), (4 * 14, 28 + 2 * 14));
        let close: Vec<Glyph> = Glyphs::plain(&[0x29], metrics).collect();
        assert_eq!((close[0].u, close[0].v), (5 * 14, 28 + 2 * 14));
    }

    #[test]
    fn truncated_extended_sequence_ends_the_stream() {
        assert!(steps(&[0xF8], metrics_jpn()).is_empty());
        assert!(steps(&[0xF9], metrics_jpn()).is_empty());
        assert_eq!(steps(&[0x0C, 0xFA], metrics_jpn()).len(), 1);
    }

    #[test]
    fn draw_text_places_glyphs_at_the_cursor() {
        let font = Font::new(sheet_with_cell(168, 28));
        let mut framebuffer = Framebuffer::new();

        font.draw_text(&mut framebuffer, 10, 10, Tint::Green, 2, &[0x0C]);

        fn pixel(framebuffer: &Framebuffer, x: usize, y: usize) -> [u8; 4] {
            let offset = (y * 320 + x) * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        }
        assert_eq!(pixel(&framebuffer, 10, 10), [0, 255, 0, 255]);
        assert_eq!(pixel(&framebuffer, 23, 23), [0, 255, 0, 255]);
        assert_eq!(pixel(&framebuffer, 24, 10), [0, 0, 0, 0]);
        assert_eq!(pixel(&framebuffer, 10, 24), [0, 0, 0, 0]);

        // A space advances one cell; the second glyph lands at x=38.
        font.draw_text(
            &mut framebuffer,
            10,
            30,
            Tint::White,
            2,
            &[0x0C, 0x00, 0x0C],
        );
        assert_eq!(pixel(&framebuffer, 10, 30), [255, 255, 255, 255]);
        assert_eq!(pixel(&framebuffer, 24, 30), [0, 0, 0, 0]);
        assert_eq!(pixel(&framebuffer, 38, 30), [255, 255, 255, 255]);

        // Brightness 15 dims the white glyph to 127.
        font.draw_text(&mut framebuffer, 10, 50, Tint::White, 15, &[0x0C]);
        assert_eq!(pixel(&framebuffer, 10, 50), [127, 127, 127, 255]);
    }

    #[test]
    fn draw_text_queues_a_black_shadow_at_the_brightness_alpha() {
        let font = Font::new(sheet_with_cell(168, 28));
        // The shadow pass uses brightness * 255 / 30 with no 0/2 collapse;
        // only the text pass maps 0 and 2 to full.
        assert_eq!(shadow_alpha(0), 0);
        assert_eq!(shadow_alpha(2), 17);
        assert_eq!(shadow_alpha(15), 127);
        assert_eq!(shadow_alpha(30), 255);

        fn pixel(framebuffer: &Framebuffer, x: i32, y: i32) -> [u8; 4] {
            let offset = (y as usize * 320 + x as usize) * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        }

        // Over a 200 grey page: the text pass paints 127 grey at brightness
        // 15, and the shadow's one-pixel edge darkens the grey by 127/255.
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
        font.draw_text(&mut framebuffer, 10, 10, Tint::White, 15, &[0x0C]);
        assert_eq!(pixel(&framebuffer, 10, 10), [127, 127, 127, 255]);
        // The shadow-only column one past the glyph's right edge.
        assert_eq!(pixel(&framebuffer, 24, 11), [100, 100, 100, 255]);

        // At the exit fade's default brightness the shadow keeps its
        // brightness-derived alpha rather than collapsing.
        let mut framebuffer = Framebuffer::new();
        framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
        font.draw_text(&mut framebuffer, 10, 10, Tint::White, 2, &[0x0C]);
        assert_eq!(pixel(&framebuffer, 24, 11), [186, 186, 186, 255]);
    }

    #[test]
    fn draw_text_draws_every_page_and_tint() {
        // White cells at the plain 0x0C, 0xF8, 0xF9 and 0xFA coordinates and
        // at the parenthesis remap target (56, 224).
        let font = Font::new(sheet_with_cells(&[
            (168, 28),
            (28, 224),
            (256 + 28, 14),
            (256 + 28, 210),
            (56, 224),
        ]));

        fn pixel(framebuffer: &Framebuffer, x: i32, y: i32) -> [u8; 4] {
            let offset = (y as usize * 320 + x as usize) * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        }

        let mut framebuffer = Framebuffer::new();
        for (line, tint) in [
            Tint::White,
            Tint::Green,
            Tint::Red,
            Tint::Grey,
            Tint::Yellow,
        ]
        .into_iter()
        .enumerate()
        {
            let y = line as i32 * 16;
            font.draw_text(&mut framebuffer, 10, y, tint, 2, &[0x0C]);
            assert_eq!(
                pixel(&framebuffer, 10, y),
                [tint.rgb()[0], tint.rgb()[1], tint.rgb()[2], 255],
                "tint {tint:?}"
            );
        }

        // One glyph per byte step: plain, the two extended pages and the
        // remapped parenthesis; the end byte draws nothing further.
        font.draw_text(
            &mut framebuffer,
            10,
            100,
            Tint::White,
            2,
            &[0x0C, 0xF8, 20, 0xF9, 20, 0xFA, 20, 0x28, 0x01],
        );
        for column in 0..5 {
            assert_eq!(
                pixel(&framebuffer, 10 + column * 14, 100),
                [255, 255, 255, 255]
            );
        }
        assert_eq!(pixel(&framebuffer, 10 + 5 * 14, 100), [0, 0, 0, 0]);
    }

    #[test]
    #[ignore = "requires ARKLAY_RE1_PACK"]
    fn real_font_new_leaves_the_shipped_sheet_untouched() {
        let Some(path) = std::env::var("ARKLAY_RE1_PACK").ok() else {
            return;
        };
        let pack = crate::pack::Pack::open(std::path::Path::new(&path)).unwrap();
        let sheet = crate::tim::decode_4bpp(&pack.read("font/font.tim").unwrap()).unwrap();
        let before = sheet.clone();
        let font = Font::new(sheet);
        assert_eq!(
            font.texture, before,
            "Font::new mutated the decoded font/font.tim"
        );

        // The shipped 0x0B cell (u 154, v 28) is the real ▼ and must stay
        // non-blank. The `0xFA 0x3B` cell (u 326, v 238), which the ja-JP
        // table uses as the blank lead-in of item 0x54's name, must stay
        // blank. (Byte-identity already implies both; assert the premises so a
        // future sheet change is noticed.)
        let shaded = |u: u32, v: u32| {
            (v..v + GLYPH_HEIGHT as u32)
                .flat_map(|y| (u..u + 14).map(move |x| (x, y)))
                .filter(|&(x, y)| font.texture.indices[(y * font.texture.width + x) as usize] != 0)
                .count()
        };
        assert!(shaded(154, 28) > 0, "the sheet's 0x0B arrow cell is blank");
        assert_eq!(shaded(326, 238), 0, "the sheet's 0xFA 0x3B cell is inked");
    }
}
