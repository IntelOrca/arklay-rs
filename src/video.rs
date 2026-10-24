//! The graphics options and how a frame is laid out in the window.
//!
//! The engine always produces a 320x240 frame: the software rasterizer draws
//! everything at that size and the GPU renderer composites the software 2D
//! layers (also 320x240) over its own 3D scene. These options only decide how
//! that frame reaches the screen:
//!
//! | option         | applies to | effect                                                     |
//! |----------------|------------|------------------------------------------------------------|
//! | `window_width`, `window_height` | both | windowed client size in pixels (F8 picks multiples of 320x240) |
//! | `fullscreen`   | both       | borderless fullscreen on the window's display              |
//! | `display`      | both       | the display (1-based) the window opens on                  |
//! | `render_scale` | GPU        | the 3D scene target as a percentage of the viewport        |
//! | `msaa`         | GPU        | samples per pixel of the 3D scene target                   |
//! | `frame_rate`   | both       | presented frames per second (see below)                    |
//!
//! The viewport is the largest 4:3 rectangle centred in the window's client
//! area ([`fit`]), black around it. A window F8 sized is exactly a multiple of
//! 320x240 and the viewport fills it; a hand-set window size or fullscreen
//! can leave bars and a fractional zoom. The
//! 320x240 layers are scaled to the viewport with nearest sampling at a whole
//! zoom and with sharp (pixel-art) sampling otherwise, which keeps every
//! source pixel the same size and only blends the one screen pixel that
//! straddles two of them. The GPU 3D scene renders at the viewport size times
//! `render_scale` and is filtered to the viewport when the two differ.
//!
//! `frame_rate` only changes presentation. The simulation always steps at the
//! original's fixed 30 Hz tick (16 ms for the non-gameplay screens); at a
//! higher rate the loop presents several frames per tick and draws the scene
//! between the last two ticks ([`crate::tween`]). Every tick is still
//! presented at least once, so the lowest accepted rate is the tick rate, 30.
//! The 16 ms non-gameplay screens (title, menus, door transitions) present
//! once per tick at about 62 fps whatever the rate below that.
//!
//! `--capture`, traces and tests never read these options: a capture draws
//! the exact 320x240 frame in software through a fixed 3x window. A headless
//! `--stats` run times presentation frames only at an explicit
//! `--frame-rate`.
//!
//! # Aspect ratio
//!
//! `aspect` (default `4:3`) is the one option a capture also honours
//! (`--aspect`). A wider aspect is a presentation crop: the frame keeps the
//! full 320 pixel width and shows a [`View`] band of [`Aspect::height`]
//! rows (180 at 16:9, 200 at 16:10) of the 4:3 scene. Gameplay, masks,
//! collision and cameras stay in 4:3 space; the band follows the player
//! ([`follow`]), and the screens lay themselves out in the 320 x
//! [`Aspect::height`] space `api:width()` x `api:height()` report. The
//! aspect is a run-wide setting held per thread ([`aspect`], [`set_aspect`])
//! so tests that pick one do not leak it into each other.

/// The engine's frame size.
pub const NATIVE_WIDTH: u32 = 320;
pub const NATIVE_HEIGHT: u32 = 240;

/// The default window size (3x).
pub const DEFAULT_WINDOW_SIZE: (u32, u32) = (NATIVE_WIDTH * 3, NATIVE_HEIGHT * 3);
/// The largest accepted window side, in pixels.
pub const MAX_WINDOW_SIDE: u32 = 16384;
/// The largest accepted `display` number.
pub const MAX_DISPLAY: u32 = 64;
/// The accepted range of `render_scale`, in percent.
pub const RENDER_SCALE_RANGE: std::ops::RangeInclusive<u32> = 25..=400;
/// The accepted `msaa` sample counts.
pub const MSAA_COUNTS: [u32; 4] = [1, 2, 4, 8];
/// The simulation's tick rate and the default `frame_rate`.
pub const TICK_RATE: u32 = 30;
/// The smallest accepted `frame_rate`: the loop presents every tick, so a
/// lower rate could not be honoured.
pub const MIN_FRAME_RATE: u32 = TICK_RATE;
/// The largest accepted `frame_rate`.
pub const MAX_FRAME_RATE: u32 = 1000;
/// The rates the options screen steps through (the config takes any
/// integer from [`MIN_FRAME_RATE`] to [`MAX_FRAME_RATE`]).
pub const FRAME_RATE_PRESETS: [u32; 6] = [30, 60, 120, 144, 165, 240];

