//! The shared text font: metrics, glyph decoding and tinted drawing.
//!
//! One encoded byte stream is decoded left to right into [`Glyph`]s. Plain
//! bytes address the glyph grid directly, `0xF8`/`0xF9`/`0xFA` select the
//! extended glyph pages with a second byte, and a few control bytes move or
//! stop the cursor. [`Font::draw_text`] samples the sheet through the
//! original's CLUT-driven [`Tint`] table and the pending 2D sprite path.
//!
//! Two byte spaces share the grid. The game-text encoding — what
//! [`crate::charset`] produces, and what the RDT and message data already
//! hold — is what [`Glyphs::new`] decodes: every plain byte draws its own
//! cell. [`Glyphs::text_8x14`] is the original's `PrintText8x14`
//! (0x00455520) ASCII-buffer renderer, whose 40/41 are `(` and `)` and are
//! remapped to the sheet's round parentheses; see its own note.

use crate::model::Texture8;
use crate::pack::{AssetClass, Pack};
use crate::render::{Framebuffer, SpriteDraw};

pub use crate::render::Tint;

/// Glyph height in pixels for both sheet widths.
pub const GLYPH_HEIGHT: i32 = 14;
/// Width at which a sheet selects the 14px Japanese metrics.
pub const JPN_SHEET_WIDTH: u32 = 768;

/// The default sheet entry: whatever font the converted install ships, the
/// Japanese `FONT.TIM` or the Western `fontus.tim`.
pub const DEFAULT_ENTRY: &str = "font/font.tim";
/// The Japanese sheet as a script-specific override, packed when the install
/// carries a Western default too.
pub const JAPANESE_ENTRY: &str = "font/jpn.tim";
/// The Western sheet as a script-specific override, packed when the install
/// carries a Japanese default.
pub const LATIN_ENTRY: &str = "font/latin.tim";
/// The default sheet's pack stem, the key a replacement resolves by.
pub const DEFAULT_STEM: &str = "font/font";

/// The pack stem a session in `script` draws with: the script's own sheet
/// when the pack ships one, else the default.
///
/// The lookup is by stem, so a mod may replace a sheet with any image format
/// and either script's override may be introduced by a mod.
pub fn session_entry(pack: &Pack, script: crate::charset::Script) -> &'static str {
    let stem = match script {
        crate::charset::Script::Japanese => "font/jpn",
        crate::charset::Script::Latin => "font/latin",
    };
    if pack.has_asset(stem, AssetClass::Image) {
        stem
    } else {
        DEFAULT_STEM
    }
}

/// Decode the sheet a session in `script` draws with, or `None` when the pack
/// carries no usable font.
pub fn load_session(pack: &Pack, script: crate::charset::Script) -> Option<Font> {
    let stem = session_entry(pack, script);
    let asset = pack.read_asset(stem, AssetClass::Image).ok().flatten()?;
    match crate::assets::decode_texture(&asset, crate::assets::Geometry::Shipped) {
        Ok(texture) => Some(Font::new(texture)),
        Err(err) => {
            eprintln!("warning: invalid {}: {err:#}", asset.path());
            None
        }
    }
}

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

/// One glyph cell's tight metrics: its left trim and advance.
///
/// A cell drawn under the tight path blits at `x - indent` and advances the
/// cursor by `advance`, so ink sits flush against the previous glyph instead
/// of leaving the Japanese cell's right bearing as a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphMetric {
    /// Pixels the cell is shifted left when drawn.
    pub indent: i32,
    /// Horizontal cursor advance after the cell.
    pub advance: i32,
}

impl GlyphMetric {
    /// The full-cell metric: no trim, one cell of advance.
    const fn full(glyph_w: i32) -> Self {
        Self {
            indent: 0,
            advance: glyph_w,
        }
    }
}

/// The plain-glyph grid's columns (18 cells per page).
const PAGE_COLUMNS: usize = 18;
/// The x offset of the second (right) glyph page.
const RIGHT_PAGE_X: i32 = 256;

/// The row a plain glyph byte addresses (the grid starts two rows down).
fn plain_row(byte: u8) -> usize {
    2 + usize::from(byte) / PAGE_COLUMNS
}

/// The column a plain glyph byte addresses.
fn plain_col(byte: u8) -> usize {
    usize::from(byte) % PAGE_COLUMNS
}

/// One font sheet's per-cell metrics, measured from its own ink.
///
/// The table covers the two glyph pages [`Glyphs`] decodes: the plain/`0xF8`
/// left page at x 0 and the `0xF9`/`0xFA` right page at x 256. Every cell
/// defaults to the full-cell [`GlyphMetric`]; [`GlyphTable::measure`] replaces
/// the entries whose ink is narrower and applies the authored rules (an 8px
/// space and full-cell digits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphTable {
    glyph_w: i32,
    pages: [Vec<GlyphMetric>; 2],
}

