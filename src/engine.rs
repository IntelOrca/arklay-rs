//! Engine entry point: open a pack, load a room, simulate and display it.

use std::ffi::{CStr, CString, c_void};
use std::path::Path;

use anyhow::{Context, Result, bail};

use sdl3_sys::blendmode::SDL_BLENDMODE_NONE;
use sdl3_sys::error::SDL_GetError;
use sdl3_sys::events::{SDL_EVENT_KEY_DOWN, SDL_EVENT_QUIT, SDL_Event, SDL_PollEvent};
use sdl3_sys::hints::{SDL_HINT_RENDER_DRIVER, SDL_HINT_VIDEO_DRIVER, SDL_SetHint};
use sdl3_sys::init::{SDL_INIT_VIDEO, SDL_Init, SDL_Quit};
use sdl3_sys::keyboard::SDL_GetKeyboardState;
use sdl3_sys::keycode::{SDL_KMOD_NONE, SDL_KMOD_SHIFT, SDLK_COMMA, SDLK_ESCAPE, SDLK_PERIOD};
use sdl3_sys::main::SDL_SetMainReady;
use sdl3_sys::pixels::SDL_PIXELFORMAT_ABGR8888;
use sdl3_sys::render::{
    SDL_CreateRenderer, SDL_CreateTexture, SDL_DestroyRenderer, SDL_DestroyTexture,
    SDL_RenderClear, SDL_RenderPresent, SDL_RenderReadPixels, SDL_RenderTexture, SDL_Renderer,
    SDL_SetRenderDrawColor, SDL_SetRenderVSync, SDL_SetTextureBlendMode, SDL_SetTextureScaleMode,
    SDL_TEXTUREACCESS_STREAMING, SDL_Texture, SDL_UpdateTexture,
};
use sdl3_sys::scancode::{
    SDL_SCANCODE_DOWN, SDL_SCANCODE_LEFT, SDL_SCANCODE_LSHIFT, SDL_SCANCODE_RETURN,
    SDL_SCANCODE_RIGHT, SDL_SCANCODE_RSHIFT, SDL_SCANCODE_SPACE, SDL_SCANCODE_UP,
};
use sdl3_sys::surface::{
    SDL_ConvertSurface, SDL_DestroySurface, SDL_SCALEMODE_NEAREST, SDL_Surface,
};
use sdl3_sys::timer::SDL_GetTicks;
use sdl3_sys::video::{
    SDL_CreateWindow, SDL_DestroyWindow, SDL_SetWindowTitle, SDL_Window, SDL_WindowFlags,
};

use crate::anim;
use crate::audio::{self, MusicPlayer};
use crate::bmp;
use crate::emd;
use crate::game;
use crate::model::Emd;
use crate::music;
use crate::pack::Pack;
use crate::player;
use crate::rdt;
use crate::render::{Camera, Framebuffer, Lighting};
use crate::scd;
use crate::state::{Image, RoomId, RoomState};

const WIDTH: i32 = 320;
const HEIGHT: i32 = 240;
const SCALE: i32 = 3;
const WINDOW_WIDTH: i32 = WIDTH * SCALE;
const WINDOW_HEIGHT: i32 = HEIGHT * SCALE;
const PIXEL_PITCH: i32 = WIDTH * 4;
/// Fixed simulation step in milliseconds (30 Hz, the original frame rate).
const TICK_MS: f64 = 1000.0 / 30.0;

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

