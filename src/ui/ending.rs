//! The post-film ending screens: the RESULT screen and the rocket-launcher
//! epilogue.
//!
//! [`ResultScreen`] mirrors the reference's `ending_result_*`
//! (`EndingScreen.cpp:327-681`): it reveals five baked plates from the
//! `en05.tim`/`en07.tim` page in pairs, fades each slot, prints the clear time
//! and the save count, waits at the step-4 input gate, and - for ending 6 -
//! shows the additive fifth plate before the confetti bonus. The row's
//! `background` flag selects the plate page and the character backdrop, and the
//! already-cleared-once bit (0x7B) swaps in the alternate stills; the
//! `epilogue` flag (endings 6/7) selects [`EpilogueScreen`].
//!
//! [`EpilogueScreen`] mirrors `ending_epilogue_*` (`EndingScreen.cpp:687-836`):
//! the `rc1121.pix` backdrop, the two caption plates from the same page and the
//! spinning `i73v.ivm` rocket launcher, held for `0x10D` frames and faded out.
//!
//! Neither screen draws a letter rank or an item summary: the original has
//! neither. The plate fades are the reference's signed `0x800` ramps, and the
//! clear-time/save-count strings are drawn through the reference's own grid
//! addressing [`draw_grid_text`], not the port's encoded text path.

use anyhow::Result;

use crate::anim::{self, Mat4x3};
use crate::assets::{Artwork, Geometry};
use crate::ending::{self, CREDIT_ENTRIES, EndStats, EndingRow, SlotKind};
use crate::font::{GLYPH_HEIGHT, Tint};
use crate::ivm::{self, Ivm};
use crate::render::{Camera, Framebuffer};
use crate::state::Image;

use super::{Screen, ScreenAction, ScreenResult, UiContext, UiInput};

/// The packed plate width: every `en05`/`en07` plate is 256 pixels wide.
const PLATE_WIDTH: i32 = 256;
/// The per-frame slot fade step (`0x800`).
const FADE_STEP: i32 = 0x800;
/// The saturated fade level (`0x8000`).
const FADE_FULL: i32 = 0x8000;
/// The step-4 wait before the timeout advances (`0x708`).
const INPUT_TIMEOUT: u16 = 0x708;
/// Frames the confetti burst runs before it is retired (`0x1E + 0x32`).
const CONFETTI_FRAMES: u16 = 0x1E + 0x32;
/// Frames the epilogue holds the built scene (`0x10D`), counted from the first
/// update after the build, fade-in included (`EndingScreen.cpp:812-818`).
const EPILOGUE_HOLD: u16 = 0x10D;
/// The epilogue backdrop's fixed shade level (`s_bgSlot->fadeLevel`, 0x6000,
/// `EndingScreen.cpp:792`): the still draws at 0x60/0x80 brightness.
const EPILOGUE_BACKDROP_FADE: i32 = 0x6000;
/// The epilogue's per-frame fade step: the reference's `0x800` counter step of
/// `0x8000` (`EndingScreen.cpp:797-798, 813-816`).
const EPILOGUE_FADE_STEP: i32 = 16;
/// The epilogue model spin step per frame (`0x20`).
const EPILOGUE_SPIN_STEP: i16 = 0x20;
/// The epilogue camera focal length (`0x1A4`).
const EPILOGUE_FOV: i32 = 0x1A4;

/// The shade a slot's fade level contributes: the high byte of the signed
/// 16-bit ramp, `0..=0x80` (`EndingScreen.cpp:352`).
fn fade_shade(level: i32) -> u8 {
    ((level.clamp(0, FADE_FULL) >> 8) & 0xFF) as u8
}

/// The `0..=255` colour multiplier a PSX `0x80` (full) shade maps to.
fn shade_scale(shade: u8) -> u8 {
    (u32::from(shade) * 255 / 0x80).min(255) as u8
}

/// The RESULT screen's character backdrop, the "escaped" still from the run's
/// clear count: the already-cleared-once bit (0x7B) swaps in the alternate
/// stills (`EndingScreen.cpp:875-898`).
fn backdrop_stem(character: u8, second_playthrough: bool) -> &'static str {
    match (character & 1 == 0, second_playthrough) {
        (true, false) => "ui/chris02",
        (false, false) => "ui/jill02",
        (true, true) => "ui/clis01",
        (false, true) => "ui/jill01",
    }
}

