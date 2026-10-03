//! Engine entry point: open a pack, load a room, display and cycle its cuts.

use std::ffi::{CStr, CString, c_void};
use std::path::Path;

use anyhow::{Context, Result, bail};

use sdl3_sys::blendmode::SDL_BLENDMODE_NONE;
use sdl3_sys::error::SDL_GetError;
use sdl3_sys::events::{SDL_EVENT_KEY_DOWN, SDL_EVENT_QUIT, SDL_Event, SDL_PollEvent};
use sdl3_sys::hints::{SDL_HINT_RENDER_DRIVER, SDL_HINT_VIDEO_DRIVER, SDL_SetHint};
use sdl3_sys::init::{SDL_INIT_VIDEO, SDL_Init, SDL_Quit};
use sdl3_sys::keycode::{SDL_KMOD_NONE, SDL_KMOD_SHIFT, SDLK_COMMA, SDLK_ESCAPE, SDLK_PERIOD};
use sdl3_sys::main::SDL_SetMainReady;
use sdl3_sys::pixels::SDL_PIXELFORMAT_ABGR8888;
use sdl3_sys::rect::SDL_Rect;
use sdl3_sys::render::{
    SDL_CreateRenderer, SDL_CreateTexture, SDL_DestroyRenderer, SDL_DestroyTexture,
    SDL_RenderClear, SDL_RenderPresent, SDL_RenderReadPixels, SDL_RenderTexture, SDL_Renderer,
    SDL_SetRenderDrawColor, SDL_SetRenderVSync, SDL_SetTextureBlendMode, SDL_SetTextureScaleMode,
    SDL_TEXTUREACCESS_STREAMING, SDL_Texture, SDL_UpdateTexture,
};
use sdl3_sys::surface::{
    SDL_ConvertSurface, SDL_DestroySurface, SDL_SCALEMODE_NEAREST, SDL_Surface,
};
use sdl3_sys::video::{
    SDL_CreateWindow, SDL_DestroyWindow, SDL_SetWindowTitle, SDL_Window, SDL_WindowFlags,
};

use crate::audio::{self, MusicPlayer};
use crate::bmp;
use crate::music;
use crate::pack::Pack;
use crate::rdt;
use crate::state::{Image, RoomId};

const WIDTH: i32 = 320;
const HEIGHT: i32 = 240;
const SCALE: i32 = 3;
const WINDOW_WIDTH: i32 = WIDTH * SCALE;
const WINDOW_HEIGHT: i32 = HEIGHT * SCALE;

struct SdlHandle;

impl Drop for SdlHandle {
    fn drop(&mut self) {
        unsafe { SDL_Quit() };
    }
}

struct WindowHandle(*mut SDL_Window);

impl Drop for WindowHandle {
    fn drop(&mut self) {
        unsafe { SDL_DestroyWindow(self.0) };
    }
}

struct RendererHandle(*mut SDL_Renderer);

impl Drop for RendererHandle {
    fn drop(&mut self) {
        unsafe { SDL_DestroyRenderer(self.0) };
    }
}

struct TextureHandle(*mut SDL_Texture);

impl Drop for TextureHandle {
    fn drop(&mut self) {
        unsafe { SDL_DestroyTexture(self.0) };
    }
}

struct SurfaceHandle(*mut SDL_Surface);

impl Drop for SurfaceHandle {
    fn drop(&mut self) {
        unsafe { SDL_DestroySurface(self.0) };
    }
}