impl GlyphTable {
    /// Measure every cell of the decoded `texture`. The sheet is never
    /// mutated, so this is exactly the ink the pack ships.
    pub fn measure(texture: &Texture8, glyph_w: i32) -> Self {
        let glyph_w = glyph_w.max(1);
        let rows = (texture.height as i32 / GLYPH_HEIGHT).max(0) as usize;
        let full = GlyphMetric::full(glyph_w);
        let mut table = Self {
            glyph_w,
            pages: [
                vec![full; rows * PAGE_COLUMNS],
                vec![full; rows * PAGE_COLUMNS],
            ],
        };
        for page in 0..2 {
            for row in 0..rows {
                for col in 0..PAGE_COLUMNS {
                    let left = page as i32 * RIGHT_PAGE_X + col as i32 * glyph_w;
                    let top = row as i32 * GLYPH_HEIGHT;
                    if left + glyph_w > texture.width as i32
                        || top + GLYPH_HEIGHT > texture.height as i32
                    {
                        continue;
                    }
                    if let Some(metric) = measure_cell(texture, left, top, glyph_w) {
                        table.pages[page][row * PAGE_COLUMNS + col] = metric;
                    }
                }
            }
        }
        table.apply_authored_rules();
        table
    }

    /// The metric of the cell at `(page, row, col)`, falling back to the full
    /// cell for out-of-range cells.
    pub fn get(&self, page: usize, row: usize, col: usize) -> GlyphMetric {
        self.pages
            .get(page)
            .and_then(|cells| cells.get(row * PAGE_COLUMNS + col))
            .copied()
            .unwrap_or_else(|| GlyphMetric::full(self.glyph_w))
    }

    /// The metric of the cell at sheet texel `(u, v)`, the coordinate pair
    /// [`Glyph`] carries.
    fn at(&self, u: i32, v: i32, glyph_w: i32) -> GlyphMetric {
        let glyph_w = glyph_w.max(1);
        let page = usize::from(u >= RIGHT_PAGE_X);
        let col = ((u - page as i32 * RIGHT_PAGE_X) / glyph_w).max(0) as usize;
        let row = (v / GLYPH_HEIGHT).max(0) as usize;
        self.get(page, row, col)
    }

    fn set(&mut self, page: usize, row: usize, col: usize, metric: GlyphMetric) {
        if let Some(cells) = self.pages.get_mut(page)
            && let Some(cell) = cells.get_mut(row * PAGE_COLUMNS + col)
        {
            *cell = metric;
        }
    }

    /// The authored overrides the derivation cannot express.
    fn apply_authored_rules(&mut self) {
        let full = GlyphMetric::full(self.glyph_w);
        // The space cell is blank in the sheet; give it an 8px advance so
        // words do not gap at the Japanese cell width.
        self.set(
            0,
            plain_row(0x00),
            plain_col(0x00),
            GlyphMetric {
                indent: 0,
                advance: 8.min(self.glyph_w),
            },
        );
        // The digits keep the full cell so counters and save timers stay
        // monospaced.
        for byte in 0x0C..=0x15u8 {
            self.set(0, plain_row(byte), plain_col(byte), full);
        }
    }
}

/// Measure one cell's ink bounding box and derive its metric, or `None` when
/// the cell is blank or already fills the cell (the full metric is the
/// default).
fn measure_cell(texture: &Texture8, left: i32, top: i32, glyph_w: i32) -> Option<GlyphMetric> {
    let mut min_x = i32::MAX;
    let mut max_x = -1;
    for y in 0..GLYPH_HEIGHT {
        for x in 0..glyph_w {
            let u = (left + x) as u32;
            let v = (top + y) as u32;
            let index = (v * texture.width + u) as usize;
            if texture.indices.get(index).copied().unwrap_or(0) != 0 {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
            }
        }
    }
    if max_x < 0 {
        return None;
    }
    let ink = max_x - min_x + 1;
    let advance = (ink + 1).min(glyph_w);
    (min_x != 0 || advance != glyph_w).then_some(GlyphMetric {
        indent: min_x,
        advance,
    })
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
    /// Pixels the cell is shifted left when drawn (the tight path's trim).
    pub indent: i32,
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
            indent: 0,
        }
    }

    /// Whether this step draws a glyph.
    pub fn visible(self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// Which glyph grid a byte stream addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphGrid {
    /// The game's encoded grid: a plain byte lands on row `byte / 18`, whose
    /// texture rows start at `v = 28` on both sheets. The original's
    /// `PrintFormattedText` (0x00455190) adds those two rows.
    Encoded,
    /// The original's `PrintText8x14` (0x00455520) grid: a plain byte
    /// addresses the grid directly with **no** two-row bias, `' '` advances
    /// without a glyph and `0x00` ends the string. The item box's empty-slot
    /// label and the countdown timer come through this renderer, which is why
    /// `_Nothing_` lands on the Latin cells of the Japanese sheet.
    Text8x14,
}