/// The preset after (`delta > 0`) or before `rate`, wrapping; a rate
/// between two presets steps to the neighbour on that side.
pub fn step_frame_rate(rate: u32, delta: i32) -> u32 {
    let presets = &FRAME_RATE_PRESETS;
    if delta > 0 {
        presets
            .iter()
            .copied()
            .find(|&preset| preset > rate)
            .unwrap_or(presets[0])
    } else {
        presets
            .iter()
            .rev()
            .copied()
            .find(|&preset| preset < rate)
            .unwrap_or(presets[presets.len() - 1])
    }
}

/// The widest canvas a frame may be: 32:9 at the native 240 rows.
pub const MAX_CANVAS_WIDTH: u32 = 854;

/// A display aspect ratio, `width:height`, no narrower than 4:3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aspect {
    pub width: u32,
    pub height: u32,
}

impl Default for Aspect {
    fn default() -> Self {
        Self::STANDARD
    }
}

impl Aspect {
    /// The original 4:3 frame.
    pub const STANDARD: Self = Self {
        width: 4,
        height: 3,
    };
    /// 16:9 widescreen.
    pub const WIDE: Self = Self {
        width: 16,
        height: 9,
    };
    /// The aspects the options screen steps through.
    pub const PRESETS: [Self; 2] = [Self::STANDARD, Self::WIDE];
    /// The shortest band an aspect may show (an aspect wider than 32:9 is
    /// refused rather than cropped to a sliver).
    pub const MIN_HEIGHT: u32 = 90;

    /// Parse `W:H` (`4:3`, `16:9`, `16:10`, ...). An aspect narrower than
    /// 4:3 or wider than 32:9 is refused.
    pub fn parse(text: &str) -> Option<Self> {
        let (width, height) = text.trim().split_once(':')?;
        let aspect = Self {
            width: width.trim().parse().ok()?,
            height: height.trim().parse().ok()?,
        };
        let valid = aspect.width > 0
            && aspect.height > 0
            && u64::from(aspect.width) * 3 >= u64::from(aspect.height) * 4
            && aspect.band_height() >= Self::MIN_HEIGHT;
        valid.then_some(aspect)
    }

    /// The config spelling.
    pub fn name(self) -> String {
        format!("{}:{}", self.width, self.height)
    }

    /// The visible rows of the 320 pixel wide frame, rounded to even.
    fn band_height(self) -> u32 {
        let rows = u64::from(NATIVE_WIDTH) * u64::from(self.height) / u64::from(self.width.max(1));
        ((rows as u32) & !1).min(NATIVE_HEIGHT)
    }

    /// The visible rows of the frame: 240 at 4:3, 180 at 16:9.
    pub fn height(self) -> u32 {
        self.band_height().max(Self::MIN_HEIGHT.min(NATIVE_HEIGHT))
    }

    /// Whether this is the full 4:3 frame.
    pub fn is_standard(self) -> bool {
        self.height() == NATIVE_HEIGHT
    }

    /// The preset after (`delta > 0`) or before this aspect, wrapping; an
    /// aspect that is not a preset steps to the first.
    pub fn step(self, delta: i32) -> Self {
        let presets = &Self::PRESETS;
        let Some(index) = presets.iter().position(|&preset| preset == self) else {
            return presets[0];
        };
        let len = presets.len() as i32;
        presets[(index as i32 + delta.signum()).rem_euclid(len) as usize]
    }