/// Draw one raw text buffer through the reference's own grid addressing:
/// `u = (c % 18) * glyph_w`, `v = (c / 18) * 14`, no header offset
/// (`ending_draw_text`, `EndingScreen.cpp:296-324`).
///
/// The buffer's bytes are ASCII (the original's `sprintf`), including the raw
/// `0x5E` separator, so the digits line up with the shipped font sheet and a
/// colon lands on the sheet's own separator cell. `shade` is the slot's
/// `0..=0x80` colour multiplier.
pub fn draw_grid_text(
    framebuffer: &mut Framebuffer,
    font: &crate::font::Font,
    x: i32,
    y: i32,
    shade: u8,
    bytes: &[u8],
) {
    let glyph_w = font.metrics.glyph_w;
    if glyph_w <= 0 {
        return;
    }
    let scale = shade_scale(shade);
    let mut pen = x;
    for &byte in bytes {
        if byte != b' ' {
            let u = i32::from(byte % 18) * glyph_w;
            let v = i32::from(byte / 18) * GLYPH_HEIGHT;
            framebuffer.draw_indexed_sprite_scaled(
                &font.texture,
                [u, v, glyph_w, GLYPH_HEIGHT],
                [pen, y, glyph_w, GLYPH_HEIGHT],
                0,
                scale,
                Tint::White,
            );
        }
        pen += glyph_w;
    }
}

/// One plate or text slot with its own fade ramp.
#[derive(Debug, Clone, Copy)]
struct PlateSlot {
    active: bool,
    kind: SlotKind,
    /// Screen X before the `-0xA0` centring.
    x: i32,
    /// Screen Y before the `-0x78` centring.
    y: i32,
    tex_v: u8,
    height: u8,
    fade_level: i32,
    fade_vel: i32,
    /// The ending-6 fifth plate's additive glow (drawn opaque in the port).
    additive: bool,
}

impl PlateSlot {
    /// A hidden slot from a credit-table entry, with Jill's background-layout
    /// shift applied (`EndingScreen.cpp:445-482`).
    fn from_entry(kind: SlotKind, x: i32, y: i32, tex_v: u8, height: u8) -> Self {
        Self {
            active: false,
            kind,
            x,
            y,
            tex_v,
            height,
            fade_level: 0,
            fade_vel: 0,
            additive: false,
        }
    }

    /// Advance the slot's fade ramp one frame, saturating like the reference's
    /// signed 16-bit ramp (`EndingScreen.cpp:342-350`).
    fn step_fade(&mut self) {
        if !self.active || self.fade_vel == 0 {
            return;
        }
        self.fade_level += self.fade_vel;
        if self.fade_level <= 0 {
            self.fade_level = 0;
            self.fade_vel = 0;
        } else if self.fade_level >= FADE_FULL {
            self.fade_level = FADE_FULL;
            self.fade_vel = 0;
        }
    }

    /// The slot's current shade multiplier.
    fn shade(&self) -> u8 {
        fade_shade(self.fade_level)
    }

    /// Whether the slot draws anything this frame.
    fn visible(&self) -> bool {
        self.active && self.fade_level != 0
    }

    /// Draw a sprite slot from `texture`, or the string `bytes` through the
    /// font for a text slot.
    fn draw(
        &self,
        framebuffer: &mut Framebuffer,
        texture: Option<&Artwork>,
        font: Option<&crate::font::Font>,
        bytes: &[u8],
    ) {
        if !self.visible() {
            return;
        }
        // The reference's slot x/y already carry the `-0xA0`/`-0x78` centring,
        // and the sprite path adds the screen origin back, so the final frame
        // position is the raw table x/y.
        let x = self.x;
        let y = self.y;
        match self.kind {
            SlotKind::Sprite => {
                let Some(texture) = texture else {
                    return;
                };
                framebuffer.draw_page_sprite_scaled(
                    texture,
                    [
                        0,
                        i32::from(self.tex_v),
                        PLATE_WIDTH,
                        i32::from(self.height),
                    ],
                    [x, y, PLATE_WIDTH, i32::from(self.height)],
                    0,
                    shade_scale(self.shade()),
                    Tint::White,
                );
            }
            SlotKind::ClearTime | SlotKind::SaveCount => {
                let Some(font) = font else {
                    return;
                };
                draw_grid_text(framebuffer, font, x, y, self.shade(), bytes);
            }
        }
    }
}