/// Iterator over the glyphs of one encoded byte stream.
///
/// `0x00` advances one cell, `0xFF` half a cell, `0xFB` is a no-op, and
/// `0x01`/`0x07` end the stream. A truncated `0xF8`/`0xF9`/`0xFA` sequence
/// also ends it without a glyph.
///
/// Every plain byte draws its own grid cell, whatever the sheet holds there.
/// The original's game-text renderers all index the grid in this same
/// unredirected way — the message pass (`message_render_chars`, 0x00456020),
/// `PrintFormattedText` (0x00455190) and the item-name drawer (0x00454fd0) —
/// so no byte of the game-text encoding is ever spent on a second meaning.
/// The `0x28`/`0x29` cells are the letters L and M on the shipped sheet; see
/// [`Glyphs::text_8x14`] for the one renderer that works in ASCII instead.
pub struct Glyphs<'a> {
    bytes: &'a [u8],
    position: usize,
    metrics: FontMetrics,
    table: Option<&'a GlyphTable>,
    /// Only the ASCII `PrintText8x14` grid sets this; see
    /// [`Glyphs::text_8x14`].
    remap_parentheses: bool,
    grid: GlyphGrid,
}

impl<'a> Glyphs<'a> {
    /// Decode game-text `bytes` with `metrics`: every plain byte samples its
    /// own left-page cell, so `0x28`/`0x29` are the letters L and M.
    ///
    /// The round parentheses are the `0xF8 0x16`/`0xF8 0x17` cells the charset
    /// encodes them to, and a stream that spells `(` with `0xF8 0x16` samples
    /// the same cell as [`Glyphs::text_8x14`]'s parenthesis remap.
    pub fn new(bytes: &'a [u8], metrics: FontMetrics) -> Self {
        Self {
            bytes,
            position: 0,
            metrics,
            table: None,
            remap_parentheses: false,
            grid: GlyphGrid::Encoded,
        }
    }

    /// Decode one byte stream with the original's `PrintText8x14`
    /// (0x00455520) grid: the plain bytes are *ASCII*, each addressing the
    /// sheet at `v = (b/18)*14` with no `+2` row bias, and 40/41 — ASCII `(`
    /// and `)` — are remapped to the sheet's round parentheses at
    /// `(56, 224)`/`(70, 224)`.
    ///
    /// The missing bias is what makes this a separate byte space: its row
    /// `b/18` is the game-text grid's row `b/18 - 2` (the grid [`Glyphs::new`]
    /// decodes starts two rows in at `v = 28`), so its `L` is byte 76 and
    /// lands on the same cell the game-text encoding spells `0x28`. The remap
    /// is a consequence of that space, not of the sheet: an ASCII `(` would
    /// otherwise sample cell 40, a controller symbol, and the letter the byte
    /// looks like is 76 instead — so it can never hide a letter. The game-text
    /// encoding spells the same parenthesis `0xF8 0x16`, which [`Glyphs::new`]
    /// samples directly.
    pub fn text_8x14(bytes: &'a [u8], metrics: FontMetrics) -> Self {
        Self {
            bytes,
            position: 0,
            metrics,
            table: None,
            remap_parentheses: true,
            grid: GlyphGrid::Text8x14,
        }
    }

    /// Use `table`'s per-glyph metrics instead of the full-cell default, the
    /// Latin tight path.
    pub fn tight(mut self, table: &'a GlyphTable) -> Self {
        self.table = Some(table);
        self
    }

    /// The metric of the glyph cell at `(u, v)`: the tight table's entry when
    /// one is selected, else the full cell.
    fn metric(&self, u: i32, v: i32) -> GlyphMetric {
        match self.table {
            Some(table) => table.at(u, v, self.metrics.glyph_w),
            None => GlyphMetric::full(self.metrics.glyph_w),
        }
    }

    fn glyph(&self, u: i32, v: i32) -> Glyph {
        let metric = self.metric(u, v);
        Glyph {
            u,
            v,
            width: self.metrics.glyph_w,
            height: GLYPH_HEIGHT,
            advance: metric.advance,
            missing: false,
            indent: metric.indent,
        }
    }

    /// The reserved missing-glyph cell: the synthetic box carries no sheet
    /// coordinate, so it samples from `u = v = 0` of the box texture and keeps
    /// the full-cell metric (the box is a full cell wide).
    fn missing_glyph(&self) -> Glyph {
        Glyph {
            u: 0,
            v: 0,
            width: self.metrics.glyph_w,
            height: GLYPH_HEIGHT,
            advance: self.metrics.glyph_w,
            missing: true,
            indent: 0,
        }
    }
}

impl Iterator for Glyphs<'_> {
    type Item = Glyph;

    fn next(&mut self) -> Option<Glyph> {
        // `PrintText8x14` has no escapes: every byte below `0x0C` is an ASCII
        // control or a space and never reaches the grid.
        if self.grid == GlyphGrid::Text8x14 {
            let byte = *self.bytes.get(self.position)?;
            self.position += 1;
            if byte == 0 {
                return None;
            }
            if byte == b' ' {
                return Some(Glyph::cursor(self.metrics.glyph_w));
            }
            return Some(self.plain_glyph(byte));
        }
        loop {
            let byte = *self.bytes.get(self.position)?;
            match byte {
                0x01 | 0x07 => return None,
                0x00 => {
                    self.position += 1;
                    // The space cell is the plain grid's first cell.
                    return Some(Glyph::cursor(self.metric(0, 28).advance));
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
                    return Some(self.plain_glyph(byte));
                }
            }
        }
    }
}

