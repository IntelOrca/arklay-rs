//! The user configuration file and the default save location.
//!
//! `arklay.toml` lives in the per-user configuration directory: an existing
//! local `arklay.toml` (`%LOCALAPPDATA%\arklay` on Windows,
//! `$XDG_DATA_HOME/arklay` on Linux) takes over, otherwise the roaming
//! directory is used (`%APPDATA%\arklay` / `$XDG_CONFIG_HOME/arklay`), and
//! `$ARKLAY_CONFIG_DIR` overrides both. The file is optional: a missing file
//! is the default configuration. The grammar is the same small TOML subset the pack manifest
//! uses — comments, blank lines and `key = "value"`, plus TOML's literal
//! `key = 'value'` strings so a Windows path needs no escaping — with two
//! root keys and six tables (the `[graphics]` table also takes bare integers
//! and booleans, and so does `[game]`):
//!
//! ```toml
//! # Where saves are read and written (default: <config dir>/save).
//! save_dir = "~/.config/arklay/save"
//! # The language tag, when `--language` is not given.
//! language = "en-US"
//!
//! # Input bindings, one single-line array of sources per action; an action
//! # left out keeps its default (see `crate::controls`).
//! [bindings]
//! aim = ["key:C", "mouse:right", "axis:lefttrigger+"]
//!
//! # Gameplay options (`--quick-turn` and `--tactical-reload` override them).
//! [game]
//! quick_turn = true      # BACKWARD + RUN turns the player 180 degrees
//! tactical_reload = true # AIM + RUN reloads a partially loaded gun
//!
//! # The interactive window (see `crate::video`); a key left out keeps its
//! # default. F8 and Alt+Enter rewrite `fullscreen` and the window size, and
//! # moving the window to another display rewrites `display`.
//! [graphics]
//! renderer = "software"  # or "gpu"; `--renderer` overrides it
//! fullscreen = false     # borderless fullscreen
//! window_width = 960     # the window's client size in pixels (F8 picks
//! window_height = 720    # multiples of 320x240; any size is letterboxed)
//! display = 1            # the display to open on, from 1
//! render_scale = 100     # GPU: the 3D scene's size, % of the viewport (25..400)
//! msaa = 1               # GPU: the 3D scene's samples per pixel (1, 2, 4, 8)
//! frame_rate = 30        # presented frames per second (30..1000); the game
//!                        # still runs at 30 Hz and higher rates tween
//! aspect = "4:3"         # or "16:9": crops the 4:3 frame to the wider shape
//!
//! # Discovered mods (the game pack's sibling `mods/` and `<config dir>/mods`).
//! # `enabled` narrows the discovered set when present; `disabled` always
//! # wins; `order` names ids applied first, in that order. Explicit `--mod`
//! # layers bypass the enable/disable selection.
//! [mods]
//! enabled = ["hd", "demo"]
//! disabled = ["balance"]
//! order = ["demo", "hd"]
//!
//! # The launcher's remembered installation (written after a successful
//! # install or rebuild, not usually edited by hand).
//! [games]
//! re1_source = 'C:\Games\Resident Evil'
//! re1_build = "21744136"
//!
//! # Update track and the last build found by a check.
//! [updates]
//! channel = "release"        # or "nightly"
//! check = true               # check on startup
//! last_check = 1775          # unix seconds
//! available_version = "0.1.1"
//! available_sha = "..."
//! ```
//!
//! Saved games default to `<config dir>/save` (the `savedat1..8.dat` slot
//! names and the 2690-byte block are unchanged), so a run from any pack
//! location reads the same saves.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::budget;
use crate::controls::{ACTIONS, Action, Bindings, Source};
use crate::video::{self, Graphics, RendererKind};

/// The configuration file's name inside the configuration directory.
pub const FILE_NAME: &str = crate::paths::CONFIG_FILE;
/// The per-user directory name under the platform's configuration root.
pub const DIR_NAME: &str = crate::paths::APP_DIR;
/// The environment variable that relocates the configuration directory (and
/// with it the default save directory).
pub const DIR_ENV: &str = crate::paths::CONFIG_ENV;
/// The default save directory's name inside the configuration directory.
pub const SAVE_DIR_NAME: &str = "save";
/// The per-user mods directory's name inside the configuration directory.
pub const MODS_DIR_NAME: &str = "mods";

/// The `[mods]` selection over auto-discovered mod layers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mods {
    /// When `Some`, only these discovered mod ids apply; `None` means every
    /// discovered mod that is not disabled.
    pub enabled: Option<Vec<String>>,
    /// Discovered mod ids that never apply.
    pub disabled: Vec<String>,
    /// Mod ids applied first, in this order; the rest follow by their
    /// manifest `(load_order, id)`.
    pub order: Vec<String>,
}

/// The user configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Where saves are read and written; `None` means the default location.
    pub save_dir: Option<PathBuf>,
    /// The language tag to use when `--language` is not given.
    pub language: Option<String>,
    /// The `[bindings]` table over the defaults; `None` when the file has
    /// none (the shipped defaults).
    pub bindings: Option<Bindings>,
    /// The `[graphics]` table over the defaults.
    pub graphics: Graphics,
    /// The `[mods]` enable/disable/order selection.
    pub mods: Mods,
    /// The `[games]` launcher bookkeeping.
    pub games: Games,
    /// The `[updates]` channel and last found build.
    pub updates: Updates,
    /// The `[game]` gameplay options.
    pub game: Game,
}

/// The update track the launcher follows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpdateChannel {
    #[default]
    Release,
    Nightly,
}

impl UpdateChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Nightly => "nightly",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "release" => Some(Self::Release),
            "nightly" => Some(Self::Nightly),
            _ => None,
        }
    }
}

/// The `[updates]` table: channel, startup check and the last found build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updates {
    pub channel: UpdateChannel,
    /// Check for a newer build on startup.
    pub check: bool,
    /// Unix seconds of the last completed check.
    pub last_check: u64,
    /// The channel of the stored available build.
    pub available_channel: Option<UpdateChannel>,
    pub available_version: Option<String>,
    pub available_sha: Option<String>,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            channel: UpdateChannel::Release,
            check: true,
            last_check: 0,
            available_channel: None,
            available_version: None,
            available_sha: None,
        }
    }
}

/// The `[games]` launcher bookkeeping: where each game's installation is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Games {
    /// The last accepted Resident Evil installation, used to rebuild the
    /// pack without asking for the folder again.
    pub re1_source: Option<PathBuf>,
    /// The Steam build id recorded next to that source, when it came from
    /// Steam.
    pub re1_build: Option<String>,
}

/// The `[game]` gameplay options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Game {
    /// Classic REbirth's quick turn: a RUN press while walking backward
    /// turns the player 180 degrees in place over eight ticks. Off by
    /// default, like the reference's `[DLL] QuickTurn`.
    pub quick_turn: bool,
    /// Classic REbirth's tactical reload (`[DLL] TactReload`): with the aim
    /// held, holding RUN reloads the equipped gun instead of firing when the
    /// clip is not full and the matching ammunition item is held. Off by
    /// default.
    pub tactical_reload: bool,
}

