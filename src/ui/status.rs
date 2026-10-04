//! Status-screen health condition, character faces and the EKG sweep.
//!
//! The pause menu's health monitor derives a state from the player's health
//! and maximum health, `(health - 1) / (max_health >> 2)` clamped to `0..=3`,
//! which selects the EKG colour red/orange/yellow/green. The poisoned flags
//! (`health_status & 0x22`) swap the face and the EKG wave group without
//! changing the colour. [`HealthBar`] runs the original's four-state animation
//! (init, sweep, flush-in, flush) over the transcribed wave tables, and
//! [`HealthBar::start_flush`] reproduces the heal flush: blue when only the
//! status was cured, green when only health was restored, white for both.
//!
//! The health face and EKG are drawn from `ui/status.tim` and into the
//! framebuffer's RGBA buffer; the EKG uses a Bresenham line so no renderer
//! primitive beyond the existing sprite path is needed.

use crate::model::Texture8;
use crate::render::{Framebuffer, Tint};

use super::layout;

/// Bottom edge of the EKG window: the heal flush stops drawing at this Y.
const FLUSH_BOTTOM_Y: i32 = 0xB0;

/// EKG colour per health state (red, orange, yellow, green) plus the black
/// placeholder the poisoned row indexes.
pub const HEALTH_COLORS: [[u8; 3]; 5] = [
    [0xC0, 0x00, 0x00],
    [0xFF, 0x7F, 0x00],
    [0xD8, 0xD8, 0x00],
    [0x00, 0xFF, 0x00],
    [0x00, 0x00, 0x00],
];

/// The health state `(health - 1) / (max_health >> 2)` clamped to `0..=3`.
///
/// A non-positive health is state 0; a maximum below four degenerates to the
/// full-health state instead of dividing by zero, matching the original's
/// intent at zero max.
pub fn health_state(health: i16, max_health: i16) -> u8 {
    if health <= 0 || max_health <= 0 {
        return 0;
    }
    let step = max_health >> 2;
    if step <= 0 {
        return 3;
    }
    (((i32::from(health) - 1) / i32::from(step)).clamp(0, 3)) as u8
}

/// Whether the health-status byte carries either poison flag (`0x20`/`0x02`).
pub fn is_poisoned(status: u8) -> bool {
    status & 0x22 != 0
}

/// The EKG colour of a non-poisoned health state (`0..=3`).
pub fn health_color(state: u8) -> [u8; 3] {
    HEALTH_COLORS[usize::from(state.min(3))]
}