    /// The width of a full-height canvas of this shape: 320 at 4:3, 427 at
    /// 16:9. Screens drawn on black (the pause-menu panel) take a canvas
    /// this wide instead of the cropped band, so they keep every row.
    pub fn canvas_width(self) -> u32 {
        let width = (u64::from(NATIVE_HEIGHT) * u64::from(self.width)
            + u64::from(self.height.max(1)) / 2)
            / u64::from(self.height.max(1));
        (width as u32).clamp(NATIVE_WIDTH, MAX_CANVAS_WIDTH)
    }

    /// The band centred on the 4:3 frame.
    pub fn centred(self) -> View {
        let height = self.height();
        View {
            top: (NATIVE_HEIGHT - height) / 2,
            height,
            pan: 0,
        }
    }
}

thread_local! {
    static ASPECT: std::cell::Cell<Aspect> = const { std::cell::Cell::new(Aspect::STANDARD) };
}

/// The aspect this thread presents at.
pub fn aspect() -> Aspect {
    ASPECT.with(std::cell::Cell::get)
}

/// Present this thread's frames at `aspect` from now on.
pub fn set_aspect(aspect: Aspect) {
    ASPECT.with(|cell| cell.set(aspect));
}

/// The band a frame whose own view is `view` presents at this thread's
/// aspect: `view` when it has the aspect's height or is the whole frame
/// (presented pillarboxed: the F1 debug menu), else the centred band (a
/// band of another aspect's height, from before a change).
pub fn presented(view: View) -> View {
    let aspect = aspect();
    if view.height == aspect.height() || view.is_full() {
        view
    } else {
        aspect.centred()
    }
}

/// The part of a `width` x `height` frame the window shows: a 320x240
/// frame's [`presented`] band, or the whole of any other frame (a wider
/// aspect's canvas).
pub fn shown(width: u32, height: u32, view: View) -> Shown {
    if width == NATIVE_WIDTH && height == NATIVE_HEIGHT {
        let band = presented(view);
        Shown {
            top: band.top,
            pan: f32::from(band.pan) / 256.0,
            width,
            height: band.height,
        }
    } else {
        Shown {
            top: 0,
            pan: 0.0,
            width,
            height,
        }
    }
}

/// The rows `top..top + height` of a frame, all `width` columns: what the
/// window shows of it ([`shown`]), moved down `pan` (under one) rows more.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shown {
    pub top: u32,
    pub pan: f32,
    pub width: u32,
    pub height: u32,
}

/// The visible band of a 320x240 frame: rows `top..top + height`,
/// presented `pan` 256ths of a row lower still. Drawing ignores the pan: it
/// is the presenter's sub-row offset that smooths a pan between ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    pub top: u32,
    pub height: u32,
    pub pan: u8,
}

impl Default for View {
    fn default() -> Self {
        Self {
            top: 0,
            height: NATIVE_HEIGHT,
            pan: 0,
        }
    }
}

impl View {
    /// Whether the band is the whole frame.
    pub fn is_full(self) -> bool {
        self.top == 0 && self.height == NATIVE_HEIGHT
    }

    /// The band of `height` rows whose top is `top`, kept inside the frame.
    pub fn at(top: i32, height: u32) -> Self {
        let height = height.min(NATIVE_HEIGHT);
        let max = (NATIVE_HEIGHT - height) as i32;
        Self {
            top: top.clamp(0, max) as u32,
            height,
            pan: 0,
        }
    }

