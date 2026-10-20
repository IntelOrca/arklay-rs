//! A save/load screen implemented by a pack Lua script.
//!
//! The screen is a normal pack entry, and the pack's scripts register it with
//! `api:register_save_screen` from their `on_init` hook (the checked-in
//! `mods/crloadsave` is the reference). The engine loads it in the same
//! sandbox the hooks use and drives three optional globals:
//!
//! ```lua
//! function on_open(api) end        -- once, before the first tick
//! function update(api, input) end  -- once per fixed tick
//! function draw(api) end           -- once per frame
//! ```
//!
//! `api` is the general screen surface a mod can use for any full-screen
//! menu: the save list, save/load/remove actions, the pad edges, the screen
//! sounds, and immediate-mode drawing in the 320x240 layout space. The full
//! method table is in `docs/modding.md`.
//!
//! A script that fails to load leaves the engine on the ported original
//! screen; a script that errors while running is logged once and the screen
//! leaves (returning to gameplay or the title) instead of trapping the
//! player.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result};
use mlua::{Function, Lua, Table, UserData, UserDataMethods, Value};

use crate::budget;
use crate::charset::{self, Script};
use crate::font::Tint;
use crate::lua::{Budget, new_sandboxed_state};
use crate::model::Texture8;
use crate::pack::Pack;
use crate::render::{Framebuffer, SpriteBlend};
use crate::save::{self, SaveEntry, SaveFile};
use crate::state::Image;
use crate::text::Text;
use crate::transition::FadeColor;
use crate::ui::save_load::{SaveOutcome, consume_snapshot_ribbon};
use crate::ui::{Screen, ScreenAction, ScreenResult, UiContext, UiCue, UiInput};

use sources::{Loader, Sources};

mod draw;
mod sources;

/// The coordinate range the drawing API accepts; wider values are clamped
/// so the sprite and font cursors cannot overflow.
const DRAW_LIMIT: i32 = 4096;

/// Which way the screen runs.
#[derive(Debug, Clone)]
enum Mode {
    /// The title's load screen.
    Load,
    /// A typewriter (or the next-cycle ending save): the block `file` is
    /// written where the script chooses, consuming an ink ribbon when
    /// asked. Boxed: a save block dwarfs the load variant.
    Save {
        file: Box<SaveFile>,
        ink_ribbon: bool,
    },
    /// A named engine screen (`ui/<id>.lua`): the script reports its own
    /// outcome with `api:exit(action)`, and a failing script reports `fail`.
    Screen { fail: ScreenAction },
}

/// One `args` field a screen's `open(api, args)` receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenArg {
    Int(i64),
    Bool(bool),
    Str(String),
}

/// One decoded pack image, kept in the shape its format has.
enum Decoded {
    /// A BMP or 16bpp TIM: straight RGBA pixels.
    Rgba(Image),
    /// A 4bpp/8bpp TIM: palette indices plus CLUT rows.
    Indexed(Texture8),
}

impl Decoded {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Rgba(image) => (image.width, image.height),
            Self::Indexed(texture) => (texture.width, texture.height),
        }
    }
}

/// The persistent state behind the `api` userdata.
struct Host {
    mode: Mode,
    save_dir: PathBuf,
    /// The byte space the script's text is encoded in.
    script: Script,
    /// Whether text takes the session's tight metrics.
    tight: bool,
    /// The save directory's scan, refreshed on open and after a write.
    list: Vec<SaveEntry>,
    /// Decoded pixels the image cache holds, against
    /// [`budget::MAX_SCREEN_IMAGE_BYTES`].
    image_bytes: usize,
    /// Whether the missing-font warning has been logged.
    missing_font_logged: bool,
    ticks: u64,
    fade: u8,
    outcome: Option<SaveOutcome>,
    load: Option<PathBuf>,
    exited: bool,
    failed: bool,
    images: HashMap<String, Decoded>,
    /// The layout space the script lays itself out in (`api:width()`).
    layout: (u32, u32),
    /// The action a named `api:exit(action)` chose.
    action: Option<ScreenAction>,
    /// The overlay `api:overlay` set; `None` keeps the black-fade default.
    overlay: Option<Option<(FadeColor, u8)>>,
    /// Values the engine reads back, set with `api:set(key, value)`.
    values: HashMap<String, i64>,
    /// Art loaded by handle (`api:picture`, `api:page`).
    assets: draw::Assets,
}

impl Host {
    /// Report a text call on a pack with no font, once per screen.
    fn note_missing_font(&mut self) {
        if !self.missing_font_logged {
            self.missing_font_logged = true;
            eprintln!("[lua] the pack carries no font; screen text is not drawn");
        }
    }

    /// Rescan the save directory.
    fn rescan(&mut self) {
        self.list = save::scan_dir(&self.save_dir);
    }

    /// The decoded pack image, cached. The cache is bounded by count and
    /// by the pixels it holds, so a script cannot pin memory with many
    /// large images.
    fn image<'a>(&'a mut self, pack: &Pack, entry: &str) -> Result<&'a Decoded> {
        if !self.images.contains_key(entry) {
            budget::check_len(
                self.images.len() + 1,
                budget::MAX_SCREEN_IMAGES,
                "screen image cache size",
            )?;
            let decoded = decode_image(pack, entry)?;
            let (width, height) = decoded.size();
            let bytes = (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4);
            budget::check_len(
                self.image_bytes.saturating_add(bytes),
                budget::MAX_SCREEN_IMAGE_BYTES,
                "screen image cache bytes",
            )?;
            self.image_bytes = self.image_bytes.saturating_add(bytes);
            self.images.insert(entry.to_string(), decoded);
        }
        Ok(self.images.get(entry).expect("just inserted"))
    }
}

/// A save/load screen driven by a Lua script.
pub struct LuaScreen {
    entry: String,
    host: Host,
    vm: ScreenVm,
}

impl LuaScreen {
    /// Load the `entry` script as the title's load screen.
    pub fn open_load(entry: &str, cx: &mut UiContext<'_>) -> Result<Self> {
        Self::open(entry, Mode::Load, &[], cx)
    }

    /// Open the engine screen `id`: the pack's `ui/<id>.lua`, or the
    /// checked-in copy when the pack predates it. `args` reach the script's
    /// `open(api, args)`; a script that fails while running reports `fail`.
    pub fn open_screen(
        id: &str,
        args: &[(&str, ScreenArg)],
        fail: ScreenAction,
        cx: &mut UiContext<'_>,
    ) -> Result<Self> {
        Self::open(&screen_entry(id), Mode::Screen { fail }, args, cx)
    }