/// The face flash tables.
pub const FACE_WARN_SPAN: [u8; 8] = [0x0C, 0x11, 0x14, 0x3C, 0x3F, 0x44, 0x50, 0x00];
/// The danger/poison face span table (indexed by [`sweep_index`]).
pub const FACE_DANGER_SPAN: [u8; 16] = [
    0x03, 0x07, 0x21, 0x25, 0x2B, 0x2F, 0x49, 0x4D, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
/// The caution face flash table.
pub const FACE_CAUTION_FLASH: [u8; 8] = [0x00, 0x03, 0x02, 0x01, 0x02, 0x03, 0x00, 0x00];
/// The danger/poison face flash table.
pub const FACE_DANGER_FLASH: [u8; 24] = [
    0x00, 0x02, 0x01, 0x02, 0x00, 0x02, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// One 4-byte EKG wave record: the segment ends at `x` with height `y`, starts
/// at `prev_x` and rises by `slope` for every pixel to the left of `x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavePoint {
    /// Segment right edge, relative to the EKG window's left edge.
    pub x: i32,
    /// Segment left edge (the previous record's `x`).
    pub prev_x: i32,
    /// Height at `x`.
    pub y: i32,
    /// Height change per pixel towards `prev_x`.
    pub slope: i32,
}

impl WavePoint {
    /// Decode one `{x, prev_x, y, slope}` record.
    pub const fn new(record: [i8; 4]) -> Self {
        Self {
            x: record[0] as i32,
            prev_x: record[1] as i32,
            y: record[2] as i32,
            slope: record[3] as i32,
        }
    }
}

macro_rules! waves {
    ($( $name:ident : [$($v:expr),* $(,)?]),* $(,)?) => {
        $(
            const $name: &[i8] = &[$($v),*];
        )*
        /// The 20 EKG wave tables, four per health state (`0..=3`) then four
        /// poisoned variants (`16..=19`), exactly as the original stores them.
        pub const EKG_WAVES: [&[i8]; 20] = [$( $name, )*];
    };
}

waves! {
    WAVE_00: [28, 47, 0, 0, 27, 28, -1, 1, 26, 27, -3, 2, 25, 26, 0, -3, 24, 25, 2, -2, 23, 24, 1, 1, 22, 23, -1, 2, 21, 22, -4, 3, 20, 21, 0, -4, 19, 20, 2, -2, 18, 19, 0, 2, 0, 18, 0, 0],
    WAVE_01: [28, 47, 0, 0, 27, 28, -1, 1, 26, 27, -3, 2, 24, 26, 1, -2, 23, 24, 0, 1, 21, 23, -4, 2, 20, 21, -1, -3, 19, 20, 1, 2, 18, 19, 0, 1, 0, 18, 0, 0],
    WAVE_02: [28, 47, 0, 0, 27, 28, -2, 2, 25, 27, 2, -2, 24, 25, 1, 1, 23, 24, -1, 2, 22, 23, -4, 3, 20, 22, 2, -3, 19, 20, 0, 2, 0, 19, 0, 0],
    WAVE_03: [28, 47, 0, 0, 27, 28, -1, 1, 26, 27, -3, 2, 24, 26, 1, -2, 23, 24, -1, 2, 22, 23, -4, 3, 21, 22, -1, -3, 20, 21, 1, -2, 19, 20, 0, 1, 0, 19, 0, 0],
    WAVE_04: [29, 47, 0, 0, 28, 29, -1, 1, 27, 28, -3, 2, 26, 27, 0, -3, 25, 26, 2, -2, 24, 25, 1, 1, 23, 24, -1, 2, 21, 23, -7, 3, 20, 21, -3, -4, 19, 20, 0, -3, 18, 19, 2, -2, 17, 18, 0, 2, 0, 17, 0, 0],
    WAVE_05: [28, 47, 0, 0, 27, 28, -1, 1, 26, 27, -3, 2, 24, 26, 1, -2, 23, 24, -1, 2, 21, 23, -7, 3, 19, 21, 1, -4, 18, 19, 0, 1, 0, 18, 0, 0],
    WAVE_06: [29, 47, 0, 0, 27, 29, -4, 2, 25, 27, 2, -3, 24, 25, 1, 1, 23, 24, -1, 2, 21, 23, -7, 3, 18, 21, 2, -3, 17, 18, 0, 2, 0, 17, 0, 0],
    WAVE_07: [29, 47, 0, 0, 28, 29, -1, 1, 27, 28, -3, 2, 25, 27, 1, -2, 24, 25, -1, 2, 22, 24, -7, 3, 20, 22, -1, -3, 19, 20, 1, -2, 18, 19, 0, 1, 0, 18, 0, 0],
    WAVE_08: [31, 47, 0, 0, 29, 31, -4, 2, 28, 29, -1, -3, 26, 28, 3, -2, 25, 26, 2, 1, 21, 25, -10, 3, 20, 21, -5, -5, 18, 20, 3, -3, 17, 18, 1, 2, 16, 17, 0, 1, 0, 16, 0, 0],
    WAVE_09: [31, 47, 0, 0, 30, 31, -2, 2, 29, 30, -5, 3, 28, 29, -2, -3, 26, 28, 2, -2, 23, 26, -4, 2, 21, 23, -10, 3, 17, 21, 2, -3, 16, 17, 0, 2, 0, 16, 0, 0],
    WAVE_10: [31, 47, 0, 0, 30, 31, -1, 1, 29, 30, -2, 2, 26, 29, 3, -2, 25, 26, 2, 1, 24, 25, 0, 2, 21, 24, -9, 3, 18, 21, 3, -4, 17, 18, 1, 2, 16, 17, 0, 1, 0, 16, 0, 0],
    WAVE_11: [31, 47, 0, 0, 29, 31, -2, 2, 26, 29, 2, -2, 25, 26, 1, 1, 24, 25, -1, 2, 21, 24, -10, 3, 17, 21, 2, -3, 16, 17, 0, 2, 0, 16, 0, 0],
    WAVE_12: [32, 47, 0, 0, 29, 32, -6, 2, 26, 29, 3, -3, 25, 26, 2, 1, 21, 25, -14, 4, 19, 21, -4, -5, 18, 19, 0, -4, 17, 18, 3, -3, 16, 17, 1, 2, 15, 16, 0, 1, 0, 15, 0, 0],
    WAVE_13: [32, 47, 0, 0, 30, 32, -6, 3, 29, 30, -4, -2, 26, 29, 2, -3, 24, 26, -2, 2, 21, 24, -14, 4, 17, 21, 2, -4, 16, 17, 0, 2, 0, 16, 0, 0],
    WAVE_14: [31, 47, 0, 0, 29, 31, -4, 2, 27, 29, 0, -2, 26, 27, 3, -3, 25, 26, 2, 1, 24, 25, -1, 3, 21, 24, -13, 4, 19, 21, -3, -5, 17, 19, 3, -3, 16, 17, 1, 2, 15, 16, 0, 1, 0, 15, 0, 0],
    WAVE_15: [32, 47, 0, 0, 30, 32, -4, 2, 27, 30, 2, -2, 26, 27, 1, 1, 21, 26, -14, 3, 17, 21, 2, -4, 16, 17, 0, 2, 0, 16, 0, 0],
    WAVE_16: [38, 47, 0, 0, 36, 38, -6, 3, 34, 36, 4, -5, 33, 34, 0, 4, 30, 33, -15, 5, 27, 30, 3, -6, 26, 27, 1, 2, 25, 26, 0, 1, 22, 25, 0, 0, 20, 22, -4, 2, 18, 20, 4, -4, 14, 18, -12, 4, 11, 14, 3, -5, 10, 11, 1, 2, 9, 10, 0, 1, 0, 9, 0, 0],
    WAVE_17: [38, 47, 0, 0, 36, 38, -4, 2, 34, 36, 4, -4, 30, 34, -12, 4, 27, 30, 3, -5, 26, 27, 1, 2, 25, 26, 0, 1, 22, 25, 0, 0, 20, 22, -6, 3, 18, 20, 4, -5, 17, 18, 0, 4, 14, 17, -15, 5, 11, 14, 3, -6, 10, 11, 1, 2, 9, 10, 0, 1, 0, 9, 0, 0],
    WAVE_18: [37, 47, 0, 0, 36, 37, -1, 1, 34, 36, -7, 3, 31, 34, 5, -4, 27, 31, -15, 5, 24, 27, 3, -6, 23, 24, 0, 3, 21, 23, 0, 0, 20, 21, -2, 2, 19, 20, -5, 3, 17, 19, 3, -4, 14, 17, -12, 5, 11, 14, 3, -5, 10, 11, 0, 3, 0, 10, 0, 0],
    WAVE_19: [37, 47, 0, 0, 36, 37, -2, 2, 35, 36, -5, 3, 33, 35, 3, -4, 30, 33, -12, 5, 27, 30, 3, -5, 26, 27, 0, 3, 24, 26, 0, 0, 23, 24, -1, 1, 21, 23, -7, 3, 18, 21, 5, -4, 14, 18, -15, 5, 11, 14, 3, -6, 10, 11, 0, 3, 0, 10, 0, 0],
}

/// Evaluate a raw wave record at screen x.
pub fn wave_y(record: [i8; 4], x: i32) -> i32 {
    let point = WavePoint::new(record);
    let right = point.x + layout::EKG_MIN_X;
    (x - right) * point.slope + point.y + layout::EKG_BASE_Y
}

/// The record of `wave` whose segment covers screen x, as the original's
/// backwards walk selects it: the first record whose right edge is at or left
/// of x. Returns the last record when x is past the wave.
pub fn wave_record(wave: &[i8], x: i32) -> [i8; 4] {
    let mut selected = [0; 4];
    for record in wave.as_chunks::<4>().0 {
        selected = *record;
        if x >= i32::from(record[0]) + layout::EKG_MIN_X {
            break;
        }
    }
    selected
}

/// The original's `FUN_004387e0`: count the span-table entries at or below the
/// sweep head's low byte minus `0x35`.
pub fn sweep_index(data: &[u8], head: i16) -> usize {
    let threshold = i32::from(head as u8 as i8) - 0x35;
    data.iter()
        .take_while(|&&byte| i32::from(byte) <= threshold)
        .count()
}

/// What a heal flush should tint the EKG line with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flush {
    /// Status cured only.
    Blue,
    /// Health restored only.
    Green,
    /// Both health and status.
    White,
}