    /// The band `alpha` of the way from `prev` (the previous tick's) to
    /// `self`, to the 256th of a row: a frame drawn between two ticks pans
    /// smoothly instead of stepping at the tick rate. A band of another
    /// height (an aspect change) does not blend.
    pub fn between(self, prev: View, alpha: f32) -> View {
        if prev.height != self.height || prev.top == self.top {
            return self;
        }
        let (from, to) = (prev.top as f32, self.top as f32);
        let fine = ((from + (to - from) * alpha.clamp(0.0, 1.0)) * 256.0).round() as u32;
        let (top, pan) = (fine / 256, (fine % 256) as u8);
        // The pan shows part of the row below the band, which must exist.
        let mut view = View::at(top as i32, self.height);
        if pan != 0 && view.top == top && top + self.height < NATIVE_HEIGHT {
            view.pan = pan;
        }
        view
    }
}

/// How far the gameplay band moves towards its target per tick, at most.
pub const FOLLOW_STEP: i32 = 4;
/// Rows of the band's middle the player may move in without it following.
pub const FOLLOW_DEAD_ZONE: i32 = 24;

/// The band's top after one tick of following a point at screen row
/// `target` (the player's projected body, `None` off screen) from `top`.
/// `snap` (a camera cut, a room entry) jumps straight to the target;
/// otherwise the band only moves once the point leaves the dead zone around
/// its centre, and then by at most [`FOLLOW_STEP`] rows.
pub fn follow(top: u32, height: u32, target: Option<i32>, snap: bool) -> u32 {
    let height = height.min(NATIVE_HEIGHT);
    let max = (NATIVE_HEIGHT - height) as i32;
    let Some(target) = target else {
        return if snap {
            (max / 2) as u32
        } else {
            top.min(max as u32)
        };
    };
    let half = height as i32 / 2;
    let wanted = (target - half).clamp(0, max);
    let top = top as i32;
    if snap {
        return wanted as u32;
    }
    let centre = top + half;
    let slack = FOLLOW_DEAD_ZONE.min(half);
    let wanted = if (target - centre).abs() <= slack {
        top
    } else if target > centre {
        (target - slack - half).clamp(0, max)
    } else {
        (target + slack - half).clamp(0, max)
    };
    (top + (wanted - top).clamp(-FOLLOW_STEP, FOLLOW_STEP)).clamp(0, max) as u32
}

/// The band's top after one tick of moving from `top` towards the centred
/// band, at most [`FOLLOW_STEP`] rows (`snap` jumps straight there). The
/// cutscene letterbox bars cover the frame's top and bottom 28 rows, so a
/// centred band crops them evenly.
pub fn centre(top: u32, height: u32, snap: bool) -> u32 {
    let height = height.min(NATIVE_HEIGHT);
    let wanted = ((NATIVE_HEIGHT - height) / 2) as i32;
    if snap {
        return wanted as u32;
    }
    let top = top as i32;
    (top + (wanted - top).clamp(-FOLLOW_STEP, FOLLOW_STEP)) as u32
}

/// Which renderer an interactive window draws the gameplay scene with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RendererKind {
    /// The software rasterizer, scaled to the window.
    Software,
    /// The SDL_GPU scene renderer ([`crate::gpu`]) and the default; falls back
    /// to software when no device is available or the device fails mid-game.
    #[default]
    Gpu,
}

impl RendererKind {
    /// The config spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Gpu => "gpu",
        }
    }

    /// Parse the config spelling.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "software" => Some(Self::Software),
            "gpu" => Some(Self::Gpu),
            _ => None,
        }
    }
}