    /// Load the `entry` script as a save screen writing `file`;
    /// `ink_ribbon` makes the write consume one.
    pub fn open_save(
        entry: &str,
        cx: &mut UiContext<'_>,
        file: SaveFile,
        ink_ribbon: bool,
    ) -> Result<Self> {
        Self::open(
            entry,
            Mode::Save {
                file: Box::new(file),
                ink_ribbon,
            },
            &[],
            cx,
        )
    }

    fn open(
        entry: &str,
        mode: Mode,
        args: &[(&str, ScreenArg)],
        cx: &mut UiContext<'_>,
    ) -> Result<Self> {
        let sources = Sources::collect(cx.pack, entry)?;
        let text = cx.text;
        let mut host = Host {
            mode,
            save_dir: cx.save_dir.to_path_buf(),
            script: text.map(Text::script).unwrap_or_default(),
            tight: text.map(Text::uses_tight_metrics).unwrap_or(true),
            list: Vec::new(),
            image_bytes: 0,
            missing_font_logged: false,
            ticks: 0,
            fade: 0,
            outcome: None,
            load: None,
            exited: false,
            failed: false,
            images: HashMap::new(),
            layout: (crate::render::WIDTH, crate::render::HEIGHT),
            action: None,
            overlay: None,
            values: HashMap::new(),
            assets: draw::Assets::default(),
        };
        let vm = ScreenVm::load(entry, Loader::new(sources))?;
        host.rescan();
        vm.call_open(&mut host, cx, args)
            .with_context(|| format!("screen script {entry} open failed"))?;
        Ok(Self {
            entry: entry.to_string(),
            host,
            vm,
        })
    }

    /// The completed save, once the script wrote one.
    pub fn take_outcome(&mut self) -> Option<SaveOutcome> {
        self.host.outcome.take()
    }

    /// A value the script published with `api:set(key, value)`; `0` when
    /// it never set one.
    pub fn value(&self, key: &str) -> i64 {
        self.host.values.get(key).copied().unwrap_or(0)
    }

    /// Run the script's `M.<name>(api)` hook, if it defines one: the engine's
    /// way to tell a running screen something happened (the pick-up viewer's
    /// `begin_exit`). A failing hook is logged and fails the screen.
    pub fn signal(&mut self, name: &str, cx: &UiContext<'_>) {
        self.signal_with(name, (), cx);
    }

    /// [`LuaScreen::signal`] with one argument after `api`: `M.<name>(api,
    /// value)` (the item box's `cursor_shown`).
    pub fn signal_with(&mut self, name: &str, value: impl mlua::IntoLuaMulti, cx: &UiContext<'_>) {
        let Self { host, vm, entry } = self;
        if let Err(err) = vm.call_hook(name, host, cx, value) {
            eprintln!("[lua] screen {entry} {name} failed: {err}");
            host.failed = true;
        }
    }

    /// [`Screen::draw`] with the engine's actors available to
    /// `api:draw_actor`.
    pub fn draw_with_actors(
        &mut self,
        cx: &UiContext<'_>,
        framebuffer: &mut Framebuffer,
        actors: Option<&mut ActorDraw<'_>>,
    ) {
        let Self { host, vm, entry } = self;
        // `api:clip` lasts for this draw only.
        let clip = framebuffer.clip_rect();
        if let Err(err) = vm.call_draw(host, cx, framebuffer, actors) {
            eprintln!("[lua] screen {entry} draw failed: {err}");
            host.failed = true;
        }
        framebuffer.set_clip_rect(clip);
    }

    /// The action an exit reports: the one the script named, else back to
    /// gameplay from a save screen, back to the title from the load screen
    /// and the opener's fallback from a named screen.
    fn exit_action(&mut self) -> ScreenAction {
        if !self.host.failed
            && let Some(action) = self.host.action.take()
        {
            return action;
        }
        match &self.host.mode {
            Mode::Save { .. } => ScreenAction::Resume,
            Mode::Load => ScreenAction::Title,
            Mode::Screen { fail } => fail.clone(),
        }
    }

    /// A Lua global, for the unit tests.
    #[cfg(test)]
    pub(super) fn test_global<T: mlua::FromLua>(&self, name: &str) -> T {
        self.vm
            .global(name)
            .unwrap_or_else(|err| panic!("global {name}: {err}"))
    }
}

#[cfg(test)]
impl LuaScreen {
    /// Evaluate `body` with the screen's module table bound to `M`.
    pub(crate) fn test_eval<T: mlua::FromLuaMulti>(&self, body: &str) -> T {
        self.vm
            .eval(body)
            .unwrap_or_else(|err| panic!("eval {body}: {err}"))
    }

    /// Publish `key` as if the script had called `api:set`.
    pub(crate) fn test_set_value(&mut self, key: &str, value: i64) {
        self.host.values.insert(key.to_owned(), value);
    }
}

impl Screen for LuaScreen {
    /// Nothing to load: [`LuaScreen::open_load`]/[`LuaScreen::open_save`]
    /// read the script and ran `on_open` already.
    fn open(&mut self, _cx: &mut UiContext<'_>) -> Result<()> {
        Ok(())
    }

    fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
        let Self { host, vm, entry } = self;
        if let Err(err) = vm.call_update(host, cx, input) {
            eprintln!("[lua] screen {entry} update failed: {err}");
            host.failed = true;
        }
        self.host.ticks = self.host.ticks.saturating_add(1);
        if let Some(path) = self.host.load.take() {
            return ScreenResult::Done(ScreenAction::LoadFile(path));
        }
        if self.host.exited || self.host.failed {
            return ScreenResult::Done(self.exit_action());
        }
        ScreenResult::Continue
    }

    fn draw(&mut self, cx: &UiContext<'_>, framebuffer: &mut Framebuffer) {
        self.draw_with_actors(cx, framebuffer, None);
    }

    fn fade(&self) -> u8 {
        self.host.fade
    }

    fn overlay(&self) -> Option<(FadeColor, u8)> {
        match self.host.overlay {
            Some(overlay) => overlay,
            None => match self.host.fade {
                0 => None,
                alpha => Some((FadeColor::Black, alpha)),
            },
        }
    }
}

impl crate::ui::save_load::SaveScreen for LuaScreen {
    fn take_outcome(&mut self) -> Option<SaveOutcome> {
        LuaScreen::take_outcome(self)
    }
}