impl Flush {
    /// The flush colour for the healed/cured flags.
    pub fn from_flags(healed: bool, cured: bool) -> Option<Self> {
        match (healed, cured) {
            (false, false) => None,
            (true, false) => Some(Self::Green),
            (false, true) => Some(Self::Blue),
            (true, true) => Some(Self::White),
        }
    }

    /// The full-brightness colour of the flush.
    pub fn color(self) -> [u8; 3] {
        match self {
            Self::Blue => [0, 0, 0xFF],
            Self::Green => [0, 0xFF, 0],
            Self::White => [0xFF, 0xFF, 0xFF],
        }
    }
}

/// The status screen's animated health monitor.
#[derive(Debug, Clone)]
pub struct HealthBar {
    /// Animation state: 0 init, 1 sweep, 2 flush-in, 3 flush.
    pub state: u8,
    /// Current health state (`0..=3`, not poisoned).
    pub health_state: u8,
    /// Whether either poison flag is set.
    pub poisoned: bool,
    /// The EKG colour of the health state before poisoning.
    pub color: [u8; 3],
    /// The active EKG wave table index.
    pub wave: usize,
    /// The trailing wave table index.
    pub secondary_wave: usize,
    /// The flush tint, when a heal flush is running.
    pub flush: Option<Flush>,
    head: i16,
    secondary_head: i16,
    secondary_color: [u8; 3],
    rng: u32,
}