/// A deterministic confetti burst: the no-save run's step-10 bonus.
#[derive(Debug, Default)]
struct Confetti {
    particles: Vec<Particle>,
}

#[derive(Debug, Clone, Copy)]
struct Particle {
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
    color: [u8; 4],
}

const CONFETTI_COLORS: [[u8; 4]; 5] = [
    [255, 64, 64, 255],
    [255, 220, 64, 255],
    [64, 220, 96, 255],
    [64, 160, 255, 255],
    [255, 128, 220, 255],
];

impl Confetti {
    /// Spawn the burst with the deterministic LCG the port uses for captures.
    fn spawn(&mut self, count: usize) {
        self.particles.clear();
        let mut seed = 0x1234_5678u32;
        for index in 0..count {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let x = (seed >> 8) as i32 % 320;
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let vx = (seed >> 12) as i32 % 5 - 2;
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let vy = -((seed >> 10) as i32 % 6) - 4;
            self.particles.push(Particle {
                x,
                y: 96,
                vx,
                vy,
                color: CONFETTI_COLORS[index % CONFETTI_COLORS.len()],
            });
        }
    }

    fn step(&mut self) {
        for particle in &mut self.particles {
            particle.x += particle.vx;
            particle.y += particle.vy;
            particle.vy += 1;
        }
    }

    fn draw(&self, framebuffer: &mut Framebuffer) {
        for particle in &self.particles {
            framebuffer.fill_rect([particle.x, particle.y, 2, 2], particle.color);
        }
    }
}

/// The RESULT screen state machine.
pub struct ResultScreen {
    stats: EndStats,
    /// The already-cleared-once bit (0x7B): swaps in the alternate stills
    /// (`EndingScreen.cpp:875-898`).
    second_playthrough: bool,
    row: EndingRow,
    plate: Option<Artwork>,
    backdrop: Option<Image>,
    /// The type-1 screen-colour slot: its fade drives the backdrop/screen.
    background: PlateSlot,
    slots: [PlateSlot; 5],
    clear_bytes: Vec<u8>,
    save_bytes: Vec<u8>,
    step: u8,
    timer: u16,
    effects: bool,
    done: bool,
    confetti: Confetti,
    confetti_timer: u16,
}

impl ResultScreen {
    /// A RESULT screen for `stats`, still without any loaded art. The
    /// `second_playthrough` bit (0x7B) selects the cleared-once backdrops.
    pub fn new(stats: EndStats, second_playthrough: bool) -> Self {
        Self {
            stats,
            second_playthrough,
            row: ending::row(stats.ending_id).unwrap_or(ending::ROWS[0]),
            plate: None,
            backdrop: None,
            background: PlateSlot {
                active: true,
                kind: SlotKind::Sprite,
                x: 0,
                y: 0,
                tex_v: 0,
                height: 0,
                fade_level: 0,
                fade_vel: 0,
                additive: false,
            },
            slots: [PlateSlot::from_entry(SlotKind::Sprite, 0, 0, 0, 0); 5],
            clear_bytes: stats.clear_time_bytes(),
            save_bytes: stats.save_count_bytes(),
            step: 0,
            timer: 0,
            effects: false,
            done: false,
            confetti: Confetti::default(),
            confetti_timer: 0,
        }
    }

    /// The step the machine is on (tests).
    pub fn step(&self) -> u8 {
        self.step
    }

    /// Whether the machine has finished.
    pub fn finished(&self) -> bool {
        self.done
    }

    /// Install the five RESULT slots from the credit table
    /// (`ending_result_build`, `EndingScreen.cpp:440-518`).
    fn build(&mut self) {
        let base = ending::credit_base(self.row.background);
        let jill = self.stats.character & 1 == 1;
        for index in 0..5 {
            let entry = CREDIT_ENTRIES[base + index];
            let mut x = i32::from(entry.x);
            if jill && self.row.background {
                x += i32::from(ending::JILL_LAYOUT_SHIFT);
            }
            if entry.kind == SlotKind::ClearTime {
                // Re-centre the clear time once it needs more than two digits
                // of hours (`EndingScreen.cpp:472-480`); unreachable in
                // practice.
                let mut hours = self.stats.time_ticks / 0x34BC0;
                while hours / 100 != 0 {
                    x -= 0x70;
                    hours /= 10;
                }
            }
            let slot =
                PlateSlot::from_entry(entry.kind, x, i32::from(entry.y), entry.tex_v, entry.height);
            self.slots[index] = slot;
        }
    }