/// The sandboxed VM one screen script runs in.
struct ScreenVm {
    lua: Lua,
    budget: Rc<Budget>,
    /// The entry chunk's environment, where a legacy script's globals live.
    #[cfg_attr(not(test), allow(dead_code))]
    env: Table,
    /// The table the entry chunk returned (the env for a legacy script).
    module: Table,
    on_open: Option<Function>,
    update: Option<Function>,
    draw: Option<Function>,
}

impl ScreenVm {
    /// Compile and run the entry chunk, then look up its entry points: the
    /// `open`/`update`/`draw` fields of the table it returns, or (the legacy
    /// form) the `on_open`/`update`/`draw` globals it defines.
    fn load(name: &str, loader: Loader) -> Result<Self> {
        let (lua, budget) = new_sandboxed_state()?;
        budget.begin();
        let source = loader
            .source(name)
            .with_context(|| format!("cannot read screen script {name}"))?;
        let env = loader
            .env(&lua, name)
            .map_err(|err| anyhow::anyhow!("{name}: {err}"))?;
        let returned: Value = lua
            .load(source)
            .set_name(name.to_string())
            .set_environment(env.clone())
            .eval()
            .map_err(|err| anyhow::anyhow!("{name}: {err}"))?;
        let table = match returned {
            Value::Table(table) => table,
            _ => env.clone(),
        };
        let lookup = |name: &str| match table.get::<Value>(name) {
            Ok(Value::Function(function)) => Some(function),
            _ => None,
        };
        let update = lookup("update");
        if update.is_none() {
            anyhow::bail!("{name} defines no update(api, input), so it could never leave");
        }
        Ok(Self {
            on_open: lookup("open").or_else(|| lookup("on_open")),
            update,
            draw: lookup("draw"),
            lua,
            budget,
            env,
            module: table,
        })
    }

    /// Run `open(api, args)`.
    fn call_open(
        &self,
        host: &mut Host,
        cx: &UiContext<'_>,
        args: &[(&str, ScreenArg)],
    ) -> Result<()> {
        let Some(function) = &self.on_open else {
            return Ok(());
        };
        let table = args_table(&self.lua, args).map_err(|err| anyhow::anyhow!("{err}"))?;
        self.budget.begin();
        self.lua
            .scope(|scope| {
                let api = scope.create_userdata(Api {
                    host: &mut *host,
                    cx,
                    framebuffer: None,
                    actors: None,
                })?;
                function.call::<()>((api, table))
            })
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(())
    }

    /// Run `update(api, input)`.
    fn call_update(&self, host: &mut Host, cx: &UiContext<'_>, input: UiInput) -> Result<()> {
        let Some(function) = &self.update else {
            return Ok(());
        };
        let table = input_table(&self.lua, input).map_err(|err| anyhow::anyhow!("{err}"))?;
        self.budget.begin();
        self.lua
            .scope(|scope| {
                let api = scope.create_userdata(Api {
                    host: &mut *host,
                    cx,
                    framebuffer: None,
                    actors: None,
                })?;
                function.call::<()>((api, table))
            })
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(())
    }

    /// Run the module's `name(api)`, when it is a function.
    fn call_hook(
        &self,
        name: &str,
        host: &mut Host,
        cx: &UiContext<'_>,
        value: impl mlua::IntoLuaMulti,
    ) -> Result<()> {
        let Ok(Value::Function(function)) = self.module.get::<Value>(name) else {
            return Ok(());
        };
        self.budget.begin();
        self.lua
            .scope(|scope| {
                let api = scope.create_userdata(Api {
                    host: &mut *host,
                    cx,
                    framebuffer: None,
                    actors: None,
                })?;
                let mut args = mlua::IntoLuaMulti::into_lua_multi(value, &self.lua)?;
                args.push_front(Value::UserData(api));
                function.call::<()>(args)
            })
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(())
    }

    /// Run `draw(api)`.
    fn call_draw(
        &self,
        host: &mut Host,
        cx: &UiContext<'_>,
        framebuffer: &mut Framebuffer,
        actors: Option<&mut ActorDraw<'_>>,
    ) -> Result<()> {
        let Some(function) = &self.draw else {
            return Ok(());
        };
        self.budget.begin();
        self.lua
            .scope(|scope| {
                let api = scope.create_userdata(Api {
                    host: &mut *host,
                    cx,
                    framebuffer: Some(&mut *framebuffer),
                    // Shorten the closure's bound to the scope's lifetime.
                    actors: actors.map(|draw| draw as &mut ActorDraw<'_>),
                })?;
                function.call::<()>(api)
            })
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        Ok(())
    }

    /// Evaluate `body` with the module table bound to `M` in the scripts'
    /// environment, for the unit tests.
    #[cfg(test)]
    fn eval<T: mlua::FromLuaMulti>(&self, body: &str) -> mlua::Result<T> {
        let function: Function = self
            .lua
            .load(format!("return function(M) {body} end"))
            .set_environment(self.env.clone())
            .eval()?;
        function.call(self.module.clone())
    }

    /// A Lua global, for the unit tests.
    #[cfg(test)]
    fn global<T: mlua::FromLua>(&self, name: &str) -> mlua::Result<T> {
        self.env.get(name)
    }
}

/// The pack entry of engine screen `id`.
pub fn screen_entry(id: &str) -> String {
    format!("ui/{id}.lua")
}

/// The `args` table one `open` call receives.
fn args_table(lua: &Lua, args: &[(&str, ScreenArg)]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (key, value) in args {
        match value {
            ScreenArg::Int(value) => table.set(*key, *value)?,
            ScreenArg::Bool(value) => table.set(*key, *value)?,
            ScreenArg::Str(value) => table.set(*key, value.as_str())?,
        }
    }
    Ok(table)
}

/// The `input` table one `update` call receives.
fn input_table(lua: &Lua, input: UiInput) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("up", input.up)?;
    table.set("down", input.down)?;
    table.set("left", input.left)?;
    table.set("right", input.right)?;
    table.set("confirm", input.confirm)?;
    table.set("cancel", input.cancel)?;
    table.set("page_left", input.page_left)?;
    table.set("page_right", input.page_right)?;
    table.set("start", input.start)?;
    table.set("any", input.any)?;
    table.set("return_title", input.return_title)?;
    table.set("pad", pad_word(input))?;
    table.set("held", held_word(input))?;
    Ok(table)
}

/// The `api` userdata handed to one script call.
///
/// It borrows the persistent host, the screen context and — during
/// `draw` — the framebuffer. Non-`'static` userdata is exactly what
/// `Lua::scope` supports, so the surface needs no raw pointers.
struct Api<'a> {
    host: &'a mut Host,
    cx: &'a UiContext<'a>,
    framebuffer: Option<&'a mut Framebuffer>,
    actors: Option<&'a mut ActorDraw<'a>>,
}

