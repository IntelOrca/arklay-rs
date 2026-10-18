//! A save/load screen implemented by a pack Lua script.
//!
//! The screen is a normal pack entry (`screen/save_cr.lua` ships with the
//! base pack) selected by `save_screen` in `arklay.toml`. The engine loads it
//! in the same sandbox the hooks use and drives three optional globals:
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

#[cfg(not(feature = "lua"))]
mod imp {
    use anyhow::Result;

    use crate::render::Framebuffer;
    use crate::save::SaveFile;
    use crate::ui::save_load::{SaveOutcome, SaveScreen as SaveScreenTrait};
    use crate::ui::{Screen, ScreenResult, UiContext, UiInput};

    /// The lean build's screen type: never constructed. Every script load
    /// fails, so the factory falls back to the ported original screen.
    pub struct LuaScreen;

    impl LuaScreen {
        /// The lean build has no Lua runtime.
        pub fn open_load(entry: &str, _cx: &mut UiContext<'_>) -> Result<Self> {
            anyhow::bail!("this build has no Lua runtime, so {entry} cannot run");
        }

        /// The lean build has no Lua runtime.
        pub fn open_save(
            entry: &str,
            _cx: &mut UiContext<'_>,
            _file: SaveFile,
            _ink_ribbon: bool,
        ) -> Result<Self> {
            anyhow::bail!("this build has no Lua runtime, so {entry} cannot run");
        }
    }

    impl Screen for LuaScreen {
        fn open(&mut self, _cx: &mut UiContext<'_>) -> Result<()> {
            Ok(())
        }

        fn update(&mut self, _cx: &UiContext<'_>, _input: UiInput) -> ScreenResult {
            ScreenResult::Continue
        }

        fn draw(&mut self, _cx: &UiContext<'_>, _framebuffer: &mut Framebuffer) {}
    }

    impl SaveScreenTrait for LuaScreen {
        fn take_outcome(&mut self) -> Option<SaveOutcome> {
            None
        }
    }
}

#[cfg(feature = "lua")]
mod imp {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::rc::Rc;

    use anyhow::{Context, Result};
    use mlua::{Function, Lua, Table, UserData, UserDataMethods, Value};

    use crate::bmp;
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
    use crate::tim;
    use crate::ui::save_load::{SaveOutcome, consume_snapshot_ribbon};
    use crate::ui::{Screen, ScreenAction, ScreenResult, UiContext, UiCue, UiInput};

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
        ticks: u64,
        fade: u8,
        outcome: Option<SaveOutcome>,
        load: Option<PathBuf>,
        exited: bool,
        failed: bool,
        images: HashMap<String, Decoded>,
    }

    impl Host {
        /// Rescan the save directory.
        fn rescan(&mut self) {
            self.list = save::scan_dir(&self.save_dir);
        }