    /// Activate a slot with a rising or falling fade velocity.
    fn set_fade(&mut self, index: usize, velocity: i32) {
        self.slots[index].active = true;
        self.slots[index].fade_vel = velocity;
    }

    fn effects_enable(&mut self) {
        // The no-save bonus is a run whose counter still reads its initial one
        // (`EndingScreen.cpp:426-434`).
        self.effects = self.stats.no_save();
    }
}

impl Screen for ResultScreen {
    fn open(&mut self, cx: &mut UiContext<'_>) -> Result<()> {
        let plate_stem = if self.row.background {
            "ui/en07"
        } else {
            "ui/en05"
        };
        self.plate = match super::load_page(cx.pack, plate_stem) {
            Ok(art) => Some(art),
            Err(err) => {
                eprintln!("warning: no result plate: {err:#}");
                None
            }
        };
        if self.row.background {
            let stem = backdrop_stem(self.stats.character, self.second_playthrough);
            self.backdrop = match super::load_picture(cx.pack, stem, Geometry::Free, None) {
                Ok(image) => Some(image),
                Err(err) => {
                    eprintln!("warning: no result backdrop: {err:#}");
                    None
                }
            };
        }
        Ok(())
    }

    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
        // Step every slot's fade first, exactly once per fixed tick.
        self.background.step_fade();
        for slot in &mut self.slots {
            slot.step_fade();
        }
        if self.effects && self.confetti_timer < CONFETTI_FRAMES {
            self.confetti.step();
            self.confetti_timer += 1;
        }

        match self.step {
            0 => {
                self.timer = 0;
                self.background.fade_level = 0;
                self.background.fade_vel = FADE_STEP;
                self.build();
                self.step = 1;
            }
            1 => {
                self.timer += 1;
                if self.timer > 0x3C {
                    self.timer = 0;
                    self.set_fade(0, FADE_STEP);
                    self.set_fade(1, FADE_STEP);
                    self.step = 2;
                }
            }
            2 => {
                self.timer += 1;
                if self.timer > 0x3C {
                    self.timer = 0;
                    self.set_fade(2, FADE_STEP);
                    self.set_fade(3, FADE_STEP);
                    self.step = 3;
                }
            }
            3 => {
                self.timer += 1;
                if self.timer > 0x5A {
                    self.timer = 0;
                    self.step = 4;
                }
            }
            4 => {
                // The single input gate: any button, or the timeout.
                self.timer += 1;
                if self.timer > INPUT_TIMEOUT || input.any || input.confirm || input.cancel {
                    self.timer = 0;
                    self.step = if self.stats.ending_id == 6 { 6 } else { 5 };
                }
            }
            5 => {
                self.step = 10;
                self.timer = 0;
                self.background.fade_vel = -FADE_STEP;
                for index in 0..4 {
                    self.slots[index].fade_vel = -FADE_STEP;
                }
            }
            6 => {
                // Ending 6 keeps the backdrop up for the fifth plate.
                self.step = 7;
                self.timer = 0;
                for index in 0..4 {
                    self.slots[index].fade_vel = -FADE_STEP;
                }
            }
            7 => {
                self.timer += 1;
                if self.timer > 0x5A {
                    self.slots[4].active = true;
                    self.slots[4].fade_vel = FADE_STEP;
                    self.slots[4].additive = true;
                    self.timer = 0;
                    self.step = 8;
                }
            }
            8 => {
                self.timer += 1;
                if self.slots[4].fade_level >= FADE_FULL {
                    self.slots[4].additive = false;
                }
                if self.timer > 0x96 {
                    self.timer = 0;
                    self.step = 9;
                    self.background.fade_vel = -FADE_STEP;
                }
            }
            9 => {
                self.timer += 1;
                if self.timer > 0x96 {
                    self.timer = 0;
                    self.step = 10;
                    self.slots[4].fade_vel = -FADE_STEP;
                    self.slots[4].additive = true;
                }
            }
            10 => {
                self.timer += 1;
                if self.timer > 0x1E {
                    self.effects_enable();
                    self.background.active = false;
                    for slot in &mut self.slots {
                        slot.active = false;
                    }
                    if self.effects {
                        // The reference's `Effect_CreateBillboard(0x0B, 0x0A)`;
                        // the port approximates it with a deterministic burst.
                        self.confetti.spawn(48);
                        cx.play_cue(super::UiCue::Decide);
                    }
                    self.confetti_timer = 0;
                    self.step = 11;
                    self.timer = 0;
                }
            }
            11 => {
                self.timer += 1;
                if self.timer > 0x32 {
                    self.step = 12;
                }
            }
            12 => {
                self.done = true;
                self.timer = 0;
            }
            _ => {}
        }
        if self.done {
            return ScreenResult::Done(ScreenAction::EndingEpilogue);
        }
        ScreenResult::Continue
    }

    fn draw(&mut self, cx: &UiContext<'_>, framebuffer: &mut Framebuffer) {
        framebuffer.clear();
        framebuffer.fill_rect(
            [0, 0, framebuffer.width as i32, framebuffer.height as i32],
            [0, 0, 0, 255],
        );
        if let Some(backdrop) = &self.backdrop {
            // A full-frame still is stretched, so a replacement may be any
            // size; for the shipped 320x240 page this is a 1:1 blit.
            let shade = shade_scale(self.background.shade());
            framebuffer.draw_background(backdrop, [0, 0], [shade, shade, shade]);
        }
        let plate = self.plate.as_ref();
        let font = cx.font;
        for (index, slot) in self.slots.iter().enumerate() {
            let bytes: &[u8] = match slot.kind {
                SlotKind::ClearTime => &self.clear_bytes,
                SlotKind::SaveCount => &self.save_bytes,
                SlotKind::Sprite => &[],
            };
            let _ = index;
            slot.draw(framebuffer, plate, font, bytes);
        }
        if self.effects && self.confetti_timer < CONFETTI_FRAMES {
            self.confetti.draw(framebuffer);
        }
    }
}