/// The table the parser is in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    Root,
    Bindings,
    Graphics,
    Mods,
    Games,
    Updates,
    Game,
    /// A table that could not be used; its keys are skipped.
    Ignored,
}

impl Config {
    /// The configuration directory: [`crate::paths::config_dir`], which is
    /// `$ARKLAY_CONFIG_DIR` when set, else an existing local `arklay.toml`,
    /// else the platform's roaming configuration directory.
    pub fn dir() -> PathBuf {
        crate::paths::config_dir()
    }

    /// The configuration file's path inside [`Config::dir`].
    pub fn default_path() -> PathBuf {
        Self::dir().join(FILE_NAME)
    }

    /// The default save directory: `<config dir>/save`.
    pub fn default_save_dir() -> PathBuf {
        Self::dir().join(SAVE_DIR_NAME)
    }

    /// The per-user mods directory: `<config dir>/mods`.
    pub fn default_mods_dir() -> PathBuf {
        Self::dir().join(MODS_DIR_NAME)
    }

    /// Read the configuration file, never failing: a missing file is the
    /// default configuration, and anything wrong with it (an unreadable file,
    /// a bad line or value) is a warning while that part keeps its default.
    pub fn load(path: &Path) -> (Self, Vec<String>) {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), Vec::new());
            }
            Err(err) => {
                let warning = format!("{}: {err}; using the defaults", path.display());
                return (Self::default(), vec![warning]);
            }
        };
        let (mut config, warnings) = Self::parse_lenient(&text);
        config.save_dir = config.save_dir.map(|dir| expand_tilde(&dir));
        config.games.re1_source = config.games.re1_source.map(|dir| expand_tilde(&dir));
        let warnings = warnings
            .into_iter()
            .map(|warning| format!("{}: {warning}", path.display()))
            .collect();
        (config, warnings)
    }

    /// Read the default configuration file ([`Config::load`]).
    pub fn load_default() -> (Self, Vec<String>) {
        Self::load(&Self::default_path())
    }

    /// Parse the file text strictly: any problem [`Config::parse_lenient`]
    /// would warn about is an error. Saving uses this, so a file with a
    /// mistake in it is never rewritten (and the mistake lost).
    pub fn parse(text: &str) -> Result<Self> {
        let (config, warnings) = Self::parse_lenient(text);
        if !warnings.is_empty() {
            bail!("{}", warnings.join("; "));
        }
        Ok(config)
    }

    /// Parse the file text, skipping what it cannot use: an unknown key or
    /// table, a malformed line or an out-of-range value is reported in the
    /// returned warnings and leaves that setting at its default, so a typo is
    /// reported without stopping the game.
    pub fn parse_lenient(text: &str) -> (Self, Vec<String>) {
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        if let Err(err) = budget::check_len(text.len(), budget::MAX_TEXT_BYTES, "config file size")
        {
            return (
                Self::default(),
                vec![format!("{err:#}; using the defaults")],
            );
        }
        let mut config = Self::default();
        let mut warnings = Vec::new();
        let mut table = Table::Root;
        let mut seen_graphics = false;
        let mut seen_mods = false;
        let mut seen_games = false;
        let mut seen_updates = false;
        let mut seen_game = false;
        let mut bound = Vec::new();
        let mut graphics_keys: Vec<String> = Vec::new();
        let mut mod_keys: Vec<String> = Vec::new();
        let mut games_keys: Vec<String> = Vec::new();
        let mut update_keys: Vec<String> = Vec::new();
        let mut game_keys: Vec<String> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = strip_comment(raw).trim();
            if content.is_empty() {
                continue;
            }
            if let Some(header) = content.strip_prefix('[') {
                table = match header.strip_suffix(']').map(str::trim) {
                    Some("bindings") if config.bindings.is_none() => {
                        config.bindings = Some(Bindings::default());
                        Table::Bindings
                    }
                    Some("graphics") if !seen_graphics => {
                        seen_graphics = true;
                        Table::Graphics
                    }
                    Some("mods") if !seen_mods => {
                        seen_mods = true;
                        Table::Mods
                    }
                    Some("games") if !seen_games => {
                        seen_games = true;
                        Table::Games
                    }
                    Some("updates") if !seen_updates => {
                        seen_updates = true;
                        Table::Updates
                    }
                    Some("game") if !seen_game => {
                        seen_game = true;
                        Table::Game
                    }
                    Some(
                        name @ ("bindings" | "graphics" | "mods" | "games" | "updates" | "game"),
                    ) => {
                        warnings.push(format!("line {line}: duplicate [{name}] table ignored"));
                        Table::Ignored
                    }
                    Some(other) => {
                        warnings.push(format!("line {line}: unknown table `[{other}]` ignored"));
                        Table::Ignored
                    }
                    None => {
                        warnings.push(format!("line {line}: unterminated table header"));
                        Table::Ignored
                    }
                };
                continue;
            }
            let parsed = match table {
                Table::Ignored => Ok(()),
                Table::Root => parse_root_key(&mut config, content, line),
                Table::Graphics => {
                    parse_graphics_line(&mut config.graphics, &mut graphics_keys, content, line)
                }
                Table::Mods => parse_mods_line(&mut config.mods, &mut mod_keys, content, line),
                Table::Games => parse_games_line(&mut config.games, &mut games_keys, content, line),
                Table::Updates => {
                    parse_updates_line(&mut config.updates, &mut update_keys, content, line)
                }
                Table::Game => parse_game_line(&mut config.game, &mut game_keys, content, line),
                Table::Bindings => parse_binding_line(&mut config, &mut bound, content, line),
            };
            if let Err(err) = parsed {
                warnings.push(format!("{err:#}"));
            }
        }
        if let Some(action) = config.bindings.as_ref().and_then(Bindings::lockout) {
            warnings.push(format!(
                "[bindings]: `{}` needs a keyboard key bound to none of up, down, left, right, \
                 confirm, cancel and start; using the default bindings",
                action.name()
            ));
            config.bindings = None;
        }
        (config, warnings)
    }

    /// Render the canonical file text (the documented keys, defaults omitted:
    /// the `[bindings]` table lists only the actions that differ from the
    /// shipped layout, so the rest keep following it).
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(dir) = &self.save_dir {
            out.push_str(&format!("save_dir = {}\n", quote(&dir.to_string_lossy())));
        }
        if let Some(language) = &self.language {
            out.push_str(&format!("language = {}\n", quote(language)));
        }
        let mods = render_mods(&self.mods);
        if !mods.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[mods]\n");
            out.push_str(&mods);
        }
        let games = render_games(&self.games);
        if !games.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[games]\n");
            out.push_str(&games);
        }
        let updates = render_updates(&self.updates);
        if !updates.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[updates]\n");
            out.push_str(&updates);
        }
        if let Some(bindings) = &self.bindings {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[bindings]\n");
            let defaults = Bindings::default();
            for action in ACTIONS {
                if bindings.get(action) == defaults.get(action) {
                    continue;
                }
                let names: Vec<String> = bindings
                    .get(action)
                    .iter()
                    .map(|source| quote(&source.to_string()))
                    .collect();
                out.push_str(&format!("{} = [{}]\n", action.name(), names.join(", ")));
            }
        }
        let game = render_game(&self.game);
        if !game.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[game]\n");
            out.push_str(&game);
        }
        let graphics = render_graphics(&self.graphics);
        if !graphics.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[graphics]\n");
            out.push_str(&graphics);
        }
        out
    }

    /// The configured bindings, or the shipped defaults.
    pub fn bindings_or_default(&self) -> Bindings {
        self.bindings.clone().unwrap_or_default()
    }

    /// Persist `bindings` into the file at `path`, keeping its other keys.
    ///
    /// Bindings equal to the defaults drop the table, so a reset leaves a
    /// fresh file. The file is rewritten in its canonical form (comments are
    /// not kept; `save_dir` stays as written, `~` included). A file that does
    /// not parse is left alone and reported.
    pub fn save_bindings(path: &Path, bindings: &Bindings) -> Result<()> {
        Self::update(path, |config| {
            config.bindings = (*bindings != Bindings::default()).then(|| bindings.clone());
        })
    }

    /// Persist the `[mods]` enable/disable/order selection.
    pub fn save_mods(path: &Path, mods: &Mods) -> Result<()> {
        Self::update(path, |config| config.mods = mods.clone())
    }

    /// Persist the update channel, check settings and last found build.
    pub fn save_updates(path: &Path, updates: &Updates) -> Result<()> {
        Self::update(path, |config| config.updates = updates.clone())
    }

    /// Persist the launcher's remembered Resident Evil source and Steam build
    /// id, keeping the file's other keys. A `None` source removes the entry.
    pub fn save_games(path: &Path, source: Option<&Path>, build: Option<&str>) -> Result<()> {
        Self::update(path, |config| {
            config.games.re1_source = source.map(Path::to_path_buf);
            config.games.re1_build = build.map(str::to_string);
        })
    }

    /// Persist the window keys of `graphics` (`fullscreen`, `window_width`,
    /// `window_height` and `display`) into the file at `path`, keeping its other keys. The
    /// renderer keys stay as the file has them, since the command line may
    /// have overridden them for this run. Rewritten like
    /// [`Config::save_bindings`].
    pub fn save_window(path: &Path, graphics: &Graphics) -> Result<()> {
        Self::update(path, |config| {
            config.graphics.fullscreen = graphics.fullscreen;
            config.graphics.window_width = graphics.window_width;
            config.graphics.window_height = graphics.window_height;
            config.graphics.display = graphics.display;
        })
    }

    /// Persist `[graphics] frame_rate` into the file at `path`, keeping its
    /// other keys. Rewritten like [`Config::save_bindings`].
    pub fn save_frame_rate(path: &Path, frame_rate: u32) -> Result<()> {
        Self::update(path, |config| config.graphics.frame_rate = frame_rate)
    }

    /// Persist `[graphics] aspect` into the file at `path`, keeping its other
    /// keys. Rewritten like [`Config::save_bindings`].
    pub fn save_aspect(path: &Path, aspect: video::Aspect) -> Result<()> {
        Self::update(path, |config| config.graphics.aspect = aspect)
    }

    /// Read the file at `path` (a missing one is empty), apply `change` and
    /// write it back in its canonical form. A file that does not parse is
    /// left alone and reported.
    fn update(path: &Path, change: impl FnOnce(&mut Self)) -> Result<()> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let mut config =
            Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        change(&mut config);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        crate::atomic::write(path, config.render().as_bytes())
    }
}

