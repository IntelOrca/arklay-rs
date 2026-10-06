//! The message window: the original's seven-state text machine and its byte
//! grammar.
//!
//! A room or global message is an encoded byte stream. [`MessageWindow`] walks
//! it one step per character delay and remembers how much has been revealed;
//! [`MessageWindow::draw`] re-walks the revealed prefix and paints it through
//! [`crate::font::Font::draw_text`], so the window carries no display list of
//! its own.
//!
//! # Byte grammar
//!
//! | Byte | Meaning |
//! |---|---|
//! | `0x01` | end; the next byte is `0` (wait for input) or a frame count |
//! | `0x02` | newline: `x` resets to the margin and `y += 16` |
//! | `0x03` | page break; five settle passes, then a paged delay operand |
//! | `0x04` | skip embedded tags: reveal jumps to the closing `0x04`, sets the |
//! | | character delay from the operand after it, and the render pass paints |
//! | | the block contents |
//! | `0x05` | select the CLUT colour, operand `0`..`3`/other for the tint |
//! | `0x06` | item-name substitution, operand `0` = the selected item |
//! | `0x07` | return from the item name |
//! | `0x08` | yes/no prompt |
//! | `0xF8`/`0xF9`/`0xFA` | extended glyphs (operand is the glyph index) |
//! | `0x00` | one-cell space |
//! | `0xFB` | no-op |
//! | `0xFF` | half-cell advance |
//!
//! # States
//!
//! `Init` (0) seeds the timers and falls straight into `Reveal` (1), which
//! advances one glyph per character delay. `PageWait` (2) is the blinking ▼
//! after a page break and continues when the action key is pressed. `Delay`
//! (3) is the paged pause that returns to the reveal. `YesNo` (4) draws the
//! ► cursor next to Yes/No and flips the choice with left/right. `WaitInput`
//! (5) and `AutoDismiss` (6) hold the finished text until the action key or a
//! frame count dismisses it.
//!
//! Dismissal clears bit `0x80` of the menu-choice byte and keeps its low bit;
//! on a yes/no confirmation the post-action stream after the `0x08` tag is
//! read as `[no-branch offset][tag][arg]` and dispatched as a
//! [`MessageAction`]: tag `9` chains into another message, tag `10` runs one of
//! the item actions.
//!
//! # Engine hook
//!
//! The engine owns the pack's [`Text`] tables and the active [`RoomState`], so
//! it drives the window through [`crate::game::GameState::update_message`]
//! once per fixed tick, feeding the held keys the same way the room tick does:
//!
//! ```ignore
//! // In the gameplay tick, after the room scripts and entity pass (the
//! // original's `main_loop` order):
//! game.update_message(
//!     MessageInput { action, left: input.left, right: input.right },
//!     &room,
//!     &text,
//! );
//!
//! // In the frame draw, after the gameplay scene is blitted and before the
//! // room transition/fade overlay, so the window is never dimmed:
//! game.message.draw(&mut framebuffer, &font, &text);
//! ```
//!
//! [`crate::game::GameState::show_message`] masks the request's pause word out
//! of [`crate::game::GameState::message_flags`]; while that masks the control
//! bit, [`crate::game::GameState::message_locks_controls`] is true and the
//! engine blanks the player's movement and action input for the tick (the
//! original's cleared d-pad word). Dismissal restores the captured flags.
//! The door-animation adapter arms its message through `show_message` too, so
//! an already-displaying window is refused rather than clobbered. Requesting
//! a second message while one is up is refused, exactly like the original's
//! `set_message_display`.

use crate::font::Font;
use crate::items;
use crate::render::{Framebuffer, Tint};
use crate::text::Text;

/// One tick of held inputs the window reads.
///
/// The window derives the press edges itself, because the original tests both
/// held and freshly pressed pad bits at different points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageInput {
    /// Action/confirm key held.
    pub action: bool,
    /// Left d-pad held.
    pub left: bool,
    /// Right d-pad held.
    pub right: bool,
}

/// The visible phase of [`MessageWindow`]. `Idle` is the Rust-side resting
/// state; the original's seven counters are `Init` through `AutoDismiss`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessagePhase {
    /// No message is displayed.
    #[default]
    Idle,
    /// State 0: seed the timers and fall into the reveal.
    Init,
    /// State 1: character-by-character reveal.
    Reveal,
    /// State 2: page break waiting with the blinking ▼ cursor.
    PageWait,
    /// State 3: paged delay before the reveal resumes.
    Delay,
    /// State 4: the yes/no prompt.
    YesNo,
    /// State 5: text complete, waiting for the action key.
    WaitInput,
    /// State 6: text complete, dismissing after a frame count.
    AutoDismiss,
}

/// A side effect a confirmed yes/no message asks the game to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAction {
    /// Post-action 0: take the room action item the message was armed for.
    TakeItem,
    /// Post-action 1: consume the selected item.
    UseSelectedItem,
    /// Post-action 2: discard the selected item (script-only path).
    DiscardSelectedItem,
    /// Post-action 9: chain into a new message id.
    Chain(u8),
}

/// Position inside the message stream or the active item-name substitution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cursor {
    /// Offset into the message bytes.
    Message(usize),
    /// Offset into the current item-name bytes.
    Name(usize),
}

impl Cursor {
    fn index(self) -> usize {
        match self {
            Cursor::Message(index) | Cursor::Name(index) => index,
        }
    }
}

fn next(cursor: Cursor) -> Cursor {
    match cursor {
        Cursor::Message(index) => Cursor::Message(index + 1),
        Cursor::Name(index) => Cursor::Name(index + 1),
    }
}

fn skip(cursor: Cursor) -> Cursor {
    next(next(cursor))
}

/// Resolve the stream drawn for an item-name substitution.
///
/// Item ids with a real name draw from the name table; the generic-name table
/// stands in for an item the player has not examined yet (ids below `0x4E`
/// whose lookup record has bit `0x80` clear in its name class).
pub fn item_name_bytes(text: &Text, item: u8, examined: &[u8; 4]) -> Vec<u8> {
    if item < 0x4E
        && let Some(record) = items::record(item)
        && !record.name_valid()
        && !examined_bit(examined, record.name_class)
    {
        return text
            .unknown_name(usize::from(record.name_class))
            .unwrap_or(&[])
            .to_vec();
    }
    text.item_name(u16::from(item)).unwrap_or(&[]).to_vec()
}

