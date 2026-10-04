//! Engine entry point: open a pack, load a room, simulate and display it.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_void};
use std::path::{Path, PathBuf};
use std::rc::Rc;

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
    SDL_SCANCODE_BACKSPACE, SDL_SCANCODE_DOWN, SDL_SCANCODE_LEFT, SDL_SCANCODE_LSHIFT,
    SDL_SCANCODE_RETURN, SDL_SCANCODE_RIGHT, SDL_SCANCODE_RSHIFT, SDL_SCANCODE_SPACE,
    SDL_SCANCODE_TAB, SDL_SCANCODE_UP, SDL_SCANCODE_X,
};
use sdl3_sys::surface::{
    SDL_ConvertSurface, SDL_DestroySurface, SDL_SCALEMODE_NEAREST, SDL_Surface,
};
use sdl3_sys::timer::SDL_GetTicks;
use sdl3_sys::video::{
    SDL_CreateWindow, SDL_DestroyWindow, SDL_SetWindowTitle, SDL_Window, SDL_WindowFlags,
};

use crate::anim;
use crate::audio::{self, Mixer, MusicPlayer};
use crate::bmp;
use crate::door;
use crate::emd;
use crate::font;
use crate::game;
use crate::mask;
use crate::message::MessageInput;
use crate::model::Emd;
use crate::music;
use crate::pack::Pack;
use crate::player;
use crate::rdt;
use crate::render::{self, Camera, Framebuffer, Lighting, MaskLayer, PlayerMesh};
use crate::save;
use crate::scd;
use crate::sfx;
use crate::state::{Image, RoomId, RoomState};
use crate::text::Text;
use crate::tim;
use crate::transition::{self, DoorStepper};
use crate::ui::main_menu::{MainMenu, MenuAssets, MenuEvent, MenuInput};
use crate::ui::{self, Screen, ScreenAction, ScreenResult, UiContext, UiInput};

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
/// opens audio.
pub fn run(pack: &Path, id: RoomId, capture: Option<&Path>) -> Result<()> {
    let pack = Pack::open(pack)?;

    if let Some(capture_path) = capture {
        let mut loaded = load_room(&pack, id)?;
        let mut game = game::GameState::new(id, &loaded.room);
        let player_state = player::spawn(id, &loaded.room);
        game.sync_entity_from_player(&player_state);
        run_room_init(&loaded, &mut game);
        drain_mask_toggles(&mut loaded.room, &mut game);
        apply_camera(&mut loaded.room, &mut game, None);
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
            loaded.player_assets.as_ref(),
            &mut MaskCache::default(),
        );
        display.present(&framebuffer)?;
        return display.capture(capture_path);
    }

    let mut session = GameSession::from_room(&pack, id)?;
    let title = window_title(
        &session.loaded.id.room3(),
        session.loaded.room.current_cut,
        session.loaded.room.cuts.len(),
    );
    let display = Display::new(&title, false)?;
    session.start_audio();
    run_session_loop(&pack, &mut session, &display)
}