/// The rocket-launcher epilogue state machine.
pub struct EpilogueScreen {
    backdrop: Option<Image>,
    plate: Option<Artwork>,
    captions: [PlateSlot; 2],
    model: Option<Ivm>,
    spin: i16,
    step: u8,
    timer: u16,
    fade: i32,
    done: bool,
    /// The composed joints, kept for the spin test.
    spin_z: i16,
}

impl EpilogueScreen {
    /// An epilogue screen, still without any loaded art.
    pub fn new() -> Self {
        Self {
            backdrop: None,
            plate: None,
            captions: [PlateSlot::from_entry(SlotKind::Sprite, 0, 0, 0, 0); 2],
            model: None,
            spin: 0,
            step: 0,
            timer: 0,
            fade: 0,
            done: false,
            spin_z: 0,
        }
    }

    /// Whether the epilogue has finished.
    pub fn finished(&self) -> bool {
        self.done
    }

    /// The current model spin angle (tests).
    pub fn spin_angle(&self) -> i16 {
        self.spin_z
    }

    /// Build the captions and model transform
    /// (`ending_epilogue_build`, `EndingScreen.cpp:687-778`).
    fn build(&mut self) {
        for (index, entry_index) in [5usize, 6].into_iter().enumerate() {
            let entry = CREDIT_ENTRIES[entry_index];
            let mut slot = PlateSlot::from_entry(
                entry.kind,
                i32::from(entry.x),
                i32::from(entry.y),
                entry.tex_v,
                entry.height,
            );
            // The epilogue captions are fully faded in from the start.
            slot.active = true;
            slot.fade_level = FADE_FULL;
            self.captions[index] = slot;
        }
        self.spin = 0;
        self.spin_z = 0;
    }

    /// The composed root -> spin -> leaf joint matrix
    /// (`ending_epilogue_build`/`update`, `EndingScreen.cpp:756-776`, `824-827`).
    fn joint_matrix(&self) -> Mat4x3 {
        // Root: X rotation 0xF95C, translation (0, 500, 0).
        let root = Mat4x3 {
            r: anim::rotation_matrix(0x0F95C & 0x0FFF, 0, 0),
            t: [0, 500, 0],
        };
        // Spin: Z rotation by the running angle.
        let spin = Mat4x3 {
            r: anim::rotation_matrix(0, 0, i32::from(self.spin)),
            t: [0, 0, 0],
        };
        // Leaf: X rotation 0xAA.
        let leaf = Mat4x3 {
            r: anim::rotation_matrix(0x0AA, 0, 0),
            t: [0, 0, 0],
        };
        anim::compose(&anim::compose(&root, &spin), &leaf)
    }