/// Draws an engine-owned actor (the DIED screen's corpse) named by
/// `api:draw_actor` through the screen's camera; returns whether it drew.
pub type ActorDraw<'a> = dyn FnMut(&str, &crate::render::Camera, &mut Framebuffer) -> bool + 'a;

/// The draw options table shared by `text`, `sprite` and friends.
struct Options {
    tint: Tint,
    brightness: u8,
    tight: bool,
    clut: usize,
    blend: SpriteBlend,
}

/// Parse an options table: `{colour = "green"|3, brightness = 2,
/// tight = true, clut = 0, blend = "opaque"}`.
fn options(host: &Host, table: Option<&Table>) -> mlua::Result<Options> {
    let mut parsed = Options {
        tint: Tint::White,
        brightness: 2,
        tight: host.tight,
        clut: 0,
        blend: SpriteBlend::Opaque,
    };
    let Some(table) = table else {
        return Ok(parsed);
    };
    if let Some(value) = table.get::<Option<Value>>("colour")? {
        parsed.tint = colour_tint(&value)?;
    }
    if let Some(brightness) = table.get::<Option<i64>>("brightness")? {
        parsed.brightness = brightness.clamp(0, 30) as u8;
    }
    if let Some(tight) = table.get::<Option<bool>>("tight")? {
        parsed.tight = tight;
    }
    if let Some(clut) = table.get::<Option<i64>>("clut")? {
        parsed.clut = clut.clamp(0, 255) as usize;
    }
    if let Some(blend) = table.get::<Option<String>>("blend")? {
        parsed.blend = match blend.to_ascii_lowercase().as_str() {
            "opaque" => SpriteBlend::Opaque,
            "half" => SpriteBlend::Half,
            "half_variant" | "variant" => SpriteBlend::HalfVariant,
            other => {
                return Err(mlua::Error::RuntimeError(format!(
                    "unknown blend `{other}` (opaque, half, half_variant)"
                )));
            }
        };
    }
    Ok(parsed)
}

/// A text colour: a tint name, a CLUT row number or `"#RRGGBB"`.
pub(super) fn colour_tint(value: &Value) -> mlua::Result<Tint> {
    match value {
        Value::Integer(row) => Ok(Tint::from_clut_row(*row as i32)),
        Value::String(text) => {
            let text = text.to_string_lossy();
            if let Some(hex) = text.strip_prefix('#') {
                return parse_hex_colour(hex)
                    .map(|[red, green, blue]| Tint::Rgb(red, green, blue))
                    .ok_or_else(|| mlua::Error::RuntimeError(format!("invalid colour `{text}`")));
            }
            match tint_named(&text) {
                Some(tint) => Ok(tint),
                None => Err(mlua::Error::RuntimeError(format!(
                    "unknown colour `{text}` (white, green, red, grey, yellow, a CLUT row, or \"#RRGGBB\")"
                ))),
            }
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "expected a colour name or CLUT row, got {}",
            other.type_name()
        ))),
    }
}

/// A tint by name.
fn tint_named(name: &str) -> Option<Tint> {
    Some(match name.to_ascii_lowercase().as_str() {
        "white" => Tint::White,
        "green" => Tint::Green,
        "red" => Tint::Red,
        "grey" | "gray" => Tint::Grey,
        "yellow" => Tint::Yellow,
        _ => return None,
    })
}

/// An RGB colour: `0xRRGGBB`, `"#RRGGBB"`, or a tint name.
pub(super) fn colour_rgb(value: &Value) -> mlua::Result<[u8; 3]> {
    match value {
        Value::Integer(rgb) => {
            let rgb = *rgb as u32;
            Ok([(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8])
        }
        Value::String(text) => {
            let text = text.to_string_lossy();
            if let Some(hex) = text.strip_prefix('#') {
                return parse_hex_colour(hex)
                    .ok_or_else(|| mlua::Error::RuntimeError(format!("invalid colour `{text}`")));
            }
            match tint_named(&text) {
                Some(tint) => Ok(tint.rgb()),
                None => Err(mlua::Error::RuntimeError(format!(
                    "invalid colour `{text}` (0xRRGGBB, \"#RRGGBB\" or a tint name)"
                ))),
            }
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "expected 0xRRGGBB or \"#RRGGBB\", got {}",
            other.type_name()
        ))),
    }
}

/// Parse `RRGGBB` or `RRGGBBAA` (alpha ignored).
///
/// The input is checked as ASCII hex digits byte for byte: indexing a
/// `str` by byte would panic on a multi-byte character.
pub(super) fn parse_hex_colour(hex: &str) -> Option<[u8; 3]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 6 && bytes.len() != 8 {
        return None;
    }
    let digit = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8);
    let byte = |index: usize| Some(digit(bytes[index])? << 4 | digit(bytes[index + 1])?);
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// Decode a pack image, resolving the entry the way every other screen
/// does: by its stem, so a mod's `webp`/`png`/`bmp`/`tim` replacement
/// wins like it does for the shipped art, and a `tim` keeps its palette
/// when it is indexed.
fn decode_image(pack: &Pack, entry: &str) -> Result<Decoded> {
    let asset = pack
        .read_asset(crate::assets::stem(entry), crate::pack::AssetClass::Image)?
        .with_context(|| format!("cannot read image {entry}"))?;
    match crate::assets::decode_page(&asset, crate::assets::Geometry::Free)? {
        crate::assets::Artwork::Indexed(texture) => Ok(Decoded::Indexed(texture)),
        crate::assets::Artwork::Rgba(image) => Ok(Decoded::Rgba(image)),
    }
}

/// A drawing coordinate or size a script supplied, clamped to a range no
/// screen needs and no cursor arithmetic can overflow.
fn coord(value: i32) -> i32 {
    value.clamp(-DRAW_LIMIT, DRAW_LIMIT)
}

/// Draw one decoded image at `dst`, scaling `src` into it.
fn draw_image(
    framebuffer: &mut Framebuffer,
    image: &Decoded,
    src: Option<[i32; 4]>,
    dst: [i32; 4],
    opts: &Options,
) {
    let (width, height) = image.size();
    let src = src.unwrap_or([0, 0, width as i32, height as i32]);
    match image {
        Decoded::Rgba(image) => {
            if opts.blend == SpriteBlend::Opaque {
                framebuffer.draw_rgba_sprite(image, src, dst, opts.brightness);
            } else {
                framebuffer.draw_rgba_sprite_blended(image, src, dst, opts.brightness, opts.blend);
            }
        }
        Decoded::Indexed(texture) => {
            if opts.blend == SpriteBlend::Opaque {
                framebuffer.draw_indexed_sprite(
                    texture,
                    src,
                    dst,
                    opts.clut,
                    opts.brightness,
                    opts.tint,
                );
            } else {
                framebuffer.draw_indexed_sprite_blended(
                    texture,
                    src,
                    dst,
                    opts.clut,
                    opts.brightness,
                    opts.tint,
                    opts.blend,
                );
            }
        }
    }
}