/// Load `id` from `pack`, simulate it and optionally capture one frame.
///
/// The room keeps running until the game quits: when a door sets a transition
/// the target room is loaded with [`load_room`], the player is placed at the
/// door's spawn and the room's scripts restart. Inventory and flags survive
/// through [`game::GameState::enter_room`]; room-scoped state (the action
/// table, camera, message) is rebuilt by the new room. Capture mode renders a
/// single frame and never interacts.
pub fn run(pack: &Path, id: RoomId, capture: Option<&Path>) -> Result<()> {
    let pack = Pack::open(pack)?;
    let mut loaded = load_room(&pack, id)?;
    let mut game = game::GameState::new(id, &loaded.room);
    let mut player_state = player::spawn(id, &loaded.room);

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

    let title = window_title(
        &loaded.id.room3(),
        loaded.room.current_cut,
        loaded.room.cuts.len(),
    )?;
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

    let mut framebuffer = Framebuffer::new();

    if let Some(capture_path) = capture {
        run_room_init(&loaded, &mut game);
        apply_camera(&mut loaded.room, &mut game, None);
        render_frame(
            &mut framebuffer,
            &loaded.room,
            &player_state,
            loaded.player_assets.as_ref(),
        );
        present(renderer, texture, &framebuffer)?;
        capture_frame(renderer, capture_path)?;
        return Ok(());
    }

    let mut music = start_music(loaded.music.take());

    let mut event = SDL_Event::default();
    let mut last_ticks = unsafe { SDL_GetTicks() };
    let mut accumulator = 0.0f64;
    let mut titled_cut = loaded.room.current_cut;
    let mut pending_transition: Option<game::RoomTransition> = None;
    loop {
        {
            let scripts = &loaded.scripts;
            let mut command_vm = scd::vm::CommandVm::new(scripts);
            let mut event_vm = scd::vm::EventVm::new(scripts);
            {
                let mut host = game::ScdGameHost::new(&mut game);
                command_vm.run_init(&mut host);
            }
            start_pending_events(&mut game, &mut event_vm);
            apply_camera(&mut loaded.room, &mut game, None);
            update_window_title(window, &loaded, &mut titled_cut)?;

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
                            let count = loaded.room.cuts.len();
                            let moved = if key == SDLK_COMMA {
                                loaded.room.current_cut =
                                    (loaded.room.current_cut + count - 1) % count;
                                true
                            } else if key == SDLK_PERIOD {
                                loaded.room.current_cut = (loaded.room.current_cut + 1) % count;
                                true
                            } else {
                                false
                            };
                            if moved {
                                game.camera.current_cut = loaded.room.current_cut;
                                update_window_title(window, &loaded, &mut titled_cut)?;
                            }
                        }
                    }
                }

                let now = unsafe { SDL_GetTicks() };
                accumulator += now.saturating_sub(last_ticks) as f64;
                last_ticks = now;
                // Avoid a burst of catch-up ticks after a stall (window drag, debugger).
                if accumulator > 250.0 {
                    accumulator = 250.0;
                }
                let (input, action) = read_input();
                while accumulator >= TICK_MS {
                    let transition = tick_room(
                        &mut command_vm,
                        &mut event_vm,
                        RoomContext {
                            room: &mut loaded.room,
                            game: &mut game,
                            player: &mut player_state,
                            player_assets: loaded.player_assets.as_ref(),
                        },
                        input,
                        action,
                    );
                    apply_bgm_requests(&mut music, &mut game.room_bgm_requests, &pack, loaded.id);
                    accumulator -= TICK_MS;
                    if let Some(transition) = transition {
                        pending_transition = Some(transition);
                        break;
                    }
                }
                if pending_transition.is_some() {
                    break;
                }

                render_frame(
                    &mut framebuffer,
                    &loaded.room,
                    &player_state,
                    loaded.player_assets.as_ref(),
                );
                present(renderer, texture, &framebuffer)?;
                if let Some(player) = &mut music {
                    player.update();
                }
                if !unsafe { SDL_RenderPresent(renderer) } {
                    bail!("SDL_RenderPresent failed: {}", sdl_error());
                }
            }
        }

        let Some(transition) = pending_transition.take() else {
            return Ok(());
        };
        loaded = enter_transition(&pack, &mut game, &mut player_state, &transition)?;
        if let Some(player) = music.as_mut() {
            player.stop();
        }
        if let Some(wav) = loaded.music.take() {
            match music.as_mut() {
                Some(player) => {
                    if let Err(err) = player.play(wav) {
                        eprintln!("warning: failed to start music: {err:#}");
                    }
                }
                None => music = start_music(Some(wav)),
            }
        }
    }
}

/// Everything loaded for one room, so a transition can load the next room with
/// the same code path as the initial load.
struct LoadedRoom {
    id: RoomId,
    room: RoomState,
    scripts: scd::ir::Scripts,
    player_assets: Option<PlayerAssets>,
    music: Option<audio::Wav>,
}