    /// The fixed epilogue camera.
    fn camera(&self) -> Camera {
        Camera::from_points([12000, 0, 0], [0, 0, 0], EPILOGUE_FOV)
    }
}

impl Default for EpilogueScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen for EpilogueScreen {
    fn open(&mut self, cx: &mut UiContext<'_>) -> Result<()> {
        self.backdrop = match super::load_picture(cx.pack, "ui/rc1121", Geometry::Free, None) {
            Ok(image) => Some(image),
            Err(err) => {
                eprintln!("warning: no epilogue backdrop: {err:#}");
                None
            }
        };
        self.plate = match super::load_page(cx.pack, "ui/en07") {
            Ok(art) => Some(art),
            Err(err) => {
                eprintln!("warning: no epilogue plate: {err:#}");
                None
            }
        };
        self.model = match cx.pack.read("item/i73v.ivm") {
            Ok(bytes) => match ivm::parse(&bytes) {
                Ok(model) => Some(model),
                Err(err) => {
                    eprintln!("warning: invalid item/i73v.ivm: {err:#}");
                    None
                }
            },
            Err(err) => {
                eprintln!("warning: missing item/i73v.ivm: {err:#}");
                None
            }
        };
        self.build();
        Ok(())
    }

    fn update(&mut self, _cx: &UiContext<'_>, _input: UiInput) -> ScreenResult {
        match self.step {
            0 => {
                self.fade = 255;
                self.step = 1;
                self.timer = 0;
            }
            1 => {
                // Fade in from black while the hold counts: the reference's
                // step-1 timer runs from the first update after the build,
                // fade-in included, and starts the fade-out past 0x10D.
                if self.fade > 0 {
                    self.fade = (self.fade - EPILOGUE_FADE_STEP).max(0);
                }
                self.timer += 1;
                if self.timer > EPILOGUE_HOLD {
                    self.step = 2;
                }
            }
            2 => {
                self.fade = (self.fade + EPILOGUE_FADE_STEP).min(255);
                if self.fade >= 255 {
                    self.done = true;
                }
            }
            _ => {}
        }
        // Spin the model about Z one step per frame (`0x20`).
        self.spin = self.spin.wrapping_add(EPILOGUE_SPIN_STEP);
        self.spin_z = self.spin;
        if self.done {
            return ScreenResult::Done(ScreenAction::EndingSave);
        }
        ScreenResult::Continue
    }

    fn draw(&mut self, cx: &UiContext<'_>, framebuffer: &mut Framebuffer) {
        framebuffer.clear();
        framebuffer.fill_rect(
            [0, 0, framebuffer.width as i32, framebuffer.height as i32],
            [0, 0, 0, 255],
        );
        if let Some(backdrop) = &self.backdrop {
            let shade = shade_scale(fade_shade(EPILOGUE_BACKDROP_FADE));
            framebuffer.draw_background(backdrop, [0, 0], [shade, shade, shade]);
        }
        let plate = self.plate.as_ref();
        for slot in &self.captions {
            slot.draw(framebuffer, plate, cx.font, &[]);
        }
        if let Some(model) = &self.model {
            let joints = vec![self.joint_matrix(); model.objects.len()];
            framebuffer.draw_ivm_unlit(model, &joints, &self.camera());
        }
    }