/// The original pad word of the input's edge flags, the same bit layout
/// the film and attract paths consume.
pub(super) fn pad_word(input: UiInput) -> u16 {
    let mut word = 0u16;
    for (flag, bit) in [
        (input.confirm, 0x0001),
        (input.cancel, 0x0002),
        (input.start, 0x0008),
        (input.up, 0x0010),
        (input.down, 0x0020),
        (input.left, 0x0040),
        (input.right, 0x0080),
        (input.page_left, 0x0400),
        (input.page_right, 0x0800),
    ] {
        if flag {
            word |= bit;
        }
    }
    word
}

/// The pad word of the held direction keys.
pub(super) fn held_word(input: UiInput) -> u16 {
    let mut word = 0u16;
    for (flag, bit) in [
        (input.held_up, 0x0010),
        (input.held_down, 0x0020),
        (input.held_left, 0x0040),
        (input.held_right, 0x0080),
    ] {
        if flag {
            word |= bit;
        }
    }
    word
}

/// The cue a script name selects.
fn cue_of(name: &str) -> Option<UiCue> {
    Some(match name {
        "cursor" => UiCue::Cursor,
        "cancel" | "error" => UiCue::Cancel,
        "decide" => UiCue::Decide,
        "title" => UiCue::Title,
        "select_cursor" => UiCue::SelectCursor,
        "select_confirm" => UiCue::SelectConfirm,
        "save_cursor" => UiCue::SaveCursor,
        "save_confirm" => UiCue::SaveConfirm,
        _ => return None,
    })
}