/// Load `id` from `pack`: RDT, camera backgrounds, SCD scripts, player assets
/// and the room's primary music track.
fn load_room(pack: &Pack, id: RoomId) -> Result<LoadedRoom> {
    let rdt_bytes = pack.read(&id.rdt_entry())?;
    let mut room = rdt::parse(rdt_bytes, id)?;
    if room.cuts.is_empty() {
        bail!("room {} has no camera cuts", id.room3());
    }
    for cut in &mut room.cuts {
        let path = id.cut_entry(cut.index);
        let bytes = pack
            .read(&path)
            .with_context(|| format!("missing background for cut {}", cut.index))?;
        cut.background = Some(bmp::decode(bytes).with_context(|| format!("invalid {path}"))?);
    }
    let scripts = scd::reader::parse(rdt_bytes).context("invalid room SCD scripts")?;
    let player_assets = load_player_assets(pack, id);
    let music = load_room_music(pack, id);
    Ok(LoadedRoom {
        id,
        room,
        scripts,
        player_assets,
        music,
    })
}

/// Run a room's init script against `game`.
fn run_room_init(loaded: &LoadedRoom, game: &mut game::GameState) {
    let mut command_vm = scd::vm::CommandVm::new(&loaded.scripts);
    let mut host = game::ScdGameHost::new(game);
    command_vm.run_init(&mut host);
}

/// Start every event script requested by `evt_exec` in the command scripts.
fn start_pending_events(game: &mut game::GameState, event_vm: &mut scd::vm::EventVm<'_>) {
    for (slot, event) in game.pending_events.drain(..) {
        event_vm.start(usize::from(slot), event);
    }
}

/// The mutable state one simulated tick works on.
struct RoomContext<'a> {
    room: &'a mut RoomState,
    game: &'a mut game::GameState,
    player: &'a mut player::PlayerState,
    player_assets: Option<&'a PlayerAssets>,
}

/// Run one fixed 30 Hz tick: scripts, interaction, player movement and camera.
/// Returns the door transition the tick requested, if any.
fn tick_room(
    command_vm: &mut scd::vm::CommandVm<'_>,
    event_vm: &mut scd::vm::EventVm<'_>,
    context: RoomContext<'_>,
    input: player::Input,
    action: bool,
) -> Option<game::RoomTransition> {
    {
        let mut host = game::ScdGameHost::new(context.game);
        command_vm.run_main(&mut host);
    }
    start_pending_events(context.game, event_vm);
    {
        let mut host = game::ScdGameHost::new(context.game);
        event_vm.step(&mut host);
        host.interact(context.player.pos, action);
    }
    context.game.advance_frame();
    if let Some(assets) = context.player_assets {
        player::update(
            context.player,
            context.room,
            &assets.emd.clips,
            &assets.emw.clips,
            input,
        );
    }
    apply_camera(context.room, context.game, Some(context.player.pos));
    context.game.transition.take()
}

/// Load the transition's target room and place the player in it.
///
/// Inventory and flags survive through [`game::GameState::enter_room`]; the
/// room action table, camera and message are rebuilt by the new room.
fn enter_transition(
    pack: &Pack,
    game: &mut game::GameState,
    player_state: &mut player::PlayerState,
    transition: &game::RoomTransition,
) -> Result<LoadedRoom> {
    let loaded = load_room(pack, transition.target)?;
    game.enter_room(transition.target, &loaded.room);
    *player_state = player::spawn(transition.target, &loaded.room);
    player_state.pos = transition.pos;
    player_state.angle = transition.angle;
    Ok(loaded)
}

/// Update the window title when the active camera cut changed.
fn update_window_title(
    window: *mut SDL_Window,
    loaded: &LoadedRoom,
    titled_cut: &mut usize,
) -> Result<()> {
    if loaded.room.current_cut == *titled_cut {
        return Ok(());
    }
    *titled_cut = loaded.room.current_cut;
    let title = window_title(
        &loaded.id.room3(),
        loaded.room.current_cut,
        loaded.room.cuts.len(),
    )?;
    if !unsafe { SDL_SetWindowTitle(window, title.as_ptr()) } {
        bail!("SDL_SetWindowTitle failed: {}", sdl_error());
    }
    Ok(())
}

/// Apply the SCD camera state to the room.
///
/// When the scripts hold the camera lock, their cut wins; otherwise the M2 zone
/// switching picks the cut from the player position. `None` keeps the current
/// cut when no position is available yet (room load and capture).
fn apply_camera(room: &mut RoomState, game: &mut game::GameState, pos: Option<[i32; 3]>) {
    let selected = if game.camera.locked && game.camera.current_cut < room.cuts.len() {
        game.camera.current_cut
    } else if let Some(pos) = pos {
        player::camera_for_position(room, room.current_cut, pos)
    } else {
        room.current_cut
    };
    game.camera.current_cut = selected;
    room.current_cut = selected;
}

