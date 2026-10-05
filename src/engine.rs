//! Engine entry point: open a pack, load a room, simulate and display it.
//!
//! # Documented deviations
//!
//! The world-item milestone leaves these deliberate gaps:
//! - no pick-up viewer: `give_item` (0x2D), the ordinary pick-up handler and
//!   the desk's take-item state award on the interaction frame, and the
//!   original's menu mode-3/4 item viewer and key-item list screen stay absent;
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
//! - the climb's camera screen-effect rectangles are recorded
//!   ([`crate::player::ScreenEffect`]) but the camera-scroll consumer that
//!   reads them is not ported;
//! - the original's fixed ordering-table depths (the flooded guardhouse and
//!   courtyard water rooms, the 1F right-stairs objects with type < 2, and the
//!   2F study/front-lesson open lid at slot 0x33) are not modelled: the port
//!   orders every object and item triangle by its mean view Z in the shared
//!   stable sort;
//! - object and item collision and floor probes keep the record's local matrix
//!   translation, exactly like the original's `update_room_objects` helpers;
//!   only rendering, the camera-switch cull and effect attach compose the SCA
//!   parent chain.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_void};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use sdl3_sys::blendmode::SDL_BLENDMODE_NONE;
use sdl3_sys::error::SDL_GetError;
use sdl3_sys::events::{
    SDL_EVENT_KEY_DOWN, SDL_EVENT_KEY_UP, SDL_EVENT_KEYBOARD_REMOVED, SDL_EVENT_QUIT,
    SDL_EVENT_WINDOW_FOCUS_LOST, SDL_Event, SDL_PollEvent,
};
use sdl3_sys::hints::{SDL_HINT_RENDER_DRIVER, SDL_HINT_VIDEO_DRIVER, SDL_SetHint};
use sdl3_sys::init::{SDL_INIT_VIDEO, SDL_Init, SDL_Quit};
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
    SDL_SCANCODE_BACKSPACE, SDL_SCANCODE_DOWN, SDL_SCANCODE_LEFT, SDL_SCANCODE_LEFTBRACKET,
    SDL_SCANCODE_LSHIFT, SDL_SCANCODE_RETURN, SDL_SCANCODE_RIGHT, SDL_SCANCODE_RIGHTBRACKET,
    SDL_SCANCODE_RSHIFT, SDL_SCANCODE_SPACE, SDL_SCANCODE_TAB, SDL_SCANCODE_UP, SDL_SCANCODE_X,
    SDL_Scancode,
};
use sdl3_sys::surface::{
    SDL_ConvertSurface, SDL_DestroySurface, SDL_SCALEMODE_NEAREST, SDL_Surface,
};
use sdl3_sys::timer::SDL_GetTicks;
use sdl3_sys::video::{
    SDL_CreateWindow, SDL_DestroyWindow, SDL_SetWindowTitle, SDL_Window, SDL_WindowFlags,
};

use crate::anim;
use crate::audio::{self, Mixer};
use crate::bgm;
use crate::bmp;
use crate::door;
use crate::effects;
use crate::emd;
use crate::font;
use crate::game;
use crate::items;
use crate::mask;
use crate::message::MessageInput;
use crate::model::Emd;
use crate::npc;
use crate::objects;
use crate::pack::Pack;
use crate::player;
use crate::rdt;
use crate::render::{
    self, Camera, EffectLayer, EffectQuad, EntityMesh, Framebuffer, Lighting, MaskLayer,
};
use crate::save;
use crate::scd;
use crate::sfx;
use crate::shadow;
use crate::state::{Image, RoomId, RoomState};
use crate::text::Text;
use crate::tim;
use crate::transition::{self, DoorStepper};
use crate::ui::file::{FileAssets, FileEvent, FileScreen};
use crate::ui::item_box::{ItemBox, ItemBoxAssets, ItemBoxEvent};
use crate::ui::main_menu::{MainMenu, MenuAssets, MenuEvent, MenuInput};
use crate::ui::{self, Screen, ScreenAction, ScreenResult, UiContext, UiInput};
use crate::voice;

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
    run_with_voice(pack, id, capture, ticks, None)
}

/// [`run`] with an explicit voice-pack path.
///
/// `voice` overrides the sibling `<pack stem>.voice.akpak` discovery; a path
/// that does not exist logs a warning and leaves voice lines skipped. The
/// voice pack is consulted only by the voice and BGM audio loaders.
pub fn run_with_voice(
    pack: &Path,
    id: RoomId,
    capture: Option<&Path>,
    ticks: u32,
    voice: Option<&Path>,
) -> Result<()> {
    let save_dir = save::default_save_dir_for_pack(pack);
    let voice_path = voice_pack_path(pack, voice);
    let pack = Pack::open(pack)?;

    if let Some(capture_path) = capture {
        let mut loaded = load_room(&pack, id)?;
        let mut game = new_game_state(&pack, id, &loaded.room);
        seed_room_items(&mut game);
        let mut player_state = player::spawn(id, &loaded.room);
        game.sync_entity_from_player(&player_state);
        run_room_init(&mut loaded, &mut game);
        drain_mask_toggles(&mut loaded.room, &mut game);
        apply_camera(&mut loaded.room, &mut game, Some(player_state.pos));
        bgm::update_room_bgm(&mut game, id, None);

        let mut npc_models = npc::EntityModelCache::default();
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
            for _ in 0..ticks {
                // TODO(parity): (harness) the transition tick_room returns is
                // discarded here, so a scripted scene that opens a door during
                // `--ticks` silently keeps its room instead of playing the
                // transition. The same drop exists in `simulate_loaded`.
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
                    },
                    player::Input::default(),
                );
                play_snd3d_requests(
                    &mut no_mixer,
                    &mut snd3d_cache,
                    &pack,
                    &loaded.room,
                    &mut game,
                );
                bgm::apply_live(&mut no_mixer, &mut game, &mut bgm_cache, &pack, None);
                tick_voice(&mut no_mixer, &mut voice_cache, &mut game, &pack, None);
                // No input is fed headlessly, so release a message's menu
                // choice the way an auto-confirm would; the F7 wait must not
                // outlive the frame that raised it.
                let choice = game.message.menu_choice_id();
                game.message.set_menu_choice_id(choice & 0x7F);
                drain_mask_toggles(&mut loaded.room, &mut game);
            }
        }

        let title = window_title(
            &loaded.id.room3(),
            loaded.room.current_cut,
            loaded.room.cuts.len(),
        );
        let display = Display::new(&title, true)?;
        let mut framebuffer = Framebuffer::new();
        render_frame(
            &mut framebuffer,
            &pack,
            loaded.id,
            &loaded.room,
            &player_state,
            &game,
            loaded.player_assets.as_ref(),
            &mut npc_models,
            &mut MaskCache::default(),
            &mut ShadowCache::default(),
            &mut EffectPageCache::default(),
        );
        display.present(&framebuffer)?;
        return display.capture(capture_path);
    }

    let mut session = GameSession::from_room(&pack, id, &save_dir)?;
    session.audio_packs = AudioPacks::new(voice_path);
    let title = window_title(
        &session.loaded.id.room3(),
        session.loaded.room.current_cut,
        session.loaded.room.cuts.len(),
    );
    let display = Display::new(&title, false)?;
    session.start_audio(&pack);
    run_session_loop(&pack, &mut session, &display)
}

/// The interactive gameplay loop shared by `--room` and the app's Play mode.
fn run_session_loop(pack: &Pack, session: &mut GameSession, display: &Display) -> Result<()> {
    let mut framebuffer = Framebuffer::new();
    let mut input = InputState::default();
    let mut event = SDL_Event::default();
    let mut last_ticks = unsafe { SDL_GetTicks() };
    let mut accumulator = 0.0f64;
    loop {
        let mut cut_delta = 0i32;
        if poll_events(&mut event, &mut input, &mut cut_delta) {
            return Ok(());
        }
        if cut_delta != 0 {
            session.step_camera_cut(cut_delta);
        }
        session.update_window_title(display.window)?;

        let now = unsafe { SDL_GetTicks() };
        accumulator += now.saturating_sub(last_ticks) as f64;
        last_ticks = now;
        if accumulator > 250.0 {
            accumulator = 250.0;
        }
        while accumulator >= TICK_MS {
            if session.transition_finished {
                accumulator = 0.0;
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
                    accumulator = 0.0;
                    break;
                }
            }
            accumulator -= TICK_MS;
        }

        // The pass that ends a transition is drawn once before teardown.
        session.render(pack);
        framebuffer.copy_from(session.frame());
        display.present(&framebuffer)?;
        display.show()?;
        if session.transition_finished {
            session.finish_transition(pack);
        }
        session.update_audio();
    }
}

/// The SDL window, renderer and streaming texture every screen shares.
///
/// The guards are declared before the raw pointers so the texture, renderer,
/// window and SDL tear down in that order.
struct Display {
    _texture: TextureHandle,
    _renderer: RendererHandle,
    _window: WindowHandle,
    _sdl: SdlHandle,
    texture: *mut SDL_Texture,
    renderer: *mut SDL_Renderer,
    window: *mut SDL_Window,
}

impl Display {
    /// Open the 960x720 window; `capture` selects SDL's offscreen software
    /// driver for headless captures.
    fn new(title: &str, capture: bool) -> Result<Self> {
        if capture {
            unsafe {
                SDL_SetHint(SDL_HINT_VIDEO_DRIVER, c"offscreen".as_ptr());
                SDL_SetHint(SDL_HINT_RENDER_DRIVER, c"software".as_ptr());
            }
        }

        unsafe { SDL_SetMainReady() };
        if !unsafe { SDL_Init(SDL_INIT_VIDEO) } {
            bail!("SDL_Init failed: {}", sdl_error());
        }
        let sdl = SdlHandle;

        let title = CString::new(title).context("window title contains a NUL byte")?;
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
        let window = WindowHandle(window);

        let renderer = unsafe { SDL_CreateRenderer(window.0, std::ptr::null()) };
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

        Ok(Self {
            texture: texture.0,
            renderer: renderer.0,
            window: window.0,
            _texture: texture,
            _renderer: renderer,
            _window: window,
            _sdl: sdl,
        })
    }

    /// Upload the framebuffer and draw it scaled to the window.
    fn present(&self, framebuffer: &Framebuffer) -> Result<()> {
        present(self.renderer, self.texture, framebuffer)
    }

    /// Flip the drawn backbuffer to the screen.
    fn show(&self) -> Result<()> {
        if !unsafe { SDL_RenderPresent(self.renderer) } {
            bail!("SDL_RenderPresent failed: {}", sdl_error());
        }
        Ok(())
    }

    /// Read the drawn frame back and write it as a BMP.
    fn capture(&self, path: &Path) -> Result<()> {
        capture_frame(self.renderer, path)
    }
}

/// One gameplay session: the loaded room, game state, player and VMs.
///
/// `--room` builds one through [`GameSession::from_room`] and the app's Play
/// mode through [`GameSession::new`] or [`GameSession::from_save`]. The engine
/// drives it one fixed tick at a time and renders through the session's own
/// framebuffer, so a gameplay modal can freeze the room and still show the
/// last frame underneath.
/// The message window, the inventory menu, the item box and the FILE tab are
/// **not** driven through the [`ui::Screen`] modal hook: the message is part
/// of [`game::GameState`] and keeps running on the gameplay tick, while the
/// others freeze the room but keep drawing the last gameplay frame, read
/// their art from the session and mutate the same `GameState`. The item box is
/// opened by a room `item_box` action and shares the inventory panel the menu
/// draws underneath it; the FILE tab is opened from the menu's top tab row
/// (the Tab key cycles it). The boxed modal hook is kept for screens that own
/// all of their state (the item viewer); it still freezes the room and draws
/// over the last gameplay frame exactly as before.
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
    /// The main pack plus the lazily opened optional voice pack.
    audio_packs: AudioPacks,
    /// Parsed NPC models, loaded lazily from the pack by the NPC driver.
    npc_models: npc::EntityModelCache,
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
    /// The typewriter save screen; the room stays frozen while it is up.
    save_screen: Option<ui::save_load::SaveLoadScreen>,
    /// Where `savedat*.dat` slot files live for this session.
    save_dir: PathBuf,
    framebuffer: Framebuffer,
    titled_cut: usize,
    transition: Option<TransitionMode>,
    transition_finished: bool,
    /// Gameplay modal hook.
    ///
    /// A gameplay agent (the item viewer) installs a boxed [`ui::Screen`]
    /// here. While one is present the engine must not call
    /// [`GameSession::tick`]: the room stays frozen, the modal advances
    /// through its own [`ui::Screen::update`], and the app draws the last
    /// gameplay frame from [`GameSession::frame`] underneath before calling
    /// the modal's draw and applying its [`ui::Screen::fade`] overlay. A modal
    /// closes by reporting [`ScreenAction::Resume`].
    modal: Option<Box<dyn ui::Screen>>,
    /// The item the open viewer was asked to examine; leaving the viewer
    /// marks it examined in [`game::GameState`].
    viewed_item: Option<u8>,
    /// A message dismissal consumed the action key while it was still held;
    /// the room tick keeps ignoring the action until the key is released,
    /// matching the original's cleared held/previous-held pad bits.
    swallow_action: bool,
}