/// Split a `key = value` line.
fn key_value(content: &str, line: usize) -> Result<(&str, &str)> {
    let Some((key, value)) = content.split_once('=') else {
        bail!("line {line}: expected `key = \"value\"`");
    };
    Ok((key.trim(), value.trim()))
}

/// Parse one top-level `key = "value"` line into `config`.
fn parse_root_key(config: &mut Config, content: &str, line: usize) -> Result<()> {
    let (key, value) = key_value(content, line)?;
    if !value.starts_with(['"', '\'']) {
        bail!("line {line}: `{key}` expects a quoted string");
    }
    let value = parse_quoted(value, line)?;
    match key {
        "save_dir" => {
            if value.is_empty() {
                bail!("line {line}: save_dir is empty");
            }
            config.save_dir = Some(PathBuf::from(value));
        }
        "language" => {
            if value.is_empty() {
                bail!("line {line}: language is empty");
            }
            config.language = Some(value);
        }
        other => bail!("line {line}: unknown key `{other}`"),
    }
    Ok(())
}

/// Parse one `[bindings]` line into `config`; a bad line leaves the action
/// at its default.
fn parse_binding_line(
    config: &mut Config,
    bound: &mut Vec<Action>,
    content: &str,
    line: usize,
) -> Result<()> {
    let (key, value) = key_value(content, line)?;
    let Some(action) = Action::from_name(key) else {
        bail!("line {line}: unknown action `{key}`");
    };
    if bound.contains(&action) {
        bail!("line {line}: duplicate action `{key}`");
    }
    bound.push(action);
    let mut sources = Vec::new();
    for name in parse_string_array(value, line)? {
        let source = Source::parse(&name)
            .with_context(|| format!("line {line}: bad binding for `{key}`"))?;
        sources.push(source);
    }
    if let Some(bindings) = config.bindings.as_mut() {
        bindings.set(action, sources);
    }
    Ok(())
}

/// Parse one `[mods]` line into `mods`.
fn parse_mods_line(
    mods: &mut Mods,
    seen: &mut Vec<String>,
    content: &str,
    line: usize,
) -> Result<()> {
    let (key, value) = key_value(content, line)?;
    if seen.iter().any(|seen| seen == key) {
        bail!("line {line}: duplicate key `{key}`");
    }
    seen.push(key.to_string());
    let ids = parse_string_array(value, line)?;
    if ids.iter().any(String::is_empty) {
        bail!("line {line}: `{key}` contains an empty mod id");
    }
    match key {
        "enabled" => mods.enabled = Some(ids),
        "disabled" => mods.disabled = ids,
        "order" => mods.order = ids,
        other => bail!("line {line}: unknown key `{other}`"),
    }
    Ok(())
}

/// Parse one `[graphics]` line into `graphics`.
fn parse_graphics_line(
    graphics: &mut Graphics,
    seen: &mut Vec<String>,
    content: &str,
    line: usize,
) -> Result<()> {
    let (key, value) = key_value(content, line)?;
    if seen.iter().any(|seen| seen == key) {
        bail!("line {line}: duplicate key `{key}`");
    }
    seen.push(key.to_string());
    parse_graphics_key(graphics, key, value, line)
}