/// Apply the room's queued BGM requests; the engine plays one track at a time.
fn apply_bgm_requests(
    music: &mut Option<MusicPlayer>,
    requests: &mut Vec<game::BgmRequest>,
    pack: &Pack,
    id: RoomId,
) {
    for request in requests.drain(..) {
        if request.start {
            if !music.as_ref().is_some_and(MusicPlayer::is_playing) {
                *music = start_music(load_room_music(pack, id));
            }
        } else if let Some(player) = music {
            player.stop();
        }
    }
}

/// Read the keyboard into a movement input plus the action key (Space/Return).
fn read_input() -> (player::Input, bool) {
    let mut count = 0i32;
    let keys = unsafe { SDL_GetKeyboardState(&mut count) };
    let down = |scancode: sdl3_sys::scancode::SDL_Scancode| {
        let index = scancode.0;
        index >= 0 && index < count && !keys.is_null() && unsafe { *keys.add(index as usize) }
    };
    let input = player::Input {
        up: down(SDL_SCANCODE_UP),
        down: down(SDL_SCANCODE_DOWN),
        left: down(SDL_SCANCODE_LEFT),
        right: down(SDL_SCANCODE_RIGHT),
        run: down(SDL_SCANCODE_LSHIFT) || down(SDL_SCANCODE_RSHIFT),
    };
    (input, down(SDL_SCANCODE_SPACE) || down(SDL_SCANCODE_RETURN))
}

/// The player's character model plus its no-weapon locomotion clips.
struct PlayerAssets {
    emd: Emd,
    emw: crate::model::Emw,
}

/// Draw the room background and the player model into `framebuffer`.
fn render_frame(
    framebuffer: &mut Framebuffer,
    room: &RoomState,
    player_state: &player::PlayerState,
    assets: Option<&PlayerAssets>,
) {
    framebuffer.clear();
    let Some(cut) = room.cuts.get(room.current_cut) else {
        return;
    };
    if let Some(background) = &cut.background {
        framebuffer.blit(background);
    }
    let Some(assets) = assets else {
        return;
    };
    let camera = Camera::from_cut(cut);
    let lighting = Lighting::from_room(room);
    let (keyframes, clips) = match player_state.clip_source {
        player::ClipSource::Emd => (&assets.emd.keyframes, &assets.emd.clips),
        player::ClipSource::Emw => (&assets.emw.keyframes, &assets.emw.clips),
    };
    let keyframe_index = player_state.anim.keyframe_index(clips);
    let Some(keyframe) = keyframes.get(keyframe_index) else {
        return;
    };
    let entity = anim::entity_matrix(player_state.pos, player_state.angle);
    let joints = anim::joint_matrices(&assets.emd.skeleton, keyframe, &entity);
    framebuffer.draw_model(
        &assets.emd.mesh,
        &assets.emd.texture,
        &joints,
        &camera,
        &lighting,
    );
}