impl UserData for Api<'_> {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // ---- queries -------------------------------------------------
        methods.add_method("mode", |_lua, api, ()| {
            Ok(match api.host.mode {
                Mode::Load => "load",
                Mode::Save { .. } => "save",
                Mode::Screen { .. } => "screen",
            })
        });

        methods.add_method("save_slots", |lua, api, ()| {
            let table = lua.create_table()?;
            for (index, slot) in save::scan_slots(&api.host.save_dir).iter().enumerate() {
                table.set(index + 1, slot.is_some())?;
            }
            Ok(table)
        });

        methods.add_method("ticks", |_lua, api, ()| Ok(api.host.ticks));
        // The eight numbered slots: `false` for an empty one, else
        // `{character, saves, stage, room}`.
        methods.add_method("slot_info", |lua, api, ()| {
            let table = lua.create_table()?;
            for (index, slot) in save::scan_slots(&api.host.save_dir).iter().enumerate() {
                match slot {
                    Some(info) => table.set(index + 1, slot_table(lua, info)?)?,
                    None => table.set(index + 1, false)?,
                }
            }
            Ok(table)
        });

        // `flag(bank, bit)`: one bit of the paused session's flag banks;
        // false outside a session.
        methods.add_method("flag", |_lua, api, (bank, bit): (usize, u8)| {
            Ok(api
                .cx
                .game
                .and_then(|game| game.flags.get(bank))
                .is_some_and(|flags| flags.bit(bit)))
        });

        // `room()`: the paused session's stage, room, camera cut and player
        // digit (0 Chris, 1 Jill); nil outside a session.
        methods.add_method("room", |_lua, api, ()| {
            Ok(match api.cx.game {
                Some(game) => (
                    Some(game.id.stage),
                    Some(game.id.room),
                    Some(game.camera.current_cut as i64),
                    Some(game.id.player_flag),
                ),
                None => (None, None, None, None),
            })
        });

        // `pack_bytes(entry)`: a pack entry's raw bytes, nil when absent.
        methods.add_method("pack_bytes", |lua, api, entry: String| {
            match api.cx.pack.read(&entry) {
                Ok(bytes) => {
                    budget::check_len(bytes.len(), budget::MAX_SCREEN_IMAGE_BYTES, "pack entry")
                        .map_err(mlua::Error::external)?;
                    Ok(Some(lua.create_string(&bytes)?))
                }
                Err(_) => Ok(None),
            }
        });

        methods.add_method("has_font", |_lua, api, ()| Ok(api.cx.font.is_some()));
        methods.add_method("has_text", |_lua, api, ()| Ok(api.cx.text.is_some()));

        // The font-encoded bytes of the text table's `key`; nil without a
        // text table. Draw them with `api:glyphs`.
        methods.add_method("ui_bytes", |lua, api, key: String| {
            api.cx
                .text
                .map(|text| lua.create_string(text.ui(&key)))
                .transpose()
        });

        // The font-encoded name of item `id`; nil without a text table or
        // a name.
        // `item_name_bytes(id, examined)`: with `examined`, the menu's name:
        // an unchecked item whose class the session has not examined yet
        // keeps its unchecked name.
        methods.add_method(
            "item_name_bytes",
            |lua, api, (id, examined): (u16, Option<bool>)| {
                let Some(text) = api.cx.text else {
                    return Ok(None);
                };
                let name = match (examined.unwrap_or(false), u8::try_from(id), api.cx.game) {
                    (true, Ok(item), Some(game)) => {
                        crate::ui::main_menu::item_name_bytes(text, item, &game.examined_flags())
                    }
                    _ => text.item_name(id),
                };
                name.map(|name| lua.create_string(name)).transpose()
            },
        );

        // `font_margin()`: the session font's left text margin.
        methods.add_method("font_margin", |_lua, api, ()| {
            Ok(api.cx.font.map_or(0, |font| font.metrics.left_margin))
        });

        // `message_active()`: whether the game's message window is up.
        methods.add_method("message_active", |_lua, api, ()| {
            Ok(api.cx.game.is_some_and(|game| game.message.active))
        });

        // `examine_spin(pitch, yaw, roll, sx, sy, sz)`: the turntable pose
        // after pre-multiplying the spin, re-extracted the reference way.
        methods.add_method(
            "examine_spin",
            |_lua, _api, (pitch, yaw, roll, sx, sy, sz): (i32, i32, i32, i32, i32, i32)| {
                let [pitch, yaw, roll] = crate::examine::spin([pitch, yaw, roll], [sx, sy, sz]);
                Ok((pitch, yaw, roll))
            },
        );

        // `examine_check(item, pitch, yaw, roll, zoom_spin, quantity)`: the
        // examine check, as `kind, id, mark, select`: `"description"`,
        // `"zoom"` or `"message"` with its global message id, whether it
        // marks the item examined and the item `\i` names.
        methods.add_method(
            "examine_check",
            |_lua, api, (item, pitch, yaw, roll, zoom_spin, quantity): (u8, i32, i32, i32, i32, u8)| {
                let item13_message = api.cx.game.is_some_and(|game| {
                    game.flag_test(crate::game::BANK_SCENARIO, 0x13, false)
                        || game.flag_test(crate::game::BANK_SCENARIO, 0x00, false)
                });
                let context = crate::examine::ExamineContext {
                    slot_quantity: quantity,
                    item13_message,
                };
                Ok(
                    match crate::examine::examine_check(item, [pitch, yaw, roll], zoom_spin, context) {
                        crate::examine::ConfirmOutcome::Description => {
                            ("description", None, false, None)
                        }
                        crate::examine::ConfirmOutcome::Zoom => ("zoom", None, false, None),
                        crate::examine::ConfirmOutcome::Message {
                            id,
                            mark_examined,
                            select,
                        } => ("message", Some(id), mark_examined, select),
                    },
                )
            },
        );

        // `class_message(item)`: the global message the item's examine
        // class shows; nil when it has none.
        methods.add_method("class_message", |_lua, _api, item: u8| {
            Ok(crate::examine::class_message(item))
        });

        // `inventory(slot)`: the id and quantity of inventory slot `slot`;
        // `0, 0` past the held run or outside a session.
        methods.add_method("inventory", |_lua, api, slot: usize| {
            let stack = api
                .cx
                .game
                .and_then(|game| game.inventory.get(slot).copied())
                .unwrap_or_default();
            Ok((stack.id, stack.quantity))
        });

        // `inventory_capacity()`: the current character's inventory slots;
        // Chris's six outside a session.
        methods.add_method("inventory_capacity", |_lua, api, ()| {
            Ok(api
                .cx
                .game
                .map_or(crate::game::INVENTORY_SLOTS_CHRIS, |game| {
                    game.inventory_capacity()
                }))
        });

        // `item_box(slot)`: the id and quantity of item-box slot `slot`;
        // `0, 0` for an out-of-range slot or outside a session.
        methods.add_method("item_box", |_lua, api, slot: usize| {
            let stack = api
                .cx
                .game
                .and_then(|game| game.item_box.get(slot).copied())
                .unwrap_or_default();
            Ok((stack.id, stack.quantity))
        });

        // `item_icon_row(item)`: the item's 40x30 row in `item/item_all`, or
        // nil when it has no icon there.
        methods.add_method("item_icon_row", |_lua, _api, item: u8| {
            let class = crate::items::image_class(item);
            let row = usize::from(class.wrapping_sub(1));
            Ok((class != 0 && row < crate::items::ITEM_ALL_ROWS).then_some(row))
        });

        // The font-encoded shorthand location label of a save's room.
        methods.add_method("location_bytes", |lua, api, (stage, room): (u8, u8)| {
            api.cx
                .text
                .map(|text| lua.create_string(text.save().location(stage, room)))
                .transpose()
        });

        methods.add_method("saves", |lua, api, ()| {
            let table = lua.create_table()?;
            for (index, entry) in api.host.list.iter().enumerate() {
                let row = lua.create_table()?;
                row.set("path", entry.path.to_string_lossy().to_string())?;
                row.set("name", entry.name.clone())?;
                row.set("base", entry.base.clone())?;
                row.set("loadable", entry.loadable())?;
                row.set("timestamp", entry.timestamp)?;
                if let Some(info) = entry.info {
                    row.set("character", info.character)?;
                    row.set("saves", info.saves)?;
                    row.set("stage", info.stage)?;
                    row.set("room", info.room)?;
                    if let Some(text) = api.cx.text {
                        let save = text.save();
                        row.set(
                            "character_name",
                            text.decode(save.character_name(usize::from(info.character & 1))),
                        )?;
                        row.set(
                            "location",
                            text.decode(save.location(info.stage, info.room)),
                        )?;
                    }
                }
                table.set(index + 1, row)?;
            }
            Ok(table)
        });

        methods.add_method("string", |_lua, api, key: String| {
            Ok(api.cx.text.and_then(|text| text.text(&key)))
        });

        methods.add_method("character_name", |_lua, api, index: usize| {
            Ok(api
                .cx
                .text
                .map(|text| text.decode(text.save().character_name(index & 1))))
        });

        methods.add_method_mut(
            "text_width",
            |_lua, api, (text, tight): (String, Option<bool>)| {
                let Some(font) = api.cx.font else {
                    api.host.note_missing_font();
                    return Ok(0);
                };
                let mut bytes = Vec::new();
                charset::encode(&text, api.host.script, &mut bytes);
                Ok(font.advance(tight.unwrap_or(api.host.tight), &bytes))
            },
        );

        // ---- actions -------------------------------------------------
        methods.add_method("sound", |_lua, api, name: String| {
            let Some(cue) = cue_of(&name) else {
                eprintln!("[lua] save screen: unknown sound `{name}`");
                return Ok(());
            };
            api.cx.play_cue(cue);
            Ok(())
        });

        methods.add_method_mut("save", |_lua, api, name: String| {
            let Some(path) = save::file_path(&api.host.save_dir, &name) else {
                return Ok((
                    false,
                    Some(format!("`{name}` is not a valid save file name")),
                ));
            };
            let Mode::Save { file, ink_ribbon } = &api.host.mode else {
                return Ok((false, Some("the screen is not in save mode".to_string())));
            };
            let mut file = file.clone();
            if *ink_ribbon {
                consume_snapshot_ribbon(&mut file);
            }
            match save::save_to_path(&path, &file) {
                Ok(()) => {
                    api.host.outcome = Some(SaveOutcome {
                        slot: None,
                        ink_ribbon: *ink_ribbon,
                    });
                    api.host.rescan();
                    Ok((true, None))
                }
                Err(err) => Ok((false, Some(format!("{err:#}")))),
            }
        });

        // Write the pending block to numbered slot `slot` (0-7), consuming
        // an ink ribbon when the opener asked: `true, info` with the
        // written slot's `{character, saves, stage, room}`, or `false, err`.
        methods.add_method_mut("save_slot", |lua, api, slot: usize| {
            let Mode::Save { file, ink_ribbon } = &api.host.mode else {
                return Ok((
                    false,
                    Value::String(lua.create_string("the screen is not in save mode")?),
                ));
            };
            if slot >= save::SAVE_SLOT_COUNT {
                return Ok((
                    false,
                    Value::String(lua.create_string(format!("no save slot {slot}"))?),
                ));
            }
            let mut file = file.clone();
            if *ink_ribbon {
                consume_snapshot_ribbon(&mut file);
            }
            match save::save(&api.host.save_dir, slot, &file) {
                Ok(()) => {
                    api.host.outcome = Some(SaveOutcome {
                        slot: Some(slot),
                        ink_ribbon: *ink_ribbon,
                    });
                    api.host.rescan();
                    let info = save::SaveSlotInfo::from_file(&file);
                    Ok((true, Value::Table(slot_table(lua, &info)?)))
                }
                Err(err) => Ok((false, Value::String(lua.create_string(format!("{err:#}"))?))),
            }
        });

        methods.add_method_mut("remove", |_lua, api, name: String| {
            let Some(path) = save::file_path(&api.host.save_dir, &name) else {
                return Ok((
                    false,
                    Some(format!("`{name}` is not a valid save file name")),
                ));
            };
            match save::remove_file(&path) {
                Ok(()) => {
                    api.host.rescan();
                    Ok((true, None))
                }
                Err(err) => Ok((false, Some(format!("{err:#}")))),
            }
        });

        methods.add_method_mut("load", |_lua, api, name: String| {
            if !matches!(api.host.mode, Mode::Load) {
                return Ok((false, Some("the screen is not in load mode".to_string())));
            }
            let Some(path) = save::file_path(&api.host.save_dir, &name) else {
                return Ok((
                    false,
                    Some(format!("`{name}` is not a valid save file name")),
                ));
            };
            if save::load_file(&path).is_err() {
                return Ok((false, Some(format!("`{name}` is not a readable save file"))));
            }
            api.host.load = Some(path);
            Ok((true, None))
        });

        methods.add_method_mut(
            "exit",
            |_lua, api, (action, arg): (Option<String>, Option<i64>)| {
                if let Some(name) = action {
                    api.host.action = Some(action_of(&name, arg)?);
                }
                api.host.exited = true;
                Ok(())
            },
        );

        methods.add_method_mut("set", |_lua, api, (key, value): (String, Value)| {
            let value = match value {
                Value::Nil => {
                    api.host.values.remove(&key);
                    return Ok(());
                }
                Value::Boolean(flag) => i64::from(flag),
                Value::Integer(value) => value,
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "api:set({key}) takes an integer or boolean, got {}",
                        other.type_name()
                    )));
                }
            };
            budget::check_len(api.host.values.len() + 1, 64, "screen values")
                .map_err(|err| mlua::Error::RuntimeError(format!("{err:#}")))?;
            api.host.values.insert(key, value);
            Ok(())
        });

        methods.add_method_mut(
            "overlay",
            |_lua, api, (colour, alpha): (Option<String>, Option<i64>)| {
                let alpha = alpha.unwrap_or(0).clamp(0, 255) as u8;
                let overlay = match colour.as_deref() {
                    None => None,
                    Some("black") => Some((FadeColor::Black, alpha)),
                    Some("white") => Some((FadeColor::White, alpha)),
                    Some(other) => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "unknown overlay colour `{other}` (black, white or nil)"
                        )));
                    }
                };
                api.host.fade = match overlay {
                    Some((FadeColor::Black, alpha)) => alpha,
                    _ => 0,
                };
                api.host.overlay = Some(overlay);
                Ok(())
            },
        );

        methods.add_method("log", |_lua, _api, text: mlua::String| {
            eprintln!("[lua] {}", text.to_string_lossy());
            Ok(())
        });

        // ---- drawing -------------------------------------------------
        methods.add_method_mut(
            "background",
            |_lua, api, (entry, opts): (String, Option<Table>)| {
                let opts = options(api.host, opts.as_ref())?;
                let Api {
                    host,
                    cx,
                    framebuffer,
                    ..
                } = api;
                let framebuffer = framebuffer.as_deref_mut().ok_or_else(no_draw)?;
                let image = host.image(cx.pack, &entry).map_err(mlua::Error::external)?;
                let (width, height) = image.size();
                let full = [0, 0, framebuffer.width as i32, framebuffer.height as i32];
                if let Decoded::Rgba(image) = image
                    && width == framebuffer.width
                    && height == framebuffer.height
                    && opts.tint == Tint::White
                    && opts.brightness == 2
                {
                    framebuffer.blit(image);
                    return Ok(());
                }
                if let Decoded::Rgba(image) = image {
                    framebuffer.draw_background(image, [0, 0], opts.tint.rgb());
                    return Ok(());
                }
                draw_image(framebuffer, image, None, full, &opts);
                Ok(())
            },
        );

        methods.add_method_mut(
            "rect",
            |_lua, api, (x, y, w, h, colour, alpha): (i32, i32, i32, i32, Value, Option<i64>)| {
                let (x, y) = (coord(x), coord(y));
                let (w, h) = (coord(w), coord(h));
                let rgb = colour_rgb(&colour)?;
                let alpha = alpha.unwrap_or(255).clamp(0, 255) as u8;
                let framebuffer = api.framebuffer.as_deref_mut().ok_or_else(no_draw)?;
                if alpha == 255 {
                    framebuffer.fill_rect([x, y, w, h], [rgb[0], rgb[1], rgb[2], 255]);
                } else {
                    framebuffer.blend_rect([x, y, w, h], rgb, alpha);
                }
                Ok(())
            },
        );

        methods.add_method_mut(
            "text",
            |_lua, api, (x, y, text, opts): (i32, i32, String, Option<Table>)| {
                let (x, y) = (coord(x), coord(y));
                let opts = options(api.host, opts.as_ref())?;
                // The framebuffer is taken first so a text call outside
                // `draw` is the error the API documents.
                let framebuffer = api.framebuffer.as_deref_mut().ok_or_else(no_draw)?;
                // A pack without a font draws no text anywhere in the
                // engine; the screen keeps running rather than dying.
                let Some(font) = api.cx.font else {
                    api.host.note_missing_font();
                    return Ok(());
                };
                let mut bytes = Vec::new();
                charset::encode(&text, api.host.script, &mut bytes);
                font.draw_text(
                    framebuffer,
                    x,
                    y,
                    opts.tint,
                    opts.brightness,
                    &bytes,
                    opts.tight,
                );
                Ok(())
            },
        );

        methods.add_method_mut(
            "sprite",
            |_lua, api, (entry, x, y, opts): (String, i32, i32, Option<Table>)| {
                let (x, y) = (coord(x), coord(y));
                let opts = options(api.host, opts.as_ref())?;
                let Api {
                    host,
                    cx,
                    framebuffer,
                    ..
                } = api;
                let framebuffer = framebuffer.as_deref_mut().ok_or_else(no_draw)?;
                let image = host.image(cx.pack, &entry).map_err(mlua::Error::external)?;
                let (width, height) = image.size();
                draw_image(
                    framebuffer,
                    image,
                    None,
                    [x, y, width as i32, height as i32],
                    &opts,
                );
                Ok(())
            },
        );

        #[allow(clippy::too_many_arguments)]
        methods.add_method_mut(
            "sprite_region",
            |_lua,
             api,
             (entry, sx, sy, sw, sh, dx, dy, dw, dh, opts): (
                String,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                Option<Table>,
            )| {
                let (sx, sy, sw, sh) = (coord(sx), coord(sy), coord(sw), coord(sh));
                let (dx, dy, dw, dh) = (coord(dx), coord(dy), coord(dw), coord(dh));
                let opts = options(api.host, opts.as_ref())?;
                let Api {
                    host,
                    cx,
                    framebuffer,
                    ..
                } = api;
                let framebuffer = framebuffer.as_deref_mut().ok_or_else(no_draw)?;
                let image = host.image(cx.pack, &entry).map_err(mlua::Error::external)?;
                draw_image(
                    framebuffer,
                    image,
                    Some([sx, sy, sw, sh]),
                    [dx, dy, dw, dh],
                    &opts,
                );
                Ok(())
            },
        );

        // `wrap(bytes, max_width, opts)`: font-encoded `bytes` broken at
        // spaces into rows no wider than `max_width` pixels (`opts.tight`
        // picks the metrics). Without a font the text is one row.
        methods.add_method(
            "wrap",
            |lua, api, (bytes, max_width, opts): (mlua::String, i32, Option<Table>)| {
                let opts = options(api.host, opts.as_ref())?;
                let bytes = bytes.as_bytes();
                let table = lua.create_table()?;
                match api.cx.font {
                    Some(font) => {
                        for (index, row) in
                            crate::font::wrap_pixels(&bytes, max_width, font, opts.tight)
                                .into_iter()
                                .enumerate()
                        {
                            table.set(index + 1, lua.create_string(row)?)?;
                        }
                    }
                    None => table.set(1, lua.create_string(&*bytes)?)?,
                }
                Ok(table)
            },
        );

        // `glyphs(x, y, bytes, opts)`: draw already font-encoded bytes (from
        // `api:ui_bytes`, `api:location_bytes` or assembled by the script).
        methods.add_method_mut(
            "glyphs",
            |_lua, api, (x, y, bytes, opts_table): (i32, i32, mlua::String, Option<Table>)| {
                let (x, y) = (coord(x), coord(y));
                let opts = options(api.host, opts_table.as_ref())?;
                let framebuffer = api.framebuffer.as_deref_mut().ok_or_else(no_draw)?;
                let Some(font) = api.cx.font else {
                    api.host.note_missing_font();
                    return Ok(());
                };
                // `fit` condenses a row wider than that many pixels.
                let fit = match &opts_table {
                    Some(table) => table.get::<Option<i32>>("fit")?.unwrap_or(0),
                    None => 0,
                };
                font.draw_text_fit(
                    framebuffer,
                    x,
                    y,
                    opts.tint,
                    opts.brightness,
                    &bytes.as_bytes(),
                    opts.tight,
                    fit,
                );
                Ok(())
            },
        );

        // `text_8x14(x, y, bytes[, opts])`: raw bytes through the original's
        // unshadowed `PrintText8x14` grid, in `opts.colour`.
        methods.add_method_mut(
            "text_8x14",
            |_lua, api, (x, y, bytes, opts): (i32, i32, mlua::String, Option<Table>)| {
                let (x, y) = (coord(x), coord(y));
                let opts = options(api.host, opts.as_ref())?;
                let framebuffer = api.framebuffer.as_deref_mut().ok_or_else(no_draw)?;
                let Some(font) = api.cx.font else {
                    api.host.note_missing_font();
                    return Ok(());
                };
                font.draw_text_8x14(framebuffer, x, y, opts.tint, &bytes.as_bytes());
                Ok(())
            },
        );

        methods.add_method_mut("image_size", |_lua, api, entry: String| {
            let image = api
                .host
                .image(api.cx.pack, &entry)
                .map_err(mlua::Error::external)?;
            Ok(image.size())
        });

        methods.add_method_mut("fade", |_lua, api, alpha: i64| {
            api.host.fade = alpha.clamp(0, 255) as u8;
            api.host.overlay = None;
            Ok(())
        });

        draw::add_methods(methods);
    }
}