impl Glyphs<'_> {
    /// One plain byte of the sheet's glyph grid, with the grid's row bias and
    /// the ASCII path's parenthesis remap applied.
    fn plain_glyph(&self, byte: u8) -> Glyph {
        // The original's `PrintText8x14` — which reads an ASCII buffer, not
        // the game-text encoding — draws ASCII '(' and ')' as the sheet's
        // round parentheses. Only the ASCII grid sets the flag.
        if self.remap_parentheses {
            if byte == 0x28 {
                return self.glyph(56, 224);
            }
            if byte == 0x29 {
                return self.glyph(70, 224);
            }
        }
        let byte = i32::from(byte);
        let col = byte % 18;
        let row = byte / 18;
        let bias = if self.grid == GlyphGrid::Encoded {
            28
        } else {
            0
        };
        self.glyph(col * self.metrics.glyph_w, bias + row * GLYPH_HEIGHT)
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
    /// The tight per-glyph metrics measured from the sheet's ink.
    pub glyph_table: GlyphTable,
}

impl Font {
    /// Pair `texture` with the metrics its width selects.
    ///
    /// The decoded sheet is left byte-identical; the missing-glyph placeholder
    /// is a separate synthetic texture, so no sheet cell is ever clobbered.
    pub fn new(texture: Texture8) -> Self {
        let metrics = FontMetrics::from_sheet_width(texture.width);
        let missing = missing_box_texture(&texture, metrics.glyph_w);
        let glyph_table = GlyphTable::measure(&texture, metrics.glyph_w);
        Self {
            texture,
            metrics,
            missing,
            glyph_table,
        }
    }

    /// The script the sheet's width selects: the 768-wide Japanese sheet or
    /// the 256-wide Western one. Every stream the session draws must be
    /// encoded in this script's byte space.
    pub fn script(&self) -> crate::charset::Script {
        if self.texture.width >= JPN_SHEET_WIDTH {
            crate::charset::Script::Japanese
        } else {
            crate::charset::Script::Latin
        }
    }

    /// The decoded glyph stream of game-text `bytes`, with the tight table
    /// when `tight`.
    fn glyphs<'a>(&'a self, tight: bool, bytes: &'a [u8]) -> Glyphs<'a> {
        let glyphs = Glyphs::new(bytes, self.metrics);
        // The tight table compensates for the Japanese sheet's full-width
        // Latin glyphs. The Western sheet was drawn at its own 8px advances
        // by the original, so it ignores the flag and every cell is whole.
        if tight && self.script() == crate::charset::Script::Japanese {
            glyphs.tight(&self.glyph_table)
        } else {
            glyphs
        }
    }

    /// The total horizontal advance of `bytes` drawn by [`Font::draw_text`]
    /// under `tight` metrics.
    pub fn advance(&self, tight: bool, bytes: &[u8]) -> i32 {
        self.glyphs(tight, bytes).map(|glyph| glyph.advance).sum()
    }

    /// The advance of the single encoded step at `bytes[0]` (with its operand
    /// for an extended page) under `tight`, on [`Font::draw_text`]'s path.
    ///
    /// Used by the pixel-budget wrapping and clipping, which mirror
    /// [`Font::draw_text`].
    pub fn step_advance(&self, tight: bool, bytes: &[u8]) -> i32 {
        match bytes.first() {
            None | Some(0x01 | 0x07 | 0xFB) => 0,
            Some(&byte) => {
                let len = if (0xF8..=0xFA).contains(&byte) { 2 } else { 1 };
                self.glyphs(tight, &bytes[..len.min(bytes.len())])
                    .map(|glyph| glyph.advance)
                    .sum()
            }
        }
    }

    /// Draw a game-text stream left to right from `(x, y)`.
    ///
    /// Every step advances the cursor by its own advance; glyphs sample
    /// palette row 0 (the sheet's grey ramp) mixed by `tint` and scaled by
    /// `brightness`. Under `tight` the sheet's per-glyph metrics trim each
    /// cell and use its own advance (the Latin path); otherwise every glyph
    /// fills its cell (the Japanese default). No byte is redirected: see
    /// [`Glyphs::new`].
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text(
        &self,
        framebuffer: &mut Framebuffer,
        x: i32,
        y: i32,
        tint: Tint,
        brightness: u8,
        bytes: &[u8],
        tight: bool,
    ) {
        self.draw_glyphs(framebuffer, x, y, tint, brightness, bytes, tight, None);
    }