/// Whether `class` is set in the four-byte examined-item flag bank, using the
/// original's most-significant-bit-first selector.
pub fn examined_bit(examined: &[u8; 4], class: u8) -> bool {
    let bank = u32::from_le_bytes(*examined);
    bank & (0x8000_0000u32 >> (class & 0x1F)) != 0
}

/// The encoded `Yes  No` row the yes/no prompt draws to the right of the ►
/// cursor. Upper and lower case exactly match the original's string.
const YES_NO: &[u8] = &[0x35, 0x41, 0x4F, 0x00, 0x00, 0x2A, 0x4B];

/// Blink period of both cursors: the timer bits the original tests, doubled
/// while the game is inactive.
const BLINK_BITS: u8 = 0x18;

/// The message window state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageWindow {
    /// Message table id, or `None` when no message was requested. Public so
    /// the engine's door-animation adapter can write it directly.
    pub id: Option<u8>,
    /// The pause word passed with the message.
    pub pause: u16,
    /// Whether a message is being displayed. Public for the engine adapter.
    pub active: bool,
    /// The four-byte examined-item bank item-name substitution reads.
    pub examined: [u8; 4],

    phase: MessagePhase,
    bytes: Vec<u8>,
    source_ready: bool,
    ptr: Cursor,
    render_start: usize,
    saved: usize,
    name: Vec<u8>,
    clut_base: u8,
    clut_copy: u8,
    line_counter: u8,
    char_delay: u8,
    char_timer: u8,
    speed_up: bool,
    menu: bool,
    selected_item: u8,
    menu_choice: u8,
    previous: MessageInput,
    actions: Vec<MessageAction>,
}

impl Default for MessageWindow {
    fn default() -> Self {
        Self {
            id: None,
            pause: 0,
            active: false,
            examined: [0; 4],
            phase: MessagePhase::Idle,
            bytes: Vec::new(),
            source_ready: false,
            ptr: Cursor::Message(0),
            render_start: 0,
            saved: 0,
            name: Vec::new(),
            clut_base: 0,
            clut_copy: 0,
            line_counter: 0,
            char_delay: 1,
            char_timer: 0,
            speed_up: false,
            menu: false,
            selected_item: 0,
            menu_choice: 0,
            previous: MessageInput::default(),
            actions: Vec::new(),
        }
    }
}

impl MessageWindow {
    /// The menu-choice byte the scripts read with `cmpb 5`: bit `0x80` while a
    /// message is up, bit `0` the yes/no answer.
    pub fn menu_choice_id(&self) -> u8 {
        self.menu_choice
    }

    /// Overwrite the menu-choice byte, e.g. when a script writes state byte 5.
    pub fn set_menu_choice_id(&mut self, value: u8) {
        self.menu_choice = value;
    }

    /// Whether the message source bytes have been resolved yet.
    pub fn has_source(&self) -> bool {
        self.source_ready
    }

    /// The resolved message bytes, for tests and the real-asset check.
    pub fn source(&self) -> &[u8] {
        &self.bytes
    }

    /// The current phase.
    pub fn phase(&self) -> MessagePhase {
        self.phase
    }

    /// Whether the window was requested by the pause menu: its text draws on
    /// the menu line and its reveal runs at half speed.
    pub fn uses_menu_position(&self) -> bool {
        self.menu
    }

    /// Request a message: the original's `set_message_display` head.
    ///
    /// Refuses while a message is up (bit `0x80` of the menu-choice byte set),
    /// stores the id and pause word and marks the menu byte. The encoded bytes
    /// arrive with [`MessageWindow::feed_source`] once the caller has looked
    /// the message up in the [`Text`]/[`crate::state::RoomState`] tables.
    pub fn request(&mut self, id: u8, pause: u16, menu: bool) -> bool {
        if self.menu_choice & 0x80 != 0 {
            return false;
        }
        self.menu_choice = 0x80;
        self.id = Some(id);
        self.pause = pause;
        self.active = true;
        self.menu = menu;
        self.speed_up = id & 0x80 != 0;
        self.source_ready = false;
        self.phase = MessagePhase::Init;
        self.bytes.clear();
        self.name.clear();
        self.ptr = Cursor::Message(0);
        self.render_start = 0;
        self.saved = 0;
        self.clut_base = 0;
        self.clut_copy = 0;
        self.line_counter = 0;
        self.char_delay = 0;
        self.char_timer = 0;
        self.actions.clear();
        true
    }

    /// Arm a message and its encoded bytes directly. Refuses exactly like
    /// [`MessageWindow::request`].
    pub fn start(&mut self, id: u8, pause: u16, bytes: &[u8], menu: bool) -> bool {
        if !self.request(id, pause, menu) {
            return false;
        }
        self.feed_source(bytes);
        true
    }

    /// Resolve the pending request's bytes. A message id with no stream reads
    /// as an empty message and dismisses.
    pub fn feed_source(&mut self, bytes: &[u8]) {
        self.bytes.clear();
        self.bytes.extend_from_slice(bytes);
        self.source_ready = !bytes.is_empty();
        if !self.source_ready {
            self.dismiss();
        }
    }