    fn fade(&self) -> u8 {
        self.fade.clamp(0, 255) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `screen` until `predicate` holds, with a tick cap.
    fn drive_until(
        screen: &mut ResultScreen,
        predicate: impl Fn(&ResultScreen) -> bool,
        cap: u32,
    ) -> bool {
        for _ in 0..cap {
            if predicate(screen) {
                return true;
            }
            screen.update(&result_ctx(), UiInput::default());
        }
        predicate(screen)
    }

    #[test]
    fn the_result_screen_runs_the_reference_step_machine() {
        let mut screen = ResultScreen::new(EndStats::debug(1, 0, 1), false);
        assert_eq!(screen.step(), 0);
        // Step 0 arms the background fade and builds the hidden slots.
        screen.update(&result_ctx(), UiInput::default());
        assert_eq!(screen.step(), 1);
        assert!(screen.background.fade_vel > 0);
        assert!(screen.slots.iter().all(|slot| !slot.active));
        // Steps 1-2 reveal the plates in pairs.
        assert!(drive_until(&mut screen, |s| s.step() == 2, 0x80));
        assert!(screen.slots[0].active && screen.slots[1].active);
        assert!(drive_until(&mut screen, |s| s.step() == 3, 0x80));
        assert!(screen.slots[2].active && screen.slots[3].active);
        // Step 3 is the reveal hold; step 4 waits for input.
        assert!(drive_until(&mut screen, |s| s.step() == 4, 0x80));

        // A button press leaves step 4 (ending 1 -> step 5 -> step 10 -> done).
        let mut ticks = 0;
        while !screen.finished() && ticks < 0x400 {
            screen.update(
                &result_ctx(),
                UiInput {
                    any: true,
                    ..UiInput::default()
                },
            );
            ticks += 1;
        }
        assert!(screen.finished(), "the machine reaches its done latch");
    }

    fn result_ctx() -> UiContext<'static> {
        // A null-pack context is enough: update never reads it.
        static PACK: std::sync::OnceLock<crate::pack::Pack> = std::sync::OnceLock::new();
        let pack = PACK.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("arklay-ending-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("empty.akpak");
            crate::pack::PackWriter::new().write(&path).unwrap();
            crate::pack::Pack::open(&path).unwrap()
        });
        static TEXT: std::sync::OnceLock<crate::text::Text> = std::sync::OnceLock::new();
        let text = TEXT.get_or_init(crate::text::Text::default);
        UiContext {
            pack,
            save_dir: std::path::Path::new("."),
            font: None,
            text: Some(text),
            ticks: 0,
            cues: Default::default(),
        }
    }

    #[test]
    fn ending_six_takes_the_extra_plate_steps() {
        let mut screen = ResultScreen::new(EndStats::debug(6, 0, 1), false);
        // Drive to step 4 (background rows clear the reveal the same way).
        assert!(drive_until(&mut screen, |s| s.step() == 4, 0x100));
        screen.update(
            &result_ctx(),
            UiInput {
                any: true,
                ..UiInput::default()
            },
        );
        assert_eq!(screen.step(), 6, "ending 6 diverts to the fifth plate");
        // The fifth plate activates after the 0x5A hold.
        assert!(drive_until(&mut screen, |s| s.step() == 8, 0x100));
        assert!(screen.slots[4].active);
    }

    #[test]
    fn a_no_save_run_enables_the_confetti_bonus() {
        let mut screen = ResultScreen::new(EndStats::debug(1, 0, 1), false);
        screen.step = 10;
        screen.timer = 0x1E;
        screen.update(&result_ctx(), UiInput::default());
        assert!(screen.effects, "a no-save run enables the bonus");
        assert!(!screen.confetti.particles.is_empty());
    }

    #[test]
    fn fade_shade_and_scale_ramp_to_full() {
        assert_eq!(fade_shade(0), 0);
        assert_eq!(fade_shade(FADE_FULL), 0x80);
        assert_eq!(shade_scale(0), 0);
        assert_eq!(shade_scale(0x80), 255);
        assert_eq!(shade_scale(0x40), 127);
    }

    #[test]
    fn the_cleared_once_bit_swaps_the_backdrops() {
        assert_eq!(backdrop_stem(0, false), "ui/chris02");
        assert_eq!(backdrop_stem(1, false), "ui/jill02");
        assert_eq!(backdrop_stem(0, true), "ui/clis01");
        assert_eq!(backdrop_stem(1, true), "ui/jill01");
        // The epilogue's fixed backdrop shade (`s_bgSlot->fadeLevel` 0x6000).
        assert_eq!(fade_shade(EPILOGUE_BACKDROP_FADE), 0x60);
        assert_eq!(shade_scale(fade_shade(EPILOGUE_BACKDROP_FADE)), 191);
    }

    #[test]
    fn the_epilogue_runs_and_spins_the_model() {
        let mut screen = EpilogueScreen::new();
        screen.build();
        assert_eq!(screen.captions[0].fade_level, FADE_FULL);
        let mut ticks = 0;
        while !screen.finished() && ticks < 0x1000 {
            screen.update(&result_ctx(), UiInput::default());
            ticks += 1;
        }
        assert!(screen.finished(), "the epilogue completes");
        assert_ne!(screen.spin_angle(), 0, "the model spins");
    }
}
