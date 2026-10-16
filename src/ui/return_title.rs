//! The in-game F9 return-to-title prompt.
//!
//! Pressing F9 during play freezes the room and shows three lines over a
//! dimmed frame; a second F9 confirms the return to the title screen and any
//! other key cancels. The engine drives the prompt like the pause menu - it
//! owns the frozen tick, pauses the game sounds while it is up and resolves
//! the confirm itself - rather than running it as a boxed
//! [`crate::ui::Screen`] modal. The text comes from the language files'
//! `UI_PROMPT_RETURN_TITLE_1..3`; on a sheet whose glyphs are wider than the
//! USA 8px cells the lines wrap to the frame instead of running off the right
//! edge.

use crate::font::{self, Font};
use crate::render::{Framebuffer, Tint};
use crate::text::Text;

use super::UiInput;

/// The full-screen dim drawn over the frozen frame before the text.
const DIM_ALPHA: u8 = 100;
/// Left edge of the prompt's text.
const TEXT_X: i32 = 16;
/// Top edge of each of the prompt's three lines.
const LINE_Y: [i32; 3] = [100, 116, 150];
/// Vertical distance between the rows a line wraps into.
const LINE_SPACING: i32 = 16;
/// The prompt's three lines, in the language files.
const KEYS: [&str; 3] = [
    "UI_PROMPT_RETURN_TITLE_1",
    "UI_PROMPT_RETURN_TITLE_2",
    "UI_PROMPT_RETURN_TITLE_3",
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
    pub fn draw(&self, framebuffer: &mut Framebuffer, font: Option<&Font>, text: &Text) {
        framebuffer.blend_black_rect(
            [0, 0, framebuffer.width as i32, framebuffer.height as i32],
            DIM_ALPHA,
        );
        let Some(font) = font else {
            return;
        };
        let tight = text.uses_tight_metrics();
        let max_width = (framebuffer.width as i32 - 2 * TEXT_X).max(1);
        let mut y = LINE_Y[0];
        for (index, key) in KEYS.iter().enumerate() {
            if index + 1 == LINE_Y.len() {
                y = y.max(LINE_Y[index]);
            }
            for row in font::wrap_pixels(text.ui(key), max_width, font, tight) {
                font.draw_text_plain(framebuffer, TEXT_X, y, Tint::White, 2, row, tight);
                y += LINE_SPACING;
            }
        }
    }
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
    fn the_prompt_text_comes_from_the_language_files() {
        let text = Text::embedded();
        for key in KEYS {
            assert!(!text.ui(key).is_empty(), "{key}");
        }
        assert_eq!(text.selected_language(), "en-US");
    }

    #[test]
    fn wrap_breaks_at_spaces_and_keeps_short_lines() {
        let text = Text::embedded();
        // The USA 8px sheet fits the original lines whole (36 cells max).
        let first = text.ui(KEYS[0]);
        assert_eq!(font::wrap_cells(first, 36), [first]);
        assert_eq!(font::wrap_cells(text.ui(KEYS[1]), 36), [text.ui(KEYS[1])]);
        assert_eq!(font::wrap_cells(text.ui(KEYS[2]), 36), [text.ui(KEYS[2])]);

        // A wider sheet wraps the long line at the last space that fits.
        let rows = font::wrap_cells(first, 20);
        assert_eq!(rows, [&first[..17], &first[18..]]);
    }

    #[test]
    fn draw_dims_the_whole_frame_without_a_font() {
        let mut framebuffer = Framebuffer::new();
        framebuffer.fill_rect([0, 0, 320, 240], [255, 255, 255, 255]);
        ReturnTitlePrompt::new().draw(&mut framebuffer, None, &Text::embedded());
        let pixel = |framebuffer: &Framebuffer, x: usize, y: usize| -> [u8; 4] {
            let offset = (y * 320 + x) * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        };
        // 255 * (255 - 100) / 255 = 155.
        assert_eq!(pixel(&framebuffer, 0, 0), [155, 155, 155, 255]);
        assert_eq!(pixel(&framebuffer, 319, 239), [155, 155, 155, 255]);
    }
}