    /// Advance the window one fixed tick.
    pub fn update(&mut self, input: MessageInput, text: &Text, selected_item: u8) {
        // The engine's door-animation adapter writes `id`/`pause`/`active`
        // directly; turn that into a real request on the next tick.
        if self.active
            && !self.source_ready
            && self.phase == MessagePhase::Idle
            && self.menu_choice & 0x80 == 0
            && let Some(id) = self.id
        {
            let pause = self.pause;
            let menu = self.menu;
            self.request(id, pause, menu);
        }
        if !self.active || !self.source_ready {
            self.previous = input;
            return;
        }

        self.selected_item = selected_item;
        let pressed_action = input.action && !self.previous.action;
        let pressed_left = input.left && !self.previous.left;
        let pressed_right = input.right && !self.previous.right;
        let shifted = self.menu;

        match self.phase {
            MessagePhase::Idle => {}
            MessagePhase::Init => {
                self.char_delay = if shifted { 2 } else { 1 };
                self.char_timer = self.char_delay;
                self.phase = MessagePhase::Reveal;
                self.update_reveal(input, text, shifted);
            }
            MessagePhase::Reveal => self.update_reveal(input, text, shifted),
            MessagePhase::PageWait => {
                if pressed_action {
                    self.render_start = match self.ptr {
                        Cursor::Message(index) => index,
                        Cursor::Name(_) => self.render_start,
                    };
                    self.clut_base = self.clut_copy;
                    self.phase = MessagePhase::Reveal;
                    self.char_timer = if shifted { 2 } else { 1 };
                } else {
                    self.char_timer = self.char_timer.wrapping_sub(1);
                }
            }
            MessagePhase::Delay => {
                self.char_timer = self.char_timer.wrapping_sub(1);
                if self.char_timer == 0 {
                    self.phase = MessagePhase::Reveal;
                    self.render_start = match self.ptr {
                        Cursor::Message(index) => index,
                        Cursor::Name(_) => self.render_start,
                    };
                    self.clut_base = self.clut_copy;
                    self.char_timer = self.char_delay << u8::from(shifted);
                }
            }
            MessagePhase::YesNo => {
                if pressed_action {
                    self.menu_choice &= 0x7F;
                    self.active = false;
                    self.phase = MessagePhase::Idle;
                    self.run_post_action();
                } else {
                    if pressed_left || pressed_right {
                        self.menu_choice ^= 1;
                        self.char_timer = 0;
                    }
                    self.char_timer = self.char_timer.wrapping_sub(1);
                }
            }
            MessagePhase::WaitInput => {
                if pressed_action {
                    self.menu_choice &= 0x7F;
                    self.active = false;
                    self.phase = MessagePhase::Idle;
                }
            }
            MessagePhase::AutoDismiss => {
                self.char_timer = self.char_timer.wrapping_sub(1);
                if self.char_timer == 0 {
                    self.menu_choice &= 0x7F;
                    self.active = false;
                    self.phase = MessagePhase::Idle;
                }
            }
        }
        self.previous = input;
    }

    /// The state-1 reveal step: decrement the character timer with the
    /// speed-up rule, then process stream bytes until one consumes the tick.
    fn update_reveal(&mut self, input: MessageInput, text: &Text, shifted: bool) {
        let previous = self.char_timer;
        self.char_timer = self.char_timer.wrapping_sub(1);
        if self.speed_up && self.char_timer != 0 {
            if input.action {
                self.char_timer = previous.wrapping_sub(2);
            }
            if self.char_timer != 0 {
                return;
            }
        } else if !self.speed_up && self.char_timer != 0 {
            return;
        }

        loop {
            let Some(byte) = self.peek(self.ptr) else {
                self.dismiss();
                return;
            };
            match byte {
                0x00 => {
                    self.ptr = next(self.ptr);
                    self.char_timer = self.char_delay;
                    return;
                }
                0x01 => {
                    self.ptr = next(self.ptr);
                    let action = self.peek(self.ptr).unwrap_or(0);
                    if action == 0 {
                        self.phase = MessagePhase::WaitInput;
                    } else {
                        self.phase = MessagePhase::AutoDismiss;
                        self.char_timer = if shifted {
                            action.wrapping_mul(2)
                        } else {
                            action
                        };
                    }
                    return;
                }
                0x02 => self.ptr = next(self.ptr),
                0x03 => {
                    let line = self.line_counter;
                    self.line_counter = line.wrapping_add(1);
                    if line < 5 {
                        self.char_timer = self.char_timer.wrapping_add(1);
                        return;
                    }
                    self.line_counter = 0;
                    self.ptr = next(self.ptr);
                    let operand = self.peek(self.ptr).unwrap_or(0);
                    self.ptr = next(self.ptr);
                    if operand == 0 {
                        self.phase = MessagePhase::PageWait;
                    } else {
                        self.phase = MessagePhase::Delay;
                        self.char_timer = if shifted {
                            operand.wrapping_mul(2)
                        } else {
                            operand
                        };
                    }
                    return;
                }
                0x04 => {
                    self.ptr = next(self.ptr);
                    let operand = self.peek(self.ptr).unwrap_or(0);
                    if operand == 0 {
                        self.ptr = next(self.ptr);
                        loop {
                            match self.peek(self.ptr) {
                                Some(0x04) => {
                                    self.ptr = next(self.ptr);
                                    break;
                                }
                                Some(0x05 | 0x06 | 0xF8..=0xFA) => self.ptr = skip(self.ptr),
                                Some(_) => self.ptr = next(self.ptr),
                                None => break,
                            }
                        }
                        let delay = self.peek(self.ptr).unwrap_or(0);
                        self.char_delay = if shifted {
                            delay.wrapping_mul(2)
                        } else {
                            delay
                        };
                        self.ptr = next(self.ptr);
                    } else {
                        self.char_delay = if shifted {
                            operand.wrapping_mul(2)
                        } else {
                            operand
                        };
                        self.ptr = next(self.ptr);
                    }
                }
                0x05 => {
                    self.ptr = next(self.ptr);
                    self.clut_copy = self.peek(self.ptr).unwrap_or(0);
                    self.ptr = next(self.ptr);
                }
                0x06 => {
                    let saved = self.ptr.index();
                    self.ptr = next(self.ptr);
                    let argument = self.peek(self.ptr).unwrap_or(0);
                    self.ptr = next(self.ptr);
                    let item = if argument == 0 {
                        self.selected_item
                    } else {
                        argument
                    };
                    self.saved = saved;
                    self.name = item_name_bytes(text, item, &self.examined);
                    if !self.name.is_empty() {
                        self.ptr = Cursor::Name(0);
                    }
                }
                0x07 => {
                    self.ptr = match self.ptr {
                        Cursor::Name(_) => Cursor::Message(self.saved + 2),
                        Cursor::Message(_) => next(self.ptr),
                    };
                }
                0x08 => {
                    self.phase = MessagePhase::YesNo;
                    return;
                }
                0xF8..=0xFA => {
                    self.ptr = skip(self.ptr);
                    self.char_timer = self.char_delay;
                    return;
                }
                0xFB => self.ptr = next(self.ptr),
                0xFF => {
                    self.ptr = next(self.ptr);
                    self.char_timer = self.char_delay;
                    return;
                }
                _ => {
                    self.ptr = next(self.ptr);
                    self.char_timer = self.char_delay;
                    return;
                }
            }
        }
    }

