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
//! `--capture`, traces and tests never read these options: a capture draws
//! the exact 320x240 frame in software through a fixed 3x window.

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

/// Which renderer an interactive window draws the gameplay scene with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RendererKind {
    /// The software rasterizer, scaled to the window (the default).
    #[default]
    Software,
    /// The SDL_GPU scene renderer ([`crate::gpu`]); falls back to software
    /// when no device is available.
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
/// overrides). Captures, `--stats` runs and tests ignore them.
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
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            renderer: RendererKind::Software,
            fullscreen: false,
            window_width: DEFAULT_WINDOW_SIZE.0,
            window_height: DEFAULT_WINDOW_SIZE.1,
            display: 1,
            render_scale: 100,
            msaa: 1,
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
        self.width.is_multiple_of(NATIVE_WIDTH)
            && self.height.is_multiple_of(NATIVE_HEIGHT)
            && self.width / NATIVE_WIDTH == self.height / NATIVE_HEIGHT
    }
}

/// The largest 4:3 rectangle centred in a `width` x `height` client area.
pub fn fit(width: u32, height: u32) -> Viewport {
    let (width, height) = (width.max(1), height.max(1));
    let (w, h) = if u64::from(width) * 3 >= u64::from(height) * 4 {
        ((height * 4 + 1) / 3, height)
    } else {
        (width, (width * 3 + 2) / 4)
    };
    let (w, h) = (w.clamp(1, width), h.clamp(1, height));
    Viewport {
        x: (width - w) / 2,
        y: (height - h) / 2,
        width: w,
        height: h,
    }
}

/// The GPU scene target for `viewport` at `render_scale` percent.
pub fn scene_size(viewport: Viewport, render_scale: u32) -> (u32, u32) {
    let scale = |size: u32| ((u64::from(size) * u64::from(render_scale) + 50) / 100).max(1) as u32;
    (scale(viewport.width), scale(viewport.height))
}

/// The largest whole multiple of 320x240 that fits in `room` (a client
/// area, in pixels); at least 320x240.
pub fn largest_window(room: (u32, u32)) -> (u32, u32) {
    let times = (room.0 / NATIVE_WIDTH).min(room.1 / NATIVE_HEIGHT).max(1);
    (NATIVE_WIDTH * times, NATIVE_HEIGHT * times)
}

/// The smallest whole multiple of 320x240 larger than every one a `width` x
/// `height` window holds.
fn next_window(width: u32, height: u32) -> (u32, u32) {
    let times = (width / NATIVE_WIDTH).min(height / NATIVE_HEIGHT) + 1;
    (NATIVE_WIDTH * times, NATIVE_HEIGHT * times)
}

/// The window state F8 steps to: the next whole multiple of 320x240 above
/// the window's current size, borderless fullscreen once that would not fit
/// in `room` (the client area the display allows, in pixels), then back to
/// 320x240. Going fullscreen keeps the window size, so Alt+Enter returns to
/// that window.
pub fn step_window(graphics: &mut Graphics, room: (u32, u32)) {
    let (width, height) = next_window(graphics.window_width, graphics.window_height);
    if graphics.fullscreen {
        graphics.fullscreen = false;
        (graphics.window_width, graphics.window_height) = (NATIVE_WIDTH, NATIVE_HEIGHT);
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