/// Parse one `[graphics]` key into `graphics`.
fn parse_graphics_key(graphics: &mut Graphics, key: &str, value: &str, line: usize) -> Result<()> {
    let integer = |range: std::ops::RangeInclusive<u32>| -> Result<u32> {
        let Ok(number) = value.parse::<u32>() else {
            bail!("line {line}: `{key}` expects an integer");
        };
        if !range.contains(&number) {
            bail!(
                "line {line}: `{key}` must be {}..={}",
                range.start(),
                range.end()
            );
        }
        Ok(number)
    };
    match key {
        "renderer" => {
            let name = parse_quoted(value, line)?;
            let Some(renderer) = RendererKind::from_name(&name) else {
                bail!("line {line}: renderer must be \"software\" or \"gpu\"");
            };
            graphics.renderer = renderer;
        }
        "fullscreen" => {
            graphics.fullscreen = match value {
                "true" => true,
                "false" => false,
                _ => bail!("line {line}: `fullscreen` expects true or false"),
            };
        }
        "window_width" => graphics.window_width = integer(1..=video::MAX_WINDOW_SIDE)?,
        "window_height" => graphics.window_height = integer(1..=video::MAX_WINDOW_SIDE)?,
        "display" => graphics.display = integer(1..=video::MAX_DISPLAY)?,
        "render_scale" => graphics.render_scale = integer(video::RENDER_SCALE_RANGE)?,
        "msaa" => {
            let samples = integer(1..=8)?;
            if !video::MSAA_COUNTS.contains(&samples) {
                bail!("line {line}: `msaa` must be 1, 2, 4 or 8");
            }
            graphics.msaa = samples;
        }
        "frame_rate" => {
            graphics.frame_rate = integer(video::MIN_FRAME_RATE..=video::MAX_FRAME_RATE)?
        }
        "aspect" => {
            let name = parse_quoted(value, line)?;
            graphics.aspect = video::Aspect::parse(&name)
                .with_context(|| format!("line {line}: invalid `aspect`"))?;
        }
        other => bail!("line {line}: unknown key `{other}`"),
    }
    Ok(())
}

/// The `[mods]` keys that are set; an absent `enabled` is not written.
fn render_mods(mods: &Mods) -> String {
    let mut out = String::new();
    if let Some(enabled) = &mods.enabled {
        let ids: Vec<String> = enabled.iter().map(|id| quote(id)).collect();
        out.push_str(&format!("enabled = [{}]\n", ids.join(", ")));
    }
    for (key, ids) in [("disabled", &mods.disabled), ("order", &mods.order)] {
        if !ids.is_empty() {
            let ids: Vec<String> = ids.iter().map(|id| quote(id)).collect();
            out.push_str(&format!("{key} = [{}]\n", ids.join(", ")));
        }
    }
    out
}

/// Parse one `[games]` line into `games`.
fn parse_games_line(
    games: &mut Games,
    seen: &mut Vec<String>,
    content: &str,
    line: usize,
) -> Result<()> {
    let (key, value) = key_value(content, line)?;
    if seen.iter().any(|seen| seen == key) {
        bail!("line {line}: duplicate key `{key}`");
    }
    seen.push(key.to_string());
    if !value.starts_with(['"', '\'']) {
        bail!("line {line}: `{key}` expects a quoted string");
    }
    let value = parse_quoted(value, line)?;
    match key {
        "re1_source" => {
            if value.is_empty() {
                bail!("line {line}: re1_source is empty");
            }
            games.re1_source = Some(PathBuf::from(value));
        }
        "re1_build" => {
            if value.is_empty() {
                bail!("line {line}: re1_build is empty");
            }
            games.re1_build = Some(value);
        }
        other => bail!("line {line}: unknown key `{other}`"),
    }
    Ok(())
}

/// The `[games]` keys that are set.
fn render_games(games: &Games) -> String {
    let mut out = String::new();
    if let Some(source) = &games.re1_source {
        out.push_str(&format!(
            "re1_source = {}\n",
            quote(&source.to_string_lossy())
        ));
    }
    if let Some(build) = &games.re1_build {
        out.push_str(&format!("re1_build = {}\n", quote(build)));
    }
    out
}

/// Parse one `[updates]` line into `updates`.
fn parse_updates_line(
    updates: &mut Updates,
    seen: &mut Vec<String>,
    content: &str,
    line: usize,
) -> Result<()> {
    let (key, value) = key_value(content, line)?;
    if seen.iter().any(|seen| seen == key) {
        bail!("line {line}: duplicate key `{key}`");
    }
    seen.push(key.to_string());
    let quoted = || -> Result<String> {
        if !value.starts_with(['"', '\'']) {
            bail!("line {line}: `{key}` expects a quoted string");
        }
        parse_quoted(value, line)
    };
    match key {
        "channel" => {
            let text = quoted()?;
            updates.channel = UpdateChannel::parse(&text)
                .with_context(|| format!("line {line}: channel must be release or nightly"))?;
        }
        "check" => {
            updates.check = match value {
                "true" => true,
                "false" => false,
                _ => bail!("line {line}: `check` expects true or false"),
            };
        }
        "last_check" => {
            updates.last_check = value
                .parse::<u64>()
                .map_err(|_| anyhow!("line {line}: bad last_check {value}"))?;
        }
        "available_channel" => {
            let text = quoted()?;
            updates.available_channel = Some(
                UpdateChannel::parse(&text)
                    .with_context(|| format!("line {line}: bad available_channel"))?,
            );
        }
        "available_version" => updates.available_version = Some(quoted()?),
        "available_sha" => updates.available_sha = Some(quoted()?),
        other => bail!("line {line}: unknown key `{other}`"),
    }
    Ok(())
}

/// The `[updates]` keys that differ from the defaults.
fn render_updates(updates: &Updates) -> String {
    let defaults = Updates::default();
    let mut out = String::new();
    if updates.channel != defaults.channel {
        out.push_str(&format!("channel = {}\n", quote(updates.channel.as_str())));
    }
    if updates.check != defaults.check {
        out.push_str(&format!("check = {}\n", updates.check));
    }
    if updates.last_check != defaults.last_check {
        out.push_str(&format!("last_check = {}\n", updates.last_check));
    }
    if let Some(channel) = updates.available_channel {
        out.push_str(&format!(
            "available_channel = {}\n",
            quote(channel.as_str())
        ));
    }
    if let Some(version) = &updates.available_version {
        out.push_str(&format!("available_version = {}\n", quote(version)));
    }
    if let Some(sha) = &updates.available_sha {
        out.push_str(&format!("available_sha = {}\n", quote(sha)));
    }
    out
}

/// Parse one `[game]` line into `game`; a bad line leaves its option at the
/// default.
fn parse_game_line(
    game: &mut Game,
    seen: &mut Vec<String>,
    content: &str,
    line: usize,
) -> Result<()> {
    let (key, value) = key_value(content, line)?;
    if seen.iter().any(|seen| seen == key) {
        bail!("line {line}: duplicate key `{key}`");
    }
    seen.push(key.to_string());
    match key {
        "quick_turn" => {
            game.quick_turn = match value {
                "true" => true,
                "false" => false,
                _ => bail!("line {line}: `quick_turn` expects true or false"),
            };
        }
        "tactical_reload" => {
            game.tactical_reload = match value {
                "true" => true,
                "false" => false,
                _ => bail!("line {line}: `tactical_reload` expects true or false"),
            };
        }
        other => bail!("line {line}: unknown key `{other}`"),
    }
    Ok(())
}