/// Upload the framebuffer and draw it scaled to the window.
fn present(
    renderer: *mut SDL_Renderer,
    texture: *mut SDL_Texture,
    framebuffer: &Framebuffer,
) -> Result<()> {
    if framebuffer.width != WIDTH as u32
        || framebuffer.height != HEIGHT as u32
        || framebuffer.rgba.len() != (WIDTH * HEIGHT * 4) as usize
    {
        bail!(
            "expected a {WIDTH}x{HEIGHT} framebuffer, got {}x{} with {} bytes",
            framebuffer.width,
            framebuffer.height,
            framebuffer.rgba.len()
        );
    }
    if !unsafe {
        SDL_UpdateTexture(
            texture,
            std::ptr::null(),
            framebuffer.rgba.as_ptr().cast::<c_void>(),
            PIXEL_PITCH,
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

/// Load the player's character model and no-weapon clips for the RDT flag.
fn load_player_assets(pack: &Pack, id: RoomId) -> Option<PlayerAssets> {
    let character = id.player_flag & 3;
    let emd_path = format!("player/{character:02}.emd");
    let emw_path = format!("player/{character:02}.emw");
    let emd_bytes = pack.read(&emd_path).map_err(|err| {
        eprintln!("warning: missing player model {emd_path}: {err:#}");
    });
    let emw_bytes = pack.read(&emw_path).map_err(|err| {
        eprintln!("warning: missing player clips {emw_path}: {err:#}");
    });
    let emd_bytes = emd_bytes.ok()?;
    let emw_bytes = emw_bytes.ok()?;
    let emd = match emd::parse(emd_bytes) {
        Ok(model) => model,
        Err(err) => {
            eprintln!("warning: invalid player model {emd_path}: {err:#}");
            return None;
        }
    };
    let emw = match emd::parse_emw(emw_bytes) {
        Ok(model) => model,
        Err(err) => {
            eprintln!("warning: invalid player clips {emw_path}: {err:#}");
            return None;
        }
    };
    Some(PlayerAssets { emd, emw })
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

fn sdl_error() -> String {
    unsafe { CStr::from_ptr(SDL_GetError()) }
        .to_string_lossy()
        .into_owned()
}

fn window_title(room: &str, cut: usize, count: usize) -> Result<CString> {
    CString::new(format!("Arklay - room {room} cut {cut}/{}", count - 1))
        .context("window title contains a NUL byte")
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

    /// One block plus the terminator, packed as an SCD procedure container.
    fn container(blocks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for body in blocks {
            let size = u16::try_from(body.len() + 2).unwrap();
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(body);
        }
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Minimal one-camera RDT carrying `init` as its init script.
    fn synthetic_rdt(init: &[u8]) -> Vec<u8> {
        const INIT_POINTER: usize = 0x48 + 6 * 4;
        const MAIN_POINTER: usize = 0x48 + 7 * 4;
        const EVENT_POINTER: usize = 0x48 + 8 * 4;

        let mut data = vec![0u8; 0x94];
        data[0x01] = 1;
        data.extend_from_slice(&[0u8; 44]);
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }

        let init_offset = data.len();
        data.extend_from_slice(&container(&[init]));
        let main_offset = data.len();
        data.extend_from_slice(&[0, 0, 0, 0]);
        let events_offset = data.len();
        data.extend_from_slice(&[0, 0, 0, 0]);

        for (pointer, offset) in [
            (INIT_POINTER, init_offset),
            (MAIN_POINTER, main_offset),
            (EVENT_POINTER, events_offset),
        ] {
            data[pointer..pointer + 4].copy_from_slice(&(offset as u32).to_le_bytes());
        }
        data
    }

    /// `door_aot_set(0, 100, 200, 300, 400, ..., RDT_001, 555, 0, 666, 1024, 0, 0)`.
    fn door_init() -> Vec<u8> {
        let mut body = vec![0x0C, 0x00];
        for value in [100i16, 200, 300, 400] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(&[0, 0, 0, 0, 0]);
        body.push(1);
        for value in [555i16, 0, 666, 1024] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&[0x00, 0x00]);
        body
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
    fn walk_animation_moves_the_model() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("1001").unwrap();
        let room = rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
        let assets = load_player_assets(&pack, id).expect("player assets");
        let mut player_state = player::spawn(id, &room);

        let mut idle = Framebuffer::new();
        render_frame(&mut idle, &room, &player_state, Some(&assets));

        let input = player::Input {
            up: true,
            ..player::Input::default()
        };
        for _ in 0..20 {
            player::update(
                &mut player_state,
                &room,
                &assets.emd.clips,
                &assets.emw.clips,
                input,
            );
        }
        let mut walking = Framebuffer::new();
        render_frame(&mut walking, &room, &player_state, Some(&assets));

        let changed = idle
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(walking.rgba.as_chunks::<4>().0)
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            changed > 100,
            "expected the walk animation to move the model, {changed} pixels changed"
        );
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

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn room_1001_runs_its_scd_through_the_game_state() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("1001").unwrap();
        let rdt_bytes = pack.read(&id.rdt_entry()).unwrap();
        let room = rdt::parse(rdt_bytes, id).unwrap();
        let scripts = scd::reader::parse(rdt_bytes).unwrap();

        let mut game = game::GameState::new(id, &room);
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);
        {
            let mut host = game::ScdGameHost::new(&mut game);
            command_vm.run_init(&mut host);
        }
        assert_eq!(game.state_bytes[2], 1, "player flag is untouched");
        assert_eq!(game.bgm.state, 0x09, "init should write the BGM state");
        assert!(!game.camera.locked);

        for _ in 0..10 {
            let mut host = game::ScdGameHost::new(&mut game);
            command_vm.run_main(&mut host);
            event_vm.step(&mut host);
            game.advance_frame();
        }

        assert_eq!(game.frame, 10);
        assert!(
            !game.placeholders.is_empty(),
            "unimplemented opcodes should be recorded"
        );
        assert_eq!(
            game.room_actions[0].map(|action| action.kind),
            Some(game::RoomActionKind::Door),
            "init should register the save-room door"
        );
        assert_eq!(
            game.room_actions[1].map(|action| action.kind),
            Some(game::RoomActionKind::Item),
            "init should register the ink ribbon"
        );
        assert!(game.doors[0].is_some(), "door record is stored");
        assert_eq!(game.camera.current_cut, 0);
        assert_eq!(game.camera.saved_cut, None);
        assert_eq!(game.message.id, None);
        assert_eq!(game.message.pause, 0);
        assert!(!game.message.active);
        assert_eq!(game.room_bgm_requests, Vec::new());
    }

    #[test]
    fn door_action_transitions_to_the_next_room() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("game.akpak");
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let a = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let b = RoomId {
            stage: 1,
            room: 1,
            player_flag: 0,
        };

        let mut writer = PackWriter::new();
        writer
            .add(&a.rdt_entry(), synthetic_rdt(&door_init()))
            .unwrap();
        writer
            .add(&b.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&a.cut_entry(0), bmp_bytes.clone()).unwrap();
        writer.add(&b.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let mut loaded = load_room(&pack, a).unwrap();
        assert_eq!(loaded.id, a);
        let mut game = game::GameState::new(a, &loaded.room);
        let mut player_state = player::spawn(a, &loaded.room);
        player_state.pos = [150, 0, 250];

        let transition = {
            let scripts = &loaded.scripts;
            let mut command_vm = scd::vm::CommandVm::new(scripts);
            let mut event_vm = scd::vm::EventVm::new(scripts);
            {
                let mut host = game::ScdGameHost::new(&mut game);
                command_vm.run_init(&mut host);
            }
            assert!(game.doors[0].is_some(), "init should register the door");

            let idle = tick_room(
                &mut command_vm,
                &mut event_vm,
                RoomContext {
                    room: &mut loaded.room,
                    game: &mut game,
                    player: &mut player_state,
                    player_assets: None,
                },
                player::Input::default(),
                false,
            );
            assert!(idle.is_none(), "walking alone must not trigger the door");
            tick_room(
                &mut command_vm,
                &mut event_vm,
                RoomContext {
                    room: &mut loaded.room,
                    game: &mut game,
                    player: &mut player_state,
                    player_assets: None,
                },
                player::Input::default(),
                true,
            )
            .expect("the action key should open the door")
        };

        assert_eq!(transition.target, b);
        assert_eq!(transition.pos, [555, 0, 666]);
        assert_eq!(transition.angle, 1024);

        loaded = enter_transition(&pack, &mut game, &mut player_state, &transition).unwrap();
        assert_eq!(loaded.id, b);
        assert_eq!(game.id, b);
        assert_eq!(game.state_bytes[0], 1);
        assert_eq!(game.state_bytes[1], 1);
        assert_eq!(game.state_bytes[2], 0);
        assert_eq!(player_state.pos, [555, 0, 666]);
        assert_eq!(player_state.angle, 1024);
        assert!(game.doors[0].is_none());
        assert!(game.room_actions[0].is_none());
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn capture_draws_the_player_in_room_1001() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let dir = TempDir::new();
        let capture_path = dir.0.join("room1001.bmp");
        run(
            Path::new(&path),
            RoomId::parse("1001").unwrap(),
            Some(&capture_path),
        )
        .unwrap();

        let image = bmp::decode(&std::fs::read(&capture_path).unwrap()).unwrap();
        assert_eq!(image.width, WIDTH as u32);
        assert_eq!(image.height, HEIGHT as u32);

        let pack = Pack::open(Path::new(&path)).unwrap();
        let background = bmp::decode(
            pack.read(&RoomId::parse("1001").unwrap().cut_entry(0))
                .unwrap(),
        )
        .unwrap();
        let changed = image
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(background.rgba.as_chunks::<4>().0)
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            changed > 200,
            "expected the player model to cover pixels, only {changed} changed"
        );
    }
}