        /// The decoded pack image, cached.
        fn image<'a>(&'a mut self, pack: &Pack, entry: &str) -> Result<&'a Decoded> {
            if !self.images.contains_key(entry) {
                budget::check_len(
                    self.images.len() + 1,
                    budget::MAX_SCREEN_IMAGES,
                    "screen image cache size",
                )?;
                let decoded = decode_image(pack, entry)?;
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
            Self::open(entry, Mode::Load, cx)
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
                cx,
            )
        }

        fn open(entry: &str, mode: Mode, cx: &mut UiContext<'_>) -> Result<Self> {
            let source = cx
                .pack
                .read(entry)
                .with_context(|| format!("cannot read screen script {entry}"))?;
            budget::check_len(
                source.len(),
                budget::MAX_LUA_CHUNK,
                &format!("lua chunk {entry} size"),
            )?;
            let text = cx.text;
            let mut host = Host {
                mode,
                save_dir: cx.save_dir.to_path_buf(),
                script: text.map(Text::script).unwrap_or_default(),
                tight: text.map(Text::uses_tight_metrics).unwrap_or(true),
                list: Vec::new(),
                ticks: 0,
                fade: 0,
                outcome: None,
                load: None,
                exited: false,
                failed: false,
                images: HashMap::new(),
            };
            let vm = ScreenVm::load(entry, &source)?;
            host.rescan();
            vm.call_open(&mut host, cx)
                .with_context(|| format!("screen script {entry} on_open failed"))?;
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

        /// The action an exit reports: back to gameplay from a save screen,
        /// back to the title from the load screen.
        fn exit_action(&self) -> ScreenAction {
            match self.host.mode {
                Mode::Save { .. } => ScreenAction::Resume,
                Mode::Load => ScreenAction::Title,
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

    impl Screen for LuaScreen {
        /// Nothing to load: [`LuaScreen::open_load`]/[`LuaScreen::open_save`]
        /// read the script and ran `on_open` already.
        fn open(&mut self, _cx: &mut UiContext<'_>) -> Result<()> {
            Ok(())
        }

        fn update(&mut self, cx: &UiContext<'_>, input: UiInput) -> ScreenResult {
            self.host.ticks = self.host.ticks.saturating_add(1);
            let Self { host, vm, entry } = self;
            if let Err(err) = vm.call_update(host, cx, input) {
                eprintln!("[lua] save screen {entry} update failed: {err}");
                host.failed = true;
            }
            if let Some(path) = self.host.load.take() {
                return ScreenResult::Done(ScreenAction::LoadFile(path));
            }
            if self.host.exited || self.host.failed {
                return ScreenResult::Done(self.exit_action());
            }
            ScreenResult::Continue
        }

        fn draw(&mut self, cx: &UiContext<'_>, framebuffer: &mut Framebuffer) {
            let Self { host, vm, entry } = self;
            if let Err(err) = vm.call_draw(host, cx, framebuffer) {
                eprintln!("[lua] save screen {entry} draw failed: {err}");
                host.failed = true;
            }
        }

        fn fade(&self) -> u8 {
            self.host.fade
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
        on_open: Option<Function>,
        update: Option<Function>,
        draw: Option<Function>,
    }

    impl ScreenVm {
        /// Compile and run the chunk, then look up its entry points.
        fn load(name: &str, source: &[u8]) -> Result<Self> {
            let (lua, budget) = new_sandboxed_state()?;
            budget.begin();
            lua.load(source)
                .set_name(name.to_string())
                .exec()
                .map_err(|err| anyhow::anyhow!("{name}: {err}"))?;
            let globals = lua.globals();
            let lookup = |name: &str| match globals.get::<Value>(name) {
                Ok(Value::Function(function)) => Some(function),
                _ => None,
            };
            let update = lookup("update");
            if update.is_none() {
                anyhow::bail!("{name} defines no update(api, input), so it could never leave");
            }
            Ok(Self {
                lua,
                budget,
                on_open: lookup("on_open"),
                update,
                draw: lookup("draw"),
            })
        }

        /// Run `on_open(api)`.
        fn call_open(&self, host: &mut Host, cx: &UiContext<'_>) -> Result<()> {
            let Some(function) = &self.on_open else {
                return Ok(());
            };
            self.budget.begin();
            self.lua
                .scope(|scope| {
                    let api = scope.create_userdata(Api {
                        host: &mut *host,
                        cx,
                        framebuffer: None,
                    })?;
                    function.call::<()>(api)
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
                    })?;
                    function.call::<()>((api, table))
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
                    })?;
                    function.call::<()>(api)
                })
                .map_err(|err| anyhow::anyhow!("{err}"))?;
            Ok(())
        }

        /// A Lua global, for the unit tests.
        #[cfg(test)]
        fn global<T: mlua::FromLua>(&self, name: &str) -> mlua::Result<T> {
            self.lua.globals().get(name)
        }
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
    }

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
                        .ok_or_else(|| {
                            mlua::Error::RuntimeError(format!("invalid colour `{text}`"))
                        });
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
                    return parse_hex_colour(hex).ok_or_else(|| {
                        mlua::Error::RuntimeError(format!("invalid colour `{text}`"))
                    });
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
    fn parse_hex_colour(hex: &str) -> Option<[u8; 3]> {
        if hex.len() != 6 && hex.len() != 8 {
            return None;
        }
        let byte = |index: usize| u8::from_str_radix(&hex[index..index + 2], 16).ok();
        Some([byte(0)?, byte(2)?, byte(4)?])
    }

    /// Decode a pack image by its extension: `.tim` through the TIM decoder
    /// (mode 0/1 indexed, mode 2 RGBA), anything else as a BMP.
    fn decode_image(pack: &Pack, entry: &str) -> Result<Decoded> {
        let bytes = pack
            .read(entry)
            .with_context(|| format!("cannot read image {entry}"))?;
        if entry.to_ascii_lowercase().ends_with(".tim") {
            return Ok(match tim::mode(&bytes)? {
                0 => Decoded::Indexed(tim::decode_4bpp(&bytes)?),
                1 => Decoded::Indexed(tim::decode_8bpp(&bytes)?),
                _ => Decoded::Rgba(tim::decode(&bytes)?),
            });
        }
        Ok(Decoded::Rgba(bmp::decode(&bytes)?))
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
                framebuffer.draw_rgba_sprite(image, src, dst, opts.brightness);
            }
            Decoded::Indexed(texture) => {
                framebuffer.draw_indexed_sprite(
                    texture,
                    src,
                    dst,
                    opts.clut,
                    opts.brightness,
                    opts.tint,
                );
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
                })
            });

            methods.add_method("ticks", |_lua, api, ()| Ok(api.host.ticks));

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

            methods.add_method(
                "text_width",
                |_lua, api, (text, tight): (String, Option<bool>)| {
                    let font = api.cx.font.ok_or_else(no_font)?;
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

            methods.add_method_mut("exit", |_lua, api, ()| {
                api.host.exited = true;
                Ok(())
            });

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
                |_lua,
                 api,
                 (x, y, w, h, colour, alpha): (i32, i32, i32, i32, Value, Option<i64>)| {
                    let rgb = colour_rgb(&colour)?;
                    let alpha = alpha.unwrap_or(255).clamp(0, 255) as u8;
                    let framebuffer = api
                        .framebuffer
                        .as_deref_mut()
                        .ok_or_else(no_draw)?;
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
                    let opts = options(api.host, opts.as_ref())?;
                    let font = api.cx.font.ok_or_else(no_font)?;
                    let mut bytes = Vec::new();
                    charset::encode(&text, api.host.script, &mut bytes);
                    let framebuffer = api.framebuffer.as_deref_mut().ok_or_else(no_draw)?;
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
                    let opts = options(api.host, opts.as_ref())?;
                    let Api {
                        host,
                        cx,
                        framebuffer,
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
                    let opts = options(api.host, opts.as_ref())?;
                    let Api {
                        host,
                        cx,
                        framebuffer,
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

            methods.add_method_mut("image_size", |_lua, api, entry: String| {
                let image = api
                    .host
                    .image(api.cx.pack, &entry)
                    .map_err(mlua::Error::external)?;
                Ok(image.size())
            });

            methods.add_method_mut("fade", |_lua, api, alpha: i64| {
                api.host.fade = alpha.clamp(0, 255) as u8;
                Ok(())
            });
        }
    }

    /// The error a draw method reports outside `draw`.
    fn no_draw() -> mlua::Error {
        mlua::Error::RuntimeError("draw methods need `draw(api)`".into())
    }

    /// The error a text method reports without a font.
    fn no_font() -> mlua::Error {
        mlua::Error::RuntimeError("the pack carries no font for this screen".into())
    }
}

pub use imp::LuaScreen;

#[cfg(test)]
mod tests {
    use super::*;

    use crate::pack::Pack;
    #[cfg(feature = "lua")]
    use crate::save;
    use crate::save::SaveFile;
    use crate::text::Text;

    /// Self-deleting temporary directory unique to this process and label.
    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("arklay-lua-screen-{}-{label}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A 4x4 RGBA image whose top-left pixel is red, encoded as a BMP.
    fn test_bmp() -> Vec<u8> {
        let mut rgba = Vec::with_capacity(4 * 4 * 4);
        for index in 0..16 {
            if index == 0 {
                rgba.extend_from_slice(&[255, 0, 0, 255]);
            } else {
                rgba.extend_from_slice(&[0, 255, 0, 255]);
            }
        }
        crate::bmp::encode_to_vec(&crate::state::Image {
            width: 4,
            height: 4,
            rgba,
        })
        .unwrap()
    }

    /// A 4x4 4bpp TIM whose palette index 1 is blue.
    fn test_tim() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x10u32.to_le_bytes());
        bytes.extend_from_slice(&0x08u32.to_le_bytes());
        bytes.extend_from_slice(&44u32.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        for entry in 0..16u16 {
            let word = if entry == 1 { 0x7C00u16 } else { 0x0000 };
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.extend_from_slice(&28u32.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        for _ in 0..8u32 {
            bytes.extend_from_slice(&0x1111u16.to_le_bytes());
        }
        bytes
    }

    /// Build a pack with `screen/save.lua` (the source), a language file and
    /// the two test images.
    fn pack(source: &str) -> &'static Pack {
        use crate::pack::PackWriter;

        let mut writer = PackWriter::new();
        writer
            .add("screen/save.lua", source.as_bytes().to_vec())
            .unwrap();
        writer.add("art/bg.bmp", test_bmp()).unwrap();
        writer.add("art/dot.tim", test_tim()).unwrap();
        writer
            .add(
                "language/en-US.txt",
                b"UI_SAVE_CR_SAVE = \"Save\"\nROOM_100_NAME_SAVE = \"M. Room 1F\"\n".to_vec(),
            )
            .unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        Box::leak(Box::new(pack))
    }

    /// A synthetic font sheet: every cell is a solid block, so a drawn glyph
    /// is measurable without shipping art in the test pack.
    fn test_font() -> crate::font::Font {
        let (width, height) = (256u32, 224u32);
        let texture = crate::model::Texture8 {
            width,
            height,
            indices: vec![1; (width * height) as usize],
            palettes: (0..256).map(|index| [index as u8, 0, 0, 255]).collect(),
            stp: vec![false; 256],
        };
        crate::font::Font::new(texture)
    }

    /// The shared test setup: a pack, the text tables, a font and a temp save
    /// directory.
    struct Fixture {
        pack: &'static Pack,
        text: Text,
        font: crate::font::Font,
        dir: TempDir,
    }

    impl Fixture {
        fn new(label: &str, source: &str) -> Self {
            let pack = pack(source);
            let text = Text::load(pack, Some("en-US"));
            Self {
                pack,
                text,
                font: test_font(),
                dir: TempDir::new(label),
            }
        }

        fn context(&self) -> crate::ui::UiContext<'_> {
            crate::ui::UiContext {
                pack: self.pack,
                save_dir: &self.dir.path,
                font: Some(&self.font),
                text: Some(&self.text),
                ticks: 0,
                cues: std::cell::RefCell::new(Vec::new()),
            }
        }

        #[cfg(feature = "lua")]
        fn load(&self) -> LuaScreen {
            let mut cx = self.context();
            LuaScreen::open_load("screen/save.lua", &mut cx).unwrap()
        }

        #[cfg(feature = "lua")]
        fn save(&self, file: SaveFile, ink_ribbon: bool) -> LuaScreen {
            let mut cx = self.context();
            LuaScreen::open_save("screen/save.lua", &mut cx, file, ink_ribbon).unwrap()
        }
    }

    #[cfg(not(feature = "lua"))]
    #[test]
    fn the_lean_build_has_no_lua_screen() {
        let fixture = Fixture::new("lean", "local x = 1");
        let mut cx = fixture.context();
        assert!(LuaScreen::open_load("screen/save.lua", &mut cx).is_err());
        assert!(
            LuaScreen::open_save("screen/save.lua", &mut cx, SaveFile::default(), false).is_err()
        );
    }

    #[cfg(feature = "lua")]
    use crate::ui::{Screen, ScreenAction, ScreenResult, UiInput};

    #[cfg(feature = "lua")]
    fn draw(screen: &mut LuaScreen, cx: &crate::ui::UiContext<'_>) -> crate::render::Framebuffer {
        let mut framebuffer = crate::render::Framebuffer::new();
        screen.draw(cx, &mut framebuffer);
        framebuffer
    }

    #[cfg(feature = "lua")]
    #[test]
    fn the_script_draws_rects_and_text() {
        let fixture = Fixture::new(
            "draw",
            r##"
function update(api, input) end

function draw(api)
    api:rect(0, 0, 320, 240, 0x204060)
    api:rect(10, 10, 20, 20, "#FF0000", 128)
    api:text(8, 8, "Hello " .. api:string("UI_SAVE_CR_SAVE"))
    width = api:text_width("Hello")
    api:fade(64)
end
"##,
        );
        let mut screen = fixture.load();
        let cx = fixture.context();
        let framebuffer = draw(&mut screen, &cx);
        assert_eq!(&framebuffer.rgba[..4], &[0x20, 0x40, 0x60, 255]);
        let pixel = |x: usize, y: usize| {
            let index = (y * 320 + x) * 4;
            [
                framebuffer.rgba[index],
                framebuffer.rgba[index + 1],
                framebuffer.rgba[index + 2],
            ]
        };
        assert_ne!(pixel(15, 15), pixel(60, 60), "the translucent rect blended");
        let text_pixels = (0..30)
            .flat_map(|y| (0..160).map(move |x| (x, y)))
            .filter(|&(x, y)| pixel(x, y) != [0x20, 0x40, 0x60])
            .count();
        assert!(text_pixels > 20, "the text drew {text_pixels} pixels");
        assert!(
            screen.test_global::<i32>("width") > 0,
            "text_width measures"
        );
        assert_eq!(screen.fade(), 64);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn sprites_and_backgrounds_draw_from_pack_images() {
        let fixture = Fixture::new(
            "sprites",
            r##"
function update(api, input) end

function draw(api)
    api:background("art/bg.bmp")
    api:sprite("art/dot.tim", 10, 20)
    api:sprite_region("art/dot.tim", 0, 0, 2, 2, 40, 40, 8, 8)
    size_w, size_h = api:image_size("art/dot.tim")
    api:sprite("art/none.bmp", 0, 0)
end
"##,
        );
        let mut screen = fixture.load();
        let cx = fixture.context();
        let framebuffer = draw(&mut screen, &cx);
        let pixel = |x: usize, y: usize| {
            let index = (y * 320 + x) * 4;
            [
                framebuffer.rgba[index],
                framebuffer.rgba[index + 1],
                framebuffer.rgba[index + 2],
            ]
        };
        // The 4x4 BMP stretched over the screen keeps its top-left red.
        assert_eq!(pixel(0, 0), [255, 0, 0]);
        // The TIM sprite drew its blue palette at (10, 20).
        assert_eq!(pixel(11, 21), [0, 0, 255]);
        // The stretched background's second column is green there.
        assert_eq!(pixel(100, 100), [0, 255, 0]);
        // The scaled sub-region drew blue at (40, 40) and (47, 47).
        assert_eq!(pixel(40, 40), [0, 0, 255]);
        assert_eq!(pixel(47, 47), [0, 0, 255]);
        assert_eq!(screen.test_global::<u32>("size_w"), 8);
        assert_eq!(screen.test_global::<u32>("size_h"), 4);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_missing_image_leaves_the_screen() {
        let fixture = Fixture::new(
            "no-image",
            r#"
function update(api, input) end
function draw(api)
    api:sprite("art/none.bmp", 0, 0)
end
"#,
        );
        let mut screen = fixture.load();
        let cx = fixture.context();
        let _ = draw(&mut screen, &cx);
        assert_eq!(
            screen.update(&cx, UiInput::default()),
            ScreenResult::Done(ScreenAction::Title)
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn saves_lists_the_directory_with_parsed_metadata() {
        let fixture = Fixture::new(
            "list",
            r#"
function update(api)
    local list = api:saves()
    count = #list
    first = list[1]
    second = list[2]
end
"#,
        );
        let file = SaveFile {
            character: 1,
            saves: 7,
            stage: 0,
            room: 0x00,
            ..SaveFile::default()
        };
        save::save_to_path(&fixture.dir.path.join("beta.dat"), &file).unwrap();
        std::fs::write(fixture.dir.path.join("notes.txt"), b"junk").unwrap();

        let mut screen = fixture.load();
        let cx = fixture.context();
        screen.update(&cx, UiInput::default());
        assert_eq!(screen.test_global::<i32>("count"), 2);
        let first: mlua::Table = screen.test_global("first");
        assert_eq!(first.get::<String>("name").unwrap(), "beta.dat");
        assert_eq!(first.get::<String>("base").unwrap(), "beta");
        assert!(first.get::<bool>("loadable").unwrap());
        assert_eq!(first.get::<u8>("character").unwrap(), 1);
        assert_eq!(first.get::<u8>("saves").unwrap(), 7);
        assert_eq!(first.get::<u8>("stage").unwrap(), 0);
        assert_eq!(first.get::<String>("location").unwrap(), "M. Room 1F");
        assert!(first.get::<i64>("timestamp").unwrap() > 0);
        let second: mlua::Table = screen.test_global("second");
        assert_eq!(second.get::<String>("name").unwrap(), "notes.txt");
        assert!(!second.get::<bool>("loadable").unwrap());
    }

    #[cfg(feature = "lua")]
    #[test]
    fn save_writes_the_pending_block_and_records_the_outcome() {
        let fixture = Fixture::new(
            "save",
            r#"function update(api) ok, err = api:save("new_01.dat") end"#,
        );
        let mut file = SaveFile {
            character: 0,
            saves: 3,
            total_held: 1,
            ..SaveFile::default()
        };
        file.player_slots[0] = crate::game::InventoryItem {
            id: crate::items::ITEM_INK_RIBBONS,
            quantity: 2,
        };

        let mut screen = fixture.save(file, true);
        let cx = fixture.context();
        screen.update(&cx, UiInput::default());
        assert!(screen.test_global::<bool>("ok"));
        assert_eq!(screen.test_global::<Option<String>>("err"), None);

        // The written block consumed one ribbon.
        let written = save::load_file(&fixture.dir.path.join("new_01.dat")).unwrap();
        assert_eq!(written.player_slots[0].quantity, 1);
        assert_eq!(written.used_item, crate::items::ITEM_INK_RIBBONS);
        assert_eq!(
            screen.take_outcome(),
            Some(crate::ui::save_load::SaveOutcome {
                slot: None,
                ink_ribbon: true
            })
        );

        // The scan saw the new file.
        let names: Vec<String> = fixture
            .dir
            .path
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["new_01.dat"]);
    }

    #[cfg(feature = "lua")]
    #[test]
    fn save_rejects_bad_names_and_the_wrong_mode() {
        let fixture = Fixture::new(
            "save-bad",
            r#"
function update(api)
    ok1, err1 = api:save("../escape.dat")
    ok2, err2 = api:save("")
end
"#,
        );
        let mut screen = fixture.save(SaveFile::default(), false);
        let cx = fixture.context();
        screen.update(&cx, UiInput::default());
        assert!(!screen.test_global::<bool>("ok1"));
        assert!(screen.test_global::<Option<String>>("err1").is_some());
        assert!(!screen.test_global::<bool>("ok2"));
        assert!(screen.take_outcome().is_none());

        // Load mode refuses a save.
        let fixture = Fixture::new(
            "save-mode",
            r#"function update(api) ok = api:save("x.dat") end"#,
        );
        let mut screen = fixture.load();
        let cx = fixture.context();
        screen.update(&cx, UiInput::default());
        assert!(!screen.test_global::<bool>("ok"));
    }

    #[cfg(feature = "lua")]
    #[test]
    fn load_requests_the_chosen_file_and_exit_leaves() {
        let fixture = Fixture::new(
            "load",
            r#"
function update(api, input)
    if input.confirm then
        ok = api:load("slot.dat")
    elseif input.cancel then
        api:exit()
    end
end
"#,
        );
        save::save_to_path(&fixture.dir.path.join("slot.dat"), &SaveFile::default()).unwrap();
        let mut screen = fixture.load();
        let cx = fixture.context();

        let result = screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
        );
        assert_eq!(
            result,
            ScreenResult::Done(ScreenAction::LoadFile(fixture.dir.path.join("slot.dat")))
        );

        // A second screen exits back to the title in load mode.
        let mut screen = fixture.load();
        let result = screen.update(
            &cx,
            UiInput {
                cancel: true,
                ..UiInput::default()
            },
        );
        assert_eq!(result, ScreenResult::Done(ScreenAction::Title));
    }

    #[cfg(feature = "lua")]
    #[test]
    fn save_mode_exit_resumes_and_remove_deletes() {
        let fixture = Fixture::new(
            "remove",
            r#"
function update(api, input)
    if input.confirm then
        ok = api:remove("old.dat")
        api:exit()
    end
end
"#,
        );
        save::save_to_path(&fixture.dir.path.join("old.dat"), &SaveFile::default()).unwrap();
        let mut screen = fixture.save(SaveFile::default(), false);
        let cx = fixture.context();
        let result = screen.update(
            &cx,
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
        );
        assert_eq!(result, ScreenResult::Done(ScreenAction::Resume));
        assert!(screen.test_global::<bool>("ok"));
        assert!(!fixture.dir.path.join("old.dat").exists());
    }

    #[cfg(feature = "lua")]
    #[test]
    fn input_edges_reach_the_script() {
        let fixture = Fixture::new(
            "input",
            r#"
function update(api, input)
    pad = input.pad
    up = input.up
    confirm = input.confirm
end
"#,
        );
        let mut screen = fixture.load();
        let cx = fixture.context();
        screen.update(
            &cx,
            UiInput {
                up: true,
                confirm: true,
                ..UiInput::default()
            },
        );
        assert_eq!(screen.test_global::<u16>("pad"), 0x0011);
        assert!(screen.test_global::<bool>("up"));
        assert!(screen.test_global::<bool>("confirm"));
    }

    #[cfg(feature = "lua")]
    #[test]
    fn on_open_sounds_queue_before_the_first_tick() {
        let fixture = Fixture::new(
            "on-open-sound",
            r#"
function on_open(api)
    api:sound("save_confirm")
end

function update(api, input) end
"#,
        );
        let mut cx = fixture.context();
        let _screen = LuaScreen::open_load("screen/save.lua", &mut cx).unwrap();
        assert_eq!(
            cx.cues.borrow().as_slice(),
            &[crate::ui::UiCue::SaveConfirm]
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn sounds_queue_the_screen_cues() {
        let fixture = Fixture::new(
            "sounds",
            r#"
function update(api, input)
    if input.up then api:sound("save_cursor") end
    if input.confirm then api:sound("save_confirm") end
    if input.cancel then api:sound("error") end
end
"#,
        );
        let mut screen = fixture.load();
        let cx = fixture.context();
        for input in [
            UiInput {
                up: true,
                ..UiInput::default()
            },
            UiInput {
                confirm: true,
                ..UiInput::default()
            },
            UiInput {
                cancel: true,
                ..UiInput::default()
            },
        ] {
            screen.update(&cx, input);
        }
        assert_eq!(
            cx.cues.borrow().as_slice(),
            &[
                crate::ui::UiCue::SaveCursor,
                crate::ui::UiCue::SaveConfirm,
                crate::ui::UiCue::Cancel
            ]
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_failing_update_leaves_the_screen() {
        let fixture = Fixture::new("fail", r#"function update(api) error("boom") end"#);
        let mut screen = fixture.load();
        let cx = fixture.context();
        assert_eq!(
            screen.update(&cx, UiInput::default()),
            ScreenResult::Done(ScreenAction::Title)
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_failing_draw_marks_the_screen_and_it_leaves_next_tick() {
        let fixture = Fixture::new(
            "draw-fail",
            r#"function update(api, input) end
function draw(api) error("boom") end"#,
        );
        let mut screen = fixture.save(SaveFile::default(), false);
        let cx = fixture.context();
        let _ = draw(&mut screen, &cx);
        assert_eq!(
            screen.update(&cx, UiInput::default()),
            ScreenResult::Done(ScreenAction::Resume)
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn the_sandbox_has_no_io_or_os() {
        let fixture = Fixture::new(
            "sandbox",
            r#"
function update(api)
    io_absent = io == nil
    os_absent = os == nil
    api:log("sandbox ok")
end
"#,
        );
        let mut screen = fixture.load();
        let cx = fixture.context();
        screen.update(&cx, UiInput::default());
        assert!(screen.test_global::<bool>("io_absent"));
        assert!(screen.test_global::<bool>("os_absent"));
    }

    #[test]
    fn a_missing_script_is_an_error() {
        let fixture = Fixture::new("missing", "local x = 1");
        let mut cx = fixture.context();
        assert!(LuaScreen::open_load("screen/none.lua", &mut cx).is_err());
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_script_without_update_is_an_error() {
        let fixture = Fixture::new("no-update", r#"function draw(api) end"#);
        let mut cx = fixture.context();
        let Err(err) = LuaScreen::open_load("screen/save.lua", &mut cx) else {
            panic!("a script without update must be rejected");
        };
        let message = format!("{err:#}");
        assert!(message.contains("update"), "{message}");
    }

    #[cfg(feature = "lua")]
    #[test]
    fn a_broken_script_is_an_error() {
        let fixture = Fixture::new("broken", "function update(api) return");
        let mut cx = fixture.context();
        assert!(LuaScreen::open_load("screen/save.lua", &mut cx).is_err());
    }

    #[cfg(feature = "lua")]
    #[test]
    fn pad_words_match_the_original_layout() {
        assert_eq!(imp::pad_word(UiInput::default()), 0);
        assert_eq!(
            imp::pad_word(UiInput {
                up: true,
                down: true,
                confirm: true,
                cancel: true,
                start: true,
                page_left: true,
                page_right: true,
                ..UiInput::default()
            }),
            0x0C3B
        );
        assert_eq!(
            imp::held_word(UiInput {
                held_up: true,
                held_right: true,
                ..UiInput::default()
            }),
            0x0090
        );
    }

    #[cfg(feature = "lua")]
    #[test]
    fn colour_parsing_covers_names_numbers_and_hex() {
        use crate::font::Tint;
        use imp::{colour_rgb, colour_tint};
        let lua = mlua::Lua::new();
        assert_eq!(colour_tint(&mlua::Value::Integer(1)).unwrap(), Tint::Green);
        assert_eq!(
            colour_tint(&mlua::Value::String(lua.create_string("grey").unwrap())).unwrap(),
            Tint::Grey
        );
        assert_eq!(
            colour_rgb(&mlua::Value::Integer(0x123456)).unwrap(),
            [0x12, 0x34, 0x56]
        );
        assert_eq!(
            colour_rgb(&mlua::Value::String(lua.create_string("#A0B0C0").unwrap())).unwrap(),
            [0xA0, 0xB0, 0xC0]
        );
        assert_eq!(
            colour_tint(&mlua::Value::String(lua.create_string("#3F7DFA").unwrap())).unwrap(),
            Tint::Rgb(0x3F, 0x7D, 0xFA)
        );
        assert!(colour_rgb(&mlua::Value::String(lua.create_string("#zz").unwrap())).is_err());
    }
}