/// The `[graphics]` options of `arklay.toml` (after the command line's
/// overrides). Captures, traces, tests and headless runs ignore them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Graphics {
    pub renderer: RendererKind,
    /// Borderless fullscreen rather than a window.
    pub fullscreen: bool,
    /// The windowed client size in pixels.
    pub window_width: u32,
    pub window_height: u32,
    /// The display the window opens on, 1-based in SDL's display order.
    pub display: u32,
    /// The GPU scene target's size as a percentage of the viewport.
    pub render_scale: u32,
    /// The GPU scene target's samples per pixel.
    pub msaa: u32,
    /// Presented frames per second; the simulation stays at [`TICK_RATE`].
    pub frame_rate: u32,
    /// The display aspect ratio: 4:3 or a wider crop.
    pub aspect: Aspect,
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            renderer: RendererKind::Gpu,
            fullscreen: false,
            window_width: DEFAULT_WINDOW_SIZE.0,
            window_height: DEFAULT_WINDOW_SIZE.1,
            display: 1,
            render_scale: 100,
            msaa: 1,
            frame_rate: TICK_RATE,
            aspect: Aspect::STANDARD,
        }
    }
}

/// Where the frame lands in the window's client area, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    /// Whether the viewport is an exact whole multiple of 320x240.
    pub fn is_whole_zoom(&self) -> bool {
        self.is_whole_zoom_of(NATIVE_HEIGHT)
    }

    /// Whether the viewport is an exact whole multiple of a 320 x `rows`
    /// frame.
    pub fn is_whole_zoom_of(&self, rows: u32) -> bool {
        self.is_whole_zoom_of_size(NATIVE_WIDTH, rows)
    }

    /// Whether the viewport is an exact whole multiple of a `width` x
    /// `height` frame.
    pub fn is_whole_zoom_of_size(&self, width: u32, height: u32) -> bool {
        let (width, height) = (width.max(1), height.max(1));
        self.width.is_multiple_of(width)
            && self.height.is_multiple_of(height)
            && self.width / width == self.height / height
    }
}

/// The largest 4:3 rectangle centred in a `width` x `height` client area.
pub fn fit(width: u32, height: u32) -> Viewport {
    fit_rows(width, height, NATIVE_HEIGHT)
}

/// The largest rectangle with a 320 x `rows` frame's shape centred in a
/// `width` x `height` client area.
pub fn fit_rows(width: u32, height: u32, rows: u32) -> Viewport {
    fit_size(width, height, NATIVE_WIDTH, rows)
}

/// The largest rectangle with a `frame_w` x `frame_h` frame's shape centred
/// in a `width` x `height` client area.
pub fn fit_size(width: u32, height: u32, frame_w: u32, frame_h: u32) -> Viewport {
    let (width, height) = (width.max(1), height.max(1));
    let (frame_w, frame_h) = (u64::from(frame_w.max(1)), u64::from(frame_h.max(1)));
    let (w, h) = if u64::from(width) * frame_h >= u64::from(height) * frame_w {
        (
            ((u64::from(height) * frame_w + frame_h / 2) / frame_h) as u32,
            height,
        )
    } else {
        (
            width,
            ((u64::from(width) * frame_h + frame_w / 2) / frame_w) as u32,
        )
    };
    let (w, h) = (w.clamp(1, width), h.clamp(1, height));
    Viewport {
        x: (width - w) / 2,
        y: (height - h) / 2,
        width: w,
        height: h,
    }
}

/// The viewport the whole `frame_height` rows tall frame would take so that
/// its `shown` rows land exactly on `band` (the [`fit_size`] rectangle): the
/// GPU scene and the layers are drawn into it and clipped to `band`.
pub fn frame_viewport(band: Viewport, shown: Shown, frame_height: u32) -> (f32, f32, f32, f32) {
    let scale = band.height as f32 / shown.height.max(1) as f32;
    (
        band.x as f32,
        band.y as f32 - (shown.top as f32 + shown.pan) * scale,
        band.width as f32,
        frame_height as f32 * scale,
    )
}

/// The viewport a `width` x `height` image takes inside a 4:3 frame's
/// `frame_viewport`: fitted to its own aspect and centred, so a full-screen
/// film keeps its shape wherever the frame lands.
pub fn image_viewport(
    (x, y, w, h): (f32, f32, f32, f32),
    width: u32,
    height: u32,
) -> (f32, f32, f32, f32) {
    let inner = fit_size(w.round() as u32, h.round() as u32, width, height);
    (
        x + inner.x as f32,
        y + inner.y as f32,
        inner.width as f32,
        inner.height as f32,
    )
}