/// The `[game]` keys that differ from the defaults.
fn render_game(game: &Game) -> String {
    let defaults = Game::default();
    let mut out = String::new();
    if game.quick_turn != defaults.quick_turn {
        out.push_str(&format!("quick_turn = {}\n", game.quick_turn));
    }
    if game.tactical_reload != defaults.tactical_reload {
        out.push_str(&format!("tactical_reload = {}\n", game.tactical_reload));
    }
    out
}

/// The `[graphics]` keys that differ from the defaults.
fn render_graphics(graphics: &Graphics) -> String {
    let defaults = Graphics::default();
    let mut out = String::new();
    if graphics.renderer != defaults.renderer {
        out.push_str(&format!("renderer = {}\n", quote(graphics.renderer.name())));
    }
    if graphics.fullscreen != defaults.fullscreen {
        out.push_str(&format!("fullscreen = {}\n", graphics.fullscreen));
    }
    if graphics.aspect != defaults.aspect {
        out.push_str(&format!("aspect = {}\n", quote(&graphics.aspect.name())));
    }
    for (key, value, default) in [
        ("window_width", graphics.window_width, defaults.window_width),
        (
            "window_height",
            graphics.window_height,
            defaults.window_height,
        ),
        ("display", graphics.display, defaults.display),
        ("render_scale", graphics.render_scale, defaults.render_scale),
        ("msaa", graphics.msaa, defaults.msaa),
        ("frame_rate", graphics.frame_rate, defaults.frame_rate),
    ] {
        if value != default {
            out.push_str(&format!("{key} = {value}\n"));
        }
    }
    out
}

/// A basic `"..."` string with the escapes [`parse_quoted`] reads back.
fn quote(text: &str) -> String {
    let mut out = String::from('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Parse a one-line array of quoted strings (`["a", 'b']`), or a lone quoted
/// string as a one-element array.
fn parse_string_array(value: &str, line: usize) -> Result<Vec<String>> {
    if value.starts_with(['"', '\'']) {
        return Ok(vec![parse_quoted(value, line)?]);
    }
    let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) else {
        bail!("line {line}: expected an array of quoted strings");
    };
    let mut items = Vec::new();
    let mut rest = inner.trim();
    while !rest.is_empty() {
        let end = quoted_end(rest)
            .ok_or_else(|| anyhow::anyhow!("line {line}: expected a quoted string in the array"))?;
        items.push(parse_quoted(&rest[..end], line)?);
        rest = rest[end..].trim_start();
        match rest.strip_prefix(',') {
            Some(after) => rest = after.trim_start(),
            None if rest.is_empty() => {}
            None => bail!("line {line}: expected `,` between array items"),
        }
    }
    Ok(items)
}

/// The byte length of the quoted string `text` starts with, quotes included.
fn quoted_end(text: &str) -> Option<usize> {
    let quote = text.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let mut escaped = false;
    for (index, ch) in text.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if quote == '"' && ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some(index + 1);
        }
    }
    None
}

/// The home directory: `$HOME`, else `%USERPROFILE%`.
pub fn home_dir() -> Option<PathBuf> {
    env_path("HOME").or_else(|| env_path("USERPROFILE"))
}

/// Expand a leading `~/` against the home directory.
pub fn expand_tilde(path: &Path) -> PathBuf {
    match home_dir() {
        Some(home) => expand_tilde_with(path, &home),
        None => path.to_path_buf(),
    }
}

/// [`expand_tilde`] against an explicit home directory.
pub fn expand_tilde_with(path: &Path, home: &Path) -> PathBuf {
    let Some(rest) = path.to_str().and_then(|text| text.strip_prefix("~/")) else {
        return path.to_path_buf();
    };
    home.join(rest)
}

/// A non-empty environment variable as a path.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Drop a trailing `#` comment, honouring quoted strings: a `#` inside a value
/// is not a comment, and an escape only applies inside a basic `"..."` string.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('"') => match ch {
                '\\' => escaped = true,
                '"' => quote = None,
                _ => {}
            },
            Some(_) => {
                if ch == '\'' {
                    quote = None;
                }
            }
            None => match ch {
                '"' | '\'' => quote = Some(ch),
                '#' => return &line[..index],
                _ => {}
            },
        }
    }
    line
}