    /// Read the post-action stream after a confirmed yes/no prompt.
    ///
    /// The layout is `[no-branch offset][tag][arg]`: the offset skips to the
    /// No branch's own tag, the Yes branch reads the tag immediately after the
    /// offset. Tag `9` chains into the argument id, tag `10` dispatches the
    /// item action the argument names.
    fn run_post_action(&mut self) {
        let Cursor::Message(position) = self.ptr else {
            return;
        };
        let start = position + 1;
        let no_offset = usize::from(self.bytes.get(start).copied().unwrap_or(0));
        let branch = if self.menu_choice & 1 != 0 {
            no_offset
        } else {
            0
        };
        let tag = self.bytes.get(start + branch + 1).copied().unwrap_or(0);
        let argument = self.bytes.get(start + branch + 2).copied().unwrap_or(0);
        match tag {
            9 => self.actions.push(MessageAction::Chain(argument)),
            10 => match argument {
                0 => self.actions.push(MessageAction::TakeItem),
                1 => self.actions.push(MessageAction::UseSelectedItem),
                2 => self.actions.push(MessageAction::DiscardSelectedItem),
                _ => {}
            },
            _ => {}
        }
    }

    /// Take the side effects queued since the last call.
    pub fn take_actions(&mut self) -> Vec<MessageAction> {
        std::mem::take(&mut self.actions)
    }

    /// Clear the message and leave the menu-choice byte's low bit alone.
    fn dismiss(&mut self) {
        self.menu_choice &= 0x7F;
        self.active = false;
        self.phase = MessagePhase::Idle;
    }

    fn peek(&self, cursor: Cursor) -> Option<u8> {
        match cursor {
            Cursor::Message(index) => self.bytes.get(index).copied(),
            Cursor::Name(index) => self.name.get(index).copied(),
        }
    }

    /// Render the window into `framebuffer`.
    ///
    /// The text starts at the font's left margin on the room line and follows
    /// `0x02` newlines down; the page and yes/no cursors blink on the timer's
    /// own bits. The walk mirrors the original's render pass, so `0x04` blocks
    /// paint their contents and item names paint in the colour `0x05` selected
    /// (the item-name sequence is always bracketed with the green operand).
    ///
    /// The engine calls this after the gameplay scene and before fades.
    pub fn draw(&self, framebuffer: &mut Framebuffer, font: &Font, text: &Text) {
        if !self.active || !self.source_ready {
            return;
        }
        let metrics = font.metrics;
        let glyph_w = metrics.glyph_w;
        let base_y = if self.menu {
            metrics.menu_y
        } else {
            metrics.text_y
        };
        let mut x = metrics.left_margin;
        let mut y = base_y;
        let mut tint = Tint::from_clut_row(i32::from(self.clut_base));
        let mut cursor = Cursor::Message(self.render_start);
        let mut saved = 0usize;
        let mut name: Vec<u8> = Vec::new();

        while cursor != self.ptr {
            let Some(byte) = self.peek(cursor) else {
                break;
            };
            match byte {
                0x00 => {
                    x += glyph_w;
                    cursor = next(cursor);
                }
                0x01 => cursor = next(cursor),
                0x02 => {
                    x = metrics.left_margin;
                    y += 16;
                    cursor = next(cursor);
                }
                0x03 | 0x04 => cursor = skip(cursor),
                0x05 => {
                    tint = Tint::from_clut_row(i32::from(self.peek(next(cursor)).unwrap_or(0)));
                    cursor = skip(cursor);
                }
                0x06 => {
                    let tag = cursor.index();
                    let argument = self.peek(next(cursor)).unwrap_or(0);
                    let item = if argument == 0 {
                        self.selected_item
                    } else {
                        argument
                    };
                    name = item_name_bytes(text, item, &self.examined);
                    saved = tag;
                    cursor = skip(cursor);
                    if !name.is_empty() {
                        cursor = Cursor::Name(0);
                    }
                }
                0x07 => {
                    cursor = match cursor {
                        Cursor::Name(_) => Cursor::Message(saved + 2),
                        Cursor::Message(_) => next(cursor),
                    };
                }
                0xF8..=0xFA => {
                    self.draw_step(framebuffer, font, x, y, tint, &name, cursor, 2);
                    x += glyph_w;
                    cursor = skip(cursor);
                }
                0xFB => cursor = next(cursor),
                0xFF => {
                    x += glyph_w / 2;
                    cursor = next(cursor);
                }
                _ => {
                    self.draw_step(framebuffer, font, x, y, tint, &name, cursor, 1);
                    x += glyph_w;
                    cursor = next(cursor);
                }
            }
        }

        let blink = BLINK_BITS << u8::from(self.menu);
        match self.phase {
            MessagePhase::PageWait => {
                if self.char_timer & blink != 0 {
                    font.draw_text(framebuffer, 0x99, base_y + 0x1E, Tint::White, 2, &[0x0B]);
                }
            }
            MessagePhase::YesNo => {
                let cursor_x = if glyph_w > 8 { 0xA0 } else { 0xD0 };
                if self.char_timer & blink != 0 {
                    let x = if self.menu_choice & 1 == 0 {
                        cursor_x
                    } else {
                        cursor_x + 5 * glyph_w
                    };
                    font.draw_text(framebuffer, x, base_y + 16, Tint::White, 2, &[0x02]);
                }
                font.draw_text(
                    framebuffer,
                    cursor_x + glyph_w,
                    base_y + 16,
                    Tint::White,
                    2,
                    YES_NO,
                );
            }
            _ => {}
        }
    }

