//! Engine entry point: open a pack, load a room, simulate and display it.
//!
//! # Documented deviations
//!
//! The world-item milestone leaves these deliberate gaps:
//! - the ground pick-up (menu mode 3) and scripted `give_item` (mode 4) item
//!   viewers run the model intro, the model spin and the 0xC0/0xC1/0xC2
//!   message round trip through `PickupView`. A gated `item_aot_set` entry
//!   (flags word bit 0) plays the 0x0c reach animation first through
//!   [`crate::player::LockedAction::Interact`] and the viewer opens when it
//!   completes; an ungated entry opens instantly, exactly like the original.
//!   The document handler (`set_room_event_flag`, mode 8) shows global 0xC6
//!   and files the entry instead of running the original's file-list screen:
//!   the state-8 art, layout and navigation are a separate slice;
//! - no map tab: a map pick-up raises its RoomFlags owned bit, but
//!   `ITEM_M2`'s `MAP*.TIM` pages and `Map_blue.tim` stay unpacked and the map
//!   screen is untouched;
//! - the crank hex's (item 0x1E) texture-dirty write has no analogue in the
//!   port's direct-decode design and is a no-op;
//! - no mirror reflection of item meshes or sparkle billboards: the M11 mirror
//!   pass reflects the player/NPC joint meshes only;
//! - per-texel PSX semi-transparency is absent; a primitive carrying the ABE
//!   bit uses the flat half blend the effect path already uses;
//! - the original's stage-5 texture-bank and palette overrides are documented
//!   no-ops, as is the positional fix-up path; the model's own decoded texture
//!   is used everywhere;
//! - the original's fixed ordering-table depths (the flooded guardhouse and
//!   courtyard water rooms, the 1F right-stairs objects with type < 2, and the
//!   2F study/front-lesson open lid at slot 0x33) are not modelled: the port
//!   orders every object and item triangle by its mean view Z in the shared
//!   stable sort;
//! - object and item collision and floor probes keep the record's local matrix
//!   translation, exactly like the original's `update_room_objects` helpers;
//!   only rendering, the camera-switch cull and effect attach compose the SCA
//!   parent chain.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, CString, c_void};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use sdl3_sys::blendmode::SDL_BLENDMODE_NONE;
use sdl3_sys::error::SDL_GetError;
use sdl3_sys::events::{
    SDL_EVENT_GAMEPAD_ADDED, SDL_EVENT_GAMEPAD_AXIS_MOTION, SDL_EVENT_GAMEPAD_BUTTON_DOWN,
    SDL_EVENT_GAMEPAD_BUTTON_UP, SDL_EVENT_GAMEPAD_REMOVED, SDL_EVENT_KEY_DOWN, SDL_EVENT_KEY_UP,
    SDL_EVENT_KEYBOARD_REMOVED, SDL_EVENT_MOUSE_BUTTON_DOWN, SDL_EVENT_MOUSE_BUTTON_UP,
    SDL_EVENT_MOUSE_WHEEL, SDL_EVENT_QUIT, SDL_EVENT_WINDOW_DISPLAY_CHANGED,
    SDL_EVENT_WINDOW_FOCUS_LOST, SDL_Event, SDL_PollEvent,
};
use sdl3_sys::gamepad::{
    SDL_AddGamepadMappingsFromFile, SDL_CloseGamepad, SDL_Gamepad, SDL_OpenGamepad,
};
use sdl3_sys::hints::{SDL_HINT_RENDER_DRIVER, SDL_HINT_VIDEO_DRIVER, SDL_SetHint};
use sdl3_sys::init::{SDL_INIT_GAMEPAD, SDL_INIT_VIDEO, SDL_Init, SDL_InitSubSystem, SDL_Quit};
use sdl3_sys::joystick::SDL_JoystickID;
use sdl3_sys::keycode::{SDL_KMOD_ALT, SDL_KMOD_NONE, SDL_KMOD_SHIFT, SDLK_COMMA, SDLK_PERIOD};
use sdl3_sys::main::SDL_SetMainReady;
use sdl3_sys::mouse::SDL_MOUSEWHEEL_FLIPPED;
use sdl3_sys::pixels::SDL_PIXELFORMAT_ABGR8888;
use sdl3_sys::rect::{SDL_FRect, SDL_Rect};
use sdl3_sys::render::{
    SDL_CreateRenderer, SDL_CreateTexture, SDL_DestroyRenderer, SDL_DestroyTexture,
    SDL_GetCurrentRenderOutputSize, SDL_RenderClear, SDL_RenderPresent, SDL_RenderReadPixels,
    SDL_RenderTexture, SDL_Renderer, SDL_SetRenderDrawColor, SDL_SetRenderVSync,
    SDL_SetTextureBlendMode, SDL_SetTextureScaleMode, SDL_TEXTUREACCESS_STREAMING, SDL_Texture,
    SDL_UpdateTexture,
};
use sdl3_sys::scancode::{
    SDL_SCANCODE_F8, SDL_SCANCODE_KP_ENTER, SDL_SCANCODE_RETURN, SDL_Scancode,
};
use sdl3_sys::stdinc::SDL_free;
use sdl3_sys::surface::{
    SDL_ConvertSurface, SDL_DestroySurface, SDL_SCALEMODE_NEAREST, SDL_SCALEMODE_PIXELART,
    SDL_Surface,
};
use sdl3_sys::timer::{SDL_DelayNS, SDL_GetTicksNS};
use sdl3_sys::video::{
    SDL_CreateWindow, SDL_DestroyWindow, SDL_DisplayID, SDL_GetDisplayForWindow,
    SDL_GetDisplayUsableBounds, SDL_GetDisplays, SDL_GetPrimaryDisplay, SDL_GetWindowBordersSize,
    SDL_GetWindowPixelDensity, SDL_SetWindowFullscreen, SDL_SetWindowFullscreenMode,
    SDL_SetWindowPosition, SDL_SetWindowSize, SDL_SetWindowTitle, SDL_ShowWindow, SDL_SyncWindow,
    SDL_WINDOW_HIDDEN, SDL_WINDOWPOS_CENTERED_DISPLAY, SDL_Window, SDL_WindowFlags,
};

use crate::anim;
use crate::assets;
use crate::audio::{self, Mixer};
use crate::bgm;
use crate::bmp;
use crate::controls::{self, Action};
use crate::demo;
use crate::door;
use crate::effects;
use crate::emd;
use crate::ending;
use crate::enemy;
use crate::font;
use crate::game;
use crate::gpu::GpuRenderer;
use crate::items;
use crate::lua;
use crate::mask;
use crate::message::MessageInput;
use crate::model::{Clip, Emd};
use crate::movie::{self, MovieSession, MovieTick};
use crate::objects;
use crate::pacer::{FramePacer, Pace};
use crate::pack::{AssetClass, LayerOptions, ModLayer, Pack};
use crate::player;
use crate::player_script;
use crate::rdt;
use crate::render::{
    self, Camera, EffectLayer, EffectQuad, EntityMesh, Framebuffer, Lighting, MaskLayer,
};
use crate::save;
use crate::scd;
use crate::sfx;
use crate::shadow;
use crate::state::{Image, RoomId, RoomState};
use crate::stats;
use crate::text::Text;
use crate::trace;
use crate::transition::{self, DoorStepper};
use crate::tween::{self, Tween};
use crate::ui::debug_menu::{DebugMenu, DebugMenuEvent};
use crate::ui::file::{FileAssets, FileEvent, FileScreen};
use crate::ui::item_box::{ItemBox, ItemBoxAssets, ItemBoxEvent};
use crate::ui::lua_screen::ScreenArg;
use crate::ui::main_menu::{MainMenu, MenuAssets, MenuEvent, MenuInput, MenuMode};
use crate::ui::return_title::{ReturnTitleEvent, ReturnTitlePrompt};
use crate::ui::{self, Screen, ScreenAction, ScreenResult, UiContext, UiInput};
use crate::video::{self, Graphics, RendererKind};
use crate::weapons;

const WIDTH: i32 = 320;
const HEIGHT: i32 = 240;
const SCALE: i32 = 3;
const WINDOW_WIDTH: i32 = WIDTH * SCALE;
const WINDOW_HEIGHT: i32 = HEIGHT * SCALE;
const PIXEL_PITCH: i32 = WIDTH * 4;
/// Fixed simulation step in milliseconds (30 Hz, the original frame rate).
const TICK_MS: f64 = 1000.0 / 30.0;
/// The original's non-gameplay frame-limiter period in milliseconds.
///
/// The original paces at 33 ms while the game is active and at 16 ms while
/// it is not: door transitions, the title, character select, the pause menu
/// and its modals, the endings and the save screens all run with the
/// game-active flag clear, and their state machines advance one frame per
/// platform frame. The port ticks those at this interval so their wall-clock
/// timing matches.
const UI_TICK_MS: f64 = 16.0;
/// The absent Virgin logo the boot sequence requests first.
const BOOT_VLOGO_ID: u8 = 28;
/// The Capcom logo the boot sequence requests after the Virgin logo.
const BOOT_CAPCOM_ID: u8 = 23;
/// The opening film the title requests on its first entry per process.
const TITLE_OPENING_ID: u8 = 0;
/// The prologue film the character confirm requests before a new game.
const PROLOGUE_ID: u8 = 1;

/// The process-wide SDL lifetime.
///
/// `SDL_Init` runs once for the first live handle and `SDL_Quit` when the last
/// one drops, so several displays can coexist without one tearing SDL down
/// under another. The interactive engine holds one handle for its whole run;
/// the capture/UI test seams overlap in one test process. An audio [`Mixer`]
/// retains a share of the same lifetime, so its stream is always destroyed
/// before `SDL_Quit` tears the audio subsystem down, whichever drops first.
static SDL_REFS: AtomicUsize = AtomicUsize::new(0);
static SDL_LIFETIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) struct SdlHandle;

impl SdlHandle {
    /// Initialize SDL video if this is the first live handle.
    pub(crate) fn acquire() -> Result<Self> {
        let _guard = SDL_LIFETIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if SDL_REFS.load(Ordering::SeqCst) == 0 {
            unsafe { SDL_SetMainReady() };
            if !unsafe { SDL_Init(SDL_INIT_VIDEO) } {
                bail!("SDL_Init failed: {}", sdl_error());
            }
        }
        SDL_REFS.fetch_add(1, Ordering::SeqCst);
        Ok(Self)
    }

    /// Retain the initialized SDL lifetime without initializing video.
    ///
    /// A mixer only exists once `SDL_InitSubSystem(SDL_INIT_AUDIO)` succeeded,
    /// so SDL is up; the share keeps `SDL_Quit` from running until the resource
    /// SDL owns (the audio stream) has been destroyed.
    #[must_use]
    pub(crate) fn retain() -> Self {
        let _guard = SDL_LIFETIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SDL_REFS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for SdlHandle {
    fn drop(&mut self) {
        let _guard = SDL_LIFETIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if SDL_REFS.fetch_sub(1, Ordering::SeqCst) == 1 {
            unsafe { SDL_Quit() };
        }
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

/// The oracle/authoring seams the CLI adds on top of a session boot.
///
/// These are test-only: they let a scenario author a game state once (a Lua
/// chunk through [`Authoring::setup_script`]) and serialise it to a
/// `savedatN.dat` block ([`Authoring::save_out`]) so both engines can boot the
/// exact same state from the same file ([`Authoring::load`]). None of them
/// widen the mod-facing sandbox: `--setup-script` reads a CLI-fixed path and
/// `--save-out` writes a CLI-fixed path, both from Rust.
#[derive(Clone, Copy, Debug, Default)]
pub struct Authoring<'a> {
    /// `--load`: continue from the save block at this path instead of building
    /// a new game.
    pub load: Option<&'a Path>,
    /// `--setup-script`: a Lua chunk run once on the freshly booted session,
    /// before the first tick. It receives the game state as its first argument
    /// (`local api = ...`) and may use any authoring method of the `api`.
    pub setup_script: Option<&'a Path>,
    /// `--save-out`: write the session's current save block to this path and
    /// exit before ticking. The oracle's deterministic save generator.
    pub save_out: Option<&'a Path>,
    /// The `[graphics]` options with `--renderer`, `--render-scale` and
    /// `--msaa` applied: the interactive window and what it draws with.
    pub renderer: Graphics,
    /// `--frame-rate` as given on the command line: a headless `--stats`
    /// run also draws and times the frames a display at this rate would
    /// present. Never taken from the config, so the report does not depend
    /// on the machine's settings.
    pub stats_frame_rate: Option<u32>,
}

/// Load `id` from `pack`, simulate it and optionally capture one frame.
///
/// Gameplay runs until the game quits. A door transition is played in full:
/// the `.dor` animation runs over the destination room loaded behind its black
/// frames, and control returns to the player when the animation finishes.
/// Inventory and flags survive through [`game::GameState::enter_room`];
/// room-scoped state (the action table, camera, message) is rebuilt by the new
/// room. Capture mode renders one frame (with its room-mask layer) and never
/// opens audio. `ticks` runs that many fixed 30 Hz room ticks before the frame
/// is drawn, so a scripted NPC scene can be captured without a display; the
/// same deterministic path [`simulate_room`] uses is taken, audio-free.
pub fn run(pack: &Path, id: RoomId, capture: Option<&Path>, ticks: u32) -> Result<()> {
    run_with_options(
        pack,
        id,
        capture,
        ticks,
        &ModOptions::default(),
        false,
        false,
        None,
        trace::TraceOptions::default(),
        Authoring::default(),
    )
}

/// [`run`] with the runtime layers and the `--stats` frame-time report.
///
/// `mods` is the runtime layer selection, exactly like [`open_game_pack`].
/// With `stats` the fixed-tick run records a per-tick histogram and the
/// load/update/effect/render
/// phases and prints the [`crate::stats`] report to stdout at exit; it runs
/// without a capture and without opening a display. `_debug_menu` is retained
/// for CLI compatibility: the port-only F1 room-select overlay is always
/// enabled. `trace` enables the oracle trace of `oracle/trace-v1.md`: one line
/// per fixed tick (rendering each tick so the frame hash exists) plus the
/// `--capture-every` BMP cadence.
#[allow(clippy::too_many_arguments)] // the CLI run options are all needed
pub fn run_with_options(
    pack: &Path,
    id: RoomId,
    capture: Option<&Path>,
    ticks: u32,
    mods: &ModOptions,
    stats: bool,
    _debug_menu: bool,
    language: Option<&str>,
    trace: trace::TraceOptions<'_>,
    authoring: Authoring<'_>,
) -> Result<()> {
    let save_dir = save::default_save_dir();
    let pack = open_game_pack(pack, mods, language)?;

    if capture.is_some()
        || (stats && ticks > 0)
        || trace.path.is_some()
        || authoring.save_out.is_some()
    {
        let mut timings = stats::Stats::new();
        let load_start = Instant::now();
        let mut loaded = load_room(&pack, id)?;
        let mut game = new_game_state(&pack, id, &loaded.room);
        seed_room_items(&mut game);
        // The arbitrary-room boot seeds the selected character's start block
        // like the interactive `from_room` path, so a headless run starts with
        // a live character rather than a zeroed one.
        seed_start_health(&mut game, id.player_flag);
        let mut player_state = player::spawn(id, &loaded.room);
        game.sync_entity_from_player(&player_state);
        run_room_init(&mut loaded, &mut game);
        let lua = load_lua(&pack);
        call_lua_init(lua.as_ref(), &mut game);
        call_lua_room_load(lua.as_ref(), &mut game, id);
        run_setup_chunk(lua.as_ref(), &mut game, authoring.setup_script)?;
        game.sync_player(&mut player_state);
        if let Some(path) = authoring.save_out {
            write_save_file(&pack, &game, path)?;
            return Ok(());
        }
        drain_mask_toggles(&mut loaded.room, &mut game);
        apply_camera(&mut loaded.room, &mut game, Some(player_state.pos));
        bgm::update_room_bgm(&mut game, id, None);
        timings.phases.load = load_start.elapsed();

        let mut npc_models = enemy::EntityModelCache::default();
        let mut enemy_lua = enemy::LuaEnemyHost::new();
        let mut masks = MaskCache::default();
        let mut shadows = ShadowCache::default();
        let mut effect_pages = EffectPageCache::default();
        let mut trace_writer = trace.path.map(trace::TraceWriter::create).transpose()?;
        if trace_writer.is_some()
            && trace.capture_every > 0
            && let Some(dir) = trace.capture_dir
        {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let mut framebuffer = Framebuffer::new();
        // `--stats --frame-rate` also draws the frames a display at that rate
        // would present between the ticks, off to the side of the simulation.
        let mut presentation = authoring
            .stats_frame_rate
            .filter(|_| stats)
            .map(Presentation::new);
        if ticks > 0 {
            let scripts = Rc::new(loaded.scripts.clone());
            let mut command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
            let mut event_vm = scd::vm::EventVm::from_scripts(scripts);
            // Captures stay audio-free: every `voice_play` wait is released as soon
            // as it is raised, so the frames match the audio-less corpus runs.
            let mut voice_cache = VoiceCache::default();
            let mut bgm_cache = bgm::BgmCache::default();
            let mut snd3d_cache = SfxCache::default();
            let mut no_mixer: Option<Mixer> = None;
            let mut sfx_cache = SfxCache::default();
            let mut door_transition: Option<TransitionMode> = None;
            for _ in 0..ticks {
                let tick_start = Instant::now();
                // A door transition owns the tick and follows into its
                // destination room, exactly like the interactive session (but
                // audio-free): the `--ticks` capture ends in the room the last
                // scripted door led to.
                if let Some(mut session) = door_transition.take() {
                    let frame = session.transition.tick(false);
                    session.frame = frame;
                    for message in session.transition.stepper_mut().take_messages() {
                        apply_door_message(&mut game, message);
                    }
                    let _ = session.transition.stepper_mut().take_sfx();
                    session.transition.set_sound_busy(false);
                    if frame.finished {
                        let from = loaded.id;
                        finish_transition(&mut session, &mut game, &mut player_state, &mut loaded);
                        play_room_close_sfx(&mut no_mixer, &mut sfx_cache, &pack, &session);
                        let scripts = Rc::new(loaded.scripts.clone());
                        command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
                        event_vm = scd::vm::EventVm::from_scripts(scripts);
                        run_room_init(&mut loaded, &mut game);
                        apply_event_requests(&mut game, &mut event_vm);
                        call_lua_room_load(lua.as_ref(), &mut game, loaded.id);
                        drain_mask_toggles(&mut loaded.room, &mut game);
                        apply_camera(&mut loaded.room, &mut game, Some(player_state.pos));
                        bgm::update_room_bgm(&mut game, loaded.id, Some(from));
                    } else {
                        door_transition = Some(session);
                    }
                } else {
                    let message_before = game.message.menu_choice_id() & 0x80 != 0;
                    let requested = tick_room_timed(
                        &mut command_vm,
                        &mut event_vm,
                        RoomContext {
                            room: &mut loaded.room,
                            game: &mut game,
                            player: &mut player_state,
                            player_assets: loaded.player_assets.as_ref(),
                            pack: &pack,
                            npc_models: &mut npc_models,
                            enemy_lua: Some(&mut enemy_lua),
                        },
                        player::Input::default(),
                        Some(&mut timings.phases),
                    );
                    run_lua_tick_hooks(lua.as_ref(), &mut game, message_before);
                    if let Some(request) = requested {
                        let record = game.transition_door.take().unwrap_or_default();
                        game.door_yield = None;
                        door_transition = Some(start_transition(
                            &pack,
                            &record,
                            &request,
                            loaded.id,
                            &transition::Fade::inactive(),
                        )?);
                    }
                    play_snd3d_requests(
                        &mut no_mixer,
                        &mut snd3d_cache,
                        &pack,
                        &loaded.room,
                        &mut game,
                    );
                    bgm::apply_live(&mut no_mixer, &mut game, &mut bgm_cache, &pack);
                    tick_voice(&mut no_mixer, &mut voice_cache, &mut game, &pack);
                    // No input is fed headlessly, so release a message's menu
                    // choice the way an auto-confirm would; the F7 wait must not
                    // outlive the frame that raised it. The scripts read the
                    // release through the BioCard state byte, so mirror it.
                    let choice = game.message.menu_choice_id();
                    game.message.set_menu_choice_id(choice & 0x7F);
                    game.sync_message_choice();
                    drain_mask_toggles(&mut loaded.room, &mut game);
                    // A capture never plays a film: the request is taken and
                    // dropped so a `movie_on` cannot stall the headless run.
                    game.take_fmv_request();
                }
                timings.record_tick(tick_start.elapsed());
                timings.observe_high_water(active_entities(&game), game.effects.active_count());
                if let Some(presentation) = presentation.as_mut() {
                    presentation.tick(
                        &pack,
                        &loaded,
                        &player_state,
                        &mut game,
                        &mut npc_models,
                        &mut masks,
                        &mut shadows,
                        &mut effect_pages,
                    );
                }
                // The room boot path arms no room-entry fade (that is a
                // session behaviour), so the trace reports the port's inactive
                // fade: step 0, accumulator -1.
                if let Some(writer) = trace_writer.as_mut() {
                    let index = writer.tick_index();
                    render_frame(
                        &mut framebuffer,
                        &pack,
                        loaded.id,
                        &loaded.room,
                        &player_state,
                        &mut game,
                        loaded.player_assets.as_ref(),
                        &mut npc_models,
                        &mut masks,
                        &mut shadows,
                        &mut effect_pages,
                    );
                    let inactive = transition::Fade::inactive();
                    writer.write_tick(&trace::TraceTick {
                        room: loaded.id,
                        rng: game.rand_state,
                        rseed: game.rand_seed,
                        fade_counter: inactive.counter() as i16,
                        fade_state: inactive.state(),
                        cam: loaded.room.current_cut as u8,
                        player_flags: game.player_flags,
                        entities: &*game.entities,
                        frame_hash: trace::frame_hash(&framebuffer),
                    })?;
                    if trace.capture_every > 0
                        && index % trace.capture_every == 0
                        && let Some(dir) = trace.capture_dir
                    {
                        capture_bmp(
                            &Image {
                                width: framebuffer.width,
                                height: framebuffer.height,
                                rgba: framebuffer.rgba.clone(),
                            },
                            &dir.join(format!("f{index:05}.bmp")),
                        )?;
                    }
                }
            }
        }
        if let Some(writer) = trace_writer.as_mut() {
            writer.flush()?;
        }

        let render_start = Instant::now();
        render_frame(
            &mut framebuffer,
            &pack,
            loaded.id,
            &loaded.room,
            &player_state,
            &mut game,
            loaded.player_assets.as_ref(),
            &mut npc_models,
            &mut masks,
            &mut shadows,
            &mut effect_pages,
        );
        timings.phases.render += render_start.elapsed();

        if let Some(capture_path) = capture {
            let title = window_title(
                &loaded.id.room3(),
                loaded.room.current_cut,
                loaded.room.cuts.len(),
            );
            let mut display = Display::new(&title, true, Graphics::default())?;
            display.present(&framebuffer)?;
            display.capture(capture_path)?;
        }
        if stats {
            timings.render = presentation.map(|presentation| presentation.render);
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            timings.report(&mut out)?;
        }
        return Ok(());
    }

    let mut session = GameSession::from_room(&pack, id, &save_dir)?;
    session.run_setup_script(authoring.setup_script)?;
    let title = window_title(
        &session.loaded.id.room3(),
        session.loaded.room.current_cut,
        session.loaded.room.cuts.len(),
    );
    let mut display = Display::new(&title, false, authoring.renderer)?;
    session.start_audio(&pack);
    run_session_loop(&pack, &mut session, &mut display, stats)
}

/// The presentation frames of a headless `--stats` run: after each tick, the
/// frames a display presenting `frame_rate` frames per second would draw
/// before the next one (one exact frame per tick at or below the tick rate,
/// tweened frames above it, as [`crate::pacer`] schedules them), each timed
/// against the per-frame budget. They are drawn into their own framebuffer and
/// the player's exact joints are restored afterwards, so the simulation, the
/// trace and the capture never see them.
struct Presentation {
    tween: Tween,
    render: stats::RenderStats,
    framebuffer: Framebuffer,
    ticks: u64,
}

impl Presentation {
    fn new(frame_rate: u32) -> Self {
        Self {
            tween: Tween::default(),
            render: stats::RenderStats::new(frame_rate),
            framebuffer: Framebuffer::new(),
            ticks: 0,
        }
    }

    /// The tween positions of the frames presented between tick `tick` and
    /// the next: frame `m` is presented at `m / frame_rate` seconds, between
    /// ticks `n` and `n + 1` at `alpha = m * TICK_RATE / frame_rate - n`.
    fn alphas(frame_rate: u32, tick: u64) -> Vec<Option<f32>> {
        let rate = u64::from(frame_rate.max(1));
        let ticks = u64::from(video::TICK_RATE);
        if rate <= ticks {
            return vec![None];
        }
        let first = (tick * rate).div_ceil(ticks);
        let end = ((tick + 1) * rate).div_ceil(ticks);
        (first..end)
            .map(|frame| Some(((frame * ticks) as f64 / rate as f64 - tick as f64) as f32))
            .collect()
    }

    /// Draw and time this tick's presentation frames.
    #[allow(clippy::too_many_arguments)]
    fn tick(
        &mut self,
        pack: &Pack,
        loaded: &LoadedRoom,
        player_state: &player::PlayerState,
        game: &mut game::GameState,
        npc_models: &mut enemy::EntityModelCache,
        masks: &mut MaskCache,
        shadows: &mut ShadowCache,
        effect_cache: &mut EffectPageCache,
    ) {
        self.tween.advance();
        let exact_joints = game.joint_worlds[0].clone();
        for alpha in Self::alphas(self.render.frame_rate, self.ticks) {
            self.tween.set_alpha(alpha);
            let start = Instant::now();
            render_frame_on(
                &mut self.framebuffer,
                None,
                pack,
                loaded.id,
                &loaded.room,
                player_state,
                game,
                loaded.player_assets.as_ref(),
                npc_models,
                masks,
                shadows,
                effect_cache,
                &mut self.tween,
            );
            self.render.record_frame(start.elapsed());
        }
        game.joint_worlds[0] = exact_joints;
        self.ticks += 1;
        self.render.elapsed = std::time::Duration::from_secs(self.ticks) / video::TICK_RATE;
    }
}

/// Number of active entity slots, slot 0 (the player) included.
fn active_entities(game: &game::GameState) -> usize {
    game.entities
        .iter()
        .filter(|entity| entity.active())
        .count()
}

/// The runtime mod layer selection.
///
/// `explicit` holds the `--mod` paths (always applied), `no_mods` ignores both
/// the explicit and the discovered layers, `layers` is the user config's
/// enable/disable/order selection over discovered mods, and `global_dir` is
/// the per-user `mods/` directory (`None` disables that discovery, which keeps
/// library/tests runs isolated).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModOptions {
    /// Explicit `--mod` layers, applied even when the config disables their id.
    pub explicit: Vec<PathBuf>,
    /// Ignore the explicit layers and both discovery directories.
    pub no_mods: bool,
    /// The config's selection and order for discovered mods.
    pub layers: LayerOptions,
    /// The per-user mods directory to scan in addition to the pack's sibling
    /// `mods/` directory.
    pub global_dir: Option<PathBuf>,
}

/// Open a game pack together with its runtime mod layers.
///
/// Unless `options.no_mods` is set, the pack's sibling `mods/` directory and
/// `options.global_dir` are scanned for `.akpak` files and mod source
/// directories; `options.explicit` adds the `--mod` layers. The config's
/// [`LayerOptions`] filters and orders the discovered layers, while an
/// explicit layer always applies. A layer discovered and also passed
/// explicitly applies only once (as explicit). `no_mods` gives the plain
/// single-pack path a vanilla run needs. Layer warnings are printed once here,
/// and a non-fatal base without a manifest still boots.
pub fn open_game_pack(path: &Path, options: &ModOptions, language: Option<&str>) -> Result<Pack> {
    if options.no_mods {
        let mut pack = Pack::open(path)?;
        pack.set_language(language);
        return Ok(pack);
    }
    let mut explicit: HashSet<PathBuf> = HashSet::new();
    for path in &options.explicit {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if !explicit.insert(canonical) {
            bail!("duplicate mod layer path {}", path.display());
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for layer in crate::modding::discover_mods(path, options.global_dir.as_deref()) {
        let canonical = std::fs::canonicalize(&layer).unwrap_or_else(|_| layer.clone());
        if seen.insert(canonical) {
            candidates.push(layer);
        }
    }
    for layer in &options.explicit {
        let canonical = std::fs::canonicalize(layer).unwrap_or_else(|_| layer.clone());
        if seen.insert(canonical) {
            candidates.push(layer.clone());
        }
    }
    if candidates.is_empty() {
        let mut pack = Pack::open(path)?;
        pack.set_language(language);
        return Ok(pack);
    }
    let layers: Vec<ModLayer<'_>> = candidates
        .iter()
        .map(|path| {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            ModLayer {
                path,
                explicit: explicit.contains(&canonical),
            }
        })
        .collect();
    let mut pack = Pack::open_layered_with(path, &layers, &options.layers)?;
    pack.set_language(language);
    for warning in pack.warnings() {
        eprintln!("warning: {warning}");
    }
    Ok(pack)
}

/// Load a pack's Lua hooks, logging a failure instead of failing the session.
fn load_lua(pack: &Pack) -> Option<lua::LuaVm> {
    match lua::LuaVm::load(pack) {
        Ok(vm) => vm,
        Err(err) => {
            eprintln!("warning: Lua hooks unavailable: {err:#}");
            None
        }
    }
}

/// Run `on_init` once the pack's hooks have loaded, before any room. A pack
/// uses it to register a save/load screen, which the engine reads before a
/// session exists too (see [`lua::registered_save_screen`]).
fn call_lua_init(lua: Option<&lua::LuaVm>, game: &mut game::GameState) {
    if let Some(lua) = lua {
        lua.call_init(game);
    }
}

/// Run `on_room_load` for the room that just finished entering.
fn call_lua_room_load(lua: Option<&lua::LuaVm>, game: &mut game::GameState, id: RoomId) {
    if let Some(lua) = lua {
        lua.call_room_load(game, id);
    }
}

/// Run the CLI `--setup-script` chunk once against the room state.
///
/// The chunk runs whatever the pack's own Lua entries are: a pack with hooks
/// reuses its VM, a pack without one gets a throwaway sandboxed state, so
/// `--setup-script` works with every pack. Errors are fatal: this is an
/// authoring seam, so a broken setup must not silently produce a save.
fn run_setup_chunk(
    lua: Option<&lua::LuaVm>,
    game: &mut game::GameState,
    script: Option<&Path>,
) -> Result<()> {
    let Some(path) = script else {
        return Ok(());
    };
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let name = path.display().to_string();
    match lua {
        Some(lua) => lua.run_setup(game, &bytes, &name)?,
        None => lua::LuaVm::empty()?.run_setup(game, &bytes, &name)?,
    }
    Ok(())
}

/// Write `game` as a save block to `path`, with the pack's bio-card prefix.
///
/// The `--save-out` half of [`run_setup_chunk`]: the same
/// [`save::SaveFile::from_state`] capture the typewriter screen writes, so the
/// file loads through `--load` here and `--test-load` in the reference.
fn write_save_file(pack: &Pack, game: &game::GameState, path: &Path) -> Result<()> {
    let file = match pack.read(save::SAVE_PREFIX_ENTRY) {
        Ok(prefix) => save::SaveFile::from_state_with_prefix(game, &prefix)?,
        Err(_) => save::SaveFile::from_state(game),
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    crate::atomic::write(path, &file.to_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Run the per-tick Lua hooks: `on_tick` after the room tick and `on_message`
/// rewriting the id of a message raised during it.
///
/// `message_was_up` is the window's menu byte before the tick: a request that
/// turns a down window up during the tick is a fresh message. The byte, not
/// `active`, is the signal because the headless harness releases the menu bit
/// every tick so a script's F7 wait cannot stall.
fn run_lua_tick_hooks(lua: Option<&lua::LuaVm>, game: &mut game::GameState, message_was_up: bool) {
    let Some(lua) = lua else {
        return;
    };
    lua.call_tick(game, game.frame);
    let message_raised = !message_was_up && game.message.menu_choice_id() & 0x80 != 0;
    if !message_raised {
        return;
    }
    let Some(id) = game.message.id else {
        return;
    };
    if let Some(filtered) = lua.filter_message(game, u16::from(id))
        && let Ok(filtered) = u8::try_from(filtered)
    {
        game.message.id = Some(filtered);
    }
}

/// The fixed step the session loop uses for the current screen.
///
/// Gameplay runs at the original's 30 Hz interval; while a door transition
/// or a pause-menu screen (menu, item box, pickup viewer, map, file, save
/// screen or gameplay modal) owns the screen the original clears its
/// game-active flag and runs on the 16 ms non-gameplay limiter, so the port
/// steps those at the same wall-clock rate.
fn session_tick_interval(transition_active: bool, menu_active: bool) -> f64 {
    if transition_active || menu_active {
        UI_TICK_MS
    } else {
        TICK_MS
    }
}

/// The interactive loops' clock: wall time feeds the fixed-tick accumulator
/// and a [`FramePacer`] decides when a frame is presented (see
/// [`crate::pacer`]). Nothing here feeds the simulation but the accumulator.
struct LoopClock {
    last_ns: u64,
    /// Unspent wall time since the last tick, in milliseconds.
    accumulator: f64,
    pacer: FramePacer,
    /// `--stats`: the presented frames and when the loop started.
    render: Option<(stats::RenderStats, u64)>,
}

impl LoopClock {
    /// The longest stretch of wall time one pass catches up, in milliseconds.
    const MAX_CATCH_UP_MS: f64 = 250.0;

    fn new(frame_rate: u32, stats: bool) -> Self {
        let now = unsafe { SDL_GetTicksNS() };
        Self {
            last_ns: now,
            accumulator: 0.0,
            pacer: FramePacer::new(frame_rate),
            render: stats.then(|| (stats::RenderStats::new(frame_rate), now)),
        }
    }

    /// Add the wall time since the last pass to the accumulator.
    fn advance(&mut self) {
        let now = unsafe { SDL_GetTicksNS() };
        self.accumulator += now.saturating_sub(self.last_ns) as f64 / 1e6;
        self.last_ns = now;
        if self.accumulator > Self::MAX_CATCH_UP_MS {
            self.accumulator = Self::MAX_CATCH_UP_MS;
        }
    }

    /// Spend one tick of `tick_ms` from the accumulator.
    fn ticked(&mut self, tick_ms: f64) {
        self.accumulator -= tick_ms;
        self.pacer.ticked();
    }

    /// Whether to present now; `force` presents at once (exactly, without
    /// tweening), for a pass that must be drawn before it is torn down.
    fn pace(&mut self, tick_ms: f64, force: bool) -> Pace {
        if force {
            return self.pacer.force();
        }
        // Count the time the ticks just took, so a tweened frame's `alpha`
        // is where the wall clock is now rather than where this pass began.
        self.advance();
        self.pacer.poll(self.last_ns, self.accumulator, tick_ms)
    }

    /// Change the presented frame rate.
    fn set_rate(&mut self, rate: u32) {
        self.pacer.set_rate(rate);
        if let Some((render, _)) = &mut self.render {
            render.frame_rate = rate.max(1);
        }
    }

    /// `--stats`: record a frame drawn since `start`.
    fn frame_drawn(&mut self, start: Instant) {
        let Some((render, start_ns)) = &mut self.render else {
            return;
        };
        render.record_frame(start.elapsed());
        // The rate counts from the loop's start, so it is frames per second
        // of wall time rather than of the span between the first and last.
        let now = unsafe { SDL_GetTicksNS() };
        render.elapsed = std::time::Duration::from_nanos(now.saturating_sub(*start_ns));
    }

    /// `--stats`: print the render-rate report.
    fn report(&self) -> Result<()> {
        if let Some((render, _)) = &self.render {
            render.report(&mut std::io::stdout().lock())?;
        }
        Ok(())
    }
}

/// The interactive gameplay loop shared by `--room` and the app's Play mode:
/// fixed ticks from the latched input, frames presented at the display's
/// `frame_rate` (tweened between the last two ticks above the tick rate).
/// With `stats` the presented frames are reported at exit.
fn run_session_loop(
    pack: &Pack,
    session: &mut GameSession,
    display: &mut Display,
    stats: bool,
) -> Result<()> {
    let mut framebuffer = Framebuffer::new();
    let mut input = InputState::interactive();
    session.set_bindings(input.bindings().clone());
    let mut event = SDL_Event::default();
    let mut clock = LoopClock::new(display.frame_rate(), stats);
    loop {
        let mut cut_delta = 0i32;
        if poll_events(&mut event, &mut input, &mut cut_delta, Some(display)) {
            return clock.report();
        }
        if cut_delta != 0 {
            session.step_camera_cut(cut_delta);
        }
        if let Some(bindings) = session.take_bindings_change() {
            input.apply_bindings_change(bindings);
        }
        if let Some(rate) = session.take_frame_rate_change() {
            display.set_frame_rate(rate);
            clock.set_rate(rate);
        }
        session.set_frame_rate(display.frame_rate());
        session.update_window_title(display.window)?;

        clock.advance();
        // The film skip state advances on every loop pass, not the gameplay
        // tick, exactly like the original's platform-driven film update.
        session.poll_movie_skip(movie_buttons_held(input.held_word()));
        let tick_ms = session_tick_interval(session.transition.is_some(), session.room_frozen());
        while clock.accumulator >= tick_ms {
            if session.transition_finished {
                clock.accumulator = 0.0;
                break;
            }
            // Each fixed tick consumes its own latched press, so a frame that
            // catches up several ticks cannot replay one edge and a press that
            // arrived between two ticks is never dropped.
            let tick = input.tick();
            if session.transition.is_some() {
                session.tick_transition(pack, tick.action || tick.player.run);
            } else {
                session.tick(pack, tick.ui, tick.player, tick.action)?;
                if session.transition.is_some() {
                    // The original drops the remainder of the frame's time
                    // when a door hands control to its transition phase.
                    clock.ticked(tick_ms);
                    clock.accumulator = 0.0;
                    break;
                }
            }
            clock.ticked(tick_ms);
        }

        // The pass that ends a transition is drawn at once, before teardown.
        let alpha = match clock.pace(tick_ms, session.transition_finished) {
            Pace::Present { alpha } => alpha,
            Pace::Wait(ns) => {
                session.update_audio();
                unsafe { SDL_DelayNS(ns) };
                continue;
            }
        };
        session.tween.set_alpha(alpha);
        let draw = Instant::now();
        if let Some(gpu) = display.gpu() {
            session.render_layers(pack, gpu);
            clock.frame_drawn(draw);
            display.present_layers(session.frame(), session.white_frame())?;
        } else {
            session.render(pack);
            clock.frame_drawn(draw);
            framebuffer.copy_from(session.frame());
            display.present(&framebuffer)?;
        }
        display.show()?;
        if session.transition_finished {
            session.finish_transition(pack);
        }
        session.update_audio();
    }
}

/// How a [`Display`] puts frames on screen.
enum Backend {
    /// An SDL renderer scaling a streaming 320x240 texture. The texture is
    /// declared first so it is destroyed before its renderer.
    Software {
        texture: TextureHandle,
        renderer: RendererHandle,
    },
    /// The SDL_GPU device, which owns the window's swapchain.
    Gpu(Box<GpuRenderer>),
}

/// The SDL window and the backend every screen shares.
///
/// The guards are declared before the raw window pointer so the backend,
/// window and SDL tear down in that order.
struct Display {
    backend: Backend,
    _window: WindowHandle,
    _sdl: SdlHandle,
    window: *mut SDL_Window,
    /// The window options F8 and Alt+Enter change; `None` for a capture,
    /// whose window is always the fixed 960x720 one.
    graphics: Option<Graphics>,
}

impl Display {
    /// Open the window `graphics` describes (see [`crate::video`]); `capture`
    /// instead opens a fixed 960x720 window on SDL's offscreen software
    /// driver for headless captures, which ignore `graphics`.
    fn new(title: &str, capture: bool, graphics: Graphics) -> Result<Self> {
        if capture {
            unsafe {
                SDL_SetHint(SDL_HINT_VIDEO_DRIVER, c"offscreen".as_ptr());
                SDL_SetHint(SDL_HINT_RENDER_DRIVER, c"software".as_ptr());
            }
        }

        // Headless captures fall back to the always-present dummy driver when
        // the offscreen driver cannot initialize (e.g. no EGL on CI).
        let sdl = match SdlHandle::acquire() {
            Ok(sdl) => sdl,
            Err(err) if capture => {
                unsafe {
                    SDL_SetHint(SDL_HINT_VIDEO_DRIVER, c"dummy".as_ptr());
                }
                SdlHandle::acquire().map_err(|_| err)?
            }
            Err(err) => return Err(err),
        };

        // An interactive window starts hidden at 1x so it can be sized and
        // placed on its display before it is first shown.
        let (size, flags) = if capture {
            ((WINDOW_WIDTH, WINDOW_HEIGHT), SDL_WindowFlags::default())
        } else {
            ((WIDTH, HEIGHT), SDL_WINDOW_HIDDEN)
        };
        let title = CString::new(title).context("window title contains a NUL byte")?;
        let window = unsafe { SDL_CreateWindow(title.as_ptr(), size.0, size.1, flags) };
        if window.is_null() {
            bail!("SDL_CreateWindow failed: {}", sdl_error());
        }
        let window = WindowHandle(window);
        let mut graphics = graphics;
        if !capture {
            let display = display_id(graphics.display);
            place_window(window.0, &mut graphics, display);
            unsafe { SDL_ShowWindow(window.0) };
        }

        let gpu = if !capture && graphics.renderer == RendererKind::Gpu {
            // SAFETY: the window outlives the backend (field order) and has
            // no SDL renderer yet.
            match unsafe { GpuRenderer::new(window.0, graphics.render_scale, graphics.msaa) } {
                Ok(gpu) => Some(gpu),
                Err(err) => {
                    eprintln!("warning: GPU renderer unavailable ({err:#}); using software");
                    None
                }
            }
        } else {
            None
        };
        let backend = match gpu {
            Some(gpu) => Backend::Gpu(Box::new(gpu)),
            None => Self::software_backend(window.0)?,
        };
        Ok(Self {
            backend,
            window: window.0,
            _window: window,
            _sdl: sdl,
            graphics: (!capture).then_some(graphics),
        })
    }

    /// F8: the next window size, borderless fullscreen once the next size
    /// would not fit, then back to 1x ([`video::step_window`]).
    fn step_window(&mut self) {
        let window = self.window;
        let Some(graphics) = self.graphics.as_mut() else {
            return;
        };
        let display = unsafe { SDL_GetDisplayForWindow(window) };
        let room = window_room(window, display)
            .unwrap_or((video::MAX_WINDOW_SIDE, video::MAX_WINDOW_SIDE));
        video::step_window(graphics, room);
        place_window(window, graphics, display);
        self.save_window();
    }

    /// Alt+Enter: toggle borderless fullscreen and the window.
    fn toggle_fullscreen(&mut self) {
        let window = self.window;
        let Some(graphics) = self.graphics.as_mut() else {
            return;
        };
        graphics.fullscreen = !graphics.fullscreen;
        let display = unsafe { SDL_GetDisplayForWindow(window) };
        place_window(window, graphics, display);
        self.save_window();
    }

    /// Record the display the window is now on, so the next run opens there.
    fn display_changed(&mut self) {
        let display = unsafe { SDL_GetDisplayForWindow(self.window) };
        let Some(graphics) = self.graphics.as_mut() else {
            return;
        };
        let Some(number) = display_number(display) else {
            return;
        };
        if graphics.display != number {
            graphics.display = number;
            self.save_window();
        }
    }

    /// Persist the window options to `arklay.toml`; a failure is reported and
    /// the change still applies for this run.
    fn save_window(&self) {
        let Some(graphics) = &self.graphics else {
            return;
        };
        let path = crate::config::Config::default_path();
        if let Err(err) = crate::config::Config::save_window(&path, graphics) {
            eprintln!("warning: window settings not saved: {err:#}");
        }
    }

    /// The presented frames per second (the tick rate for a capture).
    fn frame_rate(&self) -> u32 {
        self.graphics
            .map_or(video::TICK_RATE, |graphics| graphics.frame_rate)
    }

    /// Change the frame rate from the options screen and persist it.
    fn set_frame_rate(&mut self, rate: u32) {
        let Some(graphics) = self.graphics.as_mut() else {
            return;
        };
        graphics.frame_rate = rate;
        let path = crate::config::Config::default_path();
        if let Err(err) = crate::config::Config::save_frame_rate(&path, rate) {
            eprintln!("warning: frame rate not saved: {err:#}");
        }
    }

    /// The SDL renderer and streaming texture of the software backend.
    fn software_backend(window: *mut SDL_Window) -> Result<Backend> {
        let renderer = unsafe { SDL_CreateRenderer(window, std::ptr::null()) };
        if renderer.is_null() {
            bail!("SDL_CreateRenderer failed: {}", sdl_error());
        }
        let renderer = RendererHandle(renderer);
        let _ = unsafe { SDL_SetRenderVSync(renderer.0, 1) };

        let texture = unsafe {
            SDL_CreateTexture(
                renderer.0,
                SDL_PIXELFORMAT_ABGR8888,
                SDL_TEXTUREACCESS_STREAMING,
                WIDTH,
                HEIGHT,
            )
        };
        if texture.is_null() {
            bail!("SDL_CreateTexture failed: {}", sdl_error());
        }
        let texture = TextureHandle(texture);

        if !unsafe { SDL_SetTextureBlendMode(texture.0, SDL_BLENDMODE_NONE) } {
            bail!("SDL_SetTextureBlendMode failed: {}", sdl_error());
        }
        if !unsafe { SDL_SetTextureScaleMode(texture.0, SDL_SCALEMODE_NEAREST) } {
            bail!("SDL_SetTextureScaleMode failed: {}", sdl_error());
        }
        if !unsafe { SDL_SetRenderDrawColor(renderer.0, 0, 0, 0, 255) } {
            bail!("SDL_SetRenderDrawColor failed: {}", sdl_error());
        }
        Ok(Backend::Software { texture, renderer })
    }

    /// The GPU renderer, when the window uses one.
    fn gpu(&mut self) -> Option<&mut GpuRenderer> {
        match &mut self.backend {
            Backend::Gpu(gpu) => Some(gpu),
            Backend::Software { .. } => None,
        }
    }

    /// Upload the framebuffer and draw it scaled to the window.
    fn present(&mut self, framebuffer: &Framebuffer) -> Result<()> {
        self.present_layers(framebuffer, None)
    }

    /// Present a frame whose overlays were drawn over the GPU scene: `black`
    /// over a black scene and `white` over a white one (see
    /// [`GameSession::render_layers`]). `white == None` shows `black` alone.
    /// A GPU failure here drops this frame and continues in software.
    fn present_layers(&mut self, black: &Framebuffer, white: Option<&Framebuffer>) -> Result<()> {
        match &mut self.backend {
            Backend::Software { texture, renderer } => present(renderer.0, texture.0, black),
            Backend::Gpu(gpu) => match gpu.present(black, white) {
                Ok(()) => Ok(()),
                Err(err) => self.fall_back_to_software(&err),
            },
        }
    }

    /// Replace a failed GPU backend with the software one.
    fn fall_back_to_software(&mut self, err: &anyhow::Error) -> Result<()> {
        eprintln!("warning: GPU renderer failed ({err:#}); using software");
        // The device must let go of the window before an SDL renderer can
        // claim it.
        if let Backend::Gpu(gpu) = &mut self.backend {
            gpu.release_window();
        }
        self.backend = Self::software_backend(self.window)?;
        Ok(())
    }

    /// Flip the drawn backbuffer to the screen. The GPU backend presents in
    /// [`Display::present_layers`].
    fn show(&self) -> Result<()> {
        if let Backend::Software { renderer, .. } = &self.backend
            && !unsafe { SDL_RenderPresent(renderer.0) }
        {
            bail!("SDL_RenderPresent failed: {}", sdl_error());
        }
        Ok(())
    }

    /// Read the drawn frame back and write it as a BMP.
    fn capture(&self, path: &Path) -> Result<()> {
        match &self.backend {
            Backend::Software { renderer, .. } => capture_frame(renderer.0, path),
            Backend::Gpu(_) => bail!("captures always use the software renderer"),
        }
    }
}

/// Which item-viewer flow a [`PickupView`] is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickupKind {
    /// A ground pick-up: the 0xC0 yes/no prompt (or 0xC2 when the inventory
    /// is full and the item does not merge).
    Take,
    /// A scripted `give_item`: global 0xC1, with the award when the message
    /// completes.
    GotItem,
    /// A document: the original's state-8 file screen shows the document's
    /// pages, then global 0xC6 as it closes and the entry is consumed.
    Document,
}

/// Where a [`PickupView`] is in its message round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickupPhase {
    /// The model intro is still running.
    Intro,
    /// The document reader owns the screen; global 0xC6 follows its exit.
    Reader,
    /// The prompt is on screen; the engine drives it through the game's
    /// message window.
    Message,
    /// The model is spinning out; the mode closes when it finishes.
    Exit,
}

/// The item-viewer flow the engine drives itself (the original's main-menu
/// modes 3, 4 and 8).
///
/// Like the examine viewer, a pick-up shares the game's message window and
/// the inventory panel, so the engine owns its state machine: the viewer's intro/exit
/// animation steps in [`GameSession::tick_pickup_view`] and the message is
/// resolved by [`game::GameState::update_message`] exactly like a menu
/// message. A document pick-up runs the FILE reader on the picked entry
/// instead of the model viewer.
struct PickupView {
    /// The room action slot being awarded or filed.
    slot: u8,
    /// Which prompt the flow runs.
    kind: PickupKind,
    /// The model viewer; `None` for the document reader flow.
    screen: Option<ui::item_view::ItemViewScreen>,
    /// The document reader; `None` for the model-viewer flows.
    reader: Option<ui::file::FileScreen>,
    /// Where the flow is.
    phase: PickupPhase,
}

/// One gameplay session: the loaded room, game state, player and VMs.
///
/// `--room` builds one through [`GameSession::from_room`] and the app's Play
/// mode through [`GameSession::new`] or [`GameSession::from_save`]. The engine
/// drives it one fixed tick at a time and renders through the session's own
/// framebuffer, so a frozen overlay can still show the last frame underneath.
/// The message window, the inventory menu, the item box, the examine viewer
/// and the FILE tab are driven explicitly rather than through the
/// [`ui::Screen`] trait: the message is part of [`game::GameState`] and keeps
/// running on the gameplay tick, while the others freeze the room but keep
/// drawing the last gameplay frame, read their art from the session and
/// mutate the same `GameState`. The item box is opened by a room `item_box`
/// action and shares the inventory panel the menu draws underneath it; the
/// FILE tab is opened from the menu's top tab row (the Tab key cycles it).
/// The pause-menu open/close fade phase.
///
/// The original fades the frozen frame to black before revealing the menu
/// (`Out`), fades the menu in from black before it accepts input (`In`), and
/// fades the menu out again before the room unfreezes (`Closing`). `None`
/// means the menu (or gameplay) runs normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuFadePhase {
    None,
    Out,
    In,
    Closing,
}

/// Frames the player and entities stay frozen for the room-entry fade after the
/// menu closes: the original suspends game_loop for the whole fade, but the
/// first frame after the menu task exits still runs one gameplay tick before
/// the suspension takes hold.
const MENU_CLOSE_FREEZE_FRAMES: u8 = 4;

/// Which of [`GameSession::render_pass`]'s variants draws the scene.
enum ScenePass<'g> {
    /// The software rasterizer draws it into the framebuffer.
    Software,
    /// The GPU records it; the framebuffer starts black (the overlay layer
    /// over a black scene).
    Record(&'g mut GpuRenderer),
    /// The GPU scene of the black pass stands; the framebuffer starts white.
    White,
}

struct GameSession {
    loaded: LoadedRoom,
    game: game::GameState,
    player: player::PlayerState,
    command_vm: scd::vm::CommandVm,
    event_vm: scd::vm::EventVm,
    masks: MaskCache,
    shadows: ShadowCache,
    effect_pages: EffectPageCache,
    sfx_cache: SfxCache,
    /// Decoded BGM wav cache, one entry per group track.
    bgm_cache: bgm::BgmCache,
    /// Decoded voice-line cache plus the one-slot pending request.
    voice_cache: VoiceCache,
    /// The in-game film currently holding the room, if any. While it is set
    /// only the film advances; scripts, entities, effects and the player are
    /// frozen.
    movie: Option<MovieSession>,
    /// Parsed entity models, loaded lazily from the pack by the entity driver.
    npc_models: enemy::EntityModelCache,
    /// The enemy-script VM hosting the pack's `enemy/em{id:02x}.lua` update
    /// functions; one VM per session, reset only by tests.
    enemy_lua: enemy::LuaEnemyHost,
    music: Option<Mixer>,
    /// Decoded pack text tables (messages, item names, descriptions).
    text: Text,
    /// Decoded glyph sheet for the message window; absent packs cannot draw
    /// messages but still run their state machine.
    font: Option<font::Font>,
    /// Pause-menu art, loaded once on the first START.
    menu_assets: Option<MenuAssets>,
    /// Whether the one-time menu-asset load has been attempted.
    menu_assets_loaded: bool,
    /// The paused inventory/status menu; the room tick is frozen while it is
    /// up and the last gameplay frame stays under it.
    menu: Option<MainMenu>,
    /// The pause menu's open/close fade and which phase it is in.
    menu_fade: transition::Fade,
    /// The pause-menu fade phase; `None` means the menu accepts input.
    menu_fade_phase: MenuFadePhase,
    /// The first fixed tick after arming a fade must not advance it: the frame
    /// it was armed on has not been drawn yet.
    menu_fade_pending: bool,
    /// The overlay the menu fade draws this frame: the accumulator before the
    /// tick's step (the original draws and only then adds). Consumed by
    /// [`Self::draw_menu_fade`].
    menu_fade_draw: Option<transition::Overlay>,
    /// Whether the current Out/Closing fade has taken its first step. The
    /// negative accumulator reset only applies to that first step.
    menu_fade_started: bool,
    /// Whether the closing fade has already restored the player to idle (the
    /// original's exit state 7 runs one frame after the cancel).
    menu_exit_restored: bool,
    /// Frames the player/entities stay frozen for the room-entry fade after the
    /// menu closes (the original suspends game_loop while it runs). The first
    /// post-arm frame still runs.
    /// The typewriter's save-screen hand-off: `Some(ink_ribbon)` while the
    /// fade-out runs (the original's `g_typewriter_state` 2->3, counter
    /// 0x1000); the save screen opens once the fade completes. The confirm
    /// only records the request; the fade arms on the next frame, exactly
    /// like the original's next-frame `check_typewriter_state`.
    typewriter_fade: Option<bool>,
    /// The confirmed typewriter answer awaiting its fade arm.
    typewriter_arm: Option<bool>,
    /// The faded-out typewriter hand-off awaiting its save screen.
    typewriter_open: Option<bool>,
    /// The typewriter save screen has closed and the room is fading back in
    /// from the black the hand-off left (the original's `g_typewriter_state`
    /// 3->4, counter `0xf000`); the player machine gate is restored once the
    /// accumulator runs negative.
    typewriter_exit_fade: bool,
    /// Frames the item box waits between the lid settling and the panel fade
    /// (the original's menu handshake: game_loop keeps ticking while
    /// `g_openMenuFlag` walks its states).
    itembox_open_delay: u8,
    menu_close_freeze: u8,
    /// Whether the first post-arm frame has already been allowed through.
    menu_close_freeze_first: bool,
    /// Set when the menu-close freeze ends, to restore the message bits on the
    /// following frame.
    menu_close_restore: bool,
    /// One frame of delay between the opening fade finishing and the menu
    /// accepting input: the original's main_menu spends its state 0 -> 1
    /// transition frame without drawing the health bar (menu_draw_health_bar
    /// runs from state 1), so the first health-monitor tick is one frame after
    /// the fade goes inactive.
    menu_open_delay: bool,
    /// Frames the menu's cursor and item-name line stay withheld after the
    /// opening fade completes (see the decrement in `tick`). The reference's
    /// fade-complete frame and its state-0 frame both still draw *neither*.
    menu_reveal_delay: u8,
    /// Frames to hold the black screen after the closing fade reaches black
    /// before game_loop resumes and the room-entry fade arms (the original's
    /// menu task teardown takes two frames).
    menu_close_wait: u8,
    /// The item-box overlay opened by a room `item_box` action. It shares the
    /// session's state and draws over the inventory panel the menu underneath
    /// provides; the room stays frozen while it is up.
    item_box: Option<ItemBox>,
    /// Item-box art, loaded on the first open.
    item_box_assets: Option<ItemBoxAssets>,
    /// The FILE tab selector/reader; the room stays frozen while it is up.
    file: Option<FileScreen>,
    /// FILE tab art, loaded on the first open.
    file_assets: Option<FileAssets>,
    /// The map tab; the room stays frozen while it is up. It is gated by the
    /// radio scenario flag, exactly like the radio tab.
    map: Option<ui::map::MapScreen>,
    /// The typewriter save screen; the room stays frozen while it is up.
    save_screen: Option<Box<dyn ui::save_load::SaveScreen>>,
    /// Where save files live for this session.
    save_dir: PathBuf,
    /// The pack's sandboxed Lua hooks, when it ships any: one VM per session,
    /// called on room entry and after every fixed tick.
    lua: Option<lua::LuaVm>,
    framebuffer: Framebuffer,
    /// The GPU path's second overlay layer: the frame drawn over a white
    /// scene (see [`GameSession::render_layers`]). Like `framebuffer`, it
    /// persists so a frozen frame keeps drawing over the last one.
    framebuffer_white: Option<Framebuffer>,
    /// Whether the persisted framebuffers show the GPU scene: set by a frame
    /// that recorded one and cleared by any frame that covered it.
    gpu_scene_visible: bool,
    /// The room the GPU residency was last recorded for; a change retires
    /// the previous room's uploads.
    gpu_room: Option<RoomId>,
    titled_cut: usize,
    transition: Option<TransitionMode>,
    transition_finished: bool,
    /// The ground/scripted item pick-up viewer. It is engine-driven because
    /// it shares the game's message window and the pause menu's inventory
    /// panel; see [`PickupView`].
    pickup_view: Option<PickupView>,
    /// The pause menu's examine viewer (CHECK, or the radio tab without a
    /// transmission). It runs inside the live menu: the menu keeps ticking
    /// and drawing underneath, and the viewer's messages go through the
    /// game's message window.
    examine: Option<ui::item_view::ItemViewScreen>,
    /// A message dismissal consumed the action key while it was still held;
    /// the room tick keeps ignoring the action until the key is released,
    /// matching the original's cleared held/previous-held pad bits.
    swallow_action: bool,
    /// The loading narration a new game or a continue shows before the first
    /// room tick: a global message over a black screen. While it is set the
    /// room stays frozen and the room-entry fade is not armed until the
    /// message's encoded delay dismisses it.
    boot_message: Option<u8>,
    /// The room-entry fade the original arms on every gameplay (re)entry after
    /// a room load: type 2, accumulator `0x7FFF` and a negative counter that
    /// steps the alpha down once per fixed tick.
    room_fade: transition::Fade,
    /// The alpha the room fade draws this frame: the accumulator *before* the
    /// tick's step, since the original draws and only then adds the counter
    /// (`main_loop`). Captured by [`Self::arm_room_fade`] for the armed frame
    /// and by [`Self::tick_room_fade`] for the stepped frames, so the draw
    /// matches the reference while the traced state stays the post-step value.
    room_fade_draw: Option<transition::Overlay>,
    /// The presentation cache that draws the scene between the last two
    /// ticks when the render rate is above 30 fps (see [`crate::tween`]).
    tween: Tween,
    /// The original's room-events gate (`g_message_flags & 0x80`) for a
    /// new-game boot: the boot's entry fade keeps the bit clear, so
    /// `room_events_check` does not step an event the room's init script
    /// queued until the accumulator goes negative. The port normally leaves
    /// the gate open; only [`GameSession::arm_new_game_boot`] closes it.
    room_boot_gate: bool,
    /// The first fixed tick after arming the room-entry fade still runs the
    /// player's state machine: the reference's inner-loop state machine clears
    /// `g_message_flags` only after that tick, so the spawn init (player state
    /// 0 to 1) runs while the gate is still nominally closed. The freeze
    /// starts one tick later.
    room_boot_grace: bool,
    /// An arbitrary-room boot (`--ui game --room`) re-places the player at the
    /// room's first walk zone centre after the room-set event pass, exactly
    /// like the reference's `TestHooks_PlaceRoomSpawn` runs after `init_room`.
    /// Without it a queued intro event's own `dir_set` survives the boot and
    /// the anchor position differs from the reference.
    room_boot_respawn: bool,
    /// The cutscene screen-intensity value the letterbox bars blend with.
    /// The bank-5 `MSF_SCREEN_INTENSITY` flag ramps it 16 a frame: up to `0xF0`
    /// while the flag is set, back down to zero while it is clear.
    sprite_anim_intensity: i16,
    /// Whether the port-only room overlay may open on F1. Enabled by default;
    /// `--debug-menu` is retained for CLI compatibility and no longer changes
    /// the behaviour.
    debug_menu_enabled: bool,
    /// The open debug room-select overlay; the room stays frozen while it is
    /// up and the last gameplay frame stays under it.
    debug_menu: Option<DebugMenu>,
    /// The debug overlay's state while it is closed, so F1 reopens the tool,
    /// cursor and room selection it was left on.
    debug_menu_memory: Option<DebugMenu>,
    /// The action map the window reads, which the debug menu's CONTROLS
    /// tool edits.
    bindings: controls::Bindings,
    /// The CONTROLS tool changed [`Self::bindings`]; taken once by the loop,
    /// which applies and persists them.
    bindings_changed: bool,
    /// The window's presented frame rate, shown by the F1 DISPLAY tool.
    frame_rate: u32,
    /// The DISPLAY tool changed [`GameSession::frame_rate`].
    frame_rate_changed: bool,
    /// The open F9 return-to-title prompt; the room stays frozen and the game
    /// sounds are paused while it is up.
    return_title: Option<ReturnTitlePrompt>,
    /// A confirmed return to the title screen, taken once by the app.
    return_title_requested: bool,
    /// A save slot the debug menu's LOAD GAME picked, taken once by the app.
    load_slot_requested: Option<usize>,
    /// An ending id the debug menu's CLEAR GAME picked, taken once by the app.
    clear_game_requested: Option<u8>,
    /// The active attract-demo playback, when the session was booted (or
    /// handed off) into a `demo/<hex>.dat` reel. While it is set the session
    /// injects the recorded pad words and runs the reel-end watchdog.
    attract: Option<demo::AttractState>,
    /// The player's death sequence: the delay, the fades and the DIED screen.
    death: ui::death::DeathSequence,
    /// The decoded `ui/died.tim` page, loaded when the DIED screen first opens.
    died_texture: Option<assets::Artwork>,
    /// Whether the died-page load was attempted; a missing entry is not
    /// retried on every frame.
    died_texture_tried: bool,
    /// Debug jump destinations that failed to load, so the warning is logged
    /// once per session instead of on every confirm.
    debug_failed: HashSet<u16>,
    /// The lab computer terminal (room 5060) while it owns the screen. The
    /// room stays frozen; the terminal's camera, lights and parked player are
    /// installed when it opens and restored when it closes.
    computer: Option<ui::computer_lab::ComputerLab>,
    /// The room state the terminal saved at `computer_lab_init`: the camera,
    /// the player's Y and the RDT lighting.
    computer_saved: Option<ComputerSaved>,
}

/// The state `computer_lab_init` (0x004123d0) swaps out for the terminal and
/// restores on exit.
struct ComputerSaved {
    /// `g_interactiveScreenSavedCameraId`.
    cut: usize,
    /// `g_labSlidesSavedPlayerY`.
    player_y: i32,
    /// The RDT's three lights.
    lights: [crate::state::Light; 3],
    /// The RDT ambient colour.
    ambient: [i16; 3],
}

/// New-game start position X (the original's `InitPlayerData`).
pub const NEW_GAME_POS_X: i32 = 17000;
/// New-game start position Z.
pub const NEW_GAME_POS_Z: i32 = 5000;
/// New-game facing angle.
pub const NEW_GAME_ANGLE: u16 = 3072;
/// New-game starting health: Chris then Jill.
pub const NEW_GAME_HEALTH: [i16; 2] = [140, 96];
/// Fallback new-game start room (stage 1, the main hall) when the pack ships
/// no `data/bio_card.dat`. The shipped card names the real start room; this is
/// the same room the original's card carries.
pub const NEW_GAME_ROOM: u8 = 0x06;
/// New-game carried room-pickup quantities (BioCard 0x20C..0x20E): ROOM1160's
/// shotgun shells and ROOM30B0/ROOM3080's flamethrower fuel.
pub const NEW_GAME_PICKUP_QUANTITIES: [u8; 3] = [7, 240, 240];
/// The shipped 32-byte room-items flag pattern: bit set = item still there.
pub const NEW_GAME_ROOM_ITEMS: [u8; 32] = [
    0xFF, 0xFF, 0xFF, 0xBF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xF7, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];
/// Item id of the combat knife.
const ITEM_KNIFE: u8 = 0x01;
/// Item id of the Beretta (Jill's starting handgun).
const ITEM_BERETTA: u8 = 0x02;
/// Item id of the first-aid spray.
const ITEM_FIRST_AID_SPRAY: u8 = 0x41;
/// Item id of the green herb, added by the deterministic menu capture.
const ITEM_GREEN_HERB: u8 = 0x44;
/// Item id of handgun ammunition, added by the deterministic menu capture.
const ITEM_HANDGUN_AMMO: u8 = 0x0B;
/// Item id of the sword key, added by the deterministic menu capture.
const ITEM_SWORD_KEY: u8 = 0x33;
/// Item id of the communication radio (the radio tab's used item).
const ITEM_COMM_RADIO: u8 = 0x4D;

/// When a room boot runs the per-room BGM handoff.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RoomBgmHandoff {
    /// The room is live now: resolve its BGM table as part of the boot
    /// (new game, save load, direct boot and the debug jump).
    Now,
    /// A door transition already loaded the destination behind its animation.
    /// `room_transition_load` runs the handoff only after the animation task
    /// finishes, so the boot leaves the outgoing track playing.
    AtTransitionEnd,
}

impl GameSession {
    /// A new game as `character` (`0` Chris, `1` Jill): the shipped card's
    /// start room (the main hall), the original's start position, health,
    /// items and room-item flags, then the room init/main/event VMs.
    fn new(pack: &Pack, character: u8, save_dir: &Path) -> Result<Box<Self>> {
        let character = character & 1;
        let id = new_game_id(pack, character);
        let loaded = load_room(pack, id)?;
        let mut game = new_game_state(pack, id, &loaded.room);
        seed_new_game(pack, &mut game, character);
        let mut player_state = player::spawn(id, &loaded.room);
        player_state.pos = [NEW_GAME_POS_X, 0, NEW_GAME_POS_Z];
        player_state.angle = NEW_GAME_ANGLE;
        game.sync_entity_from_player(&player_state);
        Self::from_loaded(pack, loaded, game, player_state, save_dir)
    }

    /// A new game as `id.player_flag` that boots straight into `id` instead
    /// of the card's start room: the new-game items, flags and start health.
    ///
    /// The player starts at the new-game position, exactly like the reference
    /// before its test-only room hook runs: `InitializeGame` seeds the position
    /// and `init_room`'s camera switch-zone scan tests it, so the boot camera
    /// is the one the new-game position selects. The room hook (and the port's
    /// arbitrary-room boot) re-places the player at the walk zone centre after
    /// the room-set event pass, in [`GameSession::place_room_boot_spawn`] and
    /// [`GameSession::arm_new_game_boot`].
    fn new_in_room(pack: &Pack, id: RoomId, character: u8, save_dir: &Path) -> Result<Box<Self>> {
        let character = character & 1;
        let loaded = load_room(pack, id)?;
        let mut game = new_game_state(pack, id, &loaded.room);
        seed_new_game(pack, &mut game, character);
        let mut player_state = player::spawn(id, &loaded.room);
        player_state.pos = [NEW_GAME_POS_X, 0, NEW_GAME_POS_Z];
        player_state.angle = NEW_GAME_ANGLE;
        player_state.collision_mirror = [NEW_GAME_POS_X, NEW_GAME_POS_Z];
        game.sync_entity_from_player(&player_state);
        let mut session = Self::from_loaded(pack, loaded, game, player_state, save_dir)?;
        session.room_boot_respawn = true;
        Ok(session)
    }

    /// Re-place the player at the room's first walk zone centre, the
    /// reference's `TestHooks_PlaceRoomSpawn` position. The room's camera
    /// switch-zone scan is not re-run: the reference's hook only moves the
    /// player, so the camera stays as the new-game position selected it.
    fn place_room_boot_spawn(&mut self) {
        if !self.room_boot_respawn {
            return;
        }
        let spawn = player::spawn(self.loaded.id, &self.loaded.room);
        self.player.pos = spawn.pos;
        self.player.angle = spawn.angle;
        self.player.collision_mirror = spawn.collision_mirror;
        self.game.sync_entity_from_player(&self.player);
    }

    /// Boot one attract-demo reel (`demo/<hex>.dat`).
    ///
    /// This is the reference's title-timeout hand-off: `InitializeGame`'s
    /// new-game branch with `MSF2_ATTRACT_DEMO` set, which copies the bio card
    /// but loads the reel's header instead of `InitPlayerData` — the recorded
    /// character, room, position, facing, items, equipped item, camera and
    /// health. `game_loop` seeds the platform stream with `srand(0)` for
    /// attract playback, so the reel replays its recorded draws exactly.
    fn from_demo(pack: &Pack, entry: &str, save_dir: &Path) -> Result<Box<Self>> {
        let bytes = pack
            .read(entry)
            .with_context(|| format!("missing attract demo reel {entry}"))?;
        let reel = demo::DemoReel::parse(entry, &bytes)?;
        let id = reel.room;
        let loaded = load_room(pack, id)?;
        let mut game = new_game_state(pack, id, &loaded.room);
        seed_new_game(pack, &mut game, reel.character);
        // The attract branch sets the mode bit before the room loads, so every
        // script and entity driver sees it, and seeds the PRNG as game_loop
        // does for attract playback.
        game.apply_flag(5, game::MSF2_ATTRACT_DEMO, 0);
        game.srand(0);
        // The header's recorded inventory, equipped item and health replace
        // the new-game seeding (the attract branch never runs InitPlayerData).
        game.inventory.clear();
        for pair in reel.items.as_chunks::<2>().0 {
            if pair[0] != 0 {
                game.inventory.push(game::InventoryItem {
                    id: pair[0],
                    quantity: pair[1],
                });
            }
        }
        game.state_bytes[usize::from(game::STATE_BYTE_TOTAL_HELD)] = reel.total_held_items;
        // The recorded `equippedItemId` is a 1-based index into the character's
        // item slots, not an item id: `SetupCharacterData` (which runs after
        // `LoadAttractModePlayerData`) resolves it through the bio card's slot
        // array (`g_itemboxSlots[equippedItemId + 47]`, whose tail is the
        // character's own slots) and derives the equipped weapon from the item
        // found there. The attract boot runs that derivation with the reel's
        // recorded inventory already in place, so a reel whose equipped slot
        // holds the M92FS aims the M92FS, not whatever weapon the default state
        // carried - demo 3's Jill carries the Ingram (item 0x6F ->
        // `player/w18.emw`), not the shipped card's Beretta.
        let equipped_item = usize::from(reel.equipped_item)
            .checked_sub(1)
            .and_then(|slot| reel.items.get(slot * 2))
            .copied()
            .unwrap_or(0);
        game.equipped = (equipped_item != 0).then_some(equipped_item);
        game.selected_item = game.equipped;
        game.equipped_weapon_id = equipped_item;
        game.entities[0].health = reel.health;
        game.set_health_copy(reel.health);
        game.set_attract_room_camera_id(reel.camera_id);
        let mut player_state = player::spawn(id, &loaded.room);
        player_state.pos = reel.pos;
        player_state.angle = reel.angle;
        game.sync_entity_from_player(&player_state);
        let mut session = Self::from_loaded(pack, loaded, game, player_state, save_dir)?;
        session.attract = Some(demo::AttractState::new(reel));
        Ok(session)
    }

    /// Boot the `index`-th entry of the pack's `demo/` enumeration (the
    /// reference's `--test-demo` anchor and the port's `--ui demo`).
    fn from_demo_index(pack: &Pack, index: u8, save_dir: &Path) -> Result<Box<Self>> {
        let entries = demo::entries(pack);
        let entry = entries.get(usize::from(index)).with_context(|| {
            format!(
                "pack has {} demo entries; demo {index} is out of range",
                entries.len()
            )
        })?;
        Self::from_demo(pack, entry, save_dir)
    }

    /// Boot `id` directly, the `--room` path. A direct boot is a fresh game
    /// state, so the shipped room-items bank and the selected character's
    /// start health are seeded like a new game's, and the player is placed at
    /// the room's paired-door entry once the room boot has built the door
    /// table.
    fn from_room(pack: &Pack, id: RoomId, save_dir: &Path) -> Result<Box<Self>> {
        let loaded = load_room(pack, id)?;
        let mut game = new_game_state(pack, id, &loaded.room);
        seed_room_items(&mut game);
        seed_start_health(&mut game, id.player_flag);
        let player_state = player::spawn(id, &loaded.room);
        game.sync_entity_from_player(&player_state);
        let mut session = Self::from_loaded(pack, loaded, game, player_state, save_dir)?;
        session.place_room_entry(pack, id);
        Ok(session)
    }

    /// Continue from a parsed save: the block replaces the state, the saved
    /// position and angle place the player and the saved room loads.
    fn from_save(pack: &Pack, file: &save::SaveFile, save_dir: &Path) -> Result<Box<Self>> {
        let id = RoomId {
            // The block's stage byte is the original's 0-based `g_stageId`.
            stage: file.stage + 1,
            room: file.room,
            player_flag: file.character & 1,
        };
        let loaded = load_room(pack, id)?;
        let mut game = new_game_state(pack, id, &loaded.room);
        file.apply_to(&mut game);
        // `InitializeGame`'s continue branch: a zero save counter also zeroes
        // the play timer, then every load advances the counter.
        if game.state_bytes[usize::from(game::STATE_BYTE_SAVES)] == 0 {
            game.state_bytes[0x24..0x28].fill(0);
        }
        game.increment_saves();
        let mut player_state = player::spawn(id, &loaded.room);
        player_state.pos = [
            i32::from(file.pos_x),
            player_state.pos[1],
            i32::from(file.pos_z),
        ];
        player_state.angle = file.angle as u16 & 0x0FFF;
        // The room collision resolve anchors its rollback at the mirror, so a
        // continue must move it with the restored position; otherwise the
        // first tick resolves the segment back to the spawn's walk-zone centre
        // and teleports the player there.
        player_state.collision_mirror = [i32::from(file.pos_x), i32::from(file.pos_z)];
        game.sync_entity_from_player(&player_state);
        Self::from_loaded(pack, loaded, game, player_state, save_dir)
    }

    /// Assemble a session around already-built state and run the room boot.
    fn from_loaded(
        pack: &Pack,
        loaded: LoadedRoom,
        game: game::GameState,
        player_state: player::PlayerState,
        save_dir: &Path,
    ) -> Result<Box<Self>> {
        let scripts = Rc::new(loaded.scripts.clone());
        let command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
        let event_vm = scd::vm::EventVm::from_scripts(scripts);
        // The message window's glyph sheet is optional: a pack without one
        // still runs the state machine and simply draws nothing. The sheet
        // follows the script the language files encode in, so a pack that
        // ships both sheets draws each language with its own.
        let text = Text::load(pack, pack.language());
        let font = font::load_session(pack, text.script());
        // The pack's scripts load once per session; `on_init` runs before the
        // room's own hook so a screen registration is in place for the
        // typewriter.
        let lua = load_lua(pack);
        let mut game = game;
        call_lua_init(lua.as_ref(), &mut game);
        let mut session = Box::new(Self {
            loaded,
            game,
            player: player_state,
            command_vm,
            event_vm,
            masks: MaskCache::default(),
            shadows: ShadowCache::default(),
            effect_pages: EffectPageCache::default(),
            sfx_cache: SfxCache::default(),
            bgm_cache: bgm::BgmCache::default(),
            voice_cache: VoiceCache::default(),
            movie: None,
            npc_models: enemy::EntityModelCache::default(),
            enemy_lua: enemy::LuaEnemyHost::new(),
            music: None,
            text,
            font,
            menu_assets: None,
            menu_assets_loaded: false,
            menu: None,
            item_box: None,
            item_box_assets: None,
            file: None,
            file_assets: None,
            map: None,
            save_screen: None,
            save_dir: save_dir.to_path_buf(),
            lua,
            framebuffer: Framebuffer::new(),
            framebuffer_white: None,
            gpu_scene_visible: false,
            gpu_room: None,
            titled_cut: usize::MAX,
            transition: None,
            transition_finished: false,
            pickup_view: None,
            examine: None,
            swallow_action: false,
            boot_message: None,
            room_fade: transition::Fade::inactive(),
            room_fade_draw: None,
            tween: Tween::default(),
            room_boot_gate: false,
            room_boot_grace: false,
            room_boot_respawn: false,
            menu_fade: transition::Fade::inactive(),
            menu_fade_phase: MenuFadePhase::None,
            menu_fade_pending: false,
            menu_fade_draw: None,
            menu_fade_started: false,
            menu_exit_restored: false,
            typewriter_fade: None,
            typewriter_arm: None,
            typewriter_open: None,
            typewriter_exit_fade: false,
            itembox_open_delay: 0,
            menu_close_freeze: 0,
            menu_close_freeze_first: false,
            menu_close_restore: false,
            menu_open_delay: false,
            menu_reveal_delay: 0,
            menu_close_wait: 0,
            sprite_anim_intensity: 0,
            debug_menu_enabled: true,
            debug_menu: None,
            debug_menu_memory: None,
            bindings: controls::Bindings::default(),
            bindings_changed: false,
            frame_rate: video::TICK_RATE,
            frame_rate_changed: false,
            return_title: None,
            return_title_requested: false,
            load_slot_requested: None,
            clear_game_requested: None,
            attract: None,
            death: ui::death::DeathSequence::idle(),
            died_texture: None,
            died_texture_tried: false,
            debug_failed: HashSet::new(),
            computer: None,
            computer_saved: None,
        });
        session.enter_room(pack, None, RoomBgmHandoff::Now);
        Ok(session)
    }

    /// Run the room boot: init script, queued events, the player mirror, mask
    /// toggles, the camera zone scan and, when `bgm_handoff` is
    /// [`RoomBgmHandoff::Now`], the per-room BGM handoff.
    ///
    /// `from` is the room the player came from, used by the BGM state machine
    /// to resolve the outgoing group; `None` for a direct boot or save load.
    /// A door transition passes [`RoomBgmHandoff::AtTransitionEnd`]: the
    /// original loads the destination while the animation plays but runs
    /// `update_room_bgm` only after the animation task finishes (see
    /// [`GameSession::end_transition`]).
    fn enter_room(&mut self, pack: &Pack, from: Option<RoomId>, bgm_handoff: RoomBgmHandoff) {
        self.apply_costume_model(pack);
        // The original stores the outgoing room id in
        // `g_AttractMode_RoomCameraId` on a same-stage door; a fresh or
        // stage-change load resets it to 0x1F. The hunter's scripted intro
        // reads it as a camera selector.
        let attract_room_camera_id = match from {
            Some(source) if source.stage == self.game.id.stage => source.room,
            _ => 0x1F,
        };
        self.game.set_attract_room_camera_id(attract_room_camera_id);
        {
            let mut host = game::ScdGameHost::new(&mut self.game);
            self.command_vm.run_init(&mut host);
        }
        self.apply_screen_border();
        // The init script's starts and kills apply in program order before the
        // Lua room hook or the first tick can observe the slots.
        apply_event_requests(&mut self.game, &mut self.event_vm);
        self.game.apply_room_edits(&mut self.loaded.room);
        // The Lua room-entry hook runs once the room state exists (RDT, init
        // script and room edits applied) and before the first tick's movement.
        call_lua_room_load(self.lua.as_ref(), &mut self.game, self.loaded.id);
        self.game.sync_player(&mut self.player);
        drain_mask_toggles(&mut self.loaded.room, &mut self.game);
        reset_room_camera_id(&mut self.game);
        apply_camera(&mut self.loaded.room, &mut self.game, Some(self.player.pos));
        // The original runs `update_room_bgm` after the destination's init
        // script (so a `room_bgm_state_set` it performs is visible) and after
        // the room's data is loaded. A transition defers this call to the end
        // of the door animation, so the outgoing room's music keeps playing
        // until the destination is on screen; the reconciliation still runs so
        // a live BGM opcode in the init script reaches the mixer immediately.
        if bgm_handoff == RoomBgmHandoff::Now {
            bgm::update_room_bgm(&mut self.game, self.loaded.id, from);
        }
        bgm::apply_live(&mut self.music, &mut self.game, &mut self.bgm_cache, pack);
    }

    /// The alternate outfit (`LoadEntityEMD`): with the costume bit raised,
    /// the player's model loads from the costume EMD (`EM1030`-`EM1033`)
    /// instead of the character's own; the clips stay the character's. A
    /// pack without the costume entry keeps the base model.
    fn apply_costume_model(&mut self, pack: &Pack) {
        if !self.game.flag_test(5, game::MSF2_COSTUME_VARIANT, false) {
            return;
        }
        let Some(assets) = self.loaded.player_assets.as_mut() else {
            return;
        };
        let entry =
            crate::convert::costume_entry(self.loaded.id.player_flag, self.game.costume_variant);
        match pack.read(&entry).map(|bytes| emd::parse(&bytes)) {
            Ok(Ok(model)) => assets.emd = model,
            Ok(Err(err)) => eprintln!("warning: invalid costume model {entry}: {err:#}"),
            Err(err) => eprintln!("warning: missing costume model {entry}: {err:#}"),
        }
    }

    /// Re-slice the loaded backgrounds to 316x236 when the room's
    /// screen-border flag is set (msf2 bit 2, the caves): those paks store
    /// their artwork 316 pixels wide while the TIM header claims 320, so the
    /// same linear pixels are re-wrapped. The reference reads the flag when it
    /// loads the background, after the init script and the room_set event
    /// pass, so this runs at both points; a second call is a no-op.
    fn apply_screen_border(&mut self) {
        if self.game.flag_test(5, game::MSF2_SCREEN_BORDER, false) {
            for cut in &mut self.loaded.room.cuts {
                if let Some(background) = cut.background.as_mut() {
                    background.reslice(316, 236);
                }
            }
        }
    }

    /// Place the player for an arbitrary room start: the room's paired-door
    /// entry when it has one, otherwise the walk-zone spawn [`player::spawn`]
    /// already set. Runs after the room boot has built the door table and
    /// re-runs the camera switch-zone scan from the final position.
    fn place_room_entry(&mut self, pack: &Pack, id: RoomId) {
        if let Some((pos, angle)) = room_entry_placement(pack, &self.loaded.room, &self.game, id) {
            self.player.pos = pos;
            self.player.angle = angle;
            self.game.sync_entity_from_player(&self.player);
        }
        // `room_set` zeroes `g_roomCameraId` before its switch-zone scan, so a
        // room load selects its cut from camera 0. The boot scan above ran at
        // the walk-zone spawn (or, for a debug jump, at the source room's
        // position), which can select a different group; without the reset the
        // scan starts from that stale cut and misses the group-0 zone that
        // covers the door arrival (room 101 boots on cut 3, where the debug
        // jump lands, not the spawn's cut 1). A camera lock from the init
        // script survives, exactly like `room_set`'s branch.
        reset_room_camera_id(&mut self.game);
        // A room script that sets `MSF_CAMERA_DEFER` makes the boot scan raise
        // the deferred redisplay instead of switching; that camera describes
        // the pre-placement position and must not make `apply_camera` skip the
        // scan. `room_set` tests its zones directly, so drop it here.
        self.game.flags[5].apply(game::MSF_CAMERA_REDRAW, 1);
        apply_camera(&mut self.loaded.room, &mut self.game, Some(self.player.pos));
    }

    /// One fixed 30 Hz tick: scripts/entities, the message window, interaction,
    /// player movement, camera, BGM/mask/footstep drains and a door transition
    /// request.
    ///
    /// The message window advances after the script/entity pass, exactly like
    /// the original's `main_loop`, so a message a script raises this tick (and
    /// the state that script changed) is visible to the same frame's window
    /// and its yes/no post-actions (item pickup and use) run after the
    /// scripts. While a message masks the control bit the player's movement
    /// and action input are blanked, exactly like the original's cleared d-pad
    /// word. START opens the pause menu while no message owns the tick; while
    /// the menu is up the room stays frozen and the same input drives the
    /// menu.
    fn tick(&mut self, pack: &Pack, ui: UiInput, input: player::Input, action: bool) -> Result<()> {
        self.tween.advance();
        let result = self.tick_inner(pack, ui, input, action);
        // `main_loop` ramps the cutscene screen intensity in its draw phase,
        // after this frame's scripts, entities and message window have run, so
        // the ramp sees a flag a script raised on the same frame. Stepping it
        // at the top of the tick made the bars lead the reference by one
        // 16-step for the whole ramp (the oracle's frame diff showed it in the
        // top/bottom letterbox bands). The early-return paths (the death
        // screen's own loop, the computer terminal, the typewriter hand-off)
        // still step once per platform frame, exactly like the original's.
        self.tick_screen_intensity();
        result
    }

    fn tick_inner(
        &mut self,
        pack: &Pack,
        mut ui: UiInput,
        mut input: player::Input,
        mut action: bool,
    ) -> Result<()> {
        // The attract reel owns the pad while it plays: the recorded word
        // replaces the live input exactly like the original's InputSystem
        // overwrite, and any live button ends the playback (the watchdog
        // below reads it).
        let live_input = ui.any
            || input.up
            || input.down
            || input.left
            || input.right
            || input.run
            || input.action_held
            || input.aim
            || input.fire;
        if let Some(attract) = self.attract.as_mut() {
            // The playback gate opens when the room-entry fade's accumulator
            // drops below one (the reference's `g_message_flags` bit 0x200),
            // which is also the tick the player state machine unfreezes. The
            // bit is published into the message flags so the live re-read
            // below sees it, and whatever clears it later (a room script, or
            // the death sequence's block-input state, which zeroes the whole
            // word) pauses the counter and zeroes the injected pad.
            if !attract.gate && self.room_fade.state() < 1 {
                attract.gate = true;
                self.game.message_flags |= game::MESSAGE_FLAG_DEMO_PLAYBACK;
            }
            attract.live_gate = self.game.message_flags & game::MESSAGE_FLAG_DEMO_PLAYBACK != 0;
            let translated = attract.tick_input();
            ui = translated.ui;
            input = translated.player;
            action = translated.action;
        }
        // Frames the menu's cursor and 0xBA item-name line (and the item box's
        // own `itembox_draw_cursor`) are still withheld: the reference starts
        // drawing them from the menu's first *input* frame, which is the frame
        // after both the fade-complete frame and the state-0 frame that arms
        // the submenu. The reference's box interaction and `menu_handle_input`
        // own that draw, and neither runs before the state machine's switch.
        if self.menu_reveal_delay > 0 {
            self.menu_reveal_delay -= 1;
        }
        // The loading narration owns the tick while it is up: the room stays
        // frozen on a black screen and only the message window advances. The
        // tick the message clears arms the room-entry fade, so the first room
        // frames fade up from black exactly like the original.
        if self.boot_message.is_some() {
            self.tick_boot_message(input, action);
            return Ok(());
        }
        // The item box's menu handshake: the frame after the lid settles the
        // panel fade arms and the frame is owned by the menu (the settle
        // frame is game_loop's last: it raises the menu request after the
        // room has drawn the settled lid, then main_loop hands the next frame
        // to main_menu). Opening here, before the room tick, keeps the lid
        // open on screen until the UI closes and the settle handshake's rand
        // draws in step with the reference.
        if self.itembox_open_delay > 0 {
            self.itembox_open_delay -= 1;
            if self.itembox_open_delay == 0 {
                let _ = self.game.take_itembox_open();
                self.begin_item_box_open(pack);
                return Ok(());
            }
        }
        // The typewriter's save hand-off: the fade-out runs while the room
        // keeps ticking (exactly the reference's next-frame
        // `check_typewriter_state`); the frame after the fade completes still
        // runs the room tick (`LoadSaveGameState`'s transition), and the save
        // screen owns the frame after that.
        if let Some(ink_ribbon) = self.typewriter_open {
            self.typewriter_open = None;
            self.open_typewriter_save(pack, ink_ribbon);
            return Ok(());
        }
        if let Some(ink_ribbon) = self.typewriter_arm {
            self.typewriter_arm = None;
            self.typewriter_fade = Some(ink_ribbon);
            self.room_fade.set(2, 0x1000, 0);
            // The reference keeps the player machine frozen through the
            // hand-off: the typewriter flow restores the message word only
            // when the save screen closes.
            self.game.message_flags &= !game::MESSAGE_FLAG_PLAYER_STATE;
        }
        if let Some(ink_ribbon) = self.typewriter_fade
            && self.room_fade.state() < 0
        {
            self.typewriter_fade = None;
            self.typewriter_open = Some(ink_ribbon);
        }
        // The DIED screen owns the whole tick while it is up: the room stays
        // frozen and the screen's state machine, fade and spinning corpse
        // advance on their own, exactly like the original's display loop.
        if self.death.screen_open() {
            self.tick_death_screen(ui, input);
            return Ok(());
        }
        // The lab computer terminal owns the whole tick while it is up: the
        // room is frozen on its monitor camera and only the terminal's state
        // machine advances (`check_and_display_interactive_screen`).
        if self.computer.is_some() {
            self.tick_computer(ui);
            return Ok(());
        }
        // The original's room-events gate (`g_message_flags` bit 0x80): the
        // entry fade's state machine restores the bit on the tick its
        // accumulator drops below 1, and until then `room_events_check` does
        // not step. The accumulator is read before this tick's fade step
        // because the original draws and tests it before `main_loop` adds the
        // counter.
        let events_enabled = !self.room_boot_gate || self.room_fade.state() < 1;
        if events_enabled {
            if self.room_boot_gate {
                self.room_boot_gate = false;
                self.room_boot_grace = false;
                // The inner-loop state machine restores the saved word the
                // tick the entry fade's accumulator drops below 1. The port
                // mirrors the player-state, character and monster bits here
                // (its other message bits have their own consumers); the
                // reference's word is `g_short_message_flags | 0x200` = 0xFF3F.
                self.game.message_flags |= game::MESSAGE_FLAG_PLAYER_STATE
                    | game::MESSAGE_FLAG_ENTITIES
                    | game::MESSAGE_FLAG_MONSTERS
                    | game::MESSAGE_FLAG_EFFECTS;
            }
        } else {
            // While the entry fade runs the reference's `g_message_flags` is
            // cleared, which freezes the whole player animation state machine,
            // the scripted characters and the monsters: the post-door idle
            // countdown must not start and no zombie may think until the gate
            // opens. The clearing starts the tick AFTER the entry, so the spawn
            // init (player state 0 to 1) still runs under the closed gate.
            if !self.room_boot_grace {
                self.game.message_flags &= !(game::MESSAGE_FLAG_PLAYER_STATE
                    | game::MESSAGE_FLAG_ENTITIES
                    | game::MESSAGE_FLAG_MONSTERS
                    | game::MESSAGE_FLAG_EFFECTS);
            }
            self.room_boot_grace = false;
        }
        // The room-entry fade after a menu close suspends the reference's
        // game_loop for its whole run, freezing the player machine (its idle
        // countdown included) and the entities. The first frame after the menu
        // task exits still runs one gameplay tick before the freeze takes hold;
        // on that frame the reference's restored word has the player bit but
        // not the entity/monster/effect bits, so the player machine resumes one
        // frame before the NPCs.
        if self.menu_close_freeze > 0 {
            if self.menu_close_freeze_first {
                self.menu_close_freeze_first = false;
                self.game.message_flags &= !(game::MESSAGE_FLAG_ENTITIES
                    | game::MESSAGE_FLAG_MONSTERS
                    | game::MESSAGE_FLAG_EFFECTS);
            } else {
                self.menu_close_freeze -= 1;
                self.game.message_flags &= !(game::MESSAGE_FLAG_PLAYER_STATE
                    | game::MESSAGE_FLAG_ENTITIES
                    | game::MESSAGE_FLAG_MONSTERS
                    | game::MESSAGE_FLAG_EFFECTS);
                if self.menu_close_freeze == 0 {
                    self.menu_close_restore = true;
                }
            }
        } else if self.menu_close_restore {
            self.menu_close_restore = false;
            self.game.message_flags |= game::MESSAGE_FLAG_PLAYER_STATE
                | game::MESSAGE_FLAG_ENTITIES
                | game::MESSAGE_FLAG_MONSTERS
                | game::MESSAGE_FLAG_EFFECTS;
        }
        self.event_vm.set_enabled(events_enabled);
        // The typewriter exit fade has run out: restore the player machine gate
        // (the reference's `check_typewriter_state` case 4 does
        // `message_flags |= 0x45`; the port never clears the other two bits, so
        // only the player-state bit is put back). The original tests the
        // accumulator at the top of the frame, before `main_loop` advances it,
        // so the gate opens the frame AFTER the one whose step wrapped it
        // negative - that frame still runs its room tick with the player
        // machine frozen. Read the accumulator before this tick's fade step for
        // the same reason.
        if self.typewriter_exit_fade && self.room_fade.state() < 0 {
            self.typewriter_exit_fade = false;
            self.game.message_flags |= game::MESSAGE_FLAG_PLAYER_STATE;
        }
        self.tick_room_fade();
        // A film owns the tick while it plays: the room's scripts, entities,
        // effects and the player stay frozen, exactly like the original's
        // main-loop jump.
        if self.movie.is_some() {
            self.tick_movie(ui, input);
            return Ok(());
        }
        if self.save_screen.is_some() {
            self.tick_save_screen(pack, ui);
            return Ok(());
        }
        // The F9 return-to-title prompt owns the frozen tick while it is open;
        // a second F9 confirms and any other key cancels, exactly like the
        // original's prompt.
        if self.return_title.is_some() {
            self.tick_return_title(ui);
            return Ok(());
        }
        // The port-only debug room overlay owns the frozen tick while it is
        // open; it is gameplay-only, so the pause menu and its modals below
        // never see it and F1 cannot open it from them.
        if self.debug_menu.is_some() {
            self.tick_debug_menu(pack, ui);
            return Ok(());
        }
        // F9 opens the return-to-title prompt before the pause-menu checks so
        // it works over the pause menu, exactly like the original.
        if ui.return_title {
            self.open_return_title();
            return Ok(());
        }
        if self.menu_fade_phase != MenuFadePhase::None {
            self.tick_menu_fade();
            return Ok(());
        }
        if self.menu.is_some() {
            // The frame after the open fade finishes is the original's state-0
            // -> state-1 transition: no health-bar tick and no input yet.
            if self.menu_open_delay {
                self.menu_open_delay = false;
                return Ok(());
            }
            self.tick_menu(pack, ui, input, action);
            return Ok(());
        }
        // F1 opens the debug room overlay while playing, exactly like START
        // opens the pause menu.
        if self.debug_menu_enabled && ui.debug_menu {
            self.open_debug_menu(pack);
            return Ok(());
        }

        let was_active = self.game.message.active;

        // START opens the pause menu while no message owns the tick. The
        // window advances after the script pass, so the frame's start state
        // decides; the press that dismissed a window is spent on it.
        //
        // The original gates the request on `isBeingAttackedFlag == 0`
        // (GameLoop.cpp 0x00480de8: `g_playerEntity.isBeingAttackedFlag == 0`),
        // so a grabbed or bitten player cannot open the inventory mid-attack.
        // The flag's high bit (0x80) marks the scripted door/climb poses and
        // blocks the menu the same way; only an exact zero passes. This is the
        // attack clause only: the reference also checks the message word's
        // 0x100/0x40 bits and the menu-pending state, which `!was_active`
        // covers for normal message windows. The gate runs before the room
        // tick, so it reads the previous tick's entity state; a script that
        // raises the flag during this frame's SCD pass would therefore differ
        // by one frame (the reference checks after the script pass).
        if ui.start && !was_active && self.game.entities[0].is_being_attacked == 0 {
            // The reference draws this frame's random seed before its menu
            // request is even detected (game_loop's first statement), then
            // opens the menu at the end of the same tick. arklay opens the menu
            // here, ahead of the room tick, so it must draw the seed itself or
            // the shared stream falls one draw behind for the rest of the run.
            self.game.draw_frame_rand();
            self.begin_menu_open(pack);
            return Ok(());
        }

        // The press that dismissed a message last tick is swallowed until the
        // key is released, exactly like the original's cleared edge-detect
        // history. The release is seen on this tick so the room probe runs on
        // the frame the key comes up. A dismissal last tick blanked the
        // direction bits of a state-5/6 message into `game.dpad_blanked`;
        // releasing a blanked direction restores it, a still-held one stays
        // suppressed for this tick.
        if !action {
            self.swallow_action = false;
        }
        let raw = input;
        self.game.dpad_blanked &= game::dpad_word(&raw) & game::DPAD_DIRECTIONS;
        let mut input = raw;
        if self.game.dpad_blanked & 0x1 != 0 {
            input.up = false;
        }
        if self.game.dpad_blanked & 0x2 != 0 {
            input.down = false;
        }
        if self.game.dpad_blanked & 0x4 != 0 {
            input.left = false;
        }
        if self.game.dpad_blanked & 0x8 != 0 {
            input.right = false;
        }
        // The message freeze is applied inside the room tick, at the original's
        // point: the scripts still see the raw pad this frame, and the player's
        // state machine is skipped after the entity update. Only the dismissed
        // action edge is swallowed here.
        let input = if self.swallow_action {
            player::Input::default()
        } else {
            input
        };

        let message_before = self.game.message.menu_choice_id() & 0x80 != 0;
        let transition = tick_room(
            &mut self.command_vm,
            &mut self.event_vm,
            RoomContext {
                room: &mut self.loaded.room,
                game: &mut self.game,
                player: &mut self.player,
                player_assets: self.loaded.player_assets.as_ref(),
                pack,
                npc_models: &mut self.npc_models,
                enemy_lua: Some(&mut self.enemy_lua),
            },
            input,
        );
        self.game.apply_invincible();
        // While the pad-driven locomotion owns the player, mirror its
        // reference scratch into entity 0: `player_behavior_00_idle` keeps
        // `action_state` as the idle phase and `attackDirection` as the
        // 100-tick settle countdown, and the locomotion behaviours keep their
        // own action_behavior/action_state, and scripts plus the oracle trace
        // read those bytes. The room-entry gate (the original's
        // `g_message_flags` freeze) keeps the player machine from running
        // until the fade has stepped, exactly like the reference's post-load
        // window. `trace_state` is `Some` exactly when the machine ran this
        // tick - the frozen and scripted paths clear it - so it, not the live
        // message flags, is the gate: a message requested after the machine
        // ran still reports this tick's bytes, which is what the reference's
        // live globals hold.
        if !self.room_boot_gate
            && let Some((behavior, action_state, ticks)) = self.player.trace_state
        {
            // The settle countdown publishes whenever the pad machine ran this
            // tick, even if a later effect flipped the state byte: the original
            // wrote it during the player update, before the effects ran.
            if let Some(ticks) = ticks {
                self.game.entities[0].action_ticks_counter = ticks;
            }
            // The behaviour/action-state bytes are the machine's only while it
            // still owns the player; a post-update grab or splash overwrote
            // them and must win.
            if self.game.entities[0].state() == 1 {
                self.game.entities[0].action_behavior = behavior;
                self.game.entities[0].action_state = action_state;
            }
        }
        // The Lua hooks run after the room tick: `on_tick` sees the whole
        // tick's state and `on_message` may rewrite a message the tick raised
        // before the window resolves its bytes.
        run_lua_tick_hooks(self.lua.as_ref(), &mut self.game, message_before);
        // `check_and_display_interactive_screen` runs right after the scripts:
        // the room action's event has raised scenario flag 0x20 by now, and the
        // room's init set the SysFlags bit that selects the computer terminal.
        self.maybe_open_computer();
        // The message window advances after the script/entity task pass,
        // exactly like the original's `main_loop`: a request a script raised
        // this tick and the state it changed are visible to the same frame's
        // window, and the window's pickup/use post-actions run after the
        // scripts rather than before them.
        self.game.update_message(
            MessageInput {
                action,
                left: raw.left,
                right: raw.right,
                cancel: raw.run || ui.cancel,
            },
            &self.loaded.room,
            &self.text,
        );
        // The dismissal bookkeeping now sees the post-script window state: a
        // press that dismissed a window this tick latches the swallow until
        // the key is released.
        let dismissed = was_active && !self.game.message.active;
        if dismissed && action {
            self.swallow_action = true;
        }
        // The typewriter prompt was answered: a confirmed save fades the room
        // out (counter 0x1000, armed and stepped on this frame) and the save
        // screen opens when it completes; the rest of the room's tick is
        // skipped.
        if let Some(ink_ribbon) = self.game.take_typewriter_confirm() {
            self.typewriter_arm = Some(ink_ribbon);
            return Ok(());
        }
        // A typewriter runs its save prompt; the item-box overlay opens when
        // the lid has settled (`MSF_MENU_MODE_ITEMBOX`). The interaction is
        // consumed so the next probe has to fire again.
        let interaction = self.game.last_interaction.take();
        let typewriter_fired = interaction
            .is_some_and(|interaction| interaction.kind == game::RoomActionKind::Typewriter);
        if typewriter_fired {
            self.game.check_typewriter();
        }
        // The room fade-out that a confirmed typewriter save armed completes on
        // this tick (the accumulator crossed negative at the top of the frame,
        // where `typewriter_fade` published `typewriter_open`). The reference's
        // `check_typewriter_state` case 3 runs at this point in its room tick
        // and blocks inside `LoadSaveGameState`, so the save screen takes the
        // rest of the frame; opening here rather than at the next tick's top
        // keeps the screen's first drawn frame in step with the reference.
        if let Some(ink_ribbon) = self.typewriter_open.take() {
            self.open_typewriter_save(pack, ink_ribbon);
            return Ok(());
        }
        // The scripts have had their say: resolve the tick's `se_play_3d`
        // requests (a bank-4 request pans and restarts BGM channel 0 before
        // the BGM reconciliation below), then reconcile the three BGM
        // channels and start any queued voice line whose wait flag is set.
        play_snd3d_requests(
            &mut self.music,
            &mut self.sfx_cache,
            pack,
            &self.loaded.room,
            &mut self.game,
        );
        bgm::apply_live(&mut self.music, &mut self.game, &mut self.bgm_cache, pack);
        tick_voice(&mut self.music, &mut self.voice_cache, &mut self.game, pack);
        drain_mask_toggles(&mut self.loaded.room, &mut self.game);
        // `MSF2_EFFECT_ZONE` (the second dword of main-state flag bank 5) is
        // the original's slippery/effect-zone bit: it shifts every footstep
        // column by -3.
        let slow = self.game.flags[5].bit(game::MSF2_EFFECT_ZONE);
        play_footsteps(
            &mut self.music,
            &mut self.sfx_cache,
            pack,
            &self.loaded.room,
            &mut self.player,
            slow,
        );
        play_entity_sounds(
            &mut self.music,
            &mut self.sfx_cache,
            pack,
            &self.loaded.room,
            &mut self.game.entity_sounds,
        );
        play_player_sounds(
            &mut self.music,
            &mut self.sfx_cache,
            pack,
            &self.loaded.room,
            &mut self.player,
            self.game.id.player_flag,
            slow,
        );
        // Queued non-positional room SEs (`play_sfx(2, id)`) resolve through
        // the current room's sound-table column, exactly like the original's
        // bank-2 calls: the item-box lid (0x20), the locked-door rattles, the
        // key turn and the desk cues all arrive here.
        let requests = std::mem::take(&mut self.game.sfx_requests);
        if !requests.is_empty() {
            let row = sfx::room_row(self.loaded.room.stage, self.loaded.room.room);
            for id in requests {
                let Ok(column) = u8::try_from(id) else {
                    continue;
                };
                let Some(name) = sfx::room_sound(row, usize::from(column)) else {
                    continue;
                };
                let Some(wav) = self.sfx_cache.load(pack, name) else {
                    continue;
                };
                if let Some(mixer) = &mut self.music {
                    mixer.play_sfx_on_bank(sfx_bank_key(2, column), wav, 1.0, 0.0);
                }
            }
        }
        // A script's `movie_on` request takes over after this tick: the film
        // owns the next tick and the game sounds pause until it ends. A
        // pending door transition wins and leaves the request for after it.
        // A door request raised by the destination's gameplay while its own
        // transition still draws is dropped: the original reads no gameplay
        // during the door task, and the running timeline owns the overlay.
        if let Some(mut transition) = transition {
            transition.target = self.pending_player_target(pack, transition.target);
            let record = self.game.transition_door.take().unwrap_or_default();
            let door_yield = self.game.door_yield.take();
            if self.transition.is_none() {
                self.transition = Some(start_transition(
                    pack,
                    &record,
                    &transition,
                    self.loaded.id,
                    &self.room_fade,
                )?);
                if let Some(session) = self.transition.as_mut() {
                    session.door_yield = door_yield;
                }
            }
        } else if let Some(id) = self.game.take_fmv_request() {
            if let Err(err) = self.start_movie(pack, id, self.game.id.player_flag) {
                eprintln!("warning: film id {id} unavailable: {err:#}");
            }
        } else if self.game.itembox.menu_open && self.itembox_open_delay == 0 {
            // The lid settled: arm the menu handshake; the opening frame is
            // owned by the head check (no room tick), like the reference. The
            // flag stays up until then so the player machine stays frozen.
            self.itembox_open_delay = 1;
        } else if let Some(request) = self.game.item_view_request() {
            // A room action armed the item viewer: the original's main loop
            // opens its menu over the frozen room. It waits for an active
            // message to clear first (`do { ... } while (menu_choice & 0x80)`),
            // so the pending flag can outlive this tick.
            if !self.game.message.active {
                self.open_pickup_view(pack, request);
            }
        }
        // The death machine runs at the end of the gameplay frame, after the
        // room tick noticed the health drop, exactly like the original.
        self.tick_death();
        // The attract watchdog runs after the gameplay tick: it advances the
        // playback counter and, once the reel is within ten frames of its
        // recorded end or a live button is held, raises the dead flag. The
        // game-over machine picks it up on the next tick, like the original's
        // per-frame `StartAttractDemo`.
        if let Some(attract) = self.attract.as_mut() {
            attract.live_gate = self.game.message_flags & game::MESSAGE_FLAG_DEMO_PLAYBACK != 0;
            if attract.gate {
                attract.advance();
            }
            if !attract.ended && attract.should_end(live_input) {
                attract.ended = true;
                self.game.apply_flag(5, game::MSF_PLAYER_DEAD, 0);
                // The original's `StartAttractDemo` tail clears the player,
                // entity and monster gates (`g_message_flags &= 0xFE70`), so
                // the ending fade plays over a frozen world.
                self.game.message_flags &= 0xFE70;
            }
        }
        Ok(())
    }

    /// The game-over machine's pre-screen phase: notice a dead player, raise
    /// the dead flag, start the sequence and advance its delay/fade. Once the
    /// fade completes the DIED screen opens over the corpse.
    fn tick_death(&mut self) {
        if self.death.finished || self.death.screen_open() {
            return;
        }
        // The INVINCIBLE cheat never enters the game-over machine.
        if self.game.debug.invincible
            && self.death.is_idle()
            && !self.game.flags[5].bit(game::MSF2_ATTRACT_DEMO)
        {
            self.game.apply_flag(5, game::MSF_PLAYER_DEAD, 1);
            return;
        }
        if self.game.entities[0].health < 0 && !self.game.flags[5].bit(game::MSF2_ATTRACT_DEMO) {
            self.game.apply_flag(5, game::MSF_PLAYER_DEAD, 0);
        }
        if !self.game.flags[5].bit(game::MSF_PLAYER_DEAD) {
            return;
        }
        if self.death.is_idle() {
            let attract = self.game.flags[5].bit(game::MSF2_ATTRACT_DEMO);
            let variant = self.game.flags[5].bit(game::MSF2_DEATH_VARIANT) || attract;
            self.death
                .start(ui::death::immediate_fade(self.loaded.id), variant, attract);
            return;
        }
        // A scripted death (the hunter pounce) arms the fade itself at its own
        // countdown; consume the request before the delay ticks.
        if std::mem::take(&mut self.game.death_fade_request) {
            self.death.arm_fade_now();
        }
        match self.death.tick() {
            ui::death::DeathTick::OpenScreen => self.open_death_screen(),
            ui::death::DeathTick::Finished => self.finish_death(),
            ui::death::DeathTick::None => {}
        }
    }

    /// The DIED screen's own tick: advance the screen machine and spin the
    /// corpse 8 angle units, exactly like the original's display loop. The
    /// confirm button skips the rest of the screen.
    fn tick_death_screen(&mut self, ui: UiInput, input: player::Input) {
        let confirm = ui.confirm || input.action_held;
        let finished = self
            .death
            .screen
            .as_mut()
            .is_some_and(|screen| screen.tick(confirm));
        if finished {
            self.finish_death();
            return;
        }
        // The original spins the corpse's facing 8 units a frame.
        if let Some(screen) = self.death.screen.as_mut() {
            screen.spin = screen.spin.wrapping_add(8);
            self.player.angle = screen.spin;
            self.game.entities[0].angle = screen.spin & 0x0FFF;
        }
    }

    /// Open the DIED screen over the fallen player: the fixed camera anchors
    /// on the corpse, the head one-shot is consumed and the room's BGM stops.
    fn open_death_screen(&mut self) {
        // The shared grab/death one-shot hides the head joint on the first
        // death screen, exactly like the original's image value.
        if self.game.hunter_grab_one_shot {
            let entity = &mut self.game.entities[0];
            entity.set_joint_flag(1, entity.joint_flag(1) & 0xFE);
        }
        self.game.hunter_grab_one_shot = false;
        let anchor = self.player.pos;
        let angle = self.player.angle;
        let fov = self
            .loaded
            .room
            .cuts
            .get(self.loaded.room.current_cut)
            .map_or(320, |cut| cut.fov);
        self.death.screen = Some(ui::death::DeathScreen::open(anchor, angle, fov));
        if let Some(mixer) = &mut self.music {
            mixer.stop_all_bgm();
        }
    }

    /// The original's `die_state` teardown: clear the screen flags and the
    /// special-light state, then hand back to the app, which returns to the
    /// title.
    fn finish_death(&mut self) {
        self.death.screen = None;
        self.death.finished = true;
        self.sprite_anim_intensity = 0;
        self.game.message.set_menu_choice_id(0);
        self.game.sync_message_choice();
        self.game.message_flags &= !0x100;
        // The screen-mode pair: standalone set, rebuild cleared, and the
        // intensity ramp off.
        self.game.apply_flag(5, 0, 1);
        self.game.apply_flag(5, 1, 0);
        self.game.apply_flag(5, 15, 1);
        self.return_title_requested = true;
    }

    /// Whether a pause-menu screen owns the tick, so the room is frozen and
    /// the original runs on its 16 ms non-gameplay limiter.
    fn room_frozen(&self) -> bool {
        self.menu_fade_phase != MenuFadePhase::None
            || self.menu.is_some()
            || self.item_box.is_some()
            || self.pickup_view.is_some()
            || self.file.is_some()
            || self.map.is_some()
            || self.save_screen.is_some()
            || self.debug_menu.is_some()
            || self.return_title.is_some()
            || self.computer.is_some()
    }

    /// One frozen tick of the active film: advance it against the wall clock
    /// with the mixer's device-playback cursor as a bounded secondary clock
    /// (or the wall clock alone when no device is open), queue the frame of
    /// audio it produces and resume the room when it ends.
    fn tick_movie(&mut self, ui: UiInput, input: player::Input) {
        let consumed = self
            .music
            .as_ref()
            .filter(|mixer| !mixer.is_dummy())
            .map(Mixer::movie_samples_played);
        let Some(session) = self.movie.as_mut() else {
            return;
        };
        let tick = session.tick_timed(movie_buttons(ui, input), consumed, TICK_MS);
        let audio = session.take_audio();
        if !audio.is_empty()
            && let Some(mixer) = &mut self.music
        {
            mixer.load_movie_audio(audio);
        }
        if matches!(tick, MovieTick::Finished | MovieTick::Skipped) {
            self.movie = None;
            if let Some(mixer) = &mut self.music {
                mixer.stop_movie_audio();
                mixer.resume_game_sounds();
            }
        }
    }

    /// Poll the active film's skip input between ticks.
    ///
    /// The original's film state machine is driven once per platform loop
    /// iteration, not once per gameplay tick, so its grace runs on the film
    /// gate's own schedule. The next [`Self::tick_movie`] picks the skip up
    /// and tears the film down.
    fn poll_movie_skip(&mut self, buttons: u16) {
        if let Some(session) = self.movie.as_mut() {
            session.poll_skip(buttons);
        }
    }

    /// Start `id`'s film over the frozen room: pause the game sounds, queue the
    /// film's opening audio and show its first frame. Called by the film
    /// request hand-off once the scripts raise one.
    fn start_movie(&mut self, pack: &Pack, id: u8, character: u8) -> Result<()> {
        if self.movie.is_some() {
            return Ok(());
        }
        let mut session = MovieSession::open(pack, id, character)?;
        if let Some(mixer) = &mut self.music {
            mixer.pause_game_sounds();
            mixer.load_movie_audio(session.take_audio());
        }
        self.movie = Some(session);
        Ok(())
    }

    /// The lazily loaded pause-menu art; a missing or invalid sheet is logged
    /// once and leaves the menu blank but functional.
    fn ensure_menu_assets(&mut self, pack: &Pack) {
        if self.menu_assets_loaded {
            return;
        }
        self.menu_assets_loaded = true;
        let font = self.font.as_ref();
        match MenuAssets::load(pack, font) {
            Ok(assets) => self.menu_assets = Some(assets),
            Err(err) => eprintln!("warning: pause-menu art unavailable: {err:#}"),
        }
    }

    /// Add the known inventory `--ui menu` captures draw: the room boot's
    /// items plus a green herb, a handgun clip and the sword key.
    fn seed_menu_capture(&mut self) {
        self.game.add_item(ITEM_GREEN_HERB, 1);
        self.game.add_item(ITEM_HANDGUN_AMMO, 30);
        self.game.add_item(ITEM_SWORD_KEY, 1);
    }

    /// Seed the deterministic item-box capture: a mixed box plus the menu
    /// capture's inventory.
    fn seed_item_box_capture(&mut self) {
        let slots: [(usize, u8, u8); 5] = [
            (0, ITEM_HANDGUN_AMMO, 30),
            (1, ITEM_FIRST_AID_SPRAY, 1),
            (2, ITEM_GREEN_HERB, 1),
            (5, ITEM_SWORD_KEY, 1),
            (47, 0x02, 15),
        ];
        for (slot, id, quantity) in slots {
            self.game.item_box[slot] = game::InventoryItem { id, quantity };
        }
    }

    /// Seed the deterministic FILE capture: every document collected, so both
    /// books list their slots.
    fn seed_file_capture(&mut self) {
        for index in 0..game::FILE_COUNT as u8 {
            self.game.set_file_collected(index, true);
        }
    }

    /// Open the pause menu over the current inventory: the room freezes, the
    /// last gameplay frame stays underneath and messages switch to the menu
    /// line until it closes.
    fn open_menu(&mut self, pack: &Pack) {
        if self.menu.is_some() {
            return;
        }
        self.ensure_menu_assets(pack);
        self.render(pack);
        let mut menu = MainMenu::new(self.game.inventory_capacity());
        menu.open(&mut self.game);
        self.game.message_menu = true;
        self.menu = Some(menu);
    }

    /// Close the pause menu and unfreeze the room. Any item-box overlay is
    /// part of the menu and drops with it.
    ///
    /// The original's mode-7 exit restore runs with the close: the player's
    /// animation words return to pad control and one body `Joint_move`
    /// pre-applies the idle pose while the fade still covers the screen (see
    /// [`player::PlayerState::menu_exit_restore`]).
    fn close_menu(&mut self) {
        self.menu = None;
        self.examine = None;
        self.item_box = None;
        self.game.message_menu = false;
        if let Some(assets) = self.loaded.player_assets.as_ref() {
            self.player
                .menu_exit_restore(&mut self.game, &assets.emd.clips);
        }
    }

    /// Open the pause menu through the original's fade: the frozen frame
    /// darkens over `0xC00` (24 alpha per tick), then the menu fades in over
    /// `0xE800` (48 per tick) before it accepts input.
    fn begin_menu_open(&mut self, pack: &Pack) {
        if self.menu.is_some() || self.menu_fade_phase != MenuFadePhase::None {
            return;
        }
        self.open_menu(pack);
        // The original's `set_fading` only sets the counter: the accumulator is
        // left at the room-entry fade's negative leftover, and the first step
        // resets it to 0 before adding. Seed it from the room fade so the
        // traced state matches the reference's `-4097` leftovers.
        self.menu_fade.set(2, 24 << 7, self.room_fade.state());
        self.menu_fade_pending = true;
        self.menu_fade_started = false;
        self.menu_fade_phase = MenuFadePhase::Out;
    }

    /// Open the item box through the same fade the START menu uses: the
    /// original's `check_itembox_state` raises the same menu-request flag, so
    /// the frozen frame darkens, the panel (with its box overlay) fades in and
    /// only then accepts input.
    fn begin_item_box_open(&mut self, pack: &Pack) {
        if self.item_box.is_some() || self.menu_fade_phase != MenuFadePhase::None {
            return;
        }
        self.open_item_box(pack);
        self.menu_fade.set(2, 24 << 7, self.room_fade.state());
        // Unlike the START menu, the reference's box request steps the fade
        // on the frame after the arm (game_loop's menu-request path), so the
        // draw-then-add pending tick is not applied here.
        self.menu_fade_pending = false;
        self.menu_fade_started = false;
        self.menu_fade_phase = MenuFadePhase::Out;
    }

    /// Close the pause menu through the original's fade: the cancel cue
    /// plays, the menu darkens over `0xC00`, then the room unfreezes and
    /// fades back in.
    fn begin_menu_close(&mut self, pack: &Pack) {
        if self.menu.is_none() || self.menu_fade_phase != MenuFadePhase::None {
            return;
        }
        self.play_character_bank_cue(pack, sfx::UI_CANCEL, 5);
        self.begin_menu_closing_fade();
    }

    /// Close the pause menu through the same darkening fade without playing
    /// the cancel cue. The reference's `menu_exit_cleanup` is silent; the
    /// callers whose sub-function played its own cue use this: the item-box
    /// browse cancel, the pick-up viewer's resolved yes/no prompt, and the
    /// exit tab ([`MenuEvent::CloseSilent`]), whose confirm already played the
    /// decide cue.
    fn begin_menu_close_silent(&mut self) {
        self.begin_menu_closing_fade();
    }

    /// Arm the menu's closing fade: the panel darkens over `0xC00` and
    /// [`Self::tick_menu_fade`] then closes the menu and arms the room-entry
    /// fade.
    fn begin_menu_closing_fade(&mut self) {
        if self.menu.is_none() || self.menu_fade_phase != MenuFadePhase::None {
            return;
        }
        // Unlike the open, the close arms and steps in consecutive frames: the
        // original runs `menu_exit_cleanup`'s fade and main_loop's addition on
        // the cancel frame and the next, with no pending tick in between. The
        // accumulator is seeded from the room fade's negative leftover, like
        // `set_fading` leaves it.
        self.menu_fade.set(2, 24 << 7, self.room_fade.state());
        self.menu_fade_pending = false;
        self.menu_close_wait = 0;
        self.menu_fade_started = false;
        self.menu_exit_restored = false;
        self.menu_fade_phase = MenuFadePhase::Closing;
    }

    /// Play one character-bank (bank 3) cue through the mixer. The cue is
    /// keyed by its bank slot so a repeated cue restarts that bank's voice,
    /// exactly like the original's `play_sfx(3, slot)`.
    fn play_character_bank_cue(&mut self, pack: &Pack, name: &str, slot: u8) {
        if let Some(wav) = self.sfx_cache.load(pack, name)
            && let Some(mixer) = &mut self.music
        {
            mixer.play_sfx_on_bank(sfx_bank_key(3, slot), wav, 1.0, 0.0);
        }
    }

    /// One tick of the pause-menu fade. The first tick after arming only
    /// clears the pending flag (draw-then-add); when the accumulator runs
    /// negative the phase either starts the fade-in, finishes opening, or
    /// closes the menu and arms the room-entry fade.
    fn tick_menu_fade(&mut self) {
        // The closing fade holds the black screen for two frames before the
        // room resumes: the original's menu task still has to tear down after
        // state 7's wait. This runs before the draw capture so those frames
        // trace as the original's cleared `0:0`.
        if self.menu_fade_phase == MenuFadePhase::Closing && self.menu_close_wait > 0 {
            self.menu_close_wait -= 1;
            if self.menu_close_wait == 0 {
                self.menu_fade = transition::Fade::inactive();
                self.menu_fade_phase = MenuFadePhase::None;
                self.close_menu();
                // Arm the room-entry fade for its first drawn frame: the
                // original's `arm_room_fade` runs on this frame and main_loop
                // draws (then adds) on it, so this frame draws the full 0x7FFF
                // alpha and the following frames step it down.
                self.arm_room_fade();
                // The original suspends game_loop for the room-entry fade, so
                // the player and entities stay frozen until it clears. The
                // first frame after the arm still runs (the reference's
                // `g_short_message_flags | 0x200` still has the player bit),
                // then the fade holds them for the rest of its run.
                self.menu_close_freeze = MENU_CLOSE_FREEZE_FRAMES;
                self.menu_close_freeze_first = true;
                self.menu_close_restore = false;
            }
            return;
        }
        // The original's exit state 7 restores the player to pad control and the
        // idle pose on the first frame after the cancel, so the frozen player is
        // already idle while the panel darkens.
        if self.menu_fade_phase == MenuFadePhase::Closing && !self.menu_exit_restored {
            self.menu_exit_restored = true;
            if let Some(assets) = self.loaded.player_assets.as_ref() {
                self.player
                    .menu_exit_restore(&mut self.game, &assets.emd.clips);
            }
        }
        // Capture the accumulator before this frame's step: the original draws
        // the fade and only then adds the counter.
        self.menu_fade_draw = self.menu_fade.overlay();
        if self.menu_fade_pending {
            self.menu_fade_pending = false;
            return;
        }
        // The opening/closing fades start from the room-entry fade's negative
        // leftover; the original's fade machine resets a negative accumulator
        // to 0 before the first add when the counter is positive.
        if matches!(
            self.menu_fade_phase,
            MenuFadePhase::Out | MenuFadePhase::Closing
        ) && !self.menu_fade_started
        {
            self.menu_fade_started = true;
            if self.menu_fade.state() < 0 && self.menu_fade.counter() >= 1 {
                self.menu_fade
                    .set(self.menu_fade.fade_type(), self.menu_fade.counter(), 0);
            }
        }
        self.menu_fade.tick();
        if self.menu_fade.overlay().is_some() {
            return;
        }
        match self.menu_fade_phase {
            MenuFadePhase::Out => {
                // The fade-out has reached black: arm the menu's fade-in and
                // advance it once, since the original's main_menu and the same
                // frame's counter addition share the tick. This frame still
                // draws the captured fade-out alpha.
                self.menu_fade.set(2, -6144, 0x7FFF);
                self.menu_fade.tick();
                self.menu_fade_phase = MenuFadePhase::In;
            }
            MenuFadePhase::In => {
                self.menu_fade = transition::Fade::inactive();
                self.menu_fade_phase = MenuFadePhase::None;
                // The original's state-0 -> state-1 frame draws no health bar,
                // so hold one frame before the menu starts ticking; its cursor
                // and item-name line wait for the state-1 frame after that.
                self.menu_open_delay = true;
                self.menu_reveal_delay = 2;
            }
            MenuFadePhase::Closing => {
                // The fade-out has reached black. The original clears the fade
                // pair to `0:0` and the menu task tears down over the next two
                // frames before game_loop resumes and arms the room-entry fade.
                self.menu_close_wait = 2;
                self.menu_fade.set(self.menu_fade.fade_type(), 0, 0);
            }
            MenuFadePhase::None => {}
        }
    }

    /// Blend the pause-menu fade over the frame just rendered, at the alpha the
    /// original draws (the pre-step accumulator captured by
    /// [`Self::tick_menu_fade`]).
    fn draw_menu_fade(&mut self) {
        if let Some(overlay) = self.menu_fade_draw.take() {
            self.framebuffer.fade_to_color(overlay.color, overlay.alpha);
        }
    }

    /// Override whether the port-only debug room-select overlay may open on
    /// F1. The overlay is enabled by default; this seam lets a caller force
    /// it off.
    pub fn set_debug_menu(&mut self, enabled: bool) {
        self.debug_menu_enabled = enabled;
    }

    /// The action map the window reads, for the CONTROLS tool to start from.
    fn set_bindings(&mut self, bindings: controls::Bindings) {
        self.bindings = bindings;
    }

    /// The window's presented frame rate, for the F1 DISPLAY tool.
    fn set_frame_rate(&mut self, frame_rate: u32) {
        self.frame_rate = frame_rate;
    }

    /// Take the frame rate the DISPLAY tool picked, once.
    fn take_frame_rate_change(&mut self) -> Option<u32> {
        std::mem::take(&mut self.frame_rate_changed).then_some(self.frame_rate)
    }

    /// Take the bindings the CONTROLS tool changed, once.
    fn take_bindings_change(&mut self) -> Option<controls::Bindings> {
        std::mem::take(&mut self.bindings_changed).then(|| self.bindings.clone())
    }

    /// Open the F9 return-to-title prompt: pause the game sounds and freeze
    /// the room until a second F9 confirms or any other key cancels.
    fn open_return_title(&mut self) {
        if self.return_title.is_none() {
            if let Some(mixer) = &mut self.music {
                mixer.pause_game_sounds();
            }
            self.return_title = Some(ReturnTitlePrompt::new());
        }
    }

    /// One frozen tick of the F9 return-to-title prompt: a second F9 sets the
    /// pending return request and any other key cancels and resumes the
    /// sounds.
    fn tick_return_title(&mut self, ui: UiInput) {
        let event = self
            .return_title
            .as_ref()
            .map_or(ReturnTitleEvent::None, |prompt| prompt.handle_input(ui));
        match event {
            ReturnTitleEvent::None => {}
            ReturnTitleEvent::Confirm => {
                self.return_title = None;
                self.return_title_requested = true;
            }
            ReturnTitleEvent::Cancel => {
                self.return_title = None;
                if let Some(mixer) = &mut self.music {
                    mixer.resume_game_sounds();
                }
            }
        }
    }

    /// A door's destination as the debug menu's pending character: the
    /// other player variant of the room when the pack has it, else `target`
    /// unchanged (the character change then stays pending).
    fn pending_player_target(&self, pack: &Pack, target: RoomId) -> RoomId {
        let Some(choice) = self.game.debug.pending_player else {
            return target;
        };
        let wanted = RoomId {
            player_flag: choice.character & 1,
            ..target
        };
        if wanted == target || pack.contains(&wanted.rdt_entry()) {
            wanted
        } else {
            eprintln!(
                "warning: {} has no variant for the pending character; it stays pending",
                target.room3()
            );
            target
        }
    }

    /// Take the pending return-to-title request, so the app routes to the
    /// title screen exactly once.
    fn take_return_title_request(&mut self) -> bool {
        std::mem::take(&mut self.return_title_requested)
    }

    /// Open the F1 debug menu on its tool list over the frozen room. The
    /// state from the last time it was open is reused, so F1 returns to the
    /// tool and cursors it was left on.
    fn open_debug_menu(&mut self, pack: &Pack) {
        self.ensure_menu_assets(pack);
        let mut menu = match self.debug_menu_memory.take() {
            Some(mut menu) => {
                menu.reopen(self.loaded.id, &self.game, &self.bindings);
                menu
            }
            None => DebugMenu::open(pack, self.loaded.id, &self.game, &self.bindings),
        };
        menu.set_frame_rate(self.frame_rate);
        self.debug_menu = Some(menu);
    }

    /// Take the debug menu's pending LOAD GAME slot.
    fn take_load_slot_request(&mut self) -> Option<usize> {
        self.load_slot_requested.take()
    }

    /// CLEAR GAME's ending flow from the live state, with the character the
    /// films play as: the real clear time and save counter, the
    /// second-playthrough bit and a held special key carry over. The run's
    /// infinite-launcher ownership is not tracked, so it is never set.
    fn ending_flow(&self, id: u8) -> (EndingFlow, u8) {
        let character = self.game.player_choice().character;
        let flow = EndingFlow {
            stats: ending::EndStats::from_state(&self.game, id, character),
            second_playthrough: self.game.flag_test(
                game::BANK_SCENARIO,
                game::SCENARIO_FLAG_SECOND_PLAYTHROUGH,
                false,
            ),
            has_special_key: self.game.has_item(ending::ITEM_SPECIAL_KEY),
            has_infinite_launcher: false,
        };
        (flow, character)
    }

    /// Take the debug menu's pending CLEAR GAME ending id.
    fn take_clear_game_request(&mut self) -> Option<u8> {
        self.clear_game_requested.take()
    }

    /// One frozen tick of the F1 debug menu. In-menu edits apply to the game
    /// state directly; the events that leave the menu close it first.
    fn tick_debug_menu(&mut self, pack: &Pack, ui: UiInput) {
        let event = match self.debug_menu.as_mut() {
            Some(menu) => menu.handle_input(ui, &mut self.game),
            None => DebugMenuEvent::None,
        };
        if event == DebugMenuEvent::BindingsChanged {
            if let Some(menu) = &self.debug_menu {
                self.bindings = menu.controls().bindings().clone();
                self.bindings_changed = true;
            }
            return;
        }
        if let DebugMenuEvent::FrameRateChanged(rate) = event {
            self.frame_rate = rate;
            self.frame_rate_changed = true;
            return;
        }
        if event != DebugMenuEvent::None {
            self.debug_menu_memory = self.debug_menu.take();
        }
        match event {
            DebugMenuEvent::None
            | DebugMenuEvent::Close
            | DebugMenuEvent::BindingsChanged
            | DebugMenuEvent::FrameRateChanged(_) => {}
            DebugMenuEvent::Jump(target) => {
                if let Err(err) = self.debug_jump(pack, target)
                    && self.debug_failed.insert(target.rdt_number())
                {
                    eprintln!(
                        "warning: debug room {} unavailable: {err:#}",
                        target.room3()
                    );
                }
            }
            DebugMenuEvent::SaveGame => self.open_typewriter_save(pack, false),
            DebugMenuEvent::LoadGame => {
                self.render(pack);
                let mut cx = self.ui_context(pack);
                let screen =
                    ui::save_load::open_load_screen(self.game.save_screen.as_deref(), &mut cx);
                let cues = cx.cues.into_inner();
                self.save_screen = Some(screen);
                self.play_ui_cue_names(pack, cues);
            }
            DebugMenuEvent::ItemBox => self.begin_item_box_open(pack),
            DebugMenuEvent::ClearGame(id) => self.clear_game_requested = Some(id),
        }
    }

    /// Jump to `target` through the normal room-load path: load the
    /// destination, rebuild its VMs and run the room boot (init script, room
    /// edits, camera, BGM handoff), then place the player at the target's
    /// paired-door entry ([`room_entry_placement`]). A room with no door
    /// record keeps the walk-zone spawn [`player::spawn`] set. The raw
    /// placement is left in place - the first gameplay tick's collision pass
    /// settles it, exactly like a door arrival.
    fn debug_jump(&mut self, pack: &Pack, target: RoomId) -> Result<()> {
        let from = self.loaded.id;
        let loaded = load_room(pack, target)?;
        self.loaded = loaded;
        self.tween.reset();
        let scripts = Rc::new(self.loaded.scripts.clone());
        self.command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
        self.event_vm = scd::vm::EventVm::from_scripts(scripts);
        // A jump is a door load: snapshot the outgoing enemies, then reset
        // the player's animation id so the next update re-initialises pad
        // control instead of resuming a scripted state from the source room
        // (see `finish_transition`).
        self.game
            .snapshot_enemies(target.room as i8 != self.game.id.room as i8);
        self.game.clear_room_entities();
        self.game.entities[0].set_state(0);
        self.game.entities[0].action_behavior = 0;
        self.game.entities[0].action_state = 0;
        self.game.enter_room(target, &self.loaded.room);
        self.enter_room(pack, Some(from), RoomBgmHandoff::Now);
        self.player = player::spawn(target, &self.loaded.room);
        self.place_room_entry(pack, target);
        self.arm_room_fade();
        Ok(())
    }

    /// Open the examine viewer on `item` inside the open menu. It starts on
    /// its setup state: the first [`GameSession::tick_examine`] runs the
    /// setup and the first intro frame together.
    fn open_item_view(&mut self, pack: &Pack, item: u8) {
        let mut screen = ui::item_view::ItemViewScreen::new(item);
        screen.open_with(pack, &self.text);
        self.examine = Some(screen);
    }

    /// One frame of the examine viewer and the CHECK slide around it
    /// (`menu_item_view_model` and `FUN_0044e1b0`).
    ///
    /// The menu ticks first (its health monitor, and the slide); the frame
    /// the slide-out finishes starts the viewer and runs its first frame. The
    /// viewer's confirm runs the examine check against the game state and
    /// shows its message or the item description in the game's window; the
    /// frame its closing animation finishes the options start sliding back.
    fn tick_examine(&mut self, pack: &Pack, ui: UiInput) {
        let Some(menu) = self.menu.as_mut() else {
            self.examine = None;
            return;
        };
        if let Some(MenuEvent::ViewItem(item)) = menu.tick(&mut self.game) {
            self.open_item_view(pack, item);
        }
        let Self {
            examine,
            menu,
            game,
            text,
            ..
        } = self;
        let (Some(viewer), Some(menu)) = (examine.as_mut(), menu.as_mut()) else {
            return;
        };
        let input = ui::item_view::ExamineInput {
            confirm: ui.confirm,
            cancel: ui.cancel,
            up: ui.up || ui.held_up,
            down: ui.down || ui.held_down,
            left: ui.left || ui.held_left,
            right: ui.right || ui.held_right,
            message_active: game.message.active,
        };
        let slot = ui::layout::slot_of_cursor(menu.cursor);
        match viewer.step_examine(input) {
            ui::item_view::ExamineEvent::None => {}
            ui::item_view::ExamineEvent::Confirm => {
                let context = ui::item_view::ExamineContext {
                    slot_quantity: slot
                        .and_then(|slot| game.inventory.get(slot))
                        .map_or(0, |stack| stack.quantity),
                    item13_message: game.flag_test(game::BANK_SCENARIO, 0x13, false)
                        || game.flag_test(game::BANK_SCENARIO, 0x00, false),
                };
                match viewer.confirm(context) {
                    ui::item_view::ConfirmOutcome::Description => {
                        if let Some(bytes) = viewer.description() {
                            game.show_item_description(viewer.item().wrapping_sub(1), bytes);
                        }
                    }
                    ui::item_view::ConfirmOutcome::Zoom => {}
                    ui::item_view::ConfirmOutcome::Message {
                        id,
                        mark_examined,
                        select,
                    } => {
                        if mark_examined {
                            game.mark_examined(viewer.item());
                        }
                        if let Some(item) = select {
                            game.select_item(Some(item));
                        }
                        game.show_message(id, 0);
                    }
                }
            }
            ui::item_view::ExamineEvent::Message(id) => game.show_message(id, 0),
            ui::item_view::ExamineEvent::MarkExaminedBit(bit) => game.mark_examined_bit(bit),
            ui::item_view::ExamineEvent::Medal(item) => {
                if let Some(stack) = slot.and_then(|slot| game.inventory.get_mut(slot)) {
                    *stack = game::InventoryItem {
                        id: item,
                        quantity: 1,
                    };
                }
                menu.reselect(game);
                viewer.open_with(pack, text);
            }
            ui::item_view::ExamineEvent::Closed => {
                *examine = None;
                menu.view_closed();
            }
        }
    }

    /// Open the pick-up viewer for a pending menu flag.
    ///
    /// The inventory panel (the original's main-menu background) opens under
    /// the viewer. The ground and got-item modes add the model viewer with its
    /// spin-in intro; the document fallback files the entry and shows global
    /// 0xC6 directly.
    fn open_pickup_view(&mut self, pack: &Pack, request: game::ItemViewRequest) {
        if self.pickup_view.is_some() {
            return;
        }
        let Some(action) = self
            .game
            .room_actions
            .get(usize::from(request.slot))
            .copied()
            .flatten()
        else {
            self.game.clear_item_view_flags();
            return;
        };
        let item = action.item_id();
        let kind = match request.kind {
            game::ItemViewKind::Take => PickupKind::Take,
            game::ItemViewKind::GotItem => PickupKind::GotItem,
            game::ItemViewKind::Document => PickupKind::Document,
        };
        self.open_menu(pack);
        // The original sets `g_selectedItemId` to the item being taken as the
        // mode opens, so the prompt's `\i` substitution names it.
        self.game.select_item(Some(item));
        let (screen, reader, phase) = match kind {
            PickupKind::Take | PickupKind::GotItem => {
                let mut screen = if kind == PickupKind::Take {
                    ui::item_view::ItemViewScreen::new_take(item)
                } else {
                    ui::item_view::ItemViewScreen::new_got_item(item)
                };
                screen.open_with(pack, &self.text);
                (Some(screen), None, PickupPhase::Intro)
            }
            PickupKind::Document => {
                // The original's state-8 screen files the entry as it opens
                // (`file_set_collected_flag`) and runs the document reader on
                // it (`menu_update_status_screen`'s key-item scan); global
                // 0xC6 arrives once the reader's exit slide has finished.
                self.game.mark_file_collected(item);
                self.ensure_file_assets(pack);
                let mut reader = ui::file::FileScreen::default();
                reader.open_pickup(&self.game, item);
                (None, Some(reader), PickupPhase::Reader)
            }
        };
        self.pickup_view = Some(PickupView {
            slot: request.slot,
            kind,
            screen,
            reader,
            phase,
        });
    }

    /// One frozen tick of the pick-up viewer.
    ///
    /// The game's message window advances exactly like the menu's (`tick_menu`
    /// hands it the same input), then the viewer's phase machine runs: the
    /// intro steps until it settles and requests its prompt, the document
    /// reader owns the pad until its exit slide finishes and raises 0xC6, the
    /// message resolves (a confirmed 0xC0 already awarded through the message
    /// post-action; the got-item mode awards here), and the exit animation
    /// steps until the mode closes. The yes/no prompt's answer plays the
    /// character bank's confirm/cancel cue as it resolves, exactly like the
    /// original's viewer.
    fn tick_pickup_view(&mut self, pack: &Pack, ui: UiInput, input: player::Input, action: bool) {
        let was_active = self.game.message.active;
        let was_yes_no = self.game.message.phase() == crate::message::MessagePhase::YesNo;
        self.game.update_message(
            MessageInput {
                action,
                left: input.left,
                right: input.right,
                cancel: input.run || ui.cancel,
            },
            &self.loaded.room,
            &self.text,
        );
        // The dismissal bookkeeping mirrors the room tick: a press that
        // closes the prompt is swallowed until the key comes up.
        if was_active && !self.game.message.active && action {
            self.swallow_action = true;
        }
        let Some(phase) = self.pickup_view.as_ref().map(|pickup| pickup.phase) else {
            return;
        };
        match phase {
            PickupPhase::Intro => {
                let settled = match self.pickup_view.as_mut().and_then(|p| p.screen.as_mut()) {
                    Some(screen) => screen.step_intro(),
                    None => true,
                };
                if settled {
                    self.start_pickup_message();
                }
            }
            PickupPhase::Reader => {
                let (exit_done, missing) = {
                    let Self {
                        pickup_view, game, ..
                    } = self;
                    match pickup_view.as_mut().and_then(|p| p.reader.as_mut()) {
                        Some(reader) => {
                            reader.tick();
                            if let Some(event_input) = menu_input(ui) {
                                reader.handle_input(game, event_input);
                            }
                            (reader.take_reader_exit(), false)
                        }
                        None => (false, true),
                    }
                };
                if missing {
                    self.close_pickup_view();
                    return;
                }
                if exit_done {
                    // The reader's slide-out completed: the original fades its
                    // black overlay and raises 0xC6 while the entry is still
                    // pending, then consumes it once the message clears. The
                    // reader layer drops with the slide, revealing the panel
                    // the 0xC6 line belongs to.
                    self.game.show_message(game::MESSAGE_FILE_FILED, 0);
                    if let Some(pickup) = self.pickup_view.as_mut() {
                        pickup.reader = None;
                        pickup.phase = PickupPhase::Message;
                    }
                }
            }
            PickupPhase::Message => {
                if !was_active || self.game.message.active {
                    return;
                }
                let kind = self.pickup_view.as_ref().map(|pickup| pickup.kind);
                match kind {
                    Some(PickupKind::Take) => {
                        // The original plays the character bank's confirm cue
                        // on Yes and cancel cue on No as the prompt resolves.
                        if was_yes_no {
                            let (name, slot) = if self.game.message.menu_choice_id() & 1 == 0 {
                                (sfx::UI_DECIDE, 6)
                            } else {
                                (sfx::UI_CANCEL, 5)
                            };
                            self.play_character_bank_cue(pack, name, slot);
                        }
                    }
                    Some(PickupKind::GotItem) => {
                        // The original's mode 4 awards as its message
                        // completes and the exit animation takes over.
                        self.game.close_got_item_viewer();
                    }
                    Some(PickupKind::Document) => {
                        // The state-8 completion: consume the entry, record
                        // the picked id. The yes/no take path has already run
                        // through the message post-action.
                        if let Some(slot) = self.pickup_view.as_ref().map(|pickup| pickup.slot) {
                            self.game.take_document(slot);
                        }
                    }
                    _ => {}
                }
                if let Some(pickup) = self.pickup_view.as_mut() {
                    if let Some(screen) = pickup.screen.as_mut() {
                        screen.begin_exit();
                    }
                    pickup.phase = PickupPhase::Exit;
                }
            }
            PickupPhase::Exit => {
                let finished = match self.pickup_view.as_mut().and_then(|p| p.screen.as_mut()) {
                    Some(screen) => screen.step_exit(),
                    None => true,
                };
                if finished {
                    self.close_pickup_view();
                }
            }
        }
    }

    /// Request the viewer's prompt once the model intro has settled.
    ///
    /// The ground mode picks 0xC0 when the inventory has room or the pick-up
    /// merges into an existing stack, and 0xC2 when it does not; the got-item
    /// mode always uses 0xC1.
    fn start_pickup_message(&mut self) {
        let Some((kind, slot)) = self
            .pickup_view
            .as_ref()
            .map(|pickup| (pickup.kind, pickup.slot))
        else {
            return;
        };
        let message = match kind {
            PickupKind::Take => {
                if self.game.pickup_fits(slot) {
                    game::MESSAGE_TAKE_PROMPT
                } else {
                    game::MESSAGE_INVENTORY_FULL
                }
            }
            PickupKind::GotItem => game::MESSAGE_GOT_ITEM,
            PickupKind::Document => game::MESSAGE_FILE_FILED,
        };
        self.game.show_message(message, 0);
        if let Some(pickup) = self.pickup_view.as_mut() {
            pickup.phase = PickupPhase::Message;
        }
    }

    /// Close the pick-up viewer: clear the pending menu bits, drop the flow
    /// and darken the inventory panel through the pause menu's closing fade.
    /// The original's menu cleanup fades the whole menu to black, restores the
    /// game state and only then unfreezes; [`Self::tick_menu_fade`] runs that
    /// completion and arms the room-entry fade. No cue plays here: the prompt
    /// already played its own yes/no cue when it resolved.
    fn close_pickup_view(&mut self) {
        self.pickup_view = None;
        self.game.clear_item_view_flags();
        self.begin_menu_close_silent();
    }

    /// Open the save screen over the frozen room after the typewriter prompt
    /// was confirmed. The last gameplay frame is painted first so the screen
    /// fades in over it; the pending block is the state snapshot plus the
    /// bio-card prefix, and `ink_ribbon` makes the write consume one. The
    /// screen is the pack entry its scripts registered (`on_init` has run by
    /// now), else the ported original; a Lua screen that cannot load falls
    /// back to the original.
    fn open_typewriter_save(&mut self, pack: &Pack, ink_ribbon: bool) {
        if self.save_screen.is_some() {
            return;
        }
        self.render(pack);
        let file = self.snapshot_save(pack);
        let mut cx = self.ui_context(pack);
        let screen = ui::save_load::open_save_screen(
            self.game.save_screen.as_deref(),
            &mut cx,
            file,
            ink_ribbon,
        );
        let cues = cx.cues.into_inner();
        self.save_screen = Some(screen);
        self.play_ui_cue_names(pack, cues);
    }

    /// Capture the current game state as a save block with the bio-card
    /// prefix, when the pack carries one.
    fn snapshot_save(&self, pack: &Pack) -> save::SaveFile {
        let Ok(prefix) = pack.read(save::SAVE_PREFIX_ENTRY) else {
            return save::SaveFile::from_state(&self.game);
        };
        match save::SaveFile::from_state_with_prefix(&self.game, &prefix) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("warning: invalid {0}: {err:#}", save::SAVE_PREFIX_ENTRY);
                save::SaveFile::from_state(&self.game)
            }
        }
    }

    /// Run the `--setup-script` authoring chunk over the freshly booted
    /// session, then mirror the authored player state into the live player.
    ///
    /// The chunk runs after the room boot (init script, room edits and the
    /// pack's `on_room_load`) and before the first tick, so tick 0 sees the
    /// authored state. This is the CLI's oracle seam, not a mod-facing hook:
    /// the path comes from `--setup-script`, never from the sandbox.
    fn run_setup_script(&mut self, script: Option<&Path>) -> Result<()> {
        run_setup_chunk(self.lua.as_ref(), &mut self.game, script)?;
        if script.is_some() {
            self.game.sync_player(&mut self.player);
        }
        Ok(())
    }

    /// Write the session's current state to `path` as a save block.
    ///
    /// The `--save-out` authoring seam: the same capture the typewriter save
    /// screen writes, so the file loads through `--load` and the reference's
    /// `--test-load`.
    fn write_save(&self, pack: &Pack, path: &Path) -> Result<()> {
        write_save_file(pack, &self.game, path)
    }

    /// The session's UI context for a screen that runs over the frozen room.
    fn ui_context<'a>(&'a self, pack: &'a Pack) -> UiContext<'a> {
        UiContext {
            pack,
            save_dir: &self.save_dir,
            font: self.font.as_ref(),
            text: Some(&self.text),
            ticks: self.game.frame,
            cues: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// One frozen tick of the typewriter save screen. The screen writes the
    /// block itself; on completion the engine mirrors the write into the live
    /// state (ribbon and save counter) and resumes gameplay.
    fn tick_save_screen(&mut self, pack: &Pack, ui: UiInput) {
        let (result, cues) = {
            let Self {
                save_screen,
                save_dir,
                font,
                text,
                game,
                ..
            } = self;
            let Some(screen) = save_screen.as_mut() else {
                return;
            };
            let cx = UiContext {
                pack,
                save_dir,
                font: font.as_ref(),
                text: Some(text),
                ticks: game.frame,
                cues: std::cell::RefCell::new(Vec::new()),
            };
            let result = screen.update(&cx, ui);
            (result, cx.cues.into_inner())
        };
        self.play_ui_cue_names(pack, cues);
        if result == ScreenResult::Continue {
            return;
        }
        // The debug menu's LOAD GAME: the chosen slot boots a new session,
        // which the app owns.
        if let ScreenResult::Done(ScreenAction::LoadGame { slot }) = result {
            self.save_screen = None;
            self.load_slot_requested = Some(slot);
            return;
        }
        if let Some(outcome) = self
            .save_screen
            .as_mut()
            .and_then(|screen| screen.take_outcome())
        {
            if outcome.ink_ribbon {
                self.game.consume_ink_ribbon();
            }
            self.game.increment_saves();
        }
        self.save_screen = None;
        // The screen does not fade itself: the reference's `LoadSaveGameState`
        // returns to `check_typewriter_state` case 3, which fades the room back
        // in from the black the hand-off left (`counter 0xf000`, the `0x7FFF`
        // accumulator stepped once so the traced state is `0x6FFF` while the
        // drawn frame is the full `0x7FFF`). The player machine gate stays
        // frozen (the reference's case 4 restores `0x45`) until the accumulator
        // runs negative.
        self.room_fade.set(2, -0x1000, 0x6FFF);
        self.room_fade_draw = self.room_fade.drawn_overlay();
        self.typewriter_exit_fade = true;
    }

    /// One frozen tick of the pause menu. A message owns the input while it is
    /// up; the item-box overlay and the FILE tab then own the pad, and
    /// otherwise one pad edge reaches the menu and its event is consumed.
    fn tick_menu(&mut self, pack: &Pack, ui: UiInput, input: player::Input, action: bool) {
        // The pick-up viewer owns the frozen menu while it runs; it drives the
        // game's message window itself and consumes no menu input.
        if self.pickup_view.is_some() {
            self.tick_pickup_view(pack, ui, input, action);
            return;
        }
        let message_input = MessageInput {
            action,
            left: input.left,
            right: input.right,
            cancel: input.run || ui.cancel,
        };
        // The CHECK slide and the examine viewer own the frame, messages
        // included: the viewer waits on its own messages. The original's menu
        // task runs before the message task, so a description the viewer asks
        // for starts revealing this frame and a dismissal reaches the viewer
        // on the next one.
        if self.examine.is_some() || self.menu.as_ref().is_some_and(MainMenu::viewing) {
            self.tick_examine(pack, ui);
            self.game
                .update_message(message_input, &self.loaded.room, &self.text);
            return;
        }
        let message_was_up = self.game.message.active;
        self.game
            .update_message(message_input, &self.loaded.room, &self.text);
        if message_was_up || self.game.message.active {
            // The message owns the input, but the original's menu task still
            // runs `menu_draw_health_bar` under it (the status-screen message
            // wait and the submenu's message states), so the health monitor
            // and its shared `rand()` draws keep advancing.
            let deferred = match &mut self.menu {
                Some(menu) => menu.tick(&mut self.game),
                None => None,
            };
            if let Some(event) = deferred {
                self.apply_menu_event(pack, event);
            }
            return;
        }

        // The item-box overlay is a sub-screen of the inventory panel: it
        // freezes the room with the menu open underneath. It owns its copy of
        // the inventory cursor, so after its input the menu's cursor (which
        // draws the slot highlight and the item name) is mirrored onto the
        // box's cursor to keep the drawn slot and the acted-on slot equal.
        if self.item_box.is_some() {
            // The original's item-box state 3 calls `menu_draw_health_bar`
            // every frame, which keeps the face and the EKG heartbeat sweeping
            // while the box is up.
            if let Some(menu) = self.menu.as_mut() {
                menu.tick(&mut self.game);
            }
            if let Some(item_box) = self.item_box.as_mut() {
                item_box.tick();
            }
            if let Some(event_input) = menu_input(ui) {
                let cue = menu_cue(event_input);
                let event = self
                    .item_box
                    .as_mut()
                    .map(|item_box| item_box.handle_input(&mut self.game, event_input));
                if let Some(cue) = cue {
                    self.play_ui_cue_names(pack, vec![cue]);
                }
                if event == Some(ItemBoxEvent::Close) {
                    // `menu_itembox_interaction` returns 1 for a browse cancel
                    // and for a confirm on a cursor without a slot; the menu
                    // driver turns that into `menu_exit_cleanup`, so the whole
                    // pause menu fades out and the room resumes - the box never
                    // drops back to the inventory panel. The overlay stays drawn
                    // through the closing fade (`close_menu` drops it with the
                    // panel) and the interaction already played the cancel cue,
                    // so the close is silent.
                    self.game.reset_itembox();
                    self.begin_menu_close_silent();
                }
            }
            if let Some(cursor) = self
                .item_box
                .as_ref()
                .map(|item_box| item_box.player_cursor)
                && let Some(menu) = self.menu.as_mut()
            {
                menu.move_cursor_to(&mut self.game, cursor);
            }
            return;
        }

        // The map tab returns to the inventory on cancel.
        if self.map.is_some() {
            // The original's menu task keeps running `menu_update_status_screen`
            // (the health monitor and its shared `rand()` draws) while the map
            // browser is up, exactly like the item box below.
            if let Some(menu) = self.menu.as_mut() {
                menu.tick(&mut self.game);
            }
            if let Some(map) = self.map.as_mut() {
                map.tick();
            }
            let Some(event_input) = menu_input(ui) else {
                return;
            };
            let cue = menu_cue(event_input);
            let event = self
                .map
                .as_mut()
                .map(|map| map.handle_input(&self.game, event_input));
            if let Some(cue) = cue {
                self.play_ui_cue_names(pack, vec![cue]);
            }
            if event == Some(ui::map::MapEvent::Close) {
                self.map = None;
            }
            return;
        }

        // The FILE tab returns to the inventory on close.
        if self.file.is_some() {
            // The original's menu task keeps running `menu_update_status_screen`
            // (the health monitor and its shared `rand()` draws) while the
            // document selector/reader is up.
            if let Some(menu) = self.menu.as_mut() {
                menu.tick(&mut self.game);
            }
            if let Some(file) = self.file.as_mut() {
                file.tick();
            }
            let Some(event_input) = menu_input(ui) else {
                return;
            };
            let cue = menu_cue(event_input);
            let event = self
                .file
                .as_mut()
                .map(|file| file.handle_input(&mut self.game, event_input));
            if let Some(cue) = cue {
                self.play_ui_cue_names(pack, vec![cue]);
            }
            if event == Some(FileEvent::Close) {
                self.file = None;
            }
            return;
        }

        let has_radio = self.game.has_radio();
        // A use/view confirmed on the previous frame runs now, before this
        // frame's health-bar draw, exactly like the original's submenu state 4.
        let deferred = match &mut self.menu {
            Some(menu) => menu.tick(&mut self.game),
            None => return,
        };
        if let Some(event) = deferred {
            self.apply_menu_event(pack, event);
        }
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        // The action submenu ignores all input while its 8-frame open or
        // close animation runs (main_menu's states 1 and 3).
        if menu.mode == MenuMode::Actions && (!menu.submenu_ready() || menu.submenu_closing) {
            return;
        }
        // START (Tab) cycles the top tabs while the menu is open; the file
        // tab opens the document selector.
        if ui.start {
            let cursor = menu.cycle_tab(&mut self.game, has_radio);
            self.handle_tab(pack, cursor);
            return;
        }
        let Some(event_input) = menu_input(ui) else {
            // An idle navigation frame counts the cursor blink down.
            if menu.mode == MenuMode::Navigation {
                menu.blink();
            }
            return;
        };
        let was_navigation = menu.mode == MenuMode::Navigation;
        let event = menu.handle_input(&mut self.game, event_input);
        let mode = menu.mode;
        // A confirm that did nothing (an empty slot) stays silent, exactly like
        // the original's `goto input_blink` path.
        //
        // The navigation cancel is silent here too: its `play_sfx(3, 5)` is the
        // state-1 exit cue [`Self::begin_menu_close`] plays, and the reference
        // reaches that exit without ever running `menu_handle_input`'s
        // navigation path (the cancel bit fails `cond1`), which owns no cue of
        // its own.
        let cue = match event_input {
            MenuInput::Confirm
                if event == MenuEvent::None && was_navigation && mode == MenuMode::Navigation =>
            {
                None
            }
            MenuInput::Cancel if event == MenuEvent::Close => None,
            _ => menu_cue(event_input),
        };
        if let Some(cue) = cue {
            self.play_ui_cue_names(pack, vec![cue]);
        }
        self.apply_menu_event(pack, event);
    }

    /// Apply one completed menu event.
    fn apply_menu_event(&mut self, pack: &Pack, event: MenuEvent) {
        match event {
            MenuEvent::None => {}
            MenuEvent::Close => self.begin_menu_close(pack),
            MenuEvent::CloseSilent => self.begin_menu_close_silent(),
            MenuEvent::Message(id) => self.game.show_message(id as u8, 0),
            // CHECK's slide-out finished: the viewer starts. The slide's own
            // frames run through `tick_examine`, which handles this itself.
            MenuEvent::ViewItem(item) => self.open_item_view(pack, item),
            MenuEvent::Tab(cursor) => self.handle_tab(pack, cursor),
            MenuEvent::Changed => {}
        }
    }

    /// Run the action of a selected top tab.
    ///
    /// The map tab (cursor 0) always opens the floor-plan browser: the
    /// original's `menu_tab_map` runs its state machine without any scenario
    /// check (`DAT_00ae9f1f` gates only the radio tab). The file tab (cursor 2)
    /// opens the document selector/reader. The radio tab (cursor 4) uses the
    /// carried radio when the room's per-frame item-use latch (bank 9 selector
    /// `0x3F`, the original's `g_itemUseFlags` radio-transmission bit) is up:
    /// the used-item byte 0x4D reaches the room scripts and the menu closes
    /// through the normal exit fade. Without the latch it opens the radio's
    /// model viewer (the original's `FUN_0044e1b0` item view) like a CHECK.
    fn handle_tab(&mut self, pack: &Pack, cursor: u8) {
        match cursor {
            0 => self.open_map(pack),
            2 => self.open_file(pack),
            4 => {
                if !self.game.has_radio() {
                    return;
                }
                let transmitting = self.game.flags[usize::from(game::BANK_ITEM_USE)].bit(0x3F);
                if transmitting {
                    self.game.record_used_item(ITEM_COMM_RADIO);
                    // `menu_tab_radio`'s transmitting branch plays the
                    // character bank's radio cue (slot 7) before the state-1
                    // exit's cancel cue.
                    self.play_character_bank_cue(pack, sfx::UI_RADIO, 7);
                    self.begin_menu_close(pack);
                } else {
                    self.open_item_view(pack, ITEM_COMM_RADIO);
                }
            }
            _ => self.close_menu(),
        }
    }

    /// Load the item-box art once; a missing sheet is logged and leaves the
    /// overlay frame blank.
    fn ensure_item_box_assets(&mut self, pack: &Pack) {
        if self.item_box_assets.is_some() {
            return;
        }
        match ItemBoxAssets::load(pack) {
            Ok(assets) => self.item_box_assets = Some(assets),
            Err(err) => eprintln!("warning: item-box art unavailable: {err:#}"),
        }
    }

    /// Load the FILE tab art once; a missing TIM is logged and leaves that
    /// layer blank.
    fn ensure_file_assets(&mut self, pack: &Pack) {
        if self.file_assets.is_some() {
            return;
        }
        match FileAssets::load(pack) {
            Ok(assets) => self.file_assets = Some(assets),
            Err(err) => eprintln!("warning: FILE tab art unavailable: {err:#}"),
        }
    }

    /// Open the item box over the frozen inventory panel.
    fn open_item_box(&mut self, pack: &Pack) {
        if self.item_box.is_some() {
            return;
        }
        self.ensure_item_box_assets(pack);
        self.open_menu(pack);
        // The box shares the inventory panel's cursor: it opens on whatever
        // slot the menu is showing, and `tick_menu` keeps the two in step.
        let cursor = self
            .menu
            .as_ref()
            .map_or(ui::layout::FIRST_SLOT_CURSOR, |menu| menu.cursor);
        let mut item_box = ItemBox::default();
        item_box.open(&self.game, cursor);
        self.item_box = Some(item_box);
    }

    /// Open the FILE tab selector over the frozen inventory panel.
    fn open_file(&mut self, pack: &Pack) {
        if self.file.is_some() {
            return;
        }
        self.ensure_file_assets(pack);
        self.open_menu(pack);
        let mut file = FileScreen::default();
        file.open(&self.game);
        self.file = Some(file);
    }

    /// Open the map tab over the frozen inventory panel. The table/art load
    /// warns and leaves the tab blank when the pack lacks the map entries.
    fn open_map(&mut self, pack: &Pack) {
        if self.map.is_some() {
            return;
        }
        self.open_menu(pack);
        self.map = Some(ui::map::MapScreen::open(pack, &self.game));
    }

    /// Advance an active transition one `main_loop` frame and run the parts
    /// of `room_transition_load` the frame reached: the placement, the
    /// destination's `room_set` and, on the frame it returns, its tail. The
    /// finished flag is held until [`GameSession::finish_transition`] drops
    /// the timeline after the frame was presented.
    fn tick_transition(&mut self, pack: &Pack, skip: bool) {
        self.tween.advance();
        let Some(mut session) = self.transition.take() else {
            return;
        };
        let frame = session.transition.tick(skip);
        session.frame = frame;
        session.door_yield = None;
        for effect in session.transition.stepper_mut().take_sfx() {
            play_door_animation_sfx(
                &mut self.music,
                &mut self.sfx_cache,
                pack,
                &session.door,
                effect,
            );
        }
        for message in session.transition.stepper_mut().take_messages() {
            apply_door_message(&mut self.game, message);
        }
        // The software mixer queues synchronously, so the VM's SFX op never
        // has to wait.
        session.transition.set_sound_busy(false);
        self.transition_finished = frame.finished;
        if frame.place {
            place_transition_player(&mut session, &mut self.game, &mut self.loaded);
        }
        self.transition = Some(session);
        if frame.load {
            self.enter_transition_destination(pack);
        }
        if frame.finished {
            self.end_transition(pack);
        }
        // The original's `main_loop` ramps the cutscene screen intensity once
        // per platform frame regardless of the door task, and only skips
        // *drawing* the bars while the transition owns the screen. The room's
        // gameplay tick is paused here, so the ramp must run on the animation
        // frame or the bars would still be fading when the destination's
        // first traced tick is drawn.
        self.tick_screen_intensity();
    }

    /// Drop the finished transition's timeline once its last frame was
    /// presented; [`GameSession::tick_transition`] already ran the tail.
    fn finish_transition(&mut self, _pack: &Pack) {
        self.transition = None;
        self.transition_finished = false;
        // A door back into the same room and cut must not tween from the
        // pre-door poses.
        self.tween.reset();
    }

    /// The tail of `room_transition_load` and `game_loop`'s return into
    /// gameplay, on the frame the animation task's exit is noticed: a
    /// camera-only record re-tests the switch zones, the destination's BGM
    /// handoff runs, the close SE plays and a full load arms the destination's
    /// entry fade.
    fn end_transition(&mut self, pack: &Pack) {
        let Some(mut session) = self.transition.take() else {
            return;
        };
        if session.camera_only {
            finish_transition(
                &mut session,
                &mut self.game,
                &mut self.player,
                &mut self.loaded,
            );
        }
        // `room_transition_load` waits the door-animation task out before it
        // calls `update_room_bgm` (0x00481583), so the outgoing track keeps
        // playing under the whole animation. A full load already booted the
        // destination in `enter_transition_destination`, which is why its init
        // script's `room_bgm_state_set` is visible here; a camera-only record
        // never left the source room and re-runs the same room's table.
        let from = session.source;
        bgm::update_room_bgm(&mut self.game, self.loaded.id, Some(from));
        bgm::apply_live(&mut self.music, &mut self.game, &mut self.bgm_cache, pack);
        play_room_close_sfx(&mut self.music, &mut self.sfx_cache, pack, &session);
        let entered = session.entered;
        self.transition = Some(session);
        if entered {
            self.arm_room_entry_fade();
        }
    }

    /// Run a full-load transition's destination `room_set` while the `.dor`
    /// timeline keeps playing.
    ///
    /// The original's `room_transition_load` latches the door record, spawns
    /// the animation task, places the player and then loads the destination
    /// (`room_set` plus one forced-open `room_events_check`) before waiting
    /// the animation out, so the destination's init script and its flag
    /// effects run while the animation still owns the screen; no gameplay tick
    /// runs until the task finishes. The camera-only record (bit `0x80`) has
    /// no destination and keeps the room frozen until the timeline ends,
    /// exactly like the original's camera-only branch.
    fn enter_transition_destination(&mut self, pack: &Pack) {
        let Some(mut session) = self.transition.take() else {
            return;
        };
        if session.camera_only || session.entered {
            self.transition = Some(session);
            return;
        }
        let from = session.source;
        finish_transition(
            &mut session,
            &mut self.game,
            &mut self.player,
            &mut self.loaded,
        );
        self.transition = Some(session);
        let scripts = Rc::new(self.loaded.scripts.clone());
        self.command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
        self.event_vm = scd::vm::EventVm::from_scripts(scripts);
        self.enter_room(pack, Some(from), RoomBgmHandoff::AtTransitionEnd);
        // `room_set` runs `room_events_check` once with the message gate
        // forced open, so an event the init script queued executes exactly one
        // opcode during the load (RoomInit.cpp 0x00477c45), the same pass the
        // new-game boot models.
        {
            let mut host = game::ScdGameHost::new(&mut self.game);
            self.event_vm.step(&mut host);
        }
    }

    /// Arm the room-entry fade exactly like `game_loop`'s outer loop plus the
    /// first `main_loop` step: the accumulator starts at `0x7FFF`, its first
    /// draw is the full black veil and the counter's first step is already
    /// applied, and the room-events gate stays closed until the accumulator
    /// drops below 1.
    fn arm_room_entry_fade(&mut self) {
        self.arm_room_fade();
        self.room_boot_gate = true;
        self.room_boot_grace = true;
    }

    /// Arm the room-entry fade the original runs whenever gameplay (re)enters
    /// after a room load: type 2 (black), accumulator `0x7FFF` and the normal
    /// counter `0xE800` (i16 -6144, alpha 255 down to 15 in six fixed ticks),
    /// or the slow `0xFF5D` (i16 -163) while scenario flag `0x7D` is set.
    ///
    /// The armed `0x7FFF` veil is captured for this frame's draw and the
    /// accumulator takes its draw-then-add step now, exactly as `main_loop`
    /// leaves it: the frame paints fully black and the following frames step
    /// 207, 159, .... The capture is load-bearing because the app loop draws
    /// frames that advance no gameplay tick; a frame that drew the stale
    /// capture (`None` right after a room load) showed the new room unfaded
    /// for a frame before the veil snapped back - the entry flash.
    fn arm_room_fade(&mut self) {
        let slow = self.game.flag_test(
            game::BANK_SCENARIO,
            game::SCENARIO_FLAG_MENU_FADE_LATCH,
            false,
        );
        self.room_fade
            .set(2, if slow { -163 } else { -6144 }, 0x7FFF);
        self.room_fade_draw = self.room_fade.overlay();
        self.room_fade.tick();
    }

    /// The new-game boot sequence the oracle trace compares against: one
    /// room-events pass for the original's `room_set` call and the entry fade
    /// armed with its first accumulator step already applied, exactly as
    /// `game_loop`'s outer loop and the first `main_loop` leave it before the
    /// first gameplay tick. The room-events gate stays closed until the fade
    /// accumulator drops below 1 (the frame `g_message_flags` bit 0x80 is
    /// restored), so the event the init script queued does not step until
    /// then. Only the trace path arms this; the interactive narration has its
    /// own sequence.
    fn arm_new_game_boot(&mut self) {
        // `room_set` runs `room_events_check` once with the message gate
        // forced open, so an event the init script queued executes exactly one
        // opcode during the room load (RoomInit.cpp 0x00477c45).
        {
            let mut host = game::ScdGameHost::new(&mut self.game);
            self.event_vm.step(&mut host);
        }
        // An event the room's init script queued can select the screen-border
        // background geometry, and the reference reads the flag after this
        // pass (room_set runs room_events_check, then loads the background).
        self.apply_screen_border();
        // The reference's room hook re-places the player after `init_room` and
        // that event pass (`TestHooks_PlaceRoomSpawn` runs after `init_room`),
        // so a queued intro event's own `dir_set` does not survive the boot.
        // Mirror that order for arbitrary-room boots.
        self.place_room_boot_spawn();
        // `game_loop`'s outer loop arms the entry fade and `main_loop` steps
        // it once before the first inner-loop tick, so tick 0's in-tick
        // accumulator is `0x7FFF - 0x1800` (26623).
        self.arm_room_entry_fade();
    }

    /// Step the room-entry fade once per fixed tick. Every arming path captures
    /// the armed veil for its own frame's draw and applies that frame's
    /// draw-then-add step itself, so this only has to capture the pre-step
    /// overlay of the frame about to be drawn and advance. Once the accumulator
    /// goes negative the fade is over but keeps its last counter and state: the
    /// original's gameplay branch only adds while the accumulator is
    /// non-negative and never zeroes the pair outside the `MSF_FADE_ACTIVE`
    /// machine (the death and character-select fades), so the trace keeps
    /// reporting them.
    fn tick_room_fade(&mut self) {
        // The original draws the fade with the accumulator at the start of the
        // frame and only then adds the counter, so capture the pre-step overlay
        // for this frame's draw before advancing.
        let pre = self.room_fade.overlay();
        self.room_fade_draw = pre;
        if pre.is_none() {
            return;
        }
        self.room_fade.tick();
    }

    /// Blend the room-entry fade over the frame just rendered, at the alpha the
    /// original draws (the pre-step accumulator value).
    fn draw_room_fade(&mut self) {
        if let Some(overlay) = self.tween.fade(self.room_fade_draw) {
            self.framebuffer.fade_to_black(overlay.alpha);
        }
    }

    /// Blend the death fade (the white-out or the screen's final fade) over
    /// the frame just rendered.
    fn draw_death_fade(&mut self) {
        if let Some(overlay) = self.death.drawn_overlay() {
            self.framebuffer.fade_to_color(overlay.color, overlay.alpha);
        }
    }

    /// Load and decode the `ui/died` page once, when the DIED screen first
    /// opens. The screen samples fixed quadrants of the page, so a replacement
    /// keeps the shipped page's dimensions.
    fn ensure_died_texture(&mut self, pack: &Pack) {
        if self.died_texture_tried {
            return;
        }
        self.died_texture_tried = true;
        match pack.read_asset("ui/died", AssetClass::Image) {
            Ok(Some(asset)) => match assets::decode_page(&asset, assets::Geometry::Shipped) {
                Ok(art) => self.died_texture = Some(art),
                Err(err) => eprintln!("warning: invalid {}: {err:#}", asset.path()),
            },
            Ok(None) => eprintln!("warning: missing ui/died.tim"),
            Err(err) => eprintln!("warning: unreadable ui/died: {err:#}"),
        }
    }

    /// Draw the DIED screen: the static backdrop, the corpse posed at the
    /// fall's terminal frame through the fixed orbit camera, the black
    /// overlay, the wavy strip and the screen's own fade.
    fn render_death_screen(&mut self, pack: &Pack) {
        self.ensure_died_texture(pack);
        let Some(screen) = self.death.screen.as_ref() else {
            return;
        };
        let Some(texture) = self.died_texture.as_ref() else {
            self.framebuffer.clear();
            ui::death::draw_overlay(&mut self.framebuffer, screen.overlay());
            return;
        };
        self.framebuffer.clear();
        ui::death::draw_backdrop(&mut self.framebuffer, texture);
        if let Some(assets) = self.loaded.player_assets.as_ref()
            && let Some(keyframe) = self
                .player
                .anim
                .pose_keyframe(&assets.emd.clips, &assets.emd.keyframes)
        {
            let entity = crate::anim::entity_matrix(self.player.pos, self.player.angle);
            let mut joints = crate::anim::joint_matrices(&assets.emd.skeleton, &keyframe, &entity);
            apply_look_at(
                &mut joints,
                &self.game.entities[0],
                &self.game.entity_anims[0],
            );
            let camera = Camera::from_points(screen.camera_from, screen.camera_to, screen.fov);
            let lighting = Lighting::from_room(&self.loaded.room);
            let hidden = self.game.entities[0].joint_flags;
            self.framebuffer.draw_model_hidden(
                &assets.emd.mesh,
                &assets.emd.texture,
                &joints,
                &camera,
                &lighting,
                hidden,
            );
        }
        ui::death::draw_rect(&mut self.framebuffer, screen.rect_alpha());
        if let Some(param) = screen.strip_param() {
            ui::death::draw_strip(&mut self.framebuffer, texture, param);
        }
        ui::death::draw_overlay(&mut self.framebuffer, screen.overlay());
    }

    /// Request the loading narration `id` (a global message) and hold the room
    /// frozen until it clears. The original shows this black-screen typewriter
    /// message between the intro film and the first room frame, then arms the
    /// room-entry fade; the encoded action byte in the message stream is what
    /// dismisses it.
    fn request_boot_message(&mut self, id: u8) {
        self.game.show_message(id, 0);
        self.boot_message = Some(id);
    }

    /// One tick of the loading narration: advance the message window and, the
    /// tick its encoded delay clears it, arm the room-entry fade. No room
    /// script, entity, effect or player tick runs while the narration is up.
    fn tick_boot_message(&mut self, input: player::Input, action: bool) {
        self.game.update_message(
            MessageInput {
                action,
                left: input.left,
                right: input.right,
                cancel: input.run,
            },
            &self.loaded.room,
            &self.text,
        );
        if !self.game.message.active {
            self.boot_message = None;
            self.arm_room_fade();
        }
    }

    /// Step the cutscene screen-intensity value once per fixed tick. The bank-5
    /// selector `MSF_SCREEN_INTENSITY` (the original's "screen intensity
    /// ramping up" flag) drives its low byte: while the bit is set the value
    /// climbs 16 a frame to `0xF0`, and while it is clear it falls 16 a frame
    /// back to zero.
    fn tick_screen_intensity(&mut self) {
        let ramping = self.game.flags[5].bit(game::MSF_SCREEN_INTENSITY);
        let byte = self.sprite_anim_intensity as u8;
        if ramping {
            if byte < 0xF0 {
                self.sprite_anim_intensity = self.sprite_anim_intensity.wrapping_add(16);
            }
        } else if byte > 0x0F {
            self.sprite_anim_intensity = self.sprite_anim_intensity.wrapping_sub(16);
        }
    }

    /// Blend the cutscene letterbox bars over the freshly drawn scene.
    ///
    /// The original's two bar descriptors carry the `0x60000000` texture id,
    /// which `GetTextureVariant` reads as variant 3 ("fade to black"), so
    /// `draw_rect` forces the colour to black and keeps the level as the
    /// source alpha. At the `0xF0` ceiling `main_loop` first sets the
    /// descriptor's colour to white, and `draw_rect` derives the source alpha
    /// from the last non-zero colour component (`0xFF`), so the bars go fully
    /// opaque while the variant-3 branch still forces them black
    /// (`MainLoop.cpp` 0x004297e0, `Rendering.cpp` 0x00470350/0x00470580).
    /// The rects are the descriptors' own spans, `(-164,-130,328,38)` and
    /// `(-164,92,328,38)` in the 320x240 origin-centred space.
    fn draw_screen_intensity_bars(&mut self) {
        let byte = self.sprite_anim_intensity as u8;
        if byte == 0 {
            return;
        }
        let alpha = if byte == 0xF0 { 255 } else { byte };
        self.framebuffer
            .blend_black_rect_weighted([-4, -10, 328, 38], alpha);
        self.framebuffer
            .blend_black_rect_weighted([-4, 212, 328, 38], alpha);
    }

    /// The room-entry fade's current alpha, for the deterministic tests.
    #[cfg(test)]
    fn room_fade_alpha(&self) -> Option<u8> {
        self.room_fade.overlay().map(|overlay| overlay.alpha)
    }

    /// The alpha the room-entry fade blends over this frame, for the
    /// deterministic tests: the draw-then-add capture `tick_room_fade` takes
    /// (or the armed veil a frame drawn before the first gameplay tick sees).
    #[cfg(test)]
    fn room_fade_draw_alpha(&self) -> Option<u8> {
        self.room_fade_draw.map(|overlay| overlay.alpha)
    }

    /// Whether the pause-menu fade is currently hiding the menu panel (the
    /// opening fade-out and the whole closing fade). The original does not draw
    /// the inventory in these phases: the room is suspended (open) or the
    /// menu's exit state 7 skipped the draw (close).
    fn menu_fade_hides(&self) -> bool {
        matches!(
            self.menu_fade_phase,
            MenuFadePhase::Out | MenuFadePhase::Closing
        )
    }

    /// The pause-menu fade's current alpha, for the deterministic tests.
    #[cfg(test)]
    fn menu_fade_alpha(&self) -> Option<u8> {
        self.menu_fade.overlay().map(|overlay| overlay.alpha)
    }

    /// The per-tick oracle-trace snapshot (`oracle/trace-v1.md`): the live
    /// room, game and room-entry fade state plus the rendered frame's hash.
    ///
    /// The reference's `g_fading_counter`/`g_fading_state` is one global. The
    /// death machine takes it over first; while a pause-menu fade is running
    /// (open, fade-in or close) that menu fade owns it; otherwise it is the
    /// room-entry fade.
    fn trace_tick<'a>(&'a self, frame: &Framebuffer) -> trace::TraceTick<'a> {
        // A running door owns the shared `g_fading_*` pair until the frame
        // its `room_transition_load` returns and arms the room-entry fade.
        let door_fade = self
            .transition
            .as_ref()
            .filter(|_| !self.transition_finished)
            .map(|session| session.transition.fade());
        let (fade_counter, fade_state) = if let Some(fade) = door_fade {
            (fade.counter(), fade.state())
        } else if !self.death.is_idle() {
            (self.death.fade_counter(), self.death.fade_state())
        } else if self.menu_fade_phase != MenuFadePhase::None {
            (self.menu_fade.counter(), self.menu_fade.state())
        } else {
            (self.room_fade.counter(), self.room_fade.state())
        };
        // The frame a door fires is seen at the original's mid-pass yield.
        let door_yield = self
            .transition
            .as_ref()
            .and_then(|session| session.door_yield.as_ref());
        let (rng, rseed, entities) = match door_yield {
            Some(view) => (view.rand_state, view.rand_seed, &view.entities[..]),
            None => (
                self.game.rand_state,
                self.game.rand_seed,
                &self.game.entities[..],
            ),
        };
        trace::TraceTick {
            room: self.loaded.id,
            rng,
            rseed,
            fade_counter: fade_counter as i16,
            fade_state,
            cam: self.loaded.room.current_cut as u8,
            player_flags: self.game.player_flags,
            entities,
            frame_hash: trace::frame_hash(frame),
        }
    }

    /// Render the current frame into the session framebuffer: a transition
    /// frame while one runs, the gameplay scene otherwise, with the pause
    /// menu and then the message window drawn on top. The message is painted
    /// after the scene (and outside the transition path) so it is never
    /// covered by the menu and never dimmed by a fade or door overlay.
    fn render(&mut self, pack: &Pack) {
        self.render_pass(pack, ScenePass::Software);
    }

    /// Render the frame for the GPU backend: the scene is recorded on `gpu`
    /// and every software overlay is drawn twice, once over a black scene
    /// (`framebuffer`) and once over a white one (`framebuffer_white`).
    ///
    /// Each overlay operation (sprites, text, fades, blends) is affine in the
    /// pixel underneath, so the presented pixel is `black + scene * (white -
    /// black)`: opaque overlay pixels hide the scene, fades and blends scale
    /// it. Returns whether the frame shows the GPU scene; when it does not,
    /// `framebuffer` is the whole frame. See [`GameSession::white_frame`].
    fn render_layers(&mut self, pack: &Pack, gpu: &mut GpuRenderer) {
        // The pause-menu fade is consumed as it draws; the white pass draws
        // the same one.
        let menu_fade = self.menu_fade_draw;
        self.render_pass(pack, ScenePass::Record(gpu));
        if !self.gpu_scene_visible {
            return;
        }
        self.menu_fade_draw = menu_fade;
        let mut white = self.framebuffer_white.take().unwrap_or_default();
        std::mem::swap(&mut self.framebuffer, &mut white);
        self.render_pass(pack, ScenePass::White);
        std::mem::swap(&mut self.framebuffer, &mut white);
        self.framebuffer_white = Some(white);
    }

    /// The frame drawn over a white scene, when the last
    /// [`GameSession::render_layers`] showed the GPU scene.
    fn white_frame(&self) -> Option<&Framebuffer> {
        self.framebuffer_white
            .as_ref()
            .filter(|_| self.gpu_scene_visible)
    }

    fn render_pass(&mut self, pack: &Pack, pass: ScenePass<'_>) {
        // The screens below replace the whole frame, scene included.
        if self.boot_message.is_some()
            || self.movie.is_some()
            || self.transition.is_some()
            || self.death.screen_open()
        {
            self.gpu_scene_visible = false;
        }
        // The loading narration shows only its typewriter message over black;
        // no world frame is drawn under it.
        if self.boot_message.is_some() {
            self.framebuffer.clear();
            self.framebuffer.fill_rect(
                [
                    0,
                    0,
                    self.framebuffer.width as i32,
                    self.framebuffer.height as i32,
                ],
                [0, 0, 0, 255],
            );
            if let Some(font) = &self.font {
                self.game
                    .message
                    .draw(&mut self.framebuffer, font, &self.text);
            }
            return;
        }
        if let Some(movie) = &self.movie {
            let rgba = movie.frame_rgba();
            self.framebuffer.rgba.copy_from_slice(rgba);
            return;
        }
        if let Some(transition) = &self.transition {
            render_transition(&mut self.framebuffer, transition);
            return;
        }
        // The DIED screen owns the frame while it is up: the static backdrop,
        // the spinning corpse, the wavy strip and the screen's own fades.
        if self.death.screen_open() {
            self.render_death_screen(pack);
            return;
        }
        // While the menu is up the room is frozen: keep the frame it was
        // opened over instead of re-rendering the scene, then draw the menu
        // and the window over it.
        if self.menu.is_none() {
            match pass {
                // The scene was recorded by the black pass; this pass only
                // redraws what lies over it.
                ScenePass::White => {
                    self.framebuffer
                        .fill_rect([0, 0, WIDTH, HEIGHT], [255, 255, 255, 255]);
                    draw_special_light(
                        &mut self.framebuffer,
                        &self.game,
                        self.loaded.id,
                        self.loaded.room.current_cut,
                    );
                }
                pass => {
                    let gpu = if let ScenePass::Record(gpu) = pass {
                        // A new room retires the previous room's uploads.
                        if self.gpu_room.replace(self.loaded.id) != Some(self.loaded.id) {
                            gpu.retire();
                        }
                        Some(gpu)
                    } else {
                        None
                    };
                    self.gpu_scene_visible = render_frame_on(
                        &mut self.framebuffer,
                        gpu,
                        pack,
                        self.loaded.id,
                        &self.loaded.room,
                        &self.player,
                        &mut self.game,
                        self.loaded.player_assets.as_ref(),
                        &mut self.npc_models,
                        &mut self.masks,
                        &mut self.shadows,
                        &mut self.effect_pages,
                        &mut self.tween,
                    );
                }
            }
            // The cutscene letterbox bars sit on the scene after the
            // room-entry fade has darkened it: `main_loop` draws the fading
            // rect first (0x0042... at the fade block) and then the two
            // letterbox bars, so an overlapping fade and bar ramp composite
            // bars-over-fade. The death fade overlays the falling room last,
            // and the menu/message overlays stay above all of it.
            self.draw_room_fade();
            self.draw_screen_intensity_bars();
            self.draw_death_fade();
        } else if self.menu_fade_hides()
            && !(self.menu_fade_phase == MenuFadePhase::Out && self.menu_fade_pending)
        {
            // The original suspends game_loop while the menu opens or closes,
            // so the room is not redrawn: the screen is the menu's black
            // background with the fade over it, not the frozen room darkening
            // (open) or the menu panel darkening (close; the original's exit
            // state 7 does not draw the inventory). Clear to black so the
            // frames match the reference. The opening request frame still shows
            // the frozen room, so only the Out-pending frame skips the clear;
            // the closing request frame is already black in the reference.
            self.framebuffer.clear();
            self.gpu_scene_visible = false;
        }
        // The computer terminal draws over the frozen monitor-camera scene.
        if self.computer.is_some() {
            if let Some(screen) = &self.computer {
                screen.draw(&mut self.framebuffer, self.font.as_ref(), &self.text);
            }
            return;
        }
        // The menu is hidden while the opening fade-out darkens the frozen
        // frame and while the closing fade runs; it is revealed for the
        // fade-in.
        if self.menu.is_some() && !self.menu_fade_hides() {
            self.ensure_menu_assets(pack);
            // The cursor and the 0xBA item-name line are drawn from
            // menu_handle_input (state 1), which only runs once the opening
            // fade has finished *and* the state-0 transition frame has passed;
            // while either is still up neither is drawn. The name line also
            // belongs to whatever owns the screen - a message window, the
            // examine viewer or a pick-up prompt draws there - so the menu must
            // not paint its own name under it.
            let menu_ready =
                self.menu_fade_phase == MenuFadePhase::None && self.menu_reveal_delay == 0;
            let show_item_name = menu_ready
                && !self.game.message.active
                && self.pickup_view.is_none()
                && self.examine.is_none();
            // While the item box is open its interaction draws the shared slot
            // cursor itself (`itembox_draw_cursor`), with the box's own blink
            // counter; the menu only keeps a cursor on the tab row, which the
            // box's cursor movement cannot reach but a menu-opened box can
            // inherit.
            let box_owns_cursor = self
                .item_box
                .as_ref()
                .is_some_and(|item_box| !item_box.cursor_on_tab());
            if let (Some(menu), Some(assets)) = (&self.menu, &self.menu_assets) {
                menu.draw_with_name(
                    &mut self.framebuffer,
                    assets,
                    &self.text,
                    &self.game,
                    menu_ready && !box_owns_cursor,
                    show_item_name,
                );
            }
        }
        // The examine viewer draws over the live menu: the model, its
        // rotation arrows and the item name.
        if let Some(viewer) = &self.examine
            && !self.menu_fade_hides()
        {
            viewer.draw_examine(
                &mut self.framebuffer,
                self.menu_assets.as_ref().map(|assets| &assets.status),
                self.font.as_ref(),
                &self.text,
                &self.game.examined_flags(),
            );
        }
        if self.item_box.is_some() && !self.menu_fade_hides() {
            self.ensure_item_box_assets(pack);
            // The box's cursor comes from the interaction itself, so it is
            // withheld until the menu's first input frame like the menu's own.
            let reveal_cursor =
                self.menu_fade_phase == MenuFadePhase::None && self.menu_reveal_delay == 0;
            if let (Some(item_box), Some(item_box_assets), Some(menu_assets)) =
                (&self.item_box, &self.item_box_assets, &self.menu_assets)
            {
                item_box.draw(
                    &mut self.framebuffer,
                    item_box_assets,
                    menu_assets,
                    &self.text,
                    &self.game,
                );
                // The box interaction draws the cursor after its own overlay
                // (`itembox_draw_cursor`, pending depth 5 -> nearer than the
                // frame pieces but behind the masks).
                if reveal_cursor {
                    item_box.draw_cursor(&mut self.framebuffer, menu_assets);
                }
            }
        }
        if self.file.is_some() {
            self.ensure_file_assets(pack);
            if let (Some(file), Some(file_assets), Some(menu_assets)) =
                (&self.file, &self.file_assets, &self.menu_assets)
            {
                file.draw(
                    &mut self.framebuffer,
                    file_assets,
                    menu_assets,
                    &self.text,
                    &self.game,
                );
            }
        }
        if let Some(map) = self.map.as_mut() {
            map.draw(&mut self.framebuffer, pack, &self.game);
        }
        if let Some(map) = self.map.as_ref() {
            // The original's depth puts the status screen's frame decorations
            // in front of the map tab's sprites, so the panel's dithered
            // right-hand edge shows over the composite (see
            // `MainMenu::draw_frames_over`).
            let rect = map.content_rect();
            if let (Some(menu), Some(assets)) = (&self.menu, &self.menu_assets) {
                menu.draw_frames_over(&mut self.framebuffer, assets, rect);
            }
        }
        if self.save_screen.is_some() {
            let Self {
                save_screen,
                save_dir,
                font,
                text,
                game,
                framebuffer,
                ..
            } = self;
            if let Some(screen) = save_screen {
                let cx = UiContext {
                    pack,
                    save_dir,
                    font: font.as_ref(),
                    text: Some(text),
                    ticks: game.frame,
                    cues: std::cell::RefCell::new(Vec::new()),
                };
                screen.draw(&cx, framebuffer);
                framebuffer.fade_overlay(screen.overlay());
            }
        }
        // The pick-up viewer draws over the inventory panel, before the
        // message so its intro/exit fade never dims the prompt text. A
        // document pick-up draws the FILE reader instead of the model viewer.
        if self.pickup_view.is_some() {
            self.ensure_file_assets(pack);
            let Self {
                pickup_view,
                file_assets,
                menu_assets,
                save_dir,
                font,
                text,
                game,
                framebuffer,
                ..
            } = self;
            if let Some(pickup) = pickup_view.as_mut() {
                if let Some(screen) = pickup.screen.as_mut() {
                    let cx = UiContext {
                        pack,
                        save_dir,
                        font: font.as_ref(),
                        text: Some(text),
                        ticks: game.frame,
                        cues: std::cell::RefCell::new(Vec::new()),
                    };
                    screen.draw(&cx, framebuffer);
                    framebuffer.fade_overlay(screen.overlay());
                }
                if let Some(reader) = pickup.reader.as_mut()
                    && let (Some(file_assets), Some(menu_assets)) =
                        (file_assets.as_ref(), menu_assets.as_ref())
                {
                    reader.draw(framebuffer, file_assets, menu_assets, text, game);
                }
            }
        }
        // The pause-menu open/close fade sits over the menu and its overlays.
        self.draw_menu_fade();
        if let Some(font) = &self.font {
            self.game
                .message
                .draw(&mut self.framebuffer, font, &self.text);
        }
        // The debug overlay is drawn last so it stays legible over the frozen
        // frame and any message the room raised before it opened.
        if let Some(debug_menu) = self.debug_menu.as_mut() {
            debug_menu.draw(
                &mut self.framebuffer,
                self.font.as_ref(),
                &self.text,
                self.menu_assets.as_ref(),
                &self.game,
            );
        }
        // The return-to-title prompt draws over everything, including the
        // debug overlay, with its own full-screen dim.
        if let Some(return_title) = self.return_title.as_ref() {
            return_title.draw(&mut self.framebuffer, self.font.as_ref(), &self.text);
        }
    }

    /// The last rendered frame.
    fn frame(&self) -> &Framebuffer {
        &self.framebuffer
    }

    /// Open the lab computer terminal if the room's scripts raised the
    /// interactive-screen gate for it.
    ///
    /// The original's `check_and_display_interactive_screen` (0x0042a030) tests
    /// scenario flag `0x20` and dispatches on the SysFlags bit: `0x1e` is the
    /// computer terminal (room 5060), whose init parks the player 9000 units
    /// below the floor, switches to camera 4 and installs the terminal's lights.
    fn maybe_open_computer(&mut self) {
        if self.computer.is_some() {
            return;
        }
        if !self.game.flag_test(game::BANK_SCENARIO, 0x20, false) {
            return;
        }
        if !self.game.flag_test(game::BANK_SYSTEM, 0x1e, false) {
            return;
        }
        self.open_computer();
    }

    /// `computer_lab_init` (0x004123d0): install the terminal's camera, lights
    /// and parked player, then open the screen.
    fn open_computer(&mut self) {
        let cut = self.game.camera.current_cut;
        let player_y = self.player.pos[1];
        self.computer_saved = Some(ComputerSaved {
            cut,
            player_y,
            lights: self.loaded.room.lights,
            ambient: self.loaded.room.ambient,
        });
        // The monitor close-up is camera 4 in room 5060.
        if self.loaded.room.cuts.get(4).is_some() {
            self.game.set_camera_cut(4);
            self.loaded.room.current_cut = 4;
        }
        self.player.pos[1] = -9000;
        self.game.sync_entity_from_player(&self.player);
        for (index, light) in self.loaded.room.lights.iter_mut().enumerate() {
            light.pos = ui::computer_lab::LAB_LIGHTS[index];
            light.color = [0x78, 0x78, 0x78];
            light.kind = 0;
            light.radius = 0xFFFF;
        }
        self.loaded.room.ambient = [ui::computer_lab::LAB_AMBIENT; 3];
        self.computer = Some(ui::computer_lab::ComputerLab::new());
    }

    /// One frozen tick of the computer terminal.
    fn tick_computer(&mut self, ui: UiInput) {
        let Some(screen) = self.computer.as_mut() else {
            return;
        };
        if screen.update(ui, &self.text) {
            self.close_computer();
        }
    }

    /// `computer_lab_finish` (0x00413930): apply the unlock, clear the gate and
    /// restore the saved camera, lights and player.
    fn close_computer(&mut self) {
        let Some(screen) = self.computer.take() else {
            return;
        };
        if let Some(bit) = screen.unlock() {
            self.game.apply_flag(2, bit, 0);
        }
        if screen.map_flag() {
            self.game
                .apply_flag(game::BANK_ROOM_FLAGS, ui::computer_lab::LAB_MAP_FLAG, 0);
        }
        self.game.apply_flag(game::BANK_SCENARIO, 0x20, 1);
        if let Some(saved) = self.computer_saved.take() {
            self.loaded.room.lights = saved.lights;
            self.loaded.room.ambient = saved.ambient;
            self.player.pos[1] = saved.player_y;
            self.game.sync_entity_from_player(&self.player);
            if saved.cut < self.loaded.room.cuts.len() {
                self.game.set_camera_cut(saved.cut);
                self.loaded.room.current_cut = saved.cut;
            }
        }
    }

    /// Step the camera cut with the debug shift+`,`/`.` keys.
    fn step_camera_cut(&mut self, delta: i32) {
        let count = self.loaded.room.cuts.len();
        if count == 0 {
            return;
        }
        self.loaded.room.current_cut =
            (self.loaded.room.current_cut as i32 + delta).rem_euclid(count as i32) as usize;
        self.game.camera.current_cut = self.loaded.room.current_cut;
    }

    /// Refresh the window title when the active cut changed.
    fn update_window_title(&mut self, window: *mut SDL_Window) -> Result<()> {
        update_window_title(window, &self.loaded, &mut self.titled_cut)
    }

    /// Open the audio device and load the room's BGM banks, if not already
    /// open. A missing device logs a warning and leaves the engine silent.
    fn start_audio(&mut self, pack: &Pack) {
        if self.music.is_none() {
            self.music = Mixer::open();
            if self.music.is_none() {
                eprintln!("warning: no audio device; continuing without sound");
            }
        }
        bgm::apply_live(&mut self.music, &mut self.game, &mut self.bgm_cache, pack);
    }

    /// Feed the mixer's streaming voices.
    fn update_audio(&mut self) {
        if let Some(mixer) = &mut self.music {
            mixer.update();
        }
    }

    /// Play UI cues through the session's own mixer (the typewriter save
    /// screen runs while the room's mixer is open).
    fn play_ui_cue_names(&mut self, pack: &Pack, cues: Vec<ui::UiCue>) {
        if cues.is_empty() {
            return;
        }
        let Some(mixer) = self.music.as_mut() else {
            return;
        };
        for cue in cues {
            let Some(name) = cue.name() else {
                continue;
            };
            if let Some(wav) = self.sfx_cache.load(pack, name) {
                mixer.play_sfx(wav, 1.0, 0.0);
            }
        }
    }
}

/// Apply the shipped new-game room-items bank: bit set = the item is still in
/// its room. Every boot that is not loading a save starts from this pattern,
/// so a directly booted room's `item_aot_set` sites register instead of
/// reading as already taken.
fn seed_room_items(game: &mut game::GameState) {
    game.flags[7]
        .bytes_mut()
        .copy_from_slice(&NEW_GAME_ROOM_ITEMS);
}

/// Apply the selected character's playable start: the model-id and
/// selected-character bytes, full health, the character's maximum, the BioCard
/// health copy and the `0x10` status byte.
///
/// `InitializeGame` writes these after the BioCard copy on every new game; the
/// arbitrary-room boot seeds the same block so a directly booted room starts
/// with a live character rather than a zeroed one.
fn seed_start_health(game: &mut game::GameState, character: u8) {
    let character = character & 1;
    game.state_bytes[usize::from(game::STATE_BYTE_CHARACTER_MODEL)] = character;
    game.state_bytes[usize::from(game::STATE_BYTE_CHARACTER)] = character;
    // `InitializeGame` also writes the selected character into the player
    // entity's id (`g_playerEntity.id = g_SelectedCharactedId`, GameStart.cpp
    // 0x0047...); the room load's model switch then leaves it alone because
    // the id already equals `g_CharacterModelId`.
    game.entities[0].id = character;
    game.entities[0].health = NEW_GAME_HEALTH[usize::from(character)];
    game.max_health = game::character_max_health(character);
    game.set_health_copy(NEW_GAME_HEALTH[usize::from(character)]);
    // `set_health_status` also mirrors the byte the scripts read with `cmpb 50`.
    game.set_health_status(0x10);
}

/// The player placement for a room booted directly rather than through a door
/// (`--room` and the debug jump's target).
///
/// The original takes the room's first stairwell door entry when it has one (a
/// stairwell room's landing is its real entry) and otherwise the first door
/// record. It decodes where that door leads, loads the neighbour's RDT and
/// finds the neighbour's record whose destination decodes back to `target`;
/// that record's arrival is the exact spot the game uses when the player walks
/// in through the paired door. With no pair the placement falls back to a free
/// spot at the first walk zone's edge, and `None` when even that finds no
/// free spot.
pub fn room_entry_placement(
    pack: &Pack,
    room: &RoomState,
    game: &game::GameState,
    target: RoomId,
) -> Option<([i32; 3], u16)> {
    let door = entry_door(&game.doors);
    if let Some(door) = door
        && let Some(dest) = game.door_destination(target, door.next_room)
        && dest != target
        && let Ok(neighbour) = room_door_state(pack, dest)
        && let Some(back) = neighbour
            .doors
            .iter()
            .flatten()
            .find(|back| game.door_destination(dest, back.next_room) == Some(target))
    {
        return Some((back.next_pos, back.next_angle as u16 & 0x0FFF));
    }
    fallback_room_entry(room, door)
}

/// The door the arbitrary start treats as the room's entry.
///
/// A stairwell landing is a stairwell room's real entry, so the first
/// stair/ladder record wins; every other room uses its first door entry.
fn entry_door(doors: &[Option<game::Door>; game::ROOM_ACTION_SLOTS]) -> Option<game::Door> {
    doors
        .iter()
        .flatten()
        .copied()
        .find(|door| crate::door::is_stair_type(door.door_type))
        .or_else(|| doors.iter().flatten().copied().next())
}

/// Build a neighbour room's door table: parse its RDT and SCD override and run
/// the init script, without loading backgrounds or player assets, and return
/// the state the `door_aot_set` records landed in.
fn room_door_state(pack: &Pack, id: RoomId) -> Result<game::GameState> {
    let rdt_bytes = &*pack.read(&id.rdt_entry())?;
    let room = rdt::parse(rdt_bytes, id)?;
    let scripts = match pack.read(&id.scd_entry()) {
        Ok(bytes) => scd::reader::parse(&bytes)?,
        Err(_) => scd::reader::parse(rdt_bytes)?,
    };
    let mut game = game::GameState::new(id, &room);
    let mut vm = scd::vm::CommandVm::new(&scripts);
    let mut host = game::ScdGameHost::new(&mut game);
    vm.run_init(&mut host);
    Ok(game)
}

/// The paired-arrival miss fallback: a free spot at the first walk zone's
/// edge, picked from the entry door's direction, then the zone centre and
/// three inset edge spots. `None` when the room has no walk zone or every
/// candidate is blocked.
fn fallback_room_entry(room: &RoomState, door: Option<game::Door>) -> Option<([i32; 3], u16)> {
    let zone = room.walk_zones.first()?;
    let x = i32::from(zone.x1);
    let z = i32::from(zone.z1);
    let width = i32::from(zone.x2) - x;
    let depth = i32::from(zone.z2) - z;
    let (cx, cz) = (x + width / 2, z + depth / 2);
    let primary = match door.map_or(3, |door| door.direction & 3) {
        0 => ([cx, 0, z + depth + 300], 0xC00),
        1 => ([x + width + 300, 0, cz], 0),
        2 => ([x - 300, 0, cz], 0x800),
        _ => ([cx, 0, z - 300], 0x400),
    };
    if entry_spot_free(room, primary.0) {
        return Some(primary);
    }
    [
        ([cx, 0, cz], 0),
        ([x + width + 200, 0, cz], 0),
        ([x - 200, 0, cz], 0x800),
        ([cx, 0, z - 200], 0x400),
    ]
    .into_iter()
    .find(|(pos, _)| entry_spot_free(room, *pos))
}

/// The original's arbitrary-start free check: the point must sit in a walk
/// zone, and no collision record with both blocking bits (`flags & 0x300`)
/// may contain its radius-0 position or any of its four 160-unit body
/// offsets.
fn entry_spot_free(room: &RoomState, pos: [i32; 3]) -> bool {
    const BODY_OFFSETS: [[i32; 2]; 5] = [[0, 0], [160, 0], [-160, 0], [0, 160], [0, -160]];
    enemy::walk::walk_zone_find(room, pos[0], pos[2]).is_some()
        && !BODY_OFFSETS.iter().any(|[dx, dz]| {
            let x = pos[0] + dx;
            let z = pos[2] + dz;
            room.collision.records(x, z).iter().any(|rect| {
                rect.flags & 0x300 == 0x300
                    && !player::point_outside(
                        x,
                        z,
                        i32::from(rect.x_max),
                        i32::from(rect.z_max),
                        i32::from(rect.x_min),
                        i32::from(rect.z_min),
                    )
            })
        })
}

/// The shipped new-game card (`data/bio_card.dat`), when the pack carries one.
///
/// `InitializeGame` loads this file and memcpys its 1052 bytes over the
/// BioCard block on every new game; the port reads it for the same purpose.
fn bio_card(pack: &Pack) -> Option<save::SaveFile> {
    let bytes = pack.read(save::SAVE_PREFIX_ENTRY).ok()?;
    match save::SaveFile::from_bytes(&bytes) {
        Ok(card) => Some(card),
        Err(err) => {
            eprintln!("warning: invalid {}: {err:#}", save::SAVE_PREFIX_ENTRY);
            None
        }
    }
}

/// The new-game identity: the shipped card's stage/room (its stage byte is
/// 0-based, the port's is the RDT file digit), else the main hall.
fn new_game_id(pack: &Pack, character: u8) -> RoomId {
    let fallback = RoomId {
        stage: 1,
        room: NEW_GAME_ROOM,
        player_flag: character & 1,
    };
    let Some(card) = bio_card(pack) else {
        return fallback;
    };
    let stage = card.stage.saturating_add(1);
    if stage > RoomId::MAX_STAGE || card.room > RoomId::MAX_ROOM {
        return fallback;
    }
    RoomId {
        stage,
        room: card.room,
        player_flag: character & 1,
    }
}

/// Apply the shipped new-game state: the `bio_card.dat` copy
/// (`InitializeGame`'s 1052-byte memcpy), then `SetInitialItems` and the
/// explicit overrides that follow it.
fn seed_new_game(pack: &Pack, game: &mut game::GameState, character: u8) {
    let character = character & 1;
    // `TestHooks_StartNewGame` (and the character-select hand-off it mirrors)
    // raise `MSF_CHAR_VARIANT` for Jill, so the room loads the Jill RDT
    // variant and the scripts' `ck FG_STATUS, 8` tests select her spawns.
    if character == 1 {
        game.apply_flag(5, game::MSF_CHAR_VARIANT, 0);
    }
    let card = bio_card(pack);
    if let Some(card) = &card {
        card.apply_new_game_card(game);
        // The card copy restates the room bytes; the port's state image uses
        // the 1-based stage digit, so put the identity back.
        game.state_bytes[0] = game.id.stage;
        game.state_bytes[1] = game.id.room;
    }
    // `InitializeGame` writes the selected character into the model-id and
    // selected-character bytes after the card copy, then seeds the playable
    // start health; the arbitrary-room boot applies the same block.
    seed_start_health(game, character);
    seed_room_items(game);
    // The three carried room-pickup quantities SetInitialItems seeds
    // (BioCard 0x20C..0x20E): ROOM1160's shotgun shells and the two
    // flamethrower rooms' fuel.
    game.state_bytes[0x0C] = NEW_GAME_PICKUP_QUANTITIES[0];
    game.state_bytes[0x0D] = NEW_GAME_PICKUP_QUANTITIES[1];
    game.state_bytes[0x0E] = NEW_GAME_PICKUP_QUANTITIES[2];
    // The shipped card starts the counter at 1 (its save screen shows that
    // first count); a pack without one falls back to 0.
    game.state_bytes[usize::from(game::STATE_BYTE_SAVES)] =
        card.as_ref().map_or(0, |card| card.saves);
    // `InitializeGame` zeroes the play timer on a new game.
    game.state_bytes[0x24..0x28].fill(0);
    // `SetInitialItems` writes the character's whole slot run (six for Chris,
    // eight for Jill) and clears the rest. The port keeps the player's slots
    // in `inventory` and the contiguous block's last six in
    // `rebecca_inventory`; Jill's two extra slots overlap Rebecca's first two.
    game.inventory.clear();
    game.add_item(ITEM_KNIFE, 0);
    if character == 1 {
        game.add_item(ITEM_BERETTA, 15);
    }
    game.add_item(ITEM_FIRST_AID_SPRAY, 1);
    if character == 0 {
        // Chris's run is six slots, so Rebecca's card beretta survives.
        game.rebecca_inventory[0] = game::InventoryItem {
            id: ITEM_BERETTA,
            quantity: 15,
        };
    } else {
        // Jill's eight-slot run clears Rebecca's first two slots.
        let overlap = game::INVENTORY_SLOTS_JILL - game::INVENTORY_SLOTS_CHRIS;
        game.rebecca_inventory[..overlap].fill(game::InventoryItem::default());
    }
    // `InitializeGame`'s Jill block: the first playthrough raises the main
    // hall's ink-ribbon room-items bit and the Jill first-run scenario bit.
    let second_playthrough = game.flag_test(
        game::BANK_SCENARIO,
        game::SCENARIO_FLAG_SECOND_PLAYTHROUGH,
        false,
    );
    if character == 1 && !second_playthrough {
        game.apply_flag(7, game::ROOM_ITEM_FLAG_MAIN_HALL_RIBBON, 0);
        game.apply_flag(game::BANK_SCENARIO2, game::SCENARIO2_FLAG_JILL_FIRST_RUN, 0);
    }
}

/// Build the next-cycle clear-data card
/// (`EndingScreen.cpp:1049-1130`).
///
/// The card is a fresh new-game state, not a snapshot of the finished game:
/// the shipped `bio_card.dat` copy, the selected character's start health, the
/// initial item set, a zeroed save counter, then the unlocks the clear grants -
/// the infinite rocket launcher (a no-save run or an already-infinite one), the
/// special key (held, or granted by the epilogue), the under-four-hour machine
/// gun - and the second-playthrough flag.
fn build_next_cycle_save(pack: &Pack, flow: &EndingFlow) -> Result<save::SaveFile> {
    let stats = flow.stats;
    let character = stats.character & 1;
    let id = new_game_id(pack, character);
    let loaded = load_room(pack, id)?;
    let mut game = new_game_state(pack, id, &loaded.room);
    seed_new_game(pack, &mut game, character);

    // The reference reloads the blank card and never runs `InitializeGame`'s
    // Jill first-run block (`EndingScreen.cpp:1059-1126`); the port's seed
    // replays it, so put the marker back to the card's state. The 0x7B raised
    // below is what skips the block on the next cycle (`GameStart.cpp:606-614`).
    game.apply_flag(game::BANK_SCENARIO2, game::SCENARIO2_FLAG_JILL_FIRST_RUN, 1);

    // The next-cycle card restarts the save counter and keeps the clear time
    // snapshot; `InitializeGame`'s continue branch zeroes the timer again when
    // the card is loaded (`saves == 0`).
    game.state_bytes[usize::from(game::STATE_BYTE_SAVES)] = 0;
    game.set_play_timer(stats.time_ticks);

    if stats.no_save() || flow.has_infinite_launcher {
        // The no-save run and the carried-over unlock both hand out the
        // infinite rocket launcher and raise its permanent flag.
        game.apply_flag(
            game::BANK_SCENARIO,
            weapons::SCENARIO_FLAG_INF_R_LAUNCHER,
            0,
        );
        game.add_item(ending::ITEM_ROCKET_LAUNCHER, 1);
    }
    if flow.has_special_key {
        game.add_item(ending::ITEM_SPECIAL_KEY, 1);
    }
    if stats.under_time_limit() {
        let item = if character == 0 {
            items::ITEM_MINIMI
        } else {
            items::ITEM_INGRAM
        };
        game.add_item(item, 1);
    }
    game.apply_flag(
        game::BANK_SCENARIO,
        game::SCENARIO_FLAG_SECOND_PLAYTHROUGH,
        0,
    );

    let Ok(prefix) = pack.read(save::SAVE_PREFIX_ENTRY) else {
        return Ok(save::SaveFile::from_state(&game));
    };
    match save::SaveFile::from_state_with_prefix(&game, &prefix) {
        Ok(file) => Ok(file),
        Err(err) => {
            eprintln!("warning: invalid {0}: {err:#}", save::SAVE_PREFIX_ENTRY);
            Ok(save::SaveFile::from_state(&game))
        }
    }
}

/// Which screen `--ui` boots or the app opens first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppBoot {
    /// The interactive root boot: the Virgin logo (28, absent from this
    /// install), the Capcom logo (23), then the title with its once-per-process
    /// opening film. Captures and `--ui` boots use [`AppBoot::Title`] instead.
    Boot,
    /// The title screen.
    Title,
    /// The character-selection screen.
    CharSelect,
    /// The load screen.
    SaveLoad,
    /// The typewriter save screen over the deterministic capture room.
    Save,
    /// A new game as the character (`0` Chris, `1` Jill), optionally booting
    /// straight into `room` instead of the card's start room (the new-game
    /// semantics, the room's first walk zone centre as the spawn).
    NewGame { character: u8, room: Option<RoomId> },
    /// A continue from the save block at `path` (`--load`): the file's own
    /// stage, room, character and state boot the session, exactly like the
    /// title screen's load picker minus the menu.
    Continue { path: PathBuf },
    /// An attract-demo reel boot: the pack's `demo/` entry `N` replayed with
    /// its recorded input, the reference's `--test-demo` anchor.
    Demo(u8),
    /// The pause menu over the deterministic capture room.
    Menu,
    /// The item box over the deterministic capture room.
    ItemBox,
    /// The F1 debug menu's tool list over the deterministic capture room.
    Debug,
    /// The FILE tab over the deterministic capture room.
    File,
    /// The map tab over the deterministic capture room.
    Map,
    /// The item viewer over the deterministic capture room's combat knife.
    View,
    /// The DIED screen over the deterministic capture room, with the player
    /// killed through the real damage helper (`--ui death`).
    Death(u8),
    /// The post-film RESULT screen for ending `id` as `character`
    /// (`--ui result`).
    EndingResult { id: u8, character: u8 },
    /// The rocket-launcher epilogue (`--ui epilogue`).
    EndingEpilogue,
}

/// The room `--ui menu` and its capture boot into, with the known capture
/// inventory added on top of the room boot.
pub const MENU_ROOM: &str = "1001";

/// The room `--ui death` boots into: a mansion room whose player variants do
/// not run a scripted intro, so the fall and the DIED screen show the fallen
/// corpse.
pub const DEATH_ROOM: &str = "101";

/// Fixed ticks the headless menu capture settles before the frame is drawn.
const MENU_CAPTURE_TICKS: u32 = 30;

/// Fixed ticks the headless `--ui view` capture settles: the menu's, then the
/// viewer's 0x40-frame intro, so the frame shows the resting turntable.
const VIEW_CAPTURE_TICKS: u32 = MENU_CAPTURE_TICKS + 0x40;
/// Fixed ticks the headless item-box capture settles before the frame.
const ITEM_BOX_CAPTURE_TICKS: u32 = 30;
/// Fixed ticks the headless debug-menu capture settles before the frame; the
/// menu has no fade, so one frozen tick is enough.
const DEBUG_CAPTURE_TICKS: u32 = 1;
/// Fixed ticks the headless FILE capture settles before the frame.
const FILE_CAPTURE_TICKS: u32 = 30;
/// Fixed ticks the headless save-screen capture settles before the frame.
const SAVE_CAPTURE_TICKS: u32 = 36;
/// Fixed ticks the headless DIED-screen capture settles before the frame:
/// enough to run the 90-frame delay, the white-out, the reveal ramp and part
/// of the settled-text hold.
const DEATH_CAPTURE_TICKS: u32 = 400;
/// Fixed ticks the headless RESULT capture settles before the frame: past the
/// reveal pairs and the step-3 hold, on the step-4 input gate with all four
/// plates faded in.
const RESULT_CAPTURE_TICKS: u32 = 0x3C + 0x3C + 0x5A + 30;
/// Fixed ticks the headless epilogue capture settles before the frame: past the
/// fade-in and partway into the 0x10D-frame hold.
const EPILOGUE_CAPTURE_TICKS: u32 = 160;

/// The app's current screen.
enum Mode {
    /// No screen yet: [`App::new`]'s state until [`App::boot`] opens one.
    Boot,
    /// The title screen script.
    Title(Box<ui::lua_screen::LuaScreen>),
    Select(Box<ui::lua_screen::LuaScreen>),
    Load(Box<dyn ui::save_load::SaveScreen>),
    Play(Box<GameSession>),
    /// A film owns the whole app until it ends.
    Movie(Box<MovieSession>),
    /// The post-film RESULT screen.
    Ending(Box<ui::ending::ResultScreen>),
    /// The rocket-launcher epilogue.
    Epilogue(Box<ui::ending::EpilogueScreen>),
    /// The next-cycle save screen (save mode), which reports `Resume` on exit.
    Save(Box<dyn ui::save_load::SaveScreen>),
}

/// The state the post-film ending sequence carries from the RESULT screen
/// through the epilogue to the next-cycle save.
#[derive(Debug, Clone, Copy)]
struct EndingFlow {
    /// The clear time, save counter and identity the screens read.
    stats: ending::EndStats,
    /// Whether the already-cleared-once bit (0x7B) was set going in: the
    /// congratulations film and the cleared-once backdrops follow it.
    second_playthrough: bool,
    /// Whether the special key is carried into the next cycle: held from the
    /// run, or granted by playing the epilogue (`s_hasSpecialKey++`,
    /// `EndingScreen.cpp:1044`).
    has_special_key: bool,
    /// Whether the completed run already owned the infinite rocket launcher.
    has_infinite_launcher: bool,
}

/// The mode machine: one screen at a time, with the pack and decoded shared
/// assets owned here.
/// A film an app screen queued: the session plus the action to apply when it
/// finishes (`None` for the standalone `--fmv` playback, which quits).
struct PendingMovie {
    session: MovieSession,
    action: Option<ScreenAction>,
}

struct App {
    pack: Pack,
    save_dir: PathBuf,
    /// The action map the window reads, handed to every gameplay session.
    bindings: controls::Bindings,
    /// The save/load screen entry the pack's scripts registered (`None` for
    /// the ported original).
    save_screen: Option<String>,
    font: Option<font::Font>,
    text: Text,
    mode: Mode,
    framebuffer: Framebuffer,
    ticks: u64,
    /// Whether entering a game opens the audio device; captures leave it off.
    audio: bool,
    /// The UI screens' own mixer, opened lazily on the first cue so captures
    /// and audio-less runs stay silent. Gameplay sessions own a separate
    /// mixer; UI cues never share it.
    ui_music: Option<Mixer>,
    /// The app-level film's mixer, opened when a queued film starts and only
    /// on the interactive path; a dummy device is treated as absent so films
    /// pace on the fixed tick.
    movie_music: Option<Mixer>,
    /// The action to apply when the active app film finishes.
    movie_action: Option<ScreenAction>,
    /// A film waiting to start on the next tick.
    pending_movie: Option<PendingMovie>,
    /// Films queued to play back to back, with `(id, character)`; the chain's
    /// last film is followed by [`App::chain_action`] (or a quit when `None`).
    film_chain: VecDeque<(u8, u8)>,
    /// The action applied when [`App::film_chain`] drains.
    chain_action: Option<ScreenAction>,
    /// Whether a film chain is active: a finishing film advances the chain
    /// instead of applying `movie_action`.
    chain_active: bool,
    /// Whether the automatic opening/intro films may queue. True only on the
    /// interactive root boot; captures, `--ui` and `--ending` leave it off so
    /// their frames stay deterministic.
    films: bool,
    /// Whether the title's opening film (id 0) has been queued once.
    opening_played: bool,
    /// The pack's attract-demo entries (`demo/<hex>.dat`), sorted; the title
    /// cycles them instead of a hard-coded list.
    demos: Vec<String>,
    /// The next attract reel (`g_CurrentAttractModeId`): the title's idle
    /// timeout plays `demos[next_demo]`, then advances the cycle.
    next_demo: usize,
    /// The death-finished tick queued its title hand-off: the next update
    /// opens the title instead of running another gameplay tick, so the tick
    /// that finished the death is still traced with the session's final state.
    pending_return_title: bool,
    /// Whether the prologue film (id 1) is playing for a pending new game:
    /// the follow-up `NewGame` action then builds the session instead of
    /// queueing the film again. A later confirm queues the prologue anew,    /// exactly like the original.
    prologue_pending: bool,
    /// One-shot cache for the UI cue sounds (`se/cursor.wav`, ...).
    ui_sfx_cache: SfxCache,
    /// Whether the port-only debug room overlay is enabled for the gameplay
    /// sessions this app starts. Enabled by default; `--debug-menu` is
    /// retained for CLI compatibility.
    debug_menu: bool,
    /// The active post-film ending sequence, or `None` outside one.
    ending: Option<EndingFlow>,
}

/// How one app tick ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppFlow {
    Continue,
    Quit,
}

impl App {
    /// Open the pack's shared assets; the mode machine starts on the title.
    /// `audio` opens the mixer when a game session or film starts.
    ///
    /// The pack's scripts also load once here, in a throwaway VM, so a
    /// registration they make (the save/load screen) is known before the title
    /// appears; sessions keep their own VMs.
    fn new(pack: Pack, save_dir: PathBuf, audio: bool) -> Self {
        let save_screen = lua::registered_save_screen(&pack);
        let text = Text::load(&pack, pack.language());
        let font = font::load_session(&pack, text.script());
        let demos = demo::entries(&pack);
        Self {
            pack,
            save_dir,
            bindings: controls::Bindings::default(),
            save_screen,
            font,
            text,
            mode: Mode::Boot,
            framebuffer: Framebuffer::new(),
            ticks: 0,
            audio,
            ui_music: None,
            movie_music: None,
            movie_action: None,
            pending_movie: None,
            film_chain: VecDeque::new(),
            chain_action: None,
            chain_active: false,
            films: false,
            opening_played: false,
            demos,
            next_demo: 0,
            pending_return_title: false,
            prologue_pending: false,
            ui_sfx_cache: SfxCache::default(),
            debug_menu: true,
            ending: None,
        }
    }

    /// Queue `session` to play before the app continues; `action` is applied
    /// when it finishes. The session starts on the next app tick.
    fn queue_movie(&mut self, session: MovieSession, action: Option<ScreenAction>) {
        self.pending_movie = Some(PendingMovie { session, action });
    }

    /// Open film `id` from the app's pack.
    fn open_movie(&mut self, id: u8, character: u8) -> Result<MovieSession> {
        MovieSession::open(&self.pack, id, character)
    }

    /// Play `films` back to back, then apply `action` (`None` quits the app).
    ///
    /// A film that cannot be opened is logged and skipped, so a pack without
    /// the film or the absent Virgin logo never stalls the chain. The first
    /// film starts on the next app tick.
    fn play_film_chain(
        &mut self,
        films: Vec<(u8, u8)>,
        action: Option<ScreenAction>,
    ) -> Result<AppFlow> {
        self.film_chain = films.into();
        self.chain_action = action;
        self.chain_active = true;
        self.advance_chain()
    }

    /// Start the next film of the active chain; when the chain is exhausted,
    /// apply its action or quit.
    fn advance_chain(&mut self) -> Result<AppFlow> {
        while let Some((id, character)) = self.film_chain.pop_front() {
            match self.open_movie(id, character) {
                Ok(session) => {
                    self.queue_movie(session, None);
                    return Ok(AppFlow::Continue);
                }
                Err(err) => eprintln!("warning: film id {id} unavailable: {err:#}"),
            }
        }
        self.chain_active = false;
        match self.chain_action.take() {
            Some(action) => self.apply(action),
            None => Ok(AppFlow::Quit),
        }
    }

    /// Start a queued film: open the film mixer on the interactive path, queue
    /// its opening audio and enter [`Mode::Movie`].
    fn install_movie(&mut self, pending: PendingMovie) {
        let mut session = pending.session;
        if self.audio && self.movie_music.is_none() {
            self.movie_music = Mixer::open().filter(|mixer| !mixer.is_dummy());
        }
        if let Some(mixer) = &mut self.movie_music {
            mixer.load_movie_audio(session.take_audio());
        }
        self.movie_action = pending.action;
        self.mode = Mode::Movie(Box::new(session));
    }

    /// One tick of the active film. When it ends, the queued action runs (or
    /// the app quits for a standalone `--fmv`).
    fn tick_movie(&mut self, ui: UiInput, input: player::Input) -> Result<AppFlow> {
        let consumed = self.movie_music.as_ref().map(Mixer::movie_samples_played);
        let Mode::Movie(session) = &mut self.mode else {
            return Ok(AppFlow::Continue);
        };
        let tick = session.tick_timed(movie_buttons(ui, input), consumed, TICK_MS);
        let audio = session.take_audio();
        if !audio.is_empty()
            && let Some(mixer) = &mut self.movie_music
        {
            mixer.load_movie_audio(audio);
        }
        if !matches!(tick, MovieTick::Finished | MovieTick::Skipped) {
            return Ok(AppFlow::Continue);
        }
        if let Some(mixer) = &mut self.movie_music {
            mixer.stop_movie_audio();
            mixer.resume_game_sounds();
        }
        if self.chain_active {
            return self.advance_chain();
        }
        let action = self.movie_action.take();
        match action {
            Some(action) => self.apply(action),
            None => Ok(AppFlow::Quit),
        }
    }

    /// Poll the active film's skip input between ticks; see
    /// [`GameSession::poll_movie_skip`].
    fn poll_movie_skip(&mut self, buttons: u16) {
        match &mut self.mode {
            Mode::Movie(session) => {
                session.poll_skip(buttons);
            }
            Mode::Play(session) => session.poll_movie_skip(buttons),
            _ => {}
        }
    }

    /// The fixed step the app loop uses for the current screen.
    ///
    /// The original runs the title, character select, load screen and every
    /// in-game menu/modal with its game-active flag clear, i.e. on the 16 ms
    /// non-gameplay limiter; gameplay and films use the 33 ms one. The title
    /// switches to gameplay pacing once NEW GAME or LOAD GAME is confirmed.
    fn tick_interval(&self) -> f64 {
        match &self.mode {
            Mode::Title(screen) => {
                if screen.value("game_active") != 0 {
                    TICK_MS
                } else {
                    UI_TICK_MS
                }
            }
            Mode::Boot | Mode::Select(_) | Mode::Load(_) | Mode::Save(_) => UI_TICK_MS,
            Mode::Play(session) => {
                session_tick_interval(session.transition.is_some(), session.room_frozen())
            }
            Mode::Movie(_) => TICK_MS,
            // The RESULT/epilogue state machines step once per 30 Hz frame,
            // exactly like the reference's `Task_sleep(1)` loop.
            Mode::Ending(_) | Mode::Epilogue(_) => TICK_MS,
        }
    }

    /// Build the shared screen context.
    fn context(&self) -> UiContext<'_> {
        UiContext {
            pack: &self.pack,
            save_dir: &self.save_dir,
            font: self.font.as_ref(),
            text: Some(&self.text),
            ticks: self.ticks,
            cues: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Play the cues a screen queued this tick.
    ///
    /// The mixer opens on the first cue and only on the interactive path
    /// (`audio`); capture runs leave `ui_music` closed and the queue is
    /// dropped, so their frames never depend on the sound device.
    fn play_ui_cues(&mut self, cues: Vec<ui::UiCue>) {
        if cues.is_empty() || !self.audio {
            return;
        }
        if self.ui_music.is_none() {
            self.ui_music = Mixer::open();
        }
        let App {
            pack,
            ui_music,
            ui_sfx_cache,
            ..
        } = self;
        let Some(mixer) = ui_music.as_mut() else {
            return;
        };
        for cue in cues {
            let Some(name) = cue.name() else {
                continue;
            };
            if let Some(wav) = ui_sfx_cache.load(pack, name) {
                mixer.play_sfx(wav, 1.0, 0.0);
            }
        }
    }

    /// Open the boot screen.
    fn boot(&mut self, boot: AppBoot) -> Result<()> {
        match boot {
            AppBoot::Boot => self.play_boot(),
            AppBoot::Title => self.open_title(),
            AppBoot::CharSelect => self.open_select(),
            AppBoot::SaveLoad => self.open_load(),
            AppBoot::Save => self.open_save_capture(),
            AppBoot::NewGame { character, room } => self.boot_new_game(character, room),
            AppBoot::Continue { path } => self.boot_continue(&path),
            AppBoot::Demo(index) => self.boot_demo(index),
            AppBoot::Menu => self.open_menu(),
            AppBoot::ItemBox => self.open_item_box_capture(),
            AppBoot::Debug => self.open_debug_capture(),
            AppBoot::File => self.open_file_capture(),
            AppBoot::Map => self.open_map_capture(),
            AppBoot::View => self.open_view(),
            AppBoot::Death(character) => self.open_death_capture(character),
            AppBoot::EndingResult { id, character } => {
                self.ending = Some(EndingFlow {
                    stats: ending::EndStats::debug(id, character, 1),
                    second_playthrough: false,
                    has_special_key: false,
                    has_infinite_launcher: false,
                });
                self.open_result()
            }
            AppBoot::EndingEpilogue => {
                self.ending = Some(EndingFlow {
                    stats: ending::EndStats::debug(6, 0, 1),
                    second_playthrough: false,
                    has_special_key: false,
                    has_infinite_launcher: false,
                });
                self.open_epilogue()
            }
        }
    }

    /// The interactive `AppBoot::NewGame` arm.
    ///
    /// The by-value session lives here rather than in [`App::boot`]'s match:
    /// a debug build materialises every `GameSession` temporary as a stack
    /// slot, and the three session arms used to push that function's frame
    /// past the Windows main thread's 1 MiB stack (the same split keeps
    /// `App::apply`'s frame bounded; see `App::start_new_game`).
    fn boot_new_game(&mut self, character: u8, room: Option<RoomId>) -> Result<()> {
        let mut session = match room {
            Some(id) => GameSession::new_in_room(&self.pack, id, character, &self.save_dir)?,
            None => GameSession::new(&self.pack, character, &self.save_dir)?,
        };
        // The interactive arbitrary-room boot lands on the walk zone centre
        // right away; the trace path re-places after the room-set event pass
        // instead (see `arm_new_game_boot`).
        session.place_room_boot_spawn();
        self.start_session(session);
        Ok(())
    }

    /// The interactive `AppBoot::Continue` arm.
    fn boot_continue(&mut self, path: &Path) -> Result<()> {
        let file = save::load_file(path)?;
        let session = GameSession::from_save(&self.pack, &file, &self.save_dir)?;
        self.start_narrated_session(session, game::MESSAGE_BOOT_CONTINUE);
        Ok(())
    }

    /// The interactive `AppBoot::Demo` arm.
    fn boot_demo(&mut self, index: u8) -> Result<()> {
        let session = GameSession::from_demo_index(&self.pack, index, &self.save_dir)?;
        self.start_session(session);
        Ok(())
    }

    /// The interactive root boot: enable the automatic films and queue the
    /// two logos, then the title (which queues its opening film).
    fn play_boot(&mut self) -> Result<()> {
        self.films = true;
        self.play_film_chain(
            vec![(BOOT_VLOGO_ID, 0), (BOOT_CAPCOM_ID, 0)],
            Some(ScreenAction::Title),
        )?;
        Ok(())
    }

    /// Open the title screen. On the interactive root boot the first entry
    /// also queues the opening film (id 0) exactly once per process.
    fn open_title(&mut self) -> Result<()> {
        // A registration that names a missing entry falls back to the ported
        // screen when it opens, which only lists the eight slots, so the
        // folder probe follows the entry the pack actually carries.
        let folder_saves = self
            .save_screen
            .as_deref()
            .is_some_and(|entry| self.pack.contains(entry));
        let mut cx = self.context();
        let screen = ui::lua_screen::LuaScreen::open_screen(
            "title",
            &[("folder_saves", ScreenArg::Bool(folder_saves))],
            ScreenAction::Quit,
            &mut cx,
        )?;
        let cues = cx.cues.into_inner();
        self.mode = Mode::Title(Box::new(screen));
        self.play_ui_cues(cues);
        if self.films && !self.opening_played {
            self.opening_played = true;
            match self.open_movie(TITLE_OPENING_ID, 0) {
                Ok(session) => self.queue_movie(session, Some(ScreenAction::Title)),
                Err(err) => eprintln!("warning: opening film unavailable: {err:#}"),
            }
        }
        Ok(())
    }

    /// Play ending `id`'s film chain into the RESULT screen, as the debug
    /// menu's CLEAR GAME (the `--ending` path over the live run's stats).
    fn clear_game(&mut self, id: u8, flow: EndingFlow, character: u8) -> Result<AppFlow> {
        let films = ending::chain(
            id,
            character,
            flow.second_playthrough,
            flow.has_infinite_launcher,
        );
        self.ending = Some(flow);
        let chain = films.iter().map(|&film| (film, character)).collect();
        self.play_film_chain(chain, Some(ScreenAction::EndingResult))
    }

    /// Open the character-selection screen.
    fn open_select(&mut self) -> Result<()> {
        let mut cx = self.context();
        let screen = ui::lua_screen::LuaScreen::open_screen(
            "char_select",
            &[],
            ScreenAction::Title,
            &mut cx,
        )?;
        let cues = cx.cues.into_inner();
        self.mode = Mode::Select(Box::new(screen));
        self.play_ui_cues(cues);
        Ok(())
    }

    /// Open the load screen. A script's `on_open` may queue cues before the
    /// first tick; they are played here, like a tick's.
    fn open_load(&mut self) -> Result<()> {
        let entry = self.save_screen.clone();
        let mut cx = self.context();
        let screen = ui::save_load::open_load_screen(entry.as_deref(), &mut cx);
        let cues = cx.cues.into_inner();
        self.mode = Mode::Load(screen);
        self.play_ui_cues(cues);
        Ok(())
    }

    /// Enter a gameplay session, opening audio on the interactive path.
    fn start_session(&mut self, mut session: Box<GameSession>) {
        session.set_debug_menu(self.debug_menu);
        session.set_bindings(self.bindings.clone());
        if self.audio {
            session.start_audio(&self.pack);
        }
        self.mode = Mode::Play(session);
    }

    /// Start a session for a new game or a continue through the original's
    /// loading narration: a black screen with the global typewriter message
    /// `message` (0x5B for a new game, 0x5C for a continue) until the encoded
    /// action byte dismisses it. The room-entry fade is armed when the message
    /// clears, so the first gameplay frames fade up from black. The
    /// deterministic `--ui` captures build their sessions directly and stay
    /// unnarrated and unfaded.
    fn start_narrated_session(&mut self, mut session: Box<GameSession>, message: u8) {
        session.request_boot_message(message);
        self.start_session(session);
    }

    /// Build a new game's session and start it through the loading narration.
    ///
    /// The session lives here rather than in [`Self::apply`]: a debug build
    /// materialises every by-value `GameSession` temporary as a stack slot, and
    /// `apply`'s arms used to add up to most of a megabyte of them, over the
    /// Windows main thread's 1 MiB stack. Keeping the moves in this small
    /// helper bounds the frame.
    fn start_new_game(&mut self, character: u8) -> Result<()> {
        let session = GameSession::new(&self.pack, character, &self.save_dir)?;
        self.start_narrated_session(session, game::MESSAGE_BOOT_NEW_GAME);
        Ok(())
    }

    /// Load save `slot` and start its session through the loading narration.
    /// See [`Self::start_new_game`] for why this is not inlined into `apply`.
    fn continue_from_slot(&mut self, slot: usize) -> Result<()> {
        let file = save::load(&self.save_dir, slot)?;
        self.continue_from_file(file)
    }

    /// Continue from a parsed save block (a Lua screen's chosen file).
    fn continue_from_file(&mut self, file: save::SaveFile) -> Result<()> {
        let session = GameSession::from_save(&self.pack, &file, &self.save_dir)?;
        self.start_narrated_session(session, game::MESSAGE_BOOT_CONTINUE);
        Ok(())
    }

    /// Boot the pause-menu session over [`MENU_ROOM`] with the deterministic
    /// capture inventory and the menu already open.
    fn open_menu(&mut self) -> Result<()> {
        let id = RoomId::parse(MENU_ROOM)?;
        let mut session = GameSession::from_room(&self.pack, id, &self.save_dir)?;
        session.seed_menu_capture();
        session.open_menu(&self.pack);
        self.start_session(session);
        Ok(())
    }

    /// Boot the item-box capture over [`MENU_ROOM`]: the known inventory plus
    /// a few box slots, with the box overlay already open.
    fn open_item_box_capture(&mut self) -> Result<()> {
        let id = RoomId::parse(MENU_ROOM)?;
        let mut session = GameSession::from_room(&self.pack, id, &self.save_dir)?;
        session.seed_menu_capture();
        session.seed_item_box_capture();
        session.open_item_box(&self.pack);
        self.start_session(session);
        Ok(())
    }

    /// Boot the debug-menu capture over [`MENU_ROOM`]: the capture
    /// inventory with the F1 tool list open.
    fn open_debug_capture(&mut self) -> Result<()> {
        let id = RoomId::parse(MENU_ROOM)?;
        let mut session = GameSession::from_room(&self.pack, id, &self.save_dir)?;
        session.seed_menu_capture();
        session.open_debug_menu(&self.pack);
        self.start_session(session);
        Ok(())
    }

    /// Boot the FILE tab capture over [`MENU_ROOM`] with a known set of
    /// collected documents.
    fn open_file_capture(&mut self) -> Result<()> {
        let id = RoomId::parse(MENU_ROOM)?;
        let mut session = GameSession::from_room(&self.pack, id, &self.save_dir)?;
        session.seed_file_capture();
        session.open_file(&self.pack);
        self.start_session(session);
        Ok(())
    }

    /// Boot the map tab capture over [`MENU_ROOM`] with the capture inventory.
    /// The tab is always available (only the radio tab reads the scenario
    /// flag), so no extra flag is seeded.
    fn open_map_capture(&mut self) -> Result<()> {
        let id = RoomId::parse(MENU_ROOM)?;
        let mut session = GameSession::from_room(&self.pack, id, &self.save_dir)?;
        session.seed_menu_capture();
        session.open_map(&self.pack);
        self.start_session(session);
        Ok(())
    }

    /// Boot the item viewer over the [`MENU_ROOM`] capture inventory with the
    /// combat knife examined, the deterministic `--ui view` path.
    fn open_view(&mut self) -> Result<()> {
        let id = RoomId::parse(MENU_ROOM)?;
        let mut session = GameSession::from_room(&self.pack, id, &self.save_dir)?;
        session.seed_menu_capture();
        session.game.add_item(ITEM_KNIFE, 0);
        session.open_menu(&self.pack);
        session.open_item_view(&self.pack, ITEM_KNIFE);
        self.start_session(session);
        Ok(())
    }

    /// Boot the deterministic `--ui death` capture: a session over
    /// [`DEATH_ROOM`] as the selected character, killed through the shared
    /// damage helper so the whole fall → fade → DIED flow runs headlessly.
    fn open_death_capture(&mut self, character: u8) -> Result<()> {
        let mut id = RoomId::parse(DEATH_ROOM)?;
        id.player_flag = character & 1;
        let mut session = GameSession::from_room(&self.pack, id, &self.save_dir)?;
        session.game.apply_player_hurt(i16::MAX, 0);
        self.start_session(session);
        Ok(())
    }

    /// Boot the deterministic `--ui save` capture: a session over
    /// [`MENU_ROOM`] with the known inventory plus an ink ribbon, and the
    /// typewriter save screen already open over the frozen frame.
    fn open_save_capture(&mut self) -> Result<()> {
        let id = RoomId::parse(MENU_ROOM)?;
        let mut session = GameSession::from_room(&self.pack, id, &self.save_dir)?;
        session.seed_menu_capture();
        session.game.add_item(items::ITEM_INK_RIBBONS, 1);
        session.open_typewriter_save(&self.pack, true);
        self.start_session(session);
        Ok(())
    }

    /// Record the active ending sequence and open its RESULT screen.
    fn open_result(&mut self) -> Result<()> {
        let Some(flow) = self.ending else {
            return self.open_title();
        };
        let mut screen = ui::ending::ResultScreen::new(flow.stats, flow.second_playthrough);
        {
            let mut cx = self.context();
            screen.open(&mut cx)?;
        }
        self.mode = Mode::Ending(Box::new(screen));
        Ok(())
    }

    /// After the RESULT screen: run the rocket-launcher epilogue for the
    /// qualifying rows (endings 6/7 without the special key), else go straight
    /// to the next-cycle save. Playing the epilogue grants the special key for
    /// the next cycle (`s_hasSpecialKey++`, `EndingScreen.cpp:1044`).
    fn ending_after_result(&mut self) -> Result<()> {
        let Some(flow) = self.ending else {
            return self.open_title();
        };
        let epilogue = ending::row(flow.stats.ending_id).is_some_and(|row| row.epilogue)
            && !flow.has_special_key;
        if epilogue {
            if let Some(flow) = self.ending.as_mut() {
                flow.has_special_key = true;
            }
            self.open_epilogue()
        } else {
            self.open_ending_save()
        }
    }

    /// Open the rocket-launcher epilogue.
    fn open_epilogue(&mut self) -> Result<()> {
        let mut screen = ui::ending::EpilogueScreen::new();
        {
            let mut cx = self.context();
            screen.open(&mut cx)?;
        }
        self.mode = Mode::Epilogue(Box::new(screen));
        Ok(())
    }

    /// Build the next-cycle card and open the normal save screen over it.
    fn open_ending_save(&mut self) -> Result<()> {
        let Some(flow) = self.ending else {
            return self.open_title();
        };
        let file = match build_next_cycle_save(&self.pack, &flow) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("warning: could not build the next-cycle save: {err:#}");
                self.ending = None;
                return self.open_title();
            }
        };
        let entry = self.save_screen.clone();
        let mut cx = self.context();
        let screen = ui::save_load::open_save_screen(entry.as_deref(), &mut cx, file, false);
        let cues = cx.cues.into_inner();
        self.mode = Mode::Save(screen);
        self.play_ui_cues(cues);
        Ok(())
    }

    /// Apply a completed screen action.
    fn apply(&mut self, action: ScreenAction) -> Result<AppFlow> {
        match action {
            ScreenAction::CharSelect => self.open_select()?,
            ScreenAction::SaveLoad => self.open_load()?,
            ScreenAction::NewGame { character } => {
                // The interactive root boot plays the prologue film (id 1)
                // with the chosen character before the session is built. The
                // film's follow-up action comes back here and builds the
                // session; a missing film falls through directly.
                if self.films && !self.prologue_pending {
                    self.prologue_pending = true;
                    // The reference's `DisplayIntroAndStartGame` hard-stops the
                    // confirmation cue (`sounds_reset`) the moment the film
                    // starts; the UI mixer is that cue's home.
                    self.ui_music = None;
                    match self.open_movie(PROLOGUE_ID, character) {
                        Ok(session) => {
                            self.queue_movie(session, Some(ScreenAction::NewGame { character }))
                        }
                        Err(err) => {
                            eprintln!("warning: intro film unavailable: {err:#}");
                            self.prologue_pending = false;
                            self.start_new_game(character)?;
                        }
                    }
                } else {
                    self.prologue_pending = false;
                    self.start_new_game(character)?;
                }
            }
            ScreenAction::LoadGame { slot } => {
                self.continue_from_slot(slot)?;
            }
            ScreenAction::LoadFile(path) => {
                let file = save::load_file(&path)?;
                self.continue_from_file(file)?;
            }
            ScreenAction::Title => self.open_title()?,
            ScreenAction::Attract => {
                // The title's idle timeout: play the next entry of the pack's
                // `demo/` enumeration and advance the cycle (the original's
                // `LoadAttractModePlayerData` advances after the load). A pack
                // without reels simply stays on the title.
                match self.next_attract_entry() {
                    Some(entry) => match GameSession::from_demo(&self.pack, &entry, &self.save_dir)
                    {
                        Ok(mut session) => {
                            session.arm_new_game_boot();
                            self.start_session(session);
                        }
                        Err(err) => {
                            eprintln!("warning: attract demo unavailable: {err:#}");
                            self.open_title()?;
                        }
                    },
                    None => self.open_title()?,
                }
            }
            ScreenAction::Resume => {
                if matches!(self.mode, Mode::Save(_)) {
                    // The next-cycle save screen always continues to the title,
                    // whether or not the player wrote a slot.
                    self.ending = None;
                    self.open_title()?;
                }
            }
            ScreenAction::EndingResult => self.open_result()?,
            ScreenAction::EndingEpilogue => self.ending_after_result()?,
            ScreenAction::EndingSave => self.open_ending_save()?,
            ScreenAction::Quit => return Ok(AppFlow::Quit),
        }
        Ok(AppFlow::Continue)
    }

    /// The next attract reel to play and the advanced cycle: the original's
    /// `LoadAttractModePlayerData` wraps after the last shipped `pdemoN.dat`;
    /// here the pack's `demo/` enumeration is the list. `None` when the pack
    /// ships no reels.
    fn next_attract_entry(&mut self) -> Option<String> {
        if self.demos.is_empty() {
            return None;
        }
        let entry = self.demos[self.next_demo % self.demos.len()].clone();
        self.next_demo = (self.next_demo + 1) % self.demos.len();
        Some(entry)
    }

    /// One app tick: start a queued film, advance the active film, or advance
    /// the active screen/room.
    fn update(&mut self, ui: UiInput, input: player::Input, action: bool) -> Result<AppFlow> {
        self.ticks = self.ticks.saturating_add(1);
        // The death-finished tick has been traced; the title hand-off it
        // queued owns this update instead of a gameplay tick.
        if std::mem::take(&mut self.pending_return_title) {
            return self.apply(ScreenAction::Title);
        }
        // A queued film takes over on the next tick; the tick that starts it
        // shows frame 0 without advancing it.
        if let Some(pending) = self.pending_movie.take() {
            self.install_movie(pending);
            return Ok(AppFlow::Continue);
        }
        if matches!(self.mode, Mode::Movie(_)) {
            return self.tick_movie(ui, input);
        }
        let result = self.screen_update(ui);
        if let ScreenResult::Done(action) = result {
            return self.apply(action);
        }
        self.play_update(ui, input, action)
    }

    /// Advance the title/select/load screen; Play reports `Continue` here.
    fn screen_update(&mut self, ui: UiInput) -> ScreenResult {
        let (result, cues) = {
            let App {
                pack,
                save_dir,
                font,
                text,
                mode,
                ticks,
                ..
            } = self;
            let cx = UiContext {
                pack,
                save_dir,
                font: font.as_ref(),
                text: Some(text),
                ticks: *ticks,
                cues: std::cell::RefCell::new(Vec::new()),
            };
            let result = match mode {
                Mode::Title(screen) => screen.update(&cx, ui),
                Mode::Select(screen) => screen.update(&cx, ui),
                Mode::Load(screen) => screen.update(&cx, ui),
                Mode::Save(screen) => screen.update(&cx, ui),
                Mode::Ending(screen) => screen.update(&cx, ui),
                Mode::Epilogue(screen) => screen.update(&cx, ui),
                Mode::Boot | Mode::Play(_) | Mode::Movie(_) => ScreenResult::Continue,
            };
            (result, cx.cues.into_inner())
        };
        self.play_ui_cues(cues);
        result
    }

    /// Tick the gameplay session.
    fn play_update(&mut self, ui: UiInput, input: player::Input, action: bool) -> Result<AppFlow> {
        let mut return_title = false;
        let mut death_finished = false;
        let mut load_slot = None;
        let mut clear_game = None;
        {
            let App { pack, mode, .. } = self;
            let Mode::Play(session) = mode else {
                return Ok(AppFlow::Continue);
            };
            if !session.transition_finished {
                if session.transition.is_some() {
                    // The original's `room_transition_load` blocks the gameplay
                    // loop while the door-animation task runs: the room is
                    // loaded behind the animation but no gameplay tick happens
                    // until the task finishes, and the first tick after it runs
                    // in the destination. Input stays gated exactly the same.
                    session.tick_transition(pack, action || input.run);
                } else {
                    session.tick(pack, ui, input, action)?;
                }
                return_title = session.take_return_title_request();
                death_finished = session.death.finished;
                load_slot = session.take_load_slot_request();
                clear_game = session
                    .take_clear_game_request()
                    .map(|id| (id, session.ending_flow(id)));
            }
        }
        if let Some(slot) = load_slot {
            if let Err(err) = self.continue_from_slot(slot) {
                eprintln!("warning: save slot {slot} unavailable: {err:#}");
                // Back to the debug tools rather than silently resuming.
                let App { pack, mode, .. } = self;
                if let Mode::Play(session) = mode {
                    session.open_debug_menu(pack);
                }
            }
            return Ok(AppFlow::Continue);
        }
        if let Some((id, (flow, character))) = clear_game {
            return self.clear_game(id, flow, character);
        }
        if return_title {
            if death_finished {
                // The tick the death finished is still a traced gameplay tick
                // (the reference's `game_loop` returns after its last inner
                // iteration and that frame is emitted); the title hand-off
                // happens on the next update.
                self.pending_return_title = true;
            } else {
                return self.apply(ScreenAction::Title);
            }
        }
        Ok(AppFlow::Continue)
    }

    /// Draw the active screen into the app framebuffer.
    fn draw(&mut self) {
        let App {
            pack,
            save_dir,
            font,
            text,
            mode,
            framebuffer,
            ticks,
            ..
        } = self;
        match mode {
            Mode::Boot => {}
            Mode::Title(screen) => {
                let cx = UiContext {
                    pack,
                    save_dir,
                    font: font.as_ref(),
                    text: Some(text),
                    ticks: *ticks,
                    cues: std::cell::RefCell::new(Vec::new()),
                };
                screen.draw(&cx, framebuffer);
                framebuffer.fade_overlay(screen.overlay());
            }
            Mode::Select(screen) => {
                let cx = UiContext {
                    pack,
                    save_dir,
                    font: font.as_ref(),
                    text: Some(text),
                    ticks: *ticks,
                    cues: std::cell::RefCell::new(Vec::new()),
                };
                screen.draw(&cx, framebuffer);
                framebuffer.fade_overlay(screen.overlay());
            }
            Mode::Load(screen) => {
                let cx = UiContext {
                    pack,
                    save_dir,
                    font: font.as_ref(),
                    text: Some(text),
                    ticks: *ticks,
                    cues: std::cell::RefCell::new(Vec::new()),
                };
                screen.draw(&cx, framebuffer);
                framebuffer.fade_overlay(screen.overlay());
            }
            Mode::Save(screen) => {
                let cx = UiContext {
                    pack,
                    save_dir,
                    font: font.as_ref(),
                    text: Some(text),
                    ticks: *ticks,
                    cues: std::cell::RefCell::new(Vec::new()),
                };
                screen.draw(&cx, framebuffer);
                framebuffer.fade_overlay(screen.overlay());
            }
            Mode::Ending(screen) => {
                let cx = UiContext {
                    pack,
                    save_dir,
                    font: font.as_ref(),
                    text: Some(text),
                    ticks: *ticks,
                    cues: std::cell::RefCell::new(Vec::new()),
                };
                screen.draw(&cx, framebuffer);
                framebuffer.fade_overlay(screen.overlay());
            }
            Mode::Epilogue(screen) => {
                let cx = UiContext {
                    pack,
                    save_dir,
                    font: font.as_ref(),
                    text: Some(text),
                    ticks: *ticks,
                    cues: std::cell::RefCell::new(Vec::new()),
                };
                screen.draw(&cx, framebuffer);
                framebuffer.fade_overlay(screen.overlay());
            }
            Mode::Play(session) => {
                session.render(pack);
                framebuffer.copy_from(session.frame());
            }
            Mode::Movie(session) => {
                framebuffer.rgba.copy_from_slice(session.frame_rgba());
            }
        }
    }

    /// [`App::draw`] for the GPU backend: a game session renders its two
    /// overlay layers over the GPU scene (see [`GameSession::render_layers`]);
    /// every other screen draws as usual.
    fn draw_layers(&mut self, gpu: &mut GpuRenderer) {
        if let Mode::Play(session) = &mut self.mode {
            session.render_layers(&self.pack, gpu);
            self.framebuffer.copy_from(session.frame());
        } else {
            // Gameplay has ended (or yielded to a film): its uploads go.
            gpu.release_residency();
            self.draw();
        }
    }

    /// The frame drawn over a white GPU scene, when the session shows one.
    fn white_frame(&self) -> Option<&Framebuffer> {
        match &self.mode {
            Mode::Play(session) => session.white_frame(),
            _ => None,
        }
    }

    /// The app framebuffer.
    fn frame(&self) -> &Framebuffer {
        &self.framebuffer
    }

    /// Step the camera cut when playing.
    /// The action map the window reads, for every session to start from.
    fn set_bindings(&mut self, bindings: controls::Bindings) {
        if let Mode::Play(session) = &mut self.mode {
            session.set_bindings(bindings.clone());
        }
        self.bindings = bindings;
    }

    /// Take the bindings the playing session's CONTROLS tool changed, once.
    fn take_bindings_change(&mut self) -> Option<controls::Bindings> {
        let Mode::Play(session) = &mut self.mode else {
            return None;
        };
        let bindings = session.take_bindings_change()?;
        self.bindings = bindings.clone();
        Some(bindings)
    }

    fn step_camera_cut(&mut self, delta: i32) {
        if let Mode::Play(session) = &mut self.mode {
            session.step_camera_cut(delta);
        }
    }

    /// Refresh the window title when playing.
    fn update_window_title(&mut self, window: *mut SDL_Window) -> Result<()> {
        if let Mode::Play(session) = &mut self.mode {
            session.update_window_title(window)?;
        }
        Ok(())
    }

    /// After presenting: tear down a finished transition, then feed the
    /// mixer.
    fn post_present(&mut self) {
        if let Mode::Play(session) = &mut self.mode
            && session.transition_finished
        {
            session.finish_transition(&self.pack);
        }
        self.update_audio();
    }

    /// Whether the playing session's door transition has finished and waits
    /// for its last frame to be presented.
    fn transition_finished(&self) -> bool {
        matches!(&self.mode, Mode::Play(session) if session.transition_finished)
    }

    /// The tween position of the next drawn frame (see [`crate::tween`]).
    fn set_tween_alpha(&mut self, alpha: Option<f32>) {
        if let Mode::Play(session) = &mut self.mode {
            session.tween.set_alpha(alpha);
        }
    }

    /// Tell the playing session the window's presented frame rate.
    fn set_frame_rate(&mut self, frame_rate: u32) {
        if let Mode::Play(session) = &mut self.mode {
            session.set_frame_rate(frame_rate);
        }
    }

    /// A frame-rate change from the playing session's options screen.
    fn take_frame_rate_change(&mut self) -> Option<u32> {
        match &mut self.mode {
            Mode::Play(session) => session.take_frame_rate_change(),
            _ => None,
        }
    }

    /// Feed the mixers.
    fn update_audio(&mut self) {
        if let Mode::Play(session) = &mut self.mode {
            session.update_audio();
        }
        // The UI screens' cue mixer streams independently of the gameplay one;
        // the app-level film mixer streams while a queued film runs.
        if let Some(mixer) = &mut self.ui_music {
            mixer.update();
        }
        if let Some(mixer) = &mut self.movie_music {
            mixer.update();
        }
    }

    /// Run `count` ticks without input, for deterministic captures.
    fn settle(&mut self, count: u32) -> Result<()> {
        for _ in 0..count {
            self.update(UiInput::default(), player::Input::default(), false)?;
        }
        Ok(())
    }

    /// Write the current app frame as a BMP.
    fn capture(&self, path: &Path) -> Result<()> {
        capture_bmp(
            &Image {
                width: self.framebuffer.width,
                height: self.framebuffer.height,
                rgba: self.framebuffer.rgba.clone(),
            },
            path,
        )
    }
}

/// Boot one UI screen directly instead of a room.
///
/// `font` renders the decoded font sheet with sample text; `title`, `select`,
/// `game`, `load` and `save` boot the app screens, `menu` boots the pause menu
/// over [`MENU_ROOM`] with the deterministic capture inventory, `file`, `map`
/// and `box` boot those tabs over the same inventory, and `view` boots the
/// item viewer over it with the combat knife examined. `capture` renders one
/// deterministic frame and exits; otherwise the window stays up until the user
/// quits.
pub fn run_ui(pack: &Path, screen: &str, capture: Option<&Path>) -> Result<()> {
    let save_dir = save::default_save_dir();
    run_ui_with_options(pack, screen, capture, &save_dir, 0)
}

/// [`run_ui`] with an explicit save directory and character digit.
pub fn run_ui_with_options(
    pack: &Path,
    screen: &str,
    capture: Option<&Path>,
    save_dir: &Path,
    character: u8,
) -> Result<()> {
    run_ui_with_mods(
        pack,
        screen,
        capture,
        save_dir,
        character,
        None,
        0,
        &ModOptions::default(),
        false,
        None,
        trace::TraceOptions::default(),
        Authoring::default(),
    )
}

/// [`run_ui_with_options`] with the runtime layers; `mods` behaves exactly
/// like [`open_game_pack`]. `_debug_menu` is retained for CLI
/// compatibility: the port-only F1 room-select overlay is always enabled.
/// `trace` enables the `--ui game` oracle run: `ticks` fixed updates with a
/// trace line per tick and the `capture_every` BMP cadence. `authoring` adds
/// the CLI's oracle seams: a `--load` continue boot, the `--setup-script`
/// chunk and the `--save-out` dump.
#[allow(clippy::too_many_arguments)] // the CLI boot options are all needed
pub fn run_ui_with_mods(
    pack: &Path,
    screen: &str,
    capture: Option<&Path>,
    save_dir: &Path,
    character: u8,
    room: Option<RoomId>,
    demo: u8,
    mods: &ModOptions,
    _debug_menu: bool,
    language: Option<&str>,
    trace: trace::TraceOptions<'_>,
    authoring: Authoring<'_>,
) -> Result<()> {
    if authoring.load.is_some() && screen != "game" {
        bail!("--load is only supported with `--ui game`");
    }
    let boot = match screen {
        "font" => return run_font_ui(pack, capture, mods),
        "title" => AppBoot::Title,
        "select" => AppBoot::CharSelect,
        "load" => AppBoot::SaveLoad,
        "save" => AppBoot::Save,
        "game" => match authoring.load {
            Some(path) => AppBoot::Continue {
                path: path.to_path_buf(),
            },
            None => AppBoot::NewGame {
                character: room.map_or(character & 1, |id| id.player_flag & 1),
                room,
            },
        },
        "demo" => AppBoot::Demo(demo),
        "menu" => AppBoot::Menu,
        "box" | "itembox" => AppBoot::ItemBox,
        "debug" => AppBoot::Debug,
        "file" => AppBoot::File,
        "map" => AppBoot::Map,
        "view" => AppBoot::View,
        "death" => AppBoot::Death(character & 1),
        "result" => AppBoot::EndingResult {
            id: 1,
            character: character & 1,
        },
        "epilogue" => AppBoot::EndingEpilogue,
        other => {
            bail!(
                "unknown --ui screen `{other}`; expected `font`, `title`, `select`, `game`, \
                 `demo`, `menu`, `box`, `debug`, `file`, `map`, `view`, `save`, `load`, \
                 `death`, `result` or `epilogue`"
            )
        }
    };
    run_ui_impl(
        pack,
        boot,
        capture,
        save_dir,
        mods,
        _debug_menu,
        language,
        trace,
        authoring,
    )
}

/// The interactive root boot: the two logos, the title's opening film and the
/// character confirm's prologue, then the chosen game. A capture boots the
/// title directly and skips every automatic film, exactly like `--ui title`.
#[allow(clippy::too_many_arguments)] // the CLI boot options are all needed
pub fn run_root(
    pack: &Path,
    capture: Option<&Path>,
    save_dir: &Path,
    mods: &ModOptions,
    _debug_menu: bool,
    language: Option<&str>,
    renderer: Graphics,
) -> Result<()> {
    run_ui_impl(
        pack,
        AppBoot::Boot,
        capture,
        save_dir,
        mods,
        _debug_menu,
        language,
        trace::TraceOptions::default(),
        Authoring {
            renderer,
            ..Authoring::default()
        },
    )
}

/// Build the capture-path new-game session and start it.
///
/// The session lives here rather than in [`run_ui_impl`]'s arm: a debug build
/// materialises every by-value `GameSession` temporary as a stack slot, and
/// the three session arms used to push that function's frame past the Windows
/// main thread's 1 MiB stack. The same split keeps `App::apply`'s frame
/// bounded (see `App::start_new_game`).
fn start_capture_game(app: &mut App, character: u8, room: Option<RoomId>) -> Result<()> {
    let session = match room {
        Some(id) => GameSession::new_in_room(&app.pack, id, character, &app.save_dir)?,
        None => GameSession::new(&app.pack, character, &app.save_dir)?,
    };
    app.start_session(session);
    Ok(())
}

/// Build the capture-path continue session and start it.
fn start_capture_continue(app: &mut App, path: &Path) -> Result<()> {
    let file = save::load_file(path)?;
    let session = GameSession::from_save(&app.pack, &file, &app.save_dir)?;
    app.start_session(session);
    Ok(())
}

/// Build the capture-path attract-demo session and start it.
fn start_capture_demo(app: &mut App, index: u8) -> Result<()> {
    let session = GameSession::from_demo_index(&app.pack, index, &app.save_dir)?;
    app.start_session(session);
    Ok(())
}

/// The shared body of the `--ui` boot and the interactive root boot.
#[allow(clippy::too_many_arguments)] // the boot options are all needed
fn run_ui_impl(
    pack: &Path,
    boot: AppBoot,
    capture: Option<&Path>,
    save_dir: &Path,
    mods: &ModOptions,
    _debug_menu: bool,
    language: Option<&str>,
    trace: trace::TraceOptions<'_>,
    authoring: Authoring<'_>,
) -> Result<()> {
    if capture.is_some()
        || trace.path.is_some()
        || trace.capture_dir.is_some()
        || authoring.save_out.is_some()
    {
        let pack = open_game_pack(pack, mods, language)?;
        let mut app = App::new(pack, save_dir.to_path_buf(), false);
        match boot {
            // A root-boot capture is the title capture: no automatic films.
            AppBoot::Boot | AppBoot::Title => {
                // Reach the option menu, then let the fade settle.
                app.open_title()?;
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.update(
                    UiInput {
                        any: true,
                        ..UiInput::default()
                    },
                    player::Input::default(),
                    false,
                )?;
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(48)?;
            }
            AppBoot::CharSelect => {
                app.open_select()?;
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(48)?;
            }
            AppBoot::SaveLoad => {
                app.open_load()?;
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(48)?;
            }
            AppBoot::Save => {
                app.open_save_capture()?;
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(SAVE_CAPTURE_TICKS)?;
            }
            AppBoot::NewGame { character, room } => {
                start_capture_game(&mut app, character, room)?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                run_game_trace(&mut app, &trace)?;
            }
            AppBoot::Continue { path } => {
                start_capture_continue(&mut app, &path)?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                run_game_trace(&mut app, &trace)?;
            }
            AppBoot::Demo(index) => {
                start_capture_demo(&mut app, index)?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                run_game_trace(&mut app, &trace)?;
            }
            AppBoot::Menu => {
                app.open_menu()?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(MENU_CAPTURE_TICKS)?;
            }
            AppBoot::ItemBox => {
                app.open_item_box_capture()?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(ITEM_BOX_CAPTURE_TICKS)?;
            }
            AppBoot::Debug => {
                app.open_debug_capture()?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(DEBUG_CAPTURE_TICKS)?;
            }
            AppBoot::File => {
                app.open_file_capture()?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(FILE_CAPTURE_TICKS)?;
            }
            AppBoot::Map => {
                app.open_map_capture()?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(MENU_CAPTURE_TICKS)?;
            }
            AppBoot::View => {
                app.open_view()?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(VIEW_CAPTURE_TICKS)?;
            }
            AppBoot::Death(character) => {
                app.open_death_capture(character)?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(DEATH_CAPTURE_TICKS)?;
            }
            AppBoot::EndingResult { id, character } => {
                app.ending = Some(EndingFlow {
                    stats: ending::EndStats::debug(id, character, 1),
                    second_playthrough: false,
                    has_special_key: false,
                    has_infinite_launcher: false,
                });
                app.open_result()?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(RESULT_CAPTURE_TICKS)?;
            }
            AppBoot::EndingEpilogue => {
                app.ending = Some(EndingFlow {
                    stats: ending::EndStats::debug(6, 0, 1),
                    second_playthrough: false,
                    has_special_key: false,
                    has_infinite_launcher: false,
                });
                app.open_epilogue()?;
                if apply_authoring(&mut app, authoring)? {
                    return Ok(());
                }
                if play_app_script(&mut app, &trace)? {
                    return Ok(());
                }
                app.settle(EPILOGUE_CAPTURE_TICKS)?;
            }
        }
        // A queued film is drained, never played, on the capture path: the
        // frames stay deterministic and audio-free.
        app.pending_movie = None;
        app.draw();
        if let Some(capture_path) = capture {
            return app.capture(capture_path);
        }
        return Ok(());
    }

    let pack = open_game_pack(pack, mods, language)?;
    let mut app = App::new(pack, save_dir.to_path_buf(), true);
    app.boot(boot)?;
    if let Mode::Play(session) = &mut app.mode {
        session.run_setup_script(authoring.setup_script)?;
    } else if authoring.setup_script.is_some() {
        bail!("--setup-script requires a boot that opens a game session");
    }
    let mut display = Display::new("Arklay", false, authoring.renderer)?;
    run_app_loop(&mut app, &mut display)
}

/// Play a scripted `--input` over an app-level `--ui` boot (the title, the
/// menus, the ending screens) when one is given without `--trace`: one app
/// update per scripted tick, each followed by a draw and a `f%05d.bmp`
/// capture on the `--capture-every` cadence. The screens' own regression
/// captures drive them through whole interactions this way.
///
/// Returns `false` (nothing played) for a plain capture boot.
fn play_app_script(app: &mut App, options: &trace::TraceOptions<'_>) -> Result<bool> {
    let Some(script) = options.input else {
        return Ok(false);
    };
    if options.path.is_some() {
        return Ok(false);
    }
    if let Some(dir) = options.capture_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let mut script_input = InputState::default();
    for index in 0..options.ticks {
        let tick = script_input.tick_scripted(script.word_at(index));
        if let AppFlow::Quit = app.update(tick.ui, tick.player, tick.action)? {
            break;
        }
        if let Mode::Play(session) = &mut app.mode {
            session.movie = None;
        }
        app.pending_movie = None;
        app.draw();
        if options.capture_every > 0 && index % options.capture_every == 0 {
            write_trace_capture(app, options, index)?;
        }
        app.post_present();
    }
    Ok(true)
}

/// Apply the CLI authoring seams to the session an app boot just opened:
/// `--setup-script` runs once over it and `--save-out` serialises it and asks
/// the caller to exit before the first tick.
///
/// Returns `true` when the run should stop now (the save was written).
fn apply_authoring(app: &mut App, authoring: Authoring<'_>) -> Result<bool> {
    if authoring.setup_script.is_none() && authoring.save_out.is_none() {
        return Ok(false);
    }
    let Mode::Play(session) = &mut app.mode else {
        bail!("--setup-script/--save-out require a boot that opens a game session");
    };
    session.run_setup_script(authoring.setup_script)?;
    let Some(path) = authoring.save_out else {
        return Ok(false);
    };
    session.write_save(&app.pack, path)?;
    Ok(true)
}

/// The `--ui game` oracle run (`oracle/trace-v1.md`): `options.ticks` fixed
/// updates, each followed by a draw, one trace line and, on the
/// `capture_every` cadence, a `f%05d.bmp` frame. The room swap a finished door
/// transition defers to runs after each frame, exactly like the interactive
/// loop's `post_present`.
fn run_game_trace(app: &mut App, options: &trace::TraceOptions<'_>) -> Result<()> {
    let Some(path) = options.path else {
        return Ok(());
    };
    // The trace starts at the first tick of the new-game room, so the boot
    // sequence's hidden state (the room_set event pass and the entry fade's
    // in-flight accumulator) is installed before tick 0 exactly like the
    // reference's hook sees it.
    if let Mode::Play(session) = &mut app.mode {
        session.arm_new_game_boot();
    }
    let mut writer = trace::TraceWriter::create(path)?;
    if options.capture_every > 0
        && let Some(dir) = options.capture_dir
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    // A door transition's animation frames are not gameplay ticks: the
    // reference's `room_transition_load` blocks its gameplay loop until the
    // door-animation task finishes, so its `game_loop` hook emits no line for
    // them. With scripted input the reference traces every `main_loop` frame
    // instead (`--test-input`), so the door's frames are traced here too and
    // the script's ticks land on the same frames on both sides of the door.
    // `--ticks` counts traced lines, so the loop keeps iterating (with a
    // safety bound) until that many lines have been written.
    let mut emitted = 0u32;
    let mut iterations = 0u32;
    let mut script_input = InputState::default();
    let bound = options
        .ticks
        .saturating_mul(2)
        .saturating_add(MAX_TRANSITION_FRAMES);
    while emitted < options.ticks {
        iterations += 1;
        if iterations > bound {
            break;
        }
        let was_play = matches!(app.mode, Mode::Play(_));
        let in_transition =
            matches!(&app.mode, Mode::Play(session) if session.transition.is_some());
        // The reference emits no trace lines while the DIED screen runs: its
        // display loop is outside `game_loop`, so the tick that opens the
        // screen is the last traced one until the title cycles back into
        // gameplay.
        let screen_before = matches!(&app.mode, Mode::Play(session) if session.death.screen_open());
        // The scripted `--input` word is keyed by the trace tick, exactly like
        // the reference's `--test-input` (which reads it from its own `t`). An
        // iteration that emits no line (a door transition, the death screen)
        // keeps the previous tick's word, matching the reference's per-frame
        // injection while its trace counter is stalled.
        let (tick_ui, tick_player, tick_action) = match options.input {
            Some(script) => {
                let tick = script_input.tick_scripted(script.word_at(writer.tick_index()));
                (tick.ui, tick.player, tick.action)
            }
            None => (UiInput::default(), player::Input::default(), false),
        };
        if let AppFlow::Quit = app.update(tick_ui, tick_player, tick_action)? {
            break;
        }
        // A script's `movie_on` (MSF_FMV_REQUEST) starts a film at the end of
        // the tick. The capture path never plays films - the boot drain above
        // drops the queued ones for deterministic, audio-free frames - so drop
        // an in-game one here too. The reference's oracle hook skips an absent
        // FMV (`UpdateVideoPlayback` clears `g_bMCINotifyEnabled`), and playing
        // it froze the room mid-cutscene: room 60A's intro event set the player
        // and camera, then the film owned every tick after it.
        if let Mode::Play(session) = &mut app.mode {
            session.movie = None;
        }
        app.draw();
        // A session created by this update (the title's attract hand-off) has
        // not run a gameplay tick yet; its first trace line is that tick, not
        // the creation frame.
        if let Mode::Play(session) = &app.mode
            && was_play
            && (!in_transition || options.input.is_some())
            && !screen_before
        {
            let index = writer.tick_index();
            writer.write_tick(&session.trace_tick(&app.framebuffer))?;
            emitted += 1;
            if options.capture_every > 0 && index % options.capture_every == 0 {
                write_trace_capture(app, options, index)?;
            }
        }
        app.post_present();
    }
    writer.flush()
}

/// Write one `capture_every` frame from the app framebuffer.
fn write_trace_capture(app: &App, options: &trace::TraceOptions<'_>, index: u32) -> Result<()> {
    let Some(dir) = options.capture_dir else {
        return Ok(());
    };
    capture_bmp(
        &Image {
            width: app.framebuffer.width,
            height: app.framebuffer.height,
            rgba: app.framebuffer.rgba.clone(),
        },
        &dir.join(format!("f{index:05}.bmp")),
    )
}

/// The interactive app loop shared by `--ui` and the standalone `--fmv`
/// playback: fixed ticks from the latched input, frames presented at the
/// display's `frame_rate` like [`run_session_loop`].
fn run_app_loop(app: &mut App, display: &mut Display) -> Result<()> {
    let mut input = InputState::interactive();
    app.set_bindings(input.bindings().clone());
    let mut event = SDL_Event::default();
    let mut clock = LoopClock::new(display.frame_rate(), false);
    loop {
        let mut cut_delta = 0i32;
        if poll_events(&mut event, &mut input, &mut cut_delta, Some(display)) {
            return Ok(());
        }
        if cut_delta != 0 {
            app.step_camera_cut(cut_delta);
        }
        if let Some(bindings) = app.take_bindings_change() {
            input.apply_bindings_change(bindings);
        }
        if let Some(rate) = app.take_frame_rate_change() {
            display.set_frame_rate(rate);
            clock.set_rate(rate);
        }
        app.set_frame_rate(display.frame_rate());
        app.update_window_title(display.window)?;

        clock.advance();
        // The film skip state advances on every loop pass, not the fixed
        // tick, exactly like the original's platform-driven film update.
        app.poll_movie_skip(movie_buttons_held(input.held_word()));
        let tick_ms = app.tick_interval();
        while clock.accumulator >= tick_ms {
            let tick = input.tick();
            if let AppFlow::Quit = app.update(tick.ui, tick.player, tick.action)? {
                return Ok(());
            }
            clock.ticked(tick_ms);
        }
        let alpha = match clock.pace(tick_ms, app.transition_finished()) {
            Pace::Present { alpha } => alpha,
            Pace::Wait(ns) => {
                app.update_audio();
                unsafe { SDL_DelayNS(ns) };
                continue;
            }
        };
        app.set_tween_alpha(alpha);
        if let Some(gpu) = display.gpu() {
            app.draw_layers(gpu);
            display.present_layers(app.frame(), app.white_frame())?;
        } else {
            app.draw();
            display.present(app.frame())?;
        }
        display.show()?;
        app.post_present();
    }
}

/// Play one film standalone, the `--fmv` debug path.
///
/// The film is read from the game pack. A capture never opens audio: it
/// advances `ticks` fixed 30 Hz ticks, draws the current frame and writes it,
/// so the BMP is deterministic. Interactive playback uses the app's mode
/// machine, so a real device drives the audio-led clock, a skippable film ends
/// on a button and the app quits when the film finishes.
#[allow(clippy::too_many_arguments)] // the CLI run options are all needed
pub fn run_fmv(
    pack: &Path,
    id: u8,
    character: u8,
    capture: Option<&Path>,
    ticks: u32,
    mods: &ModOptions,
    renderer: Graphics,
) -> Result<()> {
    let save_dir = save::default_save_dir();
    if let Some(capture_path) = capture {
        let pack = open_game_pack(pack, mods, None)?;
        let mut session = MovieSession::open(&pack, id, character)?;
        for _ in 0..ticks {
            // Audio-free: the fixed 30 Hz tick is the clock.
            session.tick(0, None);
        }
        let mut display = Display::new("Arklay - fmv", true, Graphics::default())?;
        let mut framebuffer = Framebuffer::new();
        framebuffer.rgba.copy_from_slice(session.frame_rgba());
        display.present(&framebuffer)?;
        return display.capture(capture_path);
    }

    let pack = open_game_pack(pack, mods, None)?;
    let session = MovieSession::open(&pack, id, character)?;
    let mut app = App::new(pack, save_dir, true);
    app.queue_movie(session, None);
    let mut display = Display::new("Arklay", false, renderer)?;
    run_app_loop(&mut app, &mut display)
}

/// Play one ending chain (`id` 1-7) through the app's film mode, the
/// `--ending` debug path.
///
/// The chain is [`crate::ending::chain`] for a first playthrough without the
/// infinite launcher: the pre-ending film, the row's ending film, the
/// congratulations film and, for the first three rows, the staff roll. When
/// the films drain the app runs the full post-film sequence: the RESULT screen,
/// the rocket-launcher epilogue for endings 6/7, the next-cycle save and the
/// title. A capture advances `ticks` fixed 30 Hz ticks over the sequence and
/// writes the current frame; interactive playback runs until the title.
#[allow(clippy::too_many_arguments)] // the CLI run options are all needed
pub fn run_ending(
    pack: &Path,
    id: u8,
    character: u8,
    capture: Option<&Path>,
    ticks: u32,
    mods: &ModOptions,
    renderer: Graphics,
) -> Result<()> {
    // The debug entry plays a no-save first run: a zero clear time, the
    // shipped card's initial save counter (one, i.e. no saves) and no
    // second-playthrough or infinite-launcher carry.
    let flow = EndingFlow {
        stats: ending::EndStats::debug(id, character, 1),
        second_playthrough: false,
        has_special_key: false,
        has_infinite_launcher: false,
    };
    let films = ending::chain(
        id,
        character,
        flow.second_playthrough,
        flow.has_infinite_launcher,
    );
    if films.is_empty() {
        bail!("ending id {id} is outside 1-7");
    }
    let save_dir = save::default_save_dir();
    let pack = open_game_pack(pack, mods, None)?;
    let mut app = App::new(pack, save_dir, capture.is_none());
    // Keep the missing-film diagnostic the pre-sequence entry had: a pack
    // without the films would otherwise warn per film and run the RESULT
    // screen silently.
    let first = movie::name(films[0]).map(movie::pack_path);
    if !first.is_some_and(|entry| app.pack.contains(&entry)) {
        bail!("no ending film could be opened; re-run convert-game with the films");
    }
    app.ending = Some(flow);
    let chain: Vec<(u8, u8)> = films.iter().map(|&film| (film, character)).collect();
    app.play_film_chain(chain, Some(ScreenAction::EndingResult))?;
    if let Some(capture_path) = capture {
        app.settle(ticks)?;
        app.draw();
        return app.capture(capture_path);
    }
    let mut display = Display::new("Arklay - ending", false, renderer)?;
    run_app_loop(&mut app, &mut display)
}

/// The `--ui font` screen.
fn run_font_ui(pack_path: &Path, capture: Option<&Path>, mods: &ModOptions) -> Result<()> {
    let pack = open_game_pack(pack_path, mods, None)?;
    // The sampler shows the sheet the session's language would draw with, so
    // `--language en-US --ui font` samples a packed Western sheet.
    let text = Text::load(&pack, pack.language());
    let font = font::load_session(&pack, text.script())
        .context("the pack has no usable font (re-run convert-game)")?;

    if capture.is_some() {
        unsafe {
            SDL_SetHint(SDL_HINT_VIDEO_DRIVER, c"offscreen".as_ptr());
            SDL_SetHint(SDL_HINT_RENDER_DRIVER, c"software".as_ptr());
        }
    }

    let _sdl = SdlHandle::acquire()?;

    let title = CString::new("Arklay - font").context("window title contains a NUL byte")?;
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
    draw_font_screen(&mut framebuffer, &font);

    if let Some(capture_path) = capture {
        present(renderer, texture, &framebuffer)?;
        return capture_frame(renderer, capture_path);
    }

    let mut event = SDL_Event::default();
    let mut cut_delta = 0i32;
    let mut input = InputState::default();
    loop {
        if poll_events(&mut event, &mut input, &mut cut_delta, None) {
            return Ok(());
        }
        present(renderer, texture, &framebuffer)?;
        if !unsafe { SDL_RenderPresent(renderer) } {
            bail!("SDL_RenderPresent failed: {}", sdl_error());
        }
    }
}

/// The `--ui font` frame: the decoded sheet scaled into the top band, then one
/// sample line per tint and the extended glyph pages over a dark background.
fn draw_font_screen(framebuffer: &mut Framebuffer, font: &font::Font) {
    framebuffer.clear();
    for pixel in framebuffer.rgba.as_chunks_mut::<4>().0 {
        *pixel = [24, 24, 24, 255];
    }
    framebuffer.draw_indexed_sprite(
        &font.texture,
        [0, 0, font.texture.width as i32, font.texture.height as i32],
        [0, 0, WIDTH, 72],
        0,
        2,
        font::Tint::White,
    );

    let margin = font.metrics.left_margin;
    for (line, tint) in [
        font::Tint::White,
        font::Tint::Green,
        font::Tint::Red,
        font::Tint::Grey,
        font::Tint::Yellow,
    ]
    .into_iter()
    .enumerate()
    {
        font.draw_text(
            framebuffer,
            margin,
            76 + line as i32 * 16,
            tint,
            2,
            SAMPLE_TEXT,
            false,
        );
    }
    font.draw_text(
        framebuffer,
        margin,
        156,
        font::Tint::White,
        2,
        SAMPLE_SYMBOLS,
        false,
    );
    font.draw_text(
        framebuffer,
        margin,
        172,
        font::Tint::White,
        2,
        SAMPLE_KANJI_TOP,
        false,
    );
    font.draw_text(
        framebuffer,
        margin,
        188,
        font::Tint::White,
        2,
        SAMPLE_KANJI_BOTTOM,
        false,
    );
    font.draw_text(
        framebuffer,
        margin,
        204,
        font::Tint::White,
        2,
        SAMPLE_SPACING,
        false,
    );
    font.draw_text(
        framebuffer,
        margin,
        222,
        font::Tint::White,
        15,
        SAMPLE_TEXT,
        false,
    );
}

/// Digits, the page-wait ▼ (`0x0B`, placed early so it is not clipped by the
/// 320-wide frame), the letters L and M (`0x28`/`0x29`), the two round
/// parentheses the charset encodes and an end byte. The line is fifteen cells
/// wide, so every sample lands inside the frame.
const SAMPLE_TEXT: &[u8] = &[
    0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x0B, 0x28, 0x29, 0xF8, 0x16, 0xF8,
    0x17, 0x01,
];
/// The first twelve `0xF8` left-page glyphs, the 14x14 kana past the plain grid.
const SAMPLE_SYMBOLS: &[u8] = &[
    0xF8, 0, 0xF8, 1, 0xF8, 2, 0xF8, 3, 0xF8, 4, 0xF8, 5, 0xF8, 6, 0xF8, 7, 0xF8, 8, 0xF8, 9, 0xF8,
    10, 0xF8, 11, 0x01,
];
/// Right-page rows at the top (`0xF9`).
const SAMPLE_KANJI_TOP: &[u8] = &[0xF9, 0, 0xF9, 18, 0xF9, 36, 0xF9, 54, 0xF9, 72, 0x01];
/// Right-page rows beyond row 13 (`0xFA`).
const SAMPLE_KANJI_BOTTOM: &[u8] = &[0xFA, 0, 0xFA, 18, 0xFA, 36, 0xFA, 54, 0x01];
/// Half advances and no-op pad bytes.
const SAMPLE_SPACING: &[u8] = &[
    0x0C, 0xFF, 0x0C, 0xFF, 0x0C, 0x00, 0x0D, 0xFB, 0xFB, 0x0E, 0x01,
];

/// A running door transition.
///
/// The destination room is loaded and booted when the transition starts so it
/// is ready behind the leading black frames, exactly like the original's
/// `room_transition_load`, which loads and boots the room before waiting the
/// animation task out. Gameplay stays blocked for the whole animation; the
/// destination's first gameplay tick is the first one after the timeline
/// finishes.
struct TransitionMode {
    /// The animation timeline.
    transition: transition::Transition<DoorAnimation>,
    /// The record that triggered the transition.
    door: game::Door,
    /// Destination room (camera-only transitions keep the current room).
    target: RoomId,
    /// Record bit `0x80`: re-aim without reloading or moving the player.
    camera_only: bool,
    /// Record bit `0x40`: suppress the post-load door sound.
    silent: bool,
    /// The room the door was taken in.
    source: RoomId,
    /// The press frame's trace view, until the timeline's first frame.
    door_yield: Option<game::DoorYield>,
    /// Destination room loaded behind the black frames.
    destination: Option<LoadedRoom>,
    /// The player was placed at the record's entry point and the destination
    /// swapped in (`room_transition_load` before its `room_set`).
    placed: bool,
    /// The destination's `room_set` ran; the timeline now only owns the
    /// overlay and the input gate.
    entered: bool,
    /// The most recent timeline frame, for rendering.
    frame: transition::TransitionFrame,
}

/// Number of 30 Hz frames the no-art fallback holds black before finishing.
const FALLBACK_TRANSITION_FRAMES: u32 = 30;
/// Safety bound for the headless transition driver.
const MAX_TRANSITION_FRAMES: u32 = 10_000;

/// The `.dor` interpreter adapted to the transition's [`DoorStepper`] seam.
///
/// The adapter drains the VM frame's sound and message queues into per-frame
/// buffers for the engine and forwards the sound-busy handshake. When the pack
/// carries no door art it holds a short black screen instead, so a missing
/// `.dor` never aborts a doorway.
struct DoorAnimation {
    /// Parsed door file; `None` in the no-art fallback.
    dor: Option<door::Dor>,
    /// The animation VM; `None` in the no-art fallback.
    vm: Option<door::vm::Vm>,
    /// Fallback frame counter.
    fallback_frames: u32,
    /// Sound effects emitted since the engine last drained them.
    sfx: Vec<door::vm::Sfx>,
    /// Messages emitted since the engine last drained them.
    messages: Vec<door::vm::Message>,
}

impl DoorAnimation {
    fn new(dor: door::Dor, vm: door::vm::Vm) -> Self {
        Self {
            dor: Some(dor),
            vm: Some(vm),
            fallback_frames: 0,
            sfx: Vec::new(),
            messages: Vec::new(),
        }
    }

    fn missing() -> Self {
        Self {
            dor: None,
            vm: None,
            fallback_frames: 0,
            sfx: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// Drain the sound effects emitted since the last call.
    fn take_sfx(&mut self) -> Vec<door::vm::Sfx> {
        std::mem::take(&mut self.sfx)
    }

    /// Drain the messages emitted since the last call.
    fn take_messages(&mut self) -> Vec<door::vm::Message> {
        std::mem::take(&mut self.messages)
    }

    /// The parsed file and a renderable snapshot of the current VM state.
    fn render_frame(&self) -> Option<(&door::Dor, door::vm::Frame<'_>)> {
        let dor = self.dor.as_ref()?;
        let vm = self.vm.as_ref()?;
        Some((dor, vm.frame()))
    }
}

impl DoorStepper for DoorAnimation {
    fn step(&mut self) -> transition::DoorFrame {
        if let (Some(_), Some(vm)) = (self.dor.as_ref(), self.vm.as_mut()) {
            let writes = vm.fade_writes();
            let frame = vm.step();
            self.sfx.extend(frame.sfx.iter().copied());
            self.messages.extend(frame.messages.iter().copied());
            let mut door_frame = transition::DoorFrame {
                camera: transition::Camera {
                    from: frame.camera.from,
                    to: frame.camera.to,
                    focal: frame.camera.focal,
                },
                fade_type: frame.fade.fade_type,
                fade_counter: frame.fade.counter,
                fade_state: frame.fade.state,
                fade_written: false,
                done: frame.done,
                phase: frame.phase,
                hold: frame.hold,
                black: frame.black,
                sound_busy: false,
            };
            door_frame.fade_written = vm.fade_writes() != writes;
            door_frame.sound_busy = vm.sound_busy();
            return door_frame;
        }

        self.fallback_frames += 1;
        transition::DoorFrame {
            done: self.fallback_frames >= FALLBACK_TRANSITION_FRAMES,
            black: self.fallback_frames < transition::BLACK_FRAMES,
            ..transition::DoorFrame::default()
        }
    }

    fn finish(&mut self) {
        match self.vm.as_mut() {
            Some(vm) => vm.finish(),
            None => self.fallback_frames = FALLBACK_TRANSITION_FRAMES,
        }
    }

    fn set_sound_busy(&mut self, busy: bool) {
        if let Some(vm) = self.vm.as_mut() {
            vm.set_sound_busy(busy);
        }
    }
}

/// Start a transition for `record`'s destination: resolve and parse its
/// `.dor`, create the VM and load the destination room behind the black.
fn start_transition(
    pack: &Pack,
    record: &game::Door,
    transition: &game::RoomTransition,
    source: RoomId,
    fade: &transition::Fade,
) -> Result<TransitionMode> {
    let camera_only = record.camera & 0x80 != 0;
    let silent = record.camera & 0x40 != 0;
    let entry_camera = record.camera & 0x3F;
    let animation = load_door_animation(pack, record, entry_camera);
    let destination = if camera_only {
        None
    } else {
        Some(load_room(pack, transition.target)?)
    };
    // A destination byte of 0x20 and up changes stage: `init_room` yields a
    // frame before its `room_set`.
    let stage_change = record.next_room >= 0x20;
    Ok(TransitionMode {
        transition: transition::Transition::new(animation)
            .with_fade(fade.fade_type(), fade.counter(), fade.state())
            .with_load(camera_only, stage_change),
        door: *record,
        target: transition.target,
        camera_only,
        silent,
        source,
        door_yield: None,
        destination,
        placed: false,
        entered: false,
        frame: transition::TransitionFrame {
            black: true,
            ..transition::TransitionFrame::default()
        },
    })
}

/// Parse the record's `.dor` from the pack, falling back to a black screen
/// when the art is missing or invalid.
fn load_door_animation(pack: &Pack, record: &game::Door, entry_camera: u8) -> DoorAnimation {
    let name = door::type_name(record.door_type);
    let path = format!("door/{name}.dor");
    let bytes = match pack.read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("warning: missing door animation {path}: {err}");
            return DoorAnimation::missing();
        }
    };
    let dor = match door::parse(&bytes) {
        Ok(dor) => dor,
        Err(err) => {
            eprintln!("warning: invalid door animation {path}: {err:#}");
            return DoorAnimation::missing();
        }
    };
    let vm = door::vm::Vm::new(
        &dor,
        door::vm::DoorParams {
            direction: record.direction,
            sfx: record.sfx,
            door_type: record.door_type,
            entry_camera,
            dest: record.next_room,
        },
    );
    DoorAnimation::new(dor, vm)
}

/// Render one transition frame: black while the timeline says so, otherwise
/// the door panels over black with the accumulated fade overlay.
fn render_transition(framebuffer: &mut Framebuffer, session: &TransitionMode) {
    framebuffer.clear();
    if session.frame.black {
        return;
    }
    let Some((dor, mut frame)) = session.transition.stepper().render_frame() else {
        return;
    };
    // The timeline owns the accumulated fade; feed it through the door
    // frame's fade fields so the shared overlay path draws it.
    match session.frame.overlay {
        Some(overlay) => {
            frame.fade = door::vm::Fade {
                fade_type: match overlay.color {
                    transition::FadeColor::White => 1,
                    transition::FadeColor::Black => 2,
                },
                counter: 0,
                state: i16::from(overlay.alpha) << 7,
            };
        }
        None => frame.fade.state = -1,
    }
    render::draw_door_scene(framebuffer, dor, &frame);
}

/// Tear the transition down: swap in the destination room and place the
/// player. The interactive session runs the two halves on the frames the
/// original's `room_transition_load` reaches them; the headless drivers run
/// whatever is left when the animation ends.
///
/// The door's close SE (`play_sfx(0, 1, 0)`) belongs to the *end* of the
/// original's `room_transition_load`, after the animation task has been waited
/// out; every caller plays it via [`play_room_close_sfx`] on that frame.
///
/// BGM handoff runs later, once the animation task has been waited out:
/// [`GameSession::end_transition`] for the interactive session, and the
/// headless drivers' own teardown once they reach `frame.finished`.
fn finish_transition(
    session: &mut TransitionMode,
    game: &mut game::GameState,
    player_state: &mut player::PlayerState,
    loaded: &mut LoadedRoom,
) {
    // The original's `room_transition_load` latches the record's sfx byte and
    // reloads `g_RoomSfxBanks` before the destination's init runs; camera-only
    // records reload the pair too, even though the room stays.
    if session.camera_only {
        loaded.room.room_sfx = session.door.sfx;
        game.reset_message_flags_after_door();
        // The original re-tests the current camera's switch zones in place
        // (room_transition_load's camera-only branch calls
        // check_camera_switch(1)); the record's entry camera only feeds the
        // .dor animation and is not itself a room cut.
        apply_camera(&mut loaded.room, game, Some(player_state.pos));
        return;
    }
    place_transition_player(session, game, loaded);
    load_transition_destination(session, game, player_state, loaded);
}

/// The first half of `room_transition_load`'s full load: place the player at
/// the record's entry point, snapshot and tear down the outgoing room's
/// enemies and switch the room id. The destination's `room_set` runs later
/// ([`load_transition_destination`]); a stage change's `init_room` yields a
/// frame in between, which the oracle trace sees.
fn place_transition_player(
    session: &mut TransitionMode,
    game: &mut game::GameState,
    loaded: &mut LoadedRoom,
) {
    if session.camera_only || session.placed {
        return;
    }
    let Some(destination) = session.destination.take() else {
        return;
    };
    session.placed = true;
    // `g_roomCameraId` keeps the source cut until the destination's
    // `room_set` picks its own.
    let source_cut = loaded.room.current_cut;
    *loaded = destination;
    loaded.room.room_sfx = session.door.sfx;
    loaded.room.current_cut = source_cut;
    // The original snapshots the outgoing room's live enemies and tears the
    // list down before the destination's `room_set` wipes the slots. The
    // snapshot ages only when the destination room differs from the source.
    let destination_changed = session.door.next_room as i8 != game.id.room as i8;
    game.snapshot_enemies(destination_changed);
    game.clear_room_entities();
    // The original's door load resets the player's animation id before the
    // destination room boots, so the next update re-initialises state 0 back
    // to the pad-driven state 1 instead of resuming the source room's scripted
    // state 8 behaviour over its stale target. The behaviour and action state
    // keep their source-room values until that update.
    let player = &mut game.entities[0];
    player.set_state(0);
    player.pos = session.door.next_pos;
    player.angle = session.door.next_angle as u16 & 0x0FFF;
}

/// The destination's `room_set`: boot the room state and spawn the player at
/// the placed entry point.
fn load_transition_destination(
    session: &mut TransitionMode,
    game: &mut game::GameState,
    player_state: &mut player::PlayerState,
    loaded: &mut LoadedRoom,
) {
    if session.camera_only || !session.placed || session.entered {
        return;
    }
    session.entered = true;
    game.enter_room(session.target, &loaded.room);
    // `enter_room` keeps the word a door-animation message masked; the door
    // itself zeroed it, so apply the gameplay loop's post-door reset.
    game.reset_message_flags_after_door();
    *player_state = player::spawn(session.target, &loaded.room);
    player_state.pos = session.door.next_pos;
    player_state.collision_mirror = [session.door.next_pos[0], session.door.next_pos[2]];
    player_state.angle = session.door.next_angle as u16 & 0x0FFF;
    // `enter_room` clears the source room's stair state and `spawn` starts the
    // fresh player with a clean climb, so the destination cannot inherit a
    // suspended collision pass. The record's point is placed raw; the first
    // gameplay tick's collision pass pushes it out, exactly like the original
    // (the spawn's `collision_flags` skip the stairwell's shape-5 volume, so a
    // stair arrival is not wedged by it).
    game.sync_entity_from_player(player_state);

    // A freshly loaded room starts at cut 0. The switch-zone scan runs in the
    // gameplay phase after the destination's init (the original's room_set
    // runs the init script before check_camera_switch), because the init can
    // move the player and lock the camera. The record's entry camera only
    // feeds the .dor animation, so it must not seed the room camera.
    loaded.room.current_cut = 0;
    game.camera.current_cut = 0;
}

/// Play the door transition's close SE (the original's `play_sfx(0, 1, 0)`
/// at the end of `room_transition_load`), unless the record suppressed it.
fn play_room_close_sfx(
    music: &mut Option<Mixer>,
    sfx_cache: &mut SfxCache,
    pack: &Pack,
    session: &TransitionMode,
) {
    if !session.silent {
        play_room_sfx(music, sfx_cache, pack, session.door.sfx, 1);
    }
}

/// The result of [`simulate_door`]: destination state plus the frame timeline
/// and two rendered frames for deterministic capture checks.
pub struct SimulatedDoor {
    /// Destination room (the source room for camera-only transitions).
    pub target: RoomId,
    /// Loaded destination room.
    pub room: RoomState,
    /// Game state after the transition.
    pub game: game::GameState,
    /// Player placed at the door's spawn.
    pub player: player::PlayerState,
    /// Animation frame counter when the timeline finished.
    pub frame_count: u32,
    /// `(frame counter, black, overlay alpha)` for every ticked frame.
    pub timeline: Vec<(u32, bool, u8)>,
    /// The first rendered frame (black).
    pub first_frame: Image,
    /// The first non-black frame, if the animation reached one.
    pub mid_frame: Option<Image>,
    /// Index into [`SimulatedDoor::timeline`] of `mid_frame`.
    pub mid_index: Option<usize>,
    /// The destination's first gameplay frame after teardown.
    pub gameplay_frame: Image,
}

/// Drive a door transition headlessly: load `id`, trigger the door in `slot`,
/// tick the timeline to completion and tear it down.
///
/// This is the deterministic seam the real-asset tests and capture tooling
/// use; it renders every frame but opens no audio device. When `frame_dir` is
/// set the first, the first non-black and the first destination gameplay
/// frame are written as BMPs.
pub fn simulate_door(
    pack: &Pack,
    id: RoomId,
    slot: u8,
    frame_dir: Option<&Path>,
) -> Result<SimulatedDoor> {
    let mut loaded = load_room(pack, id)?;
    let mut game = new_game_state(pack, id, &loaded.room);
    let mut player_state = player::spawn(id, &loaded.room);
    run_room_init(&mut loaded, &mut game);
    drain_mask_toggles(&mut loaded.room, &mut game);
    apply_camera(&mut loaded.room, &mut game, None);

    let door = game
        .doors
        .get(usize::from(slot))
        .copied()
        .flatten()
        .with_context(|| format!("room {} has no door in slot {slot}", id.room3()))?;
    // Stand short of the zone so the reach probe lands inside it, then trigger
    // the door (walk-in doors fire on the same call).
    let center_x = i32::from(door.zone[0]) + i32::from(door.zone[2]) / 2;
    let center_z = i32::from(door.zone[1]) + i32::from(door.zone[3]) / 2;
    let (dx, dz) = player::reach_offset(0);
    player_state.pos = [center_x - dx, 0, center_z - dz];
    player_state.angle = 0;
    game.sync_entity_from_player(&player_state);
    game.interact(player_state.pos, player_state.angle, true);
    let request = game
        .transition
        .with_context(|| format!("door {slot} did not request a transition"))?;
    let record = game.transition_door.take().unwrap_or(door);
    game.door_yield = None;

    let mut session = start_transition(
        pack,
        &record,
        &request,
        loaded.id,
        &transition::Fade::inactive(),
    )?;
    let mut mixer: Option<Mixer> = None;
    let mut sfx_cache = SfxCache::default();
    let mut framebuffer = Framebuffer::new();
    let mut timeline = Vec::new();
    let mut first_frame = None;
    let mut mid_frame = None;
    let mut mid_index = None;
    for _ in 0..MAX_TRANSITION_FRAMES {
        let frame = session.transition.tick(false);
        session.frame = frame;
        for effect in session.transition.stepper_mut().take_sfx() {
            play_door_animation_sfx(&mut mixer, &mut sfx_cache, pack, &session.door, effect);
        }
        for message in session.transition.stepper_mut().take_messages() {
            apply_door_message(&mut game, message);
        }
        timeline.push((
            frame.frame,
            frame.black,
            frame.overlay.map_or(0, |overlay| overlay.alpha),
        ));
        render_transition(&mut framebuffer, &session);
        let image = Image {
            width: framebuffer.width,
            height: framebuffer.height,
            rgba: framebuffer.rgba.clone(),
        };
        if first_frame.is_none() {
            first_frame = Some(image.clone());
        }
        if !frame.black && mid_frame.is_none() {
            mid_frame = Some(image);
            mid_index = Some(timeline.len() - 1);
        }
        if frame.finished {
            break;
        }
    }
    let frame_count = session.transition.frame();
    finish_transition(&mut session, &mut game, &mut player_state, &mut loaded);

    // The engine runs the destination's init when gameplay resumes; run it
    // here too so the captured frame matches the first rendered frame (mask
    // groups, camera locks and scripted entity moves included).
    run_room_init(&mut loaded, &mut game);
    game.sync_player(&mut player_state);
    drain_mask_toggles(&mut loaded.room, &mut game);
    apply_camera(&mut loaded.room, &mut game, Some(player_state.pos));
    // `room_transition_load` runs the handoff after the destination's init and
    // camera setup and before the close SE, so the headless seam exposes the
    // same BGM selection as the interactive session.
    bgm::update_room_bgm(&mut game, loaded.id, Some(id));
    play_room_close_sfx(&mut mixer, &mut sfx_cache, pack, &session);

    // The destination's first gameplay frame, drawn from the placed player and
    // the zone-selected cut, exactly as the engine's render loop would.
    let mut gameplay = Framebuffer::new();
    let mut npc_models = enemy::EntityModelCache::default();
    render_frame(
        &mut gameplay,
        pack,
        loaded.id,
        &loaded.room,
        &player_state,
        &mut game,
        loaded.player_assets.as_ref(),
        &mut npc_models,
        &mut MaskCache::default(),
        &mut ShadowCache::default(),
        &mut EffectPageCache::default(),
    );
    let gameplay_frame = Image {
        width: gameplay.width,
        height: gameplay.height,
        rgba: gameplay.rgba,
    };

    if let Some(dir) = frame_dir {
        if let Some(image) = &first_frame {
            let path = dir.join("transition_frame0.bmp");
            bmp::encode(image, &path)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        if let Some(image) = &mid_frame {
            let path = dir.join("transition_mid.bmp");
            bmp::encode(image, &path)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        let path = dir.join("destination_frame0.bmp");
        bmp::encode(&gameplay_frame, &path)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(SimulatedDoor {
        target: session.target,
        room: loaded.room,
        game,
        player: player_state,
        frame_count,
        timeline,
        first_frame: first_frame.unwrap_or_default(),
        mid_frame,
        mid_index,
        gameplay_frame,
    })
}

/// The result of [`simulate_room`]: the state after a fixed number of ticks
/// and the final rendered frame, plus an entity-less baseline of the same
/// state for capture comparisons.
pub struct SimulatedRoom {
    /// Room that was simulated.
    pub id: RoomId,
    /// Loaded room after the ticks.
    pub room: RoomState,
    /// Game state after the ticks.
    pub game: game::GameState,
    /// Player after the ticks.
    pub player: player::PlayerState,
    /// The final gameplay frame with the player and every visible character.
    pub frame: Image,
    /// The final state rendered with every character slot deactivated, so a
    /// test can assert the characters actually painted something.
    pub baseline: Image,
    /// Where the room's SCD scripts were loaded from: the RDT or a standalone
    /// `scd/{id}.scd` override.
    pub script_source: ScriptSource,
    /// Destination rooms a scripted door entered during the run, in order.
    /// The final [`SimulatedRoom::room`] is the last destination.
    pub transitions: Vec<RoomId>,
}

/// Drive a room headlessly for `ticks` fixed ticks and render the final frame.
///
/// This is the deterministic capture seam the real-asset tests use, mirroring
/// [`simulate_door`]: it boots the room's init script, runs the main/event
/// scripts and the native entity driver with `input` held, and renders the
/// final state twice (once normally, once with the character slots hidden).
/// Opening a door is not driven here; a scene that requests one keeps its
/// gameplay frame.
pub fn simulate_room(
    pack: &Pack,
    id: RoomId,
    ticks: usize,
    input: player::Input,
) -> Result<SimulatedRoom> {
    simulate_room_with_input(pack, id, ticks, move |_| input)
}

/// [`simulate_room`] with a per-tick input schedule.
///
/// The schedule is what lets the corpus audit drive action presses and walking
/// without locking the player into one static input for the whole run.
pub fn simulate_room_with_input(
    pack: &Pack,
    id: RoomId,
    ticks: usize,
    input: impl FnMut(usize) -> player::Input,
) -> Result<SimulatedRoom> {
    let loaded = load_room(pack, id)?;
    let mut game = new_game_state(pack, id, &loaded.room);
    seed_room_items(&mut game);
    let player_state = player::spawn(id, &loaded.room);
    game.sync_entity_from_player(&player_state);
    simulate_loaded(pack, loaded, game, player_state, ticks, input)
}

/// [`simulate_room`] with flag bits set before the init script runs, for rooms
/// whose characters and follow states are gated on story flags. Each entry is
/// `(bank, bit)` and is OR-ed into the fresh state.
pub fn simulate_room_seeded(
    pack: &Pack,
    id: RoomId,
    flags: &[(u8, u8)],
    ticks: usize,
    input: player::Input,
) -> Result<SimulatedRoom> {
    let loaded = load_room(pack, id)?;
    let mut game = new_game_state(pack, id, &loaded.room);
    seed_room_items(&mut game);
    for &(bank, bit) in flags {
        game.apply_flag(bank, bit, 0);
    }
    let player_state = player::spawn(id, &loaded.room);
    game.sync_entity_from_player(&player_state);
    simulate_loaded(pack, loaded, game, player_state, ticks, move |_| input)
}

/// [`simulate_room_seeded`] with a setup hook before the first tick: a test
/// can arm the player (equip a weapon, fill the inventory) and then run a
/// per-tick input schedule through the real gameplay tick.
pub fn simulate_room_prepared(
    pack: &Pack,
    id: RoomId,
    flags: &[(u8, u8)],
    ticks: usize,
    prepare: impl FnOnce(&mut game::GameState, &mut player::PlayerState),
    input: impl FnMut(usize) -> player::Input,
) -> Result<SimulatedRoom> {
    let loaded = load_room(pack, id)?;
    let mut game = new_game_state(pack, id, &loaded.room);
    seed_room_items(&mut game);
    for &(bank, bit) in flags {
        game.apply_flag(bank, bit, 0);
    }
    let mut player_state = player::spawn(id, &loaded.room);
    game.sync_entity_from_player(&player_state);
    prepare(&mut game, &mut player_state);
    game.sync_entity_from_player(&player_state);
    simulate_loaded(pack, loaded, game, player_state, ticks, input)
}

/// [`simulate_room_seeded`] with the film hand-off live.
///
/// This is the real-asset seam for the `movie_on` handshake: a request opens a
/// session from the pack and the film freezes the room until it ends, exactly
/// like the engine's gameplay tick. The returned [`MovieHandoff`] records the
/// requested/played ids and the BGM state around the films; a pack that lacks a
/// requested film drains the request instead.
pub fn simulate_room_with_movie(
    pack: &Pack,
    id: RoomId,
    flags: &[(u8, u8)],
    ticks: usize,
    input: impl FnMut(usize) -> player::Input,
) -> Result<SimulatedMovie> {
    let loaded = load_room(pack, id)?;
    let mut game = new_game_state(pack, id, &loaded.room);
    seed_room_items(&mut game);
    for &(bank, bit) in flags {
        game.apply_flag(bank, bit, 0);
    }
    let player_state = player::spawn(id, &loaded.room);
    game.sync_entity_from_player(&player_state);
    let mut handoff = MovieHandoff::default();
    let room = simulate_loaded_with(
        pack,
        true,
        loaded,
        game,
        player_state,
        ticks,
        input,
        &mut handoff,
    )?;
    Ok(SimulatedMovie { room, handoff })
}

/// Render one gameplay frame from an already-built state.
///
/// This is the headless capture seam for effect probes: tests can tick a state
/// by hand (create effects, step [`game::GameState::tick_effects`]) and render
/// it without rebuilding the room. Player and NPC models are loaded from the
/// pack when present. The game is borrowed mutably because the frame's screen
/// shake (when `MSF2_SCREEN_SHAKE` is set) consumes three values from its
/// platform random stream.
pub fn render_game_frame(
    pack: &Pack,
    id: RoomId,
    room: &RoomState,
    game: &mut game::GameState,
    player_state: &player::PlayerState,
) -> Result<Image> {
    let assets = load_player_assets(pack, id);
    let mut npc_models = enemy::EntityModelCache::default();
    let mut framebuffer = Framebuffer::new();
    render_frame(
        &mut framebuffer,
        pack,
        id,
        room,
        player_state,
        &mut *game,
        assets.as_ref(),
        &mut npc_models,
        &mut MaskCache::default(),
        &mut ShadowCache::default(),
        &mut EffectPageCache::default(),
    );
    Ok(Image {
        width: framebuffer.width,
        height: framebuffer.height,
        rgba: framebuffer.rgba,
    })
}

/// Drive the new-game start (the shipped card's room, the main hall) headlessly:
/// the original's start position, facing and seed, then the same
/// init/ticks/render path as [`simulate_room`]. `character` selects Chris (0)
/// or Jill (1).
pub fn simulate_new_game(
    pack: &Pack,
    character: u8,
    ticks: usize,
    input: player::Input,
) -> Result<SimulatedRoom> {
    let character = character & 1;
    let id = new_game_id(pack, character);
    let loaded = load_room(pack, id)?;
    let mut game = new_game_state(pack, id, &loaded.room);
    seed_new_game(pack, &mut game, character);
    let mut player_state = player::spawn(id, &loaded.room);
    player_state.pos = [NEW_GAME_POS_X, 0, NEW_GAME_POS_Z];
    player_state.angle = NEW_GAME_ANGLE;
    game.sync_entity_from_player(&player_state);
    simulate_loaded(pack, loaded, game, player_state, ticks, move |_| input)
}

/// Film hand-off audit for [`simulate_room_with_movie`].
#[derive(Debug, Default, Clone)]
pub struct MovieHandoff {
    /// Film ids the scripts requested, in order.
    pub requested: Vec<u8>,
    /// Films that opened and played to completion.
    pub played: Vec<u8>,
    /// `game.bgm` when the first film started.
    pub bgm_at_start: Option<game::BgmState>,
    /// `game.bgm` when the last film ended.
    pub bgm_at_end: Option<game::BgmState>,
}

/// The result of [`simulate_room_with_movie`]: the room after the ticks plus
/// the film hand-off audit.
pub struct SimulatedMovie {
    /// The room after the ticks.
    pub room: SimulatedRoom,
    /// The film requests the scripts raised and the films that played.
    pub handoff: MovieHandoff,
}

/// The shared body of [`simulate_room`] and [`simulate_new_game`].
///
/// A headless run drains every film request: the request slot is taken and
/// dropped so a `movie_on` can never stall the simulation. The movie-aware
/// variant is [`simulate_room_with_movie`].
fn simulate_loaded(
    pack: &Pack,
    loaded: LoadedRoom,
    game: game::GameState,
    player_state: player::PlayerState,
    ticks: usize,
    input: impl FnMut(usize) -> player::Input,
) -> Result<SimulatedRoom> {
    let mut audit = MovieHandoff::default();
    simulate_loaded_with(
        pack,
        false,
        loaded,
        game,
        player_state,
        ticks,
        input,
        &mut audit,
    )
}

/// [`simulate_loaded`] with the film hand-off live when `play_movies` is set:
/// a request opens a session from the pack and the film freezes the room
/// exactly like the engine's gameplay tick. `audit` records the requests and
/// the BGM state around the films.
#[allow(clippy::too_many_arguments)]
fn simulate_loaded_with(
    pack: &Pack,
    play_movies: bool,
    mut loaded: LoadedRoom,
    mut game: game::GameState,
    mut player_state: player::PlayerState,
    ticks: usize,
    mut input: impl FnMut(usize) -> player::Input,
    audit: &mut MovieHandoff,
) -> Result<SimulatedRoom> {
    let id = loaded.id;
    run_room_init(&mut loaded, &mut game);
    // Match the engine's room boot: the init script's ordered event requests
    // apply before the Lua room hook and the first tick's command pass.
    let scripts = Rc::new(loaded.scripts.clone());
    let mut command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
    let mut event_vm = scd::vm::EventVm::from_scripts(scripts);
    apply_event_requests(&mut game, &mut event_vm);
    let lua = load_lua(pack);
    call_lua_init(lua.as_ref(), &mut game);
    call_lua_room_load(lua.as_ref(), &mut game, id);
    drain_mask_toggles(&mut loaded.room, &mut game);
    apply_camera(&mut loaded.room, &mut game, Some(player_state.pos));
    // Match the engine's room boot: the per-room BGM state machine runs after
    // the init script, so a headless run sees the same channel state.
    bgm::update_room_bgm(&mut game, id, None);

    let mut masks = MaskCache::default();
    let mut shadows = ShadowCache::default();
    let mut effect_pages = EffectPageCache::default();
    let mut npc_models = enemy::EntityModelCache::default();
    let mut enemy_lua = enemy::LuaEnemyHost::new();
    let mut framebuffer = Framebuffer::new();
    // Headless runs have no device: a voice wait clears the tick it is raised,
    // but the BGM state machine, its ramps/fades and the 3D SE dispatch still
    // run after the scripts exactly like the interactive tick.
    let mut voice_cache = VoiceCache::default();
    let mut bgm_cache = bgm::BgmCache::default();
    let mut snd3d_cache = SfxCache::default();
    let mut no_mixer: Option<Mixer> = None;
    let mut film: Option<MovieSession> = None;
    let mut sfx_cache = SfxCache::default();
    let mut door_transition: Option<TransitionMode> = None;
    let mut transitions: Vec<RoomId> = Vec::new();

    for tick in 0..ticks {
        // A film owns the tick while it plays: the room's scripts, entities,
        // effects and the player stay frozen.
        if let Some(session) = film.as_mut() {
            session.tick(0, None);
            if session.finished() {
                audit.played.push(session.id());
                audit.bgm_at_end = Some(game.bgm);
                film = None;
            }
            continue;
        }
        // A door transition owns the tick: run its timeline (audio-free, like
        // the capture loop) and swap the destination in when it finishes.
        if let Some(mut session) = door_transition.take() {
            let frame = session.transition.tick(false);
            session.frame = frame;
            for message in session.transition.stepper_mut().take_messages() {
                apply_door_message(&mut game, message);
            }
            let _ = session.transition.stepper_mut().take_sfx();
            session.transition.set_sound_busy(false);
            if frame.finished {
                let from = loaded.id;
                finish_transition(&mut session, &mut game, &mut player_state, &mut loaded);
                play_room_close_sfx(&mut no_mixer, &mut sfx_cache, pack, &session);
                let scripts = Rc::new(loaded.scripts.clone());
                command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
                event_vm = scd::vm::EventVm::from_scripts(scripts);
                run_room_init(&mut loaded, &mut game);
                apply_event_requests(&mut game, &mut event_vm);
                call_lua_room_load(lua.as_ref(), &mut game, loaded.id);
                drain_mask_toggles(&mut loaded.room, &mut game);
                apply_camera(&mut loaded.room, &mut game, Some(player_state.pos));
                bgm::update_room_bgm(&mut game, loaded.id, Some(from));
                transitions.push(loaded.id);
            } else {
                door_transition = Some(session);
            }
            continue;
        }
        let message_before = game.message.menu_choice_id() & 0x80 != 0;
        let requested = tick_room(
            &mut command_vm,
            &mut event_vm,
            RoomContext {
                room: &mut loaded.room,
                game: &mut game,
                player: &mut player_state,
                player_assets: loaded.player_assets.as_ref(),
                pack,
                npc_models: &mut npc_models,
                enemy_lua: Some(&mut enemy_lua),
            },
            input(tick),
        );
        run_lua_tick_hooks(lua.as_ref(), &mut game, message_before);
        // A scripted door follows into its destination: the interactive engine
        // plays the `.dor` timeline and reboots the destination room.
        if let Some(request) = requested {
            let record = game.transition_door.take().unwrap_or_default();
            game.door_yield = None;
            door_transition = Some(start_transition(
                pack,
                &record,
                &request,
                loaded.id,
                &transition::Fade::inactive(),
            )?);
        }
        play_snd3d_requests(
            &mut no_mixer,
            &mut snd3d_cache,
            pack,
            &loaded.room,
            &mut game,
        );
        bgm::apply_live(&mut no_mixer, &mut game, &mut bgm_cache, pack);
        tick_voice(&mut no_mixer, &mut voice_cache, &mut game, pack);
        // Headless runs feed no input; release a message's menu choice the way
        // an auto-confirm would so a script's F7 cannot stall on it. The
        // scripts read the release through the BioCard state byte, so mirror
        // it.
        let choice = game.message.menu_choice_id();
        game.message.set_menu_choice_id(choice & 0x7F);
        game.sync_message_choice();
        drain_mask_toggles(&mut loaded.room, &mut game);
        // A script's `movie_on` takes over the next tick. A run that does not
        // play films takes and drops the request, so it never stalls.
        if let Some(request) = game.take_fmv_request() {
            audit.requested.push(request);
            if play_movies
                && let Ok(session) = MovieSession::open(pack, request, game.id.player_flag)
            {
                audit.bgm_at_start.get_or_insert(game.bgm);
                film = Some(session);
            }
        }
    }

    render_frame(
        &mut framebuffer,
        pack,
        loaded.id,
        &loaded.room,
        &player_state,
        &mut game,
        loaded.player_assets.as_ref(),
        &mut npc_models,
        &mut masks,
        &mut shadows,
        &mut effect_pages,
    );
    let frame = Image {
        width: framebuffer.width,
        height: framebuffer.height,
        rgba: framebuffer.rgba.clone(),
    };

    let active: Vec<bool> = game.entities[1..]
        .iter()
        .map(game::Entity::active)
        .collect();
    for entity in &mut game.entities[1..] {
        entity.set_active(false);
    }
    let mut baseline_framebuffer = Framebuffer::new();
    render_frame(
        &mut baseline_framebuffer,
        pack,
        loaded.id,
        &loaded.room,
        &player_state,
        &mut game,
        loaded.player_assets.as_ref(),
        &mut npc_models,
        &mut masks,
        &mut shadows,
        &mut effect_pages,
    );
    let baseline = Image {
        width: baseline_framebuffer.width,
        height: baseline_framebuffer.height,
        rgba: baseline_framebuffer.rgba.clone(),
    };
    for (entity, was_active) in game.entities[1..].iter_mut().zip(active) {
        entity.set_active(was_active);
    }

    Ok(SimulatedRoom {
        id: loaded.id,
        room: loaded.room,
        game,
        player: player_state,
        frame,
        baseline,
        script_source: loaded.script_source,
        transitions,
    })
}

/// The result of a headless typewriter save/load round trip.
pub struct SimulatedTypewriter {
    /// Room the typewriter was used in.
    pub room: RoomId,
    /// Zero-based slot the screen wrote.
    pub slot: usize,
    /// Whether the flow consumed an ink ribbon.
    pub ink_ribbon: bool,
    /// The block that was written.
    pub saved: save::SaveFile,
    /// Live state after the typewriter save finished.
    pub state_after_save: game::GameState,
    /// State after rebuilding a session from the written block.
    pub state_after_load: game::GameState,
}

/// Drive a room's typewriter headlessly: add an ink ribbon, position the
/// player at the typewriter, answer the save prompt, run the save screen to
/// completion and rebuild a gameplay session from the written slot.
///
/// This is the deterministic seam the real-asset tests use; it writes the slot
/// through the same [`ui::save_load::SaveLoadScreen`] path the engine uses.
///
/// The seam drives the ported slot screen, so it reads slot 0 back by its
/// fixed name. A pack that registers a Lua save screen names its own file, so
/// this function refuses such a pack rather than guess the name.
pub fn simulate_typewriter(
    pack: &Pack,
    id: RoomId,
    save_dir: &Path,
) -> Result<SimulatedTypewriter> {
    let mut session = GameSession::from_room(pack, id, save_dir)?;
    if let Some(entry) = session.game.save_screen.as_deref() {
        bail!(
            "pack registers the Lua save screen {entry}: simulate_typewriter writes the ported slot 0"
        );
    }
    session.game.add_item(ITEM_KNIFE, 0);
    session.game.add_item(items::ITEM_INK_RIBBONS, 1);
    let action = session
        .game
        .room_actions
        .iter()
        .flatten()
        .find(|action| action.kind == game::RoomActionKind::Typewriter)
        .copied()
        .with_context(|| format!("room {} has no typewriter action", id.room3()))?;
    let center_x = i32::from(action.zone[0]) + i32::from(action.zone[2]) / 2;
    let center_z = i32::from(action.zone[1]) + i32::from(action.zone[3]) / 2;

    // Probe the typewriter, dismiss any startup message, page the prompt and
    // answer its yes/no choice. Action presses alternate with idle ticks so
    // the window and the probe see a fresh edge; the player is re-placed every
    // tick in case a script keeps moving them. The action rect's centre can sit
    // inside the room's furniture, so the tick's collision resolve leaves the
    // placed player on the nearest standable spot; cycle the four facings
    // until one's reach probe covers the rect, then hold it while the prompt
    // resolves.
    let mut prompted = false;
    let mut pressed = true;
    let mut facing = 0u16;
    for tick in 0..40_000 {
        if session.save_screen.is_some() {
            prompted = true;
            break;
        }
        if session.game.typewriter == game::TypewriterFlow::Idle {
            facing = [0u16, 0x400, 0x800, 0xC00][(tick / 16) % 4];
        }
        session.player.pos = [center_x, session.player.pos[1], center_z];
        session.player.angle = facing;
        session.game.sync_entity_from_player(&session.player);
        let input = player::Input {
            action_held: true,
            action_pressed: pressed,
            ..player::Input::default()
        };
        session.tick(pack, UiInput::default(), input, pressed)?;
        pressed = !pressed;
    }
    if !prompted {
        bail!(
            "the typewriter prompt never resolved for room {}",
            id.room3()
        );
    }

    // The confirmation opens the save screen; select the first slot and run
    // the reveal out.
    for step in 0..20_000u32 {
        if session.save_screen.is_none() {
            break;
        }
        let ui = UiInput {
            confirm: step == 0,
            ..UiInput::default()
        };
        session.tick(pack, ui, player::Input::default(), false)?;
    }
    if session.save_screen.is_some() {
        bail!("the typewriter save screen never finished");
    }

    let saved = save::load(save_dir, 0)?;
    let state_after_save = session.game.clone();
    let loaded = GameSession::from_save(pack, &saved, save_dir)?;
    let ink_ribbon = saved.used_item == items::ITEM_INK_RIBBONS;
    Ok(SimulatedTypewriter {
        room: id,
        slot: 0,
        ink_ribbon,
        saved,
        state_after_save,
        state_after_load: loaded.game,
    })
}

/// The result of [`simulate_death`]: the state after the fixed ticks, the
/// final rendered frame, and whether the DIED screen was reached and the
/// sequence returned to the title.
pub struct SimulatedDeath {
    /// Room the death ran in.
    pub id: RoomId,
    /// Game state after the ticks.
    pub game: game::GameState,
    /// Player after the ticks.
    pub player: player::PlayerState,
    /// The final frame: the room during the fall, or the DIED screen.
    pub frame: Image,
    /// The DIED screen opened during the run.
    pub screen_open: bool,
    /// A frame captured while the DIED screen was up (in its reveal/hold
    /// phase), or `None` when the screen never opened.
    pub screen_frame: Option<Image>,
    /// The sequence finished and requested the title.
    pub finished: bool,
}

/// Drive the player's death flow headlessly through the real session tick:
/// kill the player through the shared damage helper, run `ticks` fixed ticks
/// and report whether the DIED screen opened and whether the sequence
/// returned to the title. `hurt` is the damage the killing blow applies.
pub fn simulate_death(pack: &Pack, id: RoomId, ticks: usize, hurt: i16) -> Result<SimulatedDeath> {
    let save_dir = PathBuf::from("saves");
    let mut session = GameSession::from_room(pack, id, &save_dir)?;
    session.game.apply_player_hurt(hurt, 0);
    let mut screen_open = false;
    let mut screen_frame = None;
    let mut screen_ticks = 0usize;
    for _ in 0..ticks {
        session.tick(pack, UiInput::default(), player::Input::default(), false)?;
        if session.death.screen_open() {
            screen_open = true;
            screen_ticks += 1;
            // Capture the reveal once the strip has started drawing.
            if screen_ticks == 120 {
                session.render(pack);
                screen_frame = Some(Image {
                    width: session.framebuffer.width,
                    height: session.framebuffer.height,
                    rgba: session.framebuffer.rgba.clone(),
                });
            }
        }
        if session.death.finished {
            break;
        }
    }
    session.render(pack);
    let frame = Image {
        width: session.framebuffer.width,
        height: session.framebuffer.height,
        rgba: session.framebuffer.rgba.clone(),
    };
    Ok(SimulatedDeath {
        id,
        game: session.game,
        player: session.player,
        frame,
        screen_open,
        screen_frame,
        finished: session.death.finished,
    })
}

/// Apply the mask-group toggles the scripts queued to the current cut.
fn drain_mask_toggles(room: &mut RoomState, game: &mut game::GameState) {
    let Some(cut) = room.cuts.get_mut(room.current_cut) else {
        game.mask_toggles.clear();
        game.sprite_hide = 0;
        return;
    };
    for toggle in game.mask_toggles.drain(..) {
        mask::set_group_active(&mut cut.mask_active, toggle.group, toggle.active);
    }
    // The queued `room_sprite_hide` mask: bits 0..16 each clear the group they
    // name, then the mask is consumed. Bit 0 names no group and is inert.
    if game.sprite_hide != 0 {
        for bit in 0..0x10u8 {
            if game.sprite_hide & (1 << bit) != 0 {
                mask::set_group_active(&mut cut.mask_active, bit, false);
            }
        }
        game.sprite_hide = 0;
    }
}

/// Apply a door animation message to the game's message state.
///
/// It goes through the same request path as every other message, so a window
/// that is already displaying is refused rather than clobbered, and the pause
/// word masks the control flags exactly as the original's
/// `set_message_display` does. `update_message` resolves the encoded bytes on
/// the next gameplay tick (or after the transition).
fn apply_door_message(game: &mut game::GameState, message: door::vm::Message) {
    game.show_message(message.id, message.value);
}

/// The original's one-buffer-per-bank key: the bank table in the high byte and
/// its slot in the low byte, so a repeated cue on one bank restarts that
/// bank's voice while a different bank appends.
fn sfx_bank_key(bank: u8, slot: u8) -> u64 {
    (u64::from(bank) << 8) | u64::from(slot)
}

/// Play the room-SFX `slot` (0 open, 1 close) of the door's SFX pair.
fn play_room_sfx(
    music: &mut Option<Mixer>,
    cache: &mut SfxCache,
    pack: &Pack,
    sfx_id: u8,
    slot: usize,
) {
    let Some(mixer) = music.as_mut() else {
        return;
    };
    let Some(name) = sfx::room_sfx(usize::from(sfx_id), slot) else {
        return;
    };
    if let Some(wav) = cache.load(pack, name) {
        mixer.play_sfx_on_bank(sfx_bank_key(0, slot as u8), wav, 1.0, 0.0);
    }
}

/// The pack SE name for one `.dor` `SFX` op.
///
/// Every shipped door script uses bank 0 (the room SFX pair loaded from the
/// record's `sfx` byte); the global, enemy and character banks are not part of
/// the M5 pack, so those ops resolve to no sound.
fn door_animation_sfx_name(record: &game::Door, effect: door::vm::Sfx) -> Option<&'static str> {
    if effect.bank != 0 {
        return None;
    }
    sfx::room_sfx(usize::from(record.sfx), usize::from(effect.id))
}

/// Turn one `.dor` `SFX` op into a mixer voice.
fn play_door_animation_sfx(
    music: &mut Option<Mixer>,
    cache: &mut SfxCache,
    pack: &Pack,
    record: &game::Door,
    effect: door::vm::Sfx,
) {
    let Some(name) = door_animation_sfx_name(record, effect) else {
        return;
    };
    let Some(mixer) = music.as_mut() else {
        return;
    };
    if let Some(wav) = cache.load(pack, name) {
        mixer.play_sfx_on_bank(sfx_bank_key(0, effect.id), wav, 1.0, 0.0);
    }
}

/// Consume the tick's queued `se_play_3d` requests through the sound banks.
///
/// Each request resolves its bank: a named one-shot loads from the pack and
/// plays with [`sfx::sound_gain_pan`], bank 4 pans and restarts BGM channel 0,
/// and an unloaded bank/absent table entry is recorded in
/// [`game::GameState::snd3d_noops`] for the corpus audit.
fn play_snd3d_requests(
    music: &mut Option<Mixer>,
    cache: &mut SfxCache,
    pack: &Pack,
    room: &RoomState,
    game: &mut game::GameState,
) {
    let requests = std::mem::take(&mut game.snd3d_requests);
    if requests.is_empty() {
        return;
    }
    let character = game.id.player_flag;
    for request in requests {
        // A room without a camera cannot place the sound; drop it without
        // recording a bank no-op.
        let Some(cut) = room.cuts.get(room.current_cut) else {
            continue;
        };
        let pos = match request.pos {
            game::Snd3dPos::Point(pos) => pos,
            // Position type 3 queues no position; the one-shot falls back to
            // the player (documented).
            game::Snd3dPos::None => game.entities[0].pos,
        };
        let play = sfx::play_sfx_3d(
            room,
            character,
            request.bank,
            request.id,
            cut.pos,
            cut.look_at,
            pos,
        );
        if play.bgm {
            if let Some(bank) = game.bgm.channels.first_mut()
                && bank.name.is_some()
            {
                bank.pan = play.raw_pan;
                bank.restart = true;
            }
            continue;
        }
        let Some(name) = play.name else {
            *game
                .snd3d_noops
                .entry((request.bank, request.id))
                .or_insert(0) += 1;
            continue;
        };
        let Some(mixer) = music.as_mut() else {
            continue;
        };
        let Some(wav) = cache.load(pack, name) else {
            continue;
        };
        mixer.play_sfx_on_bank(
            sfx_bank_key(request.bank, request.id),
            wav,
            play.gain,
            play.pan,
        );
    }
}

/// Consume the tick's footstep events: resolve the floor zone's sound and play
/// each as a 3D one-shot through the mixer.
fn play_footsteps(
    music: &mut Option<Mixer>,
    cache: &mut SfxCache,
    pack: &Pack,
    room: &RoomState,
    player_state: &mut player::PlayerState,
    slow: bool,
) {
    let footsteps = player_state.take_footsteps();
    if footsteps.is_empty() {
        return;
    }
    let Some(mixer) = music.as_mut() else {
        return;
    };
    let Some(cut) = room.cuts.get(room.current_cut) else {
        return;
    };
    for footstep in footsteps {
        // The footstep carries its own entity sound type (0 for the walk,
        // turn and backward run, 1 for the forward run) and the effect-zone
        // slow flag shifts the column by -3 (`MSF2_EFFECT_ZONE`).
        let Some(name) = sfx::footstep_sound(room, footstep.pos, footstep.sound_type, slow) else {
            continue;
        };
        let Some(wav) = cache.load(pack, name) else {
            continue;
        };
        let column =
            sfx::entity_sound_column(room, footstep.pos, footstep.sound_type, slow).unwrap_or(0);
        let (gain, pan) = sfx::sound_gain_pan(cut.pos, cut.look_at, footstep.pos);
        mixer.play_sfx_on_bank(sfx_bank_key(2, column), wav, gain, pan);
    }
}

/// The bank and id one queued player cue resolves through.
///
/// The push and ladder cues are room-table columns exactly like the original's
/// `Play3DSnd(2, ...)` calls: the push grunts are ids `0x16`/`0x17`, the
/// vault/ladder step `0x23` (column 35) and the climb end `0x2D` (column 45).
/// The hit-reaction grunts are character-bank ids 0-3 (bank 3).
fn player_sound_bank(id: u16) -> (u8, u8) {
    match id {
        0..=3 => (3, id as u8),
        _ => (2, id as u8),
    }
}

/// Consume the tick's locked-behaviour sound cues through the same bank
/// dispatch as `se_play_3d`.
fn play_player_sounds(
    music: &mut Option<Mixer>,
    cache: &mut SfxCache,
    pack: &Pack,
    room: &RoomState,
    player_state: &mut player::PlayerState,
    character: u8,
    slow: bool,
) {
    let sounds = player_state.take_sounds();
    if sounds.is_empty() {
        return;
    }
    let Some(mixer) = music.as_mut() else {
        return;
    };
    let Some(cut) = room.cuts.get(room.current_cut) else {
        return;
    };
    for sound in sounds {
        if sound.id == player::SE_FOOTSTEP {
            let Some(name) = sfx::footstep_sound(room, sound.pos, 0, slow) else {
                continue;
            };
            let Some(wav) = cache.load(pack, name) else {
                continue;
            };
            let column = sfx::entity_sound_column(room, sound.pos, 0, slow).unwrap_or(0);
            let (gain, pan) = sfx::sound_gain_pan(cut.pos, cut.look_at, sound.pos);
            mixer.play_sfx_on_bank(sfx_bank_key(2, column), wav, gain, pan);
            continue;
        }
        let (bank, id) = player_sound_bank(sound.id);
        // Bank 3 follows the player's character table.
        let play = sfx::play_sfx_3d(room, character, bank, id, cut.pos, cut.look_at, sound.pos);
        let Some(name) = play.name else {
            continue;
        };
        let Some(wav) = cache.load(pack, name) else {
            continue;
        };
        mixer.play_sfx_on_bank(sfx_bank_key(bank, id), wav, play.gain, play.pan);
    }
}

/// Consume the tick's NPC sound cues: resolve each queued room sound and play
/// it as a 3D one-shot through the mixer with the entity's own pan and volume.
/// The queue is always drained so an absent mixer or camera does not let it
/// grow without bound.
fn play_entity_sounds(
    music: &mut Option<Mixer>,
    cache: &mut SfxCache,
    pack: &Pack,
    room: &RoomState,
    sounds: &mut Vec<game::EntitySound>,
) {
    if sounds.is_empty() {
        return;
    }
    let (Some(mixer), Some(cut)) = (music.as_mut(), room.cuts.get(room.current_cut)) else {
        sounds.clear();
        return;
    };
    for sound in sounds.drain(..) {
        let Some(wav) = cache.load(pack, sound.name) else {
            continue;
        };
        let (gain, pan) = sfx::sound_gain_pan(cut.pos, cut.look_at, sound.pos);
        mixer.play_sfx_on_bank(sfx_bank_key(sound.bank, sound.column), wav, gain, pan);
    }
}

/// Poll every queued SDL event into `input`, reporting whether the user quit
/// and accumulating the shift+`,`/`.` camera-cut step.
///
/// Input transitions are latched here rather than sampled from
/// `SDL_GetKeyboardState` later, because the render loop runs at the display's
/// refresh rate while the simulation runs at a fixed 30 Hz: a press that lands
/// between two ticks must still be there when the next tick asks.
///
/// F8 and Alt+Enter change `display`'s window and never reach the game.
fn poll_events(
    event: &mut SDL_Event,
    input: &mut InputState,
    cut_delta: &mut i32,
    mut display: Option<&mut Display>,
) -> bool {
    use controls::{Source, Wheel};
    let mut quit = false;
    while unsafe { SDL_PollEvent(event) } {
        let kind = unsafe { event.r#type };
        if kind == SDL_EVENT_QUIT {
            quit = true;
        } else if kind == SDL_EVENT_KEY_DOWN {
            let key = unsafe { event.key.key };
            let scancode = unsafe { event.key.scancode };
            let modifiers = unsafe { event.key.r#mod };
            let repeat = unsafe { event.key.repeat };
            if let Some(display) = display.as_deref_mut() {
                if scancode == SDL_SCANCODE_F8 {
                    if !repeat {
                        display.step_window();
                    }
                    continue;
                }
                if matches!(scancode, SDL_SCANCODE_RETURN | SDL_SCANCODE_KP_ENTER)
                    && modifiers & SDL_KMOD_ALT != SDL_KMOD_NONE
                {
                    if !repeat {
                        display.toggle_fullscreen();
                    }
                    continue;
                }
            }
            input.key_down(scancode, repeat);
            if modifiers & SDL_KMOD_SHIFT != SDL_KMOD_NONE {
                if key == SDLK_COMMA {
                    *cut_delta -= 1;
                } else if key == SDLK_PERIOD {
                    *cut_delta += 1;
                }
            }
        } else if kind == SDL_EVENT_KEY_UP {
            input.key_up(unsafe { event.key.scancode });
        } else if kind == SDL_EVENT_MOUSE_BUTTON_DOWN {
            input.source_down(0, Source::Mouse(unsafe { event.button.button }), false);
        } else if kind == SDL_EVENT_MOUSE_BUTTON_UP {
            input.source_up(0, Source::Mouse(unsafe { event.button.button }));
        } else if kind == SDL_EVENT_MOUSE_WHEEL {
            // The integer deltas are whole notches (SDL accumulates a
            // touchpad's fractional scroll into them), so smooth scrolling
            // does not storm presses.
            let wheel = unsafe { event.wheel };
            let sign = if wheel.direction == SDL_MOUSEWHEEL_FLIPPED {
                -1
            } else {
                1
            };
            let (x, y) = (wheel.integer_x * sign, wheel.integer_y * sign);
            for (delta, positive, negative) in
                [(y, Wheel::Up, Wheel::Down), (x, Wheel::Right, Wheel::Left)]
            {
                if delta > 0 {
                    input.source_tap(Source::Wheel(positive));
                } else if delta < 0 {
                    input.source_tap(Source::Wheel(negative));
                }
            }
        } else if kind == SDL_EVENT_GAMEPAD_ADDED {
            input.pads.open(unsafe { event.gdevice.which });
        } else if kind == SDL_EVENT_GAMEPAD_REMOVED {
            let which = unsafe { event.gdevice.which };
            input.pads.close(which);
            input.device_removed(which.0);
        } else if kind == SDL_EVENT_GAMEPAD_BUTTON_DOWN {
            let button = unsafe { event.gbutton };
            input.source_down(
                button.which.0,
                Source::PadButton(i32::from(button.button)),
                false,
            );
        } else if kind == SDL_EVENT_GAMEPAD_BUTTON_UP {
            let button = unsafe { event.gbutton };
            input.source_up(button.which.0, Source::PadButton(i32::from(button.button)));
        } else if kind == SDL_EVENT_GAMEPAD_AXIS_MOTION {
            let axis = unsafe { event.gaxis };
            input.axis_motion(axis.which.0, i32::from(axis.axis), axis.value);
        } else if kind == SDL_EVENT_WINDOW_DISPLAY_CHANGED {
            if let Some(display) = display.as_deref_mut() {
                display.display_changed();
            }
        } else if kind == SDL_EVENT_WINDOW_FOCUS_LOST {
            // Focusing away never delivers the held inputs' release events;
            // drop them so nothing sticks down.
            input.clear();
        } else if kind == SDL_EVENT_KEYBOARD_REMOVED {
            // Nor does unplugging a keyboard (which some gamepads also
            // expose); the pads' own held inputs are still good.
            input.device_removed(0);
        }
    }
    quit
}

/// The gamepads open for input, by joystick instance id.
///
/// SDL reports every pad already connected as an `SDL_EVENT_GAMEPAD_ADDED`
/// once the gamepad subsystem starts, and later ones as they are plugged in,
/// so opening on that event covers both start-up and hot-plug.
#[derive(Default)]
struct Gamepads {
    open: Vec<(SDL_JoystickID, *mut SDL_Gamepad)>,
}

impl Gamepads {
    /// Start the gamepad subsystem for an interactive window: SDL's built-in
    /// mapping database, `SDL_GAMECONTROLLERCONFIG`, plus an optional
    /// `gamecontrollerdb.txt` beside `arklay.toml`. A failure only costs the
    /// pads; the keyboard and mouse still work.
    fn init() {
        if !unsafe { SDL_InitSubSystem(SDL_INIT_GAMEPAD) } {
            eprintln!("warning: gamepads unavailable: {}", sdl_error());
            return;
        }
        let db = crate::config::Config::dir().join("gamecontrollerdb.txt");
        if db.is_file()
            && let Ok(path) = std::ffi::CString::new(db.as_os_str().as_encoded_bytes())
            && unsafe { SDL_AddGamepadMappingsFromFile(path.as_ptr()) } < 0
        {
            eprintln!("warning: failed to read {}: {}", db.display(), sdl_error());
        }
    }

    fn open(&mut self, which: SDL_JoystickID) {
        if self.open.iter().any(|&(id, _)| id == which) {
            return;
        }
        let pad = unsafe { SDL_OpenGamepad(which) };
        if pad.is_null() {
            eprintln!("warning: failed to open gamepad: {}", sdl_error());
        } else {
            self.open.push((which, pad));
        }
    }

    fn close(&mut self, which: SDL_JoystickID) {
        self.open.retain(|&(id, pad)| {
            if id == which {
                unsafe { SDL_CloseGamepad(pad) };
            }
            id != which
        });
    }
}

impl Drop for Gamepads {
    fn drop(&mut self) {
        for (_, pad) in self.open.drain(..) {
            unsafe { SDL_CloseGamepad(pad) };
        }
    }
}

/// Decoded WAV cache for one-shot sound effects, keyed by pack path.
#[derive(Default)]
struct SfxCache {
    wavs: HashMap<String, audio::Wav>,
    missing: HashSet<String>,
}

impl SfxCache {
    /// Load and parse `se/{name}.wav` (lower-case) from the pack, caching both
    /// hits and misses.
    fn load(&mut self, pack: &Pack, name: &str) -> Option<audio::Wav> {
        let stem = format!("se/{}", name.to_ascii_lowercase());
        if let Some(wav) = self.wavs.get(&stem) {
            return Some(wav.clone());
        }
        if self.missing.contains(&stem) {
            return None;
        }
        let asset = match pack.read_asset(&stem, AssetClass::Audio) {
            Ok(Some(asset)) => asset,
            Ok(None) => {
                eprintln!("warning: missing sound effect {stem}.wav");
                self.missing.insert(stem);
                return None;
            }
            Err(err) => {
                eprintln!("warning: unreadable sound effect {stem}: {err:#}");
                self.missing.insert(stem);
                return None;
            }
        };
        match assets::decode_audio(&asset) {
            Ok(wav) => {
                self.wavs.insert(stem, wav.clone());
                Some(wav)
            }
            Err(err) => {
                eprintln!("warning: invalid sound effect {}: {err:#}", asset.path());
                self.missing.insert(stem);
                None
            }
        }
    }
}

/// One tick's pad word for the film skip check: every reachable accept button
/// becomes a distinct non-zero bit inside the original's `0x0FFF` mask, so an
/// unmasked edge ends a skippable film.
///
/// The port's keyboard has no L2/R2 keys; the run key shares the cancel/run
/// button's bit, matching the original's B/Circle run binding.
fn movie_buttons(ui: UiInput, input: player::Input) -> u16 {
    let mut word = 0u16;
    if ui.confirm || input.action_held || input.action_pressed {
        word |= 0x0001;
    }
    if ui.cancel || input.run {
        word |= 0x0002;
    }
    if ui.start {
        word |= 0x0008;
    }
    if ui.up {
        word |= 0x0010;
    }
    if ui.down {
        word |= 0x0020;
    }
    if ui.left {
        word |= 0x0040;
    }
    if ui.right {
        word |= 0x0080;
    }
    if ui.page_left {
        word |= 0x0400;
    }
    if ui.page_right {
        word |= 0x0800;
    }
    word
}

/// [`movie_buttons`] over the raw held keyboard word.
///
/// The original feeds the raw held pad into the film state machine, which
/// does the edge detection itself, so the between-tick poll must not use the
/// latched press edges: a tap held across one poll and released before the
/// next would otherwise be missed. Presses not yet consumed by a tick are
/// included so a quick tap still reaches the poll.
fn movie_buttons_held(active: u32) -> u16 {
    let mut word = 0u16;
    if active & KEY_CONFIRM != 0 {
        word |= 0x0001;
    }
    if active & (KEY_CANCEL | KEY_RUN) != 0 {
        word |= 0x0002;
    }
    if active & KEY_START != 0 {
        word |= 0x0008;
    }
    if active & KEY_UP != 0 {
        word |= 0x0010;
    }
    if active & KEY_DOWN != 0 {
        word |= 0x0020;
    }
    if active & KEY_LEFT != 0 {
        word |= 0x0040;
    }
    if active & KEY_RIGHT != 0 {
        word |= 0x0080;
    }
    if active & KEY_PAGE_LEFT != 0 {
        word |= 0x0400;
    }
    if active & KEY_PAGE_RIGHT != 0 {
        word |= 0x0800;
    }
    word
}

/// Decoded voice-line cache plus the one-slot pending request the scripts
/// queue while a line is already sounding.
#[derive(Default)]
struct VoiceCache {
    wavs: HashMap<String, audio::Wav>,
    missing: HashSet<String>,
    /// The newest request the engine has not started yet.
    pending: Option<game::VoiceRequest>,
    /// The mixer's voice channel is held by a line this cache started.
    active: bool,
}

impl VoiceCache {
    /// Load and parse the entry `path` resolves to, caching hits and misses.
    ///
    /// The request names the shipped `.wav` entry; the lookup is by stem, so a
    /// mod may replace a line with OGG Vorbis.
    fn load(&mut self, pack: &Pack, path: &str) -> Option<audio::Wav> {
        let key = assets::stem(path).to_owned();
        if let Some(wav) = self.wavs.get(&key) {
            return Some(wav.clone());
        }
        if self.missing.contains(&key) {
            return None;
        }
        let asset = match pack.read_asset(&key, AssetClass::Audio) {
            Ok(Some(asset)) => asset,
            Ok(None) => {
                eprintln!("warning: missing voice line {path}");
                self.missing.insert(key);
                return None;
            }
            Err(err) => {
                eprintln!("warning: unreadable voice line {path}: {err:#}");
                self.missing.insert(key);
                return None;
            }
        };
        match assets::decode_audio(&asset) {
            Ok(wav) => {
                self.wavs.insert(key, wav.clone());
                Some(wav)
            }
            Err(err) => {
                eprintln!("warning: invalid voice line {}: {err:#}", asset.path());
                self.missing.insert(key);
                None
            }
        }
    }
}

/// One tick's voice handshake: queue the script's request, clear the wait flag
/// when the active line ends, and start a pending line when the channel frees
/// up.
///
/// With no audio device the flag is cleared every tick, so a headless run
/// (`--capture`, the corpus audit) advances its scripts immediately instead of
/// hanging on F7. A line whose entry is absent from the pack clears the flag
/// the same way (the documented hardening).
fn tick_voice(
    music: &mut Option<Mixer>,
    cache: &mut VoiceCache,
    game: &mut game::GameState,
    pack: &Pack,
) {
    let Some(mixer) = music.as_mut() else {
        game.voice.request = None;
        cache.pending = None;
        cache.active = false;
        game.voice.stop_requested = false;
        game.clear_voice_playing();
        return;
    };

    // A stop first drops whatever was pending or sounding; a line the script
    // queued after the stop (same tick) is taken below and survives.
    if game.voice.stop_requested {
        game.voice.stop_requested = false;
        mixer.stop_voice();
        cache.pending = None;
        cache.active = false;
        game.clear_voice_playing();
    }
    if let Some(request) = game.voice.request.take() {
        cache.pending = Some(request);
    }

    if cache.active && !mixer.voice_playing() {
        cache.active = false;
        game.clear_voice_playing();
    }

    if !cache.active
        && !mixer.voice_playing()
        && let Some(request) = cache.pending.take()
    {
        match cache.load(pack, &request.path) {
            Some(wav) => {
                let gain = sfx::volume_gain(request.volume);
                let pan = sfx::pan_from_raw(request.pan);
                mixer.play_voice(wav, gain, pan);
                cache.active = true;
                game.set_voice_playing();
            }
            None => {
                // The requested entry is absent from the pack: count the miss
                // and release the wait so the script cannot deadlock.
                game.voice.misses += 1;
                game.clear_voice_playing();
            }
        }
    }
}

/// Decoded room-mask pages for the current room, keyed by camera.
#[derive(Default)]
struct MaskCache {
    room: Option<RoomId>,
    pages: HashMap<usize, Image>,
    missing: HashSet<usize>,
}

impl MaskCache {
    /// The mask page of `camera`, loading the `roommask/{room3}_{camera:03}`
    /// stem from the pack on first use. Missing pages are remembered as
    /// absent.
    fn page_for(&mut self, pack: &Pack, id: RoomId, camera: usize) -> Option<&Image> {
        if self.room != Some(id) {
            self.room = Some(id);
            self.pages.clear();
            self.missing.clear();
        }
        if !self.pages.contains_key(&camera) && !self.missing.contains(&camera) {
            let stem = id.roommask_stem(camera);
            // A mask page is addressed by the cut's sprite UVs, so a
            // replacement keeps the shipped page's dimensions.
            match pack.read_asset(&stem, AssetClass::Image) {
                Ok(Some(asset)) => {
                    match assets::decode_picture(&asset, assets::Geometry::Shipped, Some([0, 0, 0]))
                    {
                        Ok(image) => {
                            self.pages.insert(camera, image);
                        }
                        Err(err) => {
                            eprintln!("warning: invalid room mask {}: {err:#}", asset.path());
                            self.missing.insert(camera);
                        }
                    }
                }
                _ => {
                    self.missing.insert(camera);
                }
            }
        }
        self.pages.get(&camera)
    }
}

/// The shared player-shadow coverage page, decoded from the pack once.
///
/// Unlike the mask pages this page is camera-independent, so it is cached for
/// the whole session; a pack without the entry simply never draws a shadow.
#[derive(Default)]
struct ShadowCache {
    page: Option<Image>,
    attempted: bool,
}

impl ShadowCache {
    /// The baked shadow page, loading the `shadow/kage` stem on first use.
    fn page_for(&mut self, pack: &Pack) -> Option<&Image> {
        if !self.attempted {
            self.attempted = true;
            if let Ok(Some(asset)) = pack.read_asset(shadow::KAGE_STEM, AssetClass::Image) {
                match assets::decode_page(&asset, assets::Geometry::Shipped)
                    .and_then(|art| shadow::bake(&art))
                {
                    Ok(image) => self.page = Some(image),
                    Err(err) => {
                        eprintln!("warning: invalid {}: {err:#}", asset.path());
                    }
                }
            }
        }
        self.page.as_ref()
    }
}

/// The decoded effect texture pages of one room.
///
/// `base` holds the four sheet files (`esp000` plus the room's up to three
/// `esp2xx` variants) with the room's embedded sprite TIMs composited at their
/// packed positions. `variants` holds the CLUT-row variants (`page * 3 +
/// (row - 1)`) baked from those same TIMs.
#[derive(Default)]
struct EffectPages {
    base: [Option<Image>; 4],
    variants: [Option<Image>; 12],
}

/// Decoded effect pages for the current room, keyed by room id.
///
/// Unlike [`MaskCache`] this cache composites on load: the four sheet files
/// are decoded once and every declared sprite's embedded TIM is blitted over
/// its packed page and V offset, so sampling a page-absolute UV reads the
/// room's own art.
#[derive(Default)]
struct EffectPageCache {
    room: Option<RoomId>,
    pages: EffectPages,
}

impl EffectPageCache {
    /// The room's pages, rebuilding them when the room changes.
    fn pages_for(&mut self, pack: &Pack, id: RoomId, room: &effects::RoomEffects) -> &EffectPages {
        if self.room != Some(id) {
            self.room = Some(id);
            self.pages = build_effect_pages(pack, id, room);
        }
        &self.pages
    }
}

/// A transparent 256x256 page buffer.
fn transparent_effect_page() -> Image {
    Image {
        width: 256,
        height: 256,
        rgba: vec![0; 256 * 256 * 4],
    }
}

/// Blit one room sprite's embedded TIM into a page at `(0, v)` using palette
/// row `row`; entry zero stays transparent.
fn blit_effect_sprite(page: &mut Image, texture: &crate::model::Texture8, v: u32, row: usize) {
    let Some(width) = usize::try_from(texture.width).ok() else {
        return;
    };
    if width == 0 || texture.height == 0 {
        return;
    }
    for y in 0..texture.height {
        let target_y = v + y;
        if target_y >= page.height {
            break;
        }
        for x in 0..width {
            let Some(&index) = texture.indices.get(y as usize * width + x) else {
                continue;
            };
            if index == 0 {
                continue;
            }
            let texel = texture.palette(row, index);
            let target = ((target_y * page.width) as usize + x) * 4;
            if let Some(pixel) = page.rgba.get_mut(target..target + 4) {
                pixel.copy_from_slice(&[texel[0], texel[1], texel[2], 255]);
            }
        }
    }
}

/// Decode and composite the room's effect pages.
fn build_effect_pages(pack: &Pack, id: RoomId, room: &effects::RoomEffects) -> EffectPages {
    let stage = usize::from(id.stage_index());
    let room_byte = usize::from(id.room);
    let mut pages = EffectPages::default();

    for page in 0..4 {
        let Some(name) = effects::pages::room_effect_sheet(stage, room_byte, page) else {
            continue;
        };
        let stem = format!("effspr/{name}");
        // The RDT's sprite TIMs composite onto the sheet at fixed V offsets,
        // so a replacement keeps the shipped sheet's dimensions.
        match pack.read_asset(&stem, AssetClass::Image) {
            Ok(Some(asset)) => match assets::decode_page(&asset, assets::Geometry::Shipped) {
                Ok(art) => pages.base[page] = Some(art.to_rgba()),
                Err(err) => eprintln!("warning: invalid effect page {}: {err:#}", asset.path()),
            },
            Ok(None) => {}
            Err(err) => eprintln!("warning: unreadable effect page {stem}: {err:#}"),
        }
    }

    for sprite in &room.sprites {
        let page = usize::from(sprite.info.page_index());
        if page >= 4 {
            continue;
        }
        let Some(texture) = &sprite.tim else {
            continue;
        };
        if texture.width == 0 || texture.height == 0 {
            continue;
        }
        let v = u32::from(sprite.info.page_v);
        let base = pages.base[page].get_or_insert_with(transparent_effect_page);
        blit_effect_sprite(base, texture, v, 0);
        for row in 1..=3u8 {
            if sprite.geometry.clut_rows <= row {
                break;
            }
            let index = page * 3 + usize::from(row - 1);
            let variant = pages.variants[index].get_or_insert_with(transparent_effect_page);
            blit_effect_sprite(variant, texture, v, usize::from(row));
        }
    }

    pages
}

/// The guardhouse rooms that force every effect blend opaque.
const GUARDHOUSE_FORCED_OPAQUE_ROOMS: [u8; 3] = [0x0E, 0x0F, 0x11];
/// Stage digit of the laboratory (the main-lab camera-5 quirk).
const STAGE_DIGIT_LABORATORY: u8 = 5;
/// Room byte of the laboratory's main hall.
const ROOM_MAIN_LAB: u8 = 0x13;
/// Stage digit of the second mansion return (the lesson-room V remap).
const STAGE_DIGIT_MANSION_RETURN_2F: u8 = 7;
/// Room byte of the lesson room.
const ROOM_LESSON: u8 = 0x0C;

/// Build this frame's effect billboards from the live pool.
///
/// Slots are visited 63 down to 0, the original's submission order, so equal
/// depth keys resolve in the stable sort exactly like the original's ordering
/// table. Each billboard is drawn between the previous tick's rectangle and
/// this one's by `tween` (the shake is added after the blend, since it
/// snaps).
fn build_effect_quads(
    game: &game::GameState,
    room: &RoomState,
    id: RoomId,
    camera: &Camera,
    pages: &EffectPages,
    tween: &mut Tween,
) -> Vec<EffectQuad> {
    let mut quads = Vec::new();
    let stage = usize::from(id.stage_index());
    let folded = if stage > 4 { stage - 5 } else { stage };
    let lesson_room = id.stage == STAGE_DIGIT_MANSION_RETURN_2F && id.room == ROOM_LESSON;
    let lab_special =
        id.stage == STAGE_DIGIT_LABORATORY && id.room == ROOM_MAIN_LAB && room.current_cut == 5;

    for index in (0..effects::EFFECT_POOL_SIZE).rev() {
        let Some(effect) = game.effects.slot(index) else {
            continue;
        };
        if effect.anim_id == 0 || effects::behaviour::hidden(effect) {
            continue;
        }
        if (effect.proj_depth as u32) & 0xFFFFFFF0 > effects::behaviour::PROJECTED_DEPTH_CULL as u32
        {
            continue;
        }
        if !effects::behaviour::in_switch_zone(
            room,
            room.current_cut,
            i32::from(effect.pos[0]),
            i32::from(effect.pos[2]),
        ) {
            continue;
        }
        let Some(sprite_type) = effect.sprite else {
            continue;
        };
        let room_sprite = game.room_effects.sprite(sprite_type);
        let (sprite, page, region_v, room_art) = if let Some(sprite) = room_sprite {
            (sprite, usize::from(sprite.info.page_index()), 0u32, true)
        } else if let Some(sprite) = game.weapon_effects.sprite(sprite_type) {
            let Some(slot) = game.weapon_effects.slot_of(sprite_type) else {
                continue;
            };
            let Some((page, region_v)) = effects::pages::weapon_sheet_region(slot) else {
                continue;
            };
            (sprite, usize::from(page), u32::from(region_v), false)
        } else {
            continue;
        };
        if page >= 4 {
            continue;
        }

        let mut tint = usize::from(effect.depth_group >> 3);
        let mut tex_v = u32::from(effect.uv[1]) + region_v;
        if lesson_room
            && matches!(tint, 1 | 2)
            && tex_v > 0x1A
            && tex_v + u32::from(effect.size[1]) < 99
        {
            tex_v += 0x7B;
            tint = 0;
        }
        let tex_v = tex_v as u8;

        let Some(sheet) =
            effects::pages::room_effect_sheet_index(stage, usize::from(id.room), page)
        else {
            continue;
        };
        let Some((mut blend, color_idx, _, _)) =
            effects::pages::blend_record(usize::from(sheet), tex_v)
        else {
            continue;
        };
        if id.stage == 4 && GUARDHOUSE_FORCED_OPAQUE_ROOMS.contains(&id.room) {
            blend = 0;
        }

        let Some(record) = effects::pages::EFFECT_COLOR_RECORDS.get(usize::from(color_idx)) else {
            continue;
        };
        let level = if record.count <= tint { 0 } else { tint };
        let tint = if room_art {
            [0xFF, 0xFF, 0xFF]
        } else {
            record.level(level)
        };

        let Some(quad_page) = select_effect_page(pages, page, room_art, sprite, effect.depth_group)
        else {
            continue;
        };
        let width = u32::from(effect.size[0]);
        let height = u32::from(effect.size[1]);
        if width == 0 || height == 0 {
            continue;
        }

        let light_value = u32::try_from(camera.fov).unwrap_or(0);
        let product = light_value
            .wrapping_mul(u32::from(effect.light_factor))
            .wrapping_mul(width)
            .wrapping_mul(0x100);
        let divisor = u32::from(effect.depth_scaled as u16) + 1;
        let scale = product / divisor.max(1);
        let scale = if scale >= 0x8000 {
            -0x8000
        } else {
            scale as i32
        };

        // The UV record's pivot is authored as a centre offset; the original
        // turns it into a top-left distance with `0x80 - pivot`. The frame's
        // screen offset (the shake) joins every sprite position.
        let pivot_x = 0x80 - i32::from(effect.uv[2]);
        let pivot_y = 0x80 - i32::from(effect.uv[3]);
        let screen_x = i32::from(effect.screen[0]) + camera.screen[0];
        let screen_y = i32::from(effect.screen[1]) + camera.screen[1];
        let left = screen_x - sar12(pivot_x * scale);
        let top = screen_y - sar12(pivot_y * scale);
        let right = screen_x + sar12((width as i32 - pivot_x) * scale);
        let bottom = screen_y + sar12((height as i32 - pivot_y) * scale);

        let light_record =
            effects::pages::camera_light_record(folded, usize::from(id.room), room.current_cut);
        let scale_y = if lab_special {
            0
        } else {
            i32::from(light_record[1])
        };
        let key = ((effect.proj_depth as u32) >> 4)
            .wrapping_mul(0x40)
            .wrapping_sub(scale_y as u32);

        let uv = [effect.uv[0], tex_v, effect.uv[2], effect.uv[3]];
        let mut pos = [left - camera.screen[0], top - camera.screen[1]];
        let mut size = [right - left, bottom - top];
        let spawn = [
            i32::from(effect.anim_id) << 8 | i32::from(effect.effect_type),
            effect.spawn_pos[0],
            effect.spawn_pos[1],
            effect.spawn_pos[2],
        ];
        tween.billboard(index, spawn, uv, &mut pos, &mut size);
        quads.push(EffectQuad {
            key,
            page: quad_page,
            uv,
            source: effect.size,
            pos: [pos[0] + camera.screen[0], pos[1] + camera.screen[1]],
            size,
            tint,
            blend,
        });
    }
    quads
}

/// `(value + correction) >> 12`, truncating toward zero.
fn sar12(value: i32) -> i32 {
    (value + ((value >> 31) & 0xFFF)) >> 12
}

/// The layer page index for a billboard, applying the room CLUT-row variant
/// redirect and its row-by-row fallback.
fn select_effect_page(
    pages: &EffectPages,
    page: usize,
    room_art: bool,
    sprite: &effects::EffectSprite,
    depth_group: u8,
) -> Option<usize> {
    if room_art && sprite.geometry.clut_rows > 1 {
        let rows = usize::from(sprite.geometry.clut_rows);
        let mut row = usize::from(depth_group >> 3).min(rows - 1).min(3);
        while row > 1 && pages.variants[page * 3 + (row - 1)].is_none() {
            row -= 1;
        }
        if row > 0 && pages.variants[page * 3 + (row - 1)].is_some() {
            return Some(4 + page * 3 + (row - 1));
        }
    }
    pages.base[page].as_ref().map(|_| page)
}

/// Where a room's SCD scripts were loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptSource {
    /// The scripts embedded in the room's RDT.
    Rdt,
    /// A standalone `scd/{id}.scd` override from a layered pack.
    Override,
}

/// Everything loaded for one room, so a transition can load the next room with
/// the same code path as the initial load.
struct LoadedRoom {
    id: RoomId,
    room: RoomState,
    scripts: scd::ir::Scripts,
    script_source: ScriptSource,
    player_assets: Option<PlayerAssets>,
}

/// Load `id` from `pack`: RDT, camera backgrounds, SCD scripts and player
/// assets.
///
/// A layered pack's `scd/{id}.scd` standalone container overrides the RDT's
/// embedded scripts; without one the RDT bytes are parsed exactly as before.
fn load_room(pack: &Pack, id: RoomId) -> Result<LoadedRoom> {
    let rdt_bytes = &*pack.read(&id.rdt_entry())?;
    let mut room = rdt::parse(rdt_bytes, id)?;
    for warning in &room.model_warnings {
        eprintln!("warning: {}: {warning}", id.rdt_entry());
    }
    if room.cuts.is_empty() {
        bail!("room {} has no camera cuts", id.room3());
    }
    for cut in &mut room.cuts {
        let stem = id.cut_stem(cut.index);
        let asset = pack
            .read_asset(&stem, AssetClass::Image)?
            .with_context(|| format!("missing background for cut {}", cut.index))?;
        cut.background = Some(
            assets::decode_picture(&asset, assets::Geometry::Free, None)
                .with_context(|| format!("invalid {}", asset.path()))?,
        );
    }
    let (scripts, script_source) = match pack.read(&id.scd_entry()) {
        Ok(bytes) => (
            scd::reader::parse(&bytes)
                .with_context(|| format!("invalid SCD override {}", id.scd_entry()))?,
            ScriptSource::Override,
        ),
        Err(_) => (
            scd::reader::parse(rdt_bytes).context("invalid room SCD scripts")?,
            ScriptSource::Rdt,
        ),
    };
    let player_assets = load_player_assets(pack, id);
    Ok(LoadedRoom {
        id,
        room,
        scripts,
        script_source,
        player_assets,
    })
}

/// Build a fresh game state for `id`/`room` with the pack's global weapon-FX
/// metadata installed, so script effect spawns resolve both the room's
/// declared sprites and the `core00` types.
fn new_game_state(pack: &Pack, id: RoomId, room: &RoomState) -> game::GameState {
    let mut game = game::GameState::new(id, room);
    game.set_combat_tables(crate::combat::CombatTables::load(pack));
    let weapon = effects::WeaponEffects::load(pack);
    if pack.contains(effects::room::CORE_ESP_ENTRY) || pack.contains(effects::room::CORE_ETM_ENTRY)
    {
        for warning in &weapon.warnings {
            eprintln!("warning: {warning}");
        }
    }
    game.set_weapon_effects(weapon);
    // Pack the room's sprites once against the installed weapon sheets. The
    // `GameState::new` pass above used the empty default table, so resolving
    // from the freshly parsed room data here leaves the UV V bias applied
    // exactly once.
    game.resolve_room_effects(room);
    game
}

/// Run a room's init script against `game`, then apply the room edits it
/// queued (`inst_cfg`/`obj_xfm`) before anything reads the room.
fn run_room_init(loaded: &mut LoadedRoom, game: &mut game::GameState) {
    let mut command_vm = scd::vm::CommandVm::new(&loaded.scripts);
    let mut host = game::ScdGameHost::new(game);
    command_vm.run_init(&mut host);
    game.apply_room_edits(&mut loaded.room);
}

/// Apply the command pass's `evt_exec` starts and `task_kill` kills in
/// program order.
///
/// The ordered queue is what keeps `task_kill 6` followed by
/// `evt_exec 0, 6, ...` working (the original's immediate table writes leave
/// the new event running); applying all starts before all kills would kill it.
fn apply_event_requests(game: &mut game::GameState, event_vm: &mut scd::vm::EventVm) {
    for request in game.pending_event_requests.drain(..) {
        event_vm.apply(request);
    }
}

/// The mutable state one simulated tick works on.
struct RoomContext<'a> {
    room: &'a mut RoomState,
    game: &'a mut game::GameState,
    player: &'a mut player::PlayerState,
    player_assets: Option<&'a PlayerAssets>,
    /// Pack the entity model cache and the enemy scripts read from.
    pack: &'a Pack,
    /// Parsed entity models, shared with the renderer.
    npc_models: &'a mut enemy::EntityModelCache,
    /// The enemy-script VM, when the caller owns one. A test fixture may pass
    /// `None`, which parks every monster slot.
    enemy_lua: Option<&'a mut enemy::LuaEnemyHost>,
}

/// The pad-driven half of the player's frame: the control gate has passed, so
/// the action-key scan, the stair/ladder zone branch and then the locomotion
/// (or weapon) machine run in the original's order.
fn tick_pad_driven_player(context: &mut RoomContext<'_>, pad: player::Input) {
    // `player_input_to_behavior`'s action-key scan runs at the top of the
    // player's frame-0 dispatch, before the behaviour it selects executes on
    // the same tick (`update_player_anim`, 0x00480ebd). `pad` is already
    // blanked for a closed controls latch, and the message gate is open here.
    if pad.action_pressed {
        context.game.player_action_press(context.player);
        // `check_action_object` follows the climb scan in the same dispatch,
        // still from the frame-start position, and only while no locked or
        // weapon behaviour owns the frame-1 dispatch.
        if context.player.locked == player::LockedAction::None
            && context.player.weapon.behavior == 0
        {
            context
                .game
                .check_action_object(context.player.pos, context.player.angle);
            // A gated press queued its reach: the 0x0c behaviour owns this
            // tick.
            if let Some(request) = context.game.take_pending_reach() {
                context.player.begin_interact(request);
            }
        }
    }
    // The zone branch: a zone the press just marked, or one a script or the
    // previous frame's probe raised, starts the stairway or ladder before the
    // locomotion would run.
    if context.player.weapon.behavior == 0 {
        context
            .game
            .player_zone_behavior(context.player, pad.action_pressed);
    }
    if let Some(assets) = context.player_assets {
        run_player_machine(context, assets, pad);
    }
}

/// The locomotion and weapon machine, which needs the player's model assets.
fn run_player_machine(context: &mut RoomContext<'_>, assets: &PlayerAssets, pad: player::Input) {
    // A scripted state can hand the player back mid-settle: `action_state`
    // already 1, the settle's pose words written on the entity. The
    // reference's idle then skips its case-0 entry and the settle's first
    // `Joint_move` snaps the body clip straight to the written frame with the
    // written blend counter. Adopt the words before the pad-driven machine's
    // entry check runs.
    if std::mem::take(&mut context.player.scripted_hold) {
        context
            .player
            .adopt_scripted_handback(&context.game.entities[0]);
    }
    let room_clips = context
        .room
        .room_anim
        .as_ref()
        .map(|anim| anim.clips.as_slice())
        .unwrap_or(&[]);
    // The active weapon's bank: the machine's loaded file while it runs,
    // otherwise the equipped item's, so the frame the aim starts reads the
    // right clips.
    let weapon_file = context.player.weapon.weapon_file.or_else(|| {
        context
            .game
            .equipped
            .and_then(|item| crate::weapons::weapon_emw_id(context.game.id.player_flag & 3, item))
    });
    let weapon = assets.weapon(weapon_file);
    let no_weapon = crate::model::Emw {
        skeleton: Default::default(),
        keyframes: Vec::new(),
        clips: Vec::new(),
        mesh: Default::default(),
    };
    let weapon = weapon.unwrap_or(&no_weapon);
    let clips = crate::weapons::WeaponClips {
        emd: &assets.emd.clips,
        emw: &assets.emw.clips,
        room: room_clips,
        weapon: &weapon.clips,
        emd_keyframes: &assets.emd.keyframes,
        emd_skeleton: &assets.emd.skeleton,
        weapon_keyframes: &weapon.keyframes,
        weapon_skeleton: &weapon.skeleton,
    };
    crate::weapons::update(context.game, context.player, context.room, &clips, pad);
}

/// Run one fixed 30 Hz tick: scripts, interaction, player movement and camera.
/// Returns the door transition the tick requested, if any.
///
/// The action-key room entries probe on the action-press edge carried by
/// `input.action_pressed`, never on the held level: the original runs
/// `check_action_object` from the newly-pressed branch of
/// `player_input_to_behavior`, so holding the button cannot restart an event
/// every tick.
fn tick_room(
    command_vm: &mut scd::vm::CommandVm,
    event_vm: &mut scd::vm::EventVm,
    context: RoomContext<'_>,
    input: player::Input,
) -> Option<game::RoomTransition> {
    tick_room_timed(command_vm, event_vm, context, input, None)
}

/// [`tick_room`] with optional phase timers for `--stats`.
///
/// When `phases` is given, the script/entity/player update and the effect pool
/// update add their durations to it.
fn tick_room_timed(
    command_vm: &mut scd::vm::CommandVm,
    event_vm: &mut scd::vm::EventVm,
    mut context: RoomContext<'_>,
    input: player::Input,
    mut phases: Option<&mut stats::PhaseTotals>,
) -> Option<game::RoomTransition> {
    let update_start = Instant::now();
    // The original's `main_loop` rolls the screen-shake offset at the very top
    // of the rendered frame, before the game task runs its scripts and its
    // per-frame random draw. Doing it here (rather than at render time) keeps
    // the three shake draws ahead of this tick's `g_RandSeed` draw and on the
    // frame the shake bit takes effect, matching the reference stream.
    context.game.roll_screen_shake();
    // The original publishes the remapped D-pad held/pressed words before the
    // scripts run, so a `ck_bits` (0x38) condition sees this frame's pad. The
    // press edge is the current word minus the previous frame's *raw* word
    // (`~prevRaw & held`), so the later controls blanking never re-mints a
    // held button as a fresh press.
    let held = game::dpad_word(&input);
    let pressed = held & !context.game.dpad_held;
    context.game.dpad_held = held;
    context.game.dpad_pressed = pressed;
    context.game.state_words[usize::from(game::STATE_WORD_DPAD_HELD)] = held;
    context.game.state_words[usize::from(game::STATE_WORD_DPAD_PRESSED)] = pressed;
    // The special-room-light state advances once per gameplay frame, after the
    // previous frame's overlay was drawn (the original's `main_loop`).
    context.game.advance_special_light();
    // The original zeroes the per-frame item-use flag bank at the top of every
    // game frame, before the room scripts decide what is usable this frame.
    context.game.clear_item_use_flags();
    // The desk flow, the item-box lid ramp and the deferred key consumption run
    // before the scripts (the original's `check_desk_state`/`check_itembox_state`/
    // `check_event_item_usage` are the first gameplay calls each frame).
    context.game.check_desk_state();
    context.game.check_itembox_state();
    context.game.check_event_item_usage();
    {
        let mut host = game::ScdGameHost::new(context.game);
        command_vm.run_main(&mut host);
    }
    apply_event_requests(context.game, event_vm);
    {
        let mut host = game::ScdGameHost::new(context.game);
        event_vm.step(&mut host);
    }
    // The original's per-frame room-state reset runs right after the command
    // functions and the room-events check: the picked/used item ids, the
    // forward action probe and the second system word are visible to one main
    // pass and wiped here. The native passes below may write after this point
    // (the action probe sets the press pulse the next frame consumes).
    context.game.room_state_reset();
    // Scripted room writes (`inst_cfg` collision boxes, `obj_xfm` lights)
    // become visible here, before the player's physics and before the
    // renderer reads the lighting.
    context.game.apply_room_edits(context.room);
    // The original's `check_camera_switch(0)` runs after the scripts and
    // BEFORE `update_entities`/`update_player_anim`, so it tests the position
    // the scripts left rather than this tick's movement. That is what makes a
    // cut change land one tick after the crossing frame, not on it.
    context.game.sync_player(context.player);
    let previous_cut = context.room.current_cut;
    apply_camera(context.room, context.game, Some(context.player.pos));
    if context.room.current_cut != previous_cut {
        // The effects projected above under the pre-switch camera; recompute
        // their stored screen/depth so the frame renders billboards against
        // the camera it draws.
        context.game.reproject_effects(context.room);
    }
    // The entities think after the event scripts and before the player's
    // physics, exactly like the original's `update_entities`.
    context.game.tick_entities(
        context.room,
        context.npc_models,
        context.pack,
        context.enemy_lua.as_deref_mut(),
    );
    // Scripts may have moved the player entity directly (dir_set, actor
    // motion); mirror that onto the visible player before physics run.
    context.game.sync_player(context.player);
    context.game.advance_frame();
    // The debug INVINCIBLE cheat restores health before the player machine
    // reads it, so a lethal script or enemy hit this tick never starts the
    // death fall.
    context.game.apply_invincible();
    // The original gates the whole player animation state machine on
    // `g_message_flags & 1`: a message's pause word clears the bit, so no pad
    // mapping, movement or animation advance runs until the message is
    // dismissed. Otherwise the state byte dispatches the machine: state 1 is
    // the pad-driven locomotion and every other state is a scripted window
    // that ignores the pad. The frozen and scripted paths still publish the
    // direction pad so the object probe sees it; only the action edge, which
    // belongs to the skipped input state, is withheld.
    let frozen = context.game.message_flags & game::MESSAGE_FLAG_PLAYER_STATE == 0
        // The itembox settle frame: `check_itembox_state` restores the whole
        // message word and raises the box-menu flag, and the menu takes the
        // next frame. The original's player machine still stays frozen on the
        // settle frame (its menu mode owns the frame), so gate on the flag.
        || context.game.itembox.menu_open;
    let state = context.game.entities[0].state();
    let mut pad = input;
    // The original's `game_loop` blanking (0x00480e89): outside an action
    // zone, or while the script's controls latch is clear (`set FG_6, 23, 1`
    // clears `g_message_flags` bit 0x100), the held and pressed pad words are
    // masked to zero before the player machine, so no direction, run, aim,
    // fire or action input reaches the locomotion, the weapon machine or the
    // room-action probe. The scripts still see the raw pad published above.
    let controls_locked = context.game.entities[0].zone_flags & 0x20 != 0
        || context.game.message_flags & game::MESSAGE_FLAG_CONTROLS == 0;
    if controls_locked {
        pad.up = false;
        pad.down = false;
        pad.left = false;
        pad.right = false;
        pad.run = false;
        pad.aim = false;
        pad.aim_pressed = false;
        pad.fire = false;
        pad.fire_pressed = false;
        pad.action_held = false;
        pad.action_pressed = false;
    }
    // The effect-zone flag (raised by the 0x0B handler at the end of the
    // previous probe) halves walk/run speed and the animation cadence this
    // tick, exactly like the original's `MSF2_EFFECT_ZONE` check in the
    // locomotion handlers.
    pad.slow_motion = context.game.flags[5].bit(game::MSF2_EFFECT_ZONE);
    // `g_message_flags` bit 8 clear means a displayed message holds the idle
    // settle from blending into the breathe loop (the original's
    // `player_behavior_00_idle` gate).
    pad.message_blocked = context.game.message_flags & game::MESSAGE_FLAG_CONTROLS == 0;
    // The player's collision callback byte aliases the health-status byte:
    // mirror the live status before any physics or scripted movement runs.
    context.player.collision_flags = context.game.health_status;
    context.game.entities[0].collision_flags = context.game.health_status;
    // The frame-start position the entity/room collision passes anchor their
    // rollbacks at.
    let pre_move = context.player.pos;
    // A movement's free step is consumed by this tick's collision tail; drop
    // any value a skipped tail left behind so it can never leak into this one.
    context.player.collision_pre = None;
    if state != 1 || frozen {
        // The pad-driven idle did not run this tick, so its reference scratch
        // is stale: the trace mirror must not reapply it.
        context.player.idle_trace = None;
        context.player.trace_state = None;
        pad.action_pressed = false;
        pad.action_held = false;
        context.player.input = pad;
        if !frozen {
            // A scripted state owns the player's bytes now. The reference's
            // `action_behavior` is one field, so a grab (or a cutscene pose)
            // that takes the player over writes it and an interrupted weapon
            // machine cannot resume when the state returns to 1 - the demo 2
            // bite left the port's reload latched and it restarted after the
            // recoil. Clear the machine with the state change.
            if state != 1 {
                context.player.weapon = crate::weapons::WeaponState::default();
                context.player.scripted_hold = true;
            }
            let (emd_clips, emw_clips): (&[Clip], &[Clip]) = context
                .player_assets
                .map(|assets| (assets.emd.clips.as_slice(), assets.emw.clips.as_slice()))
                .unwrap_or((&[], &[]));
            let room_clips: &[Clip] = context
                .room
                .room_anim
                .as_ref()
                .map(|anim| anim.clips.as_slice())
                .unwrap_or(&[]);
            let empty_keyframes: &[crate::model::Keyframe] = &[];
            let banks = player_script::AnimationBanks {
                emd_clips,
                emw_clips,
                room_clips,
                emd_keyframes: context
                    .player_assets
                    .map_or(empty_keyframes, |assets| assets.emd.keyframes.as_slice()),
                emw_keyframes: context
                    .player_assets
                    .map_or(empty_keyframes, |assets| assets.emw.keyframes.as_slice()),
                room_keyframes: context
                    .room
                    .room_anim
                    .as_ref()
                    .map_or(empty_keyframes, |anim| anim.keyframes.as_slice()),
                emd_skeleton: context.player_assets.map(|assets| &assets.emd.skeleton),
                room_skeleton: context.room.room_anim.as_ref().map(|anim| &anim.skeleton),
            };
            player_script::update_banks(context.game, context.player, context.room, &banks);
        }
    } else if !player_script::control_gate(context.game, context.player) {
        tick_pad_driven_player(&mut context, pad);
    }
    // `update_player_anim` runs `EntityUpdateLookAtAngles` after the state
    // function and before the collision pass, whether or not the message gate
    // let the state run. The pitch slope reads the tracking joint's world
    // translation the previous frame's composed matrices carry; those matrices
    // are refreshed by the render pass below.
    if let Some(target) = enemy::live_look_at_target(context.game, 0) {
        context.game.entities[0].target = target;
    }
    let look_at_origin = enemy::look_at_joint_origin(context.game, 0);
    context.game.entity_anims[0].slew_look_at(&context.game.entities[0], look_at_origin);
    // `update_player_anim`'s tail: the moved player is mirrored onto entity 0,
    // then `HandleEnemyPlayerCollisions` pushes the player out of every active
    // entity's SCA volume and the room collision resolves the result with the
    // frame-start position as its rollback anchor. Skipped in player state 5
    // (the grab hold), matching the original's `animationId != 5` gate; the
    // scripted door sequence's other arm never runs the room tick in the port.
    context.game.sync_entity_from_player(context.player);
    // The stairway (`action_behavior` 0x11) skips both passes as well: its
    // clip carries the player through the stairs' own collision volume.
    if context.game.entities[0].state() != 5
        && context.player.locked != player::LockedAction::Stairs
    {
        // The reference runs this pass on the *free* step the behaviour's
        // `Add_speedXZ` left - before any room resolve. The locomotion tick
        // resolves its move too (the paths that never reach this tail need
        // that), so rewind to the step it recorded and anchor at the
        // pre-resolve mirror: SCA push first, room resolve last.
        let recorded = context.player.collision_pre.take();
        if let Some(pre) = recorded {
            context.game.entities[0].pos = pre.step;
        }
        enemy::walk::separate_player_from_entities(context.game, pre_move);
        let prev = match recorded {
            Some(pre) => [
                pre.anchor[0],
                context.game.entities[0].pos[1],
                pre.anchor[1],
            ],
            None => [
                context.player.collision_mirror[0],
                context.game.entities[0].pos[1],
                context.player.collision_mirror[1],
            ],
        };
        let resolution = player::resolve_collision_full(
            &context.room.collision,
            prev,
            context.game.entities[0].pos,
            context.player.radius,
            context.player.collision_flags,
        );
        if resolution.code != 2 {
            context.player.collision_mirror = [resolution.pos[0], resolution.pos[2]];
        }
        context.player.pos = resolution.pos;
        context.game.entities[0].pos = context.player.pos;
        // The push handlers raise `status_flags` bit 4 for a Z correction and
        // clear it for an X one; the last push of the tick wins. The player
        // module's own resolve (the locomotion and push steps) records its
        // axis first, then this pass reports its own if it pushed.
        if let Some(push_z) = context.player.last_push_z.take().or(resolution.push_z) {
            if push_z {
                context.game.entities[0].status_flags |= 0x10;
            } else {
                context.game.entities[0].status_flags &= !0x10;
            }
        }
    }
    // `update_player_anim` refreshes zoneFlags bit 0 from the camera switch
    // zone after the collision pass. The renderer reads the whole low byte as
    // its whole-entity gate, so a player the current camera's zone does not
    // cover (and who carries no other interaction bit) is not drawn - the
    // original's own mechanism for hiding the character during a scripted
    // inspect.
    let in_zone = u8::from(enemy::in_camera_zone(
        context.room,
        context.room.current_cut,
        context.game.entities[0].pos,
    ));
    context.game.entities[0].zone_flags = (context.game.entities[0].zone_flags & !0x01) | in_zone;
    // The stairway's case 2 measured the root joint's world translation
    // against the player before this tick's pose is composed: the previous
    // frame's matrices, exactly like the original's `pushVelocity` (both
    // subtractions are 16-bit).
    if std::mem::take(&mut context.player.stairs_shadow_sample)
        && let Some(root) = context.game.joint_worlds[0].first()
    {
        let pos = context.player.pos;
        context.player.shadow_offset = [
            i32::from((root.t[0] as i16).wrapping_sub(pos[0] as i16)),
            i32::from((root.t[2] as i16).wrapping_sub(pos[2] as i16)),
        ];
    }
    // The render loop's `EntityComputeJointWorldMatrices` for the player:
    // composed after the physics so the next tick's held-weapon anchor reads
    // the previous frame's hand, exactly like the original's frame order.
    store_player_joint_worlds(
        context.game,
        context.player,
        context.player_assets,
        context.room,
    );
    // The gated reach finished this tick: raise the viewer flag the action
    // entry selects, return the message ready bit and clear the health lock.
    // The end-of-tick item-viewer hook then opens the viewer.
    if let Some(request) = context.player.take_interact_finished() {
        context.game.complete_reach(request);
    }
    if let Some(phases) = phases.as_mut() {
        phases.update += update_start.elapsed();
    }
    // The effects run after the player's physics and before the action probe,
    // so a script that spawns this frame animates this frame and a dust effect
    // the probe spawns does too.
    let effect_start = Instant::now();
    context.game.tick_effects(context.room);
    if let Some(phases) = phases.as_mut() {
        phases.effect += effect_start.elapsed();
    }
    // The original runs the room action probe after the player's movement, so
    // `stairs_height_update` measures the frame's final position and the climb
    // behaviour starts from where the player actually is. `update_player_position`
    // walks the whole action table every frame, scripted states included - the
    // mansion intro's scripted walk is what carries the player into the main
    // hall door's reach, so gating the probe on the pad-driven state would fire
    // the door several ticks late. The action-key entries (flags bit 0x80)
    // are not part of this pass: the press edge fired them through
    // `check_action_object` at the top of the player's dispatch.
    {
        let mut host = game::ScdGameHost::new(context.game);
        host.interact(context.player.pos, context.player.angle, false);
    }
    // The room objects run after the player's physics and the player-side
    // probe: the push probe, the object-side action probe and the climb scan
    // all read the frame's final player state.
    context.game.tick_objects(context.room, context.player);
    context.game.apply_stair_state(context.player);
    // A script that changed the camera id this frame (the desk's close-up, a
    // cutscene cut) owns the render cut from this frame on: the original
    // writes `g_roomCameraId` directly and the render reads it. The zone scan
    // itself already ran before the player's movement; this only propagates a
    // mid-tick script write.
    if context.game.camera.current_cut < context.room.cuts.len()
        && context.room.current_cut != context.game.camera.current_cut
    {
        context.room.current_cut = context.game.camera.current_cut;
        context.game.reproject_effects(context.room);
    }
    // The stair probe wrote `player.pos[1]`; mirror it onto entity 0 so the
    // renderer and the next script tick see the resolved height.
    context.game.sync_entity_from_player(context.player);
    // The vault's and the stairway's warp cases clear `isBeingAttackedFlag`
    // (the 0x80 the action scan raised) in the tick they warp.
    if std::mem::take(&mut context.player.clear_attacked_flag) {
        context.game.entities[0].is_being_attacked = 0;
    }
    context.game.transition.take()
}

/// Load the transition's target room and place the player in it.
///
/// This is the instant-swap path the transition timeline replaced; it is kept
/// for the state-machine tests that check destination placement directly.
#[cfg(test)]
fn enter_transition(
    pack: &Pack,
    game: &mut game::GameState,
    player_state: &mut player::PlayerState,
    transition: &game::RoomTransition,
) -> Result<LoadedRoom> {
    let mut loaded = load_room(pack, transition.target)?;
    // A door load snapshots the outgoing enemies and clears the live list
    // before the destination boots (see `finish_transition`).
    game.snapshot_enemies(transition.target.room as i8 != game.id.room as i8);
    game.clear_room_entities();
    // A door load resets the player's animation id (see `finish_transition`).
    game.entities[0].set_state(0);
    game.entities[0].action_behavior = 0;
    game.entities[0].action_state = 0;
    game.enter_room(transition.target, &loaded.room);
    *player_state = player::spawn(transition.target, &loaded.room);
    player_state.pos = transition.pos;
    player_state.angle = transition.angle;
    game.sync_entity_from_player(player_state);
    // Place the destination camera before the first frame is drawn; the
    // original runs the zone switch during the transition load.
    apply_camera(&mut loaded.room, game, Some(player_state.pos));
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
    let title = CString::new(window_title(
        &loaded.id.room3(),
        loaded.room.current_cut,
        loaded.room.cuts.len(),
    ))
    .context("window title contains a NUL byte")?;
    if !unsafe { SDL_SetWindowTitle(window, title.as_ptr()) } {
        bail!("SDL_SetWindowTitle failed: {}", sdl_error());
    }
    Ok(())
}

/// `room_set`'s camera block: the room camera id is zeroed before the
/// switch-zone scan, so the walk starts from camera 0 and the pre-scan id is
/// discarded - whether the room's init script wrote it or a save block was
/// loaded with it. The BioCard byte at 0x202 *is* `g_roomCameraId`, so a
/// continue must not keep the saved cut (#42). A room script that re-locked
/// the camera keeps the cut it selected instead: the original takes
/// `display_room_camera_bg`'s branch (RoomInit.cpp:670-676).
fn reset_room_camera_id(game: &mut game::GameState) {
    if !game.camera.locked {
        game.set_camera_cut(0);
    }
}

/// Apply the SCD camera state to the room.
///
/// When the scripts hold the camera lock, their cut wins; otherwise the M2 zone
/// switching picks the cut from the player position. The zone walk starts from
/// the game's current cut, not the room's: a script (`setb 2`) or handler
/// (`check_desk`) that selects a cut mid-tick is the original's
/// `g_roomCameraId` write, and the zone scan has to start from it for the
/// camera-zone walk to re-home there. `None` keeps the current cut when no
/// position is available yet (room load and capture).
fn apply_camera(room: &mut RoomState, game: &mut game::GameState, pos: Option<[i32; 3]>) {
    // `game_loop` consumes a pending deferred redisplay before it tests the
    // zones: the cut the previous frame's walk chose is the one drawn this
    // frame, and no new switch is tested until the frame after.
    if game.flags[5].bit(game::MSF_CAMERA_REDRAW) {
        game.flags[5].apply(game::MSF_CAMERA_REDRAW, 1);
        room.current_cut = game.camera.current_cut;
        return;
    }
    let selected = if game.camera.locked && game.camera.current_cut < room.cuts.len() {
        game.camera.current_cut
    } else if let Some(pos) = pos {
        player::camera_for_position(room, game.camera.current_cut, pos)
    } else {
        room.current_cut
    };
    // `check_camera_switch` writes `g_roomCameraId = camTo` and, while
    // `MSF_CAMERA_DEFER` (a script's `set FG_STATUS, 29, 0`) is set, raises
    // `MSF_CAMERA_REDRAW` and returns without redisplaying, so the script can
    // observe the new camera id and react (room 104's `movie_on`) before the
    // frame switches.
    if pos.is_some()
        && selected != game.camera.current_cut
        && game.flags[5].bit(game::MSF_CAMERA_DEFER)
        && selected < room.cuts.len()
    {
        game.set_camera_cut(selected);
        game.flags[5].apply(game::MSF_CAMERA_REDRAW, 0);
        return;
    }
    game.set_camera_cut(selected);
    room.current_cut = selected;
}

/// The latched input words carry one bit per logical action
/// ([`controls::Action::bit`]) plus the fixed port-only F1/F9 bits.
const KEY_UP: u32 = Action::Up.bit();
const KEY_DOWN: u32 = Action::Down.bit();
const KEY_LEFT: u32 = Action::Left.bit();
const KEY_RIGHT: u32 = Action::Right.bit();
const KEY_CONFIRM: u32 = Action::Confirm.bit();
const KEY_CANCEL: u32 = Action::Cancel.bit();
const KEY_RUN: u32 = Action::Run.bit();
const KEY_AIM: u32 = Action::Aim.bit();
const KEY_FIRE: u32 = Action::Fire.bit();
const KEY_START: u32 = Action::Start.bit();
const KEY_PAGE_LEFT: u32 = Action::PageLeft.bit();
const KEY_PAGE_RIGHT: u32 = Action::PageRight.bit();
const KEY_F1: u32 = controls::DEBUG_MENU_BIT;
const KEY_F9: u32 = controls::RETURN_TITLE_BIT;

/// A scripted per-tick input playback for the oracle (`--input`).
///
/// The file lists `<tick> <button>...` entries, one per line; a later entry
/// replaces the held button set from its tick on, and a tick with no buttons
/// releases everything. The trace tick is the reference's (a frame past the
/// room-entry arm), so `word_at(t)` is the set of keys held during the tick
/// that emits trace line `t`. The button names mirror the reference's
/// `--test-input` button map:
///
/// `up down left right confirm cancel run aim fire start page_left page_right`
/// (with the aliases `action/cross`, `circle`, `square`, `inventory`, `l1`,
/// `r1`).
#[derive(Debug, Default, Clone)]
pub struct InputScript {
    /// `(tick, key bit word)` entries, sorted by tick; the last entry at or
    /// before a tick is the held set.
    entries: Vec<(u32, u32)>,
}

/// The key bit a `--input` button name maps to, or `None` for an unknown name.
fn script_button_bit(name: &str) -> Option<u32> {
    Some(match name {
        "up" => KEY_UP,
        "down" => KEY_DOWN,
        "left" => KEY_LEFT,
        "right" => KEY_RIGHT,
        "confirm" | "cross" | "action" => KEY_CONFIRM,
        "cancel" | "circle" => KEY_CANCEL,
        "run" => KEY_RUN,
        "aim" | "square" => KEY_AIM,
        "fire" => KEY_FIRE,
        "start" | "inventory" => KEY_START,
        "page_left" | "l1" => KEY_PAGE_LEFT,
        "page_right" | "r1" => KEY_PAGE_RIGHT,
        _ => return None,
    })
}

impl InputScript {
    /// Parse an `--input` script body.
    pub fn parse(text: &str) -> Result<Self> {
        let mut entries: Vec<(u32, u32)> = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut tokens = line.split_whitespace();
            let Some(tick) = tokens.next() else { continue };
            let tick: u32 = tick.parse().with_context(|| {
                format!("input line {}: `{tick}` is not a tick number", number + 1)
            })?;
            let mut word = 0u32;
            for token in tokens {
                let bit = script_button_bit(token).ok_or_else(|| {
                    anyhow::anyhow!("input line {}: unknown button `{token}`", number + 1)
                })?;
                word |= bit;
            }
            // Last entry at a tick wins.
            if let Some(last) = entries.last_mut()
                && last.0 == tick
            {
                last.1 = word;
                continue;
            }
            entries.push((tick, word));
        }
        entries.sort_by_key(|entry| entry.0);
        Ok(Self { entries })
    }

    /// Read and parse an `--input` script file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read input script {}", path.display()))?;
        Self::parse(&text)
    }

    /// Whether any entry is armed (so a plain run stays byte-identical).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The key word held during traced tick `t`.
    pub fn word_at(&self, tick: u32) -> u32 {
        let mut word = 0;
        for &(at, value) in &self.entries {
            if at > tick {
                break;
            }
            word = value;
        }
        word
    }
}

/// Every input device, latched from the SDL event queue and resolved through
/// the action map.
///
/// The render loop runs at the display's refresh rate, which is not the 30 Hz
/// tick rate; deriving edges from `SDL_GetKeyboardState` every rendered frame
/// drops presses (the frame that sees the edge may run no tick, and by the
/// next tick the key is already held) and can replay one edge over a frame
/// that catches up several ticks. Feeding every input event into this latch
/// makes a press reach exactly one tick, whenever it arrived.
///
/// Held state is kept per physical source, so an action with several
/// bindings stays held until the last of them is released: letting go of
/// Space while Return (or the pad's south button) is still down must not
/// clear confirm.
#[derive(Default)]
struct InputState {
    /// The action map every source is resolved through.
    bindings: controls::Bindings,
    /// Sources currently down, tagged by device (`0` for the keyboard and
    /// mouse, else the gamepad's joystick instance id).
    sources: Vec<(u32, controls::Source)>,
    /// Each open gamepad axis's latched direction, keyed by device and axis.
    axes: HashMap<(u32, i32), i8>,
    /// Action bits currently down, for continuous movement.
    held: u32,
    /// Action bits that went down since the last tick; consumed by
    /// [`Self::tick`].
    pressed: u32,
    /// A non-repeat press of any input since the last tick.
    any_pressed: bool,
    /// The first bindable source pressed since the last tick, for the rebind
    /// screen's capture.
    captured: Option<controls::Source>,
    /// The open gamepads (none for a scripted or test latch).
    pads: Gamepads,
}

/// One fixed tick of latched input, split into the shapes its consumers want.
struct TickInput {
    /// Held movement and run for the player.
    player: player::Input,
    /// Action level: held, or this tick's press so a quick tap still registers.
    action: bool,
    /// Edge-triggered keys for the menu, message and UI screens.
    ui: UiInput,
}

/// The axis deflection that latches a direction.
const AXIS_PRESS: i16 = 16_000;
/// The deflection a latched direction holds down to (hysteresis, so a stick
/// resting near the threshold does not chatter).
const AXIS_RELEASE: i16 = 12_000;

impl InputState {
    /// A latch resolving through `bindings`.
    fn new(bindings: controls::Bindings) -> Self {
        Self {
            bindings,
            ..Self::default()
        }
    }

    /// The latch an interactive window reads: the gamepads started and the
    /// bindings from `arklay.toml` (the defaults when it has none, or when
    /// it cannot be read; the launch already reported its warnings).
    fn interactive() -> Self {
        Gamepads::init();
        let (config, _) = crate::config::Config::load_default();
        Self::new(config.bindings_or_default())
    }

    /// The action map in use.
    fn bindings(&self) -> &controls::Bindings {
        &self.bindings
    }

    /// Apply the CONTROLS tool's change and persist it to `arklay.toml`. A
    /// write failure is reported and the change still applies for this run.
    fn apply_bindings_change(&mut self, bindings: controls::Bindings) {
        let path = crate::config::Config::default_path();
        if let Err(err) = crate::config::Config::save_bindings(&path, &bindings) {
            eprintln!("warning: bindings not saved: {err:#}");
        }
        self.set_bindings(bindings);
    }

    /// Switch to new bindings. The held sources are re-resolved, so a key
    /// held across the change drives its new actions without a fresh edge.
    fn set_bindings(&mut self, bindings: controls::Bindings) {
        self.bindings = bindings;
        self.refresh_held();
    }

    /// Recompute the held action word from the held sources.
    fn refresh_held(&mut self) {
        self.held = self
            .sources
            .iter()
            .fold(0, |held, &(_, source)| held | self.bindings.mask(source));
    }

    /// Latch a source going down. A repeat keeps the held bits fresh but is
    /// not a new press, so holding a key cannot storm edges.
    fn source_down(&mut self, device: u32, source: controls::Source, repeat: bool) {
        if !self.sources.contains(&(device, source)) {
            self.sources.push((device, source));
        }
        let mask = self.bindings.mask(source);
        self.held |= mask;
        if !repeat {
            self.pressed |= mask;
            self.any_pressed = true;
            if self.captured.is_none() && source.reserved_bit().is_none() {
                self.captured = Some(source);
            }
        }
    }

    /// Latch a source going up. A pending press is left alone: a tap released
    /// before the tick must still reach it.
    fn source_up(&mut self, device: u32, source: controls::Source) {
        self.sources.retain(|&held| held != (device, source));
        self.refresh_held();
    }

    /// A momentary source (a wheel notch): one press with no hold.
    fn source_tap(&mut self, source: controls::Source) {
        self.source_down(0, source, false);
        self.source_up(0, source);
    }

    /// Latch a keyboard key-down.
    fn key_down(&mut self, scancode: SDL_Scancode, repeat: bool) {
        self.source_down(0, controls::Source::key(scancode), repeat);
    }

    /// Latch a keyboard key-up.
    fn key_up(&mut self, scancode: SDL_Scancode) {
        self.source_up(0, controls::Source::key(scancode));
    }

    /// Latch a gamepad axis reading as a direction press or release.
    fn axis_motion(&mut self, device: u32, axis: i32, value: i16) {
        let state = self.axes.get(&(device, axis)).copied().unwrap_or(0);
        let next = if value >= AXIS_PRESS || (state > 0 && value > AXIS_RELEASE) {
            1
        } else if value <= -AXIS_PRESS || (state < 0 && value < -AXIS_RELEASE) {
            -1
        } else {
            0
        };
        if next == state {
            return;
        }
        if state != 0 {
            self.source_up(device, controls::Source::PadAxis(axis, state > 0));
        }
        if next != 0 {
            self.source_down(device, controls::Source::PadAxis(axis, next > 0), false);
        }
        self.axes.insert((device, axis), next);
    }

    /// Drop every source a removed gamepad still held.
    fn device_removed(&mut self, device: u32) {
        self.sources.retain(|&(held, _)| held != device);
        self.axes.retain(|&(held, _), _| held != device);
        self.refresh_held();
    }

    /// Drop every held and pending input (window focus loss, keyboard
    /// removal).
    fn clear(&mut self) {
        self.sources.clear();
        self.axes.clear();
        self.held = 0;
        self.pressed = 0;
        self.any_pressed = false;
        self.captured = None;
    }

    /// The held word including presses not yet consumed by a tick.
    ///
    /// The film skip check runs on the render loop, between fixed ticks, and
    /// must see a tap that a tick has not consumed yet.
    fn held_word(&self) -> u32 {
        self.held | self.pressed
    }

    /// Consume the presses latched since the last tick.
    ///
    /// Held keys stay in the movement input every tick; each press is reported
    /// to the UI exactly once and, through `active`, still counts as one tick
    /// of movement/action even when its key was released before the tick.
    fn tick(&mut self) -> TickInput {
        let pressed = std::mem::take(&mut self.pressed);
        let active = self.held | pressed;
        TickInput {
            player: player::Input {
                up: active & KEY_UP != 0,
                down: active & KEY_DOWN != 0,
                left: active & KEY_LEFT != 0,
                right: active & KEY_RIGHT != 0,
                run: active & KEY_RUN != 0,
                run_pressed: pressed & KEY_RUN != 0,
                action_held: active & KEY_CONFIRM != 0,
                action_pressed: pressed & KEY_CONFIRM != 0,
                aim: active & KEY_AIM != 0,
                aim_pressed: pressed & KEY_AIM != 0,
                fire: active & KEY_FIRE != 0,
                fire_pressed: pressed & KEY_FIRE != 0,
                // The PC key map leaves the joystick button that feeds the raw
                // pad's bit 4 unbound, so the keyboard never raises it.
                lock_drop: false,
                slow_motion: false,
                message_blocked: false,
            },
            action: active & KEY_CONFIRM != 0,
            ui: UiInput {
                up: pressed & KEY_UP != 0,
                down: pressed & KEY_DOWN != 0,
                left: pressed & KEY_LEFT != 0,
                right: pressed & KEY_RIGHT != 0,
                held_up: self.held & KEY_UP != 0,
                held_down: self.held & KEY_DOWN != 0,
                held_left: self.held & KEY_LEFT != 0,
                held_right: self.held & KEY_RIGHT != 0,
                confirm: pressed & KEY_CONFIRM != 0,
                cancel: pressed & KEY_CANCEL != 0,
                page_left: pressed & KEY_PAGE_LEFT != 0,
                page_right: pressed & KEY_PAGE_RIGHT != 0,
                start: pressed & KEY_START != 0,
                debug_menu: pressed & KEY_F1 != 0,
                return_title: pressed & KEY_F9 != 0,
                any: std::mem::take(&mut self.any_pressed),
                captured: self.captured.take(),
            },
        }
    }

    /// Latch the scripted held word for one tick and consume its edges.
    ///
    /// The word is the full set of keys down during the tick, exactly what the
    /// reference's `--test-input` injects into the pad. The rising edge (keys
    /// down now that were not down last tick) becomes this tick's press, so a
    /// one-tick scripted tap reaches the UI exactly once.
    fn tick_scripted(&mut self, down: u32) -> TickInput {
        let edge = down & !self.held;
        self.held = down;
        self.pressed = edge;
        self.any_pressed = edge != 0;
        self.tick()
    }
}

/// Map one frame of edge-triggered pad input to the single inventory menu
/// input it represents; the original pad reports one new button per frame.
fn menu_input(ui: UiInput) -> Option<MenuInput> {
    if ui.up {
        Some(MenuInput::Up)
    } else if ui.down {
        Some(MenuInput::Down)
    } else if ui.left {
        Some(MenuInput::Left)
    } else if ui.right {
        Some(MenuInput::Right)
    } else if ui.page_left {
        Some(MenuInput::PageLeft)
    } else if ui.page_right {
        Some(MenuInput::PageRight)
    } else if ui.confirm {
        Some(MenuInput::Confirm)
    } else if ui.cancel {
        Some(MenuInput::Cancel)
    } else {
        None
    }
}

/// The cue a menu pad input plays.
///
/// The original's menu handlers call `play_sfx(3, ...)`: slot 4 is the cursor
/// blip on a D-pad move, slot 6 the decide chime on confirm and slot 5 the
/// cancel cue. Those slots are the `cursor`/`decide`/`cancel` samples the
/// character tables share, so the port's UI cues resolve to the same wavs.
fn menu_cue(input: MenuInput) -> Option<ui::UiCue> {
    match input {
        MenuInput::Up | MenuInput::Down | MenuInput::Left | MenuInput::Right => {
            Some(ui::UiCue::Cursor)
        }
        MenuInput::Confirm => Some(ui::UiCue::Decide),
        MenuInput::Cancel => Some(ui::UiCue::Cancel),
        MenuInput::PageLeft | MenuInput::PageRight => None,
    }
}

/// The player's character model plus its no-weapon locomotion clips.
struct PlayerAssets {
    emd: Emd,
    emw: crate::model::Emw,
    /// The character block's weapon animations by EMW file id, loaded from the
    /// row of [`crate::weapons::WEAPON_EMW`]. A missing file is absent.
    weapons: std::collections::HashMap<u8, crate::model::Emw>,
}

impl PlayerAssets {
    /// The active weapon's clips and mesh for an EMW file id.
    fn weapon(&self, file: Option<u8>) -> Option<&crate::model::Emw> {
        self.weapons.get(&file?)
    }
}

/// The player's `EntityComputeJointWorldMatrices` pass: compose the displayed
/// pose's joint worlds into `joint_worlds[0]` at the end of every tick, exactly
/// like the original's render loop, so the next tick's state-8 held-weapon
/// anchor (and the effect pass) reads the previous frame's hand. The bank
/// selection mirrors the renderer's.
fn store_player_joint_worlds(
    game: &mut game::GameState,
    player: &player::PlayerState,
    assets: Option<&PlayerAssets>,
    room: &RoomState,
) {
    let Some(assets) = assets else {
        return;
    };
    let (skeleton, keyframes, clips) = match player.clip_source {
        player::ClipSource::Emd => (
            &assets.emd.skeleton,
            &assets.emd.keyframes,
            &assets.emd.clips,
        ),
        player::ClipSource::Emw => (
            &assets.emd.skeleton,
            &assets.emw.keyframes,
            &assets.emw.clips,
        ),
        player::ClipSource::Room => match &room.room_anim {
            Some(anim) if !anim.clips.is_empty() => (&anim.skeleton, &anim.keyframes, &anim.clips),
            _ => (
                &assets.emd.skeleton,
                &assets.emd.keyframes,
                &assets.emd.clips,
            ),
        },
        player::ClipSource::Weapon => match assets.weapon(player.weapon.weapon_file) {
            Some(weapon) => (&weapon.skeleton, &weapon.keyframes, &weapon.clips),
            None => (
                &assets.emd.skeleton,
                &assets.emd.keyframes,
                &assets.emd.clips,
            ),
        },
        player::ClipSource::Damage => match &game.player_damage {
            Some(bank) => (
                &assets.emd.skeleton,
                bank.keyframes.as_ref(),
                bank.clips.as_ref(),
            ),
            None => (
                &assets.emd.skeleton,
                &assets.emd.keyframes,
                &assets.emd.clips,
            ),
        },
    };
    // Materialize the pose on the live clock, the one the render pass poses
    // from: the clock's `applied` pose is the port's `joints[].transform`,
    // which persists until the next consuming tick exactly like the original's
    // joint structs. Reading it here folds the pending blend steps so `applied`
    // always holds the frame the pass will draw; the pass's own read is then
    // idempotent.
    //
    // The entity mirror's clock is not the source: its frame words are only
    // synced at behavior entries and scripted publishes, so a pad-driven walk
    // or run would read the stale pose it last scripted (regression: the
    // locomotion animation froze on screen while the player kept moving).
    let keyframe = player.anim.pose_keyframe(clips, keyframes);
    let Some(keyframe) = keyframe else {
        return;
    };
    // Pin the pose on screen on both clocks. A scripted frame setter
    // (`plw_anim`) can point the clock at a clip and frame without consuming a
    // tick; the original's joint structs keep the last `Joint_move` output, so
    // the next scripted read must still see this pose rather than the freshly
    // named frame.
    game.entity_anims[0]
        .player
        .set_applied_pose(Some(keyframe.clone()));
    player.anim.set_applied_pose(Some(keyframe.clone()));
    let entity = crate::anim::entity_matrix(player.pos, player.angle);
    let worlds = crate::anim::joint_matrices(skeleton, &keyframe, &entity);
    if !worlds.is_empty() {
        game.joint_worlds[0] = worlds;
    }
}

/// The room objects to submit for `camera`, in declared slot order, each
/// with its slot.
///
/// A record is visible while its active bit is set, its declared pair decoded
/// into the room's asset table, and its switch-zone probe point inside the
/// current camera's zone. The original walks `g_omodel_table` in order and
/// culls each record with `is_entity_in_switch_zone(record + 0x54)`; the port
/// uses the record's live position as that probe point.
fn visible_objects<'a>(
    room: &'a RoomState,
    objects: &'a objects::ObjectTable,
    camera: usize,
    player_pos: [i32; 3],
    player_angle: u16,
) -> Vec<(
    &'a objects::ObjectAsset,
    &'a objects::ObjectRecord,
    anim::Mat4x3,
    usize,
)> {
    let mut visible = Vec::new();
    for (slot, record) in objects.records.iter().enumerate() {
        if !record.active() {
            continue;
        }
        let Some(asset) = record.asset.and_then(|pair| {
            room.object_models
                .iter()
                .find(|asset| asset.pair_index == usize::from(pair))
        }) else {
            continue;
        };
        // The camera-switch cull uses the composed world translation, the
        // original's `is_entity_in_switch_zone(record + 0x54)`.
        let mut world = objects::world_matrix(objects, slot, player_pos, player_angle);
        if !enemy::in_camera_zone(room, camera, world.t) {
            continue;
        }
        if object_render_skipped(room, camera, record) {
            continue;
        }
        // The courtyard heliport's display items 1-4 sit 1000 units further
        // along the view axis (the original bumps the composed local matrix
        // Z before submission; the cull above used the unshifted position).
        if room.stage == 3 && room.room == 0x03 && (1..5).contains(&(record.model & 0x3F)) {
            world.t[2] += 1000;
        }
        visible.push((asset, record, world, slot));
    }
    visible
}

/// The item-model pass, mirroring `visible_objects` for the item table: a
/// record is visible while its active bit is set, its declared pair decoded
/// into the room's item asset table, and its composed world probe point inside
/// the current camera's zone. The world matrix resolves the item's parent kind
/// (absolute, player or omodel); the switch-zone cull uses that composed
/// translation, the original's `is_entity_in_switch_zone`.
fn visible_items<'a>(
    room: &'a RoomState,
    items: &'a objects::ItemTable,
    objects: &'a objects::ObjectTable,
    lighting: &Lighting,
    camera: usize,
    player_pos: [i32; 3],
    player_angle: u16,
) -> Vec<(
    &'a objects::ObjectAsset,
    &'a objects::ItemRecord,
    anim::Mat4x3,
    usize,
)> {
    let mut visible = Vec::new();
    for (slot, record) in items.records.iter().enumerate() {
        if !record.active() {
            continue;
        }
        let Some(asset) = record.asset.and_then(|pair| {
            room.item_models
                .iter()
                .find(|asset| asset.pair_index == usize::from(pair))
        }) else {
            continue;
        };
        let world =
            objects::item_world_matrix(items, objects, lighting, slot, player_pos, player_angle);
        if !enemy::in_camera_zone(room, camera, world.t) {
            continue;
        }
        visible.push((asset, record, world, slot));
    }
    visible
}

/// The per-room object render suppressions the original applies in
/// `RoomObjectRender`.
///
/// These are stand-in records the scripts keep for interaction while the
/// visible model lives elsewhere (mirror/door frames), plus the boulder that
/// is dropped once it rolls within ~3600 units of the camera. The stage
/// folding uses the port's 1-based stage digit.
fn object_render_skipped(room: &RoomState, camera: usize, record: &objects::ObjectRecord) -> bool {
    let model = record.model & 0x3F;
    // Guardhouse 002's mirror stand-in on camera 4.
    if room.stage == 4 && room.room == 0x06 && camera == 4 && model == 0 {
        return true;
    }
    // The lab B3 private room's switch/door stand-in on cameras 0 and 4.
    if room.stage == 5 && room.room == 0x0A && (camera == 0 || camera == 4) && model == 0 {
        return true;
    }
    // The mansion 1F trap room's roof stand-in, at its shipped position only.
    if room.stage % 5 == 1
        && room.room == 0x15
        && camera == 0
        && model == 0
        && record.pos == [0x12FC, -0x2828, 0x12FC]
    {
        return true;
    }
    // The courtyard boulder passage drops the boulder model once it rolls
    // within 0x7274 on X of the camera-3 eye.
    if room.stage % 5 == 3
        && room.room == 0x0F
        && camera == 3
        && model == 0
        && record.pos[0] > 0x7274
    {
        return true;
    }
    false
}

/// Compose the look-at tracking joint's aim onto a character's world matrices.
///
/// Joint 1 is the original's `lookAtJointIdx` for every character model; the
/// original composes `RotMatrixYXZ(0, yaw, pitch)` onto its world matrix
/// whenever the look-at flags are non-zero, so a cleared slew-enable bit
/// freezes the last aim in place.
fn apply_look_at(joints: &mut [anim::Mat4x3], entity: &game::Entity, clock: &enemy::EntityAnim) {
    if entity.look_at_flags == 0 {
        return;
    }
    // The character models' tracking joint is 1; the Tyrant names joint 2.
    let index = usize::from(entity.look_at_joint.max(1));
    let Some(joint) = joints.get_mut(index) else {
        return;
    };
    *joint = anim::compose(
        joint,
        &anim::look_at_matrix(clock.look_at_yaw, clock.look_at_pitch),
    );
}

/// The switch-zone member class of a character model: the original's joint
/// flag bits `0x74`, whose joints are the only ones a character outside the
/// current camera's switch zone still draws, and then only when the joint
/// itself sits inside the zone. No shipped character model sets the class, so
/// an off-zone character contributes no joints at all; the synthetic tests
/// exercise a non-empty class.
fn member_joint_class(id: u8) -> u32 {
    let _ = id;
    0
}

/// The hidden-joint mask for one character: the scripted hidden bits plus the
/// original's whole-entity switch-zone gate.
///
/// `entity_in_switch_zone` is whether `has_enter_switch_zone & 0x7f` is
/// non-zero (the character has entered the current camera's switch zone). The
/// original's `render_entity` computes that once per entity and then walks its
/// joints: an entity that has NOT entered the zone drops every joint except
/// the member class (`0x74`), and even a member joint draws only when its own
/// world position is inside the zone; an entity inside the zone draws every
/// non-member joint and position-tests only the members.
fn joint_hidden_mask(
    entity_in_switch_zone: bool,
    members: u32,
    room: &RoomState,
    camera: usize,
    joint_flags: u32,
    joints: &[anim::Mat4x3],
) -> u32 {
    let mut hidden = joint_flags;
    for (index, joint) in joints.iter().enumerate() {
        if index >= 32 {
            break;
        }
        let member = members & (1 << index) != 0;
        let in_zone = enemy::in_camera_zone(room, camera, joint.t);
        let draws = if entity_in_switch_zone {
            !member || in_zone
        } else {
            member && in_zone
        };
        if !draws {
            hidden |= 1 << index;
        }
    }
    hidden
}

/// Draw one gameplay frame: the cut background, one mesh per active scripted
/// character, the player model, the player's ground shadow and the camera's
/// room-mask layer, depth-sorted together.
///
/// Every active character enters the loop, exactly like the original's entity
/// render loop, but its joints are gated by `has_enter_switch_zone`: a
/// character that has not entered the current camera's switch zone draws only
/// its `0x74` member joints, and those only while inside the zone, so an
/// off-zone character contributes nothing. A character inside the zone draws
/// as before. The meshes are submitted in entity-slot order with the player
/// last, because the original queues the enemies before the player; the shared
/// far-to-near sort is stable, so equal-depth triangles keep that submission
/// order and the player paints over an NPC at an exact tie. The mask page and
/// the shadow page are loaded lazily and cached.
#[allow(clippy::too_many_arguments)]
fn render_frame(
    framebuffer: &mut Framebuffer,
    pack: &Pack,
    id: RoomId,
    room: &RoomState,
    player_state: &player::PlayerState,
    game: &mut game::GameState,
    assets: Option<&PlayerAssets>,
    npc_models: &mut enemy::EntityModelCache,
    masks: &mut MaskCache,
    shadows: &mut ShadowCache,
    effect_cache: &mut EffectPageCache,
) {
    render_frame_on(
        framebuffer,
        None,
        pack,
        id,
        room,
        player_state,
        game,
        assets,
        npc_models,
        masks,
        shadows,
        effect_cache,
        &mut Tween::default(),
    );
}

/// [`render_frame`], drawing the scene with `gpu` when given.
///
/// The GPU path leaves the framebuffer black with only the special-light
/// overlay on it (the session composites it over the GPU scene) and returns
/// `true`. It returns `false` when the frame has no GPU scene: there is no
/// camera cut, or recording failed and the scene was drawn in software
/// instead.
#[allow(clippy::too_many_arguments)]
fn render_frame_on(
    framebuffer: &mut Framebuffer,
    gpu: Option<&mut GpuRenderer>,
    pack: &Pack,
    id: RoomId,
    room: &RoomState,
    player_state: &player::PlayerState,
    game: &mut game::GameState,
    assets: Option<&PlayerAssets>,
    npc_models: &mut enemy::EntityModelCache,
    masks: &mut MaskCache,
    shadows: &mut ShadowCache,
    effect_cache: &mut EffectPageCache,
    tween: &mut Tween,
) -> bool {
    let Some(cut) = room.cuts.get(room.current_cut) else {
        framebuffer.clear();
        return false;
    };
    // Every moving thing below is recorded exactly and drawn between the
    // previous tick and this one (a no-op unless the render rate tweens).
    tween.begin(id, room.current_cut);
    // The frame's screen shake, rolled by `roll_screen_shake` at the top of the
    // gameplay tick. It shifts the camera projection and the background's
    // display origin; the room's view matrix stays untouched.
    let shake = game.screen_shake();
    let mut camera = Camera::from_cut(cut);
    camera.screen = shake;
    let lighting = Lighting::from_room(room);

    let page = if cut.masks.is_empty() || cut.mask_active == 0 {
        None
    } else {
        masks.page_for(pack, id, room.current_cut)
    };
    let layer = page.map(|page| MaskLayer {
        room: id,
        camera: room.current_cut,
        cut,
        page,
        active: cut.mask_active,
    });

    // Every ground shadow is submitted through the same fade-sprite path: the
    // player's when its camera-zone test passes (or the room forces it on),
    // and one per active character that has entered the camera's switch zone,
    // using `character_init`'s tint, half extents and local offset.
    let shadow_texture = shadows.page_for(pack);
    let mut shadow_list: Vec<render::Shadow<'_>> = Vec::new();
    if let Some(texture) = shadow_texture
        && shadow::visible(
            id,
            room,
            room.current_cut,
            player_state,
            game.entities[0].zone_flags,
        )
    {
        // The quad sits at the player's `pushVelocity` offset and `posY`, which
        // only the stairway moves away from the entity's own point.
        let pos = player_state.pos;
        let mut shadow_pos = [
            pos[0] + player_state.shadow_offset[0],
            player_state.stairs_floor.unwrap_or(pos[1]),
            pos[2] + player_state.shadow_offset[1],
        ];
        let mut shadow_angle = player_state.angle;
        tween.shadow(tween::Key::Player, &mut shadow_pos, &mut shadow_angle);
        shadow_list.push(render::Shadow {
            texture,
            pos: shadow_pos,
            angle: shadow_angle,
            half_x: player_state.shadow_half_x,
            half_z: player_state.shadow_half_z,
            shift: player_state.shadow_shift,
            lift: shadow::offset_y(id, room.current_cut),
            tint: shadow::billboard_tint(player_state.shadow_tint),
        });
    }

    // Each character's joints are posed with the blended pose clock, the
    // tracking joint's look-at aim composed on, and the switch-zone member
    // gate folded into `hidden_joints` below.
    let player_joints = assets.and_then(|assets| {
        // The room's own animation pair drives the push/vault/ladder poses; a
        // room without it falls back to the EMD settle stance (documented).
        let (skeleton, keyframes, clips) = match player_state.clip_source {
            player::ClipSource::Emd => (
                &assets.emd.skeleton,
                &assets.emd.keyframes,
                &assets.emd.clips,
            ),
            player::ClipSource::Emw => (
                &assets.emd.skeleton,
                &assets.emw.keyframes,
                &assets.emw.clips,
            ),
            player::ClipSource::Room => match &room.room_anim {
                Some(anim) if !anim.clips.is_empty() => {
                    (&anim.skeleton, &anim.keyframes, &anim.clips)
                }
                _ => (
                    &assets.emd.skeleton,
                    &assets.emd.keyframes,
                    &assets.emd.clips,
                ),
            },
            player::ClipSource::Weapon => match assets.weapon(player_state.weapon.weapon_file) {
                Some(weapon) => (&weapon.skeleton, &weapon.keyframes, &weapon.clips),
                None => (
                    &assets.emd.skeleton,
                    &assets.emd.keyframes,
                    &assets.emd.clips,
                ),
            },
            player::ClipSource::Damage => match &game.player_damage {
                Some(bank) => (
                    &assets.emd.skeleton,
                    bank.keyframes.as_ref(),
                    bank.clips.as_ref(),
                ),
                None => (
                    &assets.emd.skeleton,
                    &assets.emd.keyframes,
                    &assets.emd.clips,
                ),
            },
        };
        // The blended pose the clock shows for the applied frame, then the
        // tracking joint's aim composed onto it.
        let keyframe = player_state.anim.pose_keyframe(clips, keyframes)?;
        let entity = anim::entity_matrix(player_state.pos, player_state.angle);
        let mut joints = anim::joint_matrices(skeleton, &keyframe, &entity);
        apply_look_at(&mut joints, &game.entities[0], &game.entity_anims[0]);
        Some(joints)
    });
    // The composed player matrices stand in for the original's render-pass
    // `joint->world`: the next tick's look-at pitch slope and the Lua joint
    // reads see exactly what was drawn.
    if let Some(joints) = player_joints.as_ref() {
        game.joint_worlds[0] = joints.clone();
    }
    // Tweened after the exact copy above: the simulation never reads a
    // presentation-only pose.
    let mut player_joints = player_joints;
    if let Some(joints) = player_joints.as_mut() {
        tween.joints(tween::Key::Player, joints);
    }

    // The parsed NPC models are held in `models` so the mesh references built
    // below stay alive for the draw call.
    let mut models: Vec<Arc<Emd>> = Vec::new();
    let mut npc_joints: Vec<Vec<anim::Mat4x3>> = Vec::new();
    let mut npc_hidden: Vec<u32> = Vec::new();
    let mut npc_tints: Vec<[u8; 3]> = Vec::new();
    let mut npc_weapon_ids: Vec<Option<u16>> = Vec::new();
    let mut npc_zone_gate: Vec<bool> = Vec::new();
    for slot in 1..game::ENTITY_COUNT {
        let entity = &game.entities[slot];
        if !entity.active() {
            continue;
        }
        // A Yawn body segment draws no mesh (the original's entity renderer
        // returns early for id 0x0D/0x12 with sub-type 1), but it still
        // queues its ground quad - except the tail segment, whose own quad the
        // original's whole-body shadow pass never reaches.
        let yawn_segment = matches!(entity.id, 0x0D | 0x12) && entity.behavior_flags == 1;
        let shadow_gate = entity.has_enter_switch_zone & 0x7F != 0
            && !(yawn_segment && entity.yawn_joint == 0x0E);
        // An entity queues its own ground quad whenever it entered the current
        // camera's switch zone: characters read the per-character tint and
        // quad from `character_init`, monsters the scripted override.
        if let Some(texture) = shadow_texture
            && shadow_gate
        {
            let shadow = if let Some(init) = enemy::data::character_shadow(entity.id, &game.flags) {
                Some(render::Shadow {
                    texture,
                    pos: [
                        entity.pos[0] + i32::from(init.shadow_offset[0]),
                        entity.pos[1],
                        entity.pos[2] + i32::from(init.shadow_offset[2]),
                    ],
                    angle: entity.angle,
                    half_x: i32::from(init.shadow_half_x),
                    half_z: i32::from(init.shadow_half_z),
                    shift: [0, 0],
                    lift: shadow::offset_y(id, room.current_cut),
                    tint: shadow::billboard_tint(init.tint),
                })
            } else if !entity.shadow_suppressed
                && (entity.shadow_half_x != 0 || entity.shadow_half_z != 0)
            {
                Some(render::Shadow {
                    texture,
                    pos: [
                        entity.pos[0] + i32::from(entity.shadow_offset[0]),
                        entity.pos[1] + i32::from(entity.shadow_offset[1]),
                        entity.pos[2] + i32::from(entity.shadow_offset[2]),
                    ],
                    angle: entity.angle,
                    half_x: i32::from(entity.shadow_half_x),
                    half_z: i32::from(entity.shadow_half_z),
                    shift: [0, 0],
                    lift: shadow::offset_y(id, room.current_cut),
                    tint: shadow::billboard_tint(entity.shadow_tint),
                })
            } else {
                None
            };
            if let Some(mut shadow) = shadow {
                tween.shadow(
                    tween::Key::Npc(slot, entity.id),
                    &mut shadow.pos,
                    &mut shadow.angle,
                );
                shadow_list.push(shadow);
            }
        }
        if yawn_segment {
            continue;
        }
        let Some(model) = npc_models.get(pack, entity.id, id.player_flag & 1) else {
            continue;
        };
        // A Yawn head draws from its own fixed-point chain; every other
        // monster poses from the shared clock's keyframe.
        let mut joints = if let Some(worlds) = game.entity_anims[slot].yawn_render_worlds(entity) {
            worlds
        } else {
            let Some(keyframe) =
                game.entity_anims[slot].pose_keyframe(entity, &model.clips, &model.keyframes)
            else {
                continue;
            };
            let entity_matrix =
                anim::entity_matrix_scaled(entity.pos, entity.angle, entity.joint_scale);
            let mut joints = anim::joint_matrices(&model.skeleton, &keyframe, &entity_matrix);
            apply_look_at(&mut joints, entity, &game.entity_anims[slot]);
            joints
        };
        let hidden = joint_hidden_mask(
            entity.has_enter_switch_zone & 0x7f != 0,
            member_joint_class(entity.id),
            room,
            room.current_cut,
            entity.joint_flags,
            &joints,
        );
        tween.joints(tween::Key::Npc(slot, entity.id), &mut joints);
        models.push(model);
        npc_joints.push(std::mem::take(&mut joints));
        npc_hidden.push(hidden);
        npc_tints.push(entity.model_tint_rgb());
        npc_weapon_ids.push(crate::weapons::character_weapon_id(
            entity.id,
            entity.behavior_flags,
        ));
        npc_zone_gate.push(entity.has_enter_switch_zone & 0x7F != 0);
    }

    // The room's object and item models are submitted before the NPC and
    // player meshes (the original's room pass then the character pass): the
    // shared far-to-near sort is stable, so equal-depth triangles keep the
    // original's object < item < character tie order.
    let visible = visible_objects(
        room,
        &game.objects,
        room.current_cut,
        game.entities[0].pos,
        game.entities[0].angle,
    );
    let visible_item_models = visible_items(
        room,
        &game.items,
        &game.objects,
        &lighting,
        room.current_cut,
        game.entities[0].pos,
        game.entities[0].angle,
    );
    let mut object_joints: Vec<Vec<anim::Mat4x3>> = Vec::with_capacity(visible.len());
    for (_, _, world, slot) in &visible {
        let mut joints = vec![*world];
        tween.joints(tween::Key::Object(*slot), &mut joints);
        object_joints.push(joints);
    }
    let mut item_joints: Vec<Vec<anim::Mat4x3>> = Vec::with_capacity(visible_item_models.len());
    for (_, _, world, slot) in &visible_item_models {
        let mut joints = vec![*world];
        tween.joints(tween::Key::Item(*slot), &mut joints);
        item_joints.push(joints);
    }

    let mut meshes: Vec<EntityMesh<'_>> =
        Vec::with_capacity(visible.len() + visible_item_models.len() + 1 + models.len());
    for ((asset, record, _, _), joints) in visible.iter().zip(&object_joints) {
        meshes.push(EntityMesh {
            mesh: &asset.model,
            texture: &asset.texture,
            joints,
            tint: record.shade(),
            // The record's background blend weight: the TMD's first
            // semi-transparent primitive picks the ABR mode, and
            // `cmd_omodel_set`'s armed override (the water-tank surface) wins
            // over it, exactly like `GetTmdBlendMode` + `DAT_004d2be0`; the
            // creator's remap lives in `record_blend_weight`.
            blend_weight: render::record_blend_weight(&asset.model, record.blend_override),
            hidden_joints: 0,
        });
    }
    for ((asset, _, _, _), joints) in visible_item_models.iter().zip(&item_joints) {
        meshes.push(EntityMesh {
            mesh: &asset.model,
            texture: &asset.texture,
            joints,
            tint: [255; 3],
            blend_weight: render::record_blend_weight(&asset.model, None),
            hidden_joints: 0,
        });
    }
    // The NPC joint gates pair with `models`/`npc_joints`/`npc_hidden`, all
    // built together in entity-slot order above; do not re-walk the slots,
    // because a model whose keyframe lookup failed is absent from all three.
    for (((model, joints), hidden), tint) in models
        .iter()
        .zip(&npc_joints)
        .zip(&npc_hidden)
        .zip(&npc_tints)
    {
        meshes.push(EntityMesh {
            mesh: &model.mesh,
            texture: &model.texture,
            joints,
            tint: *tint,
            blend_weight: None,
            hidden_joints: *hidden,
        });
    }
    // The held-weapon meshes of the scripted characters draw with the
    // character pass: the `WS*.TMD` object rides the character's hand joint
    // and reuses the character model's texture page, exactly like the
    // original's weapon-slot TMD.
    let mut held_weapons: Vec<(usize, Arc<crate::model::Tmd>, Vec<anim::Mat4x3>)> = Vec::new();
    for (index, weapon_id) in npc_weapon_ids.iter().enumerate() {
        let Some(file) = *weapon_id else {
            continue;
        };
        // The held weapon is hidden with its character outside the switch
        // zone, exactly like the body's joint gate.
        if !npc_zone_gate[index] {
            continue;
        }
        let Some(tmd) = npc_models.weapon_tmd(pack, file) else {
            continue;
        };
        let Some(hand) = npc_joints[index].get(crate::weapons::WEAPON_JOINT) else {
            continue;
        };
        held_weapons.push((index, tmd, vec![*hand]));
    }
    for (index, tmd, joints) in &held_weapons {
        let texture = &models[*index].texture;
        meshes.push(EntityMesh {
            mesh: tmd,
            texture,
            joints,
            tint: npc_tints[*index],
            blend_weight: None,
            hidden_joints: 0,
        });
    }
    if let (Some(assets), Some(joints)) = (assets, &player_joints) {
        // The player's whole-entity gate reads the live `zoneFlags` low byte
        // (the camera-zone bit plus the interaction bits the scripts raise),
        // exactly like the entity pass.
        let hidden = joint_hidden_mask(
            game.entities[0].zone_flags & 0x7F != 0,
            member_joint_class(game.entities[0].id),
            room,
            room.current_cut,
            game.entities[0].joint_flags,
            joints,
        );
        meshes.push(EntityMesh {
            mesh: &assets.emd.mesh,
            texture: &assets.emd.texture,
            joints,
            tint: game.player_tint,
            blend_weight: None,
            hidden_joints: hidden,
        });
    }
    // The held weapon's mesh draws at the hand joint while the weapon bank
    // poses the body: the EMW's single mesh object is authored in joint 14's
    // space, exactly like the original's weapon-slot TMD.
    let mut weapon_joints: Vec<Vec<anim::Mat4x3>> = Vec::new();
    // The weapon slot's TMD is the hand joint's mesh whenever a weapon is
    // equipped, so it draws in every bank (idle, walk, turn), not just aiming.
    let weapon_file = player_state.weapon.weapon_file.or_else(|| {
        game.equipped
            .and_then(|item| crate::weapons::weapon_emw_id(id.player_flag & 3, item))
    });
    if let (Some(assets), Some(joints)) = (assets, &player_joints)
        && let Some(weapon) = assets.weapon(weapon_file)
        && !weapon.mesh.objects.is_empty()
        && let Some(hand) = joints.get(crate::weapons::WEAPON_JOINT)
    {
        weapon_joints.push(vec![*hand]);
        meshes.push(EntityMesh {
            mesh: &weapon.mesh,
            // The EMW mesh carries no texture block; its primitives sample the
            // player's own texture page, like the original's page-7 bank.
            texture: &assets.emd.texture,
            joints: &weapon_joints[0],
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
        });
    }

    // The web-thread clones draw after the entity pass and the player, the
    // point the original's 2D sprite list is flushed. Only clones whose parent
    // script ticked them this frame are live; each is posed from its frozen
    // spawn-time animation snapshot with the clone's own transform. A spent
    // clone also queues its web-coloured ground shadow.
    let mut web_models: Vec<Arc<Emd>> = Vec::new();
    let mut web_joints: Vec<Vec<anim::Mat4x3>> = Vec::new();
    let mut web_hidden: Vec<u32> = Vec::new();
    let mut web_tints: Vec<[u8; 3]> = Vec::new();
    for (clone_index, clone) in game.web_clones.iter().enumerate() {
        let Some(clone) = clone else {
            continue;
        };
        if !clone.live || clone.entity.has_enter_switch_zone == 0 {
            continue;
        }
        if clone.entity.status_flags == 0
            && (clone.entity.shadow_half_x != 0 || clone.entity.shadow_half_z != 0)
            && let Some(texture) = shadow_texture
        {
            let mut pos = [
                clone.entity.pos[0] + i32::from(clone.entity.shadow_offset[0]),
                clone.entity.pos[1] + i32::from(clone.entity.shadow_offset[1]),
                clone.entity.pos[2] + i32::from(clone.entity.shadow_offset[2]),
            ];
            let mut angle = clone.entity.angle;
            tween.shadow(tween::Key::Web(clone_index), &mut pos, &mut angle);
            shadow_list.push(render::Shadow {
                texture,
                pos,
                angle,
                half_x: i32::from(clone.entity.shadow_half_x),
                half_z: i32::from(clone.entity.shadow_half_z),
                shift: [0, 0],
                lift: shadow::offset_y(id, room.current_cut),
                tint: shadow::billboard_tint(clone.entity.shadow_tint),
            });
        }
        let Some(model) = npc_models.get(pack, clone.entity.id, id.player_flag & 1) else {
            continue;
        };
        let mut joints = clone.anim.joint_worlds(&clone.entity, &model.clips);
        if joints.is_empty() {
            continue;
        }
        let hidden = joint_hidden_mask(
            true,
            member_joint_class(clone.entity.id),
            room,
            room.current_cut,
            clone.entity.joint_flags,
            &joints,
        );
        tween.joints(tween::Key::Web(clone_index), &mut joints);
        web_models.push(model);
        web_joints.push(joints);
        web_hidden.push(hidden);
        web_tints.push(clone.entity.model_tint_rgb());
    }
    for (((model, joints), hidden), tint) in web_models
        .iter()
        .zip(&web_joints)
        .zip(&web_hidden)
        .zip(&web_tints)
    {
        meshes.push(EntityMesh {
            mesh: &model.mesh,
            texture: &model.texture,
            joints,
            tint: *tint,
            blend_weight: None,
            hidden_joints: *hidden,
        });
    }

    // The Plant 42 companions draw after the entity pass: each is the plant
    // model's own extra mesh object (the flower body and the root ball) at the
    // companion's transform and scale, with every other object hidden. Only
    // companions whose plant ticked them this frame are live; the flower body
    // also queues its ground quad.
    let mut companion_models: Vec<Arc<Emd>> = Vec::new();
    let mut companion_joints: Vec<Vec<anim::Mat4x3>> = Vec::new();
    let mut companion_hidden: Vec<u32> = Vec::new();
    let mut companion_tints: Vec<[u8; 3]> = Vec::new();
    for (companion_index, companion) in game.companions.iter().enumerate() {
        let Some(companion) = companion else {
            continue;
        };
        let heart = companion.kind == enemy::companion::CompanionKind::Heart;
        if !companion.live
            || !game
                .entities
                .get(usize::from(companion.owner))
                .is_some_and(|entity| entity.active())
            // The heart's `live` bit already carries the owner's switch-zone
            // entry (its own draw gate); the plant companions are gated on the
            // companion's own zone membership instead.
            || (!heart && !enemy::in_camera_zone(room, room.current_cut, companion.entity.pos))
        {
            continue;
        }
        if companion.kind == enemy::companion::CompanionKind::Body
            && (companion.entity.shadow_half_x != 0 || companion.entity.shadow_half_z != 0)
            && let Some(texture) = shadow_texture
        {
            let mut pos = [
                companion.entity.pos[0] + i32::from(companion.entity.shadow_offset[0]),
                companion.entity.pos[1] + i32::from(companion.entity.shadow_offset[1]),
                companion.entity.pos[2] + i32::from(companion.entity.shadow_offset[2]),
            ];
            let mut angle = companion.entity.angle;
            tween.shadow(tween::Key::Companion(companion_index), &mut pos, &mut angle);
            shadow_list.push(render::Shadow {
                texture,
                pos,
                angle,
                half_x: i32::from(companion.entity.shadow_half_x),
                half_z: i32::from(companion.entity.shadow_half_z),
                shift: [0, 0],
                lift: shadow::offset_y(id, room.current_cut),
                tint: shadow::billboard_tint(companion.entity.shadow_tint),
            });
        }
        let Some(model) = npc_models.get(pack, companion.entity.id, id.player_flag & 1) else {
            continue;
        };
        let object = companion.mesh_object();
        let mut matrix = companion.matrix.unwrap_or_else(|| {
            anim::entity_matrix_rotated(
                companion.entity.pos,
                companion.entity.pitch,
                companion.entity.angle,
                companion.entity.roll,
            )
        });
        enemy::custom_anim::scale_columns_xyz(&mut matrix, companion.render_scale());
        // One identity matrix per joint, with the companion's transform at
        // its own mesh object; every other object is hidden.
        let count = model.skeleton.relative.len().max(object + 1);
        let identity = anim::Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        };
        let mut joints = vec![identity; count];
        if let Some(slot) = joints.get_mut(object) {
            *slot = matrix;
        }
        tween.joints(tween::Key::Companion(companion_index), &mut joints);
        companion_models.push(model);
        companion_joints.push(joints);
        companion_hidden.push(!(1u32 << object));
        companion_tints.push(companion.entity.model_tint_rgb());
    }
    for (((model, joints), hidden), tint) in companion_models
        .iter()
        .zip(&companion_joints)
        .zip(&companion_hidden)
        .zip(&companion_tints)
    {
        meshes.push(EntityMesh {
            mesh: &model.mesh,
            texture: &model.texture,
            joints,
            tint: *tint,
            blend_weight: None,
            hidden_joints: *hidden,
        });
    }

    // The Tyrant's claw ghosts and severed limbs draw after the companion
    // pass: each is one mesh object of the Tyrant model at a scratch matrix
    // (the ghost copies of the claw, the free limbs the rocket death
    // launched). The live pose of those joints is already hidden by the
    // script's joint flags, so nothing double-draws.
    let mut tyrant_models: Vec<Arc<Emd>> = Vec::new();
    let mut tyrant_joints: Vec<Vec<anim::Mat4x3>> = Vec::new();
    let mut tyrant_hidden: Vec<u32> = Vec::new();
    let mut tyrant_tints: Vec<[u8; 3]> = Vec::new();
    for slot in 1..game::ENTITY_COUNT {
        let entity = &game.entities[slot];
        if !entity.active() || !matches!(entity.id, 0x0C | 0x10) {
            continue;
        }
        let mut draws: Vec<(usize, anim::Mat4x3)> = Vec::new();
        if entity.id != 0x0C && entity.ty_flags() & 8 == 0 && game.tyrant.ghost.live {
            draws.push((enemy::tyrant::CLAW_OBJECT, game.tyrant.ghost.matrices[0]));
            draws.push((enemy::tyrant::CLAW_OBJECT, game.tyrant.ghost.matrices[1]));
        }
        for (index, limb) in game.tyrant.limbs.iter().enumerate() {
            if limb.live {
                draws.push((enemy::tyrant::LIMB_JOINTS[index], limb.world));
            }
        }
        if draws.is_empty() {
            continue;
        }
        let Some(model) = npc_models.get(pack, entity.id, id.player_flag & 1) else {
            continue;
        };
        let count = model.skeleton.relative.len().max(
            draws
                .iter()
                .map(|(object, _)| object + 1)
                .max()
                .unwrap_or(0),
        );
        for (draw, (object, matrix)) in draws.into_iter().enumerate() {
            let identity = anim::Mat4x3 {
                r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                t: [0, 0, 0],
            };
            let mut joints = vec![identity; count];
            if let Some(target) = joints.get_mut(object) {
                *target = matrix;
            }
            tween.joints(tween::Key::Tyrant(slot, draw), &mut joints);
            tyrant_models.push(Arc::clone(&model));
            tyrant_joints.push(joints);
            tyrant_hidden.push(!(1u32 << object));
            tyrant_tints.push(entity.model_tint_rgb());
        }
    }
    for (((model, joints), hidden), tint) in tyrant_models
        .iter()
        .zip(&tyrant_joints)
        .zip(&tyrant_hidden)
        .zip(&tyrant_tints)
    {
        meshes.push(EntityMesh {
            mesh: &model.mesh,
            texture: &model.texture,
            joints,
            tint: *tint,
            blend_weight: None,
            hidden_joints: *hidden,
        });
    }

    let pages = effect_cache.pages_for(pack, id, &game.room_effects);
    let quads = build_effect_quads(game, room, id, &camera, pages, tween);
    let mut page_refs: Vec<Option<&Image>> = Vec::with_capacity(16);
    page_refs.extend(pages.base.iter().map(Option::as_ref));
    page_refs.extend(pages.variants.iter().map(Option::as_ref));
    let effect_layer = EffectLayer {
        pages: &page_refs,
        quads: &quads,
    };
    // The mirror pass runs only while the room script enables it; the reflected
    // camera and per-joint visibility close over the room mirror's geometry.
    let mirror = game.mirror_enabled().then(|| render::MirrorPass {
        axis_x: game.mirror_axis_x(),
        plane: i32::from(game.mirror.plane),
        extent_min: game.mirror.extent_min,
        extent_max: game.mirror.extent_max,
        camera_pos: cut.pos,
        camera: camera.mirrored(game.mirror_axis_x(), i32::from(game.mirror.plane)),
    });
    // The display origin pans the background; in gameplay it is the frame's
    // screen-shake offset and the global colour stays white. A screen-border
    // room (`msf2` bit 2, the caves) runs `ApplyShakeAndRebuildSprites`
    // instead, which parks the origin at (2, 2) so the inset 316x236
    // background has the two-pixel shake margin.
    let display_origin = if game.flag_test(5, game::MSF2_SCREEN_BORDER, false) {
        [shake[0] + 2, shake[1] + 2]
    } else {
        shake
    };
    let global_colour = [255; 3];
    let scene = render::GameplayScene {
        background: cut.background.as_ref(),
        origin: display_origin,
        colour: global_colour,
        meshes: &meshes,
        shadows: &shadow_list,
        camera: &camera,
        lighting: &lighting,
        mask_layer: layer.as_ref(),
        effect_layer: Some(&effect_layer),
        mirror_from: visible.len() + visible_item_models.len(),
        mirror: mirror.as_ref(),
    };
    let on_gpu = match gpu {
        Some(gpu) => match gpu.record_scene(&scene) {
            Ok(()) => {
                framebuffer.clear();
                true
            }
            Err(err) => {
                warn_gpu_scene_failed(&err);
                scene.draw(framebuffer);
                false
            }
        },
        None => {
            scene.draw(framebuffer);
            false
        }
    };
    draw_special_light(framebuffer, game, id, room.current_cut);
    on_gpu
}

/// Report a failed GPU scene once per run; the frame falls back to software.
fn warn_gpu_scene_failed(err: &anyhow::Error) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("warning: GPU scene failed ({err:#}); drawing it in software");
    }
}

/// The special-room-light overlay (`0x1C`): a full-screen saturated tint
/// alpha-blended over the scene; the state advances once per gameplay tick in
/// `tick_room`. Its absence (a parked state, an all-zero mask or a suppressing
/// room) draws nothing, and the guardhouse control room draws the same rect
/// twice.
fn draw_special_light(
    framebuffer: &mut Framebuffer,
    game: &game::GameState,
    id: RoomId,
    cut: usize,
) {
    if let Some(light) = game.special_light_rect(id, cut) {
        for _ in 0..light.draws {
            framebuffer.blend_rect([0, 0, WIDTH, HEIGHT], light.color, light.alpha);
        }
    }
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
    // The frame fills the largest centred 4:3 viewport: nearest at a whole
    // zoom (the exact 3x a capture reads back), pixel-art sampling between.
    let (mut width, mut height) = (0, 0);
    if !unsafe { SDL_GetCurrentRenderOutputSize(renderer, &mut width, &mut height) } {
        bail!("SDL_GetCurrentRenderOutputSize failed: {}", sdl_error());
    }
    let viewport = video::fit(width.max(1) as u32, height.max(1) as u32);
    let mode = if viewport.is_whole_zoom() {
        SDL_SCALEMODE_NEAREST
    } else {
        SDL_SCALEMODE_PIXELART
    };
    if !unsafe { SDL_SetTextureScaleMode(texture, mode) } {
        bail!("SDL_SetTextureScaleMode failed: {}", sdl_error());
    }
    let destination = SDL_FRect {
        x: viewport.x as f32,
        y: viewport.y as f32,
        w: viewport.width as f32,
        h: viewport.height as f32,
    };
    if !unsafe { SDL_RenderTexture(renderer, texture, std::ptr::null(), &destination) } {
        bail!("SDL_RenderTexture failed: {}", sdl_error());
    }
    Ok(())
}

/// Size and place `window` for `graphics` on `display`: borderless fullscreen
/// there, or the `window_width` x `window_height` window centred on it. A
/// window too large for the display shrinks to the largest whole multiple of
/// 320x240 that fits, in `graphics` too.
fn place_window(window: *mut SDL_Window, graphics: &mut Graphics, display: SDL_DisplayID) {
    if graphics.fullscreen {
        unsafe {
            SDL_SetWindowPosition(
                window,
                SDL_WINDOWPOS_CENTERED_DISPLAY(display),
                SDL_WINDOWPOS_CENTERED_DISPLAY(display),
            );
            // A null mode is the desktop mode: borderless fullscreen.
            SDL_SetWindowFullscreenMode(window, std::ptr::null());
            SDL_SetWindowFullscreen(window, true);
            SDL_SyncWindow(window);
        }
        return;
    }
    unsafe {
        SDL_SetWindowFullscreen(window, false);
    }
    // A window that does not fit its display shrinks to the largest whole
    // multiple of 320x240 that does.
    if let Some((room_width, room_height)) = window_room(window, display)
        && (graphics.window_width > room_width || graphics.window_height > room_height)
    {
        (graphics.window_width, graphics.window_height) =
            video::largest_window((room_width, room_height));
    }
    // The size is in window coordinates; the client area is to be exactly
    // `window_width` x `window_height` pixels.
    let density = window_pixel_density(window);
    let coordinates = |pixels: u32| (pixels as f32 / density).round() as i32;
    unsafe {
        SDL_SetWindowSize(
            window,
            coordinates(graphics.window_width),
            coordinates(graphics.window_height),
        );
        SDL_SetWindowPosition(
            window,
            SDL_WINDOWPOS_CENTERED_DISPLAY(display),
            SDL_WINDOWPOS_CENTERED_DISPLAY(display),
        );
        SDL_SyncWindow(window);
    }
}

/// The client area, in pixels, a window on `display` can have with its
/// decorations inside the display's usable area (less the task bar).
fn window_room(window: *mut SDL_Window, display: SDL_DisplayID) -> Option<(u32, u32)> {
    let mut usable = SDL_Rect::default();
    if !unsafe { SDL_GetDisplayUsableBounds(display, &mut usable) } {
        return None;
    }
    let (mut top, mut left, mut bottom, mut right) = (0, 0, 0, 0);
    if !unsafe { SDL_GetWindowBordersSize(window, &mut top, &mut left, &mut bottom, &mut right) } {
        // Unknown decorations: allow for a typical title bar.
        top = 32;
    }
    // In pixels, like `window_width` and `window_height`.
    let density = window_pixel_density(window);
    let pixels = |coordinates: i32| (coordinates.max(0) as f32 * density) as u32;
    Some((
        pixels(usable.w).saturating_sub(pixels(left + right)),
        pixels(usable.h).saturating_sub(pixels(top + bottom)),
    ))
}

/// Pixels per window coordinate (1 unless the platform scales windows).
fn window_pixel_density(window: *mut SDL_Window) -> f32 {
    let density = unsafe { SDL_GetWindowPixelDensity(window) };
    if density > 0.0 { density } else { 1.0 }
}

/// SDL's displays, in its order.
fn displays() -> Vec<SDL_DisplayID> {
    let mut count = 0;
    let list = unsafe { SDL_GetDisplays(&mut count) };
    if list.is_null() {
        return Vec::new();
    }
    let ids = unsafe { std::slice::from_raw_parts(list, count.max(0) as usize) }.to_vec();
    unsafe { SDL_free(list.cast()) };
    ids
}

/// The display a 1-based `display` option names, else the primary display.
fn display_id(number: u32) -> SDL_DisplayID {
    let index = number.saturating_sub(1) as usize;
    displays()
        .get(index)
        .copied()
        .unwrap_or_else(|| unsafe { SDL_GetPrimaryDisplay() })
}

/// The 1-based `display` option for `display`.
fn display_number(display: SDL_DisplayID) -> Option<u32> {
    let index = displays().iter().position(|&id| id == display)?;
    Some(index as u32 + 1)
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
    let emd = match emd::parse(&emd_bytes) {
        Ok(model) => model,
        Err(err) => {
            eprintln!("warning: invalid player model {emd_path}: {err:#}");
            return None;
        }
    };
    let emw = match emd::parse_emw(&emw_bytes) {
        Ok(model) => model,
        Err(err) => {
            eprintln!("warning: invalid player clips {emw_path}: {err:#}");
            return None;
        }
    };
    // Load the character block's weapon animations: every distinct EMW file
    // id the weapon table names. A missing file is logged and skipped, and the
    // weapon machine falls back to the locomotion pose.
    let mut weapons = std::collections::HashMap::new();
    for weapon in 0..=0x0Du8 {
        let Some(file) = crate::weapons::weapon_emw_id(character, weapon) else {
            continue;
        };
        if weapons.contains_key(&file) {
            continue;
        }
        let Some(entry) = crate::weapons::weapon_emw_entry(character, weapon) else {
            continue;
        };
        match pack.read(&entry) {
            Ok(bytes) => match emd::parse_emw(&bytes) {
                Ok(model) => {
                    weapons.insert(file, model);
                }
                Err(err) => eprintln!("warning: invalid weapon clips {entry}: {err:#}"),
            },
            Err(err) => eprintln!("warning: missing weapon clips {entry}: {err:#}"),
        }
    }
    Some(PlayerAssets { emd, emw, weapons })
}

/// Load the room's shipped primary track from the pack, if the table names
/// one. Kept as the M5 real-pack check for the raw `bgm/` entries; the
/// running engine goes through [`bgm::BgmCache`] instead.
#[cfg(test)]
fn load_room_music(pack: &Pack, id: RoomId) -> Option<audio::Wav> {
    let (name, _looping) = crate::music::primary_track(id)?;
    let path = crate::music::pack_path(name)?;
    match pack.read(&path) {
        Ok(bytes) => match audio::parse_wav(&bytes) {
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

fn sdl_error() -> String {
    unsafe { CStr::from_ptr(SDL_GetError()) }
        .to_string_lossy()
        .into_owned()
}

fn window_title(room: &str, cut: usize, count: usize) -> String {
    format!("Arklay - room {room} cut {cut}/{}", count - 1)
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

    capture_bmp(
        &Image {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            rgba,
        },
        path,
    )
}

/// Write one capture BMP through a sibling temporary file, so a failed
/// capture never leaves a partial artifact behind.
fn capture_bmp(image: &Image, path: &Path) -> Result<()> {
    let data = bmp::encode_to_vec(image)?;
    crate::atomic::write(path, &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClipFrame, Emw, Keyframe, Skeleton, Texture8, Tmd};
    use crate::pack::PackWriter;
    use sdl3_sys::scancode::{
        SDL_SCANCODE_BACKSPACE, SDL_SCANCODE_C, SDL_SCANCODE_DOWN, SDL_SCANCODE_ESCAPE,
        SDL_SCANCODE_F1, SDL_SCANCODE_F9, SDL_SCANCODE_LEFT, SDL_SCANCODE_LEFTBRACKET,
        SDL_SCANCODE_LSHIFT, SDL_SCANCODE_RETURN, SDL_SCANCODE_RIGHT, SDL_SCANCODE_RIGHTBRACKET,
        SDL_SCANCODE_RSHIFT, SDL_SCANCODE_SPACE, SDL_SCANCODE_TAB, SDL_SCANCODE_UP, SDL_SCANCODE_X,
        SDL_SCANCODE_Z,
    };

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
    fn visible_objects_filters_active_zone_and_asset_in_slot_order() {
        use crate::objects::{OBJECT_FLAG_ACTIVE, ObjectAsset, ObjectTable};

        let asset = |pair: usize| ObjectAsset {
            pair_index: pair,
            model: crate::model::Tmd::default(),
            texture: crate::model::Texture8 {
                width: 0,
                height: 0,
                indices: Vec::new(),
                palettes: Vec::new(),
                stp: Vec::new(),
            },
        };
        let room = RoomState {
            object_models: vec![asset(0), asset(1), asset(2)],
            zones: vec![crate::state::Zone {
                cam_to: 0,
                cam_from: 0,
                corners: [[0, 0], [0, 1000], [1000, 1000], [1000, 0]],
            }],
            ..RoomState::default()
        };
        let mut objects = ObjectTable::new(3);
        // Slot 0 is inactive, slot 1 is active inside the zone and slot 2 is
        // active but outside it.
        {
            let record = objects.record_mut(0).unwrap();
            record.flag = 0;
            record.asset = Some(0);
        }
        {
            let record = objects.record_mut(1).unwrap();
            record.flag = OBJECT_FLAG_ACTIVE;
            record.asset = Some(1);
            record.pos = [500, 0, 500];
        }
        {
            let record = objects.record_mut(2).unwrap();
            record.flag = OBJECT_FLAG_ACTIVE;
            record.asset = Some(2);
            record.pos = [5000, 0, 5000];
        }

        let visible = visible_objects(&room, &objects, 0, [0, 0, 0], 0);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].0.pair_index, 1);
        assert_eq!(visible[0].1, objects.record(1).unwrap());
        assert_eq!(visible[0].2.t, [500, 0, 500]);

        // A record whose declared pair failed to decode is skipped.
        objects.record_mut(1).unwrap().asset = Some(9);
        assert!(visible_objects(&room, &objects, 0, [0, 0, 0], 0).is_empty());

        // A camera with no zone keeps every record out.
        objects.record_mut(1).unwrap().asset = Some(1);
        assert!(visible_objects(&room, &objects, 3, [0, 0, 0], 0).is_empty());
    }

    #[test]
    fn visible_items_filter_active_zone_and_asset_and_compose_parents() {
        use crate::objects::{ITEM_FLAG_ACTIVE, ItemTable, ObjectAsset, ObjectTable};

        let asset = |pair: usize| ObjectAsset {
            pair_index: pair,
            model: crate::model::Tmd::default(),
            texture: crate::model::Texture8 {
                width: 0,
                height: 0,
                indices: Vec::new(),
                palettes: Vec::new(),
                stp: Vec::new(),
            },
        };
        let room = RoomState {
            item_models: vec![asset(0), asset(1), asset(2)],
            zones: vec![crate::state::Zone {
                cam_to: 0,
                cam_from: 0,
                corners: [[0, 0], [0, 1000], [1000, 1000], [1000, 0]],
            }],
            ..RoomState::default()
        };
        let lighting = Lighting::from_room(&room);
        let mut items = ItemTable::new(3);
        // Slot 0 is inactive, slot 1 active inside the zone and slot 2 active
        // but outside it.
        {
            let record = items.record_mut(1).unwrap();
            record.flag = ITEM_FLAG_ACTIVE;
            record.asset = Some(1);
            record.pos = [500, 0, 500];
        }
        {
            let record = items.record_mut(2).unwrap();
            record.flag = ITEM_FLAG_ACTIVE;
            record.asset = Some(2);
            record.pos = [5000, 0, 5000];
        }

        let no_objects = ObjectTable::new(0);
        let visible = visible_items(&room, &items, &no_objects, &lighting, 0, [0, 0, 0], 0);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].0.pair_index, 1);
        assert_eq!(visible[0].1, items.record(1).unwrap());
        assert_eq!(visible[0].2.t, [500, 0, 500]);

        // A pair that failed to decode is skipped, and a camera with no zone
        // keeps every item out.
        items.record_mut(1).unwrap().asset = Some(9);
        assert!(visible_items(&room, &items, &no_objects, &lighting, 0, [0, 0, 0], 0).is_empty());
        items.record_mut(1).unwrap().asset = Some(1);
        assert!(visible_items(&room, &items, &no_objects, &lighting, 3, [0, 0, 0], 0).is_empty());

        // A player-parented item rides the entity matrix.
        items.record_mut(1).unwrap().parent = 0xFE;
        items.record_mut(1).unwrap().pos = [10, 0, 0];
        let visible = visible_items(&room, &items, &no_objects, &lighting, 0, [400, 0, 0], 0);
        assert_eq!(visible[0].2.t, [409, 0, 0]);

        // An omodel-parented item composes the parent's rebuild and still
        // culls by the composed point.
        let mut objects = ObjectTable::new(1);
        objects.record_mut(0).unwrap().pos = [200, 0, 0];
        items.record_mut(1).unwrap().parent = 0x00;
        items.record_mut(1).unwrap().pos = [50, 0, 0];
        let visible = visible_items(&room, &items, &objects, &lighting, 0, [0, 0, 0], 0);
        assert_eq!(visible[0].2.t, [249, 0, 0]);
    }

    #[test]
    fn object_render_suppressions_match_the_room_cases() {
        use crate::objects::{OBJECT_FLAG_ACTIVE, ObjectRecord};

        let record = |pos: [i32; 3]| ObjectRecord {
            flag: OBJECT_FLAG_ACTIVE,
            model: 0,
            pos,
            ..ObjectRecord::default()
        };
        let room = |stage: u8, room: u8| RoomState {
            stage,
            room,
            ..RoomState::default()
        };

        // Guardhouse 002, camera 4, model type 0.
        assert!(object_render_skipped(&room(4, 0x06), 4, &record([0, 0, 0])));
        assert!(!object_render_skipped(
            &room(4, 0x06),
            3,
            &record([0, 0, 0])
        ));

        // Lab B3 private room, cameras 0 and 4.
        assert!(object_render_skipped(&room(5, 0x0A), 0, &record([0, 0, 0])));
        assert!(object_render_skipped(&room(5, 0x0A), 4, &record([0, 0, 0])));
        assert!(!object_render_skipped(
            &room(5, 0x0B),
            0,
            &record([0, 0, 0])
        ));

        // Mansion 1F trap room, camera 0, only at the shipped position.
        assert!(object_render_skipped(
            &room(1, 0x15),
            0,
            &record([0x12FC, -0x2828, 0x12FC])
        ));
        assert!(!object_render_skipped(
            &room(1, 0x15),
            0,
            &record([0, 0, 0])
        ));

        // Courtyard boulder passage, camera 3, once X passes 0x7274.
        assert!(object_render_skipped(
            &room(3, 0x0F),
            3,
            &record([0x7275, 0, 0])
        ));
        assert!(!object_render_skipped(
            &room(3, 0x0F),
            3,
            &record([0x7274, 0, 0])
        ));

        // A type other than 0 is never suppressed by the stand-in rules.
        let mut typed = record([0, 0, 0]);
        typed.model = 1;
        assert!(!object_render_skipped(&room(4, 0x06), 4, &typed));
    }

    #[test]
    fn the_heliport_shifts_display_objects_along_the_view_axis() {
        use crate::objects::{OBJECT_FLAG_ACTIVE, ObjectAsset, ObjectRecord, ObjectTable};

        let asset = ObjectAsset {
            pair_index: 0,
            model: crate::model::Tmd::default(),
            texture: crate::model::Texture8 {
                width: 0,
                height: 0,
                indices: Vec::new(),
                palettes: Vec::new(),
                stp: Vec::new(),
            },
        };
        let room = RoomState {
            stage: 3,
            room: 0x03,
            object_models: vec![asset],
            zones: vec![crate::state::Zone {
                cam_to: 0,
                cam_from: 0,
                corners: [[0, 0], [0, 1000], [1000, 1000], [1000, 0]],
            }],
            ..RoomState::default()
        };
        let mut objects = ObjectTable::new(1);
        let record = objects.record_mut(0).unwrap();
        *record = ObjectRecord {
            flag: OBJECT_FLAG_ACTIVE,
            model: 1,
            asset: Some(0),
            pos: [500, 0, 500],
            ..ObjectRecord::default()
        };

        let visible = visible_objects(&room, &objects, 0, [0, 0, 0], 0);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].2.t, [500, 0, 1500], "display items move +Z");

        // Type 0 (and out-of-range types) stay put.
        objects.record_mut(0).unwrap().model = 0;
        let visible = visible_objects(&room, &objects, 0, [0, 0, 0], 0);
        assert_eq!(visible[0].2.t, [500, 0, 500]);
        objects.record_mut(0).unwrap().model = 5;
        let visible = visible_objects(&room, &objects, 0, [0, 0, 0], 0);
        assert_eq!(visible[0].2.t, [500, 0, 500]);
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
        synthetic_rdt_with_tables(init, 1, &[], &[])
    }

    /// Minimal RDT with `cameras` camera records plus the given camera-switch
    /// and walkable zone tables.
    fn synthetic_rdt_with_tables(
        init: &[u8],
        cameras: u8,
        zones: &[crate::state::Zone],
        walk_zones: &[crate::state::WalkZone],
    ) -> Vec<u8> {
        const INIT_POINTER: usize = 0x48 + 6 * 4;
        const MAIN_POINTER: usize = 0x48 + 7 * 4;
        const EVENT_POINTER: usize = 0x48 + 8 * 4;
        const ZONES_POINTER: usize = 0x48;
        const WALK_ZONES_POINTER: usize = 0x48 + 4 * 4;

        let mut data = vec![0u8; 0x94];
        data[0x01] = cameras;
        // 44 bytes per camera record (the RDT's first cut starts at 0x94).
        data.extend(std::iter::repeat_n(0u8, usize::from(cameras) * 44));
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }

        let init_offset = data.len();
        data.extend_from_slice(&container(&[init]));
        let main_offset = data.len();
        data.extend_from_slice(&[0, 0, 0, 0]);
        let events_offset = data.len();
        data.extend_from_slice(&[0, 0, 0, 0]);

        if !zones.is_empty() {
            let offset = data.len();
            for zone in zones {
                data.extend_from_slice(&zone.cam_to.to_le_bytes());
                data.extend_from_slice(&zone.cam_from.to_le_bytes());
                for corner in zone.corners {
                    data.extend_from_slice(&corner[0].to_le_bytes());
                    data.extend_from_slice(&corner[1].to_le_bytes());
                }
            }
            // The parser stops at the first cam_from outside the camera range.
            data.extend_from_slice(&[0xFF; 0x14]);
            data[ZONES_POINTER..ZONES_POINTER + 4].copy_from_slice(&(offset as u32).to_le_bytes());
        }
        if !walk_zones.is_empty() {
            // The table is a count byte plus a pad byte, then 12-byte records.
            let offset = data.len();
            data.push(u8::try_from(walk_zones.len()).unwrap());
            data.push(0);
            for zone in walk_zones {
                for value in [zone.x1, zone.z1, zone.x2, zone.z2] {
                    data.extend_from_slice(&value.to_le_bytes());
                }
                data.extend_from_slice(&zone.field_08.to_le_bytes());
                data.extend_from_slice(&zone.flags.to_le_bytes());
            }
            data[WALK_ZONES_POINTER..WALK_ZONES_POINTER + 4]
                .copy_from_slice(&(offset as u32).to_le_bytes());
        }

        for (pointer, offset) in [
            (INIT_POINTER, init_offset),
            (MAIN_POINTER, main_offset),
            (EVENT_POINTER, events_offset),
        ] {
            data[pointer..pointer + 4].copy_from_slice(&(offset as u32).to_le_bytes());
        }
        data
    }

    /// `door_aot_set(0, 100, 200, 300, 400, ..., RDT_001, 555, 0, 666, 1024, 0, 0x81)`.
    fn door_init() -> Vec<u8> {
        door_init_with_camera(0)
    }

    /// `synthetic_rdt` plus one kind-1 collision rectangle around the origin.
    ///
    /// The rectangle is grown by the player radius in the collision query and
    /// still does not reach the door spawn at `[555, 0, 666]`, so a placement
    /// test can tell the blocked start from the clear arrival.
    fn synthetic_rdt_with_blocking_collision(init: &[u8]) -> Vec<u8> {
        const COLLISION_POINTER: usize = 0x48 + 4;

        let mut data = synthetic_rdt(init);
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }
        let offset = data.len();
        // Cell origin, then one record in quadrant 0.
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        for count in [1i32, 0, 0, 0, 0] {
            data.extend_from_slice(&count.to_le_bytes());
        }
        // x_max, z_max, x_min, z_min, kind 1, flags.
        for value in [100u16, 100, 0, 0, 1, 0] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data[COLLISION_POINTER..COLLISION_POINTER + 4]
            .copy_from_slice(&(offset as u32).to_le_bytes());
        data
    }

    /// `door_init` with a specific record camera byte (`+0x0B`).
    fn door_init_with_camera(camera: u8) -> Vec<u8> {
        door_init_record(1, [555, 0, 666], 1024, camera)
    }

    /// A `door_aot_set` with the given destination, arrival and angle:
    /// `door_aot_set(0, 100, 200, 300, 400, 0, 0, 0, camera, 0, next_room,
    /// x, y, z, angle, 0, 0x81)`.
    fn door_init_record(next_room: u8, next_pos: [i16; 3], angle: i16, camera: u8) -> Vec<u8> {
        let mut body = vec![0x0C, 0x00];
        for value in [100i16, 200, 300, 400] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(&[0, 0, 0, camera, 0]);
        body.push(next_room);
        for value in [next_pos[0], next_pos[1], next_pos[2], angle] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        // Required item 0, probe flags 0x81 (action key, forward reach probe).
        body.extend_from_slice(&[0, 0x81]);
        body
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }

    /// Hash only the RGB channels: the framebuffer keeps alpha 0 in cleared
    /// pixels while a BMP round-trip normalises it to 255.
    fn rgb_fnv1a(image: &Image) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for pixel in image.rgba.as_chunks::<4>().0 {
            for &byte in &pixel[..3] {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
        hash
    }

    fn non_black_pixels(image: &Image) -> usize {
        image
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
            .count()
    }

    #[test]
    fn capture_writes_bmp() {
        let _sdl = crate::audio::test_lock::sdl();
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

        run(&pack_path, id, Some(&capture_path), 0).unwrap();

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
        let room = rdt::parse(&pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
        let assets = load_player_assets(&pack, id).expect("player assets");
        let mut player_state = player::spawn(id, &room);
        let mut masks = MaskCache::default();
        let mut shadows = ShadowCache::default();
        let mut npc_models = enemy::EntityModelCache::default();

        let mut idle = Framebuffer::new();
        render_frame(
            &mut idle,
            &pack,
            id,
            &room,
            &player_state,
            &mut game::GameState::new(id, &room),
            Some(&assets),
            &mut npc_models,
            &mut masks,
            &mut shadows,
            &mut EffectPageCache::default(),
        );

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
        render_frame(
            &mut walking,
            &pack,
            id,
            &room,
            &player_state,
            &mut game::GameState::new(id, &room),
            Some(&assets),
            &mut npc_models,
            &mut masks,
            &mut shadows,
            &mut EffectPageCache::default(),
        );

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
        let rdt_bytes = &*pack.read(&id.rdt_entry()).unwrap();
        let room = rdt::parse(rdt_bytes, id).unwrap();
        let scripts = scd::reader::parse(rdt_bytes).unwrap();

        let mut game = game::GameState::new(id, &room);
        // The new-game room-items bank registers the item edges; the
        // second-playthrough flag keeps ROOM1001's ink ribbon, which Jill's
        // first playthrough skips.
        game.flags[7]
            .bytes_mut()
            .copy_from_slice(&NEW_GAME_ROOM_ITEMS);
        game.apply_flag(0, 0x7B, 0);
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);
        {
            let mut host = game::ScdGameHost::new(&mut game);
            command_vm.run_init(&mut host);
        }
        assert_eq!(game.state_bytes[2], 0, "the camera id starts at zero");
        assert_eq!(
            game.state_bytes[usize::from(game::STATE_BYTE_CHARACTER)],
            1,
            "the character byte carries the player flag"
        );
        // ROOM1001's init writes its per-room BGM row; the live state byte is
        // not touched by `room_bgm_state_set`.
        assert_eq!(game.room_bgm[0], 0x09, "init should write the BGM table");
        assert_eq!(
            game.bgm.state, 0xFF,
            "nothing is playing before the handoff"
        );
        assert!(!game.camera.locked);

        for _ in 0..10 {
            let mut host = game::ScdGameHost::new(&mut game);
            command_vm.run_main(&mut host);
            event_vm.step(&mut host);
            game.advance_frame();
        }

        assert_eq!(game.frame, 10);
        assert!(
            game.placeholders.is_empty(),
            "the implemented opcode set must not record placeholders: {:?}",
            game.placeholders
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
        assert!(
            game.placeholders.is_empty(),
            "ROOM1001's init must run without placeholders: {:?}",
            game.placeholders
        );
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
        // The door's probe point is 600 units ahead of the player (facing +X),
        // so stand short of the zone and let the reach point land inside it.
        player_state.pos = [-450, 0, 250];
        player_state.angle = 0;
        game.sync_entity_from_player(&player_state);

        let transition = {
            let scripts = &loaded.scripts;
            let mut command_vm = scd::vm::CommandVm::new(scripts);
            let mut event_vm = scd::vm::EventVm::new(scripts);
            {
                let mut host = game::ScdGameHost::new(&mut game);
                command_vm.run_init(&mut host);
            }
            assert!(game.doors[0].is_some(), "init should register the door");

            let mut npc_models = enemy::EntityModelCache::default();
            let idle = tick_room(
                &mut command_vm,
                &mut event_vm,
                RoomContext {
                    room: &mut loaded.room,
                    game: &mut game,
                    player: &mut player_state,
                    player_assets: None,
                    pack: &pack,
                    npc_models: &mut npc_models,
                    enemy_lua: None,
                },
                player::Input::default(),
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
                    pack: &pack,
                    npc_models: &mut npc_models,
                    enemy_lua: None,
                },
                player::Input {
                    action_pressed: true,
                    action_held: true,
                    ..player::Input::default()
                },
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
    fn a_door_transition_snapshots_and_restores_an_enemy() {
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
            .add(&a.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer
            .add(&b.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&a.cut_entry(0), bmp_bytes.clone()).unwrap();
        writer.add(&b.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();

        let mut loaded = load_room(&pack, a).unwrap();
        let mut game = game::GameState::new(a, &loaded.room);
        let mut player_state = player::spawn(a, &loaded.room);
        // One live, non-force monster in room A.
        let entity = &mut game.entities[1];
        entity.id = 0x13;
        entity.set_active(true);
        entity.status_flags = 0x41;
        entity.behavior_flags = 3;
        entity.health = 55;
        entity.pos = [111, 0, 222];
        entity.angle = 0x400;
        entity.variant = 0x30;
        entity.death_event_id = 2;
        game.enemy_count = 1;

        // A -> B snapshots the monster and clears the live list.
        let to_b = game::RoomTransition {
            target: b,
            pos: [0, 0, 0],
            angle: 0,
        };
        loaded = enter_transition(&pack, &mut game, &mut player_state, &to_b).unwrap();
        assert_eq!(loaded.id, b);
        assert_eq!(game.enemy_count, 0);
        let saved = game.saved_enemies[0];
        assert!(saved.occupied());
        assert_eq!(saved.room, a.room);
        assert_eq!(saved.enemy_type, 0x30);
        assert_eq!(saved.pos, [111, 0, 222]);

        // B -> A ages the snapshot; the destination's no-force record
        // restores it instead of using the record's position.
        let to_a = game::RoomTransition {
            target: a,
            pos: [0, 0, 0],
            angle: 0,
        };
        loaded = enter_transition(&pack, &mut game, &mut player_state, &to_a).unwrap();
        assert_eq!(loaded.id, a);
        assert_eq!(
            game.saved_enemies[0].valid, 4,
            "one room change aged the TTL"
        );
        let operands: Vec<scd::ir::Operand> = [
            0x13u8 as i64, // id
            3,             // behavior
            0xFF,          // guard
            0,             // force off
            2,             // SCA words
            0,             // rotation X
            0,             // yaw
            0,             // rotation Z
            999,           // X (ignored on a restore hit)
            0,             // Y
            999,           // Z
            0,             // slot nibble
            0,             // animation id
            0,             // animation frame
            3,             // variant high nibble
        ]
        .into_iter()
        .map(|value| scd::ir::Operand {
            value,
            target: None,
        })
        .collect();
        assert!(game.spawn_enemy(&operands));
        assert_eq!(game.entities[1].pos, [111, 0, 222], "restored position");
        assert_eq!(game.entities[1].angle, 0x400);
        assert_eq!(game.entities[1].behavior_flags, 3);
        assert_eq!(game.entities[1].id, 0x13, "the reset block writes the id");
        assert!(!game.saved_enemies[0].occupied(), "the slot was consumed");
    }

    /// A two-room pack (`100` and `101`) whose rooms block the origin and
    /// whose doors pair: A's door arrives in B at `[777, 0, 888]` and B's door
    /// arrives in A at `[555, 0, 666]`.
    fn debug_menu_pack(dir: &TempDir) -> (Pack, RoomId, RoomId) {
        let pack_path = dir.0.join("game.akpak");
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
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        writer
            .add(
                &a.rdt_entry(),
                synthetic_rdt_with_blocking_collision(&door_init_record(
                    1,
                    [777, 0, 888],
                    0x200,
                    0,
                )),
            )
            .unwrap();
        writer
            .add(
                &b.rdt_entry(),
                synthetic_rdt_with_blocking_collision(&door_init_record(0, [555, 0, 666], 1024, 0)),
            )
            .unwrap();
        writer.add(&a.cut_entry(0), bmp_bytes.clone()).unwrap();
        writer.add(&b.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();
        (Pack::open(&pack_path).unwrap(), a, b)
    }

    #[test]
    fn f1_latches_one_debug_menu_edge_per_press() {
        let mut input = InputState::default();
        input.key_down(SDL_SCANCODE_F1, false);
        assert!(input.tick().ui.debug_menu, "the press is one edge");
        assert!(!input.tick().ui.debug_menu, "a held F1 does not repeat");
        input.key_up(SDL_SCANCODE_F1);
        input.key_down(SDL_SCANCODE_F1, false);
        assert!(input.tick().ui.debug_menu, "a fresh press is a fresh edge");
    }

    #[test]
    fn f9_latches_one_return_title_edge_per_press() {
        let mut input = InputState::default();
        input.key_down(SDL_SCANCODE_F9, false);
        assert!(input.tick().ui.return_title, "the press is one edge");
        assert!(!input.tick().ui.return_title, "a held F9 does not repeat");
        input.key_up(SDL_SCANCODE_F9);
        input.key_down(SDL_SCANCODE_F9, false);
        assert!(
            input.tick().ui.return_title,
            "a fresh press is a fresh edge"
        );
    }

    #[test]
    fn f1_opens_the_debug_overlay_by_default() {
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        let frame = session.game.frame;

        session
            .tick(
                &pack,
                UiInput {
                    debug_menu: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();

        assert!(
            session.debug_menu.is_some(),
            "the F1 edge opens the overlay"
        );
        assert_eq!(session.game.frame, frame, "the overlay freezes the room");
    }

    #[test]
    fn f1_still_opens_the_debug_overlay_when_the_flag_is_passed() {
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        session.set_debug_menu(true);

        session
            .tick(
                &pack,
                UiInput {
                    debug_menu: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();

        assert!(session.debug_menu.is_some(), "F1 opens the overlay");
    }

    #[test]
    fn the_open_debug_overlay_consumes_navigation_and_freezes_the_room() {
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        session.set_debug_menu(true);
        session
            .tick(
                &pack,
                UiInput {
                    debug_menu: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.debug_menu.is_some(), "F1 opens the overlay");
        assert!(session.room_frozen(), "the room freezes while it is open");

        let pos = session.player.pos;
        let frame = session.game.frame;
        session
            .tick(
                &pack,
                UiInput {
                    down: true,
                    ..UiInput::default()
                },
                player::Input {
                    up: true,
                    ..player::Input::default()
                },
                false,
            )
            .unwrap();
        assert_eq!(session.debug_menu.as_ref().unwrap().main_cursor(), 1);
        assert_eq!(session.player.pos, pos, "the room does not move");
        assert_eq!(session.game.frame, frame, "the room does not tick");

        session
            .tick(
                &pack,
                UiInput {
                    debug_menu: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.debug_menu.is_none(), "F1 closes the overlay");
    }

    /// The arbitrary start and the debug jump place the player at the
    /// neighbour's paired door record, not at the target's own first door
    /// spawn (which is an arrival in the neighbour).
    #[test]
    fn debug_jump_places_the_player_at_the_paired_door_arrival() {
        let dir = TempDir::new();
        let (pack, a, b) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        // A's entry is B's record: its arrival for A is `[555, 0, 666]`.
        assert_eq!(session.player.pos, [555, 0, 666], "A's paired arrival");
        assert_eq!(session.player.angle, 1024);
        assert!(
            !player::position_blocked(
                &session.loaded.room,
                session.player.pos,
                session.player.radius,
                session.player.collision_flags
            ),
            "the start lands outside collision"
        );

        session.set_debug_menu(true);
        session
            .tick(
                &pack,
                UiInput {
                    debug_menu: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        // ROOM SELECT opens on the current room; the next row is B.
        debug_press(&mut session, &pack, |ui| ui.confirm = true);
        debug_press(&mut session, &pack, |ui| ui.down = true);
        debug_press(&mut session, &pack, |ui| ui.confirm = true);

        assert!(session.debug_menu.is_none(), "confirm closes the overlay");
        assert_eq!(session.loaded.id, b);
        assert_eq!(session.game.id, b);
        // B's entry is A's record: its arrival for B is `[777, 0, 888]`, not
        // B's own record arrival `[555, 0, 666]`.
        assert_eq!(session.player.pos, [777, 0, 888], "B's paired arrival");
        assert_eq!(session.player.angle, 0x200);
        assert!(
            !player::position_blocked(
                &session.loaded.room,
                session.player.pos,
                session.player.radius,
                session.player.collision_flags
            ),
            "the arrival lands outside collision"
        );
    }

    /// F1 closes the overlay without forgetting the tool or its cursors.
    #[test]
    fn f1_reopens_the_debug_overlay_on_the_last_tool() {
        use crate::ui::debug_menu::Tool;
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        debug_enter(&mut session, &pack, Tool::Flags);
        debug_press(&mut session, &pack, |ui| ui.right = true);
        let (group, row) = {
            let menu = session.debug_menu.as_ref().unwrap();
            (menu.flags().group(), menu.flags().row())
        };
        assert_eq!(group, 1, "the FLAGS group moved off the first one");

        debug_press(&mut session, &pack, |ui| ui.debug_menu = true);
        assert!(session.debug_menu.is_none(), "F1 hides the overlay");
        debug_press(&mut session, &pack, |ui| ui.debug_menu = true);
        let menu = session.debug_menu.as_ref().unwrap();
        assert_eq!(menu.tool(), Some(Tool::Flags), "the tool is remembered");
        assert_eq!(menu.flags().group(), group, "and so is the group");
        assert_eq!(menu.flags().row(), row, "and the row");
    }

    /// The room browser follows a debug jump: reopening puts the cursor on
    /// the room the jump landed in, not the one the menu was opened from.
    #[test]
    fn f1_after_a_debug_jump_reopens_the_room_tool_on_the_new_room() {
        use crate::ui::debug_menu::Tool;
        let dir = TempDir::new();
        let (pack, a, b) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        debug_enter(&mut session, &pack, Tool::Rooms);
        debug_press(&mut session, &pack, |ui| ui.down = true);
        debug_press(&mut session, &pack, |ui| ui.confirm = true);
        assert_eq!(session.loaded.id, b, "the jump landed in B");
        assert!(session.debug_menu.is_none(), "the jump closes the overlay");

        debug_press(&mut session, &pack, |ui| ui.debug_menu = true);
        let menu = session.debug_menu.as_ref().unwrap();
        assert_eq!(menu.tool(), Some(Tool::Rooms), "ROOM SELECT is remembered");
        let selected = menu.rooms().selected().unwrap();
        assert_eq!(
            (selected.stage, selected.room),
            (b.stage, b.room),
            "the cursor follows the jumped-to room"
        );
    }

    /// An arbitrary-room boot selects the placement's cut from camera 0, like
    /// `room_set`'s switch-zone scan. Starting from the cut the walk-zone spawn
    /// selected can miss the group-0 zone that covers the door arrival: room
    /// 101 spawns in the cut-1 corridor and arrives at the stairwell, whose
    /// cut 3 is only reached from group 0 (#83).
    #[test]
    fn arbitrary_room_boot_rehomes_the_camera_from_zero_at_the_entry() {
        use crate::state::{WalkZone, Zone};

        let dir = TempDir::new();
        let pack_path = dir.0.join("entry-cut.akpak");
        let a = RoomId::parse("1000").unwrap();
        let b = RoomId::parse("1010").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        // A's record supplies B's paired arrival; B's own door points back to
        // A, which is how `room_entry_placement` recognises the pair.
        writer
            .add(
                &a.rdt_entry(),
                synthetic_rdt(&door_init_record(1, [50, 0, 50], 0x200, 0)),
            )
            .unwrap();
        writer
            .add(
                &b.rdt_entry(),
                synthetic_rdt_with_tables(
                    &door_init_record(0, [0, 0, 0], 0, 0),
                    3,
                    &[
                        // Group 0 header, then the walk-zone spawn's zone
                        // (switches to cut 2) and the door arrival's zone
                        // (switches to cut 1).
                        Zone {
                            cam_to: 0,
                            cam_from: 0,
                            corners: [[0; 2]; 4],
                        },
                        Zone {
                            cam_to: 2,
                            cam_from: 0,
                            corners: [[0, 0], [0, 20], [20, 20], [20, 0]],
                        },
                        Zone {
                            cam_to: 1,
                            cam_from: 0,
                            corners: [[40, 40], [40, 60], [60, 60], [60, 40]],
                        },
                        // Group 2 header: there is no path from cut 2 to cut 1,
                        // so a scan that starts there stays on cut 2.
                        Zone {
                            cam_to: 0,
                            cam_from: 2,
                            corners: [[0; 2]; 4],
                        },
                    ],
                    &[WalkZone {
                        x1: 0,
                        z1: 0,
                        x2: 20,
                        z2: 20,
                        field_08: 0,
                        flags: 0,
                    }],
                ),
            )
            .unwrap();
        for cut in 0..3 {
            writer.add(&b.cut_entry(cut), bmp_bytes.clone()).unwrap();
        }
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();

        let mut session = GameSession::from_room(&pack, b, Path::new("saves")).unwrap();
        assert_eq!(session.player.pos, [50, 0, 50], "the paired arrival");
        assert_eq!(
            session.loaded.room.current_cut, 1,
            "the arrival's group-0 zone wins over the spawn's cut 2"
        );
        assert_eq!(session.game.camera.current_cut, 1);

        // A deferred redisplay the spawn scan raised describes the old camera;
        // it must not make `apply_camera` skip the placement's entry scan.
        session.game.set_camera_cut(2);
        session.game.flags[5].apply(game::MSF_CAMERA_REDRAW, 0);
        session.place_room_entry(&pack, b);
        assert_eq!(
            session.loaded.room.current_cut, 1,
            "the entry scan still runs with a pending redraw"
        );
        assert_eq!(session.game.camera.current_cut, 1);
    }

    /// A debug jump into a room with no door or walk zone still re-homes the
    /// camera at the destination spawn. `enter_room` scans against the source
    /// room's position before the jump replaces the player, so the post-spawn
    /// scan is what selects the spawn's cut.
    #[test]
    fn debug_jump_rehomes_the_camera_at_a_room_without_an_entry() {
        use crate::state::{WalkZone, Zone};

        let dir = TempDir::new();
        let pack_path = dir.0.join("no-entry.akpak");
        let source = RoomId::parse("1000").unwrap();
        let target = RoomId::parse("1100").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        // The source's only walk zone puts the player at `[100, 0, 100]`,
        // inside the target's group-0 switch zone.
        writer
            .add(
                &source.rdt_entry(),
                synthetic_rdt_with_tables(
                    &[],
                    1,
                    &[],
                    &[WalkZone {
                        x1: 50,
                        z1: 50,
                        x2: 150,
                        z2: 150,
                        field_08: 0,
                        flags: 0,
                    }],
                ),
            )
            .unwrap();
        writer.add(&source.cut_entry(0), bmp_bytes.clone()).unwrap();
        // The target has no doors and no walk zones: its spawn is the origin,
        // outside every group-0 zone, so the spawn's scan must land on cut 0.
        writer
            .add(
                &target.rdt_entry(),
                synthetic_rdt_with_tables(
                    &[],
                    2,
                    &[
                        Zone {
                            cam_to: 0,
                            cam_from: 0,
                            corners: [[0; 2]; 4],
                        },
                        Zone {
                            cam_to: 1,
                            cam_from: 0,
                            corners: [[50, 50], [50, 150], [150, 150], [150, 50]],
                        },
                    ],
                    &[],
                ),
            )
            .unwrap();
        for cut in 0..2 {
            writer
                .add(&target.cut_entry(cut), bmp_bytes.clone())
                .unwrap();
        }
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();

        let mut session = GameSession::from_room(&pack, source, Path::new("saves")).unwrap();
        assert_eq!(session.player.pos, [100, 0, 100]);
        session.debug_jump(&pack, target).unwrap();
        assert_eq!(session.player.pos, [0, 0, 0], "the target's spawn");
        assert_eq!(
            session.loaded.room.current_cut, 0,
            "the spawn's scan replaces the source-position cut"
        );
        assert_eq!(session.game.camera.current_cut, 0);
    }

    /// One session tick with a single UI edge.
    fn debug_press(session: &mut GameSession, pack: &Pack, set: impl FnOnce(&mut UiInput)) {
        let mut ui = UiInput::default();
        set(&mut ui);
        session
            .tick(pack, ui, player::Input::default(), false)
            .unwrap();
    }

    /// Open the debug menu and walk its main list to `tool`, then confirm.
    fn debug_enter(session: &mut GameSession, pack: &Pack, tool: crate::ui::debug_menu::Tool) {
        debug_press(session, pack, |ui| ui.debug_menu = true);
        while crate::ui::debug_menu::TOOLS[session.debug_menu.as_ref().unwrap().main_cursor()]
            != tool
        {
            debug_press(session, pack, |ui| ui.down = true);
        }
        debug_press(session, pack, |ui| ui.confirm = true);
    }

    #[test]
    fn debug_menu_cancel_backs_out_one_level_and_f1_closes_from_a_tool() {
        use crate::ui::debug_menu::Tool;
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        let frame = session.game.frame;
        debug_enter(&mut session, &pack, Tool::Flags);
        assert_eq!(
            session.debug_menu.as_ref().unwrap().tool(),
            Some(Tool::Flags)
        );
        debug_press(&mut session, &pack, |ui| ui.cancel = true);
        assert_eq!(
            session.debug_menu.as_ref().unwrap().tool(),
            None,
            "back to the list"
        );
        debug_press(&mut session, &pack, |ui| ui.cancel = true);
        assert!(session.debug_menu.is_none(), "cancel on the list closes");
        debug_enter(&mut session, &pack, Tool::Cheats);
        debug_press(&mut session, &pack, |ui| ui.debug_menu = true);
        assert!(session.debug_menu.is_none(), "F1 closes from inside a tool");
        assert_eq!(
            session.game.frame, frame,
            "the room stayed frozen throughout"
        );
    }

    #[test]
    fn invincible_pins_health_and_skips_the_death_machine() {
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        session.game.debug.invincible = true;
        // Settle into the pad-driven state before the lethal hit lands.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.game.entities[0].state(), 1);
        session.game.entities[0].health = -5;
        session.game.set_health_status(0x80 | 0x62);
        session.game.poison_timer = 30;
        session.game.apply_flag(5, game::MSF_PLAYER_DEAD, 0);
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.game.entities[0].health, session.game.max_health);
        assert_eq!(
            session.game.health_status, 0x80,
            "only the poison bits clear"
        );
        assert_eq!(session.game.poison_timer, 0);
        assert!(!session.game.flags[5].bit(game::MSF_PLAYER_DEAD));
        assert!(session.death.is_idle(), "no death sequence starts");
        assert_ne!(
            session.game.entities[0].state(),
            3,
            "a lethal hit in the same tick never starts the death fall"
        );
    }

    #[test]
    fn debug_save_opens_the_save_screen_without_a_ribbon() {
        use crate::ui::debug_menu::Tool;
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, &dir.0.join("saves")).unwrap();
        debug_enter(&mut session, &pack, Tool::Save);
        debug_press(&mut session, &pack, |ui| ui.confirm = true);
        assert!(
            session.debug_menu.is_none(),
            "the menu hands over to the screen"
        );
        assert!(
            session.save_screen.is_some(),
            "SAVE GAME opens the save screen"
        );
    }

    #[test]
    fn debug_load_opens_the_load_screen_and_raises_the_slot_request() {
        use crate::ui::debug_menu::Tool;
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, &dir.0.join("saves")).unwrap();
        debug_enter(&mut session, &pack, Tool::Save);
        debug_press(&mut session, &pack, |ui| ui.down = true);
        debug_press(&mut session, &pack, |ui| ui.confirm = true);
        assert!(
            session.save_screen.is_some(),
            "LOAD GAME opens the load screen"
        );
        assert_eq!(session.take_load_slot_request(), None, "nothing picked yet");
    }

    #[test]
    fn debug_item_box_opens_the_box_anywhere() {
        use crate::ui::debug_menu::Tool;
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        debug_enter(&mut session, &pack, Tool::ItemBox);
        assert!(session.debug_menu.is_none());
        assert!(
            session.item_box.is_some(),
            "the box opens without a box in the room"
        );
    }

    #[test]
    fn debug_clear_game_raises_the_ending_request_once() {
        use crate::ui::debug_menu::Tool;
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        // Chris with neither survivor: ending 4 is the default.
        debug_enter(&mut session, &pack, Tool::ClearGame);
        assert_eq!(session.debug_menu.as_ref().unwrap().ending(), 4);
        debug_press(&mut session, &pack, |ui| ui.confirm = true);
        assert!(session.debug_menu.is_none());
        assert_eq!(session.take_clear_game_request(), Some(4));
        assert_eq!(session.take_clear_game_request(), None, "taken once");
        let (flow, character) = session.ending_flow(4);
        assert_eq!(character, 0);
        assert_eq!(flow.stats.ending_id, 4);
        assert!(!flow.second_playthrough);
    }

    #[test]
    fn a_pending_outfit_applies_on_the_debug_jump() {
        let dir = TempDir::new();
        let (pack, a, b) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        session.game.debug.pending_player = Some(game::PlayerChoice {
            character: 0,
            costume: 2,
        });
        session.debug_jump(&pack, b).unwrap();
        assert_eq!(session.game.debug.pending_player, None, "applied");
        assert!(session.game.flag_test(5, game::MSF2_COSTUME_VARIANT, false));
        assert_eq!(session.game.costume_variant, 1);
        // Jill is pending but the pack has no Jill rooms: the door keeps the
        // Chris variant and the change stays pending.
        session.game.debug.pending_player = Some(game::PlayerChoice {
            character: 1,
            costume: 0,
        });
        assert_eq!(session.pending_player_target(&pack, a), a);
        session.debug_jump(&pack, a).unwrap();
        assert_eq!(
            session
                .game
                .debug
                .pending_player
                .map(|choice| choice.character),
            Some(1),
            "the character change waits for a Jill room"
        );
        assert!(!session.game.flag_test(5, game::MSF2_COSTUME_VARIANT, false));
    }

    /// [`debug_menu_pack`] plus a Jill variant of the second room and
    /// Chris's first alternate outfit.
    fn debug_costume_pack(dir: &TempDir) -> (Pack, RoomId, RoomId) {
        let path = dir.0.join("costume.akpak");
        let (a, b) = (
            RoomId::parse("1000").unwrap(),
            RoomId::parse("1010").unwrap(),
        );
        let b_jill = RoomId::parse("1011").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        for (id, door) in [
            (a, door_init_record(1, [777, 0, 888], 0x200, 0)),
            (b, door_init_record(0, [555, 0, 666], 1024, 0)),
            (b_jill, door_init_record(0, [555, 0, 666], 1024, 0)),
        ] {
            writer
                .add(
                    &id.rdt_entry(),
                    synthetic_rdt_with_blocking_collision(&door),
                )
                .unwrap();
            // The backgrounds are shared between the character variants.
            if id.player_flag == 0 {
                writer.add(&id.cut_entry(0), bmp_bytes.clone()).unwrap();
            }
        }
        writer
            .add(
                &crate::convert::costume_entry(0, 0),
                crate::emd::tests::minimal_emd(),
            )
            .unwrap();
        writer.write(&path).unwrap();
        (Pack::open(&path).unwrap(), a, b)
    }

    #[test]
    fn a_pending_jill_rewrites_the_door_target_to_her_variant() {
        let dir = TempDir::new();
        let (pack, a, b) = debug_costume_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        assert_eq!(
            session.pending_player_target(&pack, b),
            b,
            "nothing pending"
        );
        session.game.debug.pending_player = Some(game::PlayerChoice {
            character: 1,
            costume: 0,
        });
        let b_jill = session.pending_player_target(&pack, b);
        assert_eq!(
            b_jill,
            RoomId {
                player_flag: 1,
                ..b
            }
        );
        // The door transition loads the rewritten target, as the menu's
        // jump already does.
        session.debug_jump(&pack, b_jill).unwrap();
        assert_eq!(session.loaded.id, b_jill);
        assert_eq!(session.game.debug.pending_player, None);
        assert_eq!(session.game.max_health, 96, "Jill's maximum");
    }

    #[test]
    fn the_outfit_bit_swaps_in_the_alternate_player_model() {
        let dir = TempDir::new();
        let (pack, a, _) = debug_costume_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        session.loaded.player_assets = Some(synthetic_player_assets());
        session.apply_costume_model(&pack);
        assert_eq!(
            session
                .loaded
                .player_assets
                .as_ref()
                .unwrap()
                .emd
                .texture
                .width,
            0,
            "the base model without the outfit bit"
        );
        session.game.apply_flag(5, game::MSF2_COSTUME_VARIANT, 0);
        session.apply_costume_model(&pack);
        assert_eq!(
            session
                .loaded
                .player_assets
                .as_ref()
                .unwrap()
                .emd
                .texture
                .width,
            2,
            "player/00_alt0.emd replaced it"
        );
        // A missing variant keeps the current model.
        session.game.costume_variant = 1;
        session.loaded.player_assets = Some(synthetic_player_assets());
        session.apply_costume_model(&pack);
        assert_eq!(
            session
                .loaded
                .player_assets
                .as_ref()
                .unwrap()
                .emd
                .texture
                .width,
            0
        );
    }

    #[test]
    fn f9_opens_the_return_prompt_and_freezes_the_room() {
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        let frame = session.game.frame;

        session
            .tick(
                &pack,
                UiInput {
                    return_title: true,
                    any: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();

        assert!(session.return_title.is_some(), "F9 opens the prompt");
        assert!(session.room_frozen(), "the room freezes while it is up");
        assert_eq!(session.game.frame, frame, "the room does not tick");

        // The prompt holds the freeze on the following ticks.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert!(session.return_title.is_some());
        assert_eq!(session.game.frame, frame, "the room stays frozen");
    }

    #[test]
    fn the_second_f9_requests_the_return_once() {
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        let f9 = UiInput {
            return_title: true,
            any: true,
            ..UiInput::default()
        };

        session
            .tick(&pack, f9, player::Input::default(), false)
            .unwrap();
        assert!(
            session.return_title.is_some(),
            "the first F9 opens the prompt"
        );

        session
            .tick(&pack, f9, player::Input::default(), false)
            .unwrap();
        assert!(
            session.return_title.is_none(),
            "the second F9 closes the prompt"
        );
        assert!(
            session.take_return_title_request(),
            "the second F9 sets the return request"
        );
        assert!(
            !session.take_return_title_request(),
            "the request is taken once"
        );
    }

    #[test]
    fn any_other_key_cancels_the_return_prompt_and_resumes_the_sounds() {
        let _sdl = crate::audio::test_lock::sdl();
        let Some(mixer) = dummy_mixer() else {
            eprintln!("skipping return-title audio test: no audio device");
            return;
        };
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        session.music = Some(mixer);

        session
            .tick(
                &pack,
                UiInput {
                    return_title: true,
                    any: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(
            session.music.as_ref().unwrap().game_sounds_paused(),
            "F9 pauses the game sounds"
        );

        session
            .tick(
                &pack,
                UiInput {
                    cancel: true,
                    any: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(
            session.return_title.is_none(),
            "the cancel closes the prompt"
        );
        assert!(
            !session.music.as_ref().unwrap().game_sounds_paused(),
            "the cancel resumes the game sounds"
        );
        assert!(!session.return_title_requested, "no return was requested");
    }

    #[test]
    fn f9_opens_the_return_prompt_over_the_pause_menu() {
        let dir = TempDir::new();
        let (pack, a, _) = debug_menu_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        session.open_menu(&pack);
        assert!(session.menu.is_some(), "the pause menu is up");

        session
            .tick(
                &pack,
                UiInput {
                    return_title: true,
                    any: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();

        assert!(
            session.return_title.is_some(),
            "F9 opens over the pause menu"
        );
        assert!(session.menu.is_some(), "the menu stays underneath");
        assert!(session.room_frozen(), "the room stays frozen");
    }

    #[test]
    fn pending_task_kills_deactivate_slots_before_the_next_step() {
        use crate::scd::host::{EventRequest, PlaceholderHost};
        use crate::scd::ir::{Decoded, Insn, Operand, Scripts, Stream, StreamKind};
        use crate::scd::opcode::{Op, event_control_op};

        fn insn(offset: usize, op: &'static Op, len: usize, values: &[i64]) -> Insn {
            Insn {
                offset,
                op: op.op,
                bytes: vec![op.op; len],
                decoded: Decoded::Event(op),
                operands: values
                    .iter()
                    .map(|&value| Operand {
                        value,
                        target: None,
                    })
                    .collect(),
            }
        }

        let sleep = event_control_op(0xF8).unwrap();
        let sleep_tick = event_control_op(0xF9).unwrap();
        let finish = event_control_op(0xFF).unwrap();
        // Event 0 sleeps essentially forever; event 1 finishes at once.
        let scripts = Scripts {
            events: vec![
                Stream {
                    kind: StreamKind::Event(0),
                    offset: 0,
                    insns: vec![
                        insn(0x6000, sleep, 4, &[0xF9, 0x7FFF]),
                        insn(0x6004, sleep_tick, 1, &[]),
                        insn(0x6005, finish, 1, &[]),
                    ],
                    trailing: Vec::new(),
                },
                Stream {
                    kind: StreamKind::Event(1),
                    offset: 0,
                    insns: vec![insn(0x7000, finish, 1, &[])],
                    trailing: Vec::new(),
                },
            ],
            ..Scripts::default()
        };

        let mut event_vm = scd::vm::EventVm::new(&scripts);
        event_vm.start(3, 0);
        event_vm.start(4, 1);
        assert_eq!(event_vm.active_slots(), 2);

        let mut game = game::GameState::default();
        game.pending_event_requests
            .extend([EventRequest::Kill(3), EventRequest::Kill(3)]);
        apply_event_requests(&mut game, &mut event_vm);
        assert!(
            game.pending_event_requests.is_empty(),
            "the request queue drains once"
        );
        assert!(!event_vm.kill(3), "a repeated kill is a no-op");
        assert!(!event_vm.kill(9), "an out-of-range slot is a no-op");

        event_vm.step(&mut PlaceholderHost);
        assert_eq!(
            event_vm.active_slots(),
            0,
            "the killed sleeping slot must not run"
        );
    }

    #[test]
    fn task_kill_and_evt_exec_apply_in_program_order() {
        use crate::scd::host::EventRequest;
        use crate::scd::ir::{Block, Decoded, Insn, Operand, Scripts, Stream, StreamKind};
        use crate::scd::opcode::{Op, command_op, event_control_op};

        fn command_insn(offset: usize, op: &'static Op, len: usize, values: &[i64]) -> Insn {
            Insn {
                offset,
                op: op.op,
                bytes: vec![op.op; len],
                decoded: Decoded::Command(op),
                operands: values
                    .iter()
                    .map(|&value| Operand {
                        value,
                        target: None,
                    })
                    .collect(),
            }
        }

        let evt_exec = command_op(0x14).unwrap();
        let task_kill = command_op(0x44).unwrap();
        let end = command_op(0x00).unwrap();
        let finish = event_control_op(0xFF).unwrap();

        // Event 0x1B exists; the other slots' streams are empty.
        let mut events: Vec<Stream> = (0..=0x1B)
            .map(|index| Stream {
                kind: StreamKind::Event(index as u8),
                offset: 0,
                insns: Vec::new(),
                trailing: Vec::new(),
            })
            .collect();
        events[0x1B] = Stream {
            kind: StreamKind::Event(0x1B),
            offset: 0,
            insns: vec![Insn {
                offset: 0x9000,
                op: finish.op,
                bytes: vec![0xFF],
                decoded: Decoded::Control(finish),
                operands: Vec::new(),
            }],
            trailing: Vec::new(),
        };
        let event_scripts = Scripts {
            events,
            ..Scripts::default()
        };

        // `kill_first` selects the ROOM3030 shape (`task_kill 6` then
        // `evt_exec 0, 6, event_1B`) vs. the reverse.
        let run = |kill_first: bool| {
            let first = if kill_first {
                command_insn(0x1000, task_kill, 2, &[6])
            } else {
                command_insn(0x1000, evt_exec, 4, &[0, 6, 0x1B])
            };
            let second_offset = 0x1000 + first.bytes.len();
            let second = if kill_first {
                command_insn(second_offset, evt_exec, 4, &[0, 6, 0x1B])
            } else {
                command_insn(second_offset, task_kill, 2, &[6])
            };
            let end_offset = second_offset + second.bytes.len();
            let scripts = Scripts {
                main: vec![Block {
                    offset: 0,
                    size: 0,
                    insns: vec![first, second, command_insn(end_offset, end, 2, &[0])],
                    trailing: Vec::new(),
                }],
                ..Scripts::default()
            };

            let mut command_vm = scd::vm::CommandVm::new(&scripts);
            let mut game = game::GameState::default();
            {
                let mut host = game::ScdGameHost::new(&mut game);
                command_vm.run_main(&mut host);
            }
            let expected = if kill_first {
                vec![
                    EventRequest::Kill(6),
                    EventRequest::Start {
                        slot: 6,
                        event: 0x1B,
                    },
                ]
            } else {
                vec![
                    EventRequest::Start {
                        slot: 6,
                        event: 0x1B,
                    },
                    EventRequest::Kill(6),
                ]
            };
            assert_eq!(game.pending_event_requests, expected, "program order");

            let mut event_vm = scd::vm::EventVm::new(&event_scripts);
            apply_event_requests(&mut game, &mut event_vm);
            assert!(game.pending_event_requests.is_empty());
            assert_eq!(
                event_vm.active_slots(),
                usize::from(kill_first),
                "a kill before the start leaves the new event running; a kill \
                 after it stops the event"
            );
        };
        run(true);
        run(false);
    }

    #[test]
    fn tick_room_steps_the_desk_countdown_before_the_scripts() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("game.akpak");
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };

        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let mut loaded = load_room(&pack, id).unwrap();
        let mut game = game::GameState::new(id, &loaded.room);
        let mut player_state = player::spawn(id, &loaded.room);
        // The desk sits mid-pan; the empty script cannot advance it.
        game.desk.state = 35;
        {
            let scripts = &loaded.scripts;
            let mut command_vm = scd::vm::CommandVm::new(scripts);
            let mut event_vm = scd::vm::EventVm::new(scripts);
            let mut npc_models = enemy::EntityModelCache::default();
            for _ in 0..2 {
                tick_room(
                    &mut command_vm,
                    &mut event_vm,
                    RoomContext {
                        room: &mut loaded.room,
                        game: &mut game,
                        player: &mut player_state,
                        player_assets: None,
                        pack: &pack,
                        npc_models: &mut npc_models,
                        enemy_lua: None,
                    },
                    player::Input::default(),
                );
            }
        }
        assert_eq!(game.desk.state, 33, "the per-frame desk state machine ran");
    }

    #[test]
    fn reset_room_camera_id_keeps_a_locked_script_cut() {
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let mut game = game::GameState::new(id, &RoomState::default());

        // The block's saved id or a pre-scan script write is discarded, so the
        // boot walk always starts from camera 0.
        game.set_camera_cut(2);
        reset_room_camera_id(&mut game);
        assert_eq!(game.camera.current_cut, 0);
        assert_eq!(
            game.state_bytes[usize::from(game::STATE_BYTE_ROOM_CAMERA)],
            0
        );

        // A room script that re-locked the camera keeps the cut it selected:
        // `room_set` takes `display_room_camera_bg`'s branch instead.
        game.set_camera_cut(2);
        game.camera.locked = true;
        reset_room_camera_id(&mut game);
        assert_eq!(
            game.camera.current_cut, 2,
            "the lock keeps the script's cut"
        );
        assert_eq!(
            game.state_bytes[usize::from(game::STATE_BYTE_ROOM_CAMERA)],
            2
        );
    }

    #[test]
    fn apply_camera_starts_the_zone_walk_from_the_games_cut() {
        use crate::state::{Cut, Zone};

        let mut room = RoomState {
            cuts: vec![Cut::default(); 3],
            ..RoomState::default()
        };
        // A camera-0 group header plus one switch zone under cut 0.
        room.zones = vec![
            Zone {
                cam_to: 0,
                cam_from: 0,
                corners: [[0; 2]; 4],
            },
            Zone {
                cam_to: 1,
                cam_from: 0,
                corners: [[0, 0], [0, 100], [100, 100], [100, 0]],
            },
        ];
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let mut game = game::GameState::new(id, &room);

        // A script (`setb 2`) or the desk selected cut 2 this tick while the
        // room still carries its previous cut. The walk must start from the
        // game's cut so the camera-0 switch zone cannot pull it back.
        game.set_camera_cut(2);
        assert_eq!(room.current_cut, 0, "the room is on its previous cut");
        apply_camera(&mut room, &mut game, Some([50, 0, 50]));
        assert_eq!(game.camera.current_cut, 2, "the mid-tick cut survives");
        assert_eq!(room.current_cut, 2);

        // From cut 0 the same position follows the switch zone.
        room.current_cut = 0;
        game.set_camera_cut(0);
        apply_camera(&mut room, &mut game, Some([50, 0, 50]));
        assert_eq!(game.camera.current_cut, 1, "the zone switch still works");
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_room_401_desk_cut_survives_the_zone_scan() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("4010").unwrap();
        let mut loaded = load_room(&pack, id).unwrap();
        let mut game = game::GameState::new(id, &loaded.room);
        // The new-game room-items bank registers the desk's item edge.
        game.flags[7]
            .bytes_mut()
            .copy_from_slice(&NEW_GAME_ROOM_ITEMS);
        let mut player_state = player::spawn(id, &loaded.room);
        // Stand in front of the desk, facing -Z so the reach lands in its zone.
        player_state.pos = [10100, 0, 11950];
        player_state.angle = 0x400;
        game.sync_entity_from_player(&player_state);
        {
            let scripts = &loaded.scripts;
            let mut command_vm = scd::vm::CommandVm::new(scripts);
            let mut host = game::ScdGameHost::new(&mut game);
            command_vm.run_init(&mut host);
        }
        assert_eq!(
            game.room_actions
                .iter()
                .flatten()
                .filter(|action| action.kind == game::RoomActionKind::Desk)
                .count(),
            1,
            "ROOM4010 registers its desk"
        );
        // Unlock the desk and hold the key: the action probe opens it.
        game.apply_flag(2, 0x21, 0);
        game.add_item(0x3D, 1);

        let mut command_vm = scd::vm::CommandVm::new(&loaded.scripts);
        let mut event_vm = scd::vm::EventVm::new(&loaded.scripts);
        let mut npc_models = enemy::EntityModelCache::default();
        // The first tick is the player's state-0 init, which reads no pad;
        // the press lands on the tick after it.
        for input in [
            player::Input::default(),
            player::Input {
                action_pressed: true,
                action_held: true,
                ..player::Input::default()
            },
        ] {
            tick_room(
                &mut command_vm,
                &mut event_vm,
                RoomContext {
                    room: &mut loaded.room,
                    game: &mut game,
                    player: &mut player_state,
                    player_assets: None,
                    pack: &pack,
                    npc_models: &mut npc_models,
                    enemy_lua: None,
                },
                input,
            );
        }
        assert_eq!(game.desk.state, 35, "the action probe opened the desk");
        assert_eq!(game.camera.current_cut, 3, "the desk cut is selected");
        assert_eq!(
            loaded.room.current_cut, 3,
            "the desk cut survived the frame's zone scan"
        );
        assert_eq!(game.sfx_requests, vec![0x24]);
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_stairwell_door_loads_its_destination_room() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("1060").unwrap();
        let mut loaded = load_room(&pack, id).unwrap();
        let mut game = game::GameState::new(id, &loaded.room);
        let mut player_state = player::spawn(id, &loaded.room);
        {
            let scripts = &loaded.scripts;
            let mut command_vm = scd::vm::CommandVm::new(scripts);
            let mut host = game::ScdGameHost::new(&mut game);
            command_vm.run_init(&mut host);
        }

        // Stand short of the stairwell door so the reach probe lands in it.
        let door = game.doors[4].expect("stair door");
        let center = [
            i32::from(door.zone[0]) + i32::from(door.zone[2]) / 2,
            0,
            i32::from(door.zone[1]) + i32::from(door.zone[3]) / 2,
        ];
        let (dx, dz) = player::reach_offset(0);
        player_state.pos = [center[0] - dx, 0, center[2] - dz];
        player_state.angle = 0;
        game.sync_entity_from_player(&player_state);
        {
            let mut host = game::ScdGameHost::new(&mut game);
            host.interact(player_state.pos, player_state.angle, true);
        }
        let transition = game.transition.take().expect("stair transition");
        assert_eq!(
            transition.target,
            RoomId {
                stage: 2,
                room: 3,
                player_flag: 0
            }
        );

        loaded = enter_transition(&pack, &mut game, &mut player_state, &transition).unwrap();
        assert_eq!(loaded.id.stage, 2);
        assert_eq!(loaded.id.room, 3);
        assert_eq!(game.id, transition.target);
        assert_eq!(player_state.pos, [17100, 0, 25300]);
        assert_eq!(player_state.angle, 3072);
        assert!(!loaded.room.cuts.is_empty(), "destination cameras loaded");
        // The entry camera comes from the destination's switch zones, applied
        // during the transition load.
        assert_eq!(
            loaded.room.current_cut,
            player::camera_for_position(&loaded.room, 0, player_state.pos)
        );
        assert_eq!(game.camera.current_cut, loaded.room.current_cut);
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn tick_room_ramps_the_player_up_the_lab_stairway() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("40D0").unwrap();
        let mut loaded = load_room(&pack, id).unwrap();
        let mut game = game::GameState::new(id, &loaded.room);
        let mut player_state = player::spawn(id, &loaded.room);
        let scripts = &loaded.scripts;
        let mut command_vm = scd::vm::CommandVm::new(scripts);
        let mut event_vm = scd::vm::EventVm::new(scripts);
        {
            let mut host = game::ScdGameHost::new(&mut game);
            command_vm.run_init(&mut host);
        }

        // On the lab stairway's clear lane, walking west up the ramp.
        player_state.pos = [24000, 0, 3600];
        player_state.angle = 0x800;
        game.sync_entity_from_player(&player_state);
        let start = player_state.pos;
        let mut npc_models = enemy::EntityModelCache::default();
        for _ in 0..30 {
            tick_room(
                &mut command_vm,
                &mut event_vm,
                RoomContext {
                    room: &mut loaded.room,
                    game: &mut game,
                    player: &mut player_state,
                    player_assets: loaded.player_assets.as_ref(),
                    pack: &pack,
                    npc_models: &mut npc_models,
                    enemy_lua: None,
                },
                player::Input {
                    up: true,
                    ..player::Input::default()
                },
            );
        }
        assert!(
            player_state.pos[0] < start[0],
            "the tick loop did not walk the ramp: {start:?} -> {:?}",
            player_state.pos
        );
        assert!(
            player_state.pos[1] > start[1] + 300,
            "the tick loop did not ramp the height: {start:?} -> {:?}",
            player_state.pos
        );
        assert_eq!(
            game.entities[0].pos[1], player_state.pos[1],
            "the entity height is not synced from the stair state"
        );
        assert_eq!(player_state.stairs.height, game.stair_height);
    }

    /// The M16 slice-2 acceptance: two `--ticks 60 --capture` runs of the
    /// deterministic room are byte-identical under the exact camera and shake.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_room100_ticks_capture_is_deterministic() {
        let _sdl = crate::audio::test_lock::sdl();
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let dir = TempDir::new();
        let first_path = dir.0.join("m16_first.bmp");
        let second_path = dir.0.join("m16_second.bmp");
        let id = RoomId::parse("1000").unwrap();
        run(Path::new(&path), id, Some(&first_path), 60).unwrap();
        run(Path::new(&path), id, Some(&second_path), 60).unwrap();
        let first = std::fs::read(&first_path).unwrap();
        let second = std::fs::read(&second_path).unwrap();
        assert_eq!(first, second, "two --ticks 60 captures differ");
    }

    /// The GPU display composites the two overlay layers over the scene and
    /// presents through a real swapchain (SDL's offscreen video driver gives
    /// Vulkan a headless surface). Skips without a SPIR-V capable GPU.
    #[test]
    fn gpu_display_presents_layered_frames() {
        let _lock = crate::audio::test_lock::sdl();
        unsafe { SDL_SetHint(SDL_HINT_VIDEO_DRIVER, c"offscreen".as_ptr()) };
        let renderer = Graphics {
            renderer: RendererKind::Gpu,
            render_scale: 200,
            ..Graphics::default()
        };
        let display = Display::new("Arklay GPU test", false, renderer);
        let _ = unsafe { sdl3_sys::hints::SDL_ResetHint(SDL_HINT_VIDEO_DRIVER) };
        let mut display = match display {
            Ok(display) => display,
            Err(err) => {
                eprintln!("skipping: {err:#}");
                return;
            }
        };
        if display.gpu().is_none() {
            eprintln!("skipping: no GPU device");
            return;
        }
        let mut black = Framebuffer::new();
        let mut white = Framebuffer::new();
        white.fill_rect([0, 0, WIDTH, HEIGHT], [255, 255, 255, 255]);
        for layer in [&mut black, &mut white] {
            layer.fill_rect([10, 10, 40, 40], [200, 30, 30, 255]);
            layer.fade_to_black(128);
        }
        for _ in 0..3 {
            display.present_layers(&black, Some(&white)).unwrap();
            display.present_layers(&black, None).unwrap();
        }

        // A GPU failure hands the window to the software backend, which
        // keeps presenting.
        display
            .fall_back_to_software(&anyhow::anyhow!("simulated device loss"))
            .unwrap();
        assert!(display.gpu().is_none());
        display.present_layers(&black, Some(&white)).unwrap();
        display.show().unwrap();
    }

    /// The GPU renderer draws real rooms (backgrounds, masks, models,
    /// shadows) close to the software rasterizer. Needs `ARKLAY_RE1_PACK` and
    /// a SPIR-V capable GPU; skips without a GPU. `ARKLAY_GPU_TEST_DUMP=<dir>`
    /// writes both frames as BMPs and `ARKLAY_GPU_TEST_SCALE=<n>` renders the
    /// GPU scene at `n` times the resolution (compared at pixel centres).
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn gpu_scene_matches_software_in_real_rooms() {
        let _lock = crate::audio::test_lock::sdl();
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let Ok(_sdl) = SdlHandle::acquire() else {
            eprintln!("skipping: SDL video unavailable");
            return;
        };
        let scale = std::env::var("ARKLAY_GPU_TEST_SCALE")
            .ok()
            .and_then(|scale| scale.parse().ok())
            .unwrap_or(1);
        let mut gpu = match GpuRenderer::offscreen(scale) {
            Ok(gpu) => gpu,
            Err(err) => {
                eprintln!("skipping: {err:#}");
                return;
            }
        };
        let (gpu_width, gpu_height) = gpu.scene_size();
        let scale = gpu_width / WIDTH as u32;
        let pack = Pack::open(Path::new(&path)).unwrap();
        let save_dir = TempDir::new();
        let mut drawn = 0;
        for (room, cut) in ["105", "106", "10B", "10D", "202", "1001"]
            .into_iter()
            .flat_map(|room| (0..8).map(move |cut| (room, cut)))
        {
            let id = RoomId::parse(room).unwrap();
            let Ok(mut session) = GameSession::from_room(&pack, id, &save_dir.0) else {
                continue;
            };
            if cut >= session.loaded.room.cuts.len() {
                continue;
            }
            session.loaded.room.current_cut = cut;
            // Let the room's scripts place and animate its entities.
            for _ in 0..30 {
                session
                    .tick(&pack, UiInput::default(), player::Input::default(), false)
                    .unwrap();
            }
            if session.loaded.room.current_cut != cut || session.transition.is_some() {
                continue;
            }
            let room = format!("{room}-{cut}");
            let mut software = Framebuffer::new();
            render_frame(
                &mut software,
                &pack,
                session.loaded.id,
                &session.loaded.room,
                &session.player,
                &mut session.game,
                session.loaded.player_assets.as_ref(),
                &mut session.npc_models,
                &mut session.masks,
                &mut session.shadows,
                &mut session.effect_pages,
            );
            let mut overlay = Framebuffer::new();
            assert!(render_frame_on(
                &mut overlay,
                Some(&mut gpu),
                &pack,
                session.loaded.id,
                &session.loaded.room,
                &session.player,
                &mut session.game,
                session.loaded.player_assets.as_ref(),
                &mut session.npc_models,
                &mut session.masks,
                &mut session.shadows,
                &mut session.effect_pages,
                &mut Tween::default(),
            ));
            let hardware = gpu.read_scene().unwrap();
            if let Ok(dir) = std::env::var("ARKLAY_GPU_TEST_DUMP") {
                for (name, rgba, width, height) in [
                    ("software", &software.rgba, WIDTH as u32, HEIGHT as u32),
                    ("gpu", &hardware, gpu_width, gpu_height),
                ] {
                    let image = Image {
                        width,
                        height,
                        rgba: rgba.clone(),
                    };
                    capture_bmp(&image, &Path::new(&dir).join(format!("{room}-{name}.bmp")))
                        .unwrap();
                }
            }
            let centre = |index: usize| {
                let (x, y) = (index as u32 % WIDTH as u32, index as u32 / WIDTH as u32);
                let offset = ((y * scale + scale / 2) * gpu_width + x * scale + scale / 2) * 4;
                &hardware[offset as usize..offset as usize + 4]
            };
            let different = software
                .rgba
                .chunks(4)
                .enumerate()
                .filter(|(index, sw)| {
                    let hw = centre(*index);
                    (0..3).any(|c| sw[c].abs_diff(hw[c]) > 48)
                })
                .count();
            let total = (WIDTH * HEIGHT) as usize;
            eprintln!("room {room}: {different} of {total} pixels differ");
            // Edges, depth-vs-painter ties and float shading differ; whole
            // missing or misplaced layers would not fit in this budget.
            assert!(
                different * 10 < total,
                "room {room}: {different} pixels differ"
            );
            drawn += 1;
        }
        assert!(drawn > 0);
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn capture_draws_the_player_in_room_1001() {
        let _sdl = crate::audio::test_lock::sdl();
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let dir = TempDir::new();
        let capture_path = dir.0.join("room1001.bmp");
        run(
            Path::new(&path),
            RoomId::parse("1001").unwrap(),
            Some(&capture_path),
            0,
        )
        .unwrap();

        let image = bmp::decode(&std::fs::read(&capture_path).unwrap()).unwrap();
        assert_eq!(image.width, WIDTH as u32);
        assert_eq!(image.height, HEIGHT as u32);

        let pack = Pack::open(Path::new(&path)).unwrap();
        let background = bmp::decode(
            &pack
                .read(&RoomId::parse("1001").unwrap().cut_entry(0))
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

    #[test]
    fn door_animation_drains_vm_sound_and_messages() {
        use crate::model::{Texture8, Tmd};
        let dor = door::Dor {
            scripts: vec![door::Script {
                offset: 0,
                bytes: vec![
                    0x1F, 0x5A, 0xFF, 0x00, // MESSAGE 0x5A, 0x00FF
                    0x20, 0x00, 0x01, 0x00, // SFX bank 0, id 1
                    0x00, 0x00, // END
                ],
            }],
            mesh: Tmd::default(),
            texture: Texture8 {
                width: 1,
                height: 1,
                indices: vec![1],
                palettes: vec![[0, 0, 0, 255], [0, 0, 0, 255]],
                stp: Vec::new(),
            },
            orders: [door::Order::default(); door::ORDER_COUNT],
        };
        let vm = door::vm::Vm::new(&dor, door::vm::DoorParams::default());
        let mut animation = DoorAnimation::new(dor, vm);

        let frame = animation.step();
        assert!(frame.done, "the script ends immediately");
        assert_eq!(
            animation.take_sfx(),
            [door::vm::Sfx {
                bank: 0,
                id: 1,
                volume: 0
            }]
        );
        assert_eq!(
            animation.take_messages(),
            [door::vm::Message {
                id: 0x5A,
                value: 0xFF
            }]
        );
        assert!(animation.render_frame().is_some());
        assert!(animation.take_sfx().is_empty(), "the queue was drained");
    }

    #[test]
    fn door_sfx_mapping_uses_the_room_pair_bank() {
        let record = game::Door {
            sfx: 0,
            ..game::Door::default()
        };
        let effect = |bank, id| door::vm::Sfx {
            bank,
            id,
            volume: 0,
        };
        assert_eq!(
            door_animation_sfx_name(&record, effect(0, 0)),
            Some("Dr_wd01")
        );
        assert_eq!(
            door_animation_sfx_name(&record, effect(0, 1)),
            Some("Dr_wd02")
        );
        // Banks 1-3 are not in the M5 pack.
        assert_eq!(door_animation_sfx_name(&record, effect(1, 0)), None);
        assert_eq!(door_animation_sfx_name(&record, effect(3, 1)), None);
        // Record sfx 2 has no first slot and only Dr_brk01 in the second.
        let record = game::Door {
            sfx: 2,
            ..game::Door::default()
        };
        assert_eq!(door_animation_sfx_name(&record, effect(0, 0)), None);
        assert_eq!(
            door_animation_sfx_name(&record, effect(0, 1)),
            Some("Dr_brk01")
        );
    }

    #[test]
    fn drain_mask_toggles_updates_the_current_cut() {
        let mut room = RoomState {
            cuts: vec![crate::state::Cut {
                mask_active: 0b0000_0111,
                ..crate::state::Cut::default()
            }],
            ..RoomState::default()
        };
        let mut game = game::GameState::default();
        game.mask_toggles.push(game::MaskToggle {
            group: 2,
            active: false,
        });
        game.mask_toggles.push(game::MaskToggle {
            group: 5,
            active: true,
        });
        game.mask_toggles.push(game::MaskToggle {
            group: 0,
            active: false,
        });

        drain_mask_toggles(&mut room, &mut game);

        assert_eq!(room.cuts[0].mask_active, 0b0001_0101);
        assert!(game.mask_toggles.is_empty());

        // `room_sprite_hide` (0x49) clears the named groups and is consumed;
        // bit 0 names no group, and bits at or above 16 are not walked.
        game.sprite_hide = 1 | (1 << 1) | (1 << 5) | (1 << 16);
        drain_mask_toggles(&mut room, &mut game);
        assert_eq!(room.cuts[0].mask_active, 0b0000_0100);
        assert_eq!(game.sprite_hide, 0);
    }

    #[test]
    fn entity_model_cache_remembers_missing_and_invalid_models() {
        let mut writer = PackWriter::new();
        writer
            .add("enemy/em23.emd", b"not-an-emd".to_vec())
            .unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let mut cache = enemy::EntityModelCache::default();
        // An absent entry yields None and is remembered so the pack is only
        // read once.
        assert!(cache.get(&pack, 0x20, 0).is_none());
        assert!(cache.get(&pack, 0x20, 0).is_none());
        assert!(cache.missing.contains(&(0x20, 0)));
        // A present but invalid entry yields None too.
        assert!(cache.get(&pack, 0x23, 0).is_none());
        assert!(cache.missing.contains(&(0x23, 0)));
        // A monster id tries its scenario and generic entries and is
        // remembered as absent when neither exists.
        assert!(cache.get(&pack, 0x1F, 0).is_none());
        assert!(cache.missing.contains(&(0x1F, 0)));
        // An id outside the dispatch range has no model path and no warning.
        assert!(cache.get(&pack, 0xFF, 0).is_none());
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_pack_npc_models_load_through_the_cache() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let mut cache = enemy::EntityModelCache::default();

        for id in crate::enemy::FIRST_ID..=crate::enemy::LAST_ID {
            let model = cache
                .get(&pack, id, 0)
                .unwrap_or_else(|| panic!("enemy/em{id:02x}.emd missing from the pack"));
            assert_eq!(
                model.skeleton.relative.len(),
                15,
                "enemy/em{id:02x}.emd joints"
            );
            assert!(!model.mesh.objects.is_empty(), "enemy/em{id:02x}.emd mesh");
        }

        // A second request returns the same parsed model, not a re-parse.
        let first = cache.get(&pack, 0x23, 0).unwrap();
        let second = cache.get(&pack, 0x23, 0).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(cache.get(&pack, 0x1F, 0).is_none());
    }

    /// The save round-trip the full-game soak asserts: capture the state,
    /// serialize it, parse the block back, apply it to a clone and check the
    /// persisted fields and the idempotent serialization.
    fn assert_save_round_trip(state: &game::GameState) {
        let file = save::SaveFile::from_state(state);
        let bytes = file.to_bytes();
        let parsed = save::SaveFile::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.to_bytes(), bytes, "to_bytes is not idempotent");

        let mut restored = state.clone();
        parsed.apply_to(&mut restored);
        assert_eq!(restored.inventory, state.inventory);
        assert_eq!(restored.item_box, state.item_box);
        assert_eq!(restored.camera.current_cut, usize::from(file.camera));
        assert_eq!(restored.entities[0].health, state.entities[0].health);
        assert_eq!(
            [restored.entities[0].pos[0], restored.entities[0].pos[2]],
            [state.entities[0].pos[0], state.entities[0].pos[2]]
        );
        assert_eq!(restored.entities[0].angle, state.entities[0].angle);
        assert_eq!(&restored.flags[1].bytes()[..], &file.scenario2[..]);
        assert_eq!(&restored.flags[3].bytes()[..], &file.enemies[..]);
        assert_eq!(&restored.flags[7].bytes()[..], &file.room_items[..]);
        assert_eq!(restored.state_bytes, parsed.state_bytes);
    }

    #[test]
    fn synthetic_transition_runs_to_completion_and_swaps_rooms() {
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

        // No `door/*.dor` in this pack: the transition must fall back to a
        // short black screen and still land the player in the destination.
        let pack = Pack::open(&pack_path).unwrap();
        let sim = simulate_door(&pack, a, 0, None).unwrap();

        assert_eq!(sim.target, b);
        assert_eq!(sim.game.id, b);
        assert_eq!(sim.player.pos, [555, 0, 666]);
        assert_eq!(sim.player.angle, 1024);
        assert_eq!(sim.room.current_cut, 0);
        assert!(sim.timeline.len() >= FALLBACK_TRANSITION_FRAMES as usize);
        assert!(sim.timeline[0].1, "the first frame is black");
        assert!(sim.timeline[2].1, "the third frame is black");
        // The phase-2 hold freezes the frame counter, so black covers it too.
        assert!(sim.timeline[7].1, "the hold frames stay black");
        assert!(sim.timeline.iter().any(|(_, black, _)| !black));
        assert_eq!(non_black_pixels(&sim.first_frame), 0);
        assert!(sim.game.doors[0].is_none(), "the door table is rebuilt");
        // The M17 soak's save round-trip over the crossed transition state.
        assert_save_round_trip(&sim.game);
    }

    #[test]
    fn synthetic_camera_only_transition_keeps_the_room_and_player() {
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
            .add(&a.rdt_entry(), synthetic_rdt(&door_init_with_camera(0x80)))
            .unwrap();
        writer
            .add(&b.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&a.cut_entry(0), bmp_bytes.clone()).unwrap();
        writer.add(&b.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let sim = simulate_door(&pack, a, 0, None).unwrap();

        assert_eq!(sim.target, a, "camera-only doors stay in the room");
        assert_eq!(sim.game.id, a);
        assert_eq!(sim.room.current_cut, 0);
        assert!(!sim.room.cuts.is_empty(), "the source room stays loaded");
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_save_room_door_transitions_to_1011() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let source = RoomId::parse("1001").unwrap();
        let sim = simulate_door(&pack, source, 0, None).unwrap();

        assert_eq!(sim.target, RoomId::parse("1011").unwrap());
        assert_eq!(sim.game.id, sim.target);
        assert_eq!(sim.player.pos, [8700, 0, 7900]);
        assert_eq!(sim.player.angle, 1024);
        assert!(sim.frame_count > transition::BLACK_FRAMES);

        // The leading frames are black and a door frame follows.
        assert!(sim.timeline[0].1 && sim.timeline[1].1 && sim.timeline[2].1);
        assert_eq!(non_black_pixels(&sim.first_frame), 0, "frame 0 is black");
        let mid = sim.mid_frame.as_ref().expect("a non-black door frame");
        assert!(
            non_black_pixels(mid) > 2000,
            "the door panels must be visible: {}",
            non_black_pixels(mid)
        );
        assert!(!sim.timeline[sim.mid_index.unwrap()].1);

        // The save-room door names camera 4 and the destination's switch zone
        // keeps it for the spawn point.
        assert_eq!(sim.room.current_cut, 4);
        assert_eq!(sim.game.camera.current_cut, 4);
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_transition_capture_is_deterministic() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let source = RoomId::parse("1001").unwrap();
        // Captures persist under the test temp directory for inspection.
        let dir = std::env::temp_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let first = simulate_door(&pack, source, 0, Some(&dir)).unwrap();
        let first_mid_bytes = std::fs::read(dir.join("transition_mid.bmp")).unwrap();
        let second = simulate_door(&pack, source, 0, Some(&dir)).unwrap();

        assert_eq!(first.timeline, second.timeline, "timelines differ");
        assert_eq!(
            fnv1a(&first.first_frame.rgba),
            fnv1a(&second.first_frame.rgba),
            "the black frame differs between runs"
        );
        assert_eq!(non_black_pixels(&first.first_frame), 0);

        let mid_a = first.mid_frame.as_ref().expect("a door frame");
        let mid_b = second.mid_frame.as_ref().expect("a door frame");
        assert!(non_black_pixels(mid_a) > 2000);
        assert_eq!(
            fnv1a(&mid_a.rgba),
            fnv1a(&mid_b.rgba),
            "the mid-transition frame differs between runs"
        );

        assert!(dir.join("transition_frame0.bmp").is_file());
        assert!(dir.join("transition_mid.bmp").is_file());
        assert_eq!(
            std::fs::read(dir.join("transition_mid.bmp")).unwrap(),
            first_mid_bytes,
            "the capture file differs between runs"
        );
        let decoded = bmp::decode(&first_mid_bytes).unwrap();
        assert_eq!(rgb_fnv1a(&decoded), rgb_fnv1a(mid_a));
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_room_1000_capture_and_mask_layer() {
        let _sdl = crate::audio::test_lock::sdl();
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("1000").unwrap();
        let dir = TempDir::new();
        let capture_path = dir.0.join("room1000_masked.bmp");
        run(Path::new(&path), id, Some(&capture_path), 0).unwrap();
        let captured = bmp::decode(&std::fs::read(&capture_path).unwrap()).unwrap();

        // The capture path is deterministic.
        let repeat_path = dir.0.join("room1000_masked2.bmp");
        run(Path::new(&path), id, Some(&repeat_path), 0).unwrap();
        let repeat = bmp::decode(&std::fs::read(&repeat_path).unwrap()).unwrap();
        assert_eq!(captured.rgba, repeat.rgba, "two captures differ");

        // Rebuild the same pose through the render API. The init script writes
        // state byte 2 (`roomCameraId`), which moves the camera to cut 2; that
        // cut has no mask sprites, so compare a masked cut's render instead.
        let mut loaded = load_room(&pack, id).unwrap();
        let mut game = game::GameState::new(id, &loaded.room);
        run_room_init(&mut loaded, &mut game);
        apply_camera(&mut loaded.room, &mut game, None);
        assert_eq!(loaded.room.current_cut, 2, "init selects camera 2");
        assert!(loaded.room.cuts[2].masks.is_empty());

        let player_state = player::spawn(id, &loaded.room);
        let cut = &loaded.room.cuts[0];
        assert!(!cut.masks.is_empty(), "room 1000 cut 0 has mask sprites");
        let assets = loaded.player_assets.as_ref().expect("player assets");
        let (keyframes, clips) = match player_state.clip_source {
            player::ClipSource::Emd
            | player::ClipSource::Room
            | player::ClipSource::Weapon
            | player::ClipSource::Damage => (&assets.emd.keyframes, &assets.emd.clips),
            player::ClipSource::Emw => (&assets.emw.keyframes, &assets.emw.clips),
        };
        let keyframe = &keyframes[player_state.anim.keyframe_index(clips)];
        let entity = anim::entity_matrix(player_state.pos, player_state.angle);
        let joints = anim::joint_matrices(&assets.emd.skeleton, keyframe, &entity);
        let entities = [EntityMesh {
            mesh: &assets.emd.mesh,
            texture: &assets.emd.texture,
            joints: &joints,
            tint: [255; 3],
            blend_weight: None,
            hidden_joints: 0,
        }];
        let camera = Camera::from_cut(cut);
        let lighting = Lighting::from_room(&loaded.room);
        let page = bmp::decode_mask(&pack.read(&id.roommask_entry(0)).unwrap()).unwrap();
        let layer = MaskLayer {
            room: id,
            camera: 0,
            cut,
            page: &page,
            active: cut.mask_active,
        };
        let mut masked = Framebuffer::new();
        render::draw_gameplay_scene(
            &mut masked,
            cut.background.as_ref(),
            &entities,
            &[],
            &camera,
            &lighting,
            Some(&layer),
        );
        let mut plain = Framebuffer::new();
        render::draw_gameplay_scene(
            &mut plain,
            cut.background.as_ref(),
            &entities,
            &[],
            &camera,
            &lighting,
            None,
        );

        let changed = masked
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(plain.rgba.as_chunks::<4>().0)
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            changed > 500,
            "the mask layer repainted only {changed} pixels"
        );
    }

    #[test]
    fn entity_sounds_drain_when_no_mixer_is_available() {
        let dir = TempDir::new();
        let path = dir.0.join("empty.akpak");
        let writer = PackWriter::new();
        writer.write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();

        let mut sounds = vec![game::EntitySound {
            name: "ft_wdA",
            bank: 2,
            column: 45,
            pos: [0, 0, 0],
        }];
        play_entity_sounds(
            &mut None,
            &mut SfxCache::default(),
            &pack,
            &RoomState::default(),
            &mut sounds,
        );
        assert!(sounds.is_empty(), "an absent mixer still drains the queue");
    }

    #[test]
    fn same_bank_entity_footsteps_restart_the_engine_voice() {
        let _sdl = crate::audio::test_lock::sdl();
        let dir = TempDir::new();
        let path = dir.0.join("footsteps.akpak");
        let mut writer = PackWriter::new();
        // Long enough to survive the mixer's per-call queue fill (2048
        // frames), so the voice count still contains the live cues.
        writer
            .add("se/ft_wda.wav", voice_wav_bytes(&[1000; 6000]))
            .unwrap();
        writer.write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();

        let _ = unsafe {
            sdl3_sys::hints::SDL_SetHint(sdl3_sys::hints::SDL_HINT_AUDIO_DRIVER, c"dummy".as_ptr())
        };
        let mixer = Mixer::open();
        let _ = unsafe { sdl3_sys::hints::SDL_ResetHint(sdl3_sys::hints::SDL_HINT_AUDIO_DRIVER) };
        let Some(mixer) = mixer else {
            eprintln!("skipping same-bank footstep test: no audio device");
            return;
        };
        let mut music = Some(mixer);
        let mut cache = SfxCache::default();
        let room = RoomState {
            cuts: vec![crate::state::Cut {
                index: 0,
                pos: [0, 0, 0],
                look_at: [1000, 0, 0],
                fov: 200,
                ..crate::state::Cut::default()
            }],
            ..RoomState::default()
        };
        fn cue(
            music: &mut Option<Mixer>,
            cache: &mut SfxCache,
            pack: &Pack,
            room: &RoomState,
            column: u8,
        ) {
            let mut sounds = vec![game::EntitySound {
                name: "ft_wdA",
                bank: 2,
                column,
                pos: [1000, 0, 0],
            }];
            play_entity_sounds(music, cache, pack, room, &mut sounds);
        }

        cue(&mut music, &mut cache, &pack, &room, 45);
        assert_eq!(music.as_ref().unwrap().active_sfx(), 1);
        // The same room-sound column keys the same bank and restarts its voice.
        cue(&mut music, &mut cache, &pack, &room, 45);
        assert_eq!(
            music.as_ref().unwrap().active_sfx(),
            1,
            "a repeated entity footstep appended a voice"
        );
        // A different column keys a different bank and stacks.
        cue(&mut music, &mut cache, &pack, &room, 46);
        assert_eq!(
            music.as_ref().unwrap().active_sfx(),
            2,
            "a distinct entity footstep bank did not append"
        );
    }

    #[test]
    fn player_cues_map_to_their_original_banks() {
        // The hit-reaction grunts are character-bank ids 0-3.
        for id in 0..=3u16 {
            assert_eq!(player_sound_bank(id), (3, id as u8));
        }
        // The push grunts (`0x16`/`0x17`), vault/ladder step (`0x23`) and
        // climb end (`0x2D`) are room-table columns.
        for id in [0x16u16, 0x17, 0x23, 0x2D] {
            assert_eq!(player_sound_bank(id), (2, id as u8));
        }
        // Room 1000's row names column 45, so the climb-end cue resolves to
        // its footstep sound through the same dispatch.
        let room = RoomState {
            stage: 1,
            room: 0,
            ..RoomState::default()
        };
        let play = sfx::play_sfx_3d(&room, 0, 2, 0x2D, [0; 3], [1000, 0, 0], [1000, 0, 0]);
        assert_eq!(play.name, Some("ft_wdA"));
    }

    #[test]
    fn snd3d_requests_resolve_audit_and_pan_the_bgm() {
        let dir = TempDir::new();
        let path = dir.0.join("empty.akpak");
        PackWriter::new().write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();

        let id = RoomId::parse("5040").unwrap();
        let mut game = game::GameState::new(id, &RoomState::default());
        let room = RoomState {
            stage: 5,
            room: 4,
            cuts: vec![crate::state::Cut {
                index: 0,
                pos: [0, 0, 0],
                look_at: [1000, 0, 0],
                fov: 200,
                ..crate::state::Cut::default()
            }],
            ..RoomState::default()
        };
        let point = |x: i32, z: i32| game::Snd3dPos::Point([x, 0, z]);
        // Row 120's panel02 resolves (the empty pack only makes the load miss,
        // which is not a bank no-op).
        game.snd3d_requests.push(game::Snd3dRequest {
            bank: 2,
            id: 24,
            volume: 0,
            pos: point(1000, 0),
        });
        // The unloaded weapon bank and an absent room column are audited.
        game.snd3d_requests.push(game::Snd3dRequest {
            bank: 1,
            id: 7,
            volume: 0,
            pos: point(0, 0),
        });
        game.snd3d_requests.push(game::Snd3dRequest {
            bank: 2,
            id: 3,
            volume: 0,
            pos: point(0, 0),
        });
        // Bank 4 pans and restarts BGM channel 0 when a bank is loaded.
        game.bgm.channels[0].name = Some("Bgm_13");
        game.snd3d_requests.push(game::Snd3dRequest {
            bank: 4,
            id: 23,
            volume: 0,
            pos: point(0, 2000),
        });

        play_snd3d_requests(&mut None, &mut SfxCache::default(), &pack, &room, &mut game);
        assert!(game.snd3d_requests.is_empty(), "the queue always drains");
        assert_eq!(game.snd3d_noops.get(&(1, 7)), Some(&1));
        assert_eq!(game.snd3d_noops.get(&(2, 3)), Some(&1));
        assert_eq!(game.snd3d_noops.get(&(2, 24)), None, "panel02 resolves");
        assert!(game.bgm.channels[0].restart, "bank 4 restarted channel 0");
        assert_ne!(game.bgm.channels[0].pan, 0, "bank 4 panned channel 0");
    }

    #[test]
    fn a_door_transition_loads_the_live_room_sfx_pair() {
        let dir = TempDir::new();
        let path = dir.0.join("pair.akpak");
        PackWriter::new().write(&path).unwrap();
        let _pack = Pack::open(&path).unwrap();

        let id = RoomId::parse("1000").unwrap();
        let mut loaded = LoadedRoom {
            id,
            room: RoomState {
                stage: 1,
                room: 2,
                ..RoomState::default()
            },
            scripts: Default::default(),
            script_source: ScriptSource::Rdt,
            player_assets: None,
        };
        let mut game = game::GameState::new(id, &loaded.room);
        let mut player_state = player::spawn(id, &loaded.room);
        // A camera-only, silent record: the pair still latches from its sfx
        // byte, exactly like the original's `room_transition_load`.
        let mut session = TransitionMode {
            transition: transition::Transition::new(DoorAnimation::missing()),
            door: game::Door {
                camera: 0x80 | 0x40,
                sfx: 1,
                ..game::Door::default()
            },
            target: id,
            camera_only: true,
            silent: true,
            source: id,
            door_yield: None,
            destination: None,
            placed: false,
            entered: false,
            frame: transition::TransitionFrame::default(),
        };
        finish_transition(&mut session, &mut game, &mut player_state, &mut loaded);
        assert_eq!(loaded.room.room_sfx, 1, "the record's sfx byte is live");
        assert_eq!(
            sfx::play_sfx_3d(&loaded.room, 0, 0, 0, [0; 3], [1000, 0, 0], [1000, 0, 0]).name,
            Some("Dr_mtl01"),
            "bank 0 now resolves through the new pair"
        );

        // The same latch happens on a room-changing record, after the
        // destination room has replaced the outgoing one.
        let target = RoomId::parse("1001").unwrap();
        let mut session = TransitionMode {
            transition: transition::Transition::new(DoorAnimation::missing()),
            door: game::Door {
                camera: 0x40,
                sfx: 2,
                next_room: 0x01,
                ..game::Door::default()
            },
            target,
            camera_only: false,
            silent: true,
            source: id,
            door_yield: None,
            placed: false,
            destination: Some(LoadedRoom {
                id: target,
                room: RoomState {
                    stage: 1,
                    room: 1,
                    ..RoomState::default()
                },
                scripts: Default::default(),
                script_source: ScriptSource::Rdt,
                player_assets: None,
            }),
            entered: false,
            frame: transition::TransitionFrame::default(),
        };
        finish_transition(&mut session, &mut game, &mut player_state, &mut loaded);
        assert_eq!(loaded.id, target);
        assert_eq!(loaded.room.room_sfx, 2, "the destination latches its pair");
        assert_eq!(
            sfx::play_sfx_3d(&loaded.room, 0, 0, 1, [0; 3], [1000, 0, 0], [1000, 0, 0]).name,
            Some("Dr_brk01"),
            "the loaded pair 2 names the trap-door sound"
        );
    }

    /// A minimal 16-bit mono WAV for the voice handshake test.
    fn voice_wav_bytes(samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut body = Vec::new();
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&22050u32.to_le_bytes());
        body.extend_from_slice(&(22050u32 * 2).to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&16u16.to_le_bytes());
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(&data);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn voice_tick_handshakes_pending_requests() {
        let _sdl = crate::audio::test_lock::sdl();
        let dir = TempDir::new();
        let path = dir.0.join("voice.akpak");
        let mut writer = PackWriter::new();
        writer
            .add("voice/01_09.wav", voice_wav_bytes(&[100, 200]))
            .unwrap();
        writer
            .add("voice/01_0a.wav", voice_wav_bytes(&[300, 400]))
            .unwrap();
        writer.write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();

        let _ = unsafe {
            sdl3_sys::hints::SDL_SetHint(sdl3_sys::hints::SDL_HINT_AUDIO_DRIVER, c"dummy".as_ptr())
        };
        let mixer = Mixer::open();
        let _ = unsafe { sdl3_sys::hints::SDL_ResetHint(sdl3_sys::hints::SDL_HINT_AUDIO_DRIVER) };
        let Some(mixer) = mixer else {
            eprintln!("skipping voice handshake test: no audio device");
            return;
        };
        let mut music = Some(mixer);
        let mut cache = VoiceCache::default();
        let id = RoomId::parse("1000").unwrap();
        let mut game = game::GameState::new(id, &RoomState::default());
        let request = |path: &str| game::VoiceRequest {
            path: path.to_string(),
            volume: 0,
            pan: 0,
        };

        // A queued request starts on the next tick and raises the F7 wait bit.
        game.voice.request = Some(request("voice/01_09.wav"));
        game.set_voice_playing();
        tick_voice(&mut music, &mut cache, &mut game, &pack);
        assert!(cache.active, "the line started");
        assert!(game.voice_playing(), "F7 waits on the active line");
        assert!(music.as_ref().unwrap().voice_playing());

        // The line runs to its end; the next tick notices, clears the wait bit
        // and leaves the channel free.
        music.as_mut().unwrap().render_for_test(4);
        assert!(!music.as_ref().unwrap().voice_playing());
        assert!(
            game.voice_playing(),
            "the wait holds until the tick sees it"
        );
        tick_voice(&mut music, &mut cache, &mut game, &pack);
        assert!(!cache.active, "the finish released the channel");
        assert!(!game.voice_playing(), "F7 advances once the line is done");

        // A request queued while a line is active waits, then starts when the
        // channel frees.
        game.voice.request = Some(request("voice/01_09.wav"));
        game.set_voice_playing();
        tick_voice(&mut music, &mut cache, &mut game, &pack);
        game.voice.request = Some(request("voice/01_0a.wav"));
        game.set_voice_playing();
        tick_voice(&mut music, &mut cache, &mut game, &pack);
        assert_eq!(
            cache.pending.as_ref().map(|pending| pending.path.as_str()),
            Some("voice/01_0a.wav"),
            "a second line stays pending while the first plays"
        );
        assert!(music.as_ref().unwrap().voice_playing());
        assert!(game.voice_playing());

        music.as_mut().unwrap().render_for_test(4);
        tick_voice(&mut music, &mut cache, &mut game, &pack);
        assert!(cache.pending.is_none(), "the pending line was taken");
        assert!(cache.active);
        assert!(
            game.voice_playing(),
            "the pending line kept the wait raised"
        );
        assert!(music.as_ref().unwrap().voice_playing());
        assert_eq!(game.voice.misses, 0);

        // A request whose entry is absent from the pack counts the miss and
        // releases the wait so the script cannot deadlock on F7.
        music.as_mut().unwrap().render_for_test(4);
        tick_voice(&mut music, &mut cache, &mut game, &pack);
        assert!(!cache.active, "the previous line finished");
        game.voice.request = Some(request("voice/00_ff.wav"));
        game.set_voice_playing();
        tick_voice(&mut music, &mut cache, &mut game, &pack);
        assert_eq!(game.voice.misses, 1, "the absent entry was counted");
        assert!(!game.voice_playing(), "the wait released on the miss");
        assert!(!cache.active);
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_room_1001_walk_emits_ft_wda() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("1001").unwrap();
        let room = rdt::parse(&pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
        let assets = load_player_assets(&pack, id).expect("player assets");
        let mut player_state = player::spawn(id, &room);
        let input = player::Input {
            up: true,
            ..player::Input::default()
        };

        let mut names = Vec::new();
        for _ in 0..60 {
            player::update(
                &mut player_state,
                &room,
                &assets.emd.clips,
                &assets.emw.clips,
                input,
            );
            for footstep in player_state.take_footsteps() {
                let name = sfx::footstep_sound(&room, footstep.pos, footstep.sound_type, false)
                    .expect("room 1001's zone resolves a footstep");
                names.push(name);
                // The 3D math must produce a finite gain and an in-range pan.
                let cut = &room.cuts[room.current_cut];
                let (gain, pan) = sfx::sound_gain_pan(cut.pos, cut.look_at, footstep.pos);
                assert!(gain.is_finite() && (0.0..=1.1).contains(&gain));
                assert!((-1.0..=1.0).contains(&pan));
            }
        }

        assert!(names.contains(&"ft_wdA"), "expected ft_wdA, got {names:?}");
        // The engine's cache resolves the pack entry the mixer would play.
        let mut cache = SfxCache::default();
        assert!(cache.load(&pack, "ft_wdA").is_some());
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_room_1001_run_emits_ft_wdb_faster_than_walk() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("1001").unwrap();
        let room = rdt::parse(&pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
        let assets = load_player_assets(&pack, id).expect("player assets");

        type Footfall = (usize, u8, u8, [i32; 3], Option<&'static str>);

        let drive = |input: player::Input, ticks: usize| {
            let mut player_state = player::spawn(id, &room);
            let mut log: Vec<Footfall> = Vec::new();
            for tick in 0..ticks {
                player::update(
                    &mut player_state,
                    &room,
                    &assets.emd.clips,
                    &assets.emw.clips,
                    input,
                );
                for footstep in player_state.take_footsteps() {
                    let name = sfx::footstep_sound(&room, footstep.pos, footstep.sound_type, false);
                    log.push((
                        tick,
                        footstep.frame,
                        footstep.sound_type,
                        footstep.pos,
                        name,
                    ));
                }
            }
            log
        };

        let walk = drive(
            player::Input {
                up: true,
                ..player::Input::default()
            },
            60,
        );
        let run = drive(
            player::Input {
                up: true,
                run: true,
                ..player::Input::default()
            },
            60,
        );
        println!("walk: {walk:?}");
        println!("run: {run:?}");

        // The shipped no-weapon model: the walk cycle (clip 2) is 28 frames
        // with contacts on 0x08/0x16, the forward run (clip 3) is 20 frames
        // with contacts on 0x00/0x0A. The run therefore lands more footfalls in
        // the same window, and they are the B variant.
        assert!(
            run.len() > walk.len(),
            "run {} footfalls, walk {}",
            run.len(),
            walk.len()
        );
        assert!(run.iter().all(|entry| entry.2 == 1), "{run:?}");
        assert!(walk.iter().all(|entry| entry.2 == 0), "{walk:?}");

        // Run contacts are 10 ticks apart and walk contacts 14, so the run is
        // faster at 3/s against the walk's 2.14/s.
        let gaps = |log: &[Footfall]| -> Vec<usize> {
            log.windows(2).map(|pair| pair[1].0 - pair[0].0).collect()
        };
        assert!(gaps(&run).iter().all(|&gap| gap == 10), "{run:?}");
        assert!(gaps(&walk).iter().all(|&gap| gap == 14), "{walk:?}");

        // The M5/M6 zone lookup names the early run contacts within room 1001,
        // and the 3D path yields a finite gain and an in-range pan.
        let run_names: Vec<&str> = run.iter().filter_map(|entry| entry.4).collect();
        assert_eq!(
            run_names.iter().take(2).copied().collect::<Vec<&str>>(),
            ["ft_wdB", "ft_wdB"],
            "{run:?}"
        );
        let cut = &room.cuts[room.current_cut];
        let (gain, pan) = sfx::sound_gain_pan(cut.pos, cut.look_at, run[0].3);
        assert!(gain.is_finite() && (0.0..=1.1).contains(&gain));
        assert!((-1.0..=1.0).contains(&pan));

        // The run's sound resolves through the same mixer cache as the walk's.
        let mut cache = SfxCache::default();
        assert!(cache.load(&pack, "ft_wdB").is_some());
    }

    /// A pack carrying `font/font.tim`: the real pack when it has the entry,
    /// otherwise a one-entry pack built from the installation's raw sheet.
    fn font_pack(path: &str, dir: &TempDir) -> Option<std::path::PathBuf> {
        let pack = Pack::open(Path::new(path)).unwrap();
        if pack.contains("font/font.tim") {
            return Some(std::path::PathBuf::from(path));
        }
        let root = std::env::var("ARKLAY_RE1_ROOT").ok()?;
        let font = std::fs::read(std::path::PathBuf::from(root).join("JPN/DATA/FONT.TIM")).ok()?;
        let mini = dir.0.join("font.akpak");
        let mut writer = PackWriter::new();
        writer.add("font/font.tim", font).unwrap();
        writer.write(&mini).unwrap();
        Some(mini)
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_font_string_render_is_stable() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let dir = TempDir::new();
        let Some(font_pack) = font_pack(&path, &dir) else {
            eprintln!("warning: neither the pack nor ARKLAY_RE1_ROOT carries FONT.TIM");
            return;
        };
        let pack = Pack::open(&font_pack).unwrap();
        let font =
            font::Font::new(crate::tim::decode_4bpp(&pack.read("font/font.tim").unwrap()).unwrap());
        assert_eq!((font.texture.width, font.texture.height), (768, 256));
        assert_eq!(font.metrics, font::FontMetrics::from_sheet_width(768));

        let mut first = Framebuffer::new();
        draw_font_screen(&mut first, &font);
        let mut second = Framebuffer::new();
        draw_font_screen(&mut second, &font);
        assert_eq!(fnv1a(&first.rgba), fnv1a(&second.rgba));
        let opaque = first
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[..3] != [24, 24, 24] && pixel[3] != 0)
            .count();
        assert!(opaque > 1000, "the font sample drew only {opaque} pixels");
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_font_capture_is_deterministic() {
        let _sdl = crate::audio::test_lock::sdl();
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let dir = TempDir::new();
        let Some(font_pack) = font_pack(&path, &dir) else {
            eprintln!("warning: neither the pack nor ARKLAY_RE1_ROOT carries FONT.TIM");
            return;
        };
        let first_path = dir.0.join("font_a.bmp");
        let second_path = dir.0.join("font_b.bmp");

        run_ui(&font_pack, "font", Some(&first_path)).unwrap();
        run_ui(&font_pack, "font", Some(&second_path)).unwrap();

        let first = std::fs::read(&first_path).unwrap();
        let second = std::fs::read(&second_path).unwrap();
        assert_eq!(first, second, "two font captures differ");
        let decoded = bmp::decode(&first).unwrap();
        assert_eq!(
            (decoded.width, decoded.height),
            (WIDTH as u32, HEIGHT as u32)
        );
        assert!(non_black_pixels(&decoded) > 5000);
    }

    /// A one-room pack with both character variants of RDT 106 (the main hall)
    /// and the shipped-style bio card. Title and select art are intentionally
    /// absent: the screens log and continue.
    fn new_game_pack(dir: &TempDir) -> PathBuf {
        let pack_path = dir.0.join("game.akpak");
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        for player_flag in 0..=1u8 {
            let id = RoomId {
                stage: 1,
                room: NEW_GAME_ROOM,
                player_flag,
            };
            writer
                .add(&id.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
                .unwrap();
        }
        writer
            .add(
                &RoomId {
                    stage: 1,
                    room: NEW_GAME_ROOM,
                    player_flag: 0,
                }
                .cut_entry(0),
                bmp_bytes.clone(),
            )
            .unwrap();
        writer
            .add(save::SAVE_PREFIX_ENTRY, synthetic_bio_card())
            .unwrap();
        writer.write(&pack_path).unwrap();
        pack_path
    }

    /// A serialized bio card carrying the shipped new-game fields: the main
    /// hall, the starting item box and the counter at 1.
    fn synthetic_bio_card() -> Vec<u8> {
        let mut card = save::SaveFile {
            room: NEW_GAME_ROOM,
            saves: 1,
            total_held: 2,
            ..save::SaveFile::default()
        };
        card.item_box[0] = game::InventoryItem {
            id: ITEM_HANDGUN_AMMO,
            quantity: 15,
        };
        card.item_box[1] = game::InventoryItem {
            id: ITEM_HANDGUN_AMMO,
            quantity: 15,
        };
        card.player_slots[0] = game::InventoryItem {
            id: ITEM_KNIFE,
            quantity: 0,
        };
        card.player_slots[1] = game::InventoryItem {
            id: ITEM_FIRST_AID_SPRAY,
            quantity: 1,
        };
        card.player_slots[6] = game::InventoryItem {
            id: ITEM_BERETTA,
            quantity: 15,
        };
        card.to_bytes().to_vec()
    }

    fn confirm_input() -> UiInput {
        UiInput {
            any: true,
            confirm: true,
            ..UiInput::default()
        }
    }

    /// Drive `app` until `done` is true, feeding one confirm per tick.
    fn run_until(app: &mut App, ticks: u32, done: impl Fn(&App) -> bool) -> bool {
        for _ in 0..ticks {
            app.update(confirm_input(), player::Input::default(), false)
                .unwrap();
            if done(app) {
                return true;
            }
        }
        false
    }

    #[test]
    fn a_press_released_between_ticks_is_seen_exactly_once() {
        let mut input = InputState::default();
        input.key_down(SDL_SCANCODE_SPACE, false);
        input.key_up(SDL_SCANCODE_SPACE);

        let tick = input.tick();
        assert!(tick.ui.confirm, "the quick tap must reach its tick");
        assert!(tick.action, "the tap is still an action press");
        assert!(tick.ui.any);

        let next = input.tick();
        assert!(!next.ui.confirm, "the press must not repeat");
        assert!(!next.action, "a released key must not act again");
        assert!(!next.ui.any);
        assert_eq!(next.player, player::Input::default());
    }

    #[test]
    fn a_held_key_edges_once_and_keeps_moving() {
        let mut input = InputState::default();
        input.key_down(SDL_SCANCODE_UP, false);
        // OS auto-repeat refreshes the held state without adding an edge.
        input.key_down(SDL_SCANCODE_UP, true);
        input.key_down(SDL_SCANCODE_UP, true);

        let first = input.tick();
        assert!(first.ui.up, "the initial press is an edge");
        assert!(first.ui.any);
        assert!(first.player.up);

        for _ in 0..3 {
            let held = input.tick();
            assert!(!held.ui.up, "a held key must not re-edge");
            assert!(!held.ui.any);
            assert!(held.player.up, "a held key keeps moving");
        }

        input.key_up(SDL_SCANCODE_UP);
        let released = input.tick();
        assert!(!released.player.up, "release clears the held direction");
        assert!(!released.ui.up);
    }

    #[test]
    fn paired_keys_are_tracked_per_physical_key() {
        let mut confirm = InputState::default();
        confirm.key_down(SDL_SCANCODE_SPACE, false);
        confirm.key_down(SDL_SCANCODE_RETURN, false);
        confirm.tick();
        confirm.key_up(SDL_SCANCODE_SPACE);
        assert!(confirm.tick().action, "Return is still held");
        confirm.key_up(SDL_SCANCODE_RETURN);
        assert!(!confirm.tick().action);

        let mut run = InputState::default();
        run.key_down(SDL_SCANCODE_LSHIFT, false);
        run.key_down(SDL_SCANCODE_RSHIFT, false);
        run.tick();
        run.key_up(SDL_SCANCODE_LSHIFT);
        assert!(run.tick().player.run, "the right Shift is still held");
        run.key_up(SDL_SCANCODE_RSHIFT);
        assert!(!run.tick().player.run);
    }

    #[test]
    fn focus_loss_clears_held_and_pending_keys() {
        let mut input = InputState::default();
        input.key_down(SDL_SCANCODE_DOWN, false);
        input.key_down(SDL_SCANCODE_SPACE, false);
        input.clear();

        let tick = input.tick();
        assert_eq!(tick.player, player::Input::default());
        assert!(!tick.action);
        assert_eq!(tick.ui, UiInput::default());
    }

    #[test]
    fn a_catching_up_frame_consumes_each_press_once() {
        let mut input = InputState::default();
        input.key_down(SDL_SCANCODE_TAB, false);
        input.key_up(SDL_SCANCODE_TAB);

        // One slow frame runs two ticks; only the first may see the press.
        assert!(input.tick().ui.start);
        assert!(!input.tick().ui.start);
        assert_eq!(menu_input(input.tick().ui), None);
    }

    #[test]
    fn menu_cues_match_the_original_play_sfx_slots() {
        // The original's inventory handlers play `play_sfx(3, 4)` on a D-pad
        // move, `(3, 6)` on confirm and `(3, 5)` on cancel; those character
        // slots are the shared cursor/decide/cancel samples.
        assert_eq!(menu_cue(MenuInput::Up), Some(ui::UiCue::Cursor));
        assert_eq!(menu_cue(MenuInput::Down), Some(ui::UiCue::Cursor));
        assert_eq!(menu_cue(MenuInput::Left), Some(ui::UiCue::Cursor));
        assert_eq!(menu_cue(MenuInput::Right), Some(ui::UiCue::Cursor));
        assert_eq!(menu_cue(MenuInput::Confirm), Some(ui::UiCue::Decide));
        assert_eq!(menu_cue(MenuInput::Cancel), Some(ui::UiCue::Cancel));
        assert_eq!(menu_cue(MenuInput::PageLeft), None);
        assert_eq!(menu_cue(MenuInput::PageRight), None);
    }

    #[test]
    fn every_bound_key_maps_to_its_edge_and_movement_field() {
        let ui_cases = [
            (
                SDL_SCANCODE_UP,
                UiInput {
                    up: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_DOWN,
                UiInput {
                    down: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_LEFT,
                UiInput {
                    left: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_RIGHT,
                UiInput {
                    right: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_SPACE,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_RETURN,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_X,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_BACKSPACE,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_ESCAPE,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_LEFTBRACKET,
                UiInput {
                    page_left: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_RIGHTBRACKET,
                UiInput {
                    page_right: true,
                    ..UiInput::default()
                },
            ),
            (
                SDL_SCANCODE_TAB,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
            ),
        ];
        for (scancode, expected) in ui_cases {
            let mut input = InputState::default();
            input.key_down(scancode, false);
            input.key_up(scancode);
            let mut expected = expected;
            expected.any = true;
            expected.captured = Some(controls::Source::key(scancode));
            assert_eq!(input.tick().ui, expected, "scancode {}", scancode.0);
        }

        let move_cases = [
            (
                SDL_SCANCODE_UP,
                player::Input {
                    up: true,
                    ..player::Input::default()
                },
            ),
            (
                SDL_SCANCODE_DOWN,
                player::Input {
                    down: true,
                    ..player::Input::default()
                },
            ),
            (
                SDL_SCANCODE_LEFT,
                player::Input {
                    left: true,
                    ..player::Input::default()
                },
            ),
            (
                SDL_SCANCODE_RIGHT,
                player::Input {
                    right: true,
                    ..player::Input::default()
                },
            ),
            (
                SDL_SCANCODE_LSHIFT,
                player::Input {
                    run: true,
                    run_pressed: true,
                    ..player::Input::default()
                },
            ),
            (
                SDL_SCANCODE_RSHIFT,
                player::Input {
                    run: true,
                    run_pressed: true,
                    ..player::Input::default()
                },
            ),
        ];
        for (scancode, expected) in move_cases {
            let mut input = InputState::default();
            input.key_down(scancode, false);
            input.key_up(scancode);
            assert_eq!(input.tick().player, expected, "scancode {}", scancode.0);
        }

        for scancode in [SDL_SCANCODE_SPACE, SDL_SCANCODE_RETURN] {
            let mut input = InputState::default();
            input.key_down(scancode, false);
            input.key_up(scancode);
            assert!(input.tick().action, "scancode {}", scancode.0);
        }
    }

    /// The old teardown order dropped the display (calling `SDL_Quit`) before
    /// the mixer, so the stream was freed twice and the process died under
    /// Windows Error Reporting after a multi-second delay. The mixer's share
    /// of the SDL lifetime keeps the subsystem up until the stream is gone.
    #[test]
    #[ignore = "requires SDL dummy video and audio drivers"]
    fn a_display_can_drop_before_a_live_mixer() {
        let _sdl = crate::audio::test_lock::sdl();
        use sdl3_sys::hints::{SDL_HINT_AUDIO_DRIVER, SDL_ResetHint, SDL_SetHint};

        let _ = unsafe { SDL_SetHint(SDL_HINT_AUDIO_DRIVER, c"dummy".as_ptr()) };
        let display =
            Display::new("Arklay test", true, Graphics::default()).expect("offscreen display");
        let mixer = Mixer::open().expect("dummy audio device should open");
        let _ = unsafe { SDL_ResetHint(SDL_HINT_AUDIO_DRIVER) };

        drop(display);
        drop(mixer);
    }

    #[test]
    fn app_boots_title_and_transitions_into_a_new_game() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, dir.0.join("saves"), false);
        app.boot(AppBoot::Title).unwrap();
        assert!(matches!(app.mode, Mode::Title(_)));

        assert!(
            run_until(&mut app, 256, |app| matches!(app.mode, Mode::Select(_))),
            "the title never opened character select"
        );
        // The confirm flash chain holds the pick for 128 white-in ticks, 128
        // white-out ticks and a 16-tick hold before the game starts.
        assert!(
            run_until(&mut app, 512, |app| matches!(app.mode, Mode::Play(_))),
            "character select never started the new game"
        );
        let Mode::Play(session) = &app.mode else {
            panic!("expected a play mode");
        };
        assert_eq!(
            session.game.id,
            RoomId {
                stage: 1,
                room: NEW_GAME_ROOM,
                player_flag: 0
            }
        );
        assert_eq!(session.game.entities[0].health, NEW_GAME_HEALTH[0]);
    }

    #[test]
    fn the_interactive_gate_opens_the_computer_terminal() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session = GameSession::new(&pack, 0, &dir.0.join("saves")).unwrap();

        // Neither gate flag alone opens the terminal.
        session.maybe_open_computer();
        assert!(session.computer.is_none());
        session.game.apply_flag(game::BANK_SCENARIO, 0x20, 0);
        session.maybe_open_computer();
        assert!(
            session.computer.is_none(),
            "the SysFlags bit is still clear"
        );
        session.game.apply_flag(game::BANK_SYSTEM, 0x1e, 0);
        session.maybe_open_computer();
        assert!(
            session.computer.is_some(),
            "both gate flags open the screen"
        );

        // `computer_lab_init` parked the player and swapped the lighting.
        assert_eq!(session.player.pos[1], -9000);
        assert_eq!(
            session.loaded.room.ambient,
            [ui::computer_lab::LAB_AMBIENT; 3]
        );
        assert_eq!(
            session.loaded.room.lights[0].pos,
            ui::computer_lab::LAB_LIGHTS[0]
        );

        // `computer_lab_finish` clears the gate and restores the saved state.
        let saved_y = session.computer_saved.as_ref().unwrap().player_y;
        session.close_computer();
        assert!(session.computer.is_none());
        assert!(!session.game.flag_test(game::BANK_SCENARIO, 0x20, false));
        assert_eq!(session.player.pos[1], saved_y);
    }

    #[test]
    fn starting_a_session_keeps_the_debug_overlay_enabled() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, dir.0.join("saves"), false);
        assert!(app.debug_menu, "the overlay is enabled by default");

        let session = GameSession::new(&app.pack, 0, &app.save_dir).unwrap();
        app.start_session(session);
        let Mode::Play(session) = &app.mode else {
            panic!("expected a play mode");
        };
        assert!(
            session.debug_menu_enabled,
            "start_session propagates the enabled overlay"
        );
    }

    #[test]
    fn app_debug_load_continues_from_the_slot_or_reopens_the_menu() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let saves = dir.0.join("saves");
        std::fs::create_dir_all(&saves).unwrap();
        let file = save::SaveFile {
            stage: 0,
            room: NEW_GAME_ROOM,
            health: 77,
            ..save::SaveFile::default()
        };
        save::save(&saves, 0, &file).unwrap();
        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, saves, false);
        let session = GameSession::new(&app.pack, 0, &app.save_dir).unwrap();
        app.start_session(session);

        // A missing slot reopens the debug tools.
        let Mode::Play(session) = &mut app.mode else {
            panic!("expected a play mode");
        };
        session.load_slot_requested = Some(3);
        app.update(UiInput::default(), player::Input::default(), false)
            .unwrap();
        let Mode::Play(session) = &mut app.mode else {
            panic!("expected a play mode");
        };
        assert!(session.debug_menu.is_some(), "the failure is visible");
        session.debug_menu = None;

        session.load_slot_requested = Some(0);
        session.game.entities[0].health = 5;
        app.update(UiInput::default(), player::Input::default(), false)
            .unwrap();
        let Mode::Play(session) = &app.mode else {
            panic!("expected a play mode");
        };
        assert_eq!(session.game.entities[0].health, 77, "slot 0 loaded");
        assert!(session.debug_menu.is_none());
    }

    #[test]
    fn app_debug_clear_game_plays_the_ending_chain() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, dir.0.join("saves"), false);
        let session = GameSession::new(&app.pack, 0, &app.save_dir).unwrap();
        app.start_session(session);
        let Mode::Play(session) = &mut app.mode else {
            panic!("expected a play mode");
        };
        session.clear_game_requested = Some(4);
        app.update(UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert!(app.ending.is_some(), "the ending flow is recorded");
        assert!(
            !matches!(app.mode, Mode::Play(_)),
            "the session hands over to the ending"
        );
    }

    #[test]
    fn app_f9_twice_returns_to_the_title() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, dir.0.join("saves"), false);
        let session = GameSession::new(&app.pack, 0, &app.save_dir).unwrap();
        app.start_session(session);

        let f9 = UiInput {
            return_title: true,
            any: true,
            ..UiInput::default()
        };
        app.update(f9, player::Input::default(), false).unwrap();
        let Mode::Play(session) = &app.mode else {
            panic!("expected a play mode");
        };
        assert!(
            session.return_title.is_some(),
            "the first F9 opens the prompt"
        );

        app.update(f9, player::Input::default(), false).unwrap();
        assert!(
            matches!(app.mode, Mode::Title(_)),
            "the second F9 returns to the title"
        );
    }

    /// The `--setup-script`/`--save-out` authoring seam and the `--load`
    /// continue seam: a chunk authored with the Lua API serialises to a block
    /// that boots back through the CLI flag with the authored state.
    #[test]
    fn the_setup_script_authors_a_save_the_load_boot_reads() {
        let _sdl = crate::audio::test_lock::sdl();
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let saves = dir.0.join("saves");
        let script = dir.0.join("setup.lua");
        let saved = dir.0.join("authored.dat");
        std::fs::write(
            &script,
            "local api = ...\napi:set_health(77)\napi:set_item_slot(0, 0x44, 1)\n",
        )
        .unwrap();

        run_ui_with_mods(
            &pack_path,
            "game",
            None,
            &saves,
            0,
            None,
            0,
            &ModOptions::default(),
            false,
            None,
            trace::TraceOptions::default(),
            Authoring {
                setup_script: Some(&script),
                save_out: Some(&saved),
                ..Authoring::default()
            },
        )
        .unwrap();

        let file = save::load_file(&saved).unwrap();
        assert_eq!(file.health, 77);
        assert_eq!(file.player_slots[0].id, 0x44);

        // Boot the written block back through the load seam and trace it.
        let trace_path = dir.0.join("load.trace");
        run_ui_with_mods(
            &pack_path,
            "game",
            None,
            &saves,
            0,
            None,
            0,
            &ModOptions::default(),
            false,
            None,
            trace::TraceOptions {
                path: Some(&trace_path),
                capture_every: 0,
                capture_dir: None,
                ticks: 3,
                input: None,
            },
            Authoring {
                load: Some(&saved),
                ..Authoring::default()
            },
        )
        .unwrap();

        let text = std::fs::read_to_string(&trace_path).unwrap();
        assert_eq!(text.lines().count(), 4, "header plus three ticks");
        assert!(text.contains("t=0 "), "{text}");
    }

    #[test]
    fn app_title_load_opens_the_picker_and_loads_a_save() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let saves = dir.0.join("saves");
        std::fs::create_dir_all(&saves).unwrap();
        let mut file = save::SaveFile {
            // The block is 0-based like the original's g_stageId.
            stage: 0,
            room: NEW_GAME_ROOM,
            character: 0,
            health: 77,
            pos_x: 1234,
            pos_z: 5678,
            angle: 1024,
            ..save::SaveFile::default()
        };
        file.player_slots[0] = game::InventoryItem {
            id: 0x41,
            quantity: 1,
        };
        file.total_held = 1;
        save::save(&saves, 0, &file).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, saves, false);
        app.boot(AppBoot::Title).unwrap();

        // With a save present the title starts on LOAD GAME.
        app.update(confirm_input(), player::Input::default(), false)
            .unwrap();
        let Mode::Title(screen) = &app.mode else {
            panic!("expected the title");
        };
        assert!(screen.test_eval::<bool>("return M.state.any_saves"));
        assert_eq!(screen.test_eval::<u8>("return M.state.selection"), 2);

        assert!(
            run_until(&mut app, 256, |app| matches!(app.mode, Mode::Load(_))),
            "the title never opened the load picker"
        );
        assert!(
            run_until(&mut app, 256, |app| matches!(app.mode, Mode::Play(_))),
            "the load picker never started the game"
        );
        let Mode::Play(session) = &app.mode else {
            panic!("expected a play mode");
        };
        assert_eq!(session.game.entities[0].health, 77);
        assert_eq!(
            session.game.max_health, 140,
            "a loaded save restores the character's maximum health"
        );
        assert_eq!(session.player.pos, [1234, 0, 5678]);
        assert_eq!(session.player.angle, 1024);
        assert_eq!(
            session.game.state_bytes[usize::from(game::STATE_BYTE_SAVES)],
            1,
            "the continue path advances the save counter"
        );
    }

    /// A synthetic 4bpp font sheet: 256x224, every texel palette index 1
    /// (white), so a drawn glyph is measurable without shipping art.
    fn synthetic_font_tim() -> Vec<u8> {
        let (width, height) = (256u16, 224u16);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x10u32.to_le_bytes());
        bytes.extend_from_slice(&0x08u32.to_le_bytes()); // 4bpp with a CLUT
        // CLUT: 16 entries, entry 1 white.
        bytes.extend_from_slice(&44u32.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        for entry in 0..16u16 {
            let word = if entry == 1 { 0x7FFFu16 } else { 0x0000 };
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        // Image: 2 pixels per 16-bit unit, all index 1.
        let units = width / 2;
        bytes.extend_from_slice(&(12 + u32::from(units) * 2 * u32::from(height)).to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&units.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        for _ in 0..(u32::from(units) * u32::from(height)) {
            bytes.extend_from_slice(&0x1111u16.to_le_bytes());
        }
        bytes
    }

    /// The test game pack plus the shipped screen scripts, language files and
    /// a synthetic font, so the Lua save screen runs end to end.
    fn pack_with_lua_screen(dir: &TempDir, base: &Path) -> (PathBuf, PathBuf) {
        let pack = Pack::open(base).unwrap();
        let mut writer = PackWriter::new();
        for path in pack.paths() {
            writer.add(path, pack.read(path).unwrap().to_vec()).unwrap();
        }
        writer.add("font/font.tim", synthetic_font_tim()).unwrap();
        // The base pack names itself, so the mod layer's `base = "re1"`
        // resolves like it does over a converted pack.
        writer
            .add(
                crate::manifest::ENTRY,
                crate::manifest::Manifest::base("re1").render().into_bytes(),
            )
            .unwrap();
        // Room 1001 is the typewriter capture's room (`MENU_ROOM`).
        for player_flag in 0..=1u8 {
            let mut id = RoomId::parse(MENU_ROOM).unwrap();
            id.player_flag = player_flag;
            writer
                .add(&id.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
                .unwrap();
        }
        writer
            .add(
                &RoomId::parse(MENU_ROOM).unwrap().cut_entry(0),
                bmp::encode_to_vec(&test_image()).unwrap(),
            )
            .unwrap();
        for (entry, source) in crate::language::LANGUAGE_FILES {
            writer.add(entry, source.as_bytes().to_vec()).unwrap();
        }
        let path = dir.0.join("lua-screen.akpak");
        writer.write(&path).unwrap();

        // The checked-in CR save-screen mod as its own layer: `mod.lua`
        // registers the screen through the hook API, so the app finds it the
        // way a player's `--mod` layer would be found.
        let manifest = crate::manifest::Manifest {
            kind: crate::manifest::PackKind::Mod,
            base: Some("re1".to_string()),
            lua: vec!["mod.lua".to_string()],
            ..crate::manifest::Manifest::base("crloadsave")
        };
        let mut writer = PackWriter::new();
        writer
            .add(crate::manifest::ENTRY, manifest.render().into_bytes())
            .unwrap();
        for (entry, source) in [
            ("mod.lua", include_str!("../mods/crloadsave/mod.lua")),
            (
                "script/loadsave.lua",
                include_str!("../mods/crloadsave/script/loadsave.lua"),
            ),
            (
                "language/en-US.txt",
                include_str!("../mods/crloadsave/language/en-US.txt"),
            ),
            (
                "language/ja-JP.txt",
                include_str!("../mods/crloadsave/language/ja-JP.txt"),
            ),
        ] {
            writer.add(entry, source.as_bytes().to_vec()).unwrap();
        }
        let mod_path = dir.0.join("crloadsave.akpak");
        writer.write(&mod_path).unwrap();
        (path, mod_path)
    }

    #[test]
    fn app_title_load_runs_the_lua_screen_and_loads_its_chosen_file() {
        let dir = TempDir::new();
        let base = new_game_pack(&dir);
        let (pack_path, mod_path) = pack_with_lua_screen(&dir, &base);
        let saves = dir.0.join("saves");
        std::fs::create_dir_all(&saves).unwrap();

        // One file, and not one of the eight vanilla slots: the title's LOAD
        // GAME comes from the Lua screen's folder probe (CR's replacement for
        // the original's "does a save exist" check), and the loaded file
        // proves the screen chose it.
        let file = save::SaveFile {
            stage: 0,
            room: NEW_GAME_ROOM,
            character: 0,
            health: 55,
            pos_x: 1234,
            pos_z: 5678,
            angle: 1024,
            ..save::SaveFile::default()
        };
        save::save_to_path(&saves.join("a_other.dat"), &file).unwrap();

        let pack = Pack::open_layered(&pack_path, &[mod_path]).unwrap();
        let mut app = App::new(pack, saves, false);
        app.boot(AppBoot::Title).unwrap();

        assert!(run_until(&mut app, 256, |app| matches!(
            app.mode,
            Mode::Load(_)
        )));
        // The screen draws through its script (the pack has no typewriter
        // photo, so the panel, band and text are the drawn layers).
        app.draw();
        let drawn = app
            .frame()
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
            .count();
        assert!(drawn > 1000, "the Lua screen drew only {drawn} pixels");

        assert!(run_until(&mut app, 256, |app| matches!(
            app.mode,
            Mode::Play(_)
        )));
        let Mode::Play(session) = &app.mode else {
            panic!("expected a play mode");
        };
        assert_eq!(
            session.game.entities[0].health, 55,
            "the Lua screen loaded the file it listed first, not slot 0"
        );
    }

    #[test]
    fn the_lua_save_screen_capture_is_deterministic() {
        let dir = TempDir::new();
        let base = new_game_pack(&dir);
        let (pack_path, mod_path) = pack_with_lua_screen(&dir, &base);
        let saves = dir.0.join("saves");
        std::fs::create_dir_all(&saves).unwrap();
        let mut file = save::SaveFile {
            stage: 0,
            room: NEW_GAME_ROOM,
            character: 1,
            saves: 3,
            ..save::SaveFile::default()
        };
        file.total_held = 0;
        save::save_to_path(&saves.join("savedat1.dat"), &file).unwrap();

        let first = dir.0.join("cr_load_a.bmp");
        let second = dir.0.join("cr_load_b.bmp");
        for capture in [&first, &second] {
            run_ui_with_mods(
                &pack_path,
                "load",
                Some(capture),
                &saves,
                0,
                None,
                0,
                &ModOptions {
                    explicit: vec![mod_path.clone()],
                    ..ModOptions::default()
                },
                false,
                None,
                trace::TraceOptions::default(),
                Authoring::default(),
            )
            .unwrap();
        }
        let first_bytes = std::fs::read(&first).unwrap();
        assert_eq!(
            first_bytes,
            std::fs::read(&second).unwrap(),
            "two Lua save-screen captures differ"
        );
        let image = bmp::decode(&first_bytes).unwrap();
        assert_eq!((image.width, image.height), (320, 240));
        assert!(
            non_black_pixels(&image) > 2000,
            "the Lua save-screen capture is mostly black"
        );
    }

    #[test]
    fn the_lua_save_screen_writes_a_file_and_consumes_the_ribbon() {
        let dir = TempDir::new();
        let base = new_game_pack(&dir);
        let (pack_path, mod_path) = pack_with_lua_screen(&dir, &base);
        let saves = dir.0.join("saves");
        std::fs::create_dir_all(&saves).unwrap();
        let old = save::SaveFile {
            stage: 0,
            room: NEW_GAME_ROOM,
            character: 0,
            saves: 2,
            ..save::SaveFile::default()
        };
        save::save_to_path(&saves.join("old.dat"), &old).unwrap();

        let pack = Pack::open_layered(&pack_path, &[mod_path]).unwrap();
        let mut app = App::new(pack, saves.clone(), false);
        app.open_save_capture().unwrap();
        let Mode::Play(session) = &app.mode else {
            panic!("expected the typewriter session");
        };
        let ribbons = session.game.item_count(items::ITEM_INK_RIBBONS);
        let saves_before = session.game.state_bytes[usize::from(game::STATE_BYTE_SAVES)];
        assert!(ribbons >= 1, "the capture seeds a ribbon");

        // The script's top row is `Create New` in save mode: it writes a new
        // file and leaves without touching the existing one. The engine
        // mirrors the write into the live state.
        for _ in 0..30 {
            app.update(confirm_input(), player::Input::default(), false)
                .unwrap();
        }
        let Mode::Play(session) = &app.mode else {
            panic!("expected the resumed session");
        };
        assert!(session.save_screen.is_none(), "the screen never closed");
        assert_eq!(
            session.game.item_count(items::ITEM_INK_RIBBONS),
            ribbons - 1,
            "the write consumed the live ribbon"
        );
        assert!(
            saves.join("old.dat").exists(),
            "Create New replaced an existing file"
        );
        let written = save::load_file(&saves.join("save_01.dat")).unwrap();
        assert_eq!(written.used_item, items::ITEM_INK_RIBBONS);
        assert_eq!(
            session.game.state_bytes[usize::from(game::STATE_BYTE_SAVES)],
            saves_before + 1,
            "the save counter advanced"
        );
    }

    #[test]
    fn the_lua_save_screen_overwrites_the_selected_existing_row() {
        let dir = TempDir::new();
        let base = new_game_pack(&dir);
        let (pack_path, mod_path) = pack_with_lua_screen(&dir, &base);
        let saves = dir.0.join("saves");
        std::fs::create_dir_all(&saves).unwrap();
        let old = save::SaveFile {
            stage: 0,
            room: NEW_GAME_ROOM,
            character: 0,
            saves: 2,
            ..save::SaveFile::default()
        };
        save::save_to_path(&saves.join("old.dat"), &old).unwrap();

        let pack = Pack::open_layered(&pack_path, &[mod_path]).unwrap();
        let mut app = App::new(pack, saves.clone(), false);
        app.open_save_capture().unwrap();

        // Row 0 is `Create New`; move to the first file before confirming.
        app.update(
            UiInput {
                any: true,
                down: true,
                ..UiInput::default()
            },
            player::Input::default(),
            false,
        )
        .unwrap();
        for _ in 0..30 {
            app.update(confirm_input(), player::Input::default(), false)
                .unwrap();
        }

        let Mode::Play(session) = &app.mode else {
            panic!("expected the resumed session");
        };
        assert!(session.save_screen.is_none(), "the screen never closed");
        assert!(
            !saves.join("old.dat").exists(),
            "the selected row was not replaced"
        );
        let written = save::load_file(&saves.join("save_01.dat")).unwrap();
        assert_eq!(written.used_item, items::ITEM_INK_RIBBONS);
    }

    #[test]
    fn the_lua_save_screen_create_new_skips_a_case_variant_name() {
        let dir = TempDir::new();
        let base = new_game_pack(&dir);
        let (pack_path, mod_path) = pack_with_lua_screen(&dir, &base);
        let saves = dir.0.join("saves");
        std::fs::create_dir_all(&saves).unwrap();
        let existing = save::SaveFile {
            stage: 0,
            room: NEW_GAME_ROOM,
            character: 0,
            ..save::SaveFile::default()
        };
        save::save_to_path(&saves.join("SAVE_01.DAT"), &existing).unwrap();

        let pack = Pack::open_layered(&pack_path, &[mod_path]).unwrap();
        let mut app = App::new(pack, saves.clone(), false);
        app.open_save_capture().unwrap();

        for _ in 0..30 {
            app.update(confirm_input(), player::Input::default(), false)
                .unwrap();
        }
        let Mode::Play(session) = &app.mode else {
            panic!("expected the resumed session");
        };
        assert!(session.save_screen.is_none(), "the screen never closed");
        assert!(
            saves.join("SAVE_01.DAT").exists(),
            "Create New overwrote a case variant"
        );
        // The scan lowercases names, so the free name after SAVE_01.DAT is
        // save_02.dat rather than an overlapping save_01.dat.
        let written = save::load_file(&saves.join("save_02.dat")).unwrap();
        assert_eq!(written.used_item, items::ITEM_INK_RIBBONS);
    }

    #[test]
    fn the_typewriter_capture_runs_the_lua_screen() {
        let dir = TempDir::new();
        let base = new_game_pack(&dir);
        let (pack_path, mod_path) = pack_with_lua_screen(&dir, &base);
        let saves = dir.0.join("saves");
        let first = dir.0.join("cr_save_a.bmp");
        let second = dir.0.join("cr_save_b.bmp");
        for capture in [&first, &second] {
            run_ui_with_mods(
                &pack_path,
                "save",
                Some(capture),
                &saves,
                0,
                None,
                0,
                &ModOptions {
                    explicit: vec![mod_path.clone()],
                    ..ModOptions::default()
                },
                false,
                None,
                trace::TraceOptions::default(),
                Authoring::default(),
            )
            .unwrap();
        }
        let first_bytes = std::fs::read(&first).unwrap();
        assert_eq!(
            first_bytes,
            std::fs::read(&second).unwrap(),
            "two typewriter captures differ"
        );
        let image = bmp::decode(&first_bytes).unwrap();
        assert!(
            non_black_pixels(&image) > 2000,
            "the typewriter capture with the Lua screen is mostly black"
        );
    }

    fn solid_image(color: [u8; 4]) -> Image {
        let mut rgba = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
        for _ in 0..WIDTH * HEIGHT {
            rgba.extend_from_slice(&color);
        }
        Image {
            width: WIDTH as u32,
            height: HEIGHT as u32,
            rgba,
        }
    }

    #[test]
    fn select_slide_frame_sequence_moves_the_cards() {
        // A pack with the two select sheets, as `--ui select` needs to draw
        // anything at all.
        let dir = TempDir::new();
        let pack_path = dir.0.join("select.akpak");
        let mut writer = PackWriter::new();
        writer
            .add(
                "ui/sel_back.bmp",
                bmp::encode_to_vec(&solid_image([0, 0, 40, 255])).unwrap(),
            )
            .unwrap();
        writer
            .add(
                "ui/select_b.bmp",
                bmp::encode_to_vec(&test_image()).unwrap(),
            )
            .unwrap();
        writer.write(&pack_path).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, dir.0.join("saves"), false);
        app.boot(AppBoot::CharSelect).unwrap();
        // Let the fade-in finish so the frames differ by the slide alone.
        app.settle(40).unwrap();
        app.draw();
        let resting = app.frame().rgba.clone();

        // Press right: the first slide frame moves the cards.
        app.update(
            UiInput {
                right: true,
                ..UiInput::default()
            },
            player::Input::default(),
            false,
        )
        .unwrap();
        app.draw();
        let first = app.frame().rgba.clone();
        assert_ne!(first, resting, "the first slide frame moved the cards");

        // Mid-slide the pose keeps changing.
        app.settle(9).unwrap();
        app.draw();
        let mid = app.frame().rgba.clone();
        assert_ne!(mid, first, "the mid-slide pose is a new frame");
        assert_ne!(mid, resting);

        // The slide lands after 34 ticks with the pick flipped, and the settled
        // frame differs from the boot pose.
        app.settle(25).unwrap();
        app.draw();
        let settled = app.frame().rgba.clone();
        assert_ne!(settled, resting, "the landed pose swapped the cards");
        let Mode::Select(screen) = &app.mode else {
            panic!("character select must stay open");
        };
        assert_eq!(screen.test_eval::<String>("return M.state.stage"), "idle");
        assert_eq!(screen.test_eval::<u8>("return M.state.selected"), 1);
    }

    /// A minimal 8bpp TIM with four pixels and a 256-entry palette, enough for
    /// the map screen's plan and backdrop layers.
    fn synthetic_map_tim() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0x10u32.to_le_bytes());
        data.extend_from_slice(&9u32.to_le_bytes());
        data.extend_from_slice(&524u32.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&256u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        for index in 0..256u16 {
            let value: u16 = if index == 0 { 0 } else { 0x7FFF };
            data.extend_from_slice(&value.to_le_bytes());
        }
        let pixels = [13u8, 14, 200, 0];
        data.extend_from_slice(&(12u32 + pixels.len() as u32).to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&pixels);
        data
    }

    #[test]
    fn the_map_tab_opens_without_the_radio_flag() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session = GameSession::from_room(
            &pack,
            RoomId {
                stage: 1,
                room: NEW_GAME_ROOM,
                player_flag: 0,
            },
            Path::new("saves"),
        )
        .unwrap();
        assert!(
            !session.game.has_radio(),
            "the scenario starts without the radio"
        );
        session.open_menu(&pack);
        session.handle_tab(&pack, 0);
        assert!(
            session.map.is_some(),
            "the original's map tab needs no radio (DAT_00ae9f1f gates only \
             the radio tab)"
        );
    }

    #[test]
    fn map_capture_draws_the_floor_plan() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("map.akpak");
        let id = RoomId::parse("1001").unwrap();
        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer
            .add(&id.cut_entry(0), bmp::encode_to_vec(&test_image()).unwrap())
            .unwrap();
        writer
            .add(
                ui::map::TABLES_ENTRY,
                ui::map::MapTables::default().encode(),
            )
            .unwrap();
        writer.add("map/map0d.tim", synthetic_map_tim()).unwrap();
        writer
            .add(ui::map::BLUE_ENTRY, synthetic_map_tim())
            .unwrap();
        writer.write(&pack_path).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, dir.0.join("saves"), false);
        app.boot(AppBoot::Map).unwrap();
        app.settle(MENU_CAPTURE_TICKS).unwrap();
        app.draw();
        let painted = app
            .frame()
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
            .count();
        assert!(painted > 0, "the map capture drew the floor plan");
        let Mode::Play(session) = &app.mode else {
            panic!("the map capture must own a session");
        };
        assert!(session.map.is_some());
    }

    /// A two-room pack whose first room walks the player into a scripted door
    /// on the first tick, with distinct backgrounds so the destination frame
    /// is identifiable. Returns `(path, source, destination, source bg, dest bg)`.
    fn walk_in_door_pack(dir: &TempDir) -> (std::path::PathBuf, RoomId, RoomId, Image, Image) {
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
        let red = solid_image([200, 0, 0, 255]);
        let blue = solid_image([0, 0, 200, 255]);

        // The shipped door zone sits at [100,200,300,400]; move it to
        // [500,0,200,200] so the idle spawn's forward reach probe at [600,0,0]
        // lands inside it (negative origins wrap in the original's
        // unsigned-point test). Probe flags 0x01 walk in every frame.
        let mut init = door_init();
        for (offset, value) in [(2usize, 500i16), (4, 0), (6, 200), (8, 200)] {
            init[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        init[25] = 0x01;

        let pack_path = dir.0.join("game.akpak");
        let mut writer = PackWriter::new();
        writer.add(&a.rdt_entry(), synthetic_rdt(&init)).unwrap();
        writer
            .add(&b.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer
            .add(&a.cut_entry(0), bmp::encode_to_vec(&red).unwrap())
            .unwrap();
        writer
            .add(&b.cut_entry(0), bmp::encode_to_vec(&blue).unwrap())
            .unwrap();
        writer.write(&pack_path).unwrap();
        (pack_path, a, b, red, blue)
    }

    #[test]
    fn simulate_room_follows_a_scripted_door_into_the_destination() {
        let dir = TempDir::new();
        let (pack_path, a, b, _red, _blue) = walk_in_door_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();

        let sim = simulate_room(&pack, a, 40, player::Input::default()).unwrap();
        assert_eq!(sim.transitions, vec![b], "the door followed to room 1");
        assert_eq!(sim.id, b);
        assert_eq!(sim.game.id, b);
        assert_eq!(sim.room.room, b.room);
        // The rendered frame shows the destination's background.
        assert_eq!(sim.frame.rgba[0..4], [0, 0, 200, 255]);
    }

    #[test]
    fn ticks_capture_follows_a_scripted_door_into_the_destination() {
        let _sdl = crate::audio::test_lock::sdl();
        let dir = TempDir::new();
        let (pack_path, a, _b, _red, blue) = walk_in_door_pack(&dir);
        let capture_path = dir.0.join("capture.bmp");

        run(&pack_path, a, Some(&capture_path), 40).unwrap();
        let decoded = bmp::decode(&std::fs::read(&capture_path).unwrap()).unwrap();
        assert_eq!(
            decoded.rgba[0..4],
            blue.rgba[0..4],
            "the capture ends in the destination room"
        );
    }

    #[test]
    fn the_special_light_overlay_fills_the_frame() {
        let dir = TempDir::new();
        let (pack_path, a, _b, _red, _blue) = walk_in_door_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let loaded = load_room(&pack, a).unwrap();
        let room = loaded.room;
        let mut game = game::GameState::new(a, &room);
        game.light_fade_set(0, 0x4000, 0b111);
        game.advance_special_light();
        assert_eq!(
            game.special_light_rect(a, 0),
            Some(game::SpecialLight {
                color: [255, 255, 255],
                alpha: 0x80,
                draws: 1,
            })
        );

        let player_state = player::spawn(a, &room);
        let image = render_game_frame(&pack, a, &room, &mut game, &player_state).unwrap();
        // The saturated white veil at alpha 0x80 over the [200, 0, 0] backdrop.
        assert_eq!(
            image.rgba[0..4],
            [228, 128, 128, 255],
            "the scripted overlay covers the background"
        );
    }

    /// A two-room pack whose first room carries the test door into the second,
    /// both rooms carrying the colourful test background.
    fn two_room_door_pack(dir: &TempDir) -> (Pack, RoomId, RoomId) {
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
        (Pack::open(&pack_path).unwrap(), a, b)
    }

    /// Trigger the test door in `session` without ticking its timeline.
    fn trigger_test_door(pack: &Pack, session: &mut GameSession) {
        session.player.pos = [-450, 0, 250];
        session.player.angle = 0;
        session.game.sync_entity_from_player(&session.player);

        for _ in 0..10 {
            session
                .tick(
                    pack,
                    UiInput::default(),
                    player::Input {
                        action_pressed: true,
                        action_held: true,
                        ..player::Input::default()
                    },
                    true,
                )
                .unwrap();
            if session.transition.is_some() {
                break;
            }
        }
        assert!(
            session.transition.is_some(),
            "the door never requested a transition"
        );
    }

    /// Walk `session` into its test door, run the animation out and tear it
    /// down into the destination exactly like the interactive loop: trigger,
    /// tick the timeline, render the finished frame, then finish.
    fn walk_through_test_door(pack: &Pack, session: &mut GameSession) {
        trigger_test_door(pack, session);

        for _ in 0..MAX_TRANSITION_FRAMES {
            session.tick_transition(pack, false);
            if session.transition_finished {
                break;
            }
        }
        assert!(session.transition_finished, "the transition never finished");
        // The finished frame is rendered before teardown, as the loops do.
        session.render(pack);
        session.finish_transition(pack);
    }

    #[test]
    fn session_transition_swaps_rooms() {
        let dir = TempDir::new();
        let (pack, a, b) = two_room_door_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        walk_through_test_door(&pack, &mut session);

        assert_eq!(session.loaded.id, b);
        assert_eq!(session.game.id, b);
        assert_eq!(session.player.pos, [555, 0, 666]);
        assert_eq!(session.player.angle, 1024);
        assert!(session.game.doors[0].is_none(), "the door table is rebuilt");
        assert!(!session.transition_finished);
        assert!(session.transition.is_none());
    }

    #[test]
    fn a_door_transition_defers_the_destination_bgm_until_the_animation_ends() {
        let dir = TempDir::new();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let source = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        // Room 1003 resolves group 0x00 (Bgm_00), unlike the source's 0x0B
        // (Bgm_13) at state 0x08, so the handoff is observable.
        let target = RoomId {
            stage: 1,
            room: 3,
            player_flag: 0,
        };
        let pack_path = dir.0.join("game.akpak");
        let mut writer = PackWriter::new();
        writer
            .add(
                &source.rdt_entry(),
                synthetic_rdt(&door_init_record(3, [555, 0, 666], 1024, 0)),
            )
            .unwrap();
        writer
            .add(&target.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&source.cut_entry(0), bmp_bytes.clone()).unwrap();
        writer.add(&target.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();

        let mut session = GameSession::from_room(&pack, source, Path::new("saves")).unwrap();
        // Boot the source room's group with its channel enabled so a premature
        // handoff would visibly replace it.
        session.game.room_bgm[usize::from(source.room)] = 0x08;
        bgm::update_room_bgm(&mut session.game, source, None);
        assert_eq!(session.game.bgm.state, 0x08);
        assert_eq!(session.game.bgm.channels[0].name, Some("Bgm_13"));

        trigger_test_door(&pack, &mut session);

        let mut loaded_during_animation = false;
        for _ in 0..MAX_TRANSITION_FRAMES {
            session.tick_transition(&pack, false);
            if !session.transition_finished {
                if session.loaded.id == target {
                    loaded_during_animation = true;
                }
                assert_eq!(
                    session.game.bgm.state, 0x08,
                    "the destination BGM state must wait for the animation's end"
                );
                assert_eq!(
                    session.game.bgm.channels[0].name,
                    Some("Bgm_13"),
                    "the outgoing room's track must keep playing under the door"
                );
            }
            if session.transition_finished {
                break;
            }
        }
        assert!(session.transition_finished, "the transition never finished");
        assert!(
            loaded_during_animation,
            "the destination must boot while the animation still runs"
        );
        assert_eq!(session.game.bgm.state, 0x08);
        assert_eq!(
            session.game.bgm.channels[0].name,
            Some("Bgm_00"),
            "the destination's group starts on the animation's final frame"
        );
    }

    #[test]
    fn a_camera_only_transition_defers_the_bgm_handoff_until_the_animation_ends() {
        let dir = TempDir::new();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let room = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let pack_path = dir.0.join("game.akpak");
        let mut writer = PackWriter::new();
        writer
            .add(
                &room.rdt_entry(),
                synthetic_rdt(&door_init_with_camera(0x80)),
            )
            .unwrap();
        writer.add(&room.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();

        let mut session = GameSession::from_room(&pack, room, Path::new("saves")).unwrap();
        session.game.room_bgm[usize::from(room.room)] = 0x08;
        bgm::update_room_bgm(&mut session.game, room, None);
        assert_eq!(session.game.bgm.state, 0x08);
        // The door leaves the room loaded, so the table change is visible only
        // to the handoff at the animation's end.
        session.game.room_bgm[usize::from(room.room)] = 0x10;

        trigger_test_door(&pack, &mut session);
        for _ in 0..MAX_TRANSITION_FRAMES {
            session.tick_transition(&pack, false);
            if !session.transition_finished {
                assert_eq!(
                    session.game.bgm.state, 0x08,
                    "a camera-only record must not re-run the handoff early"
                );
                assert_eq!(session.game.bgm.channels[0].name, Some("Bgm_13"));
            }
            if session.transition_finished {
                break;
            }
        }
        assert!(session.transition_finished, "the transition never finished");
        assert_eq!(session.loaded.id, room, "the room must not reload");
        assert_eq!(session.game.id, room, "the room must not reload");
        assert_eq!(
            session.game.bgm.state, 0x10,
            "the camera-only handoff runs on the animation's final frame"
        );
        assert_eq!(
            session.game.bgm.channels[0].name,
            Some("Bgm_13"),
            "the same group keeps its loaded bank"
        );
    }

    #[test]
    fn a_door_transition_fades_the_destination_in_from_black() {
        let dir = TempDir::new();
        let (pack, a, b) = two_room_door_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        walk_through_test_door(&pack, &mut session);
        assert_eq!(session.loaded.id, b);

        // The destination's entry fade is armed like `game_loop`'s outer loop
        // plus the first `main_loop` step: the accumulator starts at `0x7FFF`
        // and `0xE800` is already subtracted, so tick 0 draws `26623 >> 7` =
        // 207 and traces 159 (the post-step state). The counter then steps the
        // alpha down 48 a tick: 111, 63, 15, before the state goes negative.
        //
        // The app loop draws frames without a gameplay tick too, and the first
        // of them lands right after the teardown: it must draw the armed
        // `0x7FFF` veil, the reference's outer-loop arm frame. The tick frames
        // then draw the accumulator before their step, so the visible sequence
        // is 255, 207, 159, 111, 63, 15, clear.
        session.render(&pack);
        assert_eq!(
            session.framebuffer.rgba[0..4],
            [0, 0, 0, 255],
            "the first destination frame is fully veiled, not the unfaded room"
        );
        let mut drawn = vec![session.room_fade_draw_alpha()];
        let mut alphas = Vec::new();
        for _ in 0..7 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            session.render(&pack);
            alphas.push(session.room_fade_alpha());
            drawn.push(session.room_fade_draw_alpha());
        }
        assert_eq!(
            alphas,
            [Some(159), Some(111), Some(63), Some(15), None, None, None]
        );
        assert_eq!(
            drawn,
            [
                Some(255),
                Some(207),
                Some(159),
                Some(111),
                Some(63),
                Some(15),
                None,
                None
            ],
            "the first destination frame is the full black veil, then the draw-then-add steps"
        );
        // The fade stays cleared after it has run out.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        session.render(&pack);
        assert_eq!(session.room_fade_alpha(), None);
        assert_eq!(session.room_fade_draw_alpha(), None);
    }

    #[test]
    fn the_menu_fade_latch_slows_the_room_entry_fade() {
        let dir = TempDir::new();
        let (pack, a, _b) = two_room_door_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        session
            .game
            .apply_flag(game::BANK_SCENARIO, game::SCENARIO_FLAG_MENU_FADE_LATCH, 0);
        walk_through_test_door(&pack, &mut session);

        // `0xFF5D` (-163) steps the alpha down by one or two rather than 48;
        // the accumulator already carries the entry's first step, so the first
        // drawn alpha is 253.
        let mut alphas = Vec::new();
        for _ in 0..3 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            session.render(&pack);
            alphas.push(session.room_fade_alpha());
        }
        assert_eq!(alphas, [Some(253), Some(252), Some(250)]);
        // The slow counter is still running where the normal one has cleared.
        for _ in 0..3 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        session.render(&pack);
        assert!(session.room_fade_alpha().is_some());
        // It does run out eventually (about 200 frames).
        for _ in 0..220 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        session.render(&pack);
        assert_eq!(session.room_fade_alpha(), None);
    }

    /// The cutscene letterbox bars ramp in while the bank-5 intensity flag is
    /// set and ramp back out when it clears.
    #[test]
    fn screen_intensity_bars_ramp_over_the_scene() {
        let dir = TempDir::new();
        let (pack, a, _b) = two_room_door_pack(&dir);
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        let pixel = |session: &GameSession, x: usize, y: usize| -> [u8; 4] {
            let offset = (y * session.framebuffer.width as usize + x) * 4;
            session.framebuffer.rgba[offset..offset + 4]
                .try_into()
                .unwrap()
        };

        session.game.apply_flag(5, game::MSF_SCREEN_INTENSITY, 0);
        for _ in 0..16 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        session.render(&pack);
        assert_eq!(
            &pixel(&session, 160, 10)[..3],
            &[0, 0, 0],
            "the top bar is opaque at the ramp ceiling"
        );
        assert_eq!(
            &pixel(&session, 160, 220)[..3],
            &[0, 0, 0],
            "the bottom bar is opaque at the ramp ceiling"
        );
        assert_ne!(
            &pixel(&session, 160, 100)[..3],
            &[0, 0, 0],
            "the scene still shows between the bars"
        );

        session.game.apply_flag(5, game::MSF_SCREEN_INTENSITY, 1);
        for _ in 0..16 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        session.render(&pack);
        assert_ne!(
            &pixel(&session, 160, 10)[..3],
            &[0, 0, 0],
            "the top bar ramped away"
        );
        assert_ne!(
            &pixel(&session, 160, 220)[..3],
            &[0, 0, 0],
            "the bottom bar ramped away"
        );
    }

    #[test]
    fn a_transition_reruns_on_room_load_for_the_destination() {
        use crate::manifest;

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
        let manifest = manifest::Manifest {
            kind: manifest::PackKind::Mod,
            base: Some("base".to_string()),
            lua: vec!["lua/rooms.lua".to_string()],
            ..manifest::Manifest::base("demo")
        };
        let mut writer = PackWriter::new();
        writer
            .add(manifest::ENTRY, manifest.render().into_bytes())
            .unwrap();
        writer
            .add(&a.rdt_entry(), synthetic_rdt(&door_init()))
            .unwrap();
        writer
            .add(&b.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&a.cut_entry(0), bmp_bytes.clone()).unwrap();
        writer.add(&b.cut_entry(0), bmp_bytes).unwrap();
        writer
            .add(
                "lua/rooms.lua",
                "\
function on_room_load(api)
    if api:room() == \"100\" then api:flag_set(0, 5, true) end
    if api:room() == \"101\" then api:flag_set(0, 6, true) end
end
"
                .as_bytes()
                .to_vec(),
            )
            .unwrap();
        writer.write(&pack_path).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
        assert!(session.game.flags[0].bit(5), "the source room loaded");
        assert!(!session.game.flags[0].bit(6));
        session.player.pos = [-450, 0, 250];
        session.player.angle = 0;
        session.game.sync_entity_from_player(&session.player);

        for _ in 0..10 {
            session
                .tick(
                    &pack,
                    UiInput::default(),
                    player::Input {
                        action_pressed: true,
                        action_held: true,
                        ..player::Input::default()
                    },
                    true,
                )
                .unwrap();
            if session.transition.is_some() {
                break;
            }
        }
        assert!(session.transition.is_some(), "the door never opened");
        for _ in 0..MAX_TRANSITION_FRAMES {
            session.tick_transition(&pack, false);
            if session.transition_finished {
                break;
            }
        }
        session.finish_transition(&pack);

        assert_eq!(session.loaded.id, b);
        assert!(
            session.game.flags[0].bit(6),
            "the destination's on_room_load ran again"
        );
    }

    /// A pack with one synthetic room, its cut background and a 64-entry
    /// global message table whose entries all wait for input.
    fn message_pack(dir: &TempDir) -> PathBuf {
        let pack_path = dir.0.join("game.akpak");
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        // Index 0 is global 0xC0, the ground pick-up yes/no prompt: "A" then
        // confirm. The branch record is `[no-offset 2][tag 10][arg 0]`: Yes
        // runs the take action, No lands on the `0x01` terminator and does
        // nothing.
        // Index 63 is the same yes/no stream used by the direct post-action
        // test.
        let mut language = String::new();
        for index in 0..64 {
            let value = if index == 0x00 || index == 0x3F {
                "0{branch 2 10 0}"
            } else {
                "0"
            };
            language.push_str(&format!("MESSAGE_{index:02X} = \"{value}\"\n"));
        }
        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer
            .add("language/en-US.txt", language.as_bytes().to_vec())
            .unwrap();
        writer.write(&pack_path).unwrap();
        pack_path
    }

    /// A pack with one synthetic room, its cut background and the cancel cue
    /// the pause menu plays, for the cue-counting tests.
    fn menu_cue_pack(dir: &TempDir) -> PathBuf {
        let pack_path = dir.0.join("menu-cues.akpak");
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        // Long enough to outlive the tick: the count reads the live voices.
        writer
            .add("se/cancel.wav", voice_wav_bytes(&[1000; 6000]))
            .unwrap();
        writer
            .add("se/decide.wav", voice_wav_bytes(&[1000; 6000]))
            .unwrap();
        writer.write(&pack_path).unwrap();
        pack_path
    }

    #[test]
    fn a_paused_message_ignores_player_input_until_dismissed() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // Global id 0x41 with the inspect pause word 0x00FF, which clears the
        // player state-machine bit 0. (Index 0 is the 0xC0 take prompt in this
        // fixture.)
        session.game.show_message(0x41, 0x00FF);
        assert!(session.game.message_locks_controls());

        let before = session.game.frame;
        let movement = player::Input {
            up: true,
            ..player::Input::default()
        };
        for _ in 0..4 {
            session
                .tick(&pack, UiInput::default(), movement, false)
                .unwrap();
        }
        assert_eq!(session.game.frame, before + 4, "the room tick continues");
        assert!(session.game.message.active);
        assert!(
            session.game.message_locks_controls(),
            "the window is still masking the player-state bit"
        );
        // The scripts still see the raw pad while the player is frozen; only
        // the player's own state machine is skipped.
        assert_eq!(session.game.dpad_held & 1, 1, "the raw pad was published");

        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        session
            .tick(&pack, UiInput::default(), player::Input::default(), true)
            .unwrap();
        assert!(!session.game.message.active, "the action key dismissed it");
        assert!(!session.game.message_locks_controls());
    }

    /// A player model with flat one-keyframe clips: enough for the locomotion
    /// and the scripted clip driver without packing real assets.
    fn synthetic_player_assets() -> PlayerAssets {
        fn clip_bank(count: usize) -> Vec<Clip> {
            vec![
                Clip {
                    frames: (0..3)
                        .map(|_| ClipFrame {
                            keyframe: 0,
                            timing: 1,
                        })
                        .collect(),
                };
                count
            ]
        }
        let keyframes = vec![Keyframe::default()];
        PlayerAssets {
            emd: Emd {
                skeleton: Skeleton::default(),
                keyframes: keyframes.clone(),
                clips: clip_bank(0x24),
                damage_clips: Vec::new(),
                damage_keyframes: Vec::new(),
                mesh: Tmd::default(),
                texture: Texture8 {
                    width: 0,
                    height: 0,
                    indices: Vec::new(),
                    palettes: Vec::new(),
                    stp: Vec::new(),
                },
            },
            emw: Emw {
                skeleton: Skeleton::default(),
                keyframes,
                clips: clip_bank(6),
                mesh: Tmd::default(),
            },
            weapons: std::collections::HashMap::new(),
        }
    }

    /// The end-of-tick joint store poses from the live clock: a pad-driven
    /// walk must advance its skeleton frame by frame. Regression: the store
    /// read the script mirror's clock (whose words only change on scripted
    /// publishes) and pinned the live clock to it every tick, so the walk
    /// clock's pending blends were dropped and the model rendered a single
    /// frozen pose while the trace kept moving.
    #[test]
    fn a_walking_player_advances_through_the_joint_store() {
        let (pack, _id, mut room, mut game, mut player_state, mut assets) = scripted_room_fixture();
        // Two poses the walk clip alternates between, on the fixture's body
        // skeleton so the composed joint world carries the rotation.
        assets.emd.skeleton = Skeleton {
            relative: vec![[0, 0, 0]],
            children: vec![vec![]],
        };
        assets.emw.keyframes = vec![
            Keyframe {
                vertex: [0, 0, 0],
                offset: [0, 0, 0],
                rotations: vec![[0, 0, 0]],
            },
            Keyframe {
                vertex: [0, 0, 0],
                offset: [0, 0, 0],
                rotations: vec![[0x200, 0, 0]],
            },
        ];
        let still = Clip {
            frames: vec![ClipFrame {
                keyframe: 0,
                timing: 1,
            }],
        };
        assets.emw.clips = vec![
            still.clone(),
            still.clone(),
            // EMW clip 2 is the walk cycle.
            Clip {
                frames: vec![
                    ClipFrame {
                        keyframe: 1,
                        timing: 1,
                    },
                    ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    },
                ],
            },
            still,
        ];
        let scripts = scd::ir::Scripts::default();
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);
        let walking = player::Input {
            up: true,
            ..player::Input::default()
        };
        let mut poses = Vec::new();
        for _ in 0..6 {
            tick_scripted_room(
                &pack,
                &mut room,
                &mut game,
                &mut player_state,
                &assets,
                &mut command_vm,
                &mut event_vm,
                walking,
            );
            let first = game
                .joint_worlds
                .first()
                .and_then(|worlds| worlds.first())
                .expect("the store composed the player's joint worlds");
            poses.push(first.r);
        }
        assert_eq!(player_state.behavior, 1, "the pad selected the walk");
        assert!(
            poses.windows(2).any(|pair| pair[0] != pair[1]),
            "the stored walk pose stayed frozen across six ticks: {poses:?}"
        );
    }

    #[allow(clippy::too_many_arguments)] // the seam already takes this many
    fn tick_scripted_room(
        pack: &Pack,
        room: &mut RoomState,
        game: &mut game::GameState,
        player_state: &mut player::PlayerState,
        assets: &PlayerAssets,
        command_vm: &mut scd::vm::CommandVm,
        event_vm: &mut scd::vm::EventVm,
        input: player::Input,
    ) {
        let mut npc_models = enemy::EntityModelCache::default();
        let _ = tick_room(
            command_vm,
            event_vm,
            RoomContext {
                room,
                game,
                player: player_state,
                player_assets: Some(assets),
                pack,
                npc_models: &mut npc_models,
                enemy_lua: None,
            },
            input,
        );
    }

    /// The player-side fixture shared by the message and scripted-state tests:
    /// a room with one cut and one walk zone, the player at the origin and an
    /// empty pack for the models.
    fn scripted_room_fixture() -> (
        Pack,
        RoomId,
        RoomState,
        game::GameState,
        player::PlayerState,
        PlayerAssets,
    ) {
        let pack = Pack::from_bytes(PackWriter::new().to_bytes().unwrap()).unwrap();
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let room = effect_test_room();
        let mut game = game::GameState::new(id, &room);
        // The player is past its one-frame spawn init, exactly like every
        // tick after a room boot.
        game.entities[0].set_state(1);
        let mut player_state = player::spawn(id, &room);
        player_state.pos = [0, 0, 0];
        player_state.angle = 0;
        game.sync_entity_from_player(&player_state);
        (
            pack,
            id,
            room,
            game,
            player_state,
            synthetic_player_assets(),
        )
    }

    #[test]
    fn an_inspect_message_freezes_the_player_until_dismissed() {
        let (pack, _id, mut room, mut game, mut player_state, assets) = scripted_room_fixture();
        game.room_actions[2] = Some(game::RoomAction {
            slot: 2,
            kind: game::RoomActionKind::Message,
            zone: [0, 0, 2000, 2000],
            sce: 2,
            handler: 2,
            flags: 0x81,
            params: [2, 0x81, 0x41, 0x00, 0xFF, 0x00, 0, 0],
            item_data: None,
            room_items_flag: 0xFF,
            reach_animation: false,
        });
        let scripts = scd::ir::Scripts::default();
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);

        let start = player_state.pos;
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input {
                up: true,
                action_pressed: true,
                action_held: true,
                ..player::Input::default()
            },
        );
        assert!(game.message.active, "the inspect probe raised the window");
        assert!(game.message_locks_controls());
        let frozen = player_state.pos;
        assert_ne!(
            frozen, start,
            "the probe fires after the locomotion, so the raise tick still moved"
        );

        // From the next tick the pad no longer reaches the locomotion.
        let hold = player::Input {
            up: true,
            ..player::Input::default()
        };
        for _ in 0..5 {
            tick_scripted_room(
                &pack,
                &mut room,
                &mut game,
                &mut player_state,
                &assets,
                &mut command_vm,
                &mut event_vm,
                hold,
            );
        }
        assert_eq!(
            player_state.pos, frozen,
            "the inspect message froze the player"
        );
        assert_eq!(game.dpad_held & 1, 1, "the scripts still see the held pad");

        // Dismissing restores the pad-driven locomotion.
        game.cancel_message();
        assert!(!game.message_locks_controls());
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            hold,
        );
        assert_ne!(
            player_state.pos, frozen,
            "movement resumed once the window was dismissed"
        );
    }

    #[test]
    fn a_room_tick_wipes_the_per_frame_room_state_fields() {
        let (pack, _id, mut room, mut game, mut player_state, assets) = scripted_room_fixture();
        game.state_bytes[usize::from(game::STATE_BYTE_PICKED_ITEM)] = 9;
        game.state_bytes[usize::from(game::STATE_BYTE_USED_ITEM)] = 8;
        game.state_bytes[usize::from(game::STATE_BYTE_FWD_ACTION)] = 7;
        game.flags[usize::from(game::BANK_SYSTEM)].bytes_mut()[5] = 0xAB;
        let scripts = scd::ir::Scripts::default();
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);

        // Empty scripts write nothing, so only the tick's own reset can clear
        // the seeded fields.
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input::default(),
        );

        assert_eq!(
            game.state_bytes[usize::from(game::STATE_BYTE_PICKED_ITEM)],
            0
        );
        assert_eq!(game.state_bytes[usize::from(game::STATE_BYTE_USED_ITEM)], 0);
        assert_eq!(
            game.state_bytes[usize::from(game::STATE_BYTE_FWD_ACTION)],
            0
        );
        assert_eq!(
            game.flags[usize::from(game::BANK_SYSTEM)].bytes()[4..8],
            [0; 4],
            "the second system word is cleared"
        );
    }

    #[test]
    fn an_action_press_pulse_reaches_the_next_frame_only() {
        let (pack, _id, mut room, mut game, mut player_state, assets) = scripted_room_fixture();
        game.room_actions[2] = Some(game::RoomAction {
            slot: 2,
            kind: game::RoomActionKind::FlagBankSet,
            zone: [0, 0, 2000, 2000],
            sce: 7,
            handler: 7,
            flags: 0x81,
            params: [7, 0x81, 4, 0, 32, 0, 1, 0],
            item_data: None,
            room_items_flag: 0xFF,
            reach_animation: false,
        });
        let container = scd::asm::assemble(
            "\
.version 1

.main
.block
    if                      off_0010
    ck                      FG_ROOM, 32, 0
    message                 0x41, 0x00FF
off_0010:
    endif                   0
    end                     0
",
        )
        .unwrap()
        .to_container()
        .unwrap();
        let scripts = scd::reader::parse(&container).unwrap();
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);

        // The probe runs after the main script, so the press frame's main pass
        // does not see the pulse it sets.
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input {
                action_pressed: true,
                action_held: true,
                ..player::Input::default()
            },
        );
        assert!(
            !game.message.active,
            "the press frame's script ran before its own probe"
        );
        assert!(
            game.flags[usize::from(game::BANK_SYSTEM)].bit(32),
            "the probe set the pulse"
        );

        // The next frame's main pass sees the pulse, and the room-state reset
        // after the event pass wipes it.
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input::default(),
        );
        assert!(game.message.active, "the main script saw the pulse");
        assert_eq!(game.message.id, Some(0x41));
        assert!(
            !game.flags[usize::from(game::BANK_SYSTEM)].bit(32),
            "the reset wiped the pulse"
        );

        // No press, no pulse: the following frame raises nothing new.
        game.cancel_message();
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input::default(),
        );
        assert!(!game.message.active, "the pulse does not latch");
    }

    /// `update_player_anim` refreshes `zoneFlags` bit 0 from the current
    /// camera's switch zone every frame. The renderer's whole-entity gate
    /// reads the low byte, so the bit is what keeps a scripted inspect (which
    /// parks the player outside the camera zone) from drawing the model.
    #[test]
    fn the_player_switch_zone_bit_tracks_the_camera_zone() {
        let (pack, _id, mut room, mut game, mut player_state, assets) = scripted_room_fixture();
        let scripts = scd::ir::Scripts::default();
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);

        // Inside the cut-0 switch zone [0,0]-[2000,2000].
        player_state.pos = [1000, 0, 1000];
        game.sync_entity_from_player(&player_state);
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input::default(),
        );
        assert_eq!(game.entities[0].zone_flags & 0x01, 0x01);

        // Outside it: the bit clears and the whole render gate reads zero.
        player_state.pos = [5000, 0, 1000];
        game.sync_entity_from_player(&player_state);
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input::default(),
        );
        assert_eq!(game.entities[0].zone_flags & 0x01, 0);
        assert_eq!(
            game.entities[0].zone_flags & 0x7F,
            0,
            "the whole-entity gate reads clear"
        );
    }

    /// The player runs `EntityUpdateLookAtAngles` every tick after the state
    /// machine, exactly like `update_player_anim`. Before the port only the
    /// NPC/monster drivers slewed, so a cutscene that aimed the player's head
    /// with `s1_lookat` left it facing straight ahead.
    #[test]
    fn the_player_runs_the_look_at_slew_every_tick() {
        let (pack, _id, mut room, mut game, mut player_state, assets) = scripted_room_fixture();
        let scripts = scd::ir::Scripts::default();
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);

        game.entities[0].angle = 0;
        game.entities[0].look_at_flags = 0x13; // slew + yaw + absolute target
        game.entities[0].look_at_yaw_step = 0x40;
        game.entities[0].look_at_pitch_step = 0x40;
        game.entities[0].target = [1000, 0, 1000];

        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input::default(),
        );
        assert_eq!(
            game.entity_anims[0].look_at_yaw, -0x40,
            "the player's tracking joint slewed"
        );

        // Clearing the slew bit freezes the angle in place (the render pass
        // still composes the last aim).
        game.entities[0].look_at_flags = 0;
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input::default(),
        );
        assert_eq!(game.entity_anims[0].look_at_yaw, -0x40);
    }

    #[test]
    fn an_inspect_message_without_a_pause_word_keeps_control() {
        let (pack, _id, mut room, mut game, mut player_state, assets) = scripted_room_fixture();
        game.room_actions[2] = Some(game::RoomAction {
            slot: 2,
            kind: game::RoomActionKind::Message,
            zone: [0, 0, 2000, 2000],
            sce: 2,
            handler: 2,
            flags: 0x81,
            // Pause word 0 changes nothing at all.
            params: [2, 0x81, 0x41, 0x00, 0x00, 0x00, 0, 0],
            item_data: None,
            room_items_flag: 0xFF,
            reach_animation: false,
        });
        let scripts = scd::ir::Scripts::default();
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);

        let hold = player::Input {
            up: true,
            ..player::Input::default()
        };
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            player::Input {
                action_pressed: true,
                action_held: true,
                ..hold
            },
        );
        assert!(game.message.active);
        assert!(!game.message_locks_controls());
        let before = player_state.pos;
        for _ in 0..3 {
            tick_scripted_room(
                &pack,
                &mut room,
                &mut game,
                &mut player_state,
                &assets,
                &mut command_vm,
                &mut event_vm,
                hold,
            );
        }
        assert_ne!(
            player_state.pos, before,
            "a zero pause word leaves the locomotion running"
        );
    }

    #[test]
    fn a_script_message_freezes_the_player_on_the_same_tick() {
        let (pack, _id, mut room, mut game, mut player_state, assets) = scripted_room_fixture();
        let container = scd::asm::assemble(
            "\
.version 1

.main
.block
    message                 0x45, 0x00FF
",
        )
        .unwrap()
        .to_container()
        .unwrap();
        let scripts = scd::reader::parse(&container).unwrap();
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);

        let hold = player::Input {
            up: true,
            ..player::Input::default()
        };
        let start = player_state.pos;
        // The command pass runs before the player's update, so the window the
        // script raises this tick freezes this tick.
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            hold,
        );
        assert!(game.message.active, "the main script raised the window");
        assert!(game.message_locks_controls());
        assert_eq!(
            player_state.pos, start,
            "the same tick's locomotion was already frozen"
        );
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            hold,
        );
        assert_eq!(player_state.pos, start, "and it stays frozen");
    }

    #[test]
    fn a_scripted_player_state_locks_movement_until_state_one_returns() {
        let (pack, _id, mut room, mut game, mut player_state, assets) = scripted_room_fixture();
        let container = scd::asm::assemble(
            "\
.version 1

.main

.event event_00
    evt_actor_begin
    act_anim_flags          0x20, 0x21, 0
    act_end
    evt_finish

.event event_01
    evt_actor_begin
    act_idle
    act_end
    evt_finish
",
        )
        .unwrap()
        .to_container()
        .unwrap();
        let scripts = scd::reader::parse(&container).unwrap();
        let mut command_vm = scd::vm::CommandVm::new(&scripts);
        let mut event_vm = scd::vm::EventVm::new(&scripts);

        let hold = player::Input {
            up: true,
            ..player::Input::default()
        };
        let start = player_state.pos;
        // Event 0 poses the scripted clip on the player slot; the player update
        // runs the state-8 driver instead of the locomotion.
        event_vm.start(0, 0);
        for _ in 0..12 {
            tick_scripted_room(
                &pack,
                &mut room,
                &mut game,
                &mut player_state,
                &assets,
                &mut command_vm,
                &mut event_vm,
                hold,
            );
        }
        assert_eq!(game.entities[0].state(), 8);
        assert_eq!(game.entities[0].animation_id, 0x20);
        assert_eq!(player_state.anim.clip, 0x20, "the scripted clip is posed");
        assert_eq!(
            player_state.pos, start,
            "the scripted state ignores the pad"
        );
        assert!(
            game.flags[usize::from(game::BANK_SYSTEM)].bit(0x21),
            "the scripted completion flag was raised"
        );

        // `act_idle` hands the player back to state 1 and the pad drives again.
        event_vm.start(1, 1);
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            hold,
        );
        assert_eq!(game.entities[0].state(), 1);
        tick_scripted_room(
            &pack,
            &mut room,
            &mut game,
            &mut player_state,
            &assets,
            &mut command_vm,
            &mut event_vm,
            hold,
        );
        assert_ne!(
            player_state.pos, start,
            "the locomotion resumed once state 1 returned"
        );
    }

    #[test]
    fn a_script_raised_message_advances_in_the_same_tick() {
        // The main script raises message 5 every tick. The window must
        // advance after the script pass, so by the end of the first tick it
        // has already resolved its bytes and left the idle phase.
        let dir = TempDir::new();
        let path = dir.0.join("same-tick.akpak");
        let id = RoomId::parse("1000").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let container = scd::asm::assemble(
            "\
.version 1

.init

.main
.block
    message                 0x45, 0
",
        )
        .unwrap()
        .to_container()
        .unwrap();
        let mut language = String::new();
        for index in 0..64 {
            language.push_str(&format!("MESSAGE_{index:02X} = \"0\"\n"));
        }
        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x0E, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.add(&id.scd_entry(), container).unwrap();
        writer
            .add("language/en-US.txt", language.as_bytes().to_vec())
            .unwrap();
        writer.write(&path).unwrap();

        let pack = Pack::open(&path).unwrap();
        let mut session = GameSession::from_room(&pack, id, Path::new("saves")).unwrap();
        for _ in 0..3 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        assert!(session.game.message.active, "the script's message is up");
        assert!(
            session.game.message.has_source(),
            "the window resolved its bytes in the same tick"
        );
        assert_ne!(
            session.game.message.phase(),
            crate::message::MessagePhase::Idle,
            "the window advanced in the same tick"
        );
    }

    #[test]
    fn a_message_pause_freezes_the_scripted_characters_end_to_end() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // A character parked in the spawn state: only the entity tick can move
        // it to idle, so the state word reports whether the tick ran.
        session.game.entities[1] = game::Entity {
            id: 0x23,
            status_flags: game::ENTITY_STATUS_ACTIVE,
            ..game::Entity::default()
        };
        session.game.show_message(0x41, game::MESSAGE_FLAG_ENTITIES);
        assert!(session.game.message_freezes_entities());

        for _ in 0..4 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        assert!(session.game.message.active, "the window is still up");
        assert_eq!(
            session.game.entities[1].state(),
            0,
            "the frozen character never initialised"
        );

        // Closing the window releases the entity tick on the next frame.
        session.game.cancel_message();
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(
            session.game.entities[1].state(),
            1,
            "the character initialised once the window closed"
        );
    }

    #[test]
    #[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
    fn real_message_freeze_blocks_the_climb_press() {
        let Ok(pack_path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&pack_path)).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("1070").unwrap(), Path::new("saves"))
                .unwrap();

        // Stand in reach of the room's climbable ladder, exactly like the
        // real-asset vault test.
        let ladder_slot = (0..session.game.objects.records.len())
            .find(|&slot| {
                session.game.objects.record(slot).unwrap().flag & objects::OBJECT_FLAG_CLIMBABLE
                    != 0
            })
            .expect("ROOM107 declares a climbable ladder");
        let ladder = *session.game.objects.record(ladder_slot).unwrap();
        let angle = (ladder.rotation[1].wrapping_sub(0x800) as u16) & 0x0FFF;
        let radians = f64::from(angle) * std::f64::consts::TAU / 4096.0;
        let reach = 300 + i32::from(ladder.half_extents[0]);
        session.player.angle = angle;
        session.player.pos = [
            ladder.pos[0] - (radians.cos() * f64::from(reach)) as i32,
            0,
            ladder.pos[2] + (radians.sin() * f64::from(reach)) as i32,
        ];
        session.game.sync_entity_from_player(&session.player);

        let press = player::Input {
            action_pressed: true,
            action_held: true,
            ..player::Input::default()
        };

        // The player-state bit clear is the message freeze: the press never
        // reaches the climb scan.
        session.game.message_flags &= !game::MESSAGE_FLAG_PLAYER_STATE;
        assert!(session.game.message_locks_controls());
        session
            .tick(&pack, UiInput::default(), press, true)
            .unwrap();
        assert_eq!(session.player.locked, player::LockedAction::None);
        assert!(!session.player.vault_bit);

        // Releasing the freeze lets the same press latch the vault.
        session.game.message_flags |= game::MESSAGE_FLAG_PLAYER_STATE;
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        session
            .tick(&pack, UiInput::default(), press, true)
            .unwrap();
        assert_eq!(session.player.locked, player::LockedAction::Vault);
        assert!(session.player.vault_bit);
    }

    #[test]
    fn a_dismissed_message_swallows_the_held_action_key() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // An action-gated message zone the player stands in and faces: any
        // action press the room tick sees re-arms message 0x41.
        session.player.pos = [0, 0, 0];
        session.player.angle = 0;
        session.game.sync_entity_from_player(&session.player);
        session.game.room_actions[2] = Some(game::RoomAction {
            slot: 2,
            kind: game::RoomActionKind::Message,
            zone: [0, 0, 1000, 1000],
            sce: 2,
            handler: 2,
            flags: 0x81,
            item_data: None,
            room_items_flag: 0xFF,
            reach_animation: false,
            params: [2, 0x81, 0x41, 0, 0, 0, 0, 0],
        });

        // A message that does not pause gameplay: the action key dismisses it.
        // Under the script-first order the dismissing tick's room probe runs
        // while the window is still up, so the zone's own request is refused;
        // the still-held key must not re-arm it once the window closes.
        session.game.show_message(0x41, 0);
        for _ in 0..600 {
            if session.game.message.phase() == crate::message::MessagePhase::WaitInput {
                break;
            }
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        assert_eq!(
            session.game.message.phase(),
            crate::message::MessagePhase::WaitInput
        );

        session
            .tick(
                &pack,
                UiInput::default(),
                player::Input {
                    action_pressed: true,
                    action_held: true,
                    ..player::Input::default()
                },
                true,
            )
            .unwrap();
        assert!(!session.game.message.active);
        assert_eq!(
            session.game.message.id,
            Some(0x41),
            "the zone did not re-arm the window on the dismissal tick"
        );

        // The still-held key stays swallowed until it is released.
        session
            .tick(
                &pack,
                UiInput::default(),
                player::Input {
                    action_held: true,
                    ..player::Input::default()
                },
                true,
            )
            .unwrap();
        assert!(
            !session.game.message.active,
            "a still-held action key re-triggered the zone"
        );

        // Releasing and pressing again is a fresh edge and fires as usual.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        session
            .tick(
                &pack,
                UiInput::default(),
                player::Input {
                    action_pressed: true,
                    action_held: true,
                    ..player::Input::default()
                },
                true,
            )
            .unwrap();
        assert!(
            session.game.message.active,
            "a fresh press after release must reach the zone"
        );
        assert_eq!(session.game.message.id, Some(0x41));
    }

    #[test]
    fn a_dismissed_message_blanks_a_held_direction_until_release() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // Pause word 1 masks message_flags bit 0, so the dismissal is not
        // protected and must blank the held direction.
        session.game.show_message(0x41, 1);
        let held_up = player::Input {
            up: true,
            ..player::Input::default()
        };
        for _ in 0..600 {
            if session.game.message.phase() == crate::message::MessagePhase::WaitInput {
                break;
            }
            session
                .tick(&pack, UiInput::default(), held_up, false)
                .unwrap();
        }
        assert_eq!(
            session.game.message.phase(),
            crate::message::MessagePhase::WaitInput
        );

        // The dismissal happens after the script pass, so the dismissing
        // tick's own direction reaches the room; the dismissal records the
        // blank, which applies from the next tick.
        session
            .tick(
                &pack,
                UiInput::default(),
                player::Input {
                    up: true,
                    action_pressed: true,
                    action_held: true,
                    ..player::Input::default()
                },
                true,
            )
            .unwrap();
        assert!(!session.game.message.active);
        assert_eq!(
            session.game.dpad_blanked & 1,
            1,
            "the dismissal recorded the blank direction"
        );
        assert_eq!(
            session.game.dpad_held & 1,
            1,
            "the dismissing tick's direction still reached the room"
        );

        // Still held: the blank now suppresses the direction.
        session
            .tick(&pack, UiInput::default(), held_up, false)
            .unwrap();
        assert_eq!(session.game.dpad_held & 1, 0);

        // Release, then press again: control resumes.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        session
            .tick(&pack, UiInput::default(), held_up, false)
            .unwrap();
        assert_eq!(
            session.game.dpad_held & 1,
            1,
            "release restores the direction"
        );
    }

    #[test]
    fn a_message_that_protects_bit_zero_keeps_the_direction() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // Pause word 0xFE leaves bit 0 set, the dismissal's protection bit.
        session.game.show_message(0x41, 0xFE);
        for _ in 0..600 {
            if session.game.message.phase() == crate::message::MessagePhase::WaitInput {
                break;
            }
            session
                .tick(
                    &pack,
                    UiInput::default(),
                    player::Input {
                        up: true,
                        ..player::Input::default()
                    },
                    false,
                )
                .unwrap();
        }
        session
            .tick(
                &pack,
                UiInput::default(),
                player::Input {
                    up: true,
                    action_pressed: true,
                    action_held: true,
                    ..player::Input::default()
                },
                true,
            )
            .unwrap();
        assert!(!session.game.message.active);
        // The dismissing tick also swallows the action press, blanking the
        // whole pad for that frame; the protection shows on the next tick,
        // when the still-held direction is published again.
        session
            .tick(
                &pack,
                UiInput::default(),
                player::Input {
                    up: true,
                    ..player::Input::default()
                },
                false,
            )
            .unwrap();
        assert_eq!(
            session.game.dpad_held & 1,
            1,
            "bit 0 protects the held direction"
        );
    }

    #[test]
    fn every_room_tick_clears_the_item_use_flag_bank_first() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // The per-frame bank starts cleared: a script-set bit from last frame
        // must not leak into this frame's USE checks.
        session.game.set_item_use_flag(ITEM_SWORD_KEY, true);
        assert!(session.game.item_use_flag(ITEM_SWORD_KEY));
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert!(
            !session.game.item_use_flag(ITEM_SWORD_KEY),
            "the room tick must clear bank 9 before the main script runs"
        );
    }

    #[test]
    fn start_opens_the_menu_freezes_the_room_and_cancel_resumes() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        let before = session.game.frame;
        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.menu.is_some(), "START opens the menu");
        assert!(session.game.message_menu, "menu messages use the menu line");
        assert_eq!(session.game.frame, before, "the menu froze the room");
        settle_menu_fade(&mut session, &pack);

        for _ in 0..5 {
            session
                .tick(
                    &pack,
                    UiInput::default(),
                    player::Input {
                        up: true,
                        ..player::Input::default()
                    },
                    false,
                )
                .unwrap();
        }
        assert_eq!(session.game.frame, before, "the room stays frozen");
        assert!(session.menu.is_some());

        session
            .tick(
                &pack,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_none(), "X closes the menu");
        assert!(!session.game.message_menu);

        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.game.frame, before + 1, "the room resumes");
    }

    #[test]
    fn start_is_ignored_while_the_player_is_being_attacked() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // The reference's menu-button detection only opens when
        // `g_playerEntity.isBeingAttackedFlag == 0` (GameLoop.cpp 0x00480de8),
        // so a grabbed or bitten player cannot reach the inventory. The high
        // bit is the scripted door/climb pose and blocks it too.
        for flag in [1, 0x80] {
            session.game.entities[0].is_being_attacked = flag;
            session
                .tick(
                    &pack,
                    UiInput {
                        start: true,
                        ..UiInput::default()
                    },
                    player::Input::default(),
                    false,
                )
                .unwrap();
            assert!(
                session.menu.is_none(),
                "START opened the menu with is_being_attacked={flag:#04x}"
            );
            assert_eq!(
                session.menu_fade_phase,
                MenuFadePhase::None,
                "the menu fade started with is_being_attacked={flag:#04x}"
            );
        }

        // Once the attack ends a fresh START press opens it, so the test pins
        // the flag rather than a broken input path.
        session.game.entities[0].is_being_attacked = 0;
        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.menu.is_some(), "START opens the menu once clear");
    }

    #[test]
    fn the_pause_menu_fades_out_then_in_and_closes_through_the_room_fade() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        // The fade starts from the room-entry fade's negative leftover, so the
        // arming frame is inactive; the first step resets it to 0 and darkens
        // the frozen frame by 24 alpha a tick.
        assert_eq!(session.menu_fade_alpha(), None);
        let mut out = Vec::new();
        while session.menu_fade_phase == MenuFadePhase::Out {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            if session.menu_fade_phase == MenuFadePhase::Out
                && let Some(alpha) = session.menu_fade_alpha()
            {
                out.push(alpha);
            }
        }
        assert_eq!(out, vec![24, 48, 72, 96, 120, 144, 168, 192, 216, 240]);

        // The menu fades in from full black over 0xE800 (48 a tick); the step
        // is applied on the frame it is armed, so the first traced value is
        // already one step in.
        let mut fade_in = Vec::new();
        while session.menu_fade_phase == MenuFadePhase::In {
            if let Some(alpha) = session.menu_fade_alpha() {
                fade_in.push(alpha);
            }
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        assert_eq!(fade_in, vec![207, 159, 111, 63, 15]);
        assert_eq!(session.menu_fade_phase, MenuFadePhase::None);
        assert!(session.menu.is_some(), "the menu accepts input afterwards");
        // The original's state-0 -> state-1 transition frame takes no input.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();

        // Cancel darkens the menu, then unfreezes the room through the
        // room-entry fade.
        session
            .tick(
                &pack,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert_eq!(session.menu_fade_phase, MenuFadePhase::Closing);
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_none());
        assert_eq!(
            session.room_fade_draw_alpha(),
            Some(255),
            "the room-entry arm frame draws the full black veil"
        );
        assert_eq!(
            session.room_fade_alpha(),
            Some(207),
            "the room fades back in after the menu closes"
        );
    }

    #[test]
    fn the_pause_menu_cancel_plays_the_exit_cue_once() {
        // The reference's state-1 exit plays exactly one `play_sfx(3, 5)` on
        // the cancel frame (`MainMenu.cpp` 0x00463a6b-0x00463a9a: the cancel
        // bit skips `menu_handle_input`, whose navigation path has no cue of
        // its own, and the state falls through to `menu_exit_cleanup();
        // play_sfx(3, 5);` once). The port's menu-cue layer and the close both
        // sounded, so the cancel played twice.
        let _sdl = crate::audio::test_lock::sdl();
        let Some(mixer) = dummy_mixer() else {
            eprintln!("skipping cancel-cue test: no audio device");
            return;
        };
        let dir = TempDir::new();
        let pack_path = menu_cue_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        session.music = Some(mixer);

        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_some());
        assert_eq!(session.music.as_ref().unwrap().active_sfx(), 0);

        session
            .tick(
                &pack,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert_eq!(session.menu_fade_phase, MenuFadePhase::Closing);
        assert_eq!(
            session.music.as_ref().unwrap().active_sfx(),
            1,
            "the cancel close played the exit cue more than once"
        );
    }

    #[test]
    fn the_exit_tab_closes_silently_after_its_decide_cue() {
        // The reference's exit tab (`menu_tab_exit`) runs `menu_exit_cleanup`
        // directly: the confirm that reached it played `play_sfx(3, 6)`
        // (`MainMenu.cpp` menu_handle_input's confirm path) and no
        // `play_sfx(3, 5)` follows. The port used to close through the
        // state-1 exit and add the cancel cue on top of the decide.
        let _sdl = crate::audio::test_lock::sdl();
        let Some(mixer) = dummy_mixer() else {
            eprintln!("skipping exit-tab cue test: no audio device");
            return;
        };
        let dir = TempDir::new();
        let pack_path = menu_cue_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        session.music = Some(mixer);

        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        settle_menu_fade(&mut session, &pack);
        session.menu.as_mut().unwrap().cursor = 6;

        session
            .tick(
                &pack,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert_eq!(session.menu_fade_phase, MenuFadePhase::Closing);
        assert_eq!(
            session.music.as_ref().unwrap().active_sfx(),
            1,
            "the exit tab's confirm played an extra cue"
        );
    }

    #[test]
    fn a_film_freezes_the_room_and_resumes_when_it_ends() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("game.akpak");
        let id = RoomId::parse("100").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer
            .add("movie/oj.avi", crate::movie::test_avi(30))
            .unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        session.start_movie(&pack, 0, 0).unwrap();
        assert!(session.movie.is_some());
        let before = session.game.frame;
        // 30 frames at 10 fps take 90 fixed ticks, and none of them may reach
        // the room's scripts, entities, effects or player.
        let mut ticks = 0;
        while session.movie.is_some() {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            ticks += 1;
            assert_eq!(session.game.frame, before, "the film froze the room");
            assert!(ticks < 200, "the film never ended");
        }
        assert_eq!(ticks, 90, "30 frames at 10 fps take 90 ticks");

        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(
            session.game.frame,
            before + 1,
            "the room resumed after the film"
        );
    }

    /// Open a dummy-driver mixer, or `None` when no audio device exists.
    fn dummy_mixer() -> Option<Mixer> {
        let _ = unsafe {
            sdl3_sys::hints::SDL_SetHint(sdl3_sys::hints::SDL_HINT_AUDIO_DRIVER, c"dummy".as_ptr())
        };
        let mixer = Mixer::open();
        let _ = unsafe { sdl3_sys::hints::SDL_ResetHint(sdl3_sys::hints::SDL_HINT_AUDIO_DRIVER) };
        mixer
    }

    /// Advance the pause-menu open/close fade to completion.
    fn settle_menu_fade(session: &mut GameSession, pack: &Pack) {
        for _ in 0..64 {
            if session.menu_fade_phase == MenuFadePhase::None && !session.menu_open_delay {
                return;
            }
            session
                .tick(pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        panic!("the pause-menu fade did not settle");
    }

    /// Advance the pause menu until its action submenu's 8-frame open
    /// animation has finished and it accepts input again.
    fn settle_submenu_open(session: &mut GameSession, pack: &Pack) {
        for _ in 0..32 {
            if session
                .menu
                .as_ref()
                .is_none_or(|menu| menu.submenu_ready())
            {
                return;
            }
            session
                .tick(pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        panic!("the action submenu did not finish opening");
    }

    /// Run a confirmed action's deferred frame and the submenu's close
    /// animation until the box is gone (or the whole menu starts closing).
    fn settle_submenu_close(session: &mut GameSession, pack: &Pack) {
        for _ in 0..64 {
            let closing = session
                .menu
                .as_ref()
                .is_some_and(|menu| menu.submenu_closing);
            if !closing {
                return;
            }
            session
                .tick(pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        panic!("the action submenu did not finish closing");
    }

    #[test]
    fn a_movie_on_request_starts_the_film_and_resumes_the_sounds() {
        let _sdl = crate::audio::test_lock::sdl();
        let Some(mixer) = dummy_mixer() else {
            eprintln!("skipping film hand-off test: no audio device");
            return;
        };
        let dir = TempDir::new();
        let pack_path = dir.0.join("game.akpak");
        let id = RoomId::parse("1000").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        // The init script's bytes: `movie_on 3` then `end`.
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x29, 0x03, 0x00, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer
            .add("movie/dm3.avi", crate::movie::test_avi(30))
            .unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();
        let mut session = GameSession::from_room(&pack, id, Path::new("saves")).unwrap();
        session.music = Some(mixer);
        // A recognizable BGM state the film must not disturb.
        session.game.bgm = game::BgmState {
            state: 0x24,
            target: 0x24,
            ..game::BgmState::default()
        };
        let bgm = session.game.bgm;

        assert_eq!(
            session.game.fmv.request,
            Some(3),
            "the init queued the film"
        );
        assert!(session.game.fmv_requested());
        assert_eq!(session.game.fmv.taken, 0);

        // The first tick runs the room, then the hand-off takes the request
        // and pauses the game sounds.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.movie.as_ref().map(MovieSession::id), Some(3));
        assert!(
            session.music.as_ref().unwrap().game_sounds_paused(),
            "the film pauses the game sounds"
        );
        assert_eq!(session.game.fmv.request, None);
        assert!(!session.game.fmv_requested());
        assert_eq!(session.game.fmv.taken, 1);
        let before = session.game.frame;

        // The film owns every tick until it finishes.
        let mut ticks = 0;
        while session.movie.is_some() {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            ticks += 1;
            assert_eq!(session.game.frame, before, "the film froze the room");
            assert!(ticks < 200, "the film never ended");
        }
        assert!(
            !session.music.as_ref().unwrap().game_sounds_paused(),
            "the sounds resume when the film ends"
        );
        assert_eq!(session.game.bgm, bgm, "the BGM state survives the film");

        // The room continues on the next tick.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.game.frame, before + 1);
        assert!(session.movie.is_none(), "the film is not restarted");
    }

    #[test]
    fn a_film_request_without_a_film_drains_and_never_stalls() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("game.akpak");
        let id = RoomId::parse("1000").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x29, 0x03, 0x00, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();
        let mut session = GameSession::from_room(&pack, id, Path::new("saves")).unwrap();

        for _ in 0..3 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        assert!(session.movie.is_none(), "the missing film never starts");
        assert_eq!(session.game.fmv.taken, 1, "the request was drained");
        assert_eq!(session.game.frame, 3, "the room kept ticking");
    }

    /// A pack carrying a room and one synthetic film per `(id, name)` entry.
    fn film_pack(dir: &TempDir, films: &[(u8, &str)]) -> Pack {
        let pack_path = dir.0.join("films.akpak");
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        for player_flag in 0..=1u8 {
            let id = RoomId {
                stage: 1,
                room: NEW_GAME_ROOM,
                player_flag,
            };
            writer
                .add(&id.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
                .unwrap();
        }
        writer
            .add(
                &RoomId {
                    stage: 1,
                    room: NEW_GAME_ROOM,
                    player_flag: 0,
                }
                .cut_entry(0),
                bmp_bytes,
            )
            .unwrap();
        for &(film_id, name) in films {
            let _ = film_id;
            writer
                .add(&crate::movie::pack_path(name), crate::movie::test_avi(30))
                .unwrap();
        }
        writer.write(&pack_path).unwrap();
        Pack::open(&pack_path).unwrap()
    }

    #[test]
    fn movie_buttons_map_every_reachable_accept_button() {
        assert_eq!(
            movie_buttons(UiInput::default(), player::Input::default()),
            0
        );
        let cases = [
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
            UiInput {
                cancel: true,
                ..UiInput::default()
            },
            UiInput {
                start: true,
                ..UiInput::default()
            },
            UiInput {
                up: true,
                ..UiInput::default()
            },
            UiInput {
                down: true,
                ..UiInput::default()
            },
            UiInput {
                left: true,
                ..UiInput::default()
            },
            UiInput {
                right: true,
                ..UiInput::default()
            },
            UiInput {
                page_left: true,
                ..UiInput::default()
            },
            UiInput {
                page_right: true,
                ..UiInput::default()
            },
        ];
        let mut seen = 0u16;
        for ui in cases {
            let word = movie_buttons(ui, player::Input::default());
            assert_ne!(word, 0);
            assert_eq!(word & !0x0FFF, 0, "a bit outside the accept mask");
            assert_eq!(seen & word, 0, "two buttons share bit 0x{word:04X}");
            seen |= word;
        }
        assert_eq!(seen, 0x0CFB, "the full reachable accept set");
        assert_eq!(
            movie_buttons(
                UiInput::default(),
                player::Input {
                    action_held: true,
                    ..player::Input::default()
                }
            ),
            0x0001
        );
        assert_eq!(
            movie_buttons(
                UiInput::default(),
                player::Input {
                    run: true,
                    ..player::Input::default()
                }
            ),
            0x0002,
            "the run key shares the cancel/run bit"
        );
    }

    #[test]
    fn the_held_movie_word_keeps_an_unconsumed_press() {
        // A tap that no fixed tick has consumed yet must still reach the
        // between-tick film poll; the original feeds the held pad word and
        // edge-detects inside the film machine.
        let input = InputState {
            held: KEY_CONFIRM,
            pressed: KEY_CANCEL | KEY_UP,
            ..InputState::default()
        };
        assert_eq!(
            movie_buttons_held(input.held_word()),
            0x0001 | 0x0002 | 0x0010
        );
    }

    #[test]
    fn a_gamepad_axis_latches_with_hysteresis_and_flips_directly() {
        use sdl3_sys::gamepad::SDL_GAMEPAD_AXIS_LEFTX;
        let axis = SDL_GAMEPAD_AXIS_LEFTX.0;
        let mut input = InputState::default();
        input.axis_motion(7, axis, AXIS_PRESS - 1);
        assert!(!input.tick().player.right, "below the press threshold");
        input.axis_motion(7, axis, AXIS_PRESS);
        let tick = input.tick();
        assert!(tick.ui.right && tick.player.right);
        // Held down to the release threshold without a second edge.
        input.axis_motion(7, axis, AXIS_RELEASE + 1);
        let tick = input.tick();
        assert!(tick.player.right && !tick.ui.right);
        input.axis_motion(7, axis, AXIS_RELEASE);
        assert!(!input.tick().player.right, "released");
        // A flick straight across releases one side and presses the other.
        input.axis_motion(7, axis, i16::MAX);
        input.tick();
        input.axis_motion(7, axis, i16::MIN);
        let tick = input.tick();
        assert!(tick.ui.left && tick.player.left && !tick.player.right);
    }

    #[test]
    fn removing_a_gamepad_releases_only_its_inputs() {
        use sdl3_sys::gamepad::{SDL_GAMEPAD_AXIS_LEFTY, SDL_GAMEPAD_BUTTON_WEST};
        let mut input = InputState::default();
        input.source_down(
            7,
            controls::Source::PadButton(SDL_GAMEPAD_BUTTON_WEST.0),
            false,
        );
        input.axis_motion(7, SDL_GAMEPAD_AXIS_LEFTY.0, i16::MAX);
        input.key_down(SDL_SCANCODE_C, false);
        let tick = input.tick();
        assert!(tick.player.run && tick.player.down && tick.player.aim);
        input.device_removed(7);
        let tick = input.tick();
        assert!(!tick.player.run && !tick.player.down, "pad inputs released");
        assert!(tick.player.aim, "the keyboard's held key stays down");
        // The pad's axis state went with it: a fresh push presses again.
        input.axis_motion(7, SDL_GAMEPAD_AXIS_LEFTY.0, i16::MAX);
        assert!(input.tick().ui.down);
    }

    #[test]
    fn a_wheel_notch_is_one_press_with_no_hold() {
        let mut bindings = controls::Bindings::default();
        let wheel = controls::Source::Wheel(controls::Wheel::Down);
        assert!(bindings.bind(controls::Action::Fire, usize::MAX, wheel));
        let mut input = InputState::new(bindings);
        input.source_tap(wheel);
        let tick = input.tick();
        assert!(tick.player.fire_pressed && tick.player.fire);
        assert!(!input.tick().player.fire, "nothing stays held");
    }

    #[test]
    fn mouse_buttons_and_rebinding_while_held_never_stick() {
        let mut bindings = controls::Bindings::default();
        let right = controls::Source::Mouse(3);
        assert!(bindings.bind(controls::Action::Aim, usize::MAX, right));
        let mut input = InputState::new(bindings.clone());
        input.source_down(0, right, false);
        let tick = input.tick();
        assert!(tick.player.aim_pressed && tick.player.aim);
        input.source_up(0, right);
        assert!(!input.tick().player.aim);

        // Z held for fire; fire moves to C while Z is down: fire releases at
        // once, no stray edge appears, and Z's key-up leaves nothing behind.
        input.key_down(SDL_SCANCODE_Z, false);
        assert!(input.tick().player.fire);
        bindings.set(
            controls::Action::Fire,
            vec![controls::Source::key(SDL_SCANCODE_C)],
        );
        input.set_bindings(bindings.clone());
        let tick = input.tick();
        assert!(!tick.player.fire && !tick.player.fire_pressed);
        // C held across a change that binds it to fire drives fire at once,
        // without a fresh press.
        input.key_up(SDL_SCANCODE_Z);
        input.key_down(SDL_SCANCODE_X, false);
        input.tick();
        let mut moved = bindings.clone();
        moved.set(
            controls::Action::Fire,
            vec![controls::Source::key(SDL_SCANCODE_X)],
        );
        input.set_bindings(moved);
        let tick = input.tick();
        assert!(tick.player.fire && !tick.player.fire_pressed);
        input.key_up(SDL_SCANCODE_X);
        let tick = input.tick();
        assert!(!tick.player.fire && !tick.ui.cancel);
    }

    #[test]
    fn a_tick_captures_the_first_bindable_press_only() {
        use sdl3_sys::gamepad::{SDL_GAMEPAD_BUTTON_BACK, SDL_GAMEPAD_BUTTON_SOUTH};
        let mut input = InputState::default();
        input.key_down(SDL_SCANCODE_F1, false);
        input.source_down(0, controls::Source::Mouse(2), false);
        input.key_down(SDL_SCANCODE_Z, false);
        let tick = input.tick();
        assert_eq!(tick.ui.captured, Some(controls::Source::Mouse(2)));
        assert!(tick.ui.debug_menu, "F1 is never captured but still opens");
        // A repeat is not a press, so it captures nothing.
        input.key_down(SDL_SCANCODE_Z, true);
        assert_eq!(input.tick().ui.captured, None);
        // The pad's back button is the fixed debug-menu input; south is
        // captured and confirms.
        let back = controls::Source::PadButton(SDL_GAMEPAD_BUTTON_BACK.0);
        input.source_down(7, back, false);
        let tick = input.tick();
        assert!(tick.ui.debug_menu && tick.ui.captured.is_none());
        let south = controls::Source::PadButton(SDL_GAMEPAD_BUTTON_SOUTH.0);
        input.source_down(7, south, false);
        let tick = input.tick();
        assert!(tick.ui.confirm);
        assert_eq!(tick.ui.captured, Some(south));
    }

    #[test]
    fn stats_presentation_frames_follow_the_pacer_schedule() {
        assert_eq!(Presentation::alphas(30, 7), vec![None]);
        assert_eq!(Presentation::alphas(20, 7), vec![None]);
        assert_eq!(
            Presentation::alphas(120, 3),
            vec![Some(0.0), Some(0.25), Some(0.5), Some(0.75)]
        );
        assert_eq!(Presentation::alphas(60, 0), vec![Some(0.0), Some(0.5)]);
        // A rate that is not a multiple of the tick still presents its frames
        // on time: 75 fps is 2 or 3 frames a tick, 75 a second.
        let frames: Vec<Vec<Option<f32>>> =
            (0..30).map(|tick| Presentation::alphas(75, tick)).collect();
        assert_eq!(frames.iter().map(Vec::len).sum::<usize>(), 75);
        let close = |a: Option<f32>, b: f32| (a.unwrap() - b).abs() < 1e-5;
        assert!(close(frames[1][0], 0.2) && close(frames[1][1], 0.6));
    }

    #[test]
    fn the_session_step_switches_to_the_door_interval() {
        assert_eq!(session_tick_interval(false, false), TICK_MS);
        assert_eq!(session_tick_interval(true, false), UI_TICK_MS);
        assert_eq!(session_tick_interval(false, true), UI_TICK_MS);
        // The original's door pass runs on the 16 ms non-gameplay limiter, so
        // a 300-pass door file takes about 4.8 s rather than the 10 s the
        // 30 Hz gameplay step would give.
        assert!((UI_TICK_MS * 300.0 - 4800.0).abs() < 0.1);
    }

    #[test]
    fn a_shoulder_button_skips_a_film() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("film.akpak");
        let mut writer = PackWriter::new();
        writer
            .add("movie/oj.avi", crate::movie::test_avi(400))
            .unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();
        let mut session = MovieSession::open(&pack, 0, 0).unwrap();
        assert_eq!(session.tick(0, None), MovieTick::Waiting);
        for _ in 0..150 {
            session.tick(0, None);
        }
        let ui = UiInput {
            page_left: true,
            ..UiInput::default()
        };
        assert_eq!(
            session.tick(movie_buttons(ui, player::Input::default()), None),
            MovieTick::Skipped,
            "L1 must be able to skip a skippable film"
        );
    }

    #[test]
    fn a_film_skip_tap_between_ticks_is_polled_and_ends_the_film() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("film.akpak");
        let mut writer = PackWriter::new();
        writer
            .add("movie/oj.avi", crate::movie::test_avi(400))
            .unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();
        let session = MovieSession::open(&pack, 0, 0).unwrap();
        let mut app = App::new(pack, dir.0.join("saves"), false);
        app.queue_movie(session, None);
        let l1 = movie_buttons(
            UiInput {
                page_left: true,
                ..UiInput::default()
            },
            player::Input::default(),
        );
        // The loop's order: every pass polls the skip, then runs its ticks.
        let tick = |app: &mut App, held| {
            app.poll_movie_skip(held);
            app.update(UiInput::default(), player::Input::default(), false)
                .unwrap()
        };
        for _ in 0..150 {
            assert!(matches!(tick(&mut app, 0), AppFlow::Continue));
        }
        // A tap pressed and released on passes that ran no tick (frames
        // presented faster than ticks, or waits between them) is still seen,
        // and the next tick ends the film.
        app.poll_movie_skip(l1);
        app.poll_movie_skip(0);
        assert!(matches!(tick(&mut app, 0), AppFlow::Quit));
    }

    /// Run the app until `predicate` holds, returning the film ids seen in
    /// order. Panics after 4000 ticks.
    fn run_app_until(app: &mut App, mut predicate: impl FnMut(&App) -> bool) -> Vec<u8> {
        let mut seen: Vec<u8> = Vec::new();
        for _ in 0..4000 {
            app.update(UiInput::default(), player::Input::default(), false)
                .unwrap();
            if let Mode::Movie(session) = &app.mode
                && seen.last() != Some(&session.id())
            {
                seen.push(session.id());
            }
            if predicate(app) {
                return seen;
            }
        }
        panic!("the app never reached the expected mode");
    }

    #[test]
    fn the_boot_queue_plays_the_logos_then_the_title_opening_once() {
        let dir = TempDir::new();
        let pack = film_pack(&dir, &[(28, "vlogo"), (23, "capcom"), (0, "oj")]);
        let mut app = App::new(pack, dir.0.clone(), false);
        app.boot(AppBoot::Boot).unwrap();

        // The logos play in order, then the title opens and queues its film.
        let seen = run_app_until(&mut app, |app| matches!(app.mode, Mode::Title(_)));
        assert_eq!(seen, vec![28, 23], "the logos play in boot order");
        let pending = app.pending_movie.as_ref().expect("the title opening film");
        assert_eq!(pending.session.id(), 0);

        // The opening film finishes and reopens the title without requeueing.
        let seen = run_app_until(&mut app, |app| {
            matches!(app.mode, Mode::Title(_)) && app.pending_movie.is_none()
        });
        assert_eq!(seen, vec![0], "the opening film played once");
        assert!(app.pending_movie.is_none());

        // A later title entry never replays it.
        app.open_title().unwrap();
        assert!(app.pending_movie.is_none(), "id 0 is once per process");
        assert!(app.opening_played);
    }

    #[test]
    fn the_boot_queue_skips_the_absent_virgin_logo() {
        let dir = TempDir::new();
        // The shipped JPN install has no `vlogo.avi`.
        let pack = film_pack(&dir, &[(23, "capcom"), (0, "oj")]);
        let mut app = App::new(pack, dir.0.clone(), false);
        app.boot(AppBoot::Boot).unwrap();
        let seen = run_app_until(&mut app, |app| matches!(app.mode, Mode::Title(_)));
        assert_eq!(seen, vec![23], "the missing logo is skipped");
        assert_eq!(
            app.pending_movie
                .as_ref()
                .map(|pending| pending.session.id()),
            Some(0)
        );
    }

    #[test]
    fn a_title_capture_queues_no_films() {
        let dir = TempDir::new();
        let pack = film_pack(&dir, &[(0, "oj"), (1, "pj")]);
        let mut app = App::new(pack, dir.0.clone(), false);
        // Captures and `--ui` boots leave the automatic films off.
        app.boot(AppBoot::Title).unwrap();
        assert!(app.pending_movie.is_none());
        assert!(matches!(app.mode, Mode::Title(_)));
        assert!(!app.films);
    }

    #[test]
    fn the_character_confirm_queues_the_intro_with_the_character() {
        let dir = TempDir::new();
        let pack = film_pack(&dir, &[(1, "pj")]);
        let mut app = App::new(pack, dir.0.clone(), false);
        app.films = true;
        app.apply(ScreenAction::NewGame { character: 1 }).unwrap();
        let pending = app.pending_movie.as_ref().expect("the intro film");
        assert_eq!(pending.session.id(), 1);
        assert_eq!(pending.session.character(), 1, "the chosen character");
        assert_eq!(pending.action, Some(ScreenAction::NewGame { character: 1 }));
        assert!(matches!(app.mode, Mode::Boot), "the session waits");

        // The intro finishes and the NewGame action builds the session.
        run_app_until(&mut app, |app| matches!(app.mode, Mode::Play(_)));
        let Mode::Play(session) = &app.mode else {
            unreachable!();
        };
        assert_eq!(session.game.id.player_flag, 1, "Jill's session");
        assert_eq!(
            session.boot_message,
            Some(game::MESSAGE_BOOT_NEW_GAME),
            "the session starts on the new-game loading narration"
        );
        assert_eq!(
            session.room_fade_alpha(),
            None,
            "the room-entry fade waits for the narration to clear"
        );
        assert!(!app.prologue_pending);

        // A later confirm queues the prologue again, like the original.
        app.films = true;
        app.apply(ScreenAction::NewGame { character: 0 }).unwrap();
        let pending = app.pending_movie.as_ref().expect("a second intro film");
        assert_eq!(pending.session.id(), 1);
        assert_eq!(pending.session.character(), 0);
    }

    #[test]
    fn an_ending_chain_plays_every_film_and_skips_missing_ones() {
        let dir = TempDir::new();
        let pack = film_pack(
            &dir,
            &[(14, "dme"), (15, "ed1"), (27, "staf_b"), (22, "ed8")],
        );
        let mut app = App::new(pack, dir.0.clone(), false);
        // The null id 10 sits in the middle: it is skipped, not fatal.
        app.play_film_chain(vec![(14, 0), (10, 0), (15, 0), (27, 0), (22, 0)], None)
            .unwrap();
        let seen = run_app_until(&mut app, |app| !app.chain_active);
        assert_eq!(seen, vec![14, 15, 27, 22], "the chain order");
        assert!(!app.chain_active);
        // The last film is still presented; the next tick quits.
        assert_eq!(
            app.update(UiInput::default(), player::Input::default(), false)
                .unwrap(),
            AppFlow::Quit
        );
    }

    #[test]
    fn the_ending_sequence_flows_from_the_films_to_the_title() {
        let dir = TempDir::new();
        let pack = film_pack(
            &dir,
            &[(14, "dme"), (15, "ed1"), (27, "staf_b"), (22, "ed8")],
        );
        let mut app = App::new(pack, dir.0.clone(), false);
        app.ending = Some(EndingFlow {
            stats: ending::EndStats::debug(1, 0, 1),
            second_playthrough: false,
            has_special_key: false,
            has_infinite_launcher: false,
        });
        app.play_film_chain(
            vec![(14, 0), (15, 0), (27, 0), (22, 0)],
            Some(ScreenAction::EndingResult),
        )
        .unwrap();
        // The films drain into the RESULT screen.
        run_app_until(&mut app, |app| matches!(app.mode, Mode::Ending(_)));

        // Press through the RESULT screen; ending 1 has no epilogue, so the
        // sequence lands on the next-cycle save screen.
        let any = UiInput {
            any: true,
            ..UiInput::default()
        };
        let mut reached = false;
        for _ in 0..4000 {
            if matches!(app.mode, Mode::Save(_)) {
                reached = true;
                break;
            }
            app.update(any, player::Input::default(), false).unwrap();
        }
        assert!(reached, "the sequence reaches the next-cycle save screen");

        // Cancelling the save still continues to the title.
        app.update(
            UiInput {
                cancel: true,
                ..UiInput::default()
            },
            player::Input::default(),
            false,
        )
        .unwrap();
        run_app_until(&mut app, |app| matches!(app.mode, Mode::Title(_)));
        assert!(app.ending.is_none(), "the sequence clears its flow");
    }

    #[test]
    fn the_special_key_skips_the_epilogue_but_the_sixth_ending_runs_it() {
        let dir = TempDir::new();
        let pack = film_pack(&dir, &[]);
        let mut app = App::new(pack, dir.0.clone(), false);

        app.ending = Some(EndingFlow {
            stats: ending::EndStats::debug(6, 0, 1),
            second_playthrough: false,
            has_special_key: true,
            has_infinite_launcher: false,
        });
        app.ending_after_result().unwrap();
        assert!(
            matches!(app.mode, Mode::Save(_)),
            "the special key skips the epilogue"
        );

        app.ending = Some(EndingFlow {
            stats: ending::EndStats::debug(6, 0, 1),
            second_playthrough: false,
            has_special_key: false,
            has_infinite_launcher: false,
        });
        app.ending_after_result().unwrap();
        assert!(
            matches!(app.mode, Mode::Epilogue(_)),
            "ending 6 without the key runs the epilogue"
        );
        assert!(
            app.ending.is_some_and(|flow| flow.has_special_key),
            "the epilogue grants the special key for the next cycle"
        );

        // Ending 1 never runs the epilogue.
        app.ending = Some(EndingFlow {
            stats: ending::EndStats::debug(1, 0, 1),
            second_playthrough: false,
            has_special_key: false,
            has_infinite_launcher: false,
        });
        app.ending_after_result().unwrap();
        assert!(matches!(app.mode, Mode::Save(_)));
        assert!(
            app.ending.is_some_and(|flow| !flow.has_special_key),
            "an ending without the epilogue still has no key to carry"
        );
    }

    #[test]
    fn the_next_cycle_card_grants_the_unlocks_and_flags() {
        let dir = TempDir::new();
        let pack = film_pack(&dir, &[]);
        let flow = EndingFlow {
            stats: ending::EndStats::debug(6, 0, 1),
            second_playthrough: false,
            has_special_key: true,
            has_infinite_launcher: false,
        };
        let file = build_next_cycle_save(&pack, &flow).unwrap();
        assert_eq!(file.saves, 0, "the next-cycle counter restarts at zero");
        let has = |item: u8| file.player_slots.iter().any(|slot| slot.id == item);
        assert!(
            has(ending::ITEM_ROCKET_LAUNCHER),
            "a no-save run grants the rocket launcher"
        );
        assert!(
            has(ending::ITEM_SPECIAL_KEY),
            "the special key carries over"
        );
        assert!(has(items::ITEM_MINIMI), "Chris's sub-four-hour gun");

        // The flags round-trip through the scenario bank.
        let id = new_game_id(&pack, 0);
        let loaded = load_room(&pack, id).unwrap();
        let mut state = new_game_state(&pack, id, &loaded.room);
        file.apply_to(&mut state);
        assert!(state.flag_test(
            game::BANK_SCENARIO,
            weapons::SCENARIO_FLAG_INF_R_LAUNCHER,
            false
        ));
        assert!(state.flag_test(
            game::BANK_SCENARIO,
            game::SCENARIO_FLAG_SECOND_PLAYTHROUGH,
            false
        ));

        // The Jill card never carries the first-run marker: the marker is only
        // raised for a first run, and the card's 0x7B keeps it clear from here
        // on (`EndingScreen.cpp:1059-1126`, `GameStart.cpp:606-614`).
        let jill = EndingFlow {
            stats: ending::EndStats::debug(6, 1, 1),
            second_playthrough: false,
            has_special_key: true,
            has_infinite_launcher: false,
        };
        let file = build_next_cycle_save(&pack, &jill).unwrap();
        let mut state = new_game_state(&pack, id, &loaded.room);
        file.apply_to(&mut state);
        assert!(
            state.flag_test(
                game::BANK_SCENARIO2,
                game::SCENARIO2_FLAG_JILL_FIRST_RUN,
                true
            ),
            "the next-cycle card clears the Jill first-run marker"
        );
    }

    fn press(confirm: bool, cancel: bool) -> UiInput {
        UiInput {
            confirm,
            cancel,
            ..UiInput::default()
        }
    }

    /// Ticks the session with no input until `done` holds, up to `limit`.
    fn idle_until(
        session: &mut GameSession,
        pack: &Pack,
        limit: u32,
        done: impl Fn(&GameSession) -> bool,
    ) -> u32 {
        for tick in 0..limit {
            if done(session) {
                return tick;
            }
            session
                .tick(pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        panic!("the condition never held within {limit} ticks");
    }

    #[test]
    fn check_slides_into_the_examine_viewer_and_cancel_slides_back() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        session.game.add_item(ITEM_KNIFE, 0);

        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.menu.is_some());
        settle_menu_fade(&mut session, &pack);

        // Slot 0 holds the knife: confirm opens the action submenu, down
        // selects CHECK, confirm starts the slide into the viewer.
        session
            .tick(&pack, press(true, false), player::Input::default(), false)
            .unwrap();
        settle_submenu_open(&mut session, &pack);
        for ui in [
            UiInput {
                down: true,
                ..UiInput::default()
            },
            press(true, false),
        ] {
            session
                .tick(&pack, ui, player::Input::default(), false)
                .unwrap();
        }
        assert!(session.menu.as_ref().is_some_and(MainMenu::viewing));
        let frozen = session.game.frame;
        // The submenu slides out for eight frames before the viewer starts.
        let slide = idle_until(&mut session, &pack, 20, |session| session.examine.is_some());
        assert_eq!(slide, 8, "the viewer starts once the slide reaches 8");
        assert_eq!(
            session
                .examine
                .as_ref()
                .map(ui::item_view::ItemViewScreen::item),
            Some(ITEM_KNIFE)
        );
        idle_until(&mut session, &pack, 0x80, |session| {
            session
                .examine
                .as_ref()
                .is_some_and(ui::item_view::ItemViewScreen::turntable_ready)
        });
        assert_eq!(session.game.frame, frozen, "the room stays frozen");

        // Cancel runs the exit animation, then the submenu slides back in
        // and the menu owns the input again.
        session
            .tick(&pack, press(false, true), player::Input::default(), false)
            .unwrap();
        assert!(session.examine.is_some(), "the exit animation still draws");
        idle_until(&mut session, &pack, 0x80, |session| {
            session.examine.is_none()
        });
        idle_until(&mut session, &pack, 20, |session| {
            !session.menu.as_ref().is_some_and(MainMenu::viewing)
        });
        assert!(
            session.menu.is_some(),
            "leaving the viewer lands on the menu"
        );
        assert_eq!(session.game.frame, frozen, "the room stays frozen");
    }

    #[test]
    fn confirming_a_plain_key_in_the_viewer_marks_it_examined() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // Lab key A's name class is 7 with no angle combo: confirming on the
        // turntable shows the class message and marks the class examined,
        // which reveals the key's real name.
        assert!(!crate::message::examined_bit(
            &session.game.examined_flags(),
            7
        ));
        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        settle_menu_fade(&mut session, &pack);
        session.open_item_view(&pack, 0x37);
        idle_until(&mut session, &pack, 0x80, |session| {
            session
                .examine
                .as_ref()
                .is_some_and(ui::item_view::ItemViewScreen::turntable_ready)
        });
        assert!(!crate::message::examined_bit(
            &session.game.examined_flags(),
            7
        ));
        session
            .tick(&pack, press(true, false), player::Input::default(), false)
            .unwrap();
        assert!(
            crate::message::examined_bit(&session.game.examined_flags(), 7),
            "the confirm marks the item examined"
        );
        assert!(session.game.message.active, "the class message is up");
    }

    #[test]
    fn a_confirmed_message_pickup_reaches_the_session_state() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // A room item action armed behind a yes/no message; the zone sits far
        // from the spawn so the action key cannot fire it directly.
        session.game.room_actions[3] = Some(game::RoomAction {
            slot: 3,
            kind: game::RoomActionKind::Item,
            zone: [10000, 10000, 100, 100],
            sce: 4,
            handler: 4,
            flags: 0x81,
            item_data: None,
            room_items_flag: 0xFF,
            reach_animation: false,
            params: [ITEM_FIRST_AID_SPRAY, 1, 0, 0, 0, 0, 0, 0],
        });
        // Global id 0x7f (index 63) is the yes/no pickup stream.
        session.game.show_message_for_action(3, 0x7F, 0xFF);
        assert!(session.game.message.active);

        for _ in 0..600 {
            if session.game.message.phase() == crate::message::MessagePhase::YesNo {
                break;
            }
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        assert_eq!(
            session.game.message.phase(),
            crate::message::MessagePhase::YesNo
        );

        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        session
            .tick(
                &pack,
                UiInput::default(),
                player::Input {
                    action_pressed: true,
                    action_held: true,
                    ..player::Input::default()
                },
                true,
            )
            .unwrap();
        assert!(!session.game.message.active);
        assert!(
            session.game.has_item(ITEM_FIRST_AID_SPRAY),
            "the post-action pickup reached the session"
        );
        assert!(
            session.game.room_actions[3].is_none(),
            "the action is consumed"
        );
    }

    /// A room item action on the player's own position, fired by the action
    /// key (`0xC1`: action-key probe at the entity position).
    fn item_room_action(slot: u8, item: u8, quantity: u8, handler: u8) -> game::RoomAction {
        game::RoomAction {
            slot,
            kind: game::RoomActionKind::Item,
            zone: [0, 0, 10, 10],
            sce: handler,
            handler,
            flags: 0xC1,
            params: [item, quantity, 0, 0, 0, 0, 0, 0],
            item_data: None,
            room_items_flag: 0xFE,
            reach_animation: false,
        }
    }

    /// Place the player at the origin so the entity-position item probes
    /// match.
    fn place_player(session: &mut GameSession) {
        session.player.pos = [0, 0, 0];
        session.player.angle = 0;
        session.game.sync_entity_from_player(&session.player);
    }

    /// Run the session's first tick without input. That tick is the player's
    /// state-0 init, which reads no pad (the reference's `player_state_init`),
    /// so an action press only reaches the probe from the tick after it.
    fn settle_control(session: &mut GameSession, pack: &Pack) {
        session
            .tick(pack, UiInput::default(), player::Input::default(), true)
            .unwrap();
        assert_eq!(
            session.game.entities[0].state(),
            1,
            "the player has control"
        );
    }

    /// The action-key press edge and held level.
    fn pressed() -> player::Input {
        player::Input {
            action_pressed: true,
            action_held: true,
            ..player::Input::default()
        }
    }

    /// Tick `session` until `check` holds, panicking after `limit` ticks.
    fn tick_until(
        session: &mut GameSession,
        pack: &Pack,
        limit: usize,
        what: &str,
        mut check: impl FnMut(&GameSession) -> bool,
    ) {
        for _ in 0..limit {
            if check(session) {
                return;
            }
            session
                .tick(pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        panic!("{what} never happened");
    }

    #[test]
    fn a_ground_pickup_opens_the_viewer_and_a_confirmed_prompt_awards() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        place_player(&mut session);
        settle_control(&mut session, &pack);
        session.game.room_actions[1] = Some(item_room_action(1, ITEM_FIRST_AID_SPRAY, 1, 4));

        // The action key arms the flag; the engine opens the viewer over the
        // inventory panel at the end of the same tick.
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(session.pickup_view.is_some(), "the flag opened the viewer");
        assert!(session.menu.is_some(), "the inventory panel is underneath");
        assert_eq!(
            session
                .pickup_view
                .as_ref()
                .and_then(|pickup| pickup.screen.as_ref())
                .map(|screen| screen.item()),
            Some(ITEM_FIRST_AID_SPRAY),
            "the viewer loaded the picked item's model"
        );
        assert!(!session.game.has_item(ITEM_FIRST_AID_SPRAY));
        assert!(session.game.room_actions[1].is_some());
        assert!(session.game.menu_pending());

        // The model intro settles and requests global 0xC0.
        tick_until(&mut session, &pack, 300, "the take prompt", |session| {
            session.game.message.id == Some(game::MESSAGE_TAKE_PROMPT)
        });
        assert!(session.game.message.active);

        // The default choice is Yes; confirm it.
        tick_until(&mut session, &pack, 300, "the yes/no phase", |session| {
            session.game.message.phase() == crate::message::MessagePhase::YesNo
        });
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(
            session.game.has_item(ITEM_FIRST_AID_SPRAY),
            "the yes branch awarded the item"
        );
        assert!(
            session.game.room_actions[1].is_none(),
            "the take consumed the action"
        );

        // The exit animation plays out, then the menu darkens and closes and
        // the room resumes.
        tick_until(&mut session, &pack, 300, "the viewer close", |session| {
            session.pickup_view.is_none()
        });
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_none());
        assert!(!session.game.menu_pending());
        assert_eq!(session.game.item_view_request(), None);
    }

    /// The pause-menu / item-viewer exit restore: the opening scene's
    /// scripted kneel (state 8, behaviour 1 parked at its completion) must not
    /// survive the pick-up prompt's menu close. The original's mode-7 exit
    /// resets the animation words and pre-applies the body idle pose; without
    /// it the player stayed bent over after the item was awarded (the pick-up
    /// "no stand-up" look).
    #[test]
    fn the_menu_exit_restores_the_scripted_kneel_to_pad_control() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        place_player(&mut session);
        session.loaded.player_assets = Some(synthetic_player_assets());
        session.game.room_actions[1] = Some(item_room_action(1, ITEM_FIRST_AID_SPRAY, 1, 4));

        // The event script's kneel: state 8, behaviour 1, parked at the clip's
        // completion (action_state 5) with the scripted clip showing.
        {
            let entity = &mut session.game.entities[0];
            entity.set_state(8);
            entity.action_behavior = 1;
            entity.action_state = 5;
            entity.animation_id = 4;
            entity.attack_anim = 4;
        }
        session.player.clip_source = player::ClipSource::Room;
        session.player.anim.clip = 4;
        session.player.anim.frame = 0;
        session.player.anim.display_frame = 0;

        session.game.arm_item_pickup(1);
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert!(session.pickup_view.is_some(), "the viewer opened");
        tick_until(&mut session, &pack, 300, "the yes/no phase", |session| {
            session.game.message.phase() == crate::message::MessagePhase::YesNo
        });
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(session.game.has_item(ITEM_FIRST_AID_SPRAY));
        tick_until(&mut session, &pack, 300, "the viewer close", |session| {
            session.pickup_view.is_none()
        });
        settle_menu_fade(&mut session, &pack);

        // Pad control is back and the body idle pose has been pre-applied:
        // one `Joint_move` consumes frame 0 and arms frame 1's timing.
        let entity = session.game.entities[0];
        assert_eq!(entity.state(), 1, "the scripted state released");
        assert_eq!(entity.action_behavior, 0);
        assert_eq!(entity.action_state, 0);
        assert_eq!(entity.animation_id, 0, "attackAnim cleared");
        assert_eq!(entity.unk_8c, 0, "the clock snapped");
        assert_eq!(session.player.clip_source, player::ClipSource::Emd);
        assert_eq!(session.player.anim.clip, 0, "the body idle clip");
        assert_eq!(
            session.player.anim.frame, 1,
            "the exit restore ran the first body Joint_move"
        );
    }

    #[test]
    fn a_resolved_pickup_closes_through_the_menu_fade_and_arms_the_room_fade() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        place_player(&mut session);
        settle_control(&mut session, &pack);
        session.game.room_actions[1] = Some(item_room_action(1, ITEM_FIRST_AID_SPRAY, 1, 4));
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();

        tick_until(&mut session, &pack, 300, "the yes/no phase", |session| {
            session.game.message.phase() == crate::message::MessagePhase::YesNo
        });
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(session.game.has_item(ITEM_FIRST_AID_SPRAY));

        // The exit animation plays out; when it finishes the inventory panel
        // starts its closing fade instead of vanishing.
        tick_until(
            &mut session,
            &pack,
            300,
            "the menu closing fade",
            |session| session.menu_fade_phase == MenuFadePhase::Closing,
        );
        assert!(session.pickup_view.is_none());
        assert!(
            session.menu.is_some(),
            "the panel stays up while it darkens"
        );
        // The fade pair starts at the room-entry fade's negative leftover
        // (`set_fading` only sets the counter), so it is inactive here and the
        // first step resets it to 0 before darkening.
        assert_eq!(session.menu_fade_alpha(), None);

        // The panel darkens by 24 alpha a tick before the menu drops and the
        // room-entry fade is re-armed at full black.
        let mut alphas = Vec::new();
        while session.menu_fade_phase == MenuFadePhase::Closing {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            if session.menu_fade_phase == MenuFadePhase::Closing
                && let Some(alpha) = session.menu_fade_alpha()
            {
                alphas.push(alpha);
            }
        }
        assert_eq!(
            alphas,
            vec![24, 48, 72, 96, 120, 144, 168, 192, 216, 240, 0, 0]
        );
        assert!(
            session.menu.is_none(),
            "the menu closes only after the panel has darkened"
        );
        assert!(!session.game.menu_pending());
        assert_eq!(
            session.room_fade_alpha(),
            Some(207),
            "the room fades back in"
        );
    }

    #[test]
    fn the_viewer_waits_for_an_active_message_before_it_opens() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        place_player(&mut session);
        settle_control(&mut session, &pack);
        session.game.room_actions[1] = Some(item_room_action(1, ITEM_FIRST_AID_SPRAY, 1, 4));

        // A wait-for-input message that does not mask the control bit: the
        // action key still probes and arms the viewer.
        session.game.show_message(0x41, 0);
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(
            session.game.item_view_request().is_some(),
            "the probe armed the viewer"
        );
        assert!(
            session.pickup_view.is_none(),
            "the main loop waits for the message to clear"
        );

        // Dismissing the message lets the pending flag open the viewer.
        tick_until(&mut session, &pack, 300, "the message wait", |session| {
            session.game.message.phase() == crate::message::MessagePhase::WaitInput
        });
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(session.pickup_view.is_some());
        assert!(!session.game.message.active);
    }

    #[test]
    fn refusing_the_pickup_prompt_leaves_the_item_in_the_room() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        place_player(&mut session);
        settle_control(&mut session, &pack);
        session.game.room_actions[1] = Some(item_room_action(1, ITEM_FIRST_AID_SPRAY, 1, 4));
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();

        tick_until(&mut session, &pack, 300, "the yes/no phase", |session| {
            session.game.message.phase() == crate::message::MessagePhase::YesNo
        });
        // Right flips the cursor to No; confirm resolves the prompt without
        // an award.
        session
            .tick(
                &pack,
                UiInput::default(),
                player::Input {
                    right: true,
                    ..player::Input::default()
                },
                false,
            )
            .unwrap();
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(!session.game.has_item(ITEM_FIRST_AID_SPRAY));
        assert!(
            session.game.room_actions[1].is_some(),
            "the item stays in the room"
        );

        tick_until(&mut session, &pack, 300, "the viewer close", |session| {
            session.pickup_view.is_none()
        });
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_none());
        assert!(!session.game.menu_pending());
    }

    #[test]
    fn a_full_inventory_turns_the_pickup_into_the_no_room_message() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        place_player(&mut session);
        settle_control(&mut session, &pack);
        // Fill every slot with a distinct non-stackable item; the sword key
        // then has neither a free slot nor a stack to merge into.
        for item in [0x41, 0x42, 0x43, 0x44, 0x45, 0x46] {
            session.game.add_item(item, 1);
        }
        assert_eq!(
            session.game.inventory.len(),
            session.game.inventory_capacity()
        );
        session.game.room_actions[1] = Some(item_room_action(1, ITEM_SWORD_KEY, 1, 4));
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(session.pickup_view.is_some());

        tick_until(&mut session, &pack, 300, "the no-room message", |session| {
            session.game.message.id == Some(game::MESSAGE_INVENTORY_FULL)
        });
        assert!(session.game.message.active);
        tick_until(&mut session, &pack, 300, "the message wait", |session| {
            session.game.message.phase() == crate::message::MessagePhase::WaitInput
        });
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();

        tick_until(&mut session, &pack, 300, "the viewer close", |session| {
            session.pickup_view.is_none()
        });
        assert!(!session.game.has_item(ITEM_SWORD_KEY), "nothing was taken");
        assert!(
            session.game.room_actions[1].is_some(),
            "the item stays in the room"
        );
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_none());
        assert!(!session.game.menu_pending());
    }

    #[test]
    fn a_scripted_give_item_shows_global_c1_and_awards_on_completion() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        session.game.room_actions[1] = Some(item_room_action(1, ITEM_FIRST_AID_SPRAY, 1, 4));
        session.game.arm_got_item(1);

        // The next tick opens the viewer; the award waits for the prompt.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert!(session.pickup_view.is_some());
        assert!(session.menu.is_some());
        assert!(session.game.menu_pending());

        tick_until(
            &mut session,
            &pack,
            300,
            "the got-item message",
            |session| session.game.message.id == Some(game::MESSAGE_GOT_ITEM),
        );
        assert!(
            !session.game.has_item(ITEM_FIRST_AID_SPRAY),
            "the award waits for the message"
        );
        tick_until(&mut session, &pack, 300, "the message wait", |session| {
            session.game.message.phase() == crate::message::MessagePhase::WaitInput
        });
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(
            session.game.has_item(ITEM_FIRST_AID_SPRAY),
            "closing the read awarded the item"
        );
        assert!(session.game.room_actions[1].is_none());
        assert!(!session.game.menu_pending());

        tick_until(&mut session, &pack, 300, "the viewer close", |session| {
            session.pickup_view.is_none()
        });
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_none());
    }

    #[test]
    fn a_document_pickup_opens_the_reader_and_files_the_entry_on_close() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        place_player(&mut session);
        settle_control(&mut session, &pack);
        session.game.room_actions[1] = Some(item_room_action(1, 0x60, 1, 0x0D));

        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(session.pickup_view.is_some());
        assert!(session.menu.is_some());
        // The document reader owns the screen instead of the model viewer,
        // and the 0xC6 line waits for its exit slide.
        assert!(
            session
                .pickup_view
                .as_ref()
                .is_some_and(|pickup| pickup.reader.is_some()),
            "the state-8 flow opens the document reader"
        );
        assert_eq!(session.game.message.id, None);
        assert!(session.game.file_collected(1), "the entry is filed");
        assert!(!session.game.has_item(0x60), "a document is not carried");

        // The reader's entry slide settles, then cancel starts the exit slide.
        tick_until(
            &mut session,
            &pack,
            64,
            "the reader entry slide",
            |session| {
                session.pickup_view.as_ref().is_some_and(|pickup| {
                    pickup.reader.as_ref().is_some_and(|reader| {
                        reader.reader_state == crate::ui::file::ReaderState::Idle
                            && !reader.input_locked()
                    })
                })
            },
        );
        session
            .tick(
                &pack,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        tick_until(&mut session, &pack, 64, "the reader exit", |session| {
            session.game.message.id == Some(game::MESSAGE_FILE_FILED)
        });
        assert!(session.game.message.active);
        assert!(
            session
                .pickup_view
                .as_ref()
                .is_some_and(|pickup| pickup.reader.is_none()),
            "the reader layer drops with its exit slide"
        );
        assert!(
            session.game.room_actions[1].is_some(),
            "the entry is consumed only once the message clears"
        );

        tick_until(&mut session, &pack, 300, "the message wait", |session| {
            session.game.message.phase() == crate::message::MessagePhase::WaitInput
        });
        session
            .tick(&pack, UiInput::default(), pressed(), true)
            .unwrap();
        assert!(
            session.game.room_actions[1].is_none(),
            "the entry is consumed"
        );
        tick_until(&mut session, &pack, 300, "the viewer close", |session| {
            session.pickup_view.is_none()
        });
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_none());
        assert!(!session.game.menu_pending());
    }

    #[test]
    fn a_room_item_box_action_opens_the_box_and_freezes_the_room() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        session.game.add_item(ITEM_FIRST_AID_SPRAY, 1);

        // An item box action probing the player position.
        session.game.room_actions[2] = Some(game::RoomAction {
            slot: 2,
            kind: game::RoomActionKind::ItemBox,
            zone: [0, 0, 100, 100],
            sce: 8,
            handler: 8,
            flags: 0x41,
            params: [0; 8],
            item_data: None,
            room_items_flag: 0xFF,
            reach_animation: false,
        });
        // The handler arms the lid; the ramp settles before the UI opens.
        for _ in 0..80 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            if session.item_box.is_some() {
                break;
            }
        }
        assert!(session.item_box.is_some(), "the box opened");
        assert!(session.menu.is_some(), "the inventory panel is underneath");
        assert!(
            session
                .game
                .objects
                .records
                .iter()
                .all(|record| record.rotation[2] < 0),
            "the lid record ramped open"
        );
        // The box opens through the menu's fade (darken, then fade in), and
        // only accepts input once it has settled.
        assert_eq!(session.menu_fade_phase, MenuFadePhase::Out);
        settle_menu_fade(&mut session, &pack);
        let before = session.game.frame;

        // Confirm arms the swap, confirm again deposits the spray.
        session
            .tick(
                &pack,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        session
            .tick(
                &pack,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert_eq!(session.game.item_box[0].id, ITEM_FIRST_AID_SPRAY);
        assert!(session.game.inventory.is_empty());
        assert_eq!(session.game.frame, before, "the room stayed frozen");

        // Cancel runs `menu_exit_cleanup` (`menu_itembox_interaction` returns
        // 1): the whole menu - box overlay still drawn - fades out and the
        // room resumes.
        session
            .tick(
                &pack,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert_eq!(session.menu_fade_phase, MenuFadePhase::Closing);
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_none(), "the panel closed");
        assert!(session.item_box.is_none(), "the box dropped with the panel");
        let frozen = session.game.frame;
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.game.frame, frozen + 1, "the room resumed");
    }

    #[test]
    fn the_item_box_cursor_drives_the_menu_cursor_and_name() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        session.game.add_item(ITEM_FIRST_AID_SPRAY, 1);
        session.game.add_item(ITEM_HANDGUN_AMMO, 30);
        session.open_item_box(&pack);

        // Right moves the box's cursor; the menu that draws the slot
        // highlight and the item name mirrors it.
        session
            .tick(
                &pack,
                UiInput {
                    right: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert_eq!(
            session
                .item_box
                .as_ref()
                .map(|item_box| item_box.player_cursor),
            Some(10)
        );
        let menu = session.menu.as_ref().expect("the panel stays open");
        assert_eq!(menu.cursor, 10, "the drawn cursor follows the box");
        assert_eq!(
            menu.selected_item, ITEM_HANDGUN_AMMO,
            "the drawn name follows"
        );

        // Confirm arms and confirms the swap under the drawn cursor: the ammo
        // in inventory slot 1 goes into box slot 0.
        for _ in 0..2 {
            session
                .tick(
                    &pack,
                    UiInput {
                        confirm: true,
                        ..UiInput::default()
                    },
                    player::Input::default(),
                    false,
                )
                .unwrap();
        }
        assert_eq!(session.game.item_box[0].id, ITEM_HANDGUN_AMMO);
        assert!(!session.game.has_item(ITEM_HANDGUN_AMMO));
        assert_eq!(session.menu.as_ref().unwrap().cursor, 10);
    }

    #[test]
    fn the_item_box_keeps_the_heartbeat_sweeping() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        session.game.add_item(ITEM_FIRST_AID_SPRAY, 1);
        session.open_item_box(&pack);

        // A fresh monitor has not run yet: the original's item-box state 3
        // calls `menu_draw_health_bar` every frame, so the first box tick
        // initialises the EKG. Its case 0 falls through into case 1, so the
        // head steps to 0x85 and immediately wraps to 0x35 (off the window's
        // left edge); the sweep then fills the window over the next ~0x50
        // ticks.
        assert_eq!(session.menu.as_ref().unwrap().health_bar.state, 0);
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(
            session.menu.as_ref().unwrap().health_bar.state,
            1,
            "the item box starts the EKG sweep"
        );
        for _ in 0..70 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        let first = session.menu.as_ref().unwrap().health_bar.primary_span();
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        let second = session.menu.as_ref().unwrap().health_bar.primary_span();
        assert_ne!(first, second, "the EKG head advances while the box is up");
    }

    #[test]
    fn using_a_key_item_closes_the_inventory() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        session.game.add_item(ITEM_SWORD_KEY, 1);
        session.game.set_item_use_flag(ITEM_SWORD_KEY, true);

        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        settle_menu_fade(&mut session, &pack);
        // Confirm opens the submenu, confirm again uses the key: the original
        // sets the close bit and the menu exits through its fade.
        session
            .tick(
                &pack,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        settle_submenu_open(&mut session, &pack);
        session
            .tick(
                &pack,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        // The use runs on the frame after the confirm, then the submenu's
        // close animation and the menu's exit fade.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        settle_submenu_close(&mut session, &pack);
        assert_eq!(
            session.game.last_picked_item, None,
            "the key is used, not picked"
        );
        assert!(
            !session.game.has_item(ITEM_SWORD_KEY),
            "the key was consumed"
        );
        assert_eq!(session.menu_fade_phase, MenuFadePhase::Closing);
        settle_menu_fade(&mut session, &pack);
        assert!(session.menu.is_none(), "the inventory closed after the use");
        assert_eq!(
            session.room_fade_alpha(),
            Some(207),
            "the room fades back in"
        );
    }

    #[test]
    fn tab_cycles_the_menu_tabs_and_the_file_tab_lists_documents() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        session.game.set_file_collected(0x0F, true);

        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.menu.is_some());
        settle_menu_fade(&mut session, &pack);

        // The first Tab lands on the map tab, which opens the floor plan
        // (the tab needs no radio).
        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.map.is_some(), "the map tab opens");
        assert!(session.file.is_none());

        // Cancel closes the map back to the inventory.
        session
            .tick(
                &pack,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.map.is_none());

        // The second Tab reaches the file tab and opens the selector on the
        // collected document.
        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        let file = session.file.as_ref().expect("the file tab opened");
        assert_eq!(file.slot, 0);
        assert!(session.menu.is_some(), "the menu stays open underneath");

        // Confirm opens the reader; let its entry slide settle, then cancel
        // slides it back out to the selector and a second cancel closes only
        // the tab. The FILE tab's first frames still run the selector's own
        // entry slide, which holds its busy lock.
        tick_until(&mut session, &pack, 32, "the selector entry slide", |s| {
            s.file.as_ref().is_some_and(|file| !file.input_locked())
        });
        session
            .tick(
                &pack,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        // The confirm fades the selector out first; the reader appears when
        // the hand-off completes, then its entry slide settles.
        tick_until(&mut session, &pack, 32, "the reader hand-off", |session| {
            session
                .file
                .as_ref()
                .is_some_and(|file| file.mode == crate::ui::file::FileMode::Reader)
        });
        tick_until(
            &mut session,
            &pack,
            64,
            "the reader entry slide",
            |session| {
                session.file.as_ref().is_some_and(|file| {
                    file.reader_state == crate::ui::file::ReaderState::Idle && !file.input_locked()
                })
            },
        );
        session
            .tick(
                &pack,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        tick_until(
            &mut session,
            &pack,
            64,
            "the reader exit slide",
            |session| {
                session.file.as_ref().is_some_and(|file| {
                    file.mode == crate::ui::file::FileMode::List && !file.input_locked()
                })
            },
        );
        assert_eq!(
            session.file.as_ref().unwrap().mode,
            crate::ui::file::FileMode::List
        );
        session
            .tick(
                &pack,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.file.is_none());
        assert!(session.menu.is_some(), "the inventory menu remains");
    }

    #[test]
    fn start_is_refused_while_a_message_is_displayed() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        session.game.show_message(0x40, 0);
        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.menu.is_none(), "START waits for the message");
        assert!(session.game.message.active);
    }

    #[test]
    fn a_menu_refusal_message_routes_through_the_window_with_menu_rules() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();
        // At full health the green herb's USE is refused; the menu reports
        // the refusal as 0xf7 + the heal category (7).
        session.game.entities[0].health = session.game.max_health;
        session.game.add_item(ITEM_GREEN_HERB, 1);

        session
            .tick(
                &pack,
                UiInput {
                    start: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        assert!(session.menu.is_some());
        settle_menu_fade(&mut session, &pack);
        session
            .tick(
                &pack,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        settle_submenu_open(&mut session, &pack);
        session
            .tick(
                &pack,
                UiInput {
                    confirm: true,
                    ..UiInput::default()
                },
                player::Input::default(),
                false,
            )
            .unwrap();
        // The refusal action runs on the frame after the confirm.
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert!(session.game.message.active, "the refusal message opened");
        assert_eq!(session.game.message.id, Some(0xFE));
        assert!(session.game.message.uses_menu_position());
        assert_eq!(session.game.message.pause, 0, "menu messages never pause");
        assert!(session.game.message_menu);

        // The room stays frozen with the message up and the text resolves.
        let frozen = session.game.frame;
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.game.frame, frozen);
        assert!(session.game.message.has_source());
    }

    #[test]
    fn new_game_state_matches_the_shipped_start() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();

        for character in 0..=1u8 {
            let session = GameSession::new(&pack, character, Path::new("saves")).unwrap();
            assert_eq!(
                session.game.id,
                RoomId {
                    stage: 1,
                    room: NEW_GAME_ROOM,
                    player_flag: character
                }
            );
            assert_eq!(session.player.pos, [NEW_GAME_POS_X, 0, NEW_GAME_POS_Z]);
            assert_eq!(session.player.angle, NEW_GAME_ANGLE);
            assert_eq!(
                session.game.entities[0].health,
                NEW_GAME_HEALTH[usize::from(character)]
            );
            assert_eq!(
                session.game.max_health,
                NEW_GAME_HEALTH[usize::from(character)]
            );
            assert_eq!(session.game.flags[7].bytes(), &NEW_GAME_ROOM_ITEMS);
            // The card's counter rides the new game; its first save shows 1.
            assert_eq!(
                session.game.state_bytes[usize::from(game::STATE_BYTE_SAVES)],
                1
            );
            assert_eq!(&session.game.state_bytes[0x24..0x28], &[0; 4]);
            // The starting health status reaches the `cmpb 50` state byte and
            // the health copy short.
            assert_eq!(session.game.health_status, 0x10);
            assert_eq!(
                session.game.state_bytes[usize::from(game::STATE_BYTE_HEALTH_STATUS)],
                0x10
            );
            assert_eq!(
                &session.game.state_bytes[usize::from(game::STATE_BYTE_HEALTH_COPY)
                    ..usize::from(game::STATE_BYTE_HEALTH_COPY) + 2],
                &NEW_GAME_HEALTH[usize::from(character)].to_le_bytes()
            );
            // `InitializeGame` writes the selected character into the model id.
            assert_eq!(
                session.game.state_bytes[usize::from(game::STATE_BYTE_CHARACTER_MODEL)],
                character
            );
            // The card's item box carries two Handgun Ammo stacks of 15.
            for slot in 0..2 {
                assert_eq!(session.game.item_box[slot].id, ITEM_HANDGUN_AMMO);
                assert_eq!(session.game.item_box[slot].quantity, 15);
            }
            // SetInitialItems seeds the three carried room-pickup quantities.
            assert_eq!(
                &session.game.state_bytes[0x0C..0x0F],
                &NEW_GAME_PICKUP_QUANTITIES
            );
            let ids: Vec<u8> = session.game.inventory.iter().map(|slot| slot.id).collect();
            if character == 0 {
                assert_eq!(ids, [ITEM_KNIFE, ITEM_FIRST_AID_SPRAY]);
                // Chris's six-slot run leaves Rebecca's card beretta in place.
                assert_eq!(session.game.rebecca_inventory[0].id, ITEM_BERETTA);
                assert_eq!(session.game.rebecca_inventory[0].quantity, 15);
            } else {
                assert_eq!(ids, [ITEM_KNIFE, ITEM_BERETTA, ITEM_FIRST_AID_SPRAY]);
                assert_eq!(session.game.item_count(ITEM_BERETTA), 15);
                // Jill's eight-slot run clears Rebecca's first two slots.
                assert!(
                    session
                        .game
                        .rebecca_inventory
                        .iter()
                        .all(|slot| slot.id == 0)
                );
            }
            assert_eq!(session.game.item_count(ITEM_FIRST_AID_SPRAY), 1);
        }
    }

    /// A directly booted room starts with the same playable health block a new
    /// game seeds: full health, the character's maximum, the `0x10` status
    /// byte, the BioCard health copy and the character's model byte.
    #[test]
    fn arbitrary_room_boot_seeds_the_character_health_and_status() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();

        for character in 0..=1u8 {
            let id = RoomId {
                stage: 1,
                room: NEW_GAME_ROOM,
                player_flag: character,
            };
            let session = GameSession::from_room(&pack, id, Path::new("saves")).unwrap();
            let health = NEW_GAME_HEALTH[usize::from(character)];
            assert_eq!(session.game.entities[0].health, health);
            assert_eq!(
                session.game.max_health,
                game::character_max_health(character)
            );
            assert_eq!(session.game.health_status, 0x10);
            assert_eq!(
                session.game.state_bytes[usize::from(game::STATE_BYTE_HEALTH_STATUS)],
                0x10
            );
            assert_eq!(
                &session.game.state_bytes[usize::from(game::STATE_BYTE_HEALTH_COPY)
                    ..usize::from(game::STATE_BYTE_HEALTH_COPY) + 2],
                &health.to_le_bytes()
            );
            assert_eq!(
                session.game.state_bytes[usize::from(game::STATE_BYTE_CHARACTER_MODEL)],
                character
            );
        }
    }

    /// Without a paired door the arbitrary start falls back to the first walk
    /// zone's edge, picked from the entry door's direction, and rejects a spot
    /// a fully-blocking collision record covers.
    #[test]
    fn arbitrary_entry_falls_back_to_the_walk_zone_edge() {
        use crate::state::{CollisionRect, WalkZone};

        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let target = RoomId::parse("100").unwrap();
        let mut room = RoomState {
            walk_zones: vec![
                WalkZone {
                    x1: 0,
                    z1: 0,
                    x2: 1000,
                    z2: 1000,
                    field_08: 0,
                    flags: 0,
                },
                WalkZone {
                    x1: 0,
                    z1: 1000,
                    x2: 1000,
                    z2: 1600,
                    field_08: 0,
                    flags: 0,
                },
            ],
            ..RoomState::default()
        };
        let mut game = game::GameState::new(target, &room);
        // The destination room is absent from the pack, so no pair is found
        // and the direction heuristic runs.
        game.doors[0] = Some(game::Door {
            slot: 0,
            direction: 0,
            next_room: 0x1F,
            ..game::Door::default()
        });

        let (pos, angle) = room_entry_placement(&pack, &room, &game, target).unwrap();
        assert_eq!(pos, [500, 0, 1300], "dir 0: south of the first zone");
        assert_eq!(angle, 0xC00);

        // A fully-blocking rectangle over the primary spot rejects it; the
        // zone centre is used instead.
        room.collision.quadrants[0].push(CollisionRect {
            x_max: 1000,
            z_max: 1500,
            x_min: 0,
            z_min: 1150,
            kind: 1,
            flags: 0x300,
        });
        let (pos, angle) = room_entry_placement(&pack, &room, &game, target).unwrap();
        assert_eq!(pos, [500, 0, 500], "the zone centre");
        assert_eq!(angle, 0);
    }

    /// The real pack's `--room` boot places the two reported stairwell rooms
    /// on the landing their paired door arrives at.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_arbitrary_room_start_lands_on_the_stair_landing() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        for (room, pos, angle) in [
            ("2070", [8600, 2885, 19000], 0u16),
            ("2030", [17100, 0, 25300], 0xC00),
        ] {
            let id = RoomId::parse(room).unwrap();
            let session = GameSession::from_room(&pack, id, Path::new("saves")).unwrap();
            assert_eq!(session.player.pos, pos, "{room} placement");
            assert_eq!(session.player.angle, angle, "{room} angle");
        }
    }

    /// Issue #83: Jill's room 101 boot arrives at the stairwell door and must
    /// select cut 3 there. The walk-zone spawn selects cut 1 first, so the
    /// placement's re-scan has to start from camera 0.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_room_101_boots_on_the_stairwell_cut() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let session =
            GameSession::from_room(&pack, RoomId::parse("1011").unwrap(), Path::new("saves"))
                .unwrap();
        assert_eq!(session.player.pos, [4300, 0, 3100], "the stairwell arrival");
        assert_eq!(session.loaded.room.current_cut, 3);
        assert_eq!(session.game.camera.current_cut, 3);
    }

    /// The real pack must boot each new game into room 106 (the main hall)
    /// with the shipped inventory, health, item box and room-item flags.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_new_game_lands_in_room_106_for_both_characters() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        for character in 0..=1u8 {
            let session = GameSession::new(&pack, character, Path::new("saves")).unwrap();
            assert_eq!(session.loaded.id.room3(), "106");
            assert_eq!(
                session.game.id,
                RoomId {
                    stage: 1,
                    room: NEW_GAME_ROOM,
                    player_flag: character
                }
            );
            // The main hall's init script repositions the player for the
            // opening scene, so the InitPlayerData start only holds until the
            // room boot runs.
            assert_eq!(
                session.game.entities[0].health,
                NEW_GAME_HEALTH[usize::from(character)]
            );
            // The shipped card starts the counter at 1.
            assert_eq!(
                session.game.state_bytes[usize::from(game::STATE_BYTE_SAVES)],
                1
            );
            // The card's item box carries two Handgun Ammo stacks of 15.
            assert_eq!(session.game.item_box[0].id, ITEM_HANDGUN_AMMO);
            assert_eq!(session.game.item_box[0].quantity, 15);
            assert_eq!(session.game.item_box[1].id, ITEM_HANDGUN_AMMO);
            assert_eq!(session.game.item_box[1].quantity, 15);
            // `InitializeGame` writes the selected character into the model id.
            assert_eq!(
                session.game.state_bytes[usize::from(game::STATE_BYTE_CHARACTER_MODEL)],
                character
            );
            // Jill's first playthrough raises the main hall's ink-ribbon
            // room-items bit, which the shipped bank already carries; the
            // ribbon itself is skipped by the `item_aot_set` first-run rule.
            let mut expected_items = game::FlagBank::new();
            expected_items
                .bytes_mut()
                .copy_from_slice(&NEW_GAME_ROOM_ITEMS);
            assert_eq!(session.game.flags[7].bytes(), expected_items.bytes());
            if character == 1 {
                assert!(
                    session.game.flag_test(
                        game::BANK_SCENARIO2,
                        game::SCENARIO2_FLAG_JILL_FIRST_RUN,
                        false
                    ),
                    "Jill's first run raises the scenario-2 marker"
                );
            }
            let ids: Vec<u8> = session.game.inventory.iter().map(|slot| slot.id).collect();
            if character == 0 {
                assert_eq!(ids, [ITEM_KNIFE, ITEM_FIRST_AID_SPRAY]);
                // Chris's six-slot run leaves Rebecca's card beretta in place.
                assert_eq!(session.game.rebecca_inventory[0].id, ITEM_BERETTA);
                assert_eq!(session.game.rebecca_inventory[0].quantity, 15);
            } else {
                assert_eq!(ids, [ITEM_KNIFE, ITEM_BERETTA, ITEM_FIRST_AID_SPRAY]);
            }
        }
    }

    /// A save written from one session must continue through `from_save`.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_load_continues_a_save_into_gameplay() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let mut session = GameSession::new(&pack, 0, Path::new("saves")).unwrap();
        session.game.entities[0].health = 88;
        session.game.entities[0].pos = [12000, 0, 3300];
        session.game.entities[0].angle = 512;
        session.game.add_item(0x0F, 30);
        session.player.pos = [12000, 0, 3300];
        session.player.angle = 512;

        let dir = TempDir::new();
        let saves = dir.0.join("saves");
        let card = pack.read(save::SAVE_PREFIX_ENTRY).unwrap();
        let file = save::SaveFile::from_state_with_prefix(&session.game, &card).unwrap();
        save::save(&saves, 3, &file).unwrap();

        let parsed = save::load(&saves, 3).unwrap();
        let continued = GameSession::from_save(&pack, &parsed, Path::new("saves")).unwrap();
        assert_eq!(continued.game.id, session.game.id);
        assert_eq!(continued.game.entities[0].health, 88);
        // The maximum is re-derived from the saved character, not left at 0.
        assert_eq!(continued.game.max_health, 140, "Chris's continue maximum");
        assert_eq!(continued.player.pos, [12000, 0, 3300]);
        assert_eq!(continued.player.angle, 512);
        assert_eq!(continued.game.item_count(0x0F), 30);
        assert!(
            continued
                .game
                .inventory
                .iter()
                .any(|slot| slot.id == ITEM_KNIFE)
        );
        assert_eq!(
            continued.game.state_bytes[usize::from(game::STATE_BYTE_SAVES)],
            2,
            "the saved 1 plus the continue path's increment"
        );
    }

    /// A continue discards the block's saved camera id: `room_set`'s camera
    /// block zeroes `g_roomCameraId` before the switch-zone scan, so the walk
    /// re-homes from camera 0 at the saved position. Regression for #42: the
    /// block's camera byte is the cut that was live when the game was saved,
    /// not the cut the saved position selects. The block is the checked-in
    /// differential fixture, so a fixture or boot-camera change fails here.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_continue_rehomes_the_camera_from_zero() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("oracle/saves/bootcam_chris_104.dat");
        let parsed = save::load_file(&fixture).unwrap();
        // Room 104 (Chris) inside the far wall's collision volume, saved while
        // the room boot had cut 2 live.
        assert_eq!(parsed.room, 0x04);
        assert_eq!(parsed.pos_x, 3700);
        assert_eq!(parsed.pos_z, 6600);
        assert_eq!(parsed.camera, 2, "the block carries the live cut");
        let continued = GameSession::from_save(&pack, &parsed, Path::new("saves")).unwrap();
        assert_eq!(continued.player.pos, [3700, 0, 6600]);
        let expected = player::camera_for_position(&continued.loaded.room, 0, continued.player.pos);
        assert_ne!(
            expected,
            usize::from(parsed.camera),
            "the saved cut must be discarded"
        );
        assert_eq!(
            continued.game.camera.current_cut, expected,
            "the continue re-homes the camera from camera 0"
        );
        assert_eq!(continued.loaded.room.current_cut, expected);
        assert_eq!(
            continued.game.state_bytes[usize::from(game::STATE_BYTE_ROOM_CAMERA)],
            expected as u8,
            "the BioCard camera byte follows the re-homed cut"
        );
    }

    /// The real new-game loading narration holds the room frozen until the
    /// encoded action byte in global 0x5B dismisses it, then arms the
    /// room-entry fade.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_boot_narration_holds_the_room_until_it_clears() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let mut session = GameSession::new(&pack, 0, Path::new("saves")).unwrap();
        session.request_boot_message(game::MESSAGE_BOOT_NEW_GAME);

        let room_frame = session.game.frame;
        for _ in 0..5 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        assert!(
            session.game.message.active,
            "the narration is still revealing after five ticks"
        );
        assert_eq!(session.game.frame, room_frame, "the room must stay frozen");

        let mut ticks = 5;
        while session.game.message.active && ticks < 600 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            ticks += 1;
        }
        assert!(
            !session.game.message.active,
            "the narration never dismissed"
        );
        assert!(ticks > 5, "the dismissal came from the message's own delay");
        assert_eq!(
            session.room_fade_draw_alpha(),
            Some(255),
            "the clearing tick draws the armed full black veil over the room"
        );
        assert_eq!(
            session.room_fade_alpha(),
            Some(207),
            "the accumulator already took the arm frame's draw-then-add step"
        );
    }

    /// The real locked door's message renders through the session's own
    /// framebuffer over the frozen room.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_locked_door_message_draws_in_the_session() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("1010").unwrap();
        let mut session = GameSession::from_room(&pack, id, Path::new("saves")).unwrap();

        // Stand in the key door's zone with no key: the next ticks request and
        // resolve its locked message without the player moving.
        let door = session.game.doors[1].expect("ROOM1010's key door");
        let center_x = i32::from(door.zone[0]) + i32::from(door.zone[2]) / 2;
        let center_z = i32::from(door.zone[1]) + i32::from(door.zone[3]) / 2;
        let (dx, dz) = player::reach_offset(0);
        session.player.pos = [center_x - dx, 0, center_z - dz];
        session.player.angle = 0;
        session.game.sync_entity_from_player(&session.player);
        settle_control(&mut session, &pack);
        // Freeze the frame before the message exists; the tick loop below does
        // not render, so it still holds this scene when the window is drawn.
        session.render(&pack);
        session
            .tick(
                &pack,
                UiInput::default(),
                player::Input {
                    action_pressed: true,
                    action_held: true,
                    ..player::Input::default()
                },
                true,
            )
            .unwrap();
        assert_eq!(session.game.message.id, Some(201));
        for _ in 0..600 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            if session.game.message.has_source() {
                break;
            }
        }
        assert!(session.game.message.has_source());

        // Draw the resolved window with the session's own font and tables over
        // the frozen gameplay frame, exactly as `GameSession::render` does.
        let font = session.font.as_ref().expect("the real pack carries a font");
        let mut framebuffer = Framebuffer::new();
        framebuffer.copy_from(session.frame());
        let before = framebuffer.rgba.clone();
        session
            .game
            .message
            .draw(&mut framebuffer, font, &session.text);
        let changed = before
            .iter()
            .zip(&framebuffer.rgba)
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            changed > 100,
            "the locked door message drew only {changed} session pixels"
        );
    }

    /// Two `--ui title` captures must be byte-identical.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_title_capture_is_deterministic() {
        let _sdl = crate::audio::test_lock::sdl();
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let dir = TempDir::new();
        let saves = dir.0.join("saves");
        let first_path = dir.0.join("title_a.bmp");
        let second_path = dir.0.join("title_b.bmp");

        run_ui_with_options(Path::new(&path), "title", Some(&first_path), &saves, 0).unwrap();
        run_ui_with_options(Path::new(&path), "title", Some(&second_path), &saves, 0).unwrap();

        let first = std::fs::read(&first_path).unwrap();
        let second = std::fs::read(&second_path).unwrap();
        assert_eq!(first, second, "two title captures differ");
        let decoded = bmp::decode(&first).unwrap();
        assert_eq!(
            (decoded.width, decoded.height),
            (WIDTH as u32, HEIGHT as u32)
        );
        assert!(non_black_pixels(&decoded) > 5000);
        assert_eq!(fnv1a(&first), fnv1a(&second));
    }

    /// Two `--ui select` captures must be byte-identical.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_select_capture_is_deterministic() {
        let _sdl = crate::audio::test_lock::sdl();
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let dir = TempDir::new();
        let saves = dir.0.join("saves");
        let first_path = dir.0.join("select_a.bmp");
        let second_path = dir.0.join("select_b.bmp");

        run_ui_with_options(Path::new(&path), "select", Some(&first_path), &saves, 0).unwrap();
        run_ui_with_options(Path::new(&path), "select", Some(&second_path), &saves, 0).unwrap();

        let first = std::fs::read(&first_path).unwrap();
        let second = std::fs::read(&second_path).unwrap();
        assert_eq!(first, second, "two select captures differ");
        let decoded = bmp::decode(&first).unwrap();
        assert!(non_black_pixels(&decoded) > 5000);
    }

    /// The new-game boot frame capture must be deterministic.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_new_game_capture_is_deterministic() {
        let _sdl = crate::audio::test_lock::sdl();
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let dir = TempDir::new();
        let saves = dir.0.join("saves");
        let first_path = dir.0.join("game_a.bmp");
        let second_path = dir.0.join("game_b.bmp");

        run_ui_with_options(Path::new(&path), "game", Some(&first_path), &saves, 0).unwrap();
        run_ui_with_options(Path::new(&path), "game", Some(&second_path), &saves, 0).unwrap();

        let first = std::fs::read(&first_path).unwrap();
        let second = std::fs::read(&second_path).unwrap();
        assert_eq!(first, second, "two new-game captures differ");
        let decoded = bmp::decode(&first).unwrap();
        assert!(
            non_black_pixels(&decoded) > 2000,
            "the new-game frame is mostly black"
        );
    }
    fn effect_test_room() -> RoomState {
        RoomState {
            cuts: vec![crate::state::Cut {
                index: 0,
                pos: [0, 0, 0],
                look_at: [0, 0, 1000],
                fov: 200,
                ..crate::state::Cut::default()
            }],
            zones: vec![crate::state::Zone {
                cam_from: 0,
                cam_to: 0,
                corners: [[0, 0], [0, 2000], [2000, 2000], [2000, 0]],
            }],
            ..RoomState::default()
        }
    }

    fn effect_test_pages() -> EffectPages {
        let mut pages = EffectPages::default();
        pages.base[0] = Some(solid_effect_page());
        pages.base[1] = Some(solid_effect_page());
        pages
    }

    fn solid_effect_page() -> Image {
        Image {
            width: 256,
            height: 256,
            rgba: vec![7; 256 * 256 * 4],
        }
    }

    #[test]
    fn effect_quads_apply_scale_tint_and_the_depth_cull() {
        let id = RoomId::parse("1000").unwrap();
        let room = effect_test_room();
        let mut game = game::GameState::new(id, &room);
        let mut block = crate::effects::fixtures::block(1, 0, 0);
        block[14] = 0x02; // transform bit, so the position integrates
        let mut weapon =
            crate::effects::fixtures::sprite(9, std::array::from_fn(|_| vec![vec![block]]));
        weapon.info.frames[0].width = 128;
        weapon.info.frames[0].height = 128;
        weapon.info.uvs[0].pivot_x = 64;
        weapon.info.uvs[0].pivot_y = 64;
        game.weapon_effects.index[1] = 9;
        game.weapon_effects.sprites.push(weapon);

        let room_effects = Rc::clone(&game.room_effects);
        crate::effects::create(&mut game, &room_effects, 9, 8, 0, [100, 0, 1000], 0, 1).unwrap();
        crate::effects::create(&mut game, &room_effects, 9, 8, 0, [100, 0, 300_000], 0, 1).unwrap();
        game.tick_effects(&room);

        let camera = Camera::from_cut(&room.cuts[0]);
        let pages = effect_test_pages();
        let quads = build_effect_quads(&game, &room, id, &camera, &pages, &mut Tween::default());

        assert_eq!(quads.len(), 1, "the far effect must be culled");
        let quad = &quads[0];
        // depth = 250, key = (250 >> 4) * 0x40 = 960.
        assert_eq!(quad.key, 960);
        // scale = 200 * 1 * 128 * 256 / (1000 + 1) = 6548 -> 204 px wide.
        assert_eq!(quad.size, [204, 204]);
        assert_eq!(quad.pos, [77, 18]);
        // depth_group 8 -> tint level 1 of colour record 1.
        assert_eq!(quad.tint, [0xB2, 0xB2, 0xB2]);

        // The frame's screen offset (the shake) shifts the billboard.
        let mut shaken = camera;
        shaken.screen = [5, -4];
        let quads = build_effect_quads(&game, &room, id, &shaken, &pages, &mut Tween::default());
        assert_eq!(quads[0].pos, [82, 14]);
    }

    #[test]
    fn room_sprites_keep_their_authored_palette() {
        let id = RoomId::parse("1000").unwrap();
        let room = effect_test_room();
        let mut game = game::GameState::new(id, &room);
        let mut room_effects = crate::effects::RoomEffects::default();
        let mut block = crate::effects::fixtures::block(1, 0, 0);
        block[14] = 0x02;
        let mut sprite =
            crate::effects::fixtures::sprite(38, std::array::from_fn(|_| vec![vec![block]]));
        sprite.info.page_id = 0x18 + 1;
        sprite.info.page_v = 10;
        sprite.info.uvs[0].v = 10;
        room_effects.index[0] = 38;
        room_effects.sprites.push(sprite);
        game.room_effects = Rc::new(room_effects);

        let room_effects = Rc::clone(&game.room_effects);
        crate::effects::create(&mut game, &room_effects, 38, 8, 0, [100, 0, 1000], 0, 1).unwrap();
        game.tick_effects(&room);

        let camera = Camera::from_cut(&room.cuts[0]);
        let quads = build_effect_quads(
            &game,
            &room,
            id,
            &camera,
            &effect_test_pages(),
            &mut Tween::default(),
        );
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].page, 1);
        assert_eq!(quads[0].tint, [0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn variant_rows_redirect_and_fall_back() {
        let mut sprite = crate::effects::fixtures::sprite(38, std::array::from_fn(|_| Vec::new()));
        sprite.geometry.clut_rows = 3;
        let mut pages = EffectPages::default();
        pages.base[1] = Some(transparent_effect_page());
        pages.variants[3 + 1] = Some(transparent_effect_page());

        // Tint 3 clamps to the last row (2) and redirects to that variant.
        assert_eq!(select_effect_page(&pages, 1, true, &sprite, 24), Some(8));
        // Tint 2 uses its own baked row.
        assert_eq!(select_effect_page(&pages, 1, true, &sprite, 16), Some(8));
        // Tint 1 has no baked row and falls back to the base page.
        assert_eq!(select_effect_page(&pages, 1, true, &sprite, 8), Some(1));
        // Weapon art never redirects.
        assert_eq!(select_effect_page(&pages, 1, false, &sprite, 24), Some(1));
    }

    /// A standalone `.scd` override whose init sets FG_SCENARIO bit 1.
    fn override_container() -> Vec<u8> {
        scd::asm::assemble(
            "\
.version 1

.init
.block
    set                     FG_SCENARIO, 1, 0

.main
",
        )
        .unwrap()
        .to_container()
        .unwrap()
    }

    #[test]
    fn load_room_prefers_a_standalone_scd_override() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("game.akpak");
        let id = RoomId::parse("1000").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();

        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x0E, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.add(&id.scd_entry(), override_container()).unwrap();
        writer.write(&pack_path).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let loaded = load_room(&pack, id).unwrap();
        assert_eq!(loaded.script_source, ScriptSource::Override);
        assert_eq!(loaded.scripts.init.len(), 1);
        assert_eq!(
            loaded.scripts.init[0].insns[0].bytes,
            [0x05, 0x00, 0x01, 0x00]
        );
    }

    #[test]
    fn load_room_falls_back_to_the_rdt_scripts() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("game.akpak");
        let id = RoomId::parse("1000").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();

        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x0E, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.write(&pack_path).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let loaded = load_room(&pack, id).unwrap();
        assert_eq!(loaded.script_source, ScriptSource::Rdt);
        assert_eq!(loaded.scripts.init[0].insns[0].bytes, [0x0E, 0x00]);
    }

    #[test]
    fn simulate_room_runs_the_override_scripts() {
        let dir = TempDir::new();
        let pack_path = dir.0.join("game.akpak");
        let id = RoomId::parse("1000").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();

        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x0E, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.add(&id.scd_entry(), override_container()).unwrap();
        writer.write(&pack_path).unwrap();

        let pack = Pack::open(&pack_path).unwrap();
        let run = simulate_room(&pack, id, 1, player::Input::default()).unwrap();
        assert_eq!(run.script_source, ScriptSource::Override);
        assert!(
            run.game.flag_test(0, 1, false),
            "the override init set FG_SCENARIO bit 1"
        );

        // Without the override the RDT script leaves the flag clear and the
        // deterministic run stays byte-identical to the vanilla path.
        let plain_path = dir.0.join("plain.akpak");
        let mut plain = PackWriter::new();
        plain
            .add(&id.rdt_entry(), synthetic_rdt(&[0x0E, 0x00]))
            .unwrap();
        plain
            .add(&id.cut_entry(0), bmp::encode_to_vec(&test_image()).unwrap())
            .unwrap();
        plain.write(&plain_path).unwrap();
        let plain = Pack::open(&plain_path).unwrap();
        let plain_run = simulate_room(&plain, id, 1, player::Input::default()).unwrap();
        assert_eq!(plain_run.script_source, ScriptSource::Rdt);
        assert!(!plain_run.game.flag_test(0, 1, false));
        assert_eq!(plain_run.frame.rgba, run.frame.rgba);
    }

    #[test]
    fn init_event_requests_apply_in_program_order_at_room_boot() {
        // The ROOM3030 main-script shape at room-boot time: the init script's
        // `task_kill 6` immediately followed by `evt_exec 0, 6, event_00` must
        // leave the new event running. The reversed pair must stop it.
        let dir = TempDir::new();
        let id = RoomId::parse("1000").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();

        let container = |kill_first: bool| {
            let requests = if kill_first {
                "    task_kill               6\n    evt_exec                0, 6, event_00\n"
            } else {
                "    evt_exec                0, 6, event_00\n    task_kill               6\n"
            };
            let text = format!(
                ".version 1\n\n.init\n.block\n{requests}\n.main\n\n\
                 .event event_00\n    evt_single              6\n    \
                 set                     FG_SCENARIO, 5, 0\n    evt_finish\n"
            );
            scd::asm::assemble(&text).unwrap().to_container().unwrap()
        };

        let run = |kill_first: bool, path: &Path| {
            let mut writer = PackWriter::new();
            writer
                .add(&id.rdt_entry(), synthetic_rdt(&[0x0E, 0x00]))
                .unwrap();
            writer.add(&id.cut_entry(0), bmp_bytes.clone()).unwrap();
            writer.add(&id.scd_entry(), container(kill_first)).unwrap();
            writer.write(path).unwrap();
            let pack = Pack::open(path).unwrap();
            simulate_room(&pack, id, 1, player::Input::default()).unwrap()
        };

        let kill_first = run(true, &dir.0.join("kill-first.akpak"));
        assert!(
            kill_first.game.flags[0].bit(5),
            "task_kill before evt_exec must leave the new event running"
        );

        let start_first = run(false, &dir.0.join("start-first.akpak"));
        assert!(
            !start_first.game.flags[0].bit(5),
            "evt_exec before task_kill must stop the new event"
        );
    }

    #[test]
    fn a_game_session_applies_init_event_requests_at_room_boot() {
        // The literal `GameSession` boot path (`from_room` -> `enter_room`):
        // the init's ordered kill/start pair resolves before the first tick,
        // so the new event is live at boot and runs on tick one.
        let dir = TempDir::new();
        let id = RoomId::parse("1000").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let container = scd::asm::assemble(
            "\
.version 1

.init
.block
    task_kill               6
    evt_exec                0, 6, event_00

.main

.event event_00
    evt_single              6
    set                     FG_SCENARIO, 5, 0
    evt_finish
",
        )
        .unwrap()
        .to_container()
        .unwrap();
        let pack_path = dir.0.join("game.akpak");
        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x0E, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.add(&id.scd_entry(), container).unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();

        let mut session = GameSession::from_room(&pack, id, &dir.0.join("saves")).unwrap();
        assert_eq!(
            session.event_vm.active_slots(),
            1,
            "task_kill before evt_exec leaves the new event live at boot"
        );
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert!(
            session.game.flags[0].bit(5),
            "the init's event ran on the first tick"
        );
    }

    #[test]
    fn simulate_room_runs_the_lua_hooks_without_changing_the_frame() {
        use crate::manifest;

        let id = RoomId::parse("1000").unwrap();
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let override_container = scd::asm::assemble(
            "\
.version 1

.init

.main
.block
    message                 0, 0
",
        )
        .unwrap()
        .to_container()
        .unwrap();

        // The hook sets a room-entry flag, grants an item on tick 2 and
        // rewrites the raised message's id.
        let source = r#"
function on_room_load(api)
    api:flag_set(0, 4, true)
end

function on_tick(api, tick)
    if tick >= 2 then
        api:give_item(0x44)
    end
end

function on_message(api, id)
    return id + 1
end
"#;
        let manifest = manifest::Manifest {
            kind: manifest::PackKind::Mod,
            base: Some("base".to_string()),
            lua: vec!["lua/demo.lua".to_string()],
            ..manifest::Manifest::base("demo")
        };
        let mut writer = PackWriter::new();
        writer
            .add(manifest::ENTRY, manifest.render().into_bytes())
            .unwrap();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x0E, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes.clone()).unwrap();
        writer
            .add(&id.scd_entry(), override_container.clone())
            .unwrap();
        writer
            .add("lua/demo.lua", source.as_bytes().to_vec())
            .unwrap();
        let lua_pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let run = simulate_room(&lua_pack, id, 3, player::Input::default()).unwrap();
        assert!(run.game.flags[0].bit(4), "on_room_load set its flag");
        assert!(
            run.game.item_count(0x44) >= 1,
            "on_tick granted the item from tick 2"
        );
        assert_eq!(
            run.game.message.id,
            Some(1),
            "on_message rewrote the raised id"
        );

        // A Lua-less pack with the same override renders the identical frame.
        let mut plain = PackWriter::new();
        plain
            .add(&id.rdt_entry(), synthetic_rdt(&[0x0E, 0x00]))
            .unwrap();
        plain.add(&id.cut_entry(0), bmp_bytes).unwrap();
        plain.add(&id.scd_entry(), override_container).unwrap();
        let plain_pack = Pack::from_bytes(plain.to_bytes().unwrap()).unwrap();
        let plain_run = simulate_room(&plain_pack, id, 3, player::Input::default()).unwrap();
        assert!(!plain_run.game.flags[0].bit(4));
        assert_eq!(plain_run.game.message.id, Some(0));
        assert_eq!(plain_run.frame.rgba, run.frame.rgba);
    }

    #[test]
    fn switch_zone_member_joints_gate_on_the_camera_zone() {
        use crate::state::Zone;
        let room = RoomState {
            zones: vec![Zone {
                cam_from: 0,
                cam_to: 1,
                corners: [[0, 0], [0, 100], [100, 100], [100, 0]],
            }],
            ..RoomState::default()
        };
        let joints = vec![
            anim::Mat4x3 {
                r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                t: [50, 0, 50],
            },
            anim::Mat4x3 {
                r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                t: [5000, 0, 50],
            },
        ];
        // Joint 1 is a member of the 0x74 class; inside the zone it draws and
        // outside it is hidden, on top of any scripted hidden bit.
        let hidden = joint_hidden_mask(true, 0b10, &room, 0, 0, &joints);
        assert_eq!(hidden, 0b10, "the inside member must not be hidden");
        assert_eq!(
            joint_hidden_mask(true, 0b11, &room, 0, 0b01, &joints),
            0b11,
            "a member outside the zone joins the scripted hidden mask"
        );
        // Joint 0 is outside the member class and stays visible outside the
        // zone; joint 1 stops being hidden when no class is set.
        assert_eq!(joint_hidden_mask(true, 0, &room, 0, 0, &joints), 0);
        // A camera with no zone header hides every member.
        assert_eq!(joint_hidden_mask(true, 0b10, &room, 4, 0, &joints), 0b10);
        // An entity that has not entered the zone drops every non-member joint;
        // a member joint still draws while it sits inside the zone.
        assert_eq!(
            joint_hidden_mask(false, 0, &room, 0, 0, &joints),
            0b11,
            "an off-zone entity with no member class draws no joint"
        );
        assert_eq!(
            joint_hidden_mask(false, 0b01, &room, 0, 0, &joints),
            0b10,
            "only the in-zone member joint survives"
        );
        assert_eq!(
            joint_hidden_mask(false, 0b10, &room, 4, 0, &joints),
            0b11,
            "an off-zone entity draws no joint without a zone header"
        );
    }

    /// A one-joint character mesh with a single textured triangle: enough for
    /// `render_frame` to pose and paint an NPC slot without a pack. The
    /// triangle's winding faces the `effect_test_room` cut camera.
    fn synthetic_npc_model() -> Emd {
        Emd {
            skeleton: Skeleton {
                relative: vec![[0, 0, 0]],
                children: vec![Vec::new()],
            },
            keyframes: vec![Keyframe::default()],
            clips: vec![Clip {
                frames: vec![ClipFrame {
                    keyframe: 0,
                    timing: 1,
                }],
            }],
            damage_clips: Vec::new(),
            damage_keyframes: Vec::new(),
            mesh: Tmd {
                objects: vec![crate::model::TmdObject {
                    vertices: vec![[0, 0, 0], [1000, 0, 0], [0, 1000, 0]],
                    normals: vec![[0, 0, 4096]],
                    prims: vec![crate::model::TmdPrim {
                        vertices: [0, 1, 2],
                        normals: [0, 0, 0],
                        uv: [[0, 0]; 3],
                        clut: 0x7800,
                        tsb: 0x80,
                        textured: true,
                        blend: false,
                        raw_y: false,
                        flat_color: None,
                        quad: None,
                    }],
                }],
            },
            texture: Texture8 {
                width: 1,
                height: 1,
                indices: vec![1],
                palettes: vec![[0, 0, 0, 255], [200, 80, 80, 255]],
                stp: Vec::new(),
            },
        }
    }

    #[test]
    fn an_npc_outside_the_camera_switch_zone_renders_no_triangles() {
        let id = RoomId::parse("1000").unwrap();
        let mut room = effect_test_room();
        room.ambient = [4095; 3];
        let mut game = game::GameState::new(id, &room);
        let entity = &mut game.entities[1];
        entity.status_flags |= game::ENTITY_STATUS_ACTIVE;
        entity.id = 0x21;
        entity.pos = [500, 0, 1500];
        entity.angle = 0;

        let model = synthetic_npc_model();
        let mut npc_models = enemy::EntityModelCache::default();
        npc_models.models.insert((0x21, 0), Arc::new(model));
        let pack = Pack::from_bytes(PackWriter::new().to_bytes().unwrap()).unwrap();
        let player_state = player::spawn(id, &room);

        let render =
            |game: &mut game::GameState, npc_models: &mut enemy::EntityModelCache| -> Framebuffer {
                let mut framebuffer = Framebuffer::new();
                render_frame(
                    &mut framebuffer,
                    &pack,
                    id,
                    &room,
                    &player_state,
                    game,
                    None,
                    npc_models,
                    &mut MaskCache::default(),
                    &mut ShadowCache::default(),
                    &mut EffectPageCache::default(),
                );
                framebuffer
            };

        // Inside the zone: the mesh paints as before.
        game.entities[1].has_enter_switch_zone = 1;
        let inside = render(&mut game, &mut npc_models);
        assert!(
            inside
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[..3] != [0, 0, 0]),
            "the in-zone NPC painted nothing"
        );

        // Outside: every joint is hidden, so the frame is empty.
        game.entities[1].has_enter_switch_zone = 0;
        let outside = render(&mut game, &mut npc_models);
        assert!(
            outside
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[..3] == [0, 0, 0]),
            "the off-zone NPC still painted"
        );
    }

    /// A synthetic 8bpp `ui/died.tim`: a 2-pixel-wide, 256-row page whose
    /// palette entry 1 is white and whose index-1 texels sit in both the
    /// strip region (row 0) and the backdrop region (row 0x40).
    fn synthetic_died_tim() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x10u32.to_le_bytes());
        bytes.extend_from_slice(&0x9u32.to_le_bytes());
        // CLUT block: 256 entries, entry 1 white.
        bytes.extend_from_slice(&524u32.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&256u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        for entry in 0..256u16 {
            let word = if entry == 1 { 0x7FFFu16 } else { 0x0000 };
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        // Image block: 1 unit (2 pixels) wide, 256 rows.
        bytes.extend_from_slice(&524u32.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&256u16.to_le_bytes());
        for row in 0..256u16 {
            if row == 0 || row == 0x40 {
                bytes.extend_from_slice(&[1u8, 0]);
            } else {
                bytes.extend_from_slice(&[0u8, 0]);
            }
        }
        bytes
    }

    /// A synthetic damage schedule: kill the player, run the fall, the
    /// 90-frame delay, the white-out, the DIED screen and the return to the
    /// title through the real session tick.
    #[test]
    fn a_synthetic_damage_schedule_runs_the_fall_fade_died_and_title() {
        let dir = TempDir::new();
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let pack_path = dir.0.join("death.akpak");
        let mut writer = PackWriter::new();
        writer
            .add(
                &id.rdt_entry(),
                synthetic_rdt_with_blocking_collision(&door_init_record(
                    1,
                    [777, 0, 888],
                    0x200,
                    0,
                )),
            )
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.add("ui/died.tim", synthetic_died_tim()).unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();

        let mut session = GameSession::from_room(&pack, id, Path::new("saves")).unwrap();
        session.game.apply_player_hurt(i16::MAX, 0);
        assert!(session.game.entities[0].health < 0, "the player died");

        let mut screen_open_at = None;
        let mut title_at = None;
        for tick in 0..900 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            if session.death.screen_open() {
                screen_open_at.get_or_insert(tick);
            }
            if session.take_return_title_request() {
                title_at = Some(tick);
                break;
            }
        }

        // The fall runs while the delay counts down: the player is in the
        // death state from the first tick. The synthetic pack carries no
        // player model, so the fall clip never completes and the state stays
        // 3; the game-over machine still runs to the title.
        assert_eq!(session.game.entities[0].state(), 3, "the fall state");
        let screen_open_at = screen_open_at.expect("the DIED screen opened");
        // The trigger tick starts the machine (the original's case 0 falls
        // through into case 1 and decrements once), 89 more delay ticks, then
        // 128 fade steps with the first already applied on the arm tick.
        assert_eq!(screen_open_at, 217);
        let title_at = title_at.expect("the title returned");
        assert!(title_at > screen_open_at, "the screen ran before the title");
        assert!(!session.death.screen_open());
        assert!(session.game.flags[5].bit(game::MSF_PLAYER_DEAD));
    }

    /// A scripted death's fade request skips the 90-frame delay: the screen
    /// opens one fade (128 ticks) after the request is consumed.
    #[test]
    fn a_scripted_fade_request_skips_the_death_delay() {
        let dir = TempDir::new();
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let pack_path = dir.0.join("death-fade.akpak");
        let mut writer = PackWriter::new();
        writer
            .add(
                &id.rdt_entry(),
                synthetic_rdt_with_blocking_collision(&door_init_record(
                    1,
                    [777, 0, 888],
                    0x200,
                    0,
                )),
            )
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.add("ui/died.tim", synthetic_died_tim()).unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();

        let mut session = GameSession::from_room(&pack, id, Path::new("saves")).unwrap();
        session.game.apply_player_hurt(i16::MAX, 0);
        assert!(session.game.entities[0].health < 0, "the player died");

        let mut screen_open_at = None;
        for tick in 0..400 {
            if tick == 1 {
                session.game.death_fade_request = true;
            }
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
            if session.death.screen_open() {
                screen_open_at = Some(tick);
                break;
            }
        }
        let screen_open_at = screen_open_at.expect("the DIED screen opened");
        assert_eq!(screen_open_at, 128, "the delay is skipped");
    }

    /// The DIED screen draws its backdrop and wavy strip from the packed page
    /// once the machine reaches the settled hold.
    #[test]
    fn the_died_screen_draws_the_packed_page() {
        let dir = TempDir::new();
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let pack_path = dir.0.join("death-art.akpak");
        let mut writer = PackWriter::new();
        writer
            .add(
                &id.rdt_entry(),
                synthetic_rdt_with_blocking_collision(&door_init_record(
                    1,
                    [777, 0, 888],
                    0x200,
                    0,
                )),
            )
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer.add("ui/died.tim", synthetic_died_tim()).unwrap();
        writer.write(&pack_path).unwrap();
        let pack = Pack::open(&pack_path).unwrap();

        let mut session = GameSession::from_room(&pack, id, Path::new("saves")).unwrap();
        session.game.apply_player_hurt(i16::MAX, 0);
        // Run into the screen's settled hold (the screen opens at 219; the
        // hold begins ~78 screen ticks later).
        for _ in 0..320 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), false)
                .unwrap();
        }
        assert!(session.death.screen_open(), "the screen is up");
        session.render(&pack);
        assert!(
            session
                .framebuffer
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[..3] != [0, 0, 0]),
            "the packed page painted"
        );
    }
}