/// The interactive gameplay loop shared by `--room` and the app's Play mode.
fn run_session_loop(pack: &Pack, session: &mut GameSession, display: &Display) -> Result<()> {
    let mut framebuffer = Framebuffer::new();
    let mut edges = InputEdges::default();
    let mut event = SDL_Event::default();
    let mut last_ticks = unsafe { SDL_GetTicks() };
    let mut accumulator = 0.0f64;
    loop {
        let mut cut_delta = 0i32;
        let mut any_key = false;
        if poll_events(&mut event, &mut cut_delta, &mut any_key) {
            return Ok(());
        }
        let ui = edges.read(any_key);
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
        let (input, action) = read_input();
        while accumulator >= TICK_MS {
            if session.transition_finished {
                accumulator = 0.0;
                break;
            }
            if session.transition.is_some() {
                session.tick_transition(pack, action || input.run);
            } else {
                session.tick(pack, ui, input, action)?;
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
/// The message window and the inventory menu are **not** driven through the
/// [`ui::Screen`] modal hook: the message is part of [`game::GameState`] and
/// keeps running on the gameplay tick, while the menu freezes the room but
/// keeps drawing the last gameplay frame, reads its art from the session and
/// mutates the same `GameState`. The boxed modal hook is kept for screens
/// that own all of their state (the item viewer); it still freezes the room
/// and draws over the last gameplay frame exactly as before.
struct GameSession {
    loaded: LoadedRoom,
    game: game::GameState,
    player: player::PlayerState,
    command_vm: scd::vm::CommandVm,
    event_vm: scd::vm::EventVm,
    masks: MaskCache,
    sfx_cache: SfxCache,
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
}

/// New-game start position X (the original's `InitPlayerData`).
pub const NEW_GAME_POS_X: i32 = 17000;
/// New-game start position Z.
pub const NEW_GAME_POS_Z: i32 = 5000;
/// New-game facing angle.
pub const NEW_GAME_ANGLE: u16 = 3072;
/// New-game starting health: Chris then Jill.
pub const NEW_GAME_HEALTH: [i16; 2] = [140, 96];
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

impl GameSession {
    /// A new game as `character` (`0` Chris, `1` Jill): stage 1 room 0 with
    /// the original's start position, health, items and room-item flags, then
    /// the room init/main/event VMs.
    fn new(pack: &Pack, character: u8) -> Result<Self> {
        let character = character & 1;
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: character,
        };
        let loaded = load_room(pack, id)?;
        let mut game = game::GameState::new(id, &loaded.room);
        seed_new_game(&mut game, character);
        let mut player_state = player::spawn(id, &loaded.room);
        player_state.pos = [NEW_GAME_POS_X, 0, NEW_GAME_POS_Z];
        player_state.angle = NEW_GAME_ANGLE;
        game.sync_entity_from_player(&player_state);
        Self::from_loaded(pack, loaded, game, player_state)
    }

    /// Boot `id` directly, the `--room` path.
    fn from_room(pack: &Pack, id: RoomId) -> Result<Self> {
        let loaded = load_room(pack, id)?;
        let mut game = game::GameState::new(id, &loaded.room);
        let player_state = player::spawn(id, &loaded.room);
        game.sync_entity_from_player(&player_state);
        Self::from_loaded(pack, loaded, game, player_state)
    }

    /// Continue from a parsed save: the block replaces the state, the saved
    /// position and angle place the player and the saved room loads.
    fn from_save(pack: &Pack, file: &save::SaveFile) -> Result<Self> {
        let id = RoomId {
            stage: file.stage,
            room: file.room,
            player_flag: file.character & 1,
        };
        let loaded = load_room(pack, id)?;
        let mut game = game::GameState::new(id, &loaded.room);
        file.apply_to(&mut game);
        let mut player_state = player::spawn(id, &loaded.room);
        player_state.pos = [
            i32::from(file.pos_x),
            player_state.pos[1],
            i32::from(file.pos_z),
        ];
        player_state.angle = file.angle as u16 & 0x0FFF;
        game.sync_entity_from_player(&player_state);
        Self::from_loaded(pack, loaded, game, player_state)
    }

    /// Assemble a session around already-built state and run the room boot.
    fn from_loaded(
        pack: &Pack,
        loaded: LoadedRoom,
        game: game::GameState,
        player_state: player::PlayerState,
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
            sfx_cache: SfxCache::default(),
            music: None,
            text: Text::load(pack),
            font,
            menu_assets: None,
            menu_assets_loaded: false,
            menu: None,
            framebuffer: Framebuffer::new(),
            titled_cut: usize::MAX,
            transition: None,
            transition_finished: false,
            modal: None,
        };
        session.enter_room();
        Ok(session)
    }

    /// Run the room boot: init script, queued events, the player mirror, mask
    /// toggles and the camera zone scan.
    fn enter_room(&mut self) {
        {
            let mut host = game::ScdGameHost::new(&mut self.game);
            self.command_vm.run_init(&mut host);
        }
        start_pending_events(&mut self.game, &mut self.event_vm);
        self.game.sync_player(&mut self.player);
        drain_mask_toggles(&mut self.loaded.room, &mut self.game);
        apply_camera(&mut self.loaded.room, &mut self.game, Some(self.player.pos));
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
        if self.menu.is_some() {
            self.tick_menu(ui, input, action);
            return Ok(());
        }

        let was_locked = self.game.message_locks_controls();
        self.game.update_message(
            MessageInput {
                action,
                left: input.left,
                right: input.right,
            },
            &self.loaded.room,
            &self.text,
        );

        if ui.start && !self.game.message.active {
            self.open_menu(pack);
            return Ok(());
        }

        // A paused message ignores the player this tick; the press that
        // dismissed it is spent on the window rather than the room.
        let (input, action) = if was_locked {
            (player::Input::default(), false)
        } else {
            (input, action)
        };

        let transition = tick_room(
            &mut self.command_vm,
            &mut self.event_vm,
            RoomContext {
                room: &mut self.loaded.room,
                game: &mut self.game,
                player: &mut self.player,
                player_assets: self.loaded.player_assets.as_ref(),
            },
            input,
            action,
        );
        apply_bgm_requests(
            &mut self.music,
            &mut self.game.room_bgm_requests,
            pack,
            self.loaded.id,
        );
        drain_mask_toggles(&mut self.loaded.room, &mut self.game);
        play_footsteps(
            &mut self.music,
            &mut self.sfx_cache,
            pack,
            &self.loaded.room,
            &mut self.player,
        );
        if let Some(transition) = transition {
            let record = self.game.transition_door.take().unwrap_or_default();
            self.transition = Some(start_transition(pack, &record, &transition)?);
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

    /// One frozen tick of the pause menu. A message owns the input while it is
    /// up; otherwise one pad edge reaches the menu and its event is consumed.
    fn tick_menu(&mut self, ui: UiInput, input: player::Input, action: bool) {
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
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        menu.tick(&mut self.game);
        let Some(event_input) = menu_input(ui) else {
            return;
        };
        let event = menu.handle_input(&mut self.game, event_input);
        match event {
            MenuEvent::None => {}
            MenuEvent::Close => self.close_menu(),
            MenuEvent::Message(id) => self.game.show_message(id as u8, 0),
            // The item viewer is the next slice's screen; consume the CHECK
            // event and stay on the inventory for now.
            MenuEvent::ViewItem(_) => {}
            // The map/file/radio tabs need no engine action and the next
            // frame redraws after a Changed event.
            MenuEvent::Tab(_) | MenuEvent::Changed => {}
        }
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
        self.enter_room();
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
                self.loaded.player_assets.as_ref(),
                &mut self.masks,
            );
        }
        if self.menu.is_some() {
            self.ensure_menu_assets(pack);
            if let (Some(menu), Some(assets)) = (&self.menu, &self.menu_assets) {
                menu.draw(&mut self.framebuffer, assets, &self.text, &self.game);
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
    #[allow(dead_code)]
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

    /// Open the audio device and start the room's track, if not already open.
    fn start_audio(&mut self) {
        if self.music.is_none() {
            self.music = start_audio(&mut self.loaded);
        }
    }

    /// Feed the mixer's streaming voices.
    fn update_audio(&mut self) {
        if let Some(mixer) = &mut self.music {
            mixer.update();
        }
    }
}

/// Apply the shipped new-game state: health, room-item flags, save counter
/// and starting inventory (knife, spray, and Jill's Beretta with 15 rounds).
fn seed_new_game(game: &mut game::GameState, character: u8) {
    let character = character & 1;
    game.entities[0].health = NEW_GAME_HEALTH[usize::from(character)];
    game.max_health = NEW_GAME_HEALTH[usize::from(character)];
    game.health_status = 0x10;
    game.flags[7]
        .bytes_mut()
        .copy_from_slice(&NEW_GAME_ROOM_ITEMS);
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
    /// The load-screen picker.
    SaveLoad,
    /// A new game as the character (`0` Chris, `1` Jill).
    NewGame(u8),
    /// The pause menu over the deterministic capture room.
    Menu,
}

/// The room `--ui menu` and its capture boot into, with the known capture
/// inventory added on top of the room boot.
pub const MENU_ROOM: &str = "1001";

/// Fixed ticks the headless menu capture settles before the frame is drawn.
const MENU_CAPTURE_TICKS: u32 = 30;

/// The app's current screen.
enum Mode {
    Title(ui::title::TitleScreen),
    Select(ui::char_select::CharSelectScreen),
    Load(ui::save_load::SaveLoadScreen),
    Play(Box<GameSession>),
}

/// The mode machine: one screen at a time, with the pack and decoded shared
/// assets owned here.
struct App {
    pack: Pack,
    save_dir: PathBuf,
    font: Option<font::Font>,
    text: Text,
    mode: Mode,
    framebuffer: Framebuffer,
    ticks: u64,
    /// Whether entering a game opens the audio device; captures leave it off.
    audio: bool,
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
    fn new(pack: Pack, save_dir: PathBuf, audio: bool) -> Self {
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
            font,
            text,
            mode: Mode::Title(ui::title::TitleScreen::new()),
            framebuffer: Framebuffer::new(),
            ticks: 0,
            audio,
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
        }
    }

    /// Open the boot screen.
    fn boot(&mut self, boot: AppBoot) -> Result<()> {
        match boot {
            AppBoot::Title => self.open_title(),
            AppBoot::CharSelect => self.open_select(),
            AppBoot::SaveLoad => self.open_load(),
            AppBoot::NewGame(character) => {
                let session = GameSession::new(&self.pack, character)?;
                self.start_session(session);
                Ok(())
            }
            AppBoot::Menu => self.open_menu(),
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

    /// Open the load-screen picker.
    fn open_load(&mut self) -> Result<()> {
        let mut screen = ui::save_load::SaveLoadScreen::new();
        screen.open(&mut self.context())?;
        self.mode = Mode::Load(screen);
        Ok(())
    }

    /// Enter a gameplay session, opening audio on the interactive path.
    fn start_session(&mut self, mut session: GameSession) {
        if self.audio {
            session.start_audio();
        }
        self.mode = Mode::Play(Box::new(session));
    }

    /// Boot the pause-menu session over [`MENU_ROOM`] with the deterministic
    /// capture inventory and the menu already open.
    fn open_menu(&mut self) -> Result<()> {
        let id = RoomId::parse(MENU_ROOM)?;
        let mut session = GameSession::from_room(&self.pack, id)?;
        session.seed_menu_capture();
        session.open_menu(&self.pack);
        self.start_session(session);
        Ok(())
    }

    /// Apply a completed screen action.
    fn apply(&mut self, action: ScreenAction) -> Result<AppFlow> {
        match action {
            ScreenAction::CharSelect => self.open_select()?,
            ScreenAction::SaveLoad => self.open_load()?,
            ScreenAction::NewGame { character } => {
                let session = GameSession::new(&self.pack, character)?;
                self.start_session(session);
            }
            ScreenAction::LoadGame { slot } => {
                let file = save::load(&self.save_dir, slot)?;
                let session = GameSession::from_save(&self.pack, &file)?;
                self.start_session(session);
            }
            ScreenAction::Title => self.open_title()?,
            ScreenAction::Resume => {
                if let Mode::Play(session) = &mut self.mode {
                    session.modal = None;
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
        };
        match mode {
            Mode::Title(screen) => screen.update(&cx, ui),
            Mode::Select(screen) => screen.update(&cx, ui),
            Mode::Load(screen) => screen.update(&cx, ui),
            Mode::Play(_) => ScreenResult::Continue,
        }
    }

    /// Tick the gameplay session or its modal.
    fn play_update(&mut self, ui: UiInput, input: player::Input, action: bool) -> Result<AppFlow> {
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
            };
            match modal.update(&cx, ui) {
                ScreenResult::Done(ScreenAction::Resume) => session.modal = None,
                ScreenResult::Done(ScreenAction::Quit) => return Ok(AppFlow::Quit),
                _ => {}
            }
        } else if !session.transition_finished {
            if session.transition.is_some() {
                session.tick_transition(pack, action || input.run);
            } else {
                session.tick(pack, ui, input, action)?;
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
            Mode::Title(screen) => {
                let cx = UiContext {
                    pack,
                    save_dir,
                    font: font.as_ref(),
                    text: Some(text),
                    ticks: *ticks,
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
/// `game` and `load` boot the app screens, and `menu` boots the pause menu
/// over [`MENU_ROOM`] with the deterministic capture inventory. `capture`
/// renders one deterministic frame and exits; otherwise the window stays up
/// until the user quits.
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
    let boot = match screen {
        "font" => return run_font_ui(pack, capture),
        "title" => AppBoot::Title,
        "select" => AppBoot::CharSelect,
        "load" => AppBoot::SaveLoad,
        "game" => AppBoot::NewGame(character & 1),
        "menu" => AppBoot::Menu,
        other => {
            bail!(
                "unknown --ui screen `{other}`; expected `font`, `title`, `select`, `game`, \
                 `menu` or `load`"
            )
        }
    };

    if let Some(capture_path) = capture {
        let pack = Pack::open(pack)?;
        let mut app = App::new(pack, save_dir.to_path_buf(), false);
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
            AppBoot::NewGame(character) => {
                let session = GameSession::new(&app.pack, character)?;
                app.start_session(session);
            }
            AppBoot::Menu => {
                app.open_menu()?;
                app.settle(MENU_CAPTURE_TICKS)?;
            }
        }
        app.draw();
        return app.capture(capture_path);
    }

    let pack = Pack::open(pack)?;
    let mut app = App::new(pack, save_dir.to_path_buf(), true);
    app.boot(boot)?;
    let display = Display::new("Arklay", false)?;
    let mut edges = InputEdges::default();
    let mut event = SDL_Event::default();
    let mut last_ticks = unsafe { SDL_GetTicks() };
    let mut accumulator = 0.0f64;
    loop {
        let mut cut_delta = 0i32;
        let mut any_key = false;
        if poll_events(&mut event, &mut cut_delta, &mut any_key) {
            return Ok(());
        }
        let ui = edges.read(any_key);
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
        let (input, action) = read_input();
        while accumulator >= TICK_MS {
            if let AppFlow::Quit = app.update(ui, input, action)? {
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
    let mut any_key = false;
    loop {
        if poll_events(&mut event, &mut cut_delta, &mut any_key) {
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
/// swap in the destination room, place the player and restart the BGM.
fn finish_transition(
    pack: &Pack,
    session: &mut TransitionMode,
    game: &mut game::GameState,
    player_state: &mut player::PlayerState,
    loaded: &mut LoadedRoom,
    music: &mut Option<Mixer>,
    sfx_cache: &mut SfxCache,
) {
    if session.camera_only {
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

    match (music.as_mut(), loaded.music.take()) {
        (Some(mixer), Some(wav)) => {
            if let Err(err) = mixer.play_bgm(wav) {
                eprintln!("warning: failed to start music: {err:#}");
            }
        }
        (Some(mixer), None) => mixer.stop_bgm(),
        (None, _) => {}
    }

    // The close SE goes last: starting the destination BGM clears the mixer's
    // queued samples, which would otherwise swallow the door sound.
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
    let mut game = game::GameState::new(id, &loaded.room);
    let mut player_state = player::spawn(id, &loaded.room);
    run_room_init(&loaded, &mut game);
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
    run_room_init(&loaded, &mut game);
    game.sync_player(&mut player_state);
    drain_mask_toggles(&mut loaded.room, &mut game);
    apply_camera(&mut loaded.room, &mut game, Some(player_state.pos));

    // The destination's first gameplay frame, drawn from the placed player and
    // the zone-selected cut, exactly as the engine's render loop would.
    let mut gameplay = Framebuffer::new();
    render_frame(
        &mut gameplay,
        pack,
        loaded.id,
        &loaded.room,
        &player_state,
        loaded.player_assets.as_ref(),
        &mut MaskCache::default(),
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

/// Consume the tick's footstep events: resolve the floor zone's sound and play
/// each as a 3D one-shot through the mixer.
fn play_footsteps(
    music: &mut Option<Mixer>,
    cache: &mut SfxCache,
    pack: &Pack,
    room: &RoomState,
    player_state: &mut player::PlayerState,
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
        // The entity sound modifier is the original's effect-zone flag, which
        // no room script in this slice raises; footsteps always use type 0.
        let Some(name) = sfx::footstep_sound(room, footstep.pos, footstep.sound_type, false) else {
            continue;
        };
        let Some(wav) = cache.load(pack, name) else {
            continue;
        };
        let (gain, pan) = sfx::sound_gain_pan(cut.pos, cut.look_at, footstep.pos);
        mixer.play_sfx(wav, gain, pan);
    }
}

/// Poll every queued SDL event, reporting whether the user quit and
/// accumulating the shift+`,`/`.` camera-cut step. A non-repeat key-down also
/// sets `any_key`, the UI screens' "any button" edge.
fn poll_events(event: &mut SDL_Event, cut_delta: &mut i32, any_key: &mut bool) -> bool {
    let mut quit = false;
    while unsafe { SDL_PollEvent(event) } {
        let kind = unsafe { event.r#type };
        if kind == SDL_EVENT_QUIT {
            quit = true;
        } else if kind == SDL_EVENT_KEY_DOWN {
            let key = unsafe { event.key.key };
            let modifiers = unsafe { event.key.r#mod };
            if !unsafe { event.key.repeat } {
                *any_key = true;
            }
            if key == SDLK_ESCAPE {
                quit = true;
            } else if modifiers & SDL_KMOD_SHIFT != SDL_KMOD_NONE {
                if key == SDLK_COMMA {
                    *cut_delta -= 1;
                } else if key == SDLK_PERIOD {
                    *cut_delta += 1;
                }
            }
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
}

/// Run one fixed 30 Hz tick: scripts, interaction, player movement and camera.
/// Returns the door transition the tick requested, if any.
fn tick_room(
    command_vm: &mut scd::vm::CommandVm,
    event_vm: &mut scd::vm::EventVm,
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
    }
    // Scripts may have moved the player entity directly (dir_set, actor
    // motion); mirror that onto the visible player before physics run.
    context.game.sync_player(context.player);
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
    context.game.sync_entity_from_player(context.player);
    // The original runs the room action probe after the player's movement, so
    // `stairs_height_update` measures the frame's final position and the climb
    // behaviour starts from where the player actually is.
    {
        let mut host = game::ScdGameHost::new(context.game);
        host.interact(context.player.pos, context.player.angle, action);
    }
    context.game.apply_stair_state(context.player);
    apply_camera(context.room, context.game, Some(context.player.pos));
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

/// One frame of level-triggered UI keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Keys {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    confirm: bool,
    cancel: bool,
    /// START, the gameplay pause menu (Tab).
    start: bool,
}

/// Read the UI keys from the current keyboard state.
fn read_keys() -> Keys {
    let mut count = 0i32;
    let keys = unsafe { SDL_GetKeyboardState(&mut count) };
    let down = |scancode: sdl3_sys::scancode::SDL_Scancode| {
        let index = scancode.0;
        index >= 0 && index < count && !keys.is_null() && unsafe { *keys.add(index as usize) }
    };
    Keys {
        up: down(SDL_SCANCODE_UP),
        down: down(SDL_SCANCODE_DOWN),
        left: down(SDL_SCANCODE_LEFT),
        right: down(SDL_SCANCODE_RIGHT),
        confirm: down(SDL_SCANCODE_SPACE) || down(SDL_SCANCODE_RETURN),
        cancel: down(SDL_SCANCODE_X) || down(SDL_SCANCODE_BACKSPACE),
        start: down(SDL_SCANCODE_TAB),
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
    } else if ui.confirm {
        Some(MenuInput::Confirm)
    } else if ui.cancel {
        Some(MenuInput::Cancel)
    } else {
        None
    }
}

/// Turn the level-triggered keys into the screens' edge-triggered input.
///
/// `any_key` comes from the SDL event queue (a non-repeat key-down), so F-keys
/// and other unbound keys still open the title menu.
#[derive(Default)]
struct InputEdges {
    previous: Keys,
}

impl InputEdges {
    /// Read the current keys and report which went down since the last call.
    fn read(&mut self, any_key: bool) -> UiInput {
        let keys = read_keys();
        let input = UiInput {
            up: keys.up && !self.previous.up,
            down: keys.down && !self.previous.down,
            left: keys.left && !self.previous.left,
            right: keys.right && !self.previous.right,
            confirm: keys.confirm && !self.previous.confirm,
            cancel: keys.cancel && !self.previous.cancel,
            start: keys.start && !self.previous.start,
            any: any_key || (keys.confirm && !self.previous.confirm),
        };
        self.previous = keys;
        input
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

/// Draw one gameplay frame: the cut background, the player model and the
/// camera's room-mask layer, depth-sorted together.
///
/// The mask page is loaded lazily from the pack and cached per camera. A room
/// without a page (or a camera without mask sprites) draws without the layer.
fn render_frame(
    framebuffer: &mut Framebuffer,
    pack: &Pack,
    id: RoomId,
    room: &RoomState,
    player_state: &player::PlayerState,
    assets: Option<&PlayerAssets>,
    masks: &mut MaskCache,
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

    let Some(assets) = assets else {
        render::draw_gameplay_scene(
            framebuffer,
            cut.background.as_ref(),
            None,
            &camera,
            &lighting,
            layer.as_ref(),
        );
        return;
    };
    let (keyframes, clips) = match player_state.clip_source {
        player::ClipSource::Emd => (&assets.emd.keyframes, &assets.emd.clips),
        player::ClipSource::Emw => (&assets.emw.keyframes, &assets.emw.clips),
    };
    let keyframe_index = player_state.anim.keyframe_index(clips);
    let Some(keyframe) = keyframes.get(keyframe_index) else {
        render::draw_gameplay_scene(
            framebuffer,
            cut.background.as_ref(),
            None,
            &camera,
            &lighting,
            layer.as_ref(),
        );
        return;
    };
    let entity = anim::entity_matrix(player_state.pos, player_state.angle);
    let joints = anim::joint_matrices(&assets.emd.skeleton, keyframe, &entity);
    let player = PlayerMesh {
        mesh: &assets.emd.mesh,
        texture: &assets.emd.texture,
        joints: &joints,
    };
    render::draw_gameplay_scene(
        framebuffer,
        cut.background.as_ref(),
        Some(&player),
        &camera,
        &lighting,
        layer.as_ref(),
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

/// Open the audio device and start the room's music if it has any.
///
/// Unlike [`start_music`] this always opens the mixer, so one-shot sound
/// effects (footsteps, door SEs) still play in rooms without a primary track.
/// Audio stays best-effort: a missing device logs a warning and the engine
/// runs silently.
fn start_audio(loaded: &mut LoadedRoom) -> Option<Mixer> {
    let Some(mut mixer) = MusicPlayer::open() else {
        eprintln!("warning: no audio device; continuing without sound");
        return None;
    };
    if let Some(wav) = loaded.music.take()
        && let Err(err) = mixer.play_bgm(wav)
    {
        eprintln!("warning: failed to start music: {err:#}");
    }
    Some(mixer)
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
        let mut masks = MaskCache::default();

        let mut idle = Framebuffer::new();
        render_frame(
            &mut idle,
            &pack,
            id,
            &room,
            &player_state,
            Some(&assets),
            &mut masks,
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
            Some(&assets),
            &mut masks,
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
        for _ in 0..30 {
            tick_room(
                &mut command_vm,
                &mut event_vm,
                RoomContext {
                    room: &mut loaded.room,
                    game: &mut game,
                    player: &mut player_state,
                    player_assets: loaded.player_assets.as_ref(),
                },
                player::Input {
                    up: true,
                    ..player::Input::default()
                },
                false,
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
        run(Path::new(&path), id, Some(&capture_path)).unwrap();
        let captured = bmp::decode(&std::fs::read(&capture_path).unwrap()).unwrap();

        // The capture path is deterministic.
        let repeat_path = dir.0.join("room1000_masked2.bmp");
        run(Path::new(&path), id, Some(&repeat_path)).unwrap();
        let repeat = bmp::decode(&std::fs::read(&repeat_path).unwrap()).unwrap();
        assert_eq!(captured.rgba, repeat.rgba, "two captures differ");

        // Rebuild the same pose through the render API. The init script writes
        // state byte 2 (`roomCameraId`), which moves the camera to cut 2; that
        // cut has no mask sprites, so compare a masked cut's render instead.
        let mut loaded = load_room(&pack, id).unwrap();
        let mut game = game::GameState::new(id, &loaded.room);
        run_room_init(&loaded, &mut game);
        apply_camera(&mut loaded.room, &mut game, None);
        assert_eq!(loaded.room.current_cut, 2, "init selects camera 2");
        assert!(loaded.room.cuts[2].masks.is_empty());

        let player_state = player::spawn(id, &loaded.room);
        let cut = &loaded.room.cuts[0];
        assert!(!cut.masks.is_empty(), "room 1000 cut 0 has mask sprites");
        let assets = loaded.player_assets.as_ref().expect("player assets");
        let (keyframes, clips) = match player_state.clip_source {
            player::ClipSource::Emd => (&assets.emd.keyframes, &assets.emd.clips),
            player::ClipSource::Emw => (&assets.emw.keyframes, &assets.emw.clips),
        };
        let keyframe = &keyframes[player_state.anim.keyframe_index(clips)];
        let entity = anim::entity_matrix(player_state.pos, player_state.angle);
        let joints = anim::joint_matrices(&assets.emd.skeleton, keyframe, &entity);
        let player = PlayerMesh {
            mesh: &assets.emd.mesh,
            texture: &assets.emd.texture,
            joints: &joints,
        };
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
            Some(&player),
            &camera,
            &lighting,
            Some(&layer),
        );
        let mut plain = Framebuffer::new();
        render::draw_gameplay_scene(
            &mut plain,
            cut.background.as_ref(),
            Some(&player),
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
        let mut app = App::new(pack, saves, false);
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
        let mut app = App::new(pack, dir.0.join("saves"), false);
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
        let mut session = GameSession::from_room(&pack, a).unwrap();
        session.player.pos = [-450, 0, 250];
        session.player.angle = 0;
        session.game.sync_entity_from_player(&session.player);

        for _ in 0..10 {
            session
                .tick(&pack, UiInput::default(), player::Input::default(), true)
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
        let mut session = GameSession::from_room(&pack, RoomId::parse("100").unwrap()).unwrap();

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
    fn start_opens_the_menu_freezes_the_room_and_cancel_resumes() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session = GameSession::from_room(&pack, RoomId::parse("100").unwrap()).unwrap();

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
    fn a_confirmed_message_pickup_reaches_the_session_state() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session = GameSession::from_room(&pack, RoomId::parse("100").unwrap()).unwrap();

        // A room item action armed behind a yes/no message; the zone sits far
        // from the spawn so the action key cannot fire it directly.
        session.game.room_actions[3] = Some(game::RoomAction {
            slot: 3,
            kind: game::RoomActionKind::Item,
            zone: [10000, 10000, 100, 100],
            sce: 4,
            handler: 4,
            flags: 0x81,
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
            .tick(&pack, UiInput::default(), player::Input::default(), true)
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
    fn start_is_refused_while_a_message_is_displayed() {
        let dir = TempDir::new();
        let pack_path = message_pack(&dir);
        let pack = Pack::open(&pack_path).unwrap();
        let mut session = GameSession::from_room(&pack, RoomId::parse("100").unwrap()).unwrap();

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
        let mut session = GameSession::from_room(&pack, RoomId::parse("100").unwrap()).unwrap();
        // At full health the green herb's USE is refused; the menu reports
        // the refusal as 0xf7 + the heal category (7).
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
            let session = GameSession::new(&pack, character).unwrap();
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
            let session = GameSession::new(&pack, character).unwrap();
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
            assert_eq!(session.game.flags[7].bytes(), &NEW_GAME_ROOM_ITEMS);
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
        let mut session = GameSession::new(&pack, 0).unwrap();
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
        let continued = GameSession::from_save(&pack, &parsed).unwrap();
        assert_eq!(continued.game.id, session.game.id);
        assert_eq!(continued.game.entities[0].health, 88);
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
        let mut session = GameSession::from_room(&pack, id).unwrap();

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
            .tick(&pack, UiInput::default(), player::Input::default(), true)
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
}