impl Default for HealthBar {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthBar {
    /// A bar in the init state with no health state yet.
    pub fn new() -> Self {
        Self {
            state: 0,
            health_state: 3,
            poisoned: false,
            color: health_color(3),
            wave: 0,
            secondary_wave: 0,
            flush: None,
            head: 0x84,
            secondary_head: 0x84,
            secondary_color: health_color(3),
            rng: 0x1234_5678,
        }
    }

    /// Advance the sweep one frame for the current health and status.
    pub fn update(&mut self, health: i16, max_health: i16, status: u8) {
        self.health_state = health_state(health, max_health);
        self.poisoned = is_poisoned(status);
        match self.state {
            0 => {
                self.state = 1;
                self.head = 0x84;
                self.secondary_head = 0x84;
                self.refresh_wave();
            }
            1 => {
                self.head += 1;
                if self.head > layout::EKG_MAX_X as i16 {
                    self.head = 0x35;
                    self.refresh_wave();
                }
                self.secondary_head += 1;
                if self.secondary_head > layout::EKG_MAX_X as i16 {
                    self.secondary_head = self.head - 0x1F;
                    self.secondary_wave = self.wave;
                    self.secondary_color = self.color;
                }
            }
            2 => {
                self.state = 3;
                self.head = 0xAF;
            }
            3 => {
                self.head -= 2;
                if self.head <= layout::EKG_MAX_X as i16 {
                    self.state = 0;
                    self.flush = None;
                }
            }
            _ => self.state = 0,
        }
    }