/// A save slot's header fields as a Lua table.
fn slot_table(lua: &Lua, info: &save::SaveSlotInfo) -> mlua::Result<Table> {
    let row = lua.create_table()?;
    row.set("character", info.character)?;
    row.set("saves", info.saves)?;
    row.set("stage", info.stage)?;
    row.set("room", info.room)?;
    Ok(row)
}

/// The action an `api:exit(name, arg)` names.
fn action_of(name: &str, arg: Option<i64>) -> mlua::Result<ScreenAction> {
    let arg = |what: &str| {
        arg.ok_or_else(|| mlua::Error::RuntimeError(format!("api:exit(\"{name}\") needs {what}")))
    };
    Ok(match name {
        "title" => ScreenAction::Title,
        "char_select" => ScreenAction::CharSelect,
        "save_load" => ScreenAction::SaveLoad,
        "attract" => ScreenAction::Attract,
        "resume" => ScreenAction::Resume,
        "quit" => ScreenAction::Quit,
        "ending_result" => ScreenAction::EndingResult,
        "ending_epilogue" => ScreenAction::EndingEpilogue,
        "ending_save" => ScreenAction::EndingSave,
        "new_game" => ScreenAction::NewGame {
            character: arg("a character")?.clamp(0, 255) as u8,
        },
        "load_game" => ScreenAction::LoadGame {
            slot: arg("a slot")?.clamp(0, 7) as usize,
        },
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "unknown exit action `{other}`"
            )));
        }
    })
}

/// The error a draw method reports outside `draw`.
fn no_draw() -> mlua::Error {
    mlua::Error::RuntimeError("draw methods need `draw(api)`".into())
}

#[cfg(test)]
mod screen_tests;
#[cfg(test)]
mod tests;
