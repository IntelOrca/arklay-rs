//! The in-game F9 return-to-title prompt.
//!
//! Pressing F9 during play freezes the room and shows three lines over a
//! dimmed frame; a second F9 confirms the return to the title screen and any
//! other key cancels. The engine drives the prompt like the pause menu - it
//! owns the frozen tick, pauses the game sounds while it is up and resolves
//! the confirm itself - rather than running it as a boxed
//! [`crate::ui::Screen`] modal. The text is the original's USA prompt; on a
//! sheet whose glyphs are wider than the USA 8px cells the lines wrap to the
//! frame instead of running off the right edge.

use crate::font::Font;
use crate::render::{Framebuffer, Tint};

use super::UiInput;
use super::debug_menu::encode_ascii;

/// The full-screen dim drawn over the frozen frame before the text.
const DIM_ALPHA: u8 = 100;
/// Left edge of the prompt's text.
const TEXT_X: i32 = 16;
/// Top edge of each of the prompt's three lines.
const LINE_Y: [i32; 3] = [100, 116, 150];
/// Vertical distance between the rows a line wraps into.
const LINE_SPACING: i32 = 16;
/// The original's USA prompt text.
const LINES: [&str; 3] = [
    "Press F9 to abort game and return to",
    "title screen.",
    "Or any other key to continue game.",
];

/// What one tick of the prompt decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnTitleEvent {
    /// The input was consumed inside the prompt.
    None,
    /// A second F9 confirmed the return to the title screen.
    Confirm,
    /// Any other key cancelled the prompt.
    Cancel,
}

/// The open prompt. It carries no state: the engine owns the freeze and the
/// paused sounds, and the prompt only maps input and draws.
#[derive(Debug, Default)]
pub struct ReturnTitlePrompt;

impl ReturnTitlePrompt {
    /// Open a prompt.
    pub fn new() -> Self {
        Self
    }

    /// One tick of edge-triggered prompt input. The F9 edge wins over the
    /// same tick's `any`, so the confirm press cannot cancel itself.
    pub fn handle_input(&self, input: UiInput) -> ReturnTitleEvent {
        if input.return_title {
            ReturnTitleEvent::Confirm
        } else if input.any {
            ReturnTitleEvent::Cancel
        } else {
            ReturnTitleEvent::None
        }
    }

    /// Dim the frozen frame and draw the three lines. Without a font the dim
    /// still darkens the frame. Lines wider than the frame wrap at spaces;
    /// the third line keeps the original's gap under the shorter first lines.
    pub fn draw(&self, framebuffer: &mut Framebuffer, font: Option<&Font>) {
        framebuffer.blend_black_rect(
            [0, 0, framebuffer.width as i32, framebuffer.height as i32],
            DIM_ALPHA,
        );
        let Some(font) = font else {
            return;
        };
        let max_chars =
            ((framebuffer.width as i32 - 2 * TEXT_X) / font.metrics.glyph_w).max(1) as usize;
        let mut y = LINE_Y[0];
        for (index, line) in LINES.iter().enumerate() {
            if index + 1 == LINE_Y.len() {
                y = y.max(LINE_Y[index]);
            }
            for row in wrap(line, max_chars) {
                let bytes = encode_ascii(row);
                font.draw_text_plain(framebuffer, TEXT_X, y, Tint::White, 2, &bytes);
                y += LINE_SPACING;
            }
        }
    }
}