    /// Draw one decoded step at `(x, y)`, reading the bytes from the active
    /// cursor source.
    #[allow(clippy::too_many_arguments)]
    fn draw_step(
        &self,
        framebuffer: &mut Framebuffer,
        font: &Font,
        x: i32,
        y: i32,
        tint: Tint,
        name: &[u8],
        cursor: Cursor,
        len: usize,
    ) {
        let index = cursor.index();
        let slice = match cursor {
            Cursor::Message(_) => self.bytes.get(index..index + len),
            Cursor::Name(_) => name.get(index..index + len),
        }
        .unwrap_or(&[]);
        font.draw_text(framebuffer, x, y, tint, 2, slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PALETTE_ROW_LEN, Texture8};
    use crate::pack::{Pack, PackWriter};

    /// A 768x256 sheet with a white cell at every `(u, v)`.
    fn sheet(cells: &[(i32, i32)]) -> Texture8 {
        let width = 768u32;
        let height = 256u32;
        let mut indices = vec![0u8; (width * height) as usize];
        for &(u, v) in cells {
            for row in v..v + crate::font::GLYPH_HEIGHT {
                for column in u..u + 14 {
                    indices[(row as u32 * width + column as u32) as usize] = 1;
                }
            }
        }
        let mut palettes = vec![[0u8; 4]; PALETTE_ROW_LEN];
        palettes[1] = [255, 255, 255, 255];
        Texture8 {
            width,
            height,
            indices,
            palettes,
            stp: Vec::new(),
        }
    }

    /// The plain-grid cell the font decoder gives byte `byte`.
    fn cell(byte: u8) -> (i32, i32) {
        let byte = i32::from(byte);
        ((byte % 18) * 14, 28 + (byte / 18) * 14)
    }

    /// A font whose sheet carries a white cell for every byte in `bytes`.
    fn font_for(bytes: &[u8]) -> Font {
        let cells: Vec<(i32, i32)> = bytes.iter().map(|&byte| cell(byte)).collect();
        Font::new(sheet(&cells))
    }

    fn text_tables(names: &[&[u8]], unknown: &[&[u8]]) -> Text {
        let table = |entries: &[&[u8]]| {
            let entries: Vec<Option<Vec<u8>>> = entries
                .iter()
                .map(|entry| (!entry.is_empty()).then(|| entry.to_vec()))
                .collect();
            crate::text::encode_table(&entries)
        };
        let mut writer = PackWriter::new();
        writer.add("text/names.bin", table(names)).unwrap();
        writer.add("text/unknown.bin", table(unknown)).unwrap();
        writer
            .add(
                "text/messages.bin",
                crate::text::encode_table(&[Some(vec![0x0C, 0x01, 0x00])]),
            )
            .unwrap();
        writer
            .add("text/idesc.bin", crate::text::encode_table(&[]))
            .unwrap();
        writer
            .add("text/save.bin", crate::text::encode_table(&[]))
            .unwrap();
        Text::load(&Pack::from_bytes(writer.to_bytes().unwrap()).unwrap())
    }

    fn text() -> Text {
        text_tables(&[], &[])
    }

    fn pixel(framebuffer: &Framebuffer, x: i32, y: i32) -> [u8; 4] {
        let offset = (y as usize * 320 + x as usize) * 4;
        framebuffer.rgba[offset..offset + 4].try_into().unwrap()
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xCBF2_9CE4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }

    fn held(action: bool) -> MessageInput {
        MessageInput {
            action,
            ..MessageInput::default()
        }
    }

    fn tick(window: &mut MessageWindow, text: &Text, input: MessageInput) {
        window.update(input, text, window.selected_item);
    }

    /// Reveal until the window falls out of `Reveal`/`Init` (or a cap).
    fn reveal_all(window: &mut MessageWindow, text: &Text) {
        for _ in 0..10_000 {
            match window.phase() {
                MessagePhase::Init | MessagePhase::Reveal => tick(window, text, held(false)),
                _ => return,
            }
        }
        panic!("message never left the reveal");
    }

    #[test]
    fn request_refuses_while_active_and_stores_the_pause_word() {
        let text = text_tables(&[], &[]);
        let mut window = MessageWindow::default();
        assert!(window.start(0x2A, 79, &[0x0C, 0x01, 0x00], false));
        assert_eq!(window.id, Some(0x2A));
        assert_eq!(window.pause, 79);
        assert!(window.active);
        assert_eq!(window.menu_choice_id(), 0x80);

        assert!(!window.start(0x2B, 3, &[0x0D, 0x01, 0x00], false));
        assert_eq!(window.id, Some(0x2A), "the active message is untouched");
        assert_eq!(window.pause, 79);

        reveal_all(&mut window, &text);
        assert_eq!(window.phase(), MessagePhase::WaitInput);
        tick(&mut window, &text, held(false));
        tick(&mut window, &text, held(true));
        assert!(
            window.start(0x2B, 3, &[0x0D, 0x01, 0x00], false),
            "a dismissed message releases the menu byte"
        );
        assert_eq!(window.menu_choice_id(), 0x80);
    }

    #[test]
    fn reveal_releases_one_glyph_per_character_delay() {
        let bytes = [0x0C, 0x0D, 0x0E, 0x01, 0x00];
        let text = text();
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &bytes, false));