    /// [`Font::draw_text`], condensed horizontally when the stream's advance
    /// is wider than `max_width`.
    ///
    /// A window sized for the original's own text overflows when a pack
    /// draws Latin through the Japanese sheet's wider letters. Over budget,
    /// every glyph's position and width scale by `max_width / advance`
    /// (nearest sampling), so the whole line stays inside the window; text
    /// that already fits draws exactly as [`Font::draw_text`] does.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text_fit(
        &self,
        framebuffer: &mut Framebuffer,
        x: i32,
        y: i32,
        tint: Tint,
        brightness: u8,
        bytes: &[u8],
        tight: bool,
        max_width: i32,
    ) {
        let advance = self.advance(tight, bytes);
        let fit = (max_width > 0 && advance > max_width).then_some((max_width, advance));
        self.draw_glyphs(framebuffer, x, y, tint, brightness, bytes, tight, fit);
    }

    /// Draw a plain byte string with the original's `PrintText8x14`
    /// (0x00455520) glyph grid; see [`Glyphs::text_8x14`].
    ///
    /// The renderer is the same as [`Font::draw_text`]'s, minus the encoded
    /// grid's two-row bias (the original's `PrintText8x14(.., flags = 0)`).
    pub fn draw_text_8x14(
        &self,
        framebuffer: &mut Framebuffer,
        x: i32,
        y: i32,
        tint: Tint,
        bytes: &[u8],
    ) {
        let mut sprites: Vec<SpriteDraw<'_>> = Vec::new();
        let mut cursor = x;
        for glyph in Glyphs::text_8x14(bytes, self.metrics) {
            if glyph.visible() {
                sprites.push(SpriteDraw::indexed(
                    &self.texture,
                    [glyph.u, glyph.v, glyph.width, glyph.height],
                    [cursor, y, glyph.width, glyph.height],
                    0,
                    0,
                    tint,
                ));
            }
            cursor += glyph.advance;
        }
        framebuffer.draw_sprites(&mut sprites);
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
        tight: bool,
        fit: Option<(i32, i32)>,
    ) {
        let glyphs = self.glyphs(tight, bytes);
        // Offsets from `x` scale by `fit` = (budget, advance); a glyph's
        // width is the gap between its scaled edges so neighbours still abut.
        let scale = |offset: i32| match fit {
            Some((budget, advance)) => (offset * budget).div_euclid(advance),
            None => offset,
        };
        // Every glyph is one tinted sprite at the pending-sprite depth the
        // brightness selects. The original's text renderers all leave the
        // descriptor's colour multipliers at 0x80, so `AddTintSprite` never
        // takes its zero-tint shadow branch and no drop shadow is drawn.
        let mut sprites: Vec<SpriteDraw<'_>> = Vec::new();
        let mut cursor = x;
        for glyph in glyphs {
            if glyph.visible() {
                // The tight path trims the cell's left bearing.
                let offset = cursor - x - glyph.indent;
                let left = x + scale(offset);
                let width = if fit.is_some() {
                    (x + scale(offset + glyph.width) - left).max(1)
                } else {
                    glyph.width
                };
                let src = [glyph.u, glyph.v, glyph.width, glyph.height];
                let texture = if glyph.missing {
                    &self.missing
                } else {
                    &self.texture
                };
                sprites.push(SpriteDraw::indexed(
                    texture,
                    src,
                    [left, y, width, glyph.height],
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

/// Split an encoded stream into rows of at most `max_width` pixels, breaking
/// at the last space of a row when there is one.
///
/// Mirrors [`wrap_cells`] but budgets by the steps' advances under `tight`
/// metrics, so a wrapped row never runs past the frame when the glyphs are
/// narrower than their cells.
pub fn wrap_pixels<'a>(bytes: &'a [u8], max_width: i32, font: &Font, tight: bool) -> Vec<&'a [u8]> {
    let budget = max_width.max(1);
    let mut rows = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let mut index = 0;
        let mut width = 0;
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
            let advance = font.step_advance(tight, &rest[index..]);
            if width + advance > budget {
                break;
            }
            if byte == 0x00 {
                last_space = Some(index);
            }
            width += advance;
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

/// The longest prefix of `bytes` that draws in at most `max_width` pixels.
///
/// The pixel-budget analogue of the cell clip the debug overlay uses; the
/// terminators end the stream and `0xFB` takes no width.
pub fn fit_pixels<'a>(bytes: &'a [u8], max_width: i32, font: &Font, tight: bool) -> &'a [u8] {
    let budget = max_width.max(0);
    let mut index = 0;
    let mut width = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == 0x01 || byte == 0x07 {
            break;
        }
        let step = match byte {
            0xF8..=0xFA => 2,
            _ => 1,
        };
        let advance = font.step_advance(tight, &bytes[index..]);
        if width + advance > budget {
            break;
        }
        width += advance;
        index = (index + step).min(bytes.len());
    }
    &bytes[..index]
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

    /// A 768x256 sheet with an ink strip spanning columns `x0..=x1` of each
    /// cell at `(u, v)`.
    fn sheet_with_ink(cells: &[(i32, i32, i32, i32)]) -> Texture8 {
        let width = 768u32;
        let height = 256u32;
        let mut indices = vec![0u8; (width * height) as usize];
        for &(u, v, x0, x1) in cells {
            for y in v..v + GLYPH_HEIGHT {
                for x in u + x0..=u + x1 {
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

    /// The metric of the plain `byte` on the left page.
    fn metric(font: &Font, byte: u8) -> GlyphMetric {
        let byte = usize::from(byte);
        font.glyph_table.get(0, 2 + byte / 18, byte % 18)
    }

    #[test]
    fn glyph_table_measures_the_ink_bounding_box() {
        // 'a' 0x3D is row 5, column 7: 9px of ink at columns 2..=10 gives an
        // indent of 2 and an advance of min(14, 9 + 1) = 10.
        let font = Font::new(sheet_with_ink(&[(98, 70, 2, 10)]));
        assert_eq!(
            metric(&font, 0x3D),
            GlyphMetric {
                indent: 2,
                advance: 10,
            }
        );
        // 'i' 0x45 is row 5, column 15: 5px of ink at columns 4..=8.
        let font = Font::new(sheet_with_ink(&[(210, 70, 4, 8)]));
        assert_eq!(
            metric(&font, 0x45),
            GlyphMetric {
                indent: 4,
                advance: 6,
            }
        );
        // A cell that fills its width starting at 0 keeps the full metric.
        let font = Font::new(sheet_with_ink(&[(210, 70, 0, 13)]));
        assert_eq!(metric(&font, 0x45), GlyphMetric::full(14));
    }

    #[test]
    fn glyph_table_applies_the_authored_rules() {
        // Fill a digit cell with narrow ink: the digit rule still forces the
        // full cell.
        let font = Font::new(sheet_with_ink(&[(168, 28, 4, 8)]));
        assert_eq!(metric(&font, 0x0C), GlyphMetric::full(14));
        // The blank space cell takes the authored 8px advance.
        assert_eq!(
            metric(&font, 0x00),
            GlyphMetric {
                indent: 0,
                advance: 8,
            }
        );
    }

    #[test]
    fn tight_glyphs_use_the_table_advance_and_indent() {
        let font = Font::new(sheet_with_ink(&[(210, 70, 4, 8)]));
        let tight = Glyphs::new(&[0x45], font.metrics)
            .tight(&font.glyph_table)
            .next()
            .unwrap();
        assert_eq!((tight.indent, tight.advance), (4, 6));
        // Without the table the glyph fills its cell.
        let full = Glyphs::new(&[0x45], font.metrics).next().unwrap();
        assert_eq!((full.indent, full.advance), (0, 14));
        // The space advances 8 under the table, 14 without it.
        assert_eq!(font.advance(true, &[0x00]), 8);
        assert_eq!(font.advance(false, &[0x00]), 14);
        // The half advance stays half the cell.
        assert_eq!(font.advance(true, &[0xFF]), 7);
    }

    #[test]
    fn extended_glyphs_take_the_selected_cells_metric() {
        // `0xF8 0x16` selects left row 16, column 4; 6px of ink at 5..=10.
        let font = Font::new(sheet_with_ink(&[(56, 224, 5, 10)]));
        let glyph = Glyphs::new(&[0xF8, 0x16], font.metrics)
            .tight(&font.glyph_table)
            .next()
            .unwrap();
        assert_eq!((glyph.indent, glyph.advance), (5, 7));
        assert_eq!(font.advance(true, &[0xF8, 0x16]), 7);
    }

    #[test]
    fn wrap_pixels_budgets_by_the_advances() {
        // 'i' 0x45 has 6px advance and 14px full width on this sheet.
        let font = Font::new(sheet_with_ink(&[(210, 70, 4, 8)]));
        let text = [0x45, 0x45, 0x45, 0x45];
        // Four 6px glyphs fit in 40px tight; the full cells fit only two.
        assert_eq!(wrap_pixels(&text, 40, &font, true), [text.as_slice()]);
        assert_eq!(
            wrap_pixels(&text, 40, &font, false),
            [&text[..2], &text[2..]]
        );
    }

    #[test]
    fn fit_pixels_clips_at_the_budget() {
        let font = Font::new(sheet_with_ink(&[(210, 70, 4, 8)]));
        let text = [0x45, 0x45, 0x45];
        // Two 6px glyphs fit in 17px, but only one 14px glyph does.
        assert_eq!(fit_pixels(&text, 17, &font, true), &text[..2]);
        assert_eq!(fit_pixels(&text, 17, &font, false), &text[..1]);
        // The terminators end the stream.
        assert_eq!(fit_pixels(&[0x45, 0x01, 0x45], 100, &font, false), &[0x45]);
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
                indent: 0,
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
        font.draw_text(&mut framebuffer, 10, 10, Tint::White, 2, &[0x0B], false);
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
            false,
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
    fn encoded_letters_draw_their_own_cells() {
        // The game-text encoding spells L and M as 0x28/0x29 — the sheet's
        // letters — while the round parentheses are the 0xF8 0x16/0x17 cells.
        // The message and item-name renderers must sample what the encoding
        // says, or "Lockpick" draws "(ockpick" (issue #28).
        let l = steps(&[0x28], metrics_jpn());
        assert_eq!((l[0].u, l[0].v), (4 * 14, 28 + 2 * 14));
        let m = steps(&[0x29], metrics_jpn());
        assert_eq!((m[0].u, m[0].v), (5 * 14, 28 + 2 * 14));

        let open = steps(&[0xF8, 0x16], metrics_jpn());
        assert_eq!((open[0].u, open[0].v), (56, 224));
        let close = steps(&[0xF8, 0x17], metrics_jpn());
        assert_eq!((close[0].u, close[0].v), (70, 224));

        // The measure path follows the same cells: 0x28 spends the letter
        // cell's 10px advance here, where the parenthesis cell's is 7.
        let font = Font::new(sheet_with_ink(&[(56, 56, 2, 10), (56, 224, 5, 10)]));
        assert_eq!(font.advance(true, &[0x28]), 10, "the letter's advance");
        assert_eq!(
            font.advance(true, &[0xF8, 0x16]),
            7,
            "the parenthesis' advance"
        );
    }

    #[test]
    fn the_western_sheet_ignores_the_tight_flag() {
        // The tight table compensates for the Japanese sheet's full-width
        // Latin glyphs. The Western sheet was drawn whole by the original, so
        // a narrow cell keeps its 8px advance even when the language asks for
        // tight metrics (issue #32).
        let width = 256u32;
        let height = 256u32;
        let mut indices = vec![0u8; (width * height) as usize];
        // `I` is plain cell 0x25: row 2, column 1 -> u 8, v 28 + 2*14.
        for y in 56..56 + GLYPH_HEIGHT {
            for x in 10..=12 {
                indices[(y as u32 * width + x as u32) as usize] = 1;
            }
        }
        let mut palettes = vec![[0u8; 4]; crate::model::PALETTE_ROW_LEN];
        palettes[1] = [255, 255, 255, 255];
        let font = Font::new(Texture8 {
            width,
            height,
            indices,
            palettes,
            stp: Vec::new(),
        });
        assert_eq!(font.script(), crate::charset::Script::Latin);
        assert_eq!(font.metrics.glyph_w, 8);
        assert_eq!(font.advance(true, &[0x25]), 8, "the Western sheet is whole");
        assert_eq!(font.advance(false, &[0x25]), 8);

        // The same strip on the Japanese sheet is trimmed to its ink.
        let jpn = Font::new(sheet_with_ink(&[(14, 56, 2, 4)]));
        assert_eq!(jpn.script(), crate::charset::Script::Japanese);
        assert_eq!(jpn.advance(true, &[0x25]), 4, "the Japanese sheet trims");
        assert_eq!(jpn.advance(false, &[0x25]), 14);
    }

    #[test]
    fn ascii_parentheses_remap_to_the_sheet_paren_cells() {
        // `PrintText8x14`'s buffer is ASCII: 40/41 are `(` and `)`, which the
        // remap sends to the sheet's round parentheses. The ASCII letter L —
        // byte 76, the cell the game-text encoding also spells 0x28 — is
        // untouched by it.
        let metrics = metrics_jpn();
        let open: Vec<Glyph> = Glyphs::text_8x14(&[0x28], metrics).collect();
        assert_eq!((open[0].u, open[0].v), (56, 224));
        let close: Vec<Glyph> = Glyphs::text_8x14(&[0x29], metrics).collect();
        assert_eq!((close[0].u, close[0].v), (70, 224));
        let l: Vec<Glyph> = Glyphs::text_8x14(b"L", metrics).collect();
        assert_eq!((l[0].u, l[0].v), (4 * 14, 28 + 2 * 14));
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

        font.draw_text(&mut framebuffer, 10, 10, Tint::Green, 2, &[0x0C], false);

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
            false,
        );
        assert_eq!(pixel(&framebuffer, 10, 30), [255, 255, 255, 255]);
        assert_eq!(pixel(&framebuffer, 24, 30), [0, 0, 0, 0]);
        assert_eq!(pixel(&framebuffer, 38, 30), [255, 255, 255, 255]);

        // Brightness 15 dims the white glyph to 127.
        font.draw_text(&mut framebuffer, 10, 50, Tint::White, 15, &[0x0C], false);
        assert_eq!(pixel(&framebuffer, 10, 50), [127, 127, 127, 255]);
    }

    #[test]
    fn draw_text_fit_condenses_only_a_line_wider_than_the_budget() {
        let font = Font::new(sheet_with_cell(168, 28));
        let line = [0x0C, 0x0C, 0x0C];
        let inked = |framebuffer: &Framebuffer| -> Vec<usize> {
            (0..320)
                .filter(|&x| framebuffer.rgba[(10 * 320 + x) * 4 + 3] != 0)
                .collect()
        };

        // Three 14px cells advance 42px: a 42px budget draws them untouched.
        let mut plain = Framebuffer::new();
        font.draw_text(&mut plain, 10, 10, Tint::White, 0, &line, false);
        let mut roomy = Framebuffer::new();
        font.draw_text_fit(&mut roomy, 10, 10, Tint::White, 0, &line, false, 42);
        assert_eq!(plain.rgba, roomy.rgba);
        assert_eq!(inked(&plain), (10..52).collect::<Vec<_>>());

        // A 21px budget halves every position and width: the ink spans
        // exactly x 10..31 with no gap and nothing past the window.
        let mut condensed = Framebuffer::new();
        font.draw_text_fit(&mut condensed, 10, 10, Tint::White, 0, &line, false, 21);
        assert_eq!(inked(&condensed), (10..31).collect::<Vec<_>>());
    }

    #[test]
    fn draw_text_draws_no_drop_shadow() {
        let font = Font::new(sheet_with_cell(168, 28));
        fn pixel(framebuffer: &Framebuffer, x: i32, y: i32) -> [u8; 4] {
            let offset = (y as usize * 320 + x as usize) * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        }
        // Over a 200 grey page the text pass paints 127 grey at brightness
        // 15, and the column one past the glyph's right edge keeps the page.
        for brightness in [2, 15] {
            let mut framebuffer = Framebuffer::new();
            framebuffer.blit(&solid_image(320, 240, [200, 200, 200, 255]));
            font.draw_text(
                &mut framebuffer,
                10,
                10,
                Tint::White,
                brightness,
                &[0x0C],
                false,
            );
            if brightness == 15 {
                assert_eq!(pixel(&framebuffer, 10, 10), [127, 127, 127, 255]);
            }
            assert_eq!(pixel(&framebuffer, 24, 11), [200, 200, 200, 255]);
        }
    }

    #[test]
    fn draw_text_draws_every_page_and_tint() {
        // Ink at the plain 0x0C, 0xF8, 0xF9 and 0xFA coordinates filling the
        // cell, and split halves at the plain 0x28 letter and the 0xF8 0x16
        // parenthesis beside it, so the step sequence below also shows which
        // of the two cells 0x28 sampled.
        let font = Font::new(sheet_with_ink(&[
            (168, 28, 0, 13),
            (28, 224, 0, 13),
            (256 + 28, 14, 0, 13),
            (256 + 28, 210, 0, 13),
            (56, 56, 0, 4),
            (56, 224, 5, 13),
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
            font.draw_text(&mut framebuffer, 10, y, tint, 2, &[0x0C], false);
            assert_eq!(
                pixel(&framebuffer, 10, y),
                [tint.rgb()[0], tint.rgb()[1], tint.rgb()[2], 255],
                "tint {tint:?}"
            );
        }

        // One glyph per byte step: plain, the two extended pages, the plain
        // letter and the parenthesis pair; the end byte draws nothing further.
        // Cells 0..=3 are inked from their left edge.
        font.draw_text(
            &mut framebuffer,
            10,
            100,
            Tint::White,
            2,
            &[0x0C, 0xF8, 20, 0xF9, 20, 0xFA, 20, 0x28, 0xF8, 0x16, 0x01],
            false,
        );
        for column in 0..4 {
            assert_eq!(
                pixel(&framebuffer, 10 + column * 14, 100),
                [255, 255, 255, 255]
            );
        }
        // The 0x28 step draws the letter's half of its own cell (cell 4) and
        // leaves the parenthesis' half alone; the 0xF8 0x16 step (cell 5)
        // swaps the two.
        assert_eq!(
            pixel(&framebuffer, 10 + 4 * 14 + 2, 100),
            [255, 255, 255, 255],
            "the letter cell's ink"
        );
        assert_eq!(
            pixel(&framebuffer, 10 + 4 * 14 + 8, 100),
            [0, 0, 0, 0],
            "the parenthesis cell is not sampled"
        );
        assert_eq!(
            pixel(&framebuffer, 10 + 5 * 14 + 2, 100),
            [0, 0, 0, 0],
            "the parenthesis step leaves the letter's half alone"
        );
        assert_eq!(
            pixel(&framebuffer, 10 + 5 * 14 + 8, 100),
            [255, 255, 255, 255],
            "the parenthesis cell's ink"
        );
        assert_eq!(pixel(&framebuffer, 10 + 6 * 14, 100), [0, 0, 0, 0]);
    }

    #[test]
    fn the_lockpick_message_draws_its_l_as_a_letter() {
        // Issue #28: "You have used the lockpick." drew "(ockpick". The item
        // name is encoded through the charset, whose L is the plain 0x28 —
        // the letter cell, never the parenthesis the ASCII path's remap
        // selects. Two disjoint ink strips tell the two cells apart at draw
        // time: the letter covers columns 0..=3, the parenthesis 4..=6.
        let font = Font::new(sheet_with_ink(&[(56, 56, 0, 3), (56, 224, 4, 6)]));
        let mut name = Vec::new();
        crate::charset::encode("Lockpick", crate::charset::Script::Japanese, &mut name);
        assert_eq!(name[0], 0x28, "the charset spells L as 0x28");

        fn pixel(framebuffer: &Framebuffer, x: usize) -> [u8; 4] {
            let offset = x * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        }
        // Brightness 0 is the full-brightness render, so the cell's ink is
        // exactly the sheet's.
        let mut framebuffer = Framebuffer::new();
        font.draw_text(&mut framebuffer, 0, 0, Tint::White, 0, &name, false);
        assert_eq!(
            pixel(&framebuffer, 2),
            [255, 255, 255, 255],
            "the letter's first column is inked"
        );
        assert_eq!(
            pixel(&framebuffer, 5),
            [0, 0, 0, 0],
            "the parenthesis' ink is not sampled"
        );
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
