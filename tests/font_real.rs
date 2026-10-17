//! The tight per-glyph metrics derived from the shipped font sheet.
//!
//! Needs `ARKLAY_RE1_ROOT` and `ARKLAY_RE1_PACK`; without them the test skips.
//! The expected values are the metric table the issue documents for the
//! MediaKite `FONT.TIM`: the indent is the left trim, the advance the new
//! cursor step.

mod common;

use arklay::font::{Font, GlyphMetric};
use arklay::pack::Pack;

/// Decode and measure the pack's game-text sheet.
fn font(pack: &Pack) -> Font {
    let bytes = pack.read("font/font.tim").expect("font/font.tim");
    let texture = arklay::tim::decode_4bpp(&bytes).expect("decode the sheet");
    Font::new(texture)
}

/// The metric of the plain glyph byte `byte` (the left page grid).
fn plain(font: &Font, byte: u8) -> GlyphMetric {
    let byte = usize::from(byte);
    font.glyph_table.get(0, 2 + byte / 18, byte % 18)
}

/// The metric of an extended glyph page byte and operand.
fn extended(font: &Font, page: u8, operand: u8) -> GlyphMetric {
    let operand = usize::from(operand);
    let (page, row) = match page {
        0xF8 => (0, 15 + operand / 18),
        0xF9 => (1, operand / 18),
        0xFA => (1, 14 + operand / 18),
        _ => panic!("not an extended page byte"),
    };
    font.glyph_table.get(page, row, operand % 18)
}

/// The plain-byte vectors from the issue, in order.
#[rustfmt::skip]
const PLAIN: &[(u8, i32, i32)] = &[
    (0x00, 0, 8), (0x16, 5, 5), (0x17, 2, 5), (0x1A, 5, 4), (0x1B, 3, 8),
    (0x1D, 0, 14), (0x1E, 1, 13), (0x1F, 1, 12), (0x20, 1, 12), (0x21, 1, 12),
    (0x22, 2, 11), (0x23, 1, 13), (0x25, 4, 7), (0x26, 1, 13), (0x27, 1, 13),
    (0x28, 1, 13), (0x2B, 1, 13), (0x2C, 1, 12), (0x2F, 1, 12), (0x30, 1, 13),
    (0x31, 1, 13), (0x32, 1, 13), (0x34, 1, 13), (0x35, 1, 13), (0x36, 1, 13),
    (0x37, 4, 6), (0x39, 4, 6), (0x3A, 5, 5), (0x3B, 1, 12), (0x3D, 2, 10),
    (0x3E, 2, 10), (0x3F, 3, 9), (0x40, 2, 10), (0x41, 3, 9), (0x42, 3, 8),
    (0x43, 2, 10), (0x44, 2, 11), (0x45, 4, 6), (0x46, 4, 6), (0x47, 2, 11),
    (0x48, 4, 6), (0x4A, 2, 11), (0x4B, 2, 10), (0x4C, 2, 10), (0x4D, 2, 10),
    (0x4E, 3, 8), (0x4F, 3, 9), (0x50, 3, 8), (0x51, 2, 11), (0x52, 2, 11),
    (0x54, 2, 11), (0x55, 2, 11), (0x56, 3, 9),
];

/// The extended-glyph vectors from the issue, in order.
#[rustfmt::skip]
const EXTENDED: &[(u8, u8, i32, i32)] = &[
    (0xF8, 0x16, 5, 7), (0xF8, 0x17, 3, 7), (0xF8, 0x1A, 6, 8),
    (0xF8, 0x1B, 1, 8), (0xF8, 0x1C, 2, 5),
];

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn the_shipped_font_derives_the_expected_metrics() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).expect("open the pack");
    let font = font(&pack);

    for &(byte, indent, advance) in PLAIN {
        assert_eq!(
            plain(&font, byte),
            GlyphMetric { indent, advance },
            "plain byte {byte:#04X}"
        );
    }
    for &(page, operand, indent, advance) in EXTENDED {
        assert_eq!(
            extended(&font, page, operand),
            GlyphMetric { indent, advance },
            "extended glyph {page:#04X} {operand:#04X}"
        );
    }
    // The authored rules: the digits keep the full cell; the missing-glyph
    // box is synthetic (never a sheet cell), so it has no vector.
    for byte in 0x0C..=0x15u8 {
        assert_eq!(
            plain(&font, byte),
            GlyphMetric {
                indent: 0,
                advance: 14
            },
            "digit {byte:#04X} must stay monospaced"
        );
    }
}

/// Issue #28: the shipped sheet's `0x28` cell is the letter L, and the
/// "You have used the lockpick." message must draw it, not the round
/// parenthesis at the ASCII renderer's remap target.
#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn the_shipped_lockpick_name_draws_its_letter() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).expect("open the pack");
    let font = font(&pack);
    let text = arklay::text::Text::load(&pack, Some("en-US"));
    let name = text.item_name(0x31).expect("item 0x31's name");
    assert_eq!(name[0], 0x28, "the charset spells the name's L as 0x28");

    // The glyph samples the left page's letter cell (column 4, row 2), not
    // the parenthesis cell the old remap redirected `0x28` to.
    let glyph = arklay::font::Glyphs::new(name, font.metrics)
        .next()
        .expect("the name's first cell");
    assert_eq!((glyph.u, glyph.v), (4 * 14, 28 + 2 * 14));

    // Draw the name at brightness 0 (full brightness, no shadow pass) and
    // compare the first cell's ink with the sheet's own cells: the drawn
    // pixels must be the letter's, and the two cells must differ so the
    // comparison means something.
    let mut framebuffer = arklay::render::Framebuffer::new();
    font.draw_text(
        &mut framebuffer,
        0,
        0,
        arklay::render::Tint::White,
        0,
        name,
        false,
    );
    let drawn: Vec<bool> = (0..14 * 14)
        .map(|cell| {
            let (x, y) = (cell % 14, cell / 14);
            framebuffer.rgba[(y * 320 + x) * 4 + 3] != 0
        })
        .collect();
    let sheet_cell = |u: u32, v: u32| -> Vec<bool> {
        (0..14 * 14)
            .map(|cell| {
                let (x, y) = (cell % 14, cell / 14);
                font.texture.indices[((v + y) * font.texture.width + u + x) as usize] != 0
            })
            .collect()
    };
    let letter = sheet_cell(56, 56);
    let parenthesis = sheet_cell(56, 224);
    assert_ne!(
        letter, parenthesis,
        "the letter and parenthesis cells must differ"
    );
    assert_eq!(drawn, letter, "the first cell must draw the sheet's L");
}