/// Parse one quoted string: `"..."` with the manifest's escapes, or `'...'`
/// verbatim (TOML's literal string, so a Windows path needs no escaping).
fn parse_quoted(value: &str, line: usize) -> Result<String> {
    let Some(quote @ ('"' | '\'')) = value.chars().next() else {
        bail!("line {line}: expected a quoted string");
    };
    if !value.ends_with(quote) || value.len() < 2 {
        bail!("line {line}: unterminated string");
    }
    let inner = &value[1..value.len() - 1];
    if quote == '\'' {
        return Ok(inner.to_string());
    }
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => bail!("line {line}: unknown escape `\\{other}`"),
            None => bail!("line {line}: unterminated escape"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_the_default_config() {
        let dir =
            std::env::temp_dir().join(format!("arklay-config-{}-missing", std::process::id()));
        let path = dir.join("arklay.toml");
        assert_eq!(Config::load(&path), (Config::default(), Vec::new()));
    }

    #[test]
    fn parses_every_key() {
        let config = Config::parse(
            "# a comment\n\
             save_dir = \"/tmp/saves\"   # trailing\n\
             language = \"ja-JP\"\n",
        )
        .unwrap();
        assert_eq!(config.save_dir.as_deref(), Some(Path::new("/tmp/saves")));
        assert_eq!(config.language.as_deref(), Some("ja-JP"));
    }

    #[test]
    fn rejects_malformed_lines_and_unknown_keys() {
        for text in [
            "save_dir = /tmp/saves\n",
            "save_dir = \"unterminated\n",
            "save_dir = \"a\" trailing\n",
            "unknown_key = \"x\"\n",
            "language = \"\"\n",
            "save_dir\n",
        ] {
            assert!(Config::parse(text).is_err(), "{text:?} must be rejected");
        }
    }

    #[test]
    fn literal_strings_need_no_escaping() {
        // TOML's single-quoted literal string: a Windows path is written as
        // it is, backslashes and all.
        let config = Config::parse(
            "save_dir = 'C:\\Users\\me\\saves'   # a comment\n\
             language = 'en-US'\n",
        )
        .unwrap();
        assert_eq!(
            config.save_dir.as_deref(),
            Some(Path::new("C:\\Users\\me\\saves"))
        );
        assert_eq!(config.language.as_deref(), Some("en-US"));
        // A `#` inside either string form is not a comment, and a backslash
        // inside a literal one is not an escape.
        let config = Config::parse("save_dir = 'C:\\temp\\a#b'\n").unwrap();
        assert_eq!(config.save_dir.as_deref(), Some(Path::new("C:\\temp\\a#b")));
        assert!(Config::parse("save_dir = 'unterminated\n").is_err());
    }

    #[test]
    fn quotes_and_escapes_round_trip() {
        let config = Config::parse("save_dir = \"a\\\\b\\\"c\"\n").unwrap();
        assert_eq!(config.save_dir.as_deref(), Some(Path::new("a\\b\"c")));
        let text = Config {
            save_dir: Some(PathBuf::from("/tmp/saves")),
            language: Some("en-US".into()),
            bindings: None,
            graphics: Graphics::default(),
            mods: Mods::default(),
            games: Games::default(),
            updates: Updates::default(),
            game: Game::default(),
        }
        .render();
        assert_eq!(text, "save_dir = \"/tmp/saves\"\nlanguage = \"en-US\"\n");
        assert_eq!(
            Config::parse(&text).unwrap().language.as_deref(),
            Some("en-US")
        );
    }

    #[test]
    fn a_bindings_table_overrides_only_the_actions_it_names() {
        let config = Config::parse(
            "language = \"en-US\"\n\
             \n\
             [bindings]  # the input map\n\
             aim = [\"key:C\", 'mouse:right', \"axis:lefttrigger+\"]  # three devices\n\
             fire = \"wheel:up\"\n\
             page_left = []\n",
        )
        .unwrap();
        assert_eq!(config.language.as_deref(), Some("en-US"));
        let bindings = config.bindings_or_default();
        let names = |action| -> Vec<String> {
            bindings.get(action).iter().map(|s| s.to_string()).collect()
        };
        assert_eq!(
            names(Action::Aim),
            ["key:C", "mouse:right", "axis:lefttrigger+"]
        );
        assert_eq!(names(Action::Fire), ["wheel:up"]);
        assert!(names(Action::PageLeft).is_empty(), "an empty array unbinds");
        assert_eq!(
            bindings.get(Action::Confirm),
            Bindings::default().get(Action::Confirm),
            "unnamed actions keep their defaults"
        );
        // The rendered table parses back to the same map.
        let text = config.render();
        assert!(text.contains("\n[bindings]\n"), "{text}");
        assert!(
            !text.contains("confirm"),
            "defaults are not written: {text}"
        );
        assert_eq!(Config::parse(&text).unwrap(), config);
    }

    #[test]
    fn rejects_bad_bindings() {
        for text in [
            "[bindings]\nnope = [\"key:C\"]\n",
            "[bindings]\naim = [\"key:NoSuchKey\"]\n",
            "[bindings]\naim = [\"key:F1\"]\n",
            "[bindings]\naim = [\"key:C\" \"key:Z\"]\n",
            "[bindings]\naim = [\"key:C\"\n",
            "[bindings]\nsave_dir = \"/tmp\"\n",
            "[bindings]\n[bindings]\n",
            "[other]\n",
            "[bindings]\naim = []\naim = []\n",
            "[bindings]\nconfirm = []\n",
            "[bindings]\ncancel = [\"key:X\", \"key:Space\", \"key:Return\"]\n",
        ] {
            assert!(Config::parse(text).is_err(), "{text:?} must be rejected");
        }
    }

    #[test]
    fn saved_bindings_survive_a_reload_and_a_reset_drops_them() {
        let dir =
            std::env::temp_dir().join(format!("arklay-config-{}-bindings", std::process::id()));
        let path = dir.join("arklay.toml");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "language = \"ja-JP\"\n").unwrap();
        let mut bindings = Bindings::default();
        assert!(bindings.bind(Action::Aim, usize::MAX, Source::Mouse(3)));
        Config::save_bindings(&path, &bindings).unwrap();
        let (loaded, _) = Config::load(&path);
        assert_eq!(loaded.language.as_deref(), Some("ja-JP"), "other keys kept");
        assert_eq!(loaded.bindings_or_default(), bindings);
        Config::save_bindings(&path, &Bindings::default()).unwrap();
        assert_eq!(Config::load(&path).0.bindings, None);
        assert_eq!(fs::read_to_string(&path).unwrap(), "language = \"ja-JP\"\n");
        // A file that does not parse is never overwritten.
        fs::write(&path, "garbage\n").unwrap();
        assert!(Config::save_bindings(&path, &bindings).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "garbage\n");
        // Paths that need escaping, and `~`, survive a save untouched.
        let text = "save_dir = 'C:\\Users\\me \"x\"\\saves'\nlanguage = \"en-US\"\n";
        fs::write(&path, text).unwrap();
        Config::save_bindings(&path, &bindings).unwrap();
        let saved = Config::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved.save_dir.as_deref(),
            Some(Path::new("C:\\Users\\me \"x\"\\saves"))
        );
        fs::write(&path, "save_dir = \"~/saves\"\n").unwrap();
        Config::save_bindings(&path, &bindings).unwrap();
        let saved = Config::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.save_dir.as_deref(), Some(Path::new("~/saves")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_graphics_table_sets_the_window_and_renderer_options() {
        let config = Config::parse(
            "[graphics]\n\
             renderer = \"gpu\"\n\
             fullscreen = true  # borderless\n\
             window_width = 1280\n\
             window_height = 1024\n\
             display = 2\n\
             render_scale = 150\n\
             msaa = 4\n\
             frame_rate = 75\n\
             aspect = \"16:9\"\n",
        )
        .unwrap();
        assert_eq!(
            config.graphics,
            Graphics {
                renderer: RendererKind::Gpu,
                fullscreen: true,
                window_width: 1280,
                window_height: 1024,
                display: 2,
                render_scale: 150,
                msaa: 4,
                frame_rate: 75,
                aspect: video::Aspect::WIDE,
            }
        );
        let text = config.render();
        assert!(text.starts_with("[graphics]\n"), "{text}");
        assert_eq!(Config::parse(&text).unwrap(), config);
        // Keys left out keep their defaults, and defaults are not written.
        let config = Config::parse("[graphics]\nwindow_width = 640\n").unwrap();
        assert_eq!(config.graphics.msaa, 1);
        assert_eq!(config.graphics.window_height, 720);
        assert_eq!(config.render(), "[graphics]\nwindow_width = 640\n");
    }

    #[test]
    fn bad_graphics_values_warn_and_keep_their_defaults() {
        for text in [
            "[graphics]\nrenderer = \"opengl\"\n",
            "[graphics]\nrenderer = gpu\n",
            "[graphics]\nfullscreen = \"yes\"\n",
            "[graphics]\nwindow_width = 0\n",
            "[graphics]\nwindow_height = \"720\"\n",
            "[graphics]\nwindow_width = 99999\n",
            "[graphics]\ndisplay = 0\n",
            "[graphics]\nrender_scale = 10\n",
            "[graphics]\nmsaa = 3\n",
            "[graphics]\nframe_rate = 29\n",
            "[graphics]\nframe_rate = 1001\n",
            "[graphics]\nvsync = true\n",
            "[graphics]\nmsaa = 1\nmsaa = 2\n",
            "[graphics]\n[graphics]\n",
        ] {
            assert!(Config::parse(text).is_err(), "{text:?} must be rejected");
            let (config, warnings) = Config::parse_lenient(text);
            assert_eq!(warnings.len(), 1, "{text:?}: {warnings:?}");
            assert_eq!(config.graphics, Graphics::default(), "{text:?}");
        }
        // Only the bad line falls back; the rest of the file still applies.
        let (config, warnings) = Config::parse_lenient(
            "language = \"ja-JP\"\n\
             [graphics]\n\
             renderer = \"gpu\"\n\
             msaa = 3\n\
             window_width = 640\n\
             [bindings]\n\
             aim = [\"key:NoSuchKey\"]\n\
             fire = \"wheel:up\"\n",
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("line 4"), "{warnings:?}");
        assert_eq!(config.language.as_deref(), Some("ja-JP"));
        assert_eq!(config.graphics.renderer, RendererKind::Gpu);
        assert_eq!(config.graphics.msaa, 1);
        assert_eq!(config.graphics.window_width, 640);
        let bindings = config.bindings_or_default();
        assert_eq!(
            bindings.get(Action::Aim),
            Bindings::default().get(Action::Aim)
        );
        assert_eq!(
            bindings.get(Action::Fire),
            [Source::Wheel(crate::controls::Wheel::Up)]
        );
        // A bindings table that locks out the menus falls back as a whole.
        let (config, warnings) = Config::parse_lenient("[bindings]\nconfirm = []\n");
        assert_eq!((config.bindings, warnings.len()), (None, 1));
    }

    #[test]
    fn a_game_table_sets_the_gameplay_options() {
        let config = Config::parse("[game]\nquick_turn = true  # BACKWARD + RUN\n").unwrap();
        assert!(config.game.quick_turn);
        let text = config.render();
        assert_eq!(text, "[game]\nquick_turn = true\n");
        assert_eq!(Config::parse(&text).unwrap(), config);
        // The default is off and is not written.
        assert!(!Config::parse("").unwrap().game.quick_turn);
        assert_eq!(Config::default().render(), "");
        assert_eq!(
            Config::parse("[game]\nquick_turn = false\n")
                .unwrap()
                .render(),
            ""
        );
        // The tactical reload is the second `[game]` behaviour and renders in
        // the table's key order.
        let config = Config::parse("[game]\ntactical_reload = true\n").unwrap();
        assert!(config.game.tactical_reload);
        assert_eq!(config.render(), "[game]\ntactical_reload = true\n");
        assert_eq!(Config::parse(&config.render()).unwrap(), config);
        let config = Config::parse("[game]\nquick_turn = true\ntactical_reload = true\n").unwrap();
        assert_eq!(
            config.render(),
            "[game]\nquick_turn = true\ntactical_reload = true\n"
        );
        assert!(!Config::parse("").unwrap().game.tactical_reload);
        assert_eq!(
            Config::parse("[game]\ntactical_reload = false\n")
                .unwrap()
                .render(),
            ""
        );
    }

    #[test]
    fn bad_game_values_warn_and_keep_their_defaults() {
        for text in [
            "[game]\nquick_turn = \"yes\"\n",
            "[game]\nquick_turn = 1\n",
            "[game]\ntactical_reload = \"yes\"\n",
            "[game]\ntactical_reload = 1\n",
            "[game]\nunknown = true\n",
            "[game]\n[game]\n",
        ] {
            assert!(Config::parse(text).is_err(), "{text:?} must be rejected");
            let (config, warnings) = Config::parse_lenient(text);
            assert!(!warnings.is_empty(), "{text:?} must warn");
            assert!(!config.game.quick_turn, "{text:?}");
            assert!(!config.game.tactical_reload, "{text:?}");
        }
        // A duplicate key warns and keeps the first value.
        for (key, value) in [
            ("quick_turn", "quick_turn"),
            ("tactical_reload", "tactical_reload"),
        ] {
            let text = format!("[game]\n{key} = true\n{value} = false\n");
            assert!(Config::parse(&text).is_err());
            let (config, warnings) = Config::parse_lenient(&text);
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(
                if key == "quick_turn" {
                    config.game.quick_turn
                } else {
                    config.game.tactical_reload
                },
                "the first value is kept"
            );
        }
        // Only the bad line falls back; the rest of the file still applies.
        let (config, warnings) = Config::parse_lenient("[game]\nquick_turn = true\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(config.game.quick_turn);
        let (config, warnings) = Config::parse_lenient(
            "[game]\nquick_turn = true\n[bindings]\naim = [\"key:NoSuchKey\"]\n",
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(config.game.quick_turn);
    }

    #[test]
    fn a_mods_table_selects_and_orders_discovered_mods() {
        let config = Config::parse(
            "[mods]\n\
             enabled = [\"hd\", 'demo']  # only these discovered mods\n\
             disabled = [\"balance\"]\n\
             order = [\"demo\", \"hd\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.mods,
            Mods {
                enabled: Some(vec!["hd".into(), "demo".into()]),
                disabled: vec!["balance".into()],
                order: vec!["demo".into(), "hd".into()],
            }
        );
        let text = config.render();
        assert!(text.starts_with("[mods]\n"), "{text}");
        assert_eq!(Config::parse(&text).unwrap(), config);
        // An empty `enabled` list is meaningful (apply no discovered mods) and
        // survives the round trip.
        let config = Config::parse("[mods]\nenabled = []\n").unwrap();
        assert_eq!(config.mods.enabled, Some(Vec::new()));
        assert_eq!(Config::parse(&config.render()).unwrap(), config);
        // A config without the table keeps the all-discovered default.
        assert_eq!(Config::parse("").unwrap().mods, Mods::default());
    }

    #[test]
    fn bad_mods_values_warn_and_keep_their_defaults() {
        for text in [
            "[mods]\nenabled = [\"\"]\n",
            "[mods]\nenabled = [\"hd\" \"demo\"]\n",
            "[mods]\nenabled = [\"hd\"\n",
            "[mods]\nunknown = [\"hd\"]\n",
            "[mods]\nenabled = [\"hd\"]\norder = [\"hd\"]\norder = []\n",
            "[mods]\n[mods]\n",
            "[mods]\nenabled = 3\n",
        ] {
            assert!(Config::parse(text).is_err(), "{text:?} must be rejected");
            let (_, warnings) = Config::parse_lenient(text);
            assert!(!warnings.is_empty(), "{text:?} must warn");
        }
        // A bad mods line does not lose the rest of the file.
        let (config, warnings) =
            Config::parse_lenient("language = \"ja-JP\"\n[mods]\nunknown = []\n");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(config.language.as_deref(), Some("ja-JP"));
    }

    #[test]
    fn saving_the_window_keeps_the_renderer_keys() {
        let dir = std::env::temp_dir().join(format!("arklay-config-{}-window", std::process::id()));
        let path = dir.join("arklay.toml");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &path,
            "language = \"ja-JP\"\n[graphics]\nrender_scale = 50\n[game]\ntactical_reload = true\n",
        )
        .unwrap();
        let run = Graphics {
            renderer: RendererKind::Gpu,
            fullscreen: true,
            window_width: 1600,
            window_height: 1200,
            display: 2,
            render_scale: 200,
            msaa: 8,
            frame_rate: 120,
            aspect: video::Aspect::WIDE,
        };
        Config::save_window(&path, &run).unwrap();
        let (saved, warnings) = Config::load(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(saved.language.as_deref(), Some("ja-JP"));
        assert!(saved.game.tactical_reload, "the [game] table is kept");
        assert_eq!(
            saved.graphics,
            Graphics {
                fullscreen: true,
                window_width: 1600,
                window_height: 1200,
                display: 2,
                render_scale: 50,
                ..Graphics::default()
            },
            "only the window keys are written"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_the_frame_rate_keeps_the_other_keys() {
        let dir = std::env::temp_dir().join(format!("arklay-config-{}-rate", std::process::id()));
        let path = dir.join("arklay.toml");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "[graphics]\nmsaa = 4\n").unwrap();
        Config::save_frame_rate(&path, 144).unwrap();
        let (saved, warnings) = Config::load(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!((saved.graphics.msaa, saved.graphics.frame_rate), (4, 144));
        // The default rate is not written.
        Config::save_frame_rate(&path, 30).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "[graphics]\nmsaa = 4\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_the_aspect_keeps_the_other_keys() {
        let dir = std::env::temp_dir().join(format!("arklay-config-{}-aspect", std::process::id()));
        let path = dir.join("arklay.toml");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "[graphics]\nmsaa = 4\n").unwrap();
        Config::save_aspect(&path, video::Aspect::WIDE).unwrap();
        let (saved, warnings) = Config::load(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            (saved.graphics.msaa, saved.graphics.aspect),
            (4, video::Aspect::WIDE)
        );
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("aspect = \"16:9\"\n")
        );
        // The default 4:3 is not written.
        Config::save_aspect(&path, video::Aspect::STANDARD).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "[graphics]\nmsaa = 4\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_any_aspect_and_rejects_a_bad_one() {
        let config = Config::parse("[graphics]\naspect = \"16:10\"\n").unwrap();
        assert_eq!(config.graphics.aspect.name(), "16:10");
        for text in [
            "[graphics]\naspect = \"wide\"\n",
            "[graphics]\naspect = \"0:9\"\n",
        ] {
            assert!(Config::parse(text).is_err(), "{text:?} must be rejected");
        }
    }

    #[test]
    fn the_default_render_is_empty_and_parses_back() {
        assert_eq!(Config::default().render(), "");
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn tilde_expands_against_the_home_directory() {
        assert_eq!(
            expand_tilde_with(Path::new("~/.config/arklay/save"), Path::new("/home/ted")),
            Path::new("/home/ted/.config/arklay/save")
        );
        assert_eq!(
            expand_tilde_with(Path::new("~/saves"), Path::new("/home/ted")),
            Path::new("/home/ted/saves")
        );
        // Only a leading `~/` expands; `~user` and a bare `~` do not.
        assert_eq!(
            expand_tilde_with(Path::new("~ted/saves"), Path::new("/home/ted")),
            Path::new("~ted/saves")
        );
        assert_eq!(
            expand_tilde_with(Path::new("saves/~x"), Path::new("/home/ted")),
            Path::new("saves/~x")
        );
    }

    #[test]
    fn the_config_directory_ends_with_the_directory_name() {
        assert!(Config::dir().ends_with(DIR_NAME));
        assert!(Config::default_save_dir().ends_with(SAVE_DIR_NAME));
        assert_eq!(
            Config::default_path()
                .file_name()
                .and_then(|name| name.to_str()),
            Some(FILE_NAME)
        );
    }

    #[test]
    fn load_reads_a_file_and_reports_other_errors() {
        let dir = std::env::temp_dir().join(format!("arklay-config-{}-load", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("arklay.toml");
        fs::write(&path, "language = \"ja-JP\"\n").unwrap();
        assert_eq!(Config::load(&path).0.language.as_deref(), Some("ja-JP"));

        // A directory where the file should be is reported, and the defaults
        // apply.
        let blocked = dir.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        let (config, warnings) = Config::load(&blocked);
        assert_eq!(config, Config::default());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        // A bad value is reported with the file's path.
        fs::write(&path, "[graphics]\nmsaa = 3\n").unwrap();
        let (config, warnings) = Config::load(&path);
        assert_eq!(config, Config::default());
        assert!(warnings[0].contains("arklay.toml"), "{warnings:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_updates_table_roundtrips_and_saves() {
        let text = "[updates]\nchannel = \"nightly\"\ncheck = false\nlast_check = 1775\n\
                    available_channel = \"nightly\"\navailable_version = \"0.1.0-nightly\"\n\
                    available_sha = \"abc123\"\n";
        let config = Config::parse(text).unwrap();
        assert_eq!(config.updates.channel, UpdateChannel::Nightly);
        assert!(!config.updates.check);
        assert_eq!(config.updates.last_check, 1775);
        assert_eq!(config.updates.available_sha.as_deref(), Some("abc123"));
        assert_eq!(Config::parse(&config.render()).unwrap(), config);

        let dir =
            std::env::temp_dir().join(format!("arklay-config-{}-updates", std::process::id()));
        let path = dir.join("arklay.toml");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let updates = Updates {
            channel: UpdateChannel::Nightly,
            available_version: Some("0.1.0-nightly".to_string()),
            ..Updates::default()
        };
        Config::save_updates(&path, &updates).unwrap();
        assert_eq!(Config::load(&path).0.updates, updates);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_games_source_roundtrips_and_saves() {
        let text = "[games]\nre1_source = 'C:\\Games\\RE1'\nre1_build = \"21744136\"\n";
        let config = Config::parse(text).unwrap();
        assert_eq!(
            config.games.re1_source.as_deref(),
            Some(Path::new(r"C:\Games\RE1"))
        );
        assert_eq!(config.games.re1_build.as_deref(), Some("21744136"));
        assert_eq!(Config::parse(&config.render()).unwrap(), config);

        let dir = std::env::temp_dir().join(format!("arklay-config-{}-games", std::process::id()));
        let path = dir.join("arklay.toml");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "language = \"en-US\"\n").unwrap();
        Config::save_games(&path, Some(Path::new(r"D:\Steam\RE1")), Some("42")).unwrap();
        let (loaded, warnings) = Config::load(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            loaded.games.re1_source.as_deref(),
            Some(Path::new(r"D:\Steam\RE1"))
        );
        assert_eq!(loaded.games.re1_build.as_deref(), Some("42"));
        assert_eq!(loaded.language.as_deref(), Some("en-US"));

        Config::save_games(&path, None, None).unwrap();
        assert_eq!(Config::load(&path).0.games, Games::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_over_cap_file_is_rejected() {
        let text = "x".repeat(budget::MAX_TEXT_BYTES + 1);
        let message = budget::assert_cap_error(Config::parse(&text));
        assert!(message.contains("config file size"), "{message}");
    }
}