    /// Request the heal flush: the state jumps to the vertical sweep that
    /// fades the line to blue (cure), green (heal) or white (both).
    pub fn start_flush(&mut self, healed: bool, cured: bool) {
        if let Some(flush) = Flush::from_flags(healed, cured) {
            self.flush = Some(flush);
            self.state = 2;
        }
    }

    /// The primary sweep's left edge (clamped) and right edge.
    pub fn primary_span(&self) -> (i32, i32) {
        let left = (i32::from(self.head) + 0x0F).clamp(layout::EKG_MIN_X, layout::EKG_MAX_X);
        let right = i32::from(self.head).clamp(layout::EKG_MIN_X, layout::EKG_MAX_X);
        (left, right)
    }

    /// The trailing sweep's span.
    pub fn secondary_span(&self) -> (i32, i32) {
        let left =
            (i32::from(self.secondary_head) + 0x1F).clamp(layout::EKG_MIN_X, layout::EKG_MAX_X);
        let right = i32::from(self.secondary_head).clamp(layout::EKG_MIN_X, layout::EKG_MAX_X);
        (left, right)
    }

    /// Pick the next wave table: four variants per health state, with the
    /// poisoned group (`16..=19`) chosen by a random bit like the original.
    fn refresh_wave(&mut self) {
        let variant = self.next_random() & 3;
        let group = if self.poisoned && self.next_random() & 1 != 0 {
            4
        } else {
            self.health_state
        };
        self.wave = usize::from(group) * 4 + variant as usize;
        if self.wave >= EKG_WAVES.len() {
            self.wave = 0;
        }
        self.color = health_color(self.health_state);
        if self.state != 3 {
            self.secondary_wave = self.wave;
            self.secondary_color = self.color;
        }
    }

    fn next_random(&mut self) -> u32 {
        let mut value = self.rng;
        value ^= value << 13;
        value ^= value >> 17;
        value ^= value << 5;
        self.rng = value;
        value
    }

    /// Draw the health face and EKG into `framebuffer` from `texture`.
    pub fn draw(&self, framebuffer: &mut Framebuffer, texture: &Texture8) {
        self.draw_face(framebuffer, texture);
        if self.state == 2 || self.state == 3 {
            self.draw_flush(framebuffer);
            return;
        }
        if self.state != 1 {
            return;
        }
        let (left, right) = self.primary_span();
        if right >= layout::EKG_MIN_X && left > right {
            self.draw_wave_column(framebuffer, left, right, self.wave, self.color, true);
        }
        let (left, right) = self.secondary_span();
        if right >= layout::EKG_MIN_X && left > right {
            let color = if self.secondary_color == [0, 0, 0] {
                self.color
            } else {
                self.secondary_color
            };
            self.draw_wave_column(framebuffer, left, right, self.secondary_wave, color, false);
        }
    }

    /// Draw the face tile selected by the health state and sweep position.
    fn draw_face(&self, framebuffer: &mut Framebuffer, texture: &Texture8) {
        let row = if self.poisoned { 4 } else { self.health_state };
        let (table, base, flash) = match row {
            0 => (&FACE_DANGER_SPAN[..], 0x30, &FACE_DANGER_FLASH[..]),
            1 => (&FACE_WARN_SPAN[..], 0x18, &FACE_CAUTION_FLASH[..]),
            2 | 3 => {
                self.draw_face_tile(framebuffer, texture, 0);
                return;
            }
            _ => (&FACE_DANGER_SPAN[..], 0x40, &FACE_DANGER_FLASH[..]),
        };
        let index = sweep_index(table, self.head);
        let Some(&select) = flash.get(index) else {
            return;
        };
        if select == 0 {
            return;
        }
        self.draw_face_tile(framebuffer, texture, select * 8 + base);
    }

    fn draw_face_tile(&self, framebuffer: &mut Framebuffer, texture: &Texture8, v: u8) {
        let [x, y] = layout::HEALTH_FACE_POS;
        let [w, h] = layout::HEALTH_FACE_SIZE;
        framebuffer.draw_indexed_sprite(
            texture,
            [0x60, i32::from(v), i32::from(w), i32::from(h)],
            [i32::from(x), i32::from(y), i32::from(w), i32::from(h)],
            0,
            0,
            Tint::White,
        );
    }