/// New-game start position X (the original's `InitPlayerData`).
pub const NEW_GAME_POS_X: i32 = 17000;
/// New-game start position Z.
pub const NEW_GAME_POS_Z: i32 = 5000;
/// New-game facing angle.
pub const NEW_GAME_ANGLE: u16 = 3072;
/// New-game starting health: Chris then Jill.
pub const NEW_GAME_HEALTH: [i16; 2] = [140, 96];
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

impl GameSession {
    /// A new game as `character` (`0` Chris, `1` Jill): stage 1 room 0 with
    /// the original's start position, health, items and room-item flags, then
    /// the room init/main/event VMs.
    fn new(pack: &Pack, character: u8, save_dir: &Path) -> Result<Self> {
        let character = character & 1;
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: character,
        };
        let loaded = load_room(pack, id)?;
        let mut game = new_game_state(pack, id, &loaded.room);
        seed_new_game(&mut game, character);
        let mut player_state = player::spawn(id, &loaded.room);
        player_state.pos = [NEW_GAME_POS_X, 0, NEW_GAME_POS_Z];
        player_state.angle = NEW_GAME_ANGLE;
        game.sync_entity_from_player(&player_state);
        Self::from_loaded(pack, loaded, game, player_state, save_dir)
    }

    /// Boot `id` directly, the `--room` path. A direct boot is a fresh game
    /// state, so the shipped room-items bank is seeded like a new game's.
    fn from_room(pack: &Pack, id: RoomId, save_dir: &Path) -> Result<Self> {
        let loaded = load_room(pack, id)?;
        let mut game = new_game_state(pack, id, &loaded.room);
        seed_room_items(&mut game);
        let player_state = player::spawn(id, &loaded.room);
        game.sync_entity_from_player(&player_state);
        Self::from_loaded(pack, loaded, game, player_state, save_dir)
    }

    /// Continue from a parsed save: the block replaces the state, the saved
    /// position and angle place the player and the saved room loads.
    fn from_save(pack: &Pack, file: &save::SaveFile, save_dir: &Path) -> Result<Self> {
        let id = RoomId {
            stage: file.stage,
            room: file.room,
            player_flag: file.character & 1,
        };
        let loaded = load_room(pack, id)?;
        let mut game = new_game_state(pack, id, &loaded.room);
        file.apply_to(&mut game);
        let mut player_state = player::spawn(id, &loaded.room);
        player_state.pos = [
            i32::from(file.pos_x),
            player_state.pos[1],
            i32::from(file.pos_z),
        ];
        player_state.angle = file.angle as u16 & 0x0FFF;
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
    ) -> Result<Self> {
        let scripts = Rc::new(loaded.scripts.clone());
        let command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
        let event_vm = scd::vm::EventVm::from_scripts(scripts);
        // The message window's glyph sheet is optional: a pack without one
        // still runs the state machine and simply draws nothing.
        let font = match pack.read("font/font.tim") {
            Ok(bytes) => match tim::decode_4bpp(bytes) {
                Ok(texture) => Some(font::Font::new(texture)),
                Err(err) => {
                    eprintln!("warning: invalid font/font.tim: {err:#}");
                    None
                }
            },
            Err(_) => None,
        };
        let mut session = Self {
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
            audio_packs: AudioPacks::default(),
            npc_models: npc::EntityModelCache::default(),
            music: None,
            text: Text::load(pack),
            font,
            menu_assets: None,
            menu_assets_loaded: false,
            menu: None,
            item_box: None,
            item_box_assets: None,
            file: None,
            file_assets: None,
            save_screen: None,
            save_dir: save_dir.to_path_buf(),
            framebuffer: Framebuffer::new(),
            titled_cut: usize::MAX,
            transition: None,
            transition_finished: false,
            modal: None,
            viewed_item: None,
            swallow_action: false,
        };
        session.enter_room(pack, None);
        Ok(session)
    }

    /// Run the room boot: init script, queued events, the player mirror, mask
    /// toggles, the camera zone scan and the per-room BGM handoff.
    ///
    /// `from` is the room the player came from, used by the BGM state machine
    /// to resolve the outgoing group; `None` for a direct boot or save load.
    fn enter_room(&mut self, pack: &Pack, from: Option<RoomId>) {
        {
            let mut host = game::ScdGameHost::new(&mut self.game);
            self.command_vm.run_init(&mut host);
        }
        start_pending_events(&mut self.game, &mut self.event_vm);
        self.game.apply_room_edits(&mut self.loaded.room);
        self.game.sync_player(&mut self.player);
        drain_mask_toggles(&mut self.loaded.room, &mut self.game);
        apply_camera(&mut self.loaded.room, &mut self.game, Some(self.player.pos));
        // The original runs `update_room_bgm` after the destination's init
        // script (so a `room_bgm_state_set` it performs is visible) and after the
        // room's data is loaded.
        bgm::update_room_bgm(&mut self.game, self.loaded.id, from);
        let auxiliary = self.audio_packs.voice();
        bgm::apply_live(
            &mut self.music,
            &mut self.game,
            &mut self.bgm_cache,
            pack,
            auxiliary,
        );
    }

    /// One fixed 30 Hz tick: message window, scripts, interaction, player
    /// movement, camera, BGM/mask/footstep drains and a door transition
    /// request.
    ///
    /// The message window advances first so its yes/no post-actions (item
    /// pickup and use) reach the state before the room scripts run. While it
    /// masks the control bit the player's movement and action input are
    /// blanked, exactly like the original's cleared d-pad word. START then
    /// opens the pause menu instead of ticking the room; while the menu is up
    /// the room stays frozen and the same input drives the menu.
    fn tick(&mut self, pack: &Pack, ui: UiInput, input: player::Input, action: bool) -> Result<()> {
        if self.save_screen.is_some() {
            self.tick_save_screen(pack, ui);
            return Ok(());
        }
        if self.menu.is_some() {
            self.tick_menu(pack, ui, input, action);
            return Ok(());
        }

        let was_active = self.game.message.active;
        let was_locked = self.game.message_locks_controls();
        // TODO(parity): (scripting) the original advances the message window in
        // `main_loop` AFTER the game task (script/entity) pass; this window (and
        // its pickup/use post-actions) runs first, so state it changes is visible
        // to the same frame's scripts.
        self.game.update_message(
            MessageInput {
                action,
                left: input.left,
                right: input.right,
            },
            &self.loaded.room,
            &self.text,
        );

        // The typewriter prompt was answered: a confirmed save opens the save
        // screen instead of running the room this tick.
        if let Some(ink_ribbon) = self.game.take_typewriter_confirm() {
            self.open_typewriter_save(pack, ink_ribbon);
            return Ok(());
        }

        if ui.start && !self.game.message.active {
            self.open_menu(pack);
            return Ok(());
        }

        // A paused message ignores the player this tick; the press that
        // dismissed it is spent on the window rather than the room (the
        // original clears the held and previous held pad bits on dismissal),
        // so an action-gated zone behind the window never sees the same press.
        // The swallow then latches until the key is released, exactly like the
        // cleared edge-detect history.
        let dismissed = was_active && !self.game.message.active;
        if dismissed && action {
            self.swallow_action = true;
        } else if !action {
            self.swallow_action = false;
        }
        let input = if was_locked || self.swallow_action {
            player::Input::default()
        } else {
            input
        };

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
            },
            input,
        );
        // A typewriter runs its save prompt; the item-box overlay opens when
        // the lid has settled (`MSF_MENU_MODE_ITEMBOX`). The interaction is
        // consumed so the next probe has to fire again.
        let interaction = self.game.last_interaction.take();
        let typewriter_fired = interaction
            .is_some_and(|interaction| interaction.kind == game::RoomActionKind::Typewriter);
        if typewriter_fired {
            self.game.check_typewriter();
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
        bgm::apply_live(
            &mut self.music,
            &mut self.game,
            &mut self.bgm_cache,
            pack,
            self.audio_packs.voice(),
        );
        tick_voice(
            &mut self.music,
            &mut self.voice_cache,
            &mut self.game,
            pack,
            self.audio_packs.voice(),
        );
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
        // The global SE bank (the lid's `0x20`) has no pack mapping yet.
        self.game.sfx_requests.clear();
        if let Some(transition) = transition {
            let record = self.game.transition_door.take().unwrap_or_default();
            self.transition = Some(start_transition(pack, &record, &transition)?);
        } else if self.game.take_itembox_open() {
            self.open_item_box(pack);
        }
        Ok(())
    }

    /// The lazily loaded pause-menu art; a missing or invalid sheet is logged
    /// once and leaves the menu blank but functional.
    fn ensure_menu_assets(&mut self, pack: &Pack) {
        if self.menu_assets_loaded {
            return;
        }
        self.menu_assets_loaded = true;
        match MenuAssets::load(pack) {
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

    /// Close the pause menu and unfreeze the room.
    fn close_menu(&mut self) {
        self.menu = None;
        self.game.message_menu = false;
    }

    /// Open the item viewer as a gameplay modal over the frozen menu.
    ///
    /// The menu stays up underneath and the room stays frozen; leaving the
    /// viewer through [`ScreenAction::Resume`] returns to the menu unchanged.
    /// The frozen frame is painted here so booting straight into the viewer
    /// (`--ui view`) shows the menu under the model exactly like the
    /// interactive CHECK path.
    fn open_item_view(&mut self, pack: &Pack, item: u8) {
        self.render(pack);
        let mut screen = ui::item_view::ItemViewScreen::new(item);
        screen.open_with(pack, &self.text, &self.game.examined_flags());
        self.viewed_item = Some(item);
        self.open_modal(Box::new(screen));
    }

    /// Close the gameplay modal and run its close-out side effects. Returning
    /// from the item viewer marks the examined item, so the next name lookup
    /// shows its real name.
    fn close_modal(&mut self) {
        self.modal = None;
        if let Some(item) = self.viewed_item.take() {
            self.game.mark_examined(item);
        }
    }

    /// Open the save screen over the frozen room after the typewriter prompt
    /// was confirmed. The last gameplay frame is painted first so the screen
    /// fades in over it; the pending block is the state snapshot plus the
    /// bio-card prefix, and `ink_ribbon` makes the write consume one.
    fn open_typewriter_save(&mut self, pack: &Pack, ink_ribbon: bool) {
        if self.save_screen.is_some() {
            return;
        }
        self.render(pack);
        let file = self.snapshot_save(pack);
        let mut screen = ui::save_load::SaveLoadScreen::save(file, ink_ribbon);
        let mut cx = self.ui_context(pack);
        if let Err(err) = screen.open(&mut cx) {
            eprintln!("warning: save screen unavailable: {err:#}");
        }
        self.save_screen = Some(screen);
    }

    /// Capture the current game state as a save block with the bio-card
    /// prefix, when the pack carries one.
    fn snapshot_save(&self, pack: &Pack) -> save::SaveFile {
        let Ok(prefix) = pack.read(save::SAVE_PREFIX_ENTRY) else {
            return save::SaveFile::from_state(&self.game);
        };
        match save::SaveFile::from_state_with_prefix(&self.game, prefix) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("warning: invalid {0}: {err:#}", save::SAVE_PREFIX_ENTRY);
                save::SaveFile::from_state(&self.game)
            }
        }
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
        if let Some(outcome) = self
            .save_screen
            .as_mut()
            .and_then(ui::save_load::SaveLoadScreen::take_outcome)
        {
            if outcome.ink_ribbon {
                self.game.consume_ink_ribbon();
            }
            self.game.increment_saves();
        }
        self.save_screen = None;
    }

    /// One frozen tick of the pause menu. A message owns the input while it is
    /// up; the item-box overlay and the FILE tab then own the pad, and
    /// otherwise one pad edge reaches the menu and its event is consumed.
    fn tick_menu(&mut self, pack: &Pack, ui: UiInput, input: player::Input, action: bool) {
        let message_was_up = self.game.message.active;
        self.game.update_message(
            MessageInput {
                action,
                left: input.left,
                right: input.right,
            },
            &self.loaded.room,
            &self.text,
        );
        if message_was_up || self.game.message.active {
            return;
        }

        // The item-box overlay is a sub-screen of the inventory panel: it
        // freezes the room with the menu open underneath.
        if self.item_box.is_some() {
            if let Some(item_box) = self.item_box.as_mut() {
                item_box.tick();
            }
            let Some(event_input) = menu_input(ui) else {
                return;
            };
            let event = self
                .item_box
                .as_mut()
                .map(|item_box| item_box.handle_input(&mut self.game, event_input));
            if event == Some(ItemBoxEvent::Close) {
                self.item_box = None;
                self.close_menu();
                // The original restores the lid angle (state 4) once the box
                // menu is closed.
                self.game.reset_itembox();
            }
            return;
        }

        // The FILE tab returns to the inventory on close.
        if self.file.is_some() {
            if let Some(file) = self.file.as_mut() {
                file.tick();
            }
            let Some(event_input) = menu_input(ui) else {
                return;
            };
            let event = self
                .file
                .as_mut()
                .map(|file| file.handle_input(&mut self.game, event_input));
            if event == Some(FileEvent::Close) {
                self.file = None;
            }
            return;
        }

        let has_radio = self.game.has_radio();
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        menu.tick(&mut self.game);
        // START (Tab) cycles the top tabs while the menu is open; the file
        // tab opens the document selector.
        if ui.start {
            let cursor = menu.cycle_tab(&mut self.game, has_radio);
            self.handle_tab(pack, cursor);
            return;
        }
        let Some(event_input) = menu_input(ui) else {
            return;
        };
        let event = menu.handle_input(&mut self.game, event_input);
        match event {
            MenuEvent::None => {}
            MenuEvent::Close => self.close_menu(),
            MenuEvent::Message(id) => self.game.show_message(id as u8, 0),
            // CHECK opens the item viewer over the frozen menu.
            MenuEvent::ViewItem(item) => self.open_item_view(pack, item),
            MenuEvent::Tab(cursor) => self.handle_tab(pack, cursor),
            MenuEvent::Changed => {}
        }
    }

    /// Run the action of a selected top tab.
    ///
    /// The map tab (cursor 0) is out of M6 scope and inert. The file tab
    /// (cursor 2) opens the document selector/reader. The radio tab (cursor 4)
    /// uses the carried radio when its scenario flag is up: the used-item byte
    /// 0x4D reaches the room scripts and the menu closes, as the original's
    /// radio tab does; without the radio the tab is inert.
    fn handle_tab(&mut self, pack: &Pack, cursor: u8) {
        match cursor {
            // TODO(parity): (UI) the original's map tab (cursor 0) draws the
            // room map overlay; the port leaves it inert, so selecting the map
            // does nothing.
            0 => {}
            2 => self.open_file(pack),
            4 => {
                if self.game.has_radio() {
                    self.game.record_used_item(ITEM_COMM_RADIO);
                    self.close_menu();
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
        let mut item_box = ItemBox::default();
        item_box.open(&self.game);
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

    /// Advance an active transition animation one fixed frame. The finished
    /// flag is held until [`GameSession::finish_transition`] runs after the
    /// frame was presented.
    fn tick_transition(&mut self, pack: &Pack, skip: bool) {
        let Some(mut session) = self.transition.take() else {
            return;
        };
        let frame = session.transition.tick(skip);
        session.frame = frame;
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
        self.transition = Some(session);
    }

    /// Tear the finished transition down, swap the destination room in and
    /// rebuild its scripts/VMs.
    fn finish_transition(&mut self, pack: &Pack) {
        let Some(mut session) = self.transition.take() else {
            return;
        };
        // The BGM state machine needs the source room to resolve the outgoing
        // group for a same-group toggle (`g_AttractMode_RoomCameraId`).
        let from = self.loaded.id;
        finish_transition(
            pack,
            &mut session,
            &mut self.game,
            &mut self.player,
            &mut self.loaded,
            &mut self.music,
            &mut self.sfx_cache,
        );
        self.transition_finished = false;
        let scripts = Rc::new(self.loaded.scripts.clone());
        self.command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
        self.event_vm = scd::vm::EventVm::from_scripts(scripts);
        self.enter_room(pack, Some(from));
    }

    /// Render the current frame into the session framebuffer: a transition
    /// frame while one runs, the gameplay scene otherwise, with the pause
    /// menu and then the message window drawn on top. The message is painted
    /// after the scene (and outside the transition path) so it is never
    /// covered by the menu and never dimmed by a fade or door overlay.
    fn render(&mut self, pack: &Pack) {
        if let Some(transition) = &self.transition {
            render_transition(&mut self.framebuffer, transition);
            return;
        }
        // While the menu is up the room is frozen: keep the frame it was
        // opened over instead of re-rendering the scene, then draw the menu
        // and the window over it.
        if self.menu.is_none() {
            render_frame(
                &mut self.framebuffer,
                pack,
                self.loaded.id,
                &self.loaded.room,
                &self.player,
                &self.game,
                self.loaded.player_assets.as_ref(),
                &mut self.npc_models,
                &mut self.masks,
                &mut self.shadows,
                &mut self.effect_pages,
            );
        }
        if self.menu.is_some() {
            self.ensure_menu_assets(pack);
            if let (Some(menu), Some(assets)) = (&self.menu, &self.menu_assets) {
                menu.draw(&mut self.framebuffer, assets, &self.text, &self.game);
            }
        }
        if self.item_box.is_some() {
            self.ensure_item_box_assets(pack);
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
                let fade = screen.fade();
                framebuffer.fade_to_black(fade);
            }
        }
        if let Some(font) = &self.font {
            self.game
                .message
                .draw(&mut self.framebuffer, font, &self.text);
        }
    }

    /// The last rendered frame; gameplay modals draw over it.
    fn frame(&self) -> &Framebuffer {
        &self.framebuffer
    }

    /// Install a gameplay modal screen; the room freezes until it resumes.
    ///
    /// This hook is for screens that own all of their state (the item
    /// viewer); the message window and pause menu are handled explicitly in
    /// [`GameSession::tick`] because they share [`game::GameState`].
    fn open_modal(&mut self, screen: Box<dyn ui::Screen>) {
        self.modal = Some(screen);
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
        bgm::apply_live(
            &mut self.music,
            &mut self.game,
            &mut self.bgm_cache,
            pack,
            self.audio_packs.voice(),
        );
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

/// Apply the shipped new-game state: health, room-item flags, save counter
/// and starting inventory (knife, spray, and Jill's Beretta with 15 rounds).
fn seed_new_game(game: &mut game::GameState, character: u8) {
    let character = character & 1;
    game.entities[0].health = NEW_GAME_HEALTH[usize::from(character)];
    game.max_health = game::character_max_health(character);
    // `set_health_status` also mirrors the byte the scripts read with `cmpb 50`.
    game.set_health_status(0x10);
    seed_room_items(game);
    // The three carried room-pickup quantities SetInitialItems seeds
    // (BioCard 0x20C..0x20E): ROOM1160's shotgun shells and the two
    // flamethrower rooms' fuel.
    game.state_bytes[0x0C] = NEW_GAME_PICKUP_QUANTITIES[0];
    game.state_bytes[0x0D] = NEW_GAME_PICKUP_QUANTITIES[1];
    game.state_bytes[0x0E] = NEW_GAME_PICKUP_QUANTITIES[2];
    game.state_bytes[usize::from(game::STATE_BYTE_SAVES)] = 0;
    game.state_bytes[0x24..0x28].fill(0);
    game.add_item(ITEM_KNIFE, 0);
    if character == 1 {
        game.add_item(ITEM_BERETTA, 15);
    }
    game.add_item(ITEM_FIRST_AID_SPRAY, 1);
}

/// Which screen `--ui` boots or the app opens first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppBoot {
    /// The title screen.
    Title,
    /// The character-selection screen.
    CharSelect,
    /// The load screen.
    SaveLoad,
    /// The typewriter save screen over the deterministic capture room.
    Save,
    /// A new game as the character (`0` Chris, `1` Jill).
    NewGame(u8),
    /// The pause menu over the deterministic capture room.
    Menu,
    /// The item box over the deterministic capture room.
    ItemBox,
    /// The FILE tab over the deterministic capture room.
    File,
    /// The item viewer over the deterministic capture room's combat knife.
    View,
}

/// The room `--ui menu` and its capture boot into, with the known capture
/// inventory added on top of the room boot.
pub const MENU_ROOM: &str = "1001";

/// Fixed ticks the headless menu capture settles before the frame is drawn.
const MENU_CAPTURE_TICKS: u32 = 30;
/// Fixed ticks the headless item-box capture settles before the frame.
const ITEM_BOX_CAPTURE_TICKS: u32 = 30;
/// Fixed ticks the headless FILE capture settles before the frame.
const FILE_CAPTURE_TICKS: u32 = 30;
/// Fixed ticks the headless save-screen capture settles before the frame.
const SAVE_CAPTURE_TICKS: u32 = 36;

/// The app's current screen.
enum Mode {
    Title(ui::title::TitleScreen),
    Select(ui::char_select::CharSelectScreen),
    Load(Box<ui::save_load::SaveLoadScreen>),
    Play(Box<GameSession>),
}

/// The mode machine: one screen at a time, with the pack and decoded shared
/// assets owned here.
struct App {
    pack: Pack,
    save_dir: PathBuf,
    /// The optional voice pack path, attached to every gameplay session.
    voice_path: Option<PathBuf>,
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
    /// One-shot cache for the UI cue sounds (`se/cursor.wav`, ...).
    ui_sfx_cache: SfxCache,
}

/// How one app tick ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppFlow {
    Continue,
    Quit,
}

impl App {
    /// Open the pack's shared assets; the mode machine starts on the title.
    /// `audio` opens the mixer when a game session starts.
    fn new(pack: Pack, save_dir: PathBuf, voice_path: Option<PathBuf>, audio: bool) -> Self {
        let font = match pack.read("font/font.tim") {
            Ok(bytes) => match tim::decode_4bpp(bytes) {
                Ok(texture) => Some(font::Font::new(texture)),
                Err(err) => {
                    eprintln!("warning: invalid font/font.tim: {err:#}");
                    None
                }
            },
            Err(_) => None,
        };
        let text = Text::load(&pack);
        Self {
            pack,
            save_dir,
            voice_path,
            font,
            text,
            mode: Mode::Title(ui::title::TitleScreen::new()),
            framebuffer: Framebuffer::new(),
            ticks: 0,
            audio,
            ui_music: None,
            ui_sfx_cache: SfxCache::default(),
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
            AppBoot::Title => self.open_title(),
            AppBoot::CharSelect => self.open_select(),
            AppBoot::SaveLoad => self.open_load(),
            AppBoot::Save => self.open_save_capture(),
            AppBoot::NewGame(character) => {
                let session = GameSession::new(&self.pack, character, &self.save_dir)?;
                self.start_session(session);
                Ok(())
            }
            AppBoot::Menu => self.open_menu(),
            AppBoot::ItemBox => self.open_item_box_capture(),
            AppBoot::File => self.open_file_capture(),
            AppBoot::View => self.open_view(),
        }
    }

    /// Open the title screen.
    fn open_title(&mut self) -> Result<()> {
        let mut screen = ui::title::TitleScreen::new();
        screen.open(&mut self.context())?;
        self.mode = Mode::Title(screen);
        Ok(())
    }

    /// Open the character-selection screen.
    fn open_select(&mut self) -> Result<()> {
        let mut screen = ui::char_select::CharSelectScreen::new();
        screen.open(&mut self.context())?;
        self.mode = Mode::Select(screen);
        Ok(())
    }

    /// Open the load screen.
    fn open_load(&mut self) -> Result<()> {
        let mut screen = ui::save_load::SaveLoadScreen::load();
        screen.open(&mut self.context())?;
        self.mode = Mode::Load(Box::new(screen));
        Ok(())
    }

    /// Enter a gameplay session, opening audio on the interactive path.
    fn start_session(&mut self, mut session: GameSession) {
        session.audio_packs = AudioPacks::new(self.voice_path.clone());
        if self.audio {
            session.start_audio(&self.pack);
        }
        self.mode = Mode::Play(Box::new(session));
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

    /// Apply a completed screen action.
    fn apply(&mut self, action: ScreenAction) -> Result<AppFlow> {
        match action {
            ScreenAction::CharSelect => self.open_select()?,
            ScreenAction::SaveLoad => self.open_load()?,
            ScreenAction::NewGame { character } => {
                let session = GameSession::new(&self.pack, character, &self.save_dir)?;
                self.start_session(session);
            }
            ScreenAction::LoadGame { slot } => {
                let file = save::load(&self.save_dir, slot)?;
                let session = GameSession::from_save(&self.pack, &file, &self.save_dir)?;
                self.start_session(session);
            }
            ScreenAction::Title => self.open_title()?,
            ScreenAction::Resume => {
                if let Mode::Play(session) = &mut self.mode {
                    session.close_modal();
                }
            }
            ScreenAction::Quit => return Ok(AppFlow::Quit),
        }
        Ok(AppFlow::Continue)
    }

    /// One app tick: advance the active screen, or the room when playing.
    fn update(&mut self, ui: UiInput, input: player::Input, action: bool) -> Result<AppFlow> {
        self.ticks = self.ticks.saturating_add(1);
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
                Mode::Play(_) => ScreenResult::Continue,
            };
            (result, cx.cues.into_inner())
        };
        self.play_ui_cues(cues);
        result
    }

    /// Tick the gameplay session or its modal.
    fn play_update(&mut self, ui: UiInput, input: player::Input, action: bool) -> Result<AppFlow> {
        let mut cues: Vec<ui::UiCue> = Vec::new();
        {
            let App {
                pack,
                save_dir,
                font,
                text,
                mode,
                ticks,
                ..
            } = self;
            let Mode::Play(session) = mode else {
                return Ok(AppFlow::Continue);
            };
            if let Some(modal) = session.modal.as_mut() {
                let cx = UiContext {
                    pack,
                    save_dir,
                    font: font.as_ref(),
                    text: Some(text),
                    ticks: *ticks,
                    cues: std::cell::RefCell::new(Vec::new()),
                };
                match modal.update(&cx, ui) {
                    ScreenResult::Done(ScreenAction::Resume) => session.close_modal(),
                    ScreenResult::Done(ScreenAction::Quit) => return Ok(AppFlow::Quit),
                    _ => {}
                }
                cues = cx.cues.into_inner();
            } else if !session.transition_finished {
                if session.transition.is_some() {
                    session.tick_transition(pack, action || input.run);
                } else {
                    session.tick(pack, ui, input, action)?;
                }
            }
        }
        self.play_ui_cues(cues);
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
                framebuffer.fade_to_black(screen.fade());
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
                framebuffer.fade_to_black(screen.fade());
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
                framebuffer.fade_to_black(screen.fade());
            }
            Mode::Play(session) => {
                if session.modal.is_none() {
                    session.render(pack);
                }
                framebuffer.copy_from(session.frame());
                if let Some(modal) = session.modal.as_mut() {
                    let cx = UiContext {
                        pack,
                        save_dir,
                        font: font.as_ref(),
                        text: Some(text),
                        ticks: *ticks,
                        cues: std::cell::RefCell::new(Vec::new()),
                    };
                    modal.draw(&cx, framebuffer);
                    let fade = modal.fade();
                    framebuffer.fade_to_black(fade);
                }
            }
        }
    }

    /// The app framebuffer.
    fn frame(&self) -> &Framebuffer {
        &self.framebuffer
    }

    /// Step the camera cut when playing.
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

    /// After presenting: tear down a finished transition and feed the mixer.
    fn post_present(&mut self) {
        if let Mode::Play(session) = &mut self.mode {
            if session.transition_finished {
                session.finish_transition(&self.pack);
            }
            session.update_audio();
        }
        // The UI screens' cue mixer streams independently of the gameplay one.
        if let Some(mixer) = &mut self.ui_music {
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
        bmp::encode(
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
/// over [`MENU_ROOM`] with the deterministic capture inventory, and `view`
/// boots the item viewer over that same inventory with the combat knife
/// examined. `capture` renders one deterministic frame and exits; otherwise
/// the window stays up until the user quits.
pub fn run_ui(pack: &Path, screen: &str, capture: Option<&Path>) -> Result<()> {
    let save_dir = save::default_save_dir_for_pack(pack);
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
    run_ui_with_voice(pack, screen, capture, save_dir, character, None)
}

/// [`run_ui_with_options`] with an explicit voice-pack path.
pub fn run_ui_with_voice(
    pack: &Path,
    screen: &str,
    capture: Option<&Path>,
    save_dir: &Path,
    character: u8,
    voice: Option<&Path>,
) -> Result<()> {
    let voice_path = voice_pack_path(pack, voice);
    let boot = match screen {
        "font" => return run_font_ui(pack, capture),
        "title" => AppBoot::Title,
        "select" => AppBoot::CharSelect,
        "load" => AppBoot::SaveLoad,
        "save" => AppBoot::Save,
        "game" => AppBoot::NewGame(character & 1),
        "menu" => AppBoot::Menu,
        "box" | "itembox" => AppBoot::ItemBox,
        "file" => AppBoot::File,
        "view" => AppBoot::View,
        other => {
            bail!(
                "unknown --ui screen `{other}`; expected `font`, `title`, `select`, `game`, \
                 `menu`, `box`, `file`, `view`, `save` or `load`"
            )
        }
    };

    if let Some(capture_path) = capture {
        let pack = Pack::open(pack)?;
        let mut app = App::new(pack, save_dir.to_path_buf(), voice_path.clone(), false);
        match boot {
            AppBoot::Title => {
                // Reach the option menu, then let the fade settle.
                app.open_title()?;
                app.update(
                    UiInput {
                        any: true,
                        ..UiInput::default()
                    },
                    player::Input::default(),
                    false,
                )?;
                app.settle(48)?;
            }
            AppBoot::CharSelect => {
                app.open_select()?;
                app.settle(48)?;
            }
            AppBoot::SaveLoad => {
                app.open_load()?;
                app.settle(48)?;
            }
            AppBoot::Save => {
                app.open_save_capture()?;
                app.settle(SAVE_CAPTURE_TICKS)?;
            }
            AppBoot::NewGame(character) => {
                let session = GameSession::new(&app.pack, character, &app.save_dir)?;
                app.start_session(session);
            }
            AppBoot::Menu => {
                app.open_menu()?;
                app.settle(MENU_CAPTURE_TICKS)?;
            }
            AppBoot::ItemBox => {
                app.open_item_box_capture()?;
                app.settle(ITEM_BOX_CAPTURE_TICKS)?;
            }
            AppBoot::File => {
                app.open_file_capture()?;
                app.settle(FILE_CAPTURE_TICKS)?;
            }
            AppBoot::View => {
                app.open_view()?;
                app.settle(MENU_CAPTURE_TICKS)?;
            }
        }
        app.draw();
        return app.capture(capture_path);
    }

    let pack = Pack::open(pack)?;
    let mut app = App::new(pack, save_dir.to_path_buf(), voice_path, true);
    app.boot(boot)?;
    let display = Display::new("Arklay", false)?;
    let mut input = InputState::default();
    let mut event = SDL_Event::default();
    let mut last_ticks = unsafe { SDL_GetTicks() };
    let mut accumulator = 0.0f64;
    loop {
        let mut cut_delta = 0i32;
        if poll_events(&mut event, &mut input, &mut cut_delta) {
            return Ok(());
        }
        if cut_delta != 0 {
            app.step_camera_cut(cut_delta);
        }
        app.update_window_title(display.window)?;

        let now = unsafe { SDL_GetTicks() };
        accumulator += now.saturating_sub(last_ticks) as f64;
        last_ticks = now;
        if accumulator > 250.0 {
            accumulator = 250.0;
        }
        while accumulator >= TICK_MS {
            let tick = input.tick();
            if let AppFlow::Quit = app.update(tick.ui, tick.player, tick.action)? {
                return Ok(());
            }
            accumulator -= TICK_MS;
        }
        app.draw();
        display.present(app.frame())?;
        display.show()?;
        app.post_present();
    }
}

/// The `--ui font` screen.
fn run_font_ui(pack_path: &Path, capture: Option<&Path>) -> Result<()> {
    let pack = Pack::open(pack_path)?;
    let bytes = pack
        .read("font/font.tim")
        .context("the pack has no font/font.tim (re-run convert-game)")?;
    let texture = tim::decode_4bpp(bytes).context("failed to decode font/font.tim")?;
    let font = font::Font::new(texture);

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
        if poll_events(&mut event, &mut input, &mut cut_delta) {
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
        );
    }
    font.draw_text(
        framebuffer,
        margin,
        156,
        font::Tint::White,
        2,
        SAMPLE_SYMBOLS,
    );
    font.draw_text(
        framebuffer,
        margin,
        172,
        font::Tint::White,
        2,
        SAMPLE_KANJI_TOP,
    );
    font.draw_text(
        framebuffer,
        margin,
        188,
        font::Tint::White,
        2,
        SAMPLE_KANJI_BOTTOM,
    );
    font.draw_text(
        framebuffer,
        margin,
        204,
        font::Tint::White,
        2,
        SAMPLE_SPACING,
    );
    font.draw_text(framebuffer, margin, 222, font::Tint::White, 15, SAMPLE_TEXT);
}

/// Digits, letters, the remapped parentheses and an end byte.
const SAMPLE_TEXT: &[u8] = &[
    0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x00, 0x1D, 0x1E, 0x1F, 0x20, 0x21,
    0x22, 0x23, 0x24, 0x00, 0x28, 0x00, 0x29, 0x01,
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
/// The destination room is loaded when the transition starts so it is ready
/// behind the leading black frames; [`finish_transition`] swaps it in and
/// places the player when the timeline finishes.
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
    /// Destination room loaded behind the black frames.
    destination: Option<LoadedRoom>,
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
            let frame = vm.step();
            self.sfx.extend(frame.sfx.iter().copied());
            self.messages.extend(frame.messages.iter().copied());
            return transition::DoorFrame {
                camera: transition::Camera {
                    from: frame.camera.from,
                    to: frame.camera.to,
                    focal: frame.camera.focal,
                },
                fade_type: frame.fade.fade_type,
                fade_counter: frame.fade.counter,
                fade_state: frame.fade.state,
                done: frame.done,
                phase: frame.phase,
                hold: frame.hold,
                black: frame.black,
                sound_busy: vm.sound_busy(),
            };
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
    Ok(TransitionMode {
        transition: transition::Transition::new(animation),
        door: *record,
        target: transition.target,
        camera_only,
        silent,
        destination,
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
    let dor = match door::parse(bytes) {
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

/// Tear the transition down: play the door's close SE (unless suppressed),
/// swap in the destination room and place the player.
///
/// The BGM handoff runs later, in [`GameSession::enter_room`], after the
/// destination's init script has had its say (the original's ordering).
fn finish_transition(
    pack: &Pack,
    session: &mut TransitionMode,
    game: &mut game::GameState,
    player_state: &mut player::PlayerState,
    loaded: &mut LoadedRoom,
    music: &mut Option<Mixer>,
    sfx_cache: &mut SfxCache,
) {
    // The original's `room_transition_load` latches the record's sfx byte and
    // reloads `g_RoomSfxBanks` before the destination's init runs; camera-only
    // records reload the pair too, even though the room stays.
    if session.camera_only {
        loaded.room.room_sfx = session.door.sfx;
        if !session.silent {
            play_room_sfx(music, sfx_cache, pack, session.door.sfx, 1);
        }
        // The original re-tests the current camera's switch zones in place
        // (room_transition_load's camera-only branch calls
        // check_camera_switch(1)); the record's entry camera only feeds the
        // .dor animation and is not itself a room cut.
        apply_camera(&mut loaded.room, game, Some(player_state.pos));
        return;
    }

    let Some(destination) = session.destination.take() else {
        return;
    };
    *loaded = destination;
    loaded.room.room_sfx = session.door.sfx;
    game.enter_room(session.target, &loaded.room);
    *player_state = player::spawn(session.target, &loaded.room);
    player_state.pos = session.door.next_pos;
    player_state.angle = session.door.next_angle as u16 & 0x0FFF;
    // `enter_room` clears the source room's stair state and `spawn` starts the
    // fresh player with a clean climb, so the destination cannot inherit a
    // suspended collision pass.
    // A door arrival can sit inside a collision volume the collision pass
    // cannot clear. The original places the record's point raw and lets the
    // next frame's collision pass push it out; this engine models the
    // stair/ladder climb (which suspends that pass) only while the animation
    // runs, so a wedged arrival is still moved clear as a fallback.
    let raw = player_state.pos;
    player_state.pos = player::free_spawn(
        &loaded.room,
        player_state.pos,
        player_state.angle,
        player_state.radius,
    );
    if player_state.pos != raw {
        eprintln!(
            "warning: door spawn {raw:?} is inside collision; placed at {:?}",
            player_state.pos
        );
    }
    game.sync_entity_from_player(player_state);

    // A freshly loaded room starts at cut 0. The switch-zone scan runs in the
    // gameplay phase after the destination's init (the original's room_set
    // runs the init script before check_camera_switch), because the init can
    // move the player and lock the camera. The record's entry camera only
    // feeds the .dor animation, so it must not seed the room camera.
    loaded.room.current_cut = 0;
    game.camera.current_cut = 0;

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

    let mut session = start_transition(pack, &record, &request)?;
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
    finish_transition(
        pack,
        &mut session,
        &mut game,
        &mut player_state,
        &mut loaded,
        &mut mixer,
        &mut sfx_cache,
    );

    // The engine runs the destination's init when gameplay resumes; run it
    // here too so the captured frame matches the first rendered frame (mask
    // groups, camera locks and scripted entity moves included).
    run_room_init(&mut loaded, &mut game);
    game.sync_player(&mut player_state);
    drain_mask_toggles(&mut loaded.room, &mut game);
    apply_camera(&mut loaded.room, &mut game, Some(player_state.pos));

    // The destination's first gameplay frame, drawn from the placed player and
    // the zone-selected cut, exactly as the engine's render loop would.
    let mut gameplay = Framebuffer::new();
    let mut npc_models = npc::EntityModelCache::default();
    render_frame(
        &mut gameplay,
        pack,
        loaded.id,
        &loaded.room,
        &player_state,
        &game,
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

/// Render one gameplay frame from an already-built state.
///
/// This is the headless capture seam for effect probes: tests can tick a state
/// by hand (create effects, step [`game::GameState::tick_effects`]) and render
/// it without rebuilding the room. Player and NPC models are loaded from the
/// pack when present.
pub fn render_game_frame(
    pack: &Pack,
    id: RoomId,
    room: &RoomState,
    game: &game::GameState,
    player_state: &player::PlayerState,
) -> Result<Image> {
    let assets = load_player_assets(pack, id);
    let mut npc_models = npc::EntityModelCache::default();
    let mut framebuffer = Framebuffer::new();
    render_frame(
        &mut framebuffer,
        pack,
        id,
        room,
        player_state,
        game,
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

/// Drive the new-game start (stage 1 room 0) headlessly: the original's start
/// position, facing and seed, then the same init/ticks/render path as
/// [`simulate_room`]. `character` selects Chris (0) or Jill (1).
pub fn simulate_new_game(
    pack: &Pack,
    character: u8,
    ticks: usize,
    input: player::Input,
) -> Result<SimulatedRoom> {
    let character = character & 1;
    let id = RoomId {
        stage: 1,
        room: 0,
        player_flag: character,
    };
    let loaded = load_room(pack, id)?;
    let mut game = new_game_state(pack, id, &loaded.room);
    seed_new_game(&mut game, character);
    let mut player_state = player::spawn(id, &loaded.room);
    player_state.pos = [NEW_GAME_POS_X, 0, NEW_GAME_POS_Z];
    player_state.angle = NEW_GAME_ANGLE;
    game.sync_entity_from_player(&player_state);
    simulate_loaded(pack, loaded, game, player_state, ticks, move |_| input)
}

/// The shared body of [`simulate_room`] and [`simulate_new_game`].
fn simulate_loaded(
    pack: &Pack,
    mut loaded: LoadedRoom,
    mut game: game::GameState,
    mut player_state: player::PlayerState,
    ticks: usize,
    mut input: impl FnMut(usize) -> player::Input,
) -> Result<SimulatedRoom> {
    let id = loaded.id;
    run_room_init(&mut loaded, &mut game);
    drain_mask_toggles(&mut loaded.room, &mut game);
    apply_camera(&mut loaded.room, &mut game, Some(player_state.pos));
    // Match the engine's room boot: the per-room BGM state machine runs after
    // the init script, so a headless run sees the same channel state.
    bgm::update_room_bgm(&mut game, id, None);

    let scripts = Rc::new(loaded.scripts.clone());
    let mut command_vm = scd::vm::CommandVm::from_scripts(Rc::clone(&scripts));
    let mut event_vm = scd::vm::EventVm::from_scripts(scripts);
    let mut masks = MaskCache::default();
    let mut shadows = ShadowCache::default();
    let mut effect_pages = EffectPageCache::default();
    let mut npc_models = npc::EntityModelCache::default();
    let mut framebuffer = Framebuffer::new();
    // Headless runs have no device: a voice wait clears the tick it is raised,
    // but the BGM state machine, its ramps/fades and the 3D SE dispatch still
    // run after the scripts exactly like the interactive tick.
    let mut voice_cache = VoiceCache::default();
    let mut bgm_cache = bgm::BgmCache::default();
    let mut snd3d_cache = SfxCache::default();
    let mut no_mixer: Option<Mixer> = None;

    for tick in 0..ticks {
        tick_room(
            &mut command_vm,
            &mut event_vm,
            RoomContext {
                room: &mut loaded.room,
                game: &mut game,
                player: &mut player_state,
                player_assets: loaded.player_assets.as_ref(),
                pack,
                npc_models: &mut npc_models,
            },
            input(tick),
        );
        play_snd3d_requests(
            &mut no_mixer,
            &mut snd3d_cache,
            pack,
            &loaded.room,
            &mut game,
        );
        bgm::apply_live(&mut no_mixer, &mut game, &mut bgm_cache, pack, None);
        tick_voice(&mut no_mixer, &mut voice_cache, &mut game, pack, None);
        // Headless runs feed no input; release a message's menu choice the way
        // an auto-confirm would so a script's F7 cannot stall on it.
        let choice = game.message.menu_choice_id();
        game.message.set_menu_choice_id(choice & 0x7F);
        drain_mask_toggles(&mut loaded.room, &mut game);
    }

    render_frame(
        &mut framebuffer,
        pack,
        id,
        &loaded.room,
        &player_state,
        &game,
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
        id,
        &loaded.room,
        &player_state,
        &game,
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
        id,
        room: loaded.room,
        game,
        player: player_state,
        frame,
        baseline,
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
pub fn simulate_typewriter(
    pack: &Pack,
    id: RoomId,
    save_dir: &Path,
) -> Result<SimulatedTypewriter> {
    let mut session = GameSession::from_room(pack, id, save_dir)?;
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
    let (dx, dz) = player::reach_offset(0);
    let typewriter_pos = [center_x - dx, session.player.pos[1], center_z - dz];

    // Probe the typewriter, dismiss any startup message, page the prompt and
    // answer its yes/no choice. Action presses alternate with idle ticks so
    // the window and the probe see a fresh edge; the player is re-placed every
    // tick in case a script keeps moving them.
    let mut prompted = false;
    let mut pressed = true;
    for _ in 0..40_000 {
        if session.save_screen.is_some() {
            prompted = true;
            break;
        }
        session.player.pos = typewriter_pos;
        session.player.angle = 0;
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

/// Apply the mask-group toggles the scripts queued to the current cut.
fn drain_mask_toggles(room: &mut RoomState, game: &mut game::GameState) {
    let Some(cut) = room.cuts.get_mut(room.current_cut) else {
        game.mask_toggles.clear();
        return;
    };
    for toggle in game.mask_toggles.drain(..) {
        mask::set_group_active(&mut cut.mask_active, toggle.group, toggle.active);
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
        mixer.play_sfx(wav, 1.0, 0.0);
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
        mixer.play_sfx(wav, 1.0, 0.0);
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
        mixer.play_sfx(wav, play.gain, play.pan);
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
        let (gain, pan) = sfx::sound_gain_pan(cut.pos, cut.look_at, footstep.pos);
        mixer.play_sfx(wav, gain, pan);
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
            let (gain, pan) = sfx::sound_gain_pan(cut.pos, cut.look_at, sound.pos);
            mixer.play_sfx(wav, gain, pan);
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
        mixer.play_sfx(wav, play.gain, play.pan);
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
        mixer.play_sfx(wav, gain, pan);
    }
}

/// Poll every queued SDL event into `input`, reporting whether the user quit
/// and accumulating the shift+`,`/`.` camera-cut step.
///
/// Keyboard transitions are latched here rather than sampled from
/// `SDL_GetKeyboardState` later, because the render loop runs at the display's
/// refresh rate while the simulation runs at a fixed 30 Hz: a press that lands
/// between two ticks must still be there when the next tick asks.
fn poll_events(event: &mut SDL_Event, input: &mut InputState, cut_delta: &mut i32) -> bool {
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
            input.key_down(scancode, repeat);
            if key == SDLK_ESCAPE {
                quit = true;
            } else if modifiers & SDL_KMOD_SHIFT != SDL_KMOD_NONE {
                if key == SDLK_COMMA {
                    *cut_delta -= 1;
                } else if key == SDLK_PERIOD {
                    *cut_delta += 1;
                }
            }
        } else if kind == SDL_EVENT_KEY_UP {
            input.key_up(unsafe { event.key.scancode });
        } else if kind == SDL_EVENT_WINDOW_FOCUS_LOST || kind == SDL_EVENT_KEYBOARD_REMOVED {
            // Focusing away (or unplugging the keyboard) never delivers the
            // held keys' key-up events; drop them so nothing sticks down.
            input.clear();
        }
    }
    quit
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
        let path = format!("se/{}.wav", name.to_ascii_lowercase());
        if let Some(wav) = self.wavs.get(&path) {
            return Some(wav.clone());
        }
        if self.missing.contains(&path) {
            return None;
        }
        let bytes = match pack.read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("warning: missing sound effect {path}: {err}");
                self.missing.insert(path);
                return None;
            }
        };
        match audio::parse_wav(bytes) {
            Ok(wav) => {
                self.wavs.insert(path, wav.clone());
                Some(wav)
            }
            Err(err) => {
                eprintln!("warning: invalid sound effect {path}: {err:#}");
                self.missing.insert(path);
                None
            }
        }
    }
}

/// The main pack plus the optional voice pack, opened lazily on the first
/// voice or BGM read. A run without a voice pack never touches the file.
#[derive(Default)]
struct AudioPacks {
    path: Option<PathBuf>,
    pack: Option<Pack>,
    attempted: bool,
}

impl AudioPacks {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            pack: None,
            attempted: false,
        }
    }

    /// The voice pack, opening it on first use. A failure is reported once and
    /// leaves the fallback read absent.
    fn voice(&mut self) -> Option<&Pack> {
        if !self.attempted {
            self.attempted = true;
            if let Some(path) = &self.path {
                match Pack::open(path) {
                    Ok(pack) => self.pack = Some(pack),
                    Err(err) => eprintln!(
                        "warning: failed to open voice pack {}: {err:#}",
                        path.display()
                    ),
                }
            }
        }
        self.pack.as_ref()
    }
}

/// The discovered or explicit voice pack: `<pack stem>.voice.akpak` beside the
/// main pack.
fn voice_pack_path(main: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Some(path.to_path_buf());
        }
        eprintln!(
            "warning: voice pack {} does not exist; voice lines will be skipped",
            path.display()
        );
        return None;
    }
    let stem = main.file_stem()?.to_str()?;
    let candidate = main.with_file_name(format!("{stem}.voice.akpak"));
    candidate.is_file().then_some(candidate)
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
    /// Load and parse `voice/{name}.wav`, falling back to the voice pack.
    fn load(&mut self, pack: &Pack, auxiliary: Option<&Pack>, name: &str) -> Option<audio::Wav> {
        let path = voice::pack_path(name);
        if let Some(wav) = self.wavs.get(&path) {
            return Some(wav.clone());
        }
        if self.missing.contains(&path) {
            return None;
        }
        let bytes = match pack.read(&path) {
            Ok(bytes) => Some(bytes),
            Err(_) => auxiliary.and_then(|auxiliary| auxiliary.read(&path).ok()),
        };
        let Some(bytes) = bytes else {
            eprintln!("warning: missing voice line {path}");
            self.missing.insert(path);
            return None;
        };
        match audio::parse_wav(bytes) {
            Ok(wav) => {
                self.wavs.insert(path, wav.clone());
                Some(wav)
            }
            Err(err) => {
                eprintln!("warning: invalid voice line {path}: {err:#}");
                self.missing.insert(path);
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
/// hanging on F7. A name that resolves but cannot be read clears the flag the
/// same way (the documented hardening).
fn tick_voice(
    music: &mut Option<Mixer>,
    cache: &mut VoiceCache,
    game: &mut game::GameState,
    pack: &Pack,
    auxiliary: Option<&Pack>,
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
        match cache.load(pack, auxiliary, request.name) {
            Some(wav) => {
                let gain = sfx::volume_gain(request.volume);
                let pan = sfx::pan_from_raw(request.pan);
                mixer.play_voice(wav, gain, pan);
                cache.active = true;
                game.set_voice_playing();
            }
            None => {
                // The line resolved by name but its file is absent: count the
                // miss and release the wait so the script cannot deadlock.
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
    /// The mask page of `camera`, loading `roommask/{room3}_{camera:03}.bmp`
    /// from the pack on first use. Missing pages are remembered as absent.
    fn page_for(&mut self, pack: &Pack, id: RoomId, camera: usize) -> Option<&Image> {
        if self.room != Some(id) {
            self.room = Some(id);
            self.pages.clear();
            self.missing.clear();
        }
        if !self.pages.contains_key(&camera) && !self.missing.contains(&camera) {
            let entry = id.roommask_entry(camera);
            match pack.read(&entry) {
                Ok(bytes) => match bmp::decode_mask(bytes) {
                    Ok(image) => {
                        self.pages.insert(camera, image);
                    }
                    Err(err) => {
                        eprintln!("warning: invalid room mask {entry}: {err:#}");
                        self.missing.insert(camera);
                    }
                },
                Err(_) => {
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
    /// The baked shadow page, loading `shadow/kage.tim` on first use.
    fn page_for(&mut self, pack: &Pack) -> Option<&Image> {
        if !self.attempted {
            self.attempted = true;
            if let Ok(bytes) = pack.read(shadow::KAGE_ENTRY) {
                match shadow::decode(bytes) {
                    Ok(image) => self.page = Some(image),
                    Err(err) => {
                        eprintln!("warning: invalid {}: {err:#}", shadow::KAGE_ENTRY);
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

/// Convert a decoded indexed sheet into an RGBA page with texture entry zero
/// transparent.
fn effect_page_image(texture: &crate::model::Texture8) -> Image {
    let mut rgba = Vec::with_capacity(texture.indices.len() * 4);
    for &index in &texture.indices {
        if index == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let texel = texture.palette(0, index);
            rgba.extend_from_slice(&[texel[0], texel[1], texel[2], 255]);
        }
    }
    Image {
        width: texture.width,
        height: texture.height,
        rgba,
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
        let path = format!("effspr/{name}.tim");
        let Ok(bytes) = pack.read(&path) else {
            continue;
        };
        match tim::decode_8bpp(bytes) {
            Ok(texture) => pages.base[page] = Some(effect_page_image(&texture)),
            Err(err) => eprintln!("warning: invalid effect page {path}: {err:#}"),
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
/// table.
fn build_effect_quads(
    game: &game::GameState,
    room: &RoomState,
    id: RoomId,
    camera: &Camera,
    pages: &EffectPages,
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
        // turns it into a top-left distance with `0x80 - pivot`.
        let pivot_x = 0x80 - i32::from(effect.uv[2]);
        let pivot_y = 0x80 - i32::from(effect.uv[3]);
        let left = i32::from(effect.screen[0]) - sar12(pivot_x * scale);
        let top = i32::from(effect.screen[1]) - sar12(pivot_y * scale);
        let right = i32::from(effect.screen[0]) + sar12((width as i32 - pivot_x) * scale);
        let bottom = i32::from(effect.screen[1]) + sar12((height as i32 - pivot_y) * scale);

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

        quads.push(EffectQuad {
            key,
            page: quad_page,
            uv: [effect.uv[0], tex_v, effect.uv[2], effect.uv[3]],
            source: effect.size,
            pos: [left, top],
            size: [right - left, bottom - top],
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

/// Everything loaded for one room, so a transition can load the next room with
/// the same code path as the initial load.
struct LoadedRoom {
    id: RoomId,
    room: RoomState,
    scripts: scd::ir::Scripts,
    player_assets: Option<PlayerAssets>,
}

/// Load `id` from `pack`: RDT, camera backgrounds, SCD scripts and player
/// assets.
fn load_room(pack: &Pack, id: RoomId) -> Result<LoadedRoom> {
    let rdt_bytes = pack.read(&id.rdt_entry())?;
    let mut room = rdt::parse(rdt_bytes, id)?;
    for warning in &room.model_warnings {
        eprintln!("warning: {}: {warning}", id.rdt_entry());
    }
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
    Ok(LoadedRoom {
        id,
        room,
        scripts,
        player_assets,
    })
}

/// Build a fresh game state for `id`/`room` with the pack's global weapon-FX
/// metadata installed, so script effect spawns resolve both the room's
/// declared sprites and the `core00` types.
fn new_game_state(pack: &Pack, id: RoomId, room: &RoomState) -> game::GameState {
    let mut game = game::GameState::new(id, room);
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

/// Start every event script requested by `evt_exec` in the command scripts.
fn start_pending_events(game: &mut game::GameState, event_vm: &mut scd::vm::EventVm) {
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
    /// Pack the NPC model cache reads from.
    pack: &'a Pack,
    /// Parsed NPC models, shared with the renderer.
    npc_models: &'a mut npc::EntityModelCache,
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
    // The original zeroes the per-frame item-use flag bank at the top of every
    // game frame, before the room scripts decide what is usable this frame.
    context.game.clear_item_use_flags();
    // The desk flow and the item-box lid ramp before the scripts run (the
    // original's `check_desk_state`/`check_itembox_state` are the first
    // gameplay calls each frame).
    context.game.check_desk_state();
    context.game.check_itembox_state();
    {
        let mut host = game::ScdGameHost::new(context.game);
        command_vm.run_main(&mut host);
    }
    start_pending_events(context.game, event_vm);
    {
        let mut host = game::ScdGameHost::new(context.game);
        event_vm.step(&mut host);
    }
    // Scripted room writes (`inst_cfg` collision boxes, `obj_xfm` lights)
    // become visible here, before the player's physics and before the
    // renderer reads the lighting.
    context.game.apply_room_edits(context.room);
    // The scripted characters think after the event scripts and before the
    // player's physics, exactly like the original's `update_entities`.
    context
        .game
        .tick_entities(context.room, context.npc_models, context.pack);
    // Scripts may have moved the player entity directly (dir_set, actor
    // motion); mirror that onto the visible player before physics run.
    context.game.sync_player(context.player);
    context.game.advance_frame();
    if let Some(assets) = context.player_assets {
        let room_clips = context
            .room
            .room_anim
            .as_ref()
            .map(|anim| anim.clips.as_slice())
            .unwrap_or(&[]);
        player::update_with_room(
            context.player,
            context.room,
            &assets.emd.clips,
            &assets.emw.clips,
            room_clips,
            input,
        );
    }
    context.game.sync_entity_from_player(context.player);
    // The effects run after the player's physics and before the action probe,
    // so a script that spawns this frame animates this frame and a dust effect
    // the probe spawns does too.
    context.game.tick_effects(context.room);
    // The original runs the room action probe after the player's movement, so
    // `stairs_height_update` measures the frame's final position and the climb
    // behaviour starts from where the player actually is. The action-key
    // entries (bit 0x80, including `set_stairs_zone`) belong to the input path:
    // a locked action behaviour does not read a new press, so the probe sees
    // only the every-frame entries while the vault/push/ladder owns the tick.
    {
        let mut host = game::ScdGameHost::new(context.game);
        let probe_action =
            input.action_pressed && context.player.locked == player::LockedAction::None;
        host.interact(context.player.pos, context.player.angle, probe_action);
    }
    // The room objects run after the player's physics and the player-side
    // probe: the push probe, the object-side action probe and the climb scan
    // all read the frame's final player state.
    context.game.tick_objects(context.room, context.player);
    context.game.apply_stair_state(context.player);
    // The climb's camera-scroll consumer is not ported; drop the queued
    // screen-effect rectangles each tick so the list cannot grow unbounded.
    context.player.take_screen_effects();
    // The effects projected above under the pre-switch camera. If the zone
    // scan moved the cut, recompute their stored screen/depth so the frame
    // renders billboards against the camera it draws; without this the first
    // frame after a switch shows the old projection.
    let previous_cut = context.room.current_cut;
    apply_camera(context.room, context.game, Some(context.player.pos));
    if context.room.current_cut != previous_cut {
        context.game.reproject_effects(context.room);
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
    game.enter_room(transition.target, &loaded.room);
    *player_state = player::spawn(transition.target, &loaded.room);
    player_state.pos = transition.pos;
    player_state.angle = transition.angle;
    player_state.pos = player::free_spawn(
        &loaded.room,
        player_state.pos,
        player_state.angle,
        player_state.radius,
    );
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
    let selected = if game.camera.locked && game.camera.current_cut < room.cuts.len() {
        game.camera.current_cut
    } else if let Some(pos) = pos {
        player::camera_for_position(room, game.camera.current_cut, pos)
    } else {
        room.current_cut
    };
    game.camera.current_cut = selected;
    room.current_cut = selected;
}

/// One bit per physical key the engine maps.
///
/// Confirm, cancel and run each accept two keys, so the bits stay per physical
/// key: releasing Space while Return is still held must not clear confirm, and
/// the same goes for X/Backspace and the two Shifts.
const KEY_UP: u32 = 1 << 0;
const KEY_DOWN: u32 = 1 << 1;
const KEY_LEFT: u32 = 1 << 2;
const KEY_RIGHT: u32 = 1 << 3;
const KEY_SPACE: u32 = 1 << 4;
const KEY_RETURN: u32 = 1 << 5;
const KEY_X: u32 = 1 << 6;
const KEY_BACKSPACE: u32 = 1 << 7;
const KEY_LEFTBRACKET: u32 = 1 << 8;
const KEY_RIGHTBRACKET: u32 = 1 << 9;
const KEY_TAB: u32 = 1 << 10;
const KEY_LSHIFT: u32 = 1 << 11;
const KEY_RSHIFT: u32 = 1 << 12;
/// Confirm (Space or Return).
const KEY_CONFIRM: u32 = KEY_SPACE | KEY_RETURN;
/// Cancel (X or Backspace).
const KEY_CANCEL: u32 = KEY_X | KEY_BACKSPACE;
/// Run (either Shift).
const KEY_RUN: u32 = KEY_LSHIFT | KEY_RSHIFT;

/// The bit `scancode` maps to, or `None` for keys the engine ignores.
fn key_bit(scancode: SDL_Scancode) -> Option<u32> {
    Some(match scancode {
        SDL_SCANCODE_UP => KEY_UP,
        SDL_SCANCODE_DOWN => KEY_DOWN,
        SDL_SCANCODE_LEFT => KEY_LEFT,
        SDL_SCANCODE_RIGHT => KEY_RIGHT,
        SDL_SCANCODE_SPACE => KEY_SPACE,
        SDL_SCANCODE_RETURN => KEY_RETURN,
        SDL_SCANCODE_X => KEY_X,
        SDL_SCANCODE_BACKSPACE => KEY_BACKSPACE,
        SDL_SCANCODE_LEFTBRACKET => KEY_LEFTBRACKET,
        SDL_SCANCODE_RIGHTBRACKET => KEY_RIGHTBRACKET,
        SDL_SCANCODE_TAB => KEY_TAB,
        SDL_SCANCODE_LSHIFT => KEY_LSHIFT,
        SDL_SCANCODE_RSHIFT => KEY_RSHIFT,
        _ => return None,
    })
}

/// The keyboard, latched from the SDL event queue.
///
/// The render loop runs at the display's refresh rate, which is not the 30 Hz
/// tick rate; deriving edges from `SDL_GetKeyboardState` every rendered frame
/// drops presses (the frame that sees the edge may run no tick, and by the
/// next tick the key is already held) and can replay one edge over a frame
/// that catches up several ticks. Feeding every key event into this latch
/// makes a press reach exactly one tick, whenever it arrived.
#[derive(Default)]
struct InputState {
    /// Keys currently down, for continuous movement.
    held: u32,
    /// Keys that went down since the last tick; consumed by [`Self::tick`].
    pressed: u32,
    /// A non-repeat key-down of any key since the last tick.
    any_pressed: bool,
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

impl InputState {
    /// Latch a key-down. Auto-repeat keeps the held bit fresh but is not a new
    /// press, so holding a key cannot storm edges.
    fn key_down(&mut self, scancode: SDL_Scancode, repeat: bool) {
        if let Some(bit) = key_bit(scancode) {
            self.held |= bit;
            if !repeat {
                self.pressed |= bit;
            }
        }
        if !repeat {
            self.any_pressed = true;
        }
    }

    /// Latch a key-up. A pending press is left alone: a tap released before
    /// the tick must still reach it.
    fn key_up(&mut self, scancode: SDL_Scancode) {
        if let Some(bit) = key_bit(scancode) {
            self.held &= !bit;
        }
    }

    /// Drop every held and pending key (window focus loss, keyboard removal).
    fn clear(&mut self) {
        self.held = 0;
        self.pressed = 0;
        self.any_pressed = false;
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
                action_held: active & KEY_CONFIRM != 0,
                action_pressed: pressed & KEY_CONFIRM != 0,
            },
            action: active & KEY_CONFIRM != 0,
            ui: UiInput {
                up: pressed & KEY_UP != 0,
                down: pressed & KEY_DOWN != 0,
                left: pressed & KEY_LEFT != 0,
                right: pressed & KEY_RIGHT != 0,
                confirm: pressed & KEY_CONFIRM != 0,
                cancel: pressed & KEY_CANCEL != 0,
                page_left: pressed & KEY_LEFTBRACKET != 0,
                page_right: pressed & KEY_RIGHTBRACKET != 0,
                start: pressed & KEY_TAB != 0,
                any: std::mem::take(&mut self.any_pressed),
            },
        }
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

/// The player's character model plus its no-weapon locomotion clips.
struct PlayerAssets {
    emd: Emd,
    emw: crate::model::Emw,
}

/// The room objects to submit for `camera`, in declared slot order.
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
        if !npc::in_camera_zone(room, camera, world.t) {
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
        visible.push((asset, record, world));
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
        if !npc::in_camera_zone(room, camera, world.t) {
            continue;
        }
        visible.push((asset, record, world));
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

/// Draw one gameplay frame: the cut background, one mesh per active scripted
/// character, the player model, the player's ground shadow and the camera's
/// room-mask layer, depth-sorted together.
///
/// Every active character is drawn, exactly like the original's entity render
/// loop; the camera switch zone gates only the per-joint `0x74` draw flag and
/// the character's own shadow (`has_enter_switch_zone`), never the whole
/// entity, so a character outside the zone still paints when it projects on
/// screen. The meshes are submitted in entity-slot order with the player last,
/// because the original queues the enemies before the player; the shared
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
    game: &game::GameState,
    assets: Option<&PlayerAssets>,
    npc_models: &mut npc::EntityModelCache,
    masks: &mut MaskCache,
    shadows: &mut ShadowCache,
    effect_cache: &mut EffectPageCache,
) {
    let Some(cut) = room.cuts.get(room.current_cut) else {
        framebuffer.clear();
        return;
    };
    let camera = Camera::from_cut(cut);
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

    // The shadow rests on the player's floor height and is skipped unless the
    // player is in the current camera's zone (or the room forces it on).
    // TODO(M10): queue one shadow per visible character through the same
    // fade-sprite path, using `npc::data::character_init`'s tint and quad
    // geometry and the entity's `has_enter_switch_zone` bit; the renderer
    // currently accepts a single shadow, so that pass is deferred.
    let shadow_texture = shadows.page_for(pack);
    let shadow = shadow_texture.and_then(|texture| {
        shadow::visible(id, room, room.current_cut, player_state).then(|| render::Shadow {
            texture,
            pos: player_state.pos,
            angle: player_state.angle,
            half_x: shadow::PLAYER_HALF_X,
            half_z: shadow::PLAYER_HALF_Z,
            lift: shadow::offset_y(id, room.current_cut),
            tint: shadow::billboard_tint(shadow::PLAYER_COLOR),
        })
    });

    // TODO(parity): (visual) the original gates each joint on the entity's
    // per-joint flags (0x74 members must lie inside the camera's switch zone, a
    // couple of animation phases draw only those members, and 0x20/0x4 route to
    // the path-trail/severed-limb steps); this engine poses and draws every
    // joint unconditionally.
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
        };
        let keyframe = keyframes.get(player_state.anim.keyframe_index(clips))?;
        let entity = anim::entity_matrix(player_state.pos, player_state.angle);
        Some(anim::joint_matrices(skeleton, keyframe, &entity))
    });

    // The parsed NPC models are held in `models` so the mesh references built
    // below stay alive for the draw call.
    let mut models: Vec<Arc<Emd>> = Vec::new();
    let mut npc_joints: Vec<Vec<anim::Mat4x3>> = Vec::new();
    let mut npc_slots: Vec<usize> = Vec::new();
    // TODO(parity): (UI) the original queues a fade sprite (ground shadow) for
    // every character inside its camera switch zone, with the per-character
    // tint/quad from `npc::data::character_init`; the port draws only the
    // player's shadow, so NPCs have none (see the TODO(M10) above).
    for slot in 1..game::ENTITY_COUNT {
        let entity = &game.entities[slot];
        if !entity.active() {
            continue;
        }
        let Some(model) = npc_models.get(pack, entity.id) else {
            continue;
        };
        let keyframe_index = game.entity_anims[slot].keyframe_index(entity, &model.clips);
        let Some(keyframe) = model.keyframes.get(keyframe_index) else {
            continue;
        };
        let entity_matrix = anim::entity_matrix(entity.pos, entity.angle);
        let joints = anim::joint_matrices(&model.skeleton, keyframe, &entity_matrix);
        models.push(model);
        npc_joints.push(joints);
        npc_slots.push(slot);
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
    for (_, _, world) in &visible {
        object_joints.push(vec![*world]);
    }
    let mut item_joints: Vec<Vec<anim::Mat4x3>> = Vec::with_capacity(visible_item_models.len());
    for (_, _, world) in &visible_item_models {
        item_joints.push(vec![*world]);
    }

    // TODO(parity): (visual) the original applies each record's background
    // blend weight and semi-transparency; `EntityMesh` carries only the RGB
    // multiplier and the joint draw gate, so records blend opaquely.
    let mut meshes: Vec<EntityMesh<'_>> =
        Vec::with_capacity(visible.len() + visible_item_models.len() + 1 + models.len());
    for ((asset, record, _), joints) in visible.iter().zip(&object_joints) {
        meshes.push(EntityMesh {
            mesh: &asset.model,
            texture: &asset.texture,
            joints,
            tint: record.shade(),
            hidden_joints: 0,
        });
    }
    for ((asset, _, _), joints) in visible_item_models.iter().zip(&item_joints) {
        meshes.push(EntityMesh {
            mesh: &asset.model,
            texture: &asset.texture,
            joints,
            tint: [255; 3],
            hidden_joints: 0,
        });
    }
    // The NPC joint gates pair with `models`/`npc_joints`/`npc_slots`, all
    // built together in entity-slot order above; do not re-walk the slots,
    // because a model whose keyframe lookup failed is absent from all three.
    for ((slot, model), joints) in npc_slots.iter().zip(models.iter()).zip(&npc_joints) {
        meshes.push(EntityMesh {
            mesh: &model.mesh,
            texture: &model.texture,
            joints,
            tint: [255; 3],
            hidden_joints: u32::from(game.entities[*slot].joint_flags),
        });
    }
    if let (Some(assets), Some(joints)) = (assets, &player_joints) {
        meshes.push(EntityMesh {
            mesh: &assets.emd.mesh,
            texture: &assets.emd.texture,
            joints,
            tint: game.player_tint,
            hidden_joints: u32::from(game.entities[0].joint_flags),
        });
    }

    let pages = effect_cache.pages_for(pack, id, &game.room_effects);
    let quads = build_effect_quads(game, room, id, &camera, pages);
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
    render::draw_gameplay_scene_with_effects(
        framebuffer,
        cut.background.as_ref(),
        &meshes,
        shadow.as_ref(),
        &camera,
        &lighting,
        layer.as_ref(),
        Some(&effect_layer),
        visible.len() + visible_item_models.len(),
        mirror.as_ref(),
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

/// Load the room's shipped primary track from the pack, if the table names
/// one. Kept as the M5 real-pack check for the raw `bgm/` entries; the
/// running engine goes through [`bgm::BgmCache`] instead.
#[cfg(test)]
fn load_room_music(pack: &Pack, id: RoomId) -> Option<audio::Wav> {
    let (name, _looping) = crate::music::primary_track(id)?;
    let path = crate::music::pack_path(name)?;
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
    fn voice_pack_path_auto_discovers_the_sibling_and_honours_the_override() {
        let dir = TempDir::new();
        let main = dir.0.join("re1.akpak");
        std::fs::write(&main, b"pack").unwrap();
        // No sibling yet: voice-less, silently.
        assert_eq!(voice_pack_path(&main, None), None);
        let sibling = dir.0.join("re1.voice.akpak");
        std::fs::write(&sibling, b"voice").unwrap();
        assert_eq!(voice_pack_path(&main, None), Some(sibling.clone()));
        // An explicit path wins, and a missing one resolves to nothing (with
        // a warning on stderr).
        assert_eq!(voice_pack_path(&main, Some(&sibling)), Some(sibling));
        assert_eq!(
            voice_pack_path(&main, Some(&dir.0.join("absent.akpak"))),
            None
        );
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

    /// `door_aot_set(0, 100, 200, 300, 400, ..., RDT_001, 555, 0, 666, 1024, 0, 0x81)`.
    fn door_init() -> Vec<u8> {
        door_init_with_camera(0)
    }

    /// `door_init` with a specific record camera byte (`+0x0B`).
    fn door_init_with_camera(camera: u8) -> Vec<u8> {
        let mut body = vec![0x0C, 0x00];
        for value in [100i16, 200, 300, 400] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(&[0, 0, 0, camera, 0]);
        body.push(1);
        for value in [555i16, 0, 666, 1024] {
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
        let room = rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
        let assets = load_player_assets(&pack, id).expect("player assets");
        let mut player_state = player::spawn(id, &room);
        let mut masks = MaskCache::default();
        let mut shadows = ShadowCache::default();
        let mut npc_models = npc::EntityModelCache::default();

        let mut idle = Framebuffer::new();
        render_frame(
            &mut idle,
            &pack,
            id,
            &room,
            &player_state,
            &game::GameState::new(id, &room),
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
            &game::GameState::new(id, &room),
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
        let rdt_bytes = pack.read(&id.rdt_entry()).unwrap();
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

            let mut npc_models = npc::EntityModelCache::default();
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
            let mut npc_models = npc::EntityModelCache::default();
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
                    },
                    player::Input::default(),
                );
            }
        }
        assert_eq!(game.desk.state, 33, "the per-frame desk state machine ran");
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
        let mut npc_models = npc::EntityModelCache::default();
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
            },
            player::Input {
                action_pressed: true,
                action_held: true,
                ..player::Input::default()
            },
        );
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
        let mut npc_models = npc::EntityModelCache::default();
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
            0,
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
                indices: vec![0],
                palettes: vec![[0, 0, 0, 255]],
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
    }

    #[test]
    fn entity_model_cache_remembers_missing_and_invalid_models() {
        let mut writer = PackWriter::new();
        writer.add("npc/23.emd", b"not-an-emd".to_vec()).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let mut cache = npc::EntityModelCache::default();
        // An absent entry yields None and is remembered so the pack is only
        // read once.
        assert!(cache.get(&pack, 0x20).is_none());
        assert!(cache.get(&pack, 0x20).is_none());
        assert!(cache.missing.contains(&0x20));
        // A present but invalid entry yields None too.
        assert!(cache.get(&pack, 0x23).is_none());
        assert!(cache.missing.contains(&0x23));
        // Ids outside the character range have no model path and no warning.
        assert!(cache.get(&pack, 0x1F).is_none());
        assert!(cache.get(&pack, 0xFF).is_none());
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_pack_npc_models_load_through_the_cache() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let mut cache = npc::EntityModelCache::default();

        for id in crate::npc::FIRST_ID..=crate::npc::LAST_ID {
            let model = cache
                .get(&pack, id)
                .unwrap_or_else(|| panic!("npc/{id:02x}.emd missing from the pack"));
            assert_eq!(model.skeleton.relative.len(), 15, "npc/{id:02x}.emd joints");
            assert!(!model.mesh.objects.is_empty(), "npc/{id:02x}.emd mesh");
        }

        // A second request returns the same parsed model, not a re-parse.
        let first = cache.get(&pack, 0x23).unwrap();
        let second = cache.get(&pack, 0x23).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(cache.get(&pack, 0x1F).is_none());
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
            player::ClipSource::Emd | player::ClipSource::Room => {
                (&assets.emd.keyframes, &assets.emd.clips)
            }
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
            hidden_joints: 0,
        }];
        let camera = Camera::from_cut(cut);
        let lighting = Lighting::from_room(&loaded.room);
        let page = bmp::decode_mask(pack.read(&id.roommask_entry(0)).unwrap()).unwrap();
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
            None,
            &camera,
            &lighting,
            Some(&layer),
        );
        let mut plain = Framebuffer::new();
        render::draw_gameplay_scene(
            &mut plain,
            cut.background.as_ref(),
            &entities,
            None,
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
        let pack = Pack::open(&path).unwrap();

        let id = RoomId::parse("1000").unwrap();
        let mut loaded = LoadedRoom {
            id,
            room: RoomState {
                stage: 1,
                room: 2,
                ..RoomState::default()
            },
            scripts: Default::default(),
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
            destination: None,
            frame: transition::TransitionFrame::default(),
        };
        let mut music: Option<Mixer> = None;
        let mut cache = SfxCache::default();
        finish_transition(
            &pack,
            &mut session,
            &mut game,
            &mut player_state,
            &mut loaded,
            &mut music,
            &mut cache,
        );
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
            destination: Some(LoadedRoom {
                id: target,
                room: RoomState {
                    stage: 1,
                    room: 1,
                    ..RoomState::default()
                },
                scripts: Default::default(),
                player_assets: None,
            }),
            frame: transition::TransitionFrame::default(),
        };
        finish_transition(
            &pack,
            &mut session,
            &mut game,
            &mut player_state,
            &mut loaded,
            &mut music,
            &mut cache,
        );
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
        let dir = TempDir::new();
        let path = dir.0.join("voice.akpak");
        let mut writer = PackWriter::new();
        writer
            .add("voice/v004_00.wav", voice_wav_bytes(&[100, 200]))
            .unwrap();
        writer
            .add("voice/v004_01.wav", voice_wav_bytes(&[300, 400]))
            .unwrap();
        writer.write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();

        let _ = unsafe {
            sdl3_sys::hints::SDL_SetHint(sdl3_sys::hints::SDL_HINT_AUDIO_DRIVER, c"dummy".as_ptr())
        };
        let Some(mixer) = Mixer::open() else {
            eprintln!("skipping voice handshake test: no audio device");
            return;
        };
        let mut music = Some(mixer);
        let mut cache = VoiceCache::default();
        let id = RoomId::parse("1000").unwrap();
        let mut game = game::GameState::new(id, &RoomState::default());
        let request = |name| game::VoiceRequest {
            name,
            volume: 0,
            pan: 0,
        };

        // A queued request starts on the next tick and raises the F7 wait bit.
        game.voice.request = Some(request("V004_00"));
        game.set_voice_playing();
        tick_voice(&mut music, &mut cache, &mut game, &pack, None);
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
        tick_voice(&mut music, &mut cache, &mut game, &pack, None);
        assert!(!cache.active, "the finish released the channel");
        assert!(!game.voice_playing(), "F7 advances once the line is done");

        // A request queued while a line is active waits, then starts when the
        // channel frees.
        game.voice.request = Some(request("V004_00"));
        game.set_voice_playing();
        tick_voice(&mut music, &mut cache, &mut game, &pack, None);
        game.voice.request = Some(request("V004_01"));
        game.set_voice_playing();
        tick_voice(&mut music, &mut cache, &mut game, &pack, None);
        assert_eq!(
            cache.pending.map(|pending| pending.name),
            Some("V004_01"),
            "a second line stays pending while the first plays"
        );
        assert!(music.as_ref().unwrap().voice_playing());
        assert!(game.voice_playing());

        music.as_mut().unwrap().render_for_test(4);
        tick_voice(&mut music, &mut cache, &mut game, &pack, None);
        assert!(cache.pending.is_none(), "the pending line was taken");
        assert!(cache.active);
        assert!(
            game.voice_playing(),
            "the pending line kept the wait raised"
        );
        assert!(music.as_ref().unwrap().voice_playing());
        assert_eq!(game.voice.misses, 0);
    }

    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_room_1001_walk_emits_ft_wda() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        let id = RoomId::parse("1001").unwrap();
        let room = rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
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
        let room = rdt::parse(pack.read(&id.rdt_entry()).unwrap(), id).unwrap();
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
        let font = font::Font::new(tim::decode_4bpp(pack.read("font/font.tim").unwrap()).unwrap());
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

    /// A one-room pack with both character variants of RDT 100. Title and
    /// select art are intentionally absent: the screens log and continue.
    fn new_game_pack(dir: &TempDir) -> PathBuf {
        let pack_path = dir.0.join("game.akpak");
        let bmp_bytes = bmp::encode_to_vec(&test_image()).unwrap();
        let mut writer = PackWriter::new();
        for player_flag in 0..=1u8 {
            let id = RoomId {
                stage: 1,
                room: 0,
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
                    room: 0,
                    player_flag: 0,
                }
                .cut_entry(0),
                bmp_bytes.clone(),
            )
            .unwrap();
        writer.write(&pack_path).unwrap();
        pack_path
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
                    ..player::Input::default()
                },
            ),
            (
                SDL_SCANCODE_RSHIFT,
                player::Input {
                    run: true,
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

    #[test]
    fn app_boots_title_and_transitions_into_a_new_game() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, dir.0.join("saves"), None, false);
        app.boot(AppBoot::Title).unwrap();
        assert!(matches!(app.mode, Mode::Title(_)));

        assert!(
            run_until(&mut app, 256, |app| matches!(app.mode, Mode::Select(_))),
            "the title never opened character select"
        );
        assert!(
            run_until(&mut app, 256, |app| matches!(app.mode, Mode::Play(_))),
            "character select never started the new game"
        );
        let Mode::Play(session) = &app.mode else {
            panic!("expected a play mode");
        };
        assert_eq!(
            session.game.id,
            RoomId {
                stage: 1,
                room: 0,
                player_flag: 0
            }
        );
        assert_eq!(session.game.entities[0].health, NEW_GAME_HEALTH[0]);
    }

    #[test]
    fn app_title_load_opens_the_picker_and_loads_a_save() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let saves = dir.0.join("saves");
        std::fs::create_dir_all(&saves).unwrap();
        let mut file = save::SaveFile {
            stage: 1,
            room: 0,
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
        let mut app = App::new(pack, saves, None, false);
        app.boot(AppBoot::Title).unwrap();

        // With a save present the title starts on LOAD GAME.
        app.update(confirm_input(), player::Input::default(), false)
            .unwrap();
        let Mode::Title(screen) = &app.mode else {
            panic!("expected the title");
        };
        assert!(screen.load_enabled());
        assert_eq!(screen.selection(), 2);

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
    }

    /// A modal that counts its updates and draws nothing.
    struct CountingModal {
        updates: std::rc::Rc<std::cell::Cell<u32>>,
    }

    impl Screen for CountingModal {
        fn open(&mut self, _cx: &mut UiContext<'_>) -> Result<()> {
            Ok(())
        }

        fn update(&mut self, _cx: &UiContext<'_>, _input: UiInput) -> ScreenResult {
            self.updates.set(self.updates.get() + 1);
            ScreenResult::Continue
        }

        fn draw(&mut self, _cx: &UiContext<'_>, _framebuffer: &mut Framebuffer) {}
    }

    #[test]
    fn a_modal_freezes_the_room_tick() {
        let dir = TempDir::new();
        let pack_path = new_game_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut app = App::new(pack, dir.0.join("saves"), None, false);
        app.boot(AppBoot::NewGame(0)).unwrap();

        let updates = std::rc::Rc::new(std::cell::Cell::new(0));
        let before = match &app.mode {
            Mode::Play(session) => session.game.frame,
            _ => panic!("expected a play mode"),
        };
        {
            let Mode::Play(session) = &mut app.mode else {
                unreachable!();
            };
            session.open_modal(Box::new(CountingModal {
                updates: std::rc::Rc::clone(&updates),
            }));
        }

        app.update(UiInput::default(), player::Input::default(), false)
            .unwrap();

        let Mode::Play(session) = &app.mode else {
            panic!("expected a play mode");
        };
        assert_eq!(session.game.frame, before, "the room tick must stay frozen");
        assert_eq!(updates.get(), 1, "the modal must advance each tick");
    }

    #[test]
    fn session_transition_swaps_rooms() {
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
        let mut session = GameSession::from_room(&pack, a, Path::new("saves")).unwrap();
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
        assert!(
            session.transition.is_some(),
            "the door never requested a transition"
        );

        for _ in 0..MAX_TRANSITION_FRAMES {
            session.tick_transition(&pack, false);
            if session.transition_finished {
                break;
            }
        }
        assert!(session.transition_finished, "the transition never finished");
        // The finished frame is rendered before teardown, as the loops do.
        session.render(&pack);
        session.finish_transition(&pack);

        assert_eq!(session.loaded.id, b);
        assert_eq!(session.game.id, b);
        assert_eq!(session.player.pos, [555, 0, 666]);
        assert_eq!(session.player.angle, 1024);
        assert!(session.game.doors[0].is_none(), "the door table is rebuilt");
        assert!(!session.transition_finished);
        assert!(session.transition.is_none());
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
        let mut messages: Vec<Option<Vec<u8>>> =
            (0..64).map(|_| Some(vec![0x0C, 0x01, 0x00])).collect();
        // Index 63 is the yes/no take-item stream used by the post-action
        // test: "A" then confirm -> case 10 action 0, with the trailing `0x01`
        // the text-table scanner needs.
        messages[63] = Some(vec![0x0C, 0x08, 0x00, 0x0A, 0x00, 0x01, 0x00]);
        let mut writer = PackWriter::new();
        writer
            .add(&id.rdt_entry(), synthetic_rdt(&[0x00, 0x00]))
            .unwrap();
        writer.add(&id.cut_entry(0), bmp_bytes).unwrap();
        writer
            .add("text/messages.bin", crate::text::encode_table(&messages))
            .unwrap();
        for table in [
            "text/names.bin",
            "text/unknown.bin",
            "text/idesc.bin",
            "text/save.bin",
        ] {
            writer.add(table, crate::text::encode_table(&[])).unwrap();
        }
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

        // Global id 0x40 with a pause word that masks the control bit.
        session.game.show_message(0x40, 0x145);
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
            "the held movement never reached the room tick"
        );

        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        session
            .tick(&pack, UiInput::default(), player::Input::default(), true)
            .unwrap();
        assert!(!session.game.message.active, "the action key dismissed it");
        assert!(!session.game.message_locks_controls());
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
        session.game.show_message(0x40, game::MESSAGE_FLAG_ENTITIES);
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

        // The control bit clear is the message freeze: the press never reaches
        // the climb scan.
        session.game.message_flags &= !game::MESSAGE_FLAG_CONTROLS;
        assert!(session.game.message_locks_controls());
        session
            .tick(&pack, UiInput::default(), press, true)
            .unwrap();
        assert_eq!(session.player.locked, player::LockedAction::None);
        assert!(!session.player.vault_bit);

        // Releasing the freeze lets the same press latch the vault.
        session.game.message_flags |= game::MESSAGE_FLAG_CONTROLS;
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

        // An action-gated item zone the player stands in and faces, so any
        // action press the room tick sees picks the spray up.
        session.player.pos = [0, 0, 0];
        session.player.angle = 0;
        session.game.sync_entity_from_player(&session.player);
        session.game.room_actions[2] = Some(game::RoomAction {
            slot: 2,
            kind: game::RoomActionKind::Item,
            zone: [0, 0, 1000, 1000],
            sce: 4,
            handler: 4,
            flags: 0x81,
            item_data: None,
            room_items_flag: 0xFF,
            params: [ITEM_FIRST_AID_SPRAY, 1, 0, 0, 0, 0, 0, 0],
        });

        // A message that does not pause gameplay: the action key still
        // dismisses it, and the same held key must not reach the room probe.
        session.game.show_message(0x40, 0);
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

        // The dismissing tick swallows the press.
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
            !session.game.has_item(ITEM_FIRST_AID_SPRAY),
            "the dismissing press triggered the zone behind the window"
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
            !session.game.has_item(ITEM_FIRST_AID_SPRAY),
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
            session.game.has_item(ITEM_FIRST_AID_SPRAY),
            "a fresh press after release must reach the zone"
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
        assert!(session.menu.is_none(), "X closes the menu");
        assert!(!session.game.message_menu);

        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.game.frame, before + 1, "the room resumes");
    }

    #[test]
    fn check_opens_the_item_viewer_modal_and_resume_returns_to_the_menu() {
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

        // Slot 0 holds the knife: confirm opens the action submenu, down
        // selects CHECK, confirm installs the viewer.
        for ui in [
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
            UiInput {
                down: true,
                ..UiInput::default()
            },
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
        ] {
            session
                .tick(&pack, ui, player::Input::default(), false)
                .unwrap();
        }
        assert!(session.modal.is_some(), "CHECK installs the item viewer");
        assert!(session.menu.is_some(), "the menu stays underneath");
        let frozen = session.game.frame;
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.game.frame, frozen, "the room stays frozen");

        // The modal owns the input from here: cancel reports Resume, which the
        // app applies by dropping the modal and returning to the menu.
        let mut modal = session.modal.take().unwrap();
        let cx = UiContext {
            pack: &pack,
            save_dir: Path::new("."),
            font: None,
            text: Some(&session.text),
            ticks: 0,
            cues: Default::default(),
        };
        assert_eq!(
            modal.update(
                &cx,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
            ),
            ScreenResult::Done(ScreenAction::Resume)
        );
        assert!(session.menu.is_some(), "resuming lands back on the menu");
    }

    #[test]
    fn resuming_the_item_viewer_marks_the_item_examined() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session =
            GameSession::from_room(&pack, RoomId::parse("100").unwrap(), Path::new("saves"))
                .unwrap();

        // The sword key's name class is 3: it starts unexamined and its real
        // name is hidden until the viewer has run.
        assert!(!crate::message::examined_bit(
            &session.game.examined_flags(),
            3
        ));
        session.open_item_view(&pack, ITEM_SWORD_KEY);
        assert!(session.modal.is_some());

        // The modal reports Resume on cancel; the app applies that through
        // `close_modal`, which is where the examine mark lands.
        let mut modal = session.modal.take().unwrap();
        let cx = UiContext {
            pack: &pack,
            save_dir: Path::new("."),
            font: None,
            text: Some(&session.text),
            ticks: 0,
            cues: Default::default(),
        };
        assert_eq!(
            modal.update(
                &cx,
                UiInput {
                    cancel: true,
                    ..UiInput::default()
                },
            ),
            ScreenResult::Done(ScreenAction::Resume)
        );
        session.close_modal();
        assert!(
            crate::message::examined_bit(&session.game.examined_flags(), 3),
            "leaving the viewer marks the item examined"
        );
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

        // Cancel closes the box and the menu; the room resumes.
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
        assert!(session.item_box.is_none());
        assert!(session.menu.is_none());
        let frozen = session.game.frame;
        session
            .tick(&pack, UiInput::default(), player::Input::default(), false)
            .unwrap();
        assert_eq!(session.game.frame, frozen + 1, "the room resumed");
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

        // The first Tab lands on the map tab, which is inert.
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
        assert!(session.file.is_none());

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

        // Confirm opens the reader; cancel returns to the selector and a
        // second cancel closes only the tab.
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
        assert_eq!(
            session.file.as_ref().unwrap().mode,
            crate::ui::file::FileMode::Reader
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
                    room: 0,
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
            assert_eq!(
                session.game.state_bytes[usize::from(game::STATE_BYTE_SAVES)],
                0
            );
            assert_eq!(&session.game.state_bytes[0x24..0x28], &[0; 4]);
            // The starting health status reaches the `cmpb 50` state byte.
            assert_eq!(session.game.health_status, 0x10);
            assert_eq!(
                session.game.state_bytes[usize::from(game::STATE_BYTE_HEALTH_STATUS)],
                0x10
            );
            // SetInitialItems seeds the three carried room-pickup quantities.
            assert_eq!(
                &session.game.state_bytes[0x0C..0x0F],
                &NEW_GAME_PICKUP_QUANTITIES
            );
            let ids: Vec<u8> = session.game.inventory.iter().map(|slot| slot.id).collect();
            if character == 0 {
                assert_eq!(ids, [ITEM_KNIFE, ITEM_FIRST_AID_SPRAY]);
            } else {
                assert_eq!(ids, [ITEM_KNIFE, ITEM_BERETTA, ITEM_FIRST_AID_SPRAY]);
                assert_eq!(session.game.item_count(ITEM_BERETTA), 15);
            }
            assert_eq!(session.game.item_count(ITEM_FIRST_AID_SPRAY), 1);
        }
    }

    /// The real pack must boot each new game into room 100 with the shipped
    /// inventory, health and room-item flags.
    #[test]
    #[ignore = "requires a converted pack via ARKLAY_RE1_PACK"]
    fn real_new_game_lands_in_room_100_for_both_characters() {
        let Ok(path) = std::env::var("ARKLAY_RE1_PACK") else {
            return;
        };
        let pack = Pack::open(Path::new(&path)).unwrap();
        for character in 0..=1u8 {
            let session = GameSession::new(&pack, character, Path::new("saves")).unwrap();
            assert_eq!(session.loaded.id.room3(), "100");
            assert_eq!(
                session.game.id,
                RoomId {
                    stage: 1,
                    room: 0,
                    player_flag: character
                }
            );
            assert_eq!(session.player.pos, [17000, 0, 5000]);
            assert_eq!(session.player.angle, 3072);
            assert_eq!(
                session.game.entities[0].health,
                NEW_GAME_HEALTH[usize::from(character)]
            );
            // Jill's first playthrough clears the ink-ribbon build's
            // room-items bit (ROOM1001's 0x16, shared with the sword key in
            // the Chris variant), exactly like the original's skip.
            let mut expected_items = game::FlagBank::new();
            expected_items
                .bytes_mut()
                .copy_from_slice(&NEW_GAME_ROOM_ITEMS);
            if character == 1 {
                expected_items.apply(0x16, 1);
            }
            assert_eq!(session.game.flags[7].bytes(), expected_items.bytes());
            let ids: Vec<u8> = session.game.inventory.iter().map(|slot| slot.id).collect();
            if character == 0 {
                assert_eq!(ids, [ITEM_KNIFE, ITEM_FIRST_AID_SPRAY]);
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
        let file = save::SaveFile::from_state_with_prefix(&session.game, card).unwrap();
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
            0
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
        let quads = build_effect_quads(&game, &room, id, &camera, &pages);

        assert_eq!(quads.len(), 1, "the far effect must be culled");
        let quad = &quads[0];
        // depth = 250, key = (250 >> 4) * 0x40 = 960.
        assert_eq!(quad.key, 960);
        // scale = 200 * 1 * 128 * 256 / (1000 + 1) = 6548 -> 204 px wide.
        assert_eq!(quad.size, [204, 204]);
        assert_eq!(quad.pos, [78, 18]);
        // depth_group 8 -> tint level 1 of colour record 1.
        assert_eq!(quad.tint, [0xB2, 0xB2, 0xB2]);
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
        let quads = build_effect_quads(&game, &room, id, &camera, &effect_test_pages());
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
}