/// The GPU scene target for `viewport` at `render_scale` percent.
pub fn scene_size(viewport: Viewport, render_scale: u32) -> (u32, u32) {
    let scale = |size: u32| ((u64::from(size) * u64::from(render_scale) + 50) / 100).max(1) as u32;
    (scale(viewport.width), scale(viewport.height))
}

/// The largest whole multiple of 320x240 that fits in `room` (a client
/// area, in pixels); at least 320x240.
pub fn largest_window(room: (u32, u32)) -> (u32, u32) {
    largest_window_rows(room, NATIVE_HEIGHT)
}

/// The largest whole multiple of a 320 x `rows` frame that fits in `room`;
/// at least one.
pub fn largest_window_rows(room: (u32, u32), rows: u32) -> (u32, u32) {
    let rows = rows.max(1);
    let times = (room.0 / NATIVE_WIDTH).min(room.1 / rows).max(1);
    (NATIVE_WIDTH * times, rows * times)
}

/// The smallest whole multiple of a 320 x `rows` frame larger than every
/// one a `width` x `height` window holds.
fn next_window(width: u32, height: u32, rows: u32) -> (u32, u32) {
    let rows = rows.max(1);
    let times = (width / NATIVE_WIDTH).min(height / rows) + 1;
    (NATIVE_WIDTH * times, rows * times)
}