        tick(&mut window, &text, held(false));
        assert_eq!(window.ptr, Cursor::Message(1));
        assert_eq!(window.char_timer, 1);
        tick(&mut window, &text, held(false));
        assert_eq!(window.ptr, Cursor::Message(2));
        tick(&mut window, &text, held(false));
        assert_eq!(window.ptr, Cursor::Message(3));
        tick(&mut window, &text, held(false));
        assert_eq!(window.phase(), MessagePhase::WaitInput);
        assert_eq!(window.ptr, Cursor::Message(4), "the action byte is current");
    }

    #[test]
    fn menu_messages_reveal_at_half_speed_and_speed_up_with_action() {
        let bytes = [0x0C, 0x0D, 0x01, 0x00];
        let text = text();

        let mut slow = MessageWindow::default();
        assert!(slow.start(1, 0, &bytes, true));
        tick(&mut slow, &text, held(false));
        assert_eq!(slow.ptr, Cursor::Message(0), "delay 2 waits one tick");
        tick(&mut slow, &text, held(false));
        assert_eq!(slow.ptr, Cursor::Message(1));

        let mut fast = MessageWindow::default();
        assert!(fast.start(0x81, 0, &bytes, true));
        tick(&mut fast, &text, held(true));
        assert_eq!(fast.ptr, Cursor::Message(1), "speed-up while action held");
        tick(&mut fast, &text, held(true));
        assert_eq!(fast.ptr, Cursor::Message(2));
    }

    #[test]
    fn newline_advances_to_the_next_row_without_spending_a_tick() {
        let font = font_for(&[0x0C]);
        let text = text();
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &[0x0C, 0x02, 0x0C, 0x01, 0x00], false));

        let mut framebuffer = Framebuffer::new();
        tick(&mut window, &text, held(false));
        window.draw(&mut framebuffer, &font, &text);
        assert_eq!(pixel(&framebuffer, 0x22, 181), [255, 255, 255, 255]);
        assert_eq!(pixel(&framebuffer, 0x22, 197), [0, 0, 0, 0]);

        tick(&mut window, &text, held(false));
        let mut framebuffer = Framebuffer::new();
        window.draw(&mut framebuffer, &font, &text);
        assert_eq!(pixel(&framebuffer, 0x22, 181), [255, 255, 255, 255]);
        assert_eq!(pixel(&framebuffer, 0x22, 197), [255, 255, 255, 255]);
    }

    #[test]
    fn page_break_settles_for_five_lines_then_waits_for_action() {
        let text = text();
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &[0x0C, 0x03, 0x00, 0x0D, 0x01, 0x00], false));

        tick(&mut window, &text, held(false));
        assert_eq!(window.ptr, Cursor::Message(1));
        for line in 0..5 {
            tick(&mut window, &text, held(false));
            assert_eq!(window.line_counter, line + 1, "settle pass {line}");
            assert_eq!(window.phase(), MessagePhase::Reveal);
        }
        tick(&mut window, &text, held(false));
        assert_eq!(window.phase(), MessagePhase::PageWait);
        assert_eq!(window.ptr, Cursor::Message(3));

        // The action key resumes the page and the reveal restarts from it.
        for _ in 0..10 {
            tick(&mut window, &text, held(false));
            assert_eq!(window.phase(), MessagePhase::PageWait);
        }
        tick(&mut window, &text, held(true));
        assert_eq!(window.phase(), MessagePhase::Reveal);
        assert_eq!(window.render_start, 3);
        assert_eq!(window.ptr, Cursor::Message(3), "resuming spends a tick");
        tick(&mut window, &text, held(false));
        assert_eq!(window.ptr, Cursor::Message(4));
    }

    #[test]
    fn page_break_with_an_operand_delays_then_continues() {
        let text = text();
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &[0x0C, 0x03, 0x04, 0x0D, 0x01, 0x00], false));
        reveal_all(&mut window, &text);

        // `reveal_all` stops at Delay; four more ticks finish the delay.
        assert_eq!(window.phase(), MessagePhase::Delay);
        assert_eq!(window.char_timer, 4);
        for _ in 0..3 {
            tick(&mut window, &text, held(false));
            assert_eq!(window.phase(), MessagePhase::Delay);
        }
        tick(&mut window, &text, held(false));
        assert_eq!(window.phase(), MessagePhase::Reveal);
        assert_eq!(window.render_start, 3);
    }

    #[test]
    fn skip_block_jumps_to_the_closing_tag_and_sets_the_character_delay() {
        let text = text();
        let bytes = [0x04, 0x00, 0x0C, 0x02, 0x04, 0x03, 0x0D, 0x01, 0x00];
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &bytes, false));

        tick(&mut window, &text, held(false));
        assert_eq!(window.char_delay, 3);
        assert_eq!(
            window.ptr,
            Cursor::Message(7),
            "the block's text is visible"
        );

        // A nonzero operand sets the delay directly.
        let mut direct = MessageWindow::default();
        assert!(direct.start(1, 0, &[0x04, 0x02, 0x0D, 0x01, 0x00], false));
        tick(&mut direct, &text, held(false));
        assert_eq!(direct.char_delay, 2);
        assert_eq!(direct.ptr, Cursor::Message(3));
    }

    #[test]
    fn color_tag_changes_the_tint_and_item_names_stay_green() {
        let font = font_for(&[0x0C, 0x0D]);
        let text = text_tables(&[&[0x0C, 0x07]], &[]);
        let bytes = [0x05, 0x01, 0x0C, 0x05, 0x03, 0x0D, 0x01, 0x00];
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &bytes, false));
        reveal_all(&mut window, &text);

        let mut framebuffer = Framebuffer::new();
        window.draw(&mut framebuffer, &font, &text);
        assert_eq!(pixel(&framebuffer, 0x22, 181), [0, 255, 0, 255], "green");
        assert_eq!(
            pixel(&framebuffer, 0x22 + 14, 181),
            [204, 204, 204, 255],
            "grey"
        );
    }

    #[test]
    fn item_name_substitution_resolves_and_returns() {
        let name = [0xAA, 0xBB, 0x07];
        let text = text_tables(&[&name], &[]);
        let bytes = [0x05, 0x01, 0x06, 0x00, 0x05, 0x00, 0x0C, 0x01, 0x00];
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &bytes, false));
        window.selected_item = 1;

        // 05, 06 -> the name starts on the first tick.
        window.update(held(false), &text, 1);
        assert_eq!(window.name, name);
        assert_eq!(window.ptr, Cursor::Name(1));
        window.update(held(false), &text, 1);
        assert_eq!(window.ptr, Cursor::Name(2));
        window.update(held(false), &text, 1);
        assert_eq!(
            window.ptr,
            Cursor::Message(7),
            "0x07 returns past the 06 operand and the reveal continues"
        );

        // A nonzero operand selects the id directly, ignoring the cursor.
        let mut explicit = MessageWindow::default();
        assert!(explicit.start(1, 0, &[0x06, 0x01, 0x0C, 0x01, 0x00], false));
        explicit.update(held(false), &text, 9);
        assert_eq!(explicit.name, name);
    }

    #[test]
    fn unexamined_items_substitute_the_generic_name() {
        let item = (1..=items::MAX_ITEM)
            .find(|&item| items::record(item).is_some_and(|record| !record.name_valid()))
            .expect("an item with a generic name class");
        let class = usize::from(items::record(item).unwrap().name_class);
        let mut unknown: Vec<Vec<u8>> = (0..16).map(|_| Vec::new()).collect();
        unknown[class] = vec![0xCC, 0x07];
        let unknown_refs: Vec<&[u8]> = unknown.iter().map(Vec::as_slice).collect();
        let mut names: Vec<Vec<u8>> = vec![Vec::new(); usize::from(item)];
        names[usize::from(item) - 1] = vec![0xAA, 0x07];
        let name_refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
        let text = text_tables(&name_refs, &unknown_refs);

        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &[0x06, item, 0x07, 0x01, 0x00], false));
        window.update(held(false), &text, 99);
        assert_eq!(window.name, vec![0xCC, 0x07]);

        // Examining the class makes the real name appear.
        let examined_flags = (0x8000_0000u32 >> (class & 0x1F)).to_le_bytes();
        let mut examined = MessageWindow {
            examined: examined_flags,
            ..MessageWindow::default()
        };
        assert!(examined.start(1, 0, &[0x06, item, 0x07, 0x01, 0x00], false));
        examined.update(held(false), &text, 99);
        assert_eq!(examined.name, vec![0xAA, 0x07]);

        // The direct item-name lookup (menu, item box, viewer) reads the same
        // bank: generic before, real after.
        assert_eq!(
            crate::ui::main_menu::item_name_bytes(&text, item, &[0; 4]),
            Some([0xCC, 0x07].as_slice())
        );
        assert_eq!(
            crate::ui::main_menu::item_name_bytes(&text, item, &examined_flags),
            Some([0xAA, 0x07].as_slice())
        );
    }

    #[test]
    fn yes_no_flips_with_left_and_right_and_keeps_the_answer() {
        let text = text();
        let bytes = [0x0C, 0x08, 0x00, 0x01, 0x00];
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &bytes, false));
        reveal_all(&mut window, &text);
        assert_eq!(window.phase(), MessagePhase::YesNo);
        assert_eq!(window.menu_choice_id(), 0x80, "yes is the default");

        window.update(
            MessageInput {
                left: true,
                ..MessageInput::default()
            },
            &text,
            0,
        );
        assert_eq!(window.menu_choice_id(), 0x81, "left selects No");
        window.update(
            MessageInput {
                right: true,
                ..MessageInput::default()
            },
            &text,
            0,
        );
        assert_eq!(window.menu_choice_id(), 0x80, "right selects Yes");

        // Pressing twice flips to No, then confirm keeps the low bit.
        window.update(
            MessageInput {
                left: true,
                ..MessageInput::default()
            },
            &text,
            0,
        );
        window.update(MessageInput::default(), &text, 0);
        window.update(held(true), &text, 0);
        assert!(!window.active);
        assert_eq!(window.menu_choice_id(), 1);
        assert!(window.take_actions().is_empty());
    }

    #[test]
    fn wait_for_input_holds_until_the_action_key_is_pressed() {
        let text = text();
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &[0x0C, 0x01, 0x00], false));
        // Holding the key through the reveal must not dismiss the message.
        for _ in 0..10 {
            tick(&mut window, &text, held(true));
        }
        assert_eq!(window.phase(), MessagePhase::WaitInput);
        assert!(window.active);

        tick(&mut window, &text, held(false));
        tick(&mut window, &text, held(true));
        assert!(!window.active);
        assert_eq!(window.phase(), MessagePhase::Idle);
        assert_eq!(window.menu_choice_id(), 0);
    }

    #[test]
    fn auto_dismiss_counts_down_the_action_frames() {
        let text = text();
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &[0x0C, 0x01, 0x03], false));
        reveal_all(&mut window, &text);
        assert_eq!(window.phase(), MessagePhase::AutoDismiss);
        assert_eq!(window.char_timer, 3);

        tick(&mut window, &text, held(false));
        tick(&mut window, &text, held(false));
        assert!(window.active);
        tick(&mut window, &text, held(false));
        assert!(!window.active);
        assert_eq!(window.menu_choice_id(), 0);
    }

    #[test]
    fn auto_dismiss_doubles_while_the_game_is_inactive() {
        let text = text();
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &[0x0C, 0x01, 0x03], true));
        reveal_all(&mut window, &text);
        assert_eq!(window.char_timer, 6);
    }

    #[test]
    fn extended_glyphs_spend_their_operand_and_one_tick() {
        let font = Font::new(sheet(&[cell(0x0C), (256 + 28, 14)]));
        let text = text();
        let bytes = [0xF9, 20, 0x01, 0x00];
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &bytes, false));
        tick(&mut window, &text, held(false));
        assert_eq!(window.ptr, Cursor::Message(2));
        let mut framebuffer = Framebuffer::new();
        window.draw(&mut framebuffer, &font, &text);
        assert_eq!(pixel(&framebuffer, 0x22, 181), [255, 255, 255, 255]);
    }

    #[test]
    fn post_action_nine_chains_on_each_branch() {
        let text = text();
        let bytes = [0x0C, 0x08, 0x02, 0x09, 0x22, 0x09, 0x33];
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &bytes, false));
        reveal_all(&mut window, &text);
        window.update(held(true), &text, 0);
        assert_eq!(window.take_actions(), vec![MessageAction::Chain(0x22)]);

        let mut no = MessageWindow::default();
        assert!(no.start(1, 0, &bytes, false));
        reveal_all(&mut no, &text);
        no.update(
            MessageInput {
                left: true,
                ..MessageInput::default()
            },
            &text,
            0,
        );
        no.update(MessageInput::default(), &text, 0);
        no.update(held(true), &text, 0);
        assert_eq!(no.take_actions(), vec![MessageAction::Chain(0x33)]);
    }

    #[test]
    fn post_action_ten_dispatches_the_item_actions() {
        let text = text();
        for (case, expected) in [
            (0u8, MessageAction::TakeItem),
            (1, MessageAction::UseSelectedItem),
            (2, MessageAction::DiscardSelectedItem),
        ] {
            let bytes = [0x0C, 0x08, 0x00, 0x0A, case];
            let mut window = MessageWindow::default();
            assert!(window.start(1, 0, &bytes, false));
            reveal_all(&mut window, &text);
            window.update(held(true), &text, 0);
            assert_eq!(window.take_actions(), vec![expected], "case {case}");
        }

        for case in [3u8, 4, 5, 6] {
            let bytes = [0x0C, 0x08, 0x00, 0x0A, case];
            let mut window = MessageWindow::default();
            assert!(window.start(1, 0, &bytes, false));
            reveal_all(&mut window, &text);
            window.update(held(true), &text, 0);
            assert!(window.take_actions().is_empty(), "case {case} is inert");
        }
    }

    #[test]
    fn synthetic_message_renders_deterministically() {
        let cells = [cell(0x02), cell(0x0C), cell(0x0D), cell(0x11)];
        let font = Font::new(sheet(&cells));
        let text = text_tables(&[&[0x0C, 0x07]], &[]);
        // "AB\nC" with the second line green.
        let bytes = [0x0C, 0x0D, 0x02, 0x05, 0x01, 0x11, 0x08, 0x00, 0x01, 0x00];
        let mut window = MessageWindow::default();
        assert!(window.start(1, 0, &bytes, false));
        reveal_all(&mut window, &text);
        tick(&mut window, &text, held(false));

        let mut first = Framebuffer::new();
        window.draw(&mut first, &font, &text);
        let mut second = Framebuffer::new();
        window.draw(&mut second, &font, &text);
        assert_eq!(first.rgba, second.rgba);
        assert_eq!(pixel(&first, 0x22, 181), [255, 255, 255, 255]);
        assert_eq!(pixel(&first, 0x22 + 14, 181), [255, 255, 255, 255]);
        assert_eq!(pixel(&first, 0x22, 197), [0, 255, 0, 255]);
        assert_eq!(pixel(&first, 0x22 + 14, 197), [0, 0, 0, 0]);
        // The yes/no row sits one glyph right of the cursor column.
        assert_eq!(pixel(&first, 0xA0, 181 + 16), [255, 255, 255, 255]);
    }

    #[test]
    fn missing_source_dismisses_cleanly() {
        let mut window = MessageWindow::default();
        assert!(window.start(1, 5, &[], false));
        assert!(!window.active);
        assert_eq!(window.phase(), MessagePhase::Idle);
        assert_eq!(window.menu_choice_id(), 0);
        let mut framebuffer = Framebuffer::new();
        window.draw(&mut framebuffer, &font_for(&[]), &text());
    }

    #[test]
    #[ignore = "requires ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
    fn real_room1001_rdt_message_render_is_stable() {
        let Some(root) = std::env::var("ARKLAY_RE1_ROOT").ok() else {
            return;
        };
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(std::path::Path::new(&path)).unwrap();
        let font = Font::new(crate::tim::decode_4bpp(pack.read("font/font.tim").unwrap()).unwrap());
        let text = Text::load(&pack);

        let data = std::fs::read(std::path::Path::new(&root).join("JPN/STAGE1/ROOM1001.RDT"))
            .expect("ROOM1001.RDT");
        let room = crate::rdt::parse(&data, crate::state::RoomId::parse("1001").unwrap()).unwrap();

        // ROOM1001 ships no locked door record (its only door is unlocked), so
        // the locked-door wording in the milestone maps to the first room
        // message of the room's RDT block.
        let id = 0u8;
        let bytes = room
            .message(u16::from(id))
            .expect("room message 0")
            .to_vec();
        let mut window = MessageWindow::default();
        assert!(window.start(id, 0xFF, &bytes, false));
        assert_eq!(window.source(), bytes.as_slice());

        for _ in 0..600 {
            window.update(held(false), &text, 0);
            if window.phase() != MessagePhase::Reveal && window.phase() != MessagePhase::Init {
                break;
            }
        }
        assert!(window.active, "message dismissed before it rendered");
        assert_ne!(
            window.phase(),
            MessagePhase::Reveal,
            "message still revealing"
        );

        let mut first = Framebuffer::new();
        window.draw(&mut first, &font, &text);
        let mut second = Framebuffer::new();
        window.draw(&mut second, &font, &text);
        assert_eq!(first.rgba, second.rgba);
        assert_eq!(
            fnv1a(&first.rgba),
            0xC284_792B_7C4E_4157,
            "ROOM1001 message 0 render changed"
        );
        let opaque = first
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[3] != 0)
            .count();
        assert!(opaque > 0, "the room message drew no pixels");
    }

    #[test]
    #[ignore = "requires ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
    fn real_locked_door_global_message_render_is_stable() {
        use crate::game::{GameState, ScdGameHost};

        let Some(root) = std::env::var("ARKLAY_RE1_ROOT").ok() else {
            return;
        };
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(std::path::Path::new(&path)).unwrap();
        let font = Font::new(crate::tim::decode_4bpp(pack.read("font/font.tim").unwrap()).unwrap());
        let text = Text::load(&pack);

        // ROOM1010 slot 1 is the sword-key locked door; probe it without the
        // key so the door handler requests its locked message through the game
        // state, then resolve and draw it exactly like the engine will.
        let data = std::fs::read(std::path::Path::new(&root).join("JPN/STAGE1/ROOM1010.RDT"))
            .expect("ROOM1010.RDT");
        let id = crate::state::RoomId::parse("1010").unwrap();
        let room = crate::rdt::parse(&data, id).unwrap();
        let scripts = crate::scd::reader::parse(&data).unwrap();
        let mut state = GameState::new(id, &room);
        {
            let mut host = ScdGameHost::new(&mut state);
            let mut vm = crate::scd::vm::CommandVm::new(&scripts);
            vm.run_init(&mut host);
        }
        let door = state.doors[1].expect("key door");
        let center_x = i32::from(door.zone[0]) + i32::from(door.zone[2]) / 2;
        let center_z = i32::from(door.zone[1]) + i32::from(door.zone[3]) / 2;
        let (dx, dz) = crate::player::reach_offset(0);
        state.interact([center_x - dx, 0, center_z - dz], 0, true);
        let message_id = state.message.id.expect("locked message requested");

        for _ in 0..600 {
            state.update_message(MessageInput::default(), &room, &text);
            if state.message.has_source()
                && !matches!(
                    state.message.phase(),
                    MessagePhase::Init | MessagePhase::Reveal
                )
            {
                break;
            }
        }
        let expected = text.message(&room, u16::from(message_id)).unwrap();
        assert!(!expected.is_empty(), "global message {message_id} is empty");
        assert_eq!(state.message.source(), expected, "text comes from the pack");

        let mut first = Framebuffer::new();
        state.message.draw(&mut first, &font, &text);
        let mut second = Framebuffer::new();
        state.message.draw(&mut second, &font, &text);
        assert_eq!(first.rgba, second.rgba);
        let opaque = first
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[3] != 0)
            .count();
        assert!(opaque > 0, "the locked door message drew no pixels");
    }
}
