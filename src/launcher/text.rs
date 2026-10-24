//! The launcher's text: one embedded face rasterised at native resolution.
//!
//! The launcher is deliberately not a game screen: it must draw before a
//! pack exists, so it cannot use the game's font art. `Departure Mono` is a
//! MIT-licensed pixel/terminal face that stays crisp at the integer sizes the
//! 80x25 grid picks. Everything is laid out on that grid, so columns never
//! drift when a line is selected or a status changes.

use std::collections::HashMap;

use anyhow::Result;
use fontdue::{Font, FontSettings};

use super::Color;

/// The embedded face; see `assets/DepartureMono-LICENSE`.
const FACE: &[u8] = include_bytes!("../../assets/DepartureMono-Regular.otf");

/// Sizes tried from largest to smallest. Integer sizes keep the pixel design
/// of the face crisp.
const SIZES: [u32; 12] = [12, 14, 16, 18, 20, 24, 28, 32, 36, 40, 48, 56];

/// Number of character cells in the grid.
pub const COLS: usize = 80;
/// Number of character rows in the grid.
pub const ROWS: usize = 25;

/// One rasterised glyph, positioned relative to the pen and baseline.
struct Glyph {
    width: usize,
    height: usize,
    left: i32,
    top: i32,
    bitmap: Vec<u8>,
}

/// A rasterised face at one size.
pub struct Text {
    font: Font,
    px: u32,
    cell_w: u32,
    cell_h: u32,
    baseline: i32,
    glyphs: HashMap<char, Glyph>,
}

impl Text {
    /// Load the face and pick the largest size that lets the whole 80x25 grid
    /// fit inside `width` x `height` with a small margin.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let font = Font::from_bytes(FACE, FontSettings::default())
            .map_err(|err| anyhow::anyhow!("embedded launcher font: {err}"))?;
        let mut chosen = None;
        for &px in SIZES.iter().rev() {
            let (metrics, _) = font.rasterize('M', px as f32);
            let cell_w = metrics.advance_width.round().max(1.0) as u32;
            let cell_h = (px as f32 * 1.5).round() as u32;
            if COLS as u32 * cell_w <= width * 94 / 100 && ROWS as u32 * cell_h <= height * 94 / 100
            {
                chosen = Some((px, cell_w, cell_h));
                break;
            }
        }
        let (px, cell_w, cell_h) = chosen.unwrap_or_else(|| {
            let px = SIZES[0];
            let (metrics, _) = font.rasterize('M', px as f32);
            let cell_w = metrics.advance_width.round().max(1.0) as u32;
            (px, cell_w, (px as f32 * 1.5).round() as u32)
        });
        let (ascent, descent) = font
            .horizontal_line_metrics(px as f32)
            .map_or(((px as f32 * 0.8) as i32, -(px as f32 * 0.2) as i32), |m| {
                (m.ascent.round() as i32, m.descent.round() as i32)
            });
        let line = ascent - descent;
        let pad = (cell_h as i32 - line).max(0) / 2;
        Ok(Self {
            font,
            px,
            cell_w,
            cell_h,
            baseline: pad + ascent,
            glyphs: HashMap::new(),
        })
    }

    pub fn cell_w(&self) -> u32 {
        self.cell_w
    }

    pub fn cell_h(&self) -> u32 {
        self.cell_h
    }

    /// The baseline for a cell whose top is `top`.
    pub fn baseline(&self, top: i32) -> i32 {
        top + self.baseline
    }

    /// Draw `text` at `x` with its baseline at `baseline`, blending `color`
    /// over the RGBA frame. Characters advance by exactly one grid cell.
    #[allow(clippy::too_many_arguments)] // the frame, pen and style are all needed
    pub fn draw(
        &mut self,
        rgba: &mut [u8],
        width: u32,
        height: u32,
        x: i32,
        baseline: i32,
        text: &str,
        color: Color,
    ) {
        let mut pen = x;
        for ch in text.chars() {
            if ch != ' ' {
                let glyph = self.glyph(ch);
                draw_glyph(rgba, width, height, pen, baseline, glyph, color);
            }
            pen = pen.saturating_add(self.cell_w as i32);
        }
    }

    /// A glyph from the cache, rasterised on first use.
    fn glyph(&mut self, ch: char) -> &Glyph {
        let font = &self.font;
        let px = self.px;
        self.glyphs.entry(ch).or_insert_with(|| {
            let (metrics, bitmap) = font.rasterize(ch, px as f32);
            Glyph {
                width: metrics.width,
                height: metrics.height,
                left: metrics.xmin,
                top: -(metrics.ymin + metrics.height as i32),
                bitmap,
            }
        })
    }
}

/// Blend one glyph bitmap at `x` and `baseline`, clipping to the frame.
fn draw_glyph(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    baseline: i32,
    glyph: &Glyph,
    color: Color,
) {
    if glyph.width == 0 || glyph.height == 0 {
        return;
    }
    let start_x = x + glyph.left;
    let start_y = baseline + glyph.top;
    for row in 0..glyph.height {
        let y = start_y + row as i32;
        if y < 0 || y >= height as i32 {
            continue;
        }
        for col in 0..glyph.width {
            let px = start_x + col as i32;
            if px < 0 || px >= width as i32 {
                continue;
            }
            let coverage = glyph.bitmap[row * glyph.width + col] as u32;
            if coverage == 0 {
                continue;
            }
            let index = (y as usize * width as usize + px as usize) * 4;
            let inverse = 255 - coverage;
            rgba[index] = ((color.r as u32 * coverage + rgba[index] as u32 * inverse) / 255) as u8;
            rgba[index + 1] =
                ((color.g as u32 * coverage + rgba[index + 1] as u32 * inverse) / 255) as u8;
            rgba[index + 2] =
                ((color.b as u32 * coverage + rgba[index + 2] as u32 * inverse) / 255) as u8;
            rgba[index + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grid_fits_a_default_window() {
        let text = Text::new(960, 720).unwrap();
        assert!(COLS as u32 * text.cell_w() <= 960);
        assert!(ROWS as u32 * text.cell_h() <= 720);
        assert!(text.cell_w() > 0);
    }

    #[test]
    fn a_glyph_paints_pixels() {
        let mut text = Text::new(960, 720).unwrap();
        let mut rgba = vec![0u8; 960 * 720 * 4];
        text.draw(
            &mut rgba,
            960,
            720,
            text.cell_w() as i32,
            text.baseline(text.cell_h() as i32),
            "A",
            Color::BODY,
        );
        assert!(
            rgba.as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[..3] != [0, 0, 0])
        );
    }

    #[test]
    fn a_tiny_window_still_produces_a_face() {
        let text = Text::new(320, 240).unwrap();
        assert!(text.cell_w() > 0);
        assert!(text.cell_h() > 0);
    }
}