/// The window state F8 steps to: the next whole multiple of the frame (320 x
/// [`Aspect::height`]) above
/// the window's current size, borderless fullscreen once that would not fit
/// in `room` (the client area the display allows, in pixels), then back to
/// 320x240. Going fullscreen keeps the window size, so Alt+Enter returns to
/// that window.
pub fn step_window(graphics: &mut Graphics, room: (u32, u32)) {
    let rows = graphics.aspect.height();
    let (width, height) = next_window(graphics.window_width, graphics.window_height, rows);
    if graphics.fullscreen {
        graphics.fullscreen = false;
        (graphics.window_width, graphics.window_height) = (NATIVE_WIDTH, rows);
    } else if width <= room.0 && height <= room.1 {
        (graphics.window_width, graphics.window_height) = (width, height);
    } else {
        graphics.fullscreen = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gpu_renderer_is_the_default() {
        assert_eq!(Graphics::default().renderer, RendererKind::Gpu);
        assert_eq!(RendererKind::default(), RendererKind::Gpu);
    }

    #[test]
    fn an_hd_image_fills_the_frame_viewport_at_its_own_aspect() {
        // A 4:3 film takes the whole 4:3 frame, pillarboxed in a 16:10 window.
        let frame = (133.0, 0.0, 1333.0, 1000.0);
        assert_eq!(image_viewport(frame, 1280, 960), frame);
        // A 16:9 film is letterboxed inside it, the bars split evenly, and
        // the frame's own offset carries over.
        assert_eq!(
            image_viewport((0.0, -60.0, 1280.0, 960.0), 1920, 1080),
            (0.0, 60.0, 1280.0, 720.0)
        );
    }

    #[test]
    fn frame_rate_steps_through_the_presets_and_wraps() {
        assert_eq!(step_frame_rate(30, 1), 60);
        assert_eq!(step_frame_rate(240, 1), 30);
        assert_eq!(step_frame_rate(30, -1), 240);
        assert_eq!(step_frame_rate(144, -1), 120);
        // A configured rate between presets steps to the neighbour.
        assert_eq!(step_frame_rate(75, 1), 120);
        assert_eq!(step_frame_rate(75, -1), 60);
        assert_eq!(step_frame_rate(1000, 1), 30);
    }

    #[test]
    fn a_band_between_ticks_pans_by_fractions_of_a_row() {
        let prev = View::at(30, 180);
        let cur = View::at(34, 180);
        assert_eq!(cur.between(prev, 0.0), prev);
        assert_eq!(cur.between(prev, 1.0), cur);
        let half = cur.between(prev, 0.5);
        assert_eq!((half.top, half.pan), (32, 0));
        let eighth = cur.between(prev, 0.125);
        assert_eq!((eighth.top, eighth.pan), (30, 128));
        // Panning up blends the same way.
        let up = prev.between(cur, 0.125);
        assert_eq!((up.top, up.pan), (33, 128));
        // A band of another height snaps.
        assert_eq!(View::at(20, 200).between(prev, 0.5), View::at(20, 200));
    }

    #[test]
    fn the_letterboxed_band_steps_to_the_centre() {
        // 16:9's 180-row band centres at row 30, cropping both 28-row bars.
        assert_eq!(centre(60, 180, true), 30);
        assert_eq!(centre(0, 180, false), 4);
        assert_eq!(centre(60, 180, false), 56);
        assert_eq!(centre(32, 180, false), 30);
        assert_eq!(centre(30, 180, false), 30);
        // 16:10's 200-row band centres at row 20.
        assert_eq!(centre(40, 200, true), 20);
    }

    #[test]
    fn fit_centres_the_largest_four_by_three_rectangle() {
        let full = |x, y, width, height| Viewport {
            x,
            y,
            width,
            height,
        };
        assert_eq!(fit(960, 720), full(0, 0, 960, 720));
        assert_eq!(fit(1920, 1080), full(240, 0, 1440, 1080));
        assert_eq!(fit(2560, 1440), full(320, 0, 1920, 1440));
        assert_eq!(fit(1280, 1024), full(0, 32, 1280, 960));
        assert_eq!(fit(3440, 1440), full(760, 0, 1920, 1440));
        assert!(fit(960, 720).is_whole_zoom());
        assert!(fit(2560, 1440).is_whole_zoom());
        assert!(!fit(1920, 1080).is_whole_zoom());
        assert_eq!(fit(0, 0).width, 1);
    }

    #[test]
    fn scene_size_follows_the_viewport() {
        let viewport = fit(1920, 1080);
        assert_eq!(scene_size(viewport, 100), (1440, 1080));
        assert_eq!(scene_size(viewport, 50), (720, 540));
        assert_eq!(scene_size(viewport, 75), (1080, 810));
        assert_eq!(scene_size(viewport, 200), (2880, 2160));
    }

    #[test]
    fn f8_steps_up_to_fullscreen_then_back_to_native() {
        // 1920x1080 less a 48 px task bar and a 31 px title bar.
        let room = (1920, 1001);
        assert_eq!(largest_window(room), (1280, 960));
        let mut graphics = Graphics::default();
        let state = |g: &Graphics| (g.fullscreen, g.window_width, g.window_height);
        step_window(&mut graphics, room);
        assert_eq!(state(&graphics), (false, 1280, 960));
        step_window(&mut graphics, room);
        assert_eq!(state(&graphics), (true, 1280, 960));
        step_window(&mut graphics, room);
        assert_eq!(state(&graphics), (false, 320, 240));
        // A hand-set size steps to the next multiple above its whole zoom.
        (graphics.window_width, graphics.window_height) = (1000, 700);
        step_window(&mut graphics, room);
        assert_eq!(state(&graphics), (false, 960, 720));
        (graphics.window_width, graphics.window_height) = (200, 100);
        step_window(&mut graphics, room);
        assert_eq!(state(&graphics), (false, 320, 240));
        // A window already too large for its display goes fullscreen next.
        (graphics.window_width, graphics.window_height) = (1920, 1440);
        step_window(&mut graphics, room);
        assert!(graphics.fullscreen);
        assert_eq!(largest_window((200, 100)), (320, 240));
    }
}