    /// Draw the vertical heal flush: the original's state-3 sweep draws up to
    /// 15 one-pixel lines downward from the flush base, their tint starting at
    /// `0xFF` (or the scaled value once the base has entered the window) and
    /// dropping by `0x10` each line, stopping at the window's bottom edge.
    fn draw_flush(&self, framebuffer: &mut Framebuffer) {
        let Some(flush) = self.flush else {
            return;
        };
        let base = flush.color();
        let (mut y, count, mut tint) = Self::flush_tail(i32::from(self.head));
        for _ in 0..count {
            if y >= FLUSH_BOTTOM_Y {
                break;
            }
            let color = [
                if base[0] != 0 { tint } else { 0 },
                if base[1] != 0 { tint } else { 0 },
                if base[2] != 0 { tint } else { 0 },
            ];
            draw_line(
                framebuffer,
                layout::EKG_MIN_X,
                y,
                layout::EKG_MAX_X,
                y,
                color,
            );
            if tint != 0 {
                tint = tint.wrapping_sub(0x10);
            }
            y += 1;
        }
    }

    /// The heal flush's first line Y, line count and first-line tint for the
    /// current flush base. Once the base has left the window (`head < 0x92`)
    /// the original trims the tail and pre-fades the tint by how many rows
    /// were skipped; at or above it, a full 15-line tail starts at full tint.
    fn flush_tail(head: i32) -> (i32, i32, u8) {
        if head < 0x92 {
            let offset = (0x92 - head) as u8;
            (
                0x92,
                (0x0F - i32::from(offset)).max(0),
                0xFFu8.wrapping_sub(offset.wrapping_mul(0x10)),
            )
        } else {
            (head, 0x0F, 0xFF)
        }
    }