/// Word-wrap `text` to at most `max_chars` characters per row, breaking at
/// the last space that fits; a word longer than the limit stays on its own
/// row.
fn wrap(text: &str, max_chars: usize) -> Vec<&str> {
    let mut rows = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut cut = (start + max_chars).min(text.len());
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == text.len() {
            rows.push(&text[start..cut]);
            break;
        }
        match text[start..cut].rfind(' ') {
            Some(space) => {
                rows.push(&text[start..start + space]);
                start += space + 1;
            }
            None => {
                rows.push(&text[start..cut]);
                start = cut;
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_maps_confirm_cancel_and_idle() {
        let prompt = ReturnTitlePrompt::new();
        assert_eq!(
            prompt.handle_input(UiInput::default()),
            ReturnTitleEvent::None
        );

        let f9 = UiInput {
            return_title: true,
            any: true,
            ..UiInput::default()
        };
        assert_eq!(prompt.handle_input(f9), ReturnTitleEvent::Confirm);

        let escape = UiInput {
            cancel: true,
            any: true,
            ..UiInput::default()
        };
        assert_eq!(prompt.handle_input(escape), ReturnTitleEvent::Cancel);

        let other = UiInput {
            confirm: true,
            any: true,
            ..UiInput::default()
        };
        assert_eq!(
            prompt.handle_input(other),
            ReturnTitleEvent::Cancel,
            "any other key cancels"
        );
    }

    #[test]
    fn the_lines_encode_to_the_shipped_font_grid() {
        assert_eq!(
            encode_ascii(LINES[0]),
            [
                0x2C, 0x4E, 0x41, 0x4F, 0x4F, 0x00, 0x22, 0x15, 0x00, 0x50, 0x4B, 0x00, 0x3D, 0x3E,
                0x4B, 0x4E, 0x50, 0x00, 0x43, 0x3D, 0x49, 0x41, 0x00, 0x3D, 0x4A, 0x40, 0x00, 0x4E,
                0x41, 0x50, 0x51, 0x4E, 0x4A, 0x00, 0x50, 0x4B,
            ]
        );
        assert_eq!(
            encode_ascii(LINES[1]),
            [
                0x50, 0x45, 0x50, 0x48, 0x41, 0x00, 0x4F, 0x3F, 0x4E, 0x41, 0x41, 0x4A, 0x17
            ]
        );
        assert_eq!(
            encode_ascii(LINES[2]),
            [
                0x2B, 0x4E, 0x00, 0x3D, 0x4A, 0x55, 0x00, 0x4B, 0x50, 0x44, 0x41, 0x4E, 0x00, 0x47,
                0x41, 0x55, 0x00, 0x50, 0x4B, 0x00, 0x3F, 0x4B, 0x4A, 0x50, 0x45, 0x4A, 0x51, 0x41,
                0x00, 0x43, 0x3D, 0x49, 0x41, 0x17,
            ]
        );
    }

    #[test]
    fn wrap_breaks_at_spaces_and_keeps_short_lines() {
        // The USA 8px sheet fits the original lines whole (36 chars max).
        assert_eq!(wrap(LINES[0], 36), ["Press F9 to abort game and return to"]);
        assert_eq!(wrap(LINES[1], 36), ["title screen."]);
        assert_eq!(wrap(LINES[2], 36), ["Or any other key to continue game."]);

        // A wider sheet wraps the long lines at the last space that fits.
        assert_eq!(
            wrap(LINES[0], 20),
            ["Press F9 to abort", "game and return to"]
        );
        assert_eq!(wrap(LINES[1], 20), ["title screen."]);
        assert_eq!(
            wrap(LINES[2], 20),
            ["Or any other key to", "continue game."]
        );
    }

    #[test]
    fn draw_dims_the_whole_frame_without_a_font() {
        let mut framebuffer = Framebuffer::new();
        framebuffer.fill_rect([0, 0, 320, 240], [255, 255, 255, 255]);
        ReturnTitlePrompt::new().draw(&mut framebuffer, None);
        let pixel = |framebuffer: &Framebuffer, x: usize, y: usize| -> [u8; 4] {
            let offset = (y * 320 + x) * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        };
        // 255 * (255 - 100) / 255 = 155.
        assert_eq!(pixel(&framebuffer, 0, 0), [155, 155, 155, 255]);
        assert_eq!(pixel(&framebuffer, 319, 239), [155, 155, 155, 255]);
    }
}