/// Load `room` from `pack`, display its cuts, and optionally capture cut zero.
pub fn run(pack: &Path, id: RoomId, capture: Option<&Path>) -> Result<()> {
    let pack = Pack::open(pack)?;
    let rdt_bytes = pack.read(&id.rdt_entry())?;
    let mut state = rdt::parse(rdt_bytes, id)?;
    if state.cuts.is_empty() {
        bail!("room {} has no camera cuts", id.room3());
    }
    for cut in &mut state.cuts {
        let path = id.cut_entry(cut.index);
        let bytes = pack
            .read(&path)
            .with_context(|| format!("missing background for cut {}", cut.index))?;
        cut.background = Some(bmp::decode(bytes).with_context(|| format!("invalid {path}"))?);
    }

    // Room music: for now the engine plays the room's primary track from the
    // hardcoded table. The SCD will start/stop/pan/volume tracks later and the
    // table itself should move to a game init script in the pack.
    let music_wav = load_room_music(&pack, id);

    if capture.is_some() {
        unsafe {
            SDL_SetHint(SDL_HINT_VIDEO_DRIVER, c"offscreen".as_ptr());
            SDL_SetHint(SDL_HINT_RENDER_DRIVER, c"software".as_ptr());
        }
    }

    unsafe { SDL_SetMainReady() };
    if !unsafe { SDL_Init(SDL_INIT_VIDEO) } {
        bail!("SDL_Init failed: {}", sdl_error());
    }
    let _sdl = SdlHandle;

    let room3 = id.room3();
    let title = window_title(&room3, state.current_cut, state.cuts.len())?;
    let window = unsafe {
        SDL_CreateWindow(
            title.as_ptr(),
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            SDL_WindowFlags::default(),
        )
    };
    if window.is_null() {
        bail!("SDL_CreateWindow failed: {}", sdl_error());
    }
    let _window = WindowHandle(window);

    let renderer = unsafe { SDL_CreateRenderer(window, std::ptr::null()) };
    if renderer.is_null() {
        bail!("SDL_CreateRenderer failed: {}", sdl_error());
    }
    let _renderer = RendererHandle(renderer);
    let _ = unsafe { SDL_SetRenderVSync(renderer, 1) };

    let texture = unsafe {
        SDL_CreateTexture(
            renderer,
            SDL_PIXELFORMAT_ABGR8888,
            SDL_TEXTUREACCESS_STREAMING,
            WIDTH,
            HEIGHT,
        )
    };
    if texture.is_null() {
        bail!("SDL_CreateTexture failed: {}", sdl_error());
    }
    let _texture = TextureHandle(texture);

    if !unsafe { SDL_SetTextureBlendMode(texture, SDL_BLENDMODE_NONE) } {
        bail!("SDL_SetTextureBlendMode failed: {}", sdl_error());
    }
    if !unsafe { SDL_SetTextureScaleMode(texture, SDL_SCALEMODE_NEAREST) } {
        bail!("SDL_SetTextureScaleMode failed: {}", sdl_error());
    }
    if !unsafe { SDL_SetRenderDrawColor(renderer, 0, 0, 0, 255) } {
        bail!("SDL_SetRenderDrawColor failed: {}", sdl_error());
    }

    if let Some(capture_path) = capture {
        let image = state.cuts[0]
            .background
            .as_ref()
            .context("cut 0 has no background")?;
        draw(renderer, texture, image)?;
        capture_frame(renderer, capture_path)?;
        return Ok(());
    }

    let mut music = start_music(music_wav);

    let mut event = SDL_Event::default();
    loop {
        while unsafe { SDL_PollEvent(&mut event) } {
            let kind = unsafe { event.r#type };
            if kind == SDL_EVENT_QUIT {
                return Ok(());
            }
            if kind == SDL_EVENT_KEY_DOWN {
                let key = unsafe { event.key.key };
                let modifiers = unsafe { event.key.r#mod };
                if key == SDLK_ESCAPE {
                    return Ok(());
                }
                if modifiers & SDL_KMOD_SHIFT != SDL_KMOD_NONE {
                    let count = state.cuts.len();
                    let moved = if key == SDLK_COMMA {
                        state.current_cut = (state.current_cut + count - 1) % count;
                        true
                    } else if key == SDLK_PERIOD {
                        state.current_cut = (state.current_cut + 1) % count;
                        true
                    } else {
                        false
                    };
                    if moved {
                        let title = window_title(&room3, state.current_cut, count)?;
                        if !unsafe { SDL_SetWindowTitle(window, title.as_ptr()) } {
                            bail!("SDL_SetWindowTitle failed: {}", sdl_error());
                        }
                    }
                }
            }
        }

        let image = state.cuts[state.current_cut]
            .background
            .as_ref()
            .context("current cut has no background")?;
        draw(renderer, texture, image)?;
        if let Some(player) = &mut music {
            player.update();
        }
        if !unsafe { SDL_RenderPresent(renderer) } {
            bail!("SDL_RenderPresent failed: {}", sdl_error());
        }
    }
}

fn sdl_error() -> String {
    unsafe { CStr::from_ptr(SDL_GetError()) }
        .to_string_lossy()
        .into_owned()
}

fn window_title(room: &str, cut: usize, count: usize) -> Result<CString> {
    CString::new(format!("Arklay - room {room} cut {cut}/{}", count - 1))
        .context("window title contains a NUL byte")
}

/// Load the room's primary music track from the pack, if the table names one.
fn load_room_music(pack: &Pack, id: RoomId) -> Option<audio::Wav> {
    let (name, _looping) = music::primary_track(id)?;
    let path = music::pack_path(name)?;
    match pack.read(&path) {
        Ok(bytes) => match audio::parse_wav(bytes) {
            Ok(wav) => Some(wav),
            Err(err) => {
                eprintln!("warning: invalid music track {path}: {err:#}");
                None
            }
        },
        Err(err) => {
            eprintln!("warning: missing music track {path}: {err:#}");
            None
        }
    }
}

/// Open the audio device and start looping `wav`, if one was loaded.
///
/// Audio is best-effort: a missing device or an SDL failure logs a warning and
/// the engine keeps running silently.
fn start_music(wav: Option<audio::Wav>) -> Option<MusicPlayer> {
    let wav = wav?;
    let mut player = match MusicPlayer::open() {
        Some(player) => player,
        None => {
            eprintln!("warning: no audio device; continuing without music");
            return None;
        }
    };
    if let Err(err) = player.play(wav) {
        eprintln!("warning: failed to start music: {err:#}");
        return None;
    }
    Some(player)
}

fn draw(renderer: *mut SDL_Renderer, texture: *mut SDL_Texture, image: &Image) -> Result<()> {
    if image.width == 0
        || image.height == 0
        || image.width > WIDTH as u32
        || image.height > HEIGHT as u32
    {
        bail!(
            "expected a background up to {WIDTH}x{HEIGHT}, got {}x{}",
            image.width,
            image.height
        );
    }
    if image.rgba.len() != (image.width * image.height * 4) as usize {
        bail!(
            "background is {}x{} but has {} bytes",
            image.width,
            image.height,
            image.rgba.len()
        );
    }
    let rect = SDL_Rect {
        x: 0,
        y: 0,
        w: image.width as i32,
        h: image.height as i32,
    };
    if !unsafe {
        SDL_UpdateTexture(
            texture,
            &rect,
            image.rgba.as_ptr().cast::<c_void>(),
            (image.width * 4) as i32,
        )
    } {
        bail!("SDL_UpdateTexture failed: {}", sdl_error());
    }
    if !unsafe { SDL_RenderClear(renderer) } {
        bail!("SDL_RenderClear failed: {}", sdl_error());
    }
    if !unsafe { SDL_RenderTexture(renderer, texture, std::ptr::null(), std::ptr::null()) } {
        bail!("SDL_RenderTexture failed: {}", sdl_error());
    }
    Ok(())
}

fn capture_frame(renderer: *mut SDL_Renderer, path: &Path) -> Result<()> {
    let surface = unsafe { SDL_RenderReadPixels(renderer, std::ptr::null()) };
    if surface.is_null() {
        bail!("SDL_RenderReadPixels failed: {}", sdl_error());
    }
    let _surface = SurfaceHandle(surface);

    let converted = unsafe { SDL_ConvertSurface(surface, SDL_PIXELFORMAT_ABGR8888) };
    if converted.is_null() {
        bail!("SDL_ConvertSurface failed: {}", sdl_error());
    }
    let _converted = SurfaceHandle(converted);

    let frame = unsafe { &*converted };
    if frame.w != WINDOW_WIDTH || frame.h != WINDOW_HEIGHT {
        bail!(
            "capture returned a {}x{} frame, expected {WINDOW_WIDTH}x{WINDOW_HEIGHT}",
            frame.w,
            frame.h
        );
    }
    let pitch = frame.pitch as usize;
    if pitch < (WINDOW_WIDTH * 4) as usize {
        bail!("capture pitch {pitch} is too small");
    }

    let pixels = unsafe {
        std::slice::from_raw_parts(frame.pixels.cast::<u8>(), pitch * WINDOW_HEIGHT as usize)
    };
    let mut rgba = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT as usize {
        let row = &pixels[y * SCALE as usize * pitch..];
        for x in 0..WIDTH as usize {
            let start = x * SCALE as usize * 4;
            rgba.extend_from_slice(&row[start..start + 4]);
        }
    }

    bmp::encode(
        &Image {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            rgba,
        },
        path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::PackWriter;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("arklay-engine-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_image() -> Image {
        let mut rgba = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let index = ((x * 3 + y) % 256) as u8;
                rgba.extend_from_slice(&[index, index ^ 0x55, index.wrapping_mul(37), 255]);
            }
        }
        Image {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            rgba,
        }
    }

    #[test]
    fn capture_writes_bmp() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("game.akpak");
        let capture_path = dir.0.join("capture.bmp");

        let mut rdt = vec![0u8; 0x94];
        rdt[0x01] = 1;
        rdt.extend_from_slice(&[0u8; 44]);

        let image = test_image();
        let bmp_bytes = bmp::encode_to_vec(&image).unwrap();

        let id = RoomId::parse("1000").unwrap();
        let mut writer = PackWriter::new();
        writer.add(&id.rdt_entry(), rdt).unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();

        run(&pack_path, id, Some(&capture_path)).unwrap();

        let decoded = bmp::decode(&std::fs::read(&capture_path).unwrap()).unwrap();
        assert_eq!(decoded.width, WIDTH as u32);
        assert_eq!(decoded.height, HEIGHT as u32);
        assert_eq!(decoded.rgba, image.rgba);
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn loads_save_room_music_from_real_pack() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let wav = load_room_music(&pack, RoomId::parse("1001").unwrap())
            .expect("room 1001 should have a primary music track");
        assert_eq!(wav.sample_rate, 22050);
        assert_eq!(wav.channels, 1);
        assert!(matches!(wav.format, audio::WavFormat::U8));
        assert!(!wav.data.is_empty());
    }
}