    /// Draw one EKG sweep window as a per-column polyline.
    fn draw_wave_column(
        &self,
        framebuffer: &mut Framebuffer,
        left: i32,
        right: i32,
        wave: usize,
        color: [u8; 3],
        primary: bool,
    ) {
        let Some(&records) = EKG_WAVES.get(wave) else {
            return;
        };
        // The original sweeps from the head (right) back towards the tail
        // (left); left is always the larger edge.
        let mut previous = None;
        for x in right..=left {
            let record = wave_record(records, x);
            let y = wave_y(record, x);
            if let Some((previous_x, previous_y)) = previous {
                let shade = if primary {
                    color
                } else {
                    // The trailing line fades out to its right.
                    let span = (right - left).max(1) as f32;
                    let fade = 1.0 - (x - left) as f32 / span * 0.6;
                    [
                        (color[0] as f32 * fade) as u8,
                        (color[1] as f32 * fade) as u8,
                        (color[2] as f32 * fade) as u8,
                    ]
                };
                draw_line(framebuffer, previous_x, previous_y, x, y, shade);
            }
            previous = Some((x, y));
        }
    }
}

/// Rasterize a 1-pixel Bresenham line, clipped to the framebuffer.
pub fn draw_line(
    framebuffer: &mut Framebuffer,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: [u8; 3],
) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        put_pixel(framebuffer, x, y, color);
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

fn put_pixel(framebuffer: &mut Framebuffer, x: i32, y: i32, color: [u8; 3]) {
    if x < 0 || y < 0 || x >= framebuffer.width as i32 || y >= framebuffer.height as i32 {
        return;
    }
    let offset = (y as usize * framebuffer.width as usize + x as usize) * 4;
    if let Some(pixel) = framebuffer.rgba.get_mut(offset..offset + 4) {
        pixel[0] = color[0];
        pixel[1] = color[1];
        pixel[2] = color[2];
        pixel[3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_states_follow_the_thirds_and_clamp() {
        assert_eq!(health_state(140, 140), 3);
        assert_eq!(health_state(106, 140), 3);
        assert_eq!(health_state(105, 140), 2);
        assert_eq!(health_state(71, 140), 2);
        assert_eq!(health_state(70, 140), 1);
        assert_eq!(health_state(36, 140), 1);
        assert_eq!(health_state(35, 140), 0);
        assert_eq!(health_state(1, 140), 0);
        assert_eq!(health_state(0, 140), 0);
        // A maximum that is not a multiple of four cannot divide out to 4.
        assert_eq!(health_state(96, 96), 3);
        assert_eq!(health_state(150, 150), 3);
        assert_eq!(health_state(10, 2), 3);
        assert_eq!(health_state(10, 0), 0);
    }

    #[test]
    fn colours_and_poison_flags() {
        assert_eq!(health_color(0), [0xC0, 0, 0]);
        assert_eq!(health_color(1), [0xFF, 0x7F, 0]);
        assert_eq!(health_color(2), [0xD8, 0xD8, 0]);
        assert_eq!(health_color(3), [0, 0xFF, 0]);
        assert!(!is_poisoned(0x10));
        assert!(is_poisoned(0x20));
        assert!(is_poisoned(0x02));
        assert!(is_poisoned(0x22));
    }

    #[test]
    fn flush_colours_follow_the_set_flags() {
        assert_eq!(Flush::from_flags(false, false), None);
        assert_eq!(Flush::from_flags(true, false), Some(Flush::Green));
        assert_eq!(Flush::from_flags(false, true), Some(Flush::Blue));
        assert_eq!(Flush::from_flags(true, true), Some(Flush::White));
        assert_eq!(Flush::White.color(), [255, 255, 255]);
    }

    #[test]
    fn wave_tables_are_nineteen_records_or_less_and_terminate_at_zero() {
        assert_eq!(EKG_WAVES.len(), 20);
        for wave in EKG_WAVES {
            assert_eq!(wave.len() % 4, 0);
            let records = wave.as_chunks::<4>().0;
            assert_eq!(records.last().unwrap()[0], 0);
            assert!(records.len() <= 19);
        }
    }

    #[test]
    fn wave_evaluation_interpolates_along_the_segment() {
        // The second record of wave 0: right edge 27+0x54, height -1, slope 1.
        let record = [27i8, 28, -1, 1];
        assert_eq!(
            wave_y(record, 27 + layout::EKG_MIN_X),
            layout::EKG_BASE_Y - 1
        );
        assert_eq!(
            wave_y(record, 26 + layout::EKG_MIN_X),
            layout::EKG_BASE_Y - 2
        );
        assert_eq!(
            wave_y(record, 25 + layout::EKG_MIN_X),
            layout::EKG_BASE_Y - 3
        );

        // The walk selects the first record whose right edge is left of x.
        let wave = EKG_WAVES[0];
        assert_eq!(wave_record(wave, layout::EKG_MIN_X + 30), [28, 47, 0, 0]);
        assert_eq!(wave_record(wave, layout::EKG_MIN_X + 27), [27, 28, -1, 1]);
        assert_eq!(wave_record(wave, layout::EKG_MIN_X + 10), [0, 18, 0, 0]);
    }

    #[test]
    fn sweep_index_counts_span_entries_like_the_original() {
        assert_eq!(sweep_index(&FACE_WARN_SPAN, 0x35), 0);
        assert_eq!(sweep_index(&FACE_WARN_SPAN, 0x35 + 0x0C), 1);
        assert_eq!(sweep_index(&FACE_WARN_SPAN, 0x35 + 0x14), 3);
        // The head is read as a signed byte, so the wrap value is negative
        // and selects no frame; 0x7F selects through 0x49.
        assert_eq!(sweep_index(&FACE_DANGER_SPAN, 0x83), 0);
        assert_eq!(sweep_index(&FACE_DANGER_SPAN, 0x7F), 7);
    }

    #[test]
    fn the_sweep_walks_and_wraps() {
        let mut bar = HealthBar::new();
        bar.update(140, 140, 0);
        assert_eq!(bar.state, 1);
        // The head starts just past the right edge and wraps on the next tick.
        bar.head = layout::EKG_MAX_X as i16;
        bar.update(140, 140, 0);
        assert_eq!(bar.head, 0x35);
        assert_eq!(bar.color, health_color(3));
        for _ in 0..40 {
            bar.update(140, 140, 0);
        }
        assert_eq!(bar.state, 1);
        assert!(bar.head > 0x50 && bar.head <= layout::EKG_MAX_X as i16);
    }

    #[test]
    fn a_heal_flush_and_its_end() {
        let mut bar = HealthBar::new();
        bar.update(20, 96, 0x22);
        assert_eq!(bar.health_state, 0);
        assert!(bar.poisoned);
        bar.start_flush(true, true);
        assert_eq!(bar.state, 2);
        assert_eq!(bar.flush, Some(Flush::White));
        bar.update(96, 96, 0);
        assert_eq!(bar.state, 3);
        assert_eq!(bar.head, 0xAF);
        while bar.state == 3 {
            bar.update(96, 96, 0);
        }
        assert_eq!(bar.flush, None);
    }

    #[test]
    fn the_flush_tail_scales_its_count_and_tint() {
        // At or above the window top, a full 15-line tail starts at full tint.
        assert_eq!(HealthBar::flush_tail(0x92), (0x92, 0x0F, 0xFF));
        assert_eq!(HealthBar::flush_tail(0xAF), (0xAF, 0x0F, 0xFF));
        // Below it, skipped rows are removed and the first line is pre-faded.
        assert_eq!(HealthBar::flush_tail(0x91), (0x92, 0x0E, 0xEF));
        assert_eq!(HealthBar::flush_tail(0x85), (0x92, 0x02, 0x2F));
    }

    #[test]
    fn the_flush_draws_downward_from_the_window_top_with_the_scaled_tint() {
        let mut bar = HealthBar::new();
        bar.flush = Some(Flush::Blue);
        bar.head = 0x91;
        let mut framebuffer = Framebuffer::new();
        bar.draw_flush(&mut framebuffer);
        let pixel = |x: i32, y: i32| -> [u8; 4] {
            let offset = (y as usize * 320 + x as usize) * 4;
            framebuffer.rgba[offset..offset + 4].try_into().unwrap()
        };
        // The row above the window is untouched: the tail starts at 0x92.
        assert_eq!(pixel(layout::EKG_MIN_X, 0x91), [0, 0, 0, 0]);
        // The first line uses the pre-faded tint, the next drops by 0x10.
        assert_eq!(
            pixel(layout::EKG_MIN_X, 0x92),
            [0, 0, 0xEF, 0xFF],
            "first tail line"
        );
        assert_eq!(
            pixel(layout::EKG_MIN_X, 0x93),
            [0, 0, 0xDF, 0xFF],
            "second tail line"
        );
    }

    #[test]
    fn the_face_row_depends_on_health_and_poison() {
        let mut bar = HealthBar::new();
        bar.update(140, 140, 0);
        assert!(!bar.poisoned);
        assert_eq!(bar.health_state, 3);
        bar.update(140, 140, 0x20);
        assert!(bar.poisoned);
        assert_eq!(bar.health_state, 3);
        // Poisoned waves live in the 16..=19 group when the random bit fires.
        let mut poisoned_group = false;
        for _ in 0..200 {
            bar.update(140, 140, 0x20);
            if bar.wave >= 16 {
                poisoned_group = true;
                break;
            }
        }
        assert!(poisoned_group, "poison never selected the poisoned waves");
    }

    #[test]
    fn draw_line_marks_its_endpoints() {
        let mut framebuffer = Framebuffer::new();
        draw_line(&mut framebuffer, 10, 10, 13, 10, [255, 0, 0]);
        for x in 10..=13 {
            let offset = (10 * framebuffer.width as usize + x as usize) * 4;
            assert_eq!(&framebuffer.rgba[offset..offset + 3], &[255, 0, 0]);
        }
        draw_line(&mut framebuffer, -5, -5, -1, -1, [0, 255, 0]);
    }
}
