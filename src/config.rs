//! The user configuration file and the default save location.
//!
//! `arklay.toml` lives in the per-user configuration directory
//! (`$XDG_CONFIG_HOME/arklay`, else `~/.config/arklay`, else
//! `%APPDATA%\arklay`) and is optional: a missing file is the default
//! configuration. The grammar is the same small TOML subset the pack manifest
//! uses — comments, blank lines and `key = "value"`, plus TOML's literal
//! `key = 'value'` strings so a Windows path needs no escaping — with three
//! keys:
//!
//! ```toml
//! # Where saves are read and written (default: <config dir>/save).
//! save_dir = "~/.config/arklay/save"
//! # `vanilla` (the default), `cr`, or a pack entry with the screen script.
//! save_screen = "cr"
//! # The language tag, when `--language` is not given.
//! language = "en-US"
//! ```
//!
//! Saved games default to `<config dir>/save` (the `savedat1..8.dat` slot
//! names and the 2690-byte block are unchanged), so a run from any pack
//! location reads the same saves.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::budget;

/// The configuration file's name inside the configuration directory.
pub const FILE_NAME: &str = "arklay.toml";
/// The per-user directory name under the platform's configuration root.
pub const DIR_NAME: &str = "arklay";
/// The environment variable that relocates the configuration directory (and
/// with it the default save directory).
pub const DIR_ENV: &str = "ARKLAY_CONFIG_DIR";
/// The default save directory's name inside the configuration directory.
pub const SAVE_DIR_NAME: &str = "save";
/// The pack entry the built-in Classic REbirth-style screen ships as.
pub const CR_SCREEN_ENTRY: &str = "screen/save_cr.lua";

/// Which save/load screen the engine opens.
///
/// `Vanilla` is the ported original screen (the default). `Lua(entry)` runs
/// the pack entry's Lua script instead; `save_screen = "cr"` selects the
/// built-in [`CR_SCREEN_ENTRY`], and any other value is taken as a pack entry
/// path so a mod can ship its own screen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SaveScreen {
    /// The ported original screen (USA/MediaKite layouts).
    #[default]
    Vanilla,
    /// A Lua screen script, named by its pack entry.
    Lua(String),
}

impl SaveScreen {
    /// Parse a `save_screen` setting: `vanilla`/`rust`/`default`, `cr`, or a
    /// pack entry path.
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            bail!("save_screen is empty");
        }
        match value.to_ascii_lowercase().as_str() {
            "vanilla" | "rust" | "default" => Ok(Self::Vanilla),
            "cr" => Ok(Self::Lua(CR_SCREEN_ENTRY.to_string())),
            _ => {
                if value.contains('\0') {
                    bail!("save_screen contains a NUL byte");
                }
                Ok(Self::Lua(value.to_string()))
            }
        }
    }

    /// Whether this selects the ported original screen.
    pub fn is_vanilla(&self) -> bool {
        matches!(self, Self::Vanilla)
    }

    /// The pack entry the selection names, for a Lua screen.
    pub fn entry(&self) -> Option<&str> {
        match self {
            Self::Vanilla => None,
            Self::Lua(entry) => Some(entry),
        }
    }
}

/// The user configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Where saves are read and written; `None` means the default location.
    pub save_dir: Option<PathBuf>,
    /// Which save/load screen to open.
    pub save_screen: SaveScreen,
    /// The language tag to use when `--language` is not given.
    pub language: Option<String>,
}

impl Config {
    /// The per-user configuration directory: `$ARKLAY_CONFIG_DIR` when set,
    /// else `$XDG_CONFIG_HOME/arklay` on Unix (else `~/.config/arklay`) and
    /// `%APPDATA%\arklay` on Windows (else `~/.config/arklay`).
    pub fn dir() -> PathBuf {
        if let Some(dir) = env_path(DIR_ENV) {
            return dir;
        }
        if let Some(root) = env_path("XDG_CONFIG_HOME").filter(|_| !cfg!(windows)) {
            return root.join(DIR_NAME);
        }
        if let Some(appdata) = env_path("APPDATA").filter(|_| cfg!(windows)) {
            return appdata.join(DIR_NAME);
        }
        match home_dir() {
            Some(home) => home.join(".config").join(DIR_NAME),
            None => PathBuf::from(DIR_NAME),
        }
    }

    /// The configuration file's path inside [`Config::dir`].
    pub fn default_path() -> PathBuf {
        Self::dir().join(FILE_NAME)
    }

    /// The default save directory: `<config dir>/save`.
    pub fn default_save_dir() -> PathBuf {
        Self::dir().join(SAVE_DIR_NAME)
    }

    /// Read the configuration file; a missing file is the default
    /// configuration.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let mut config =
            Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        config.save_dir = config.save_dir.map(|dir| expand_tilde(&dir));
        Ok(config)
    }

    /// Read the default configuration file.
    pub fn load_default() -> Result<Self> {
        Self::load(&Self::default_path())
    }

    /// Parse the file text. Unknown keys and malformed lines are errors, so a
    /// typo is reported instead of silently ignored.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        budget::check_len(text.len(), budget::MAX_TEXT_BYTES, "config file size")?;
        let mut config = Self::default();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = strip_comment(raw).trim();
            if content.is_empty() {
                continue;
            }
            let Some((key, value)) = content.split_once('=') else {
                bail!("line {line}: expected `key = \"value\"`");
            };
            let key = key.trim();
            let value = value.trim();
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
                "save_screen" => config.save_screen = SaveScreen::parse(&value)?,
                "language" => {
                    if value.is_empty() {
                        bail!("line {line}: language is empty");
                    }
                    config.language = Some(value);
                }
                other => bail!("line {line}: unknown key `{other}`"),
            }
        }
        Ok(config)
    }

    /// Render the canonical file text (the documented keys, defaults omitted).
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(dir) = &self.save_dir {
            out.push_str(&format!("save_dir = \"{}\"\n", dir.display()));
        }
        match &self.save_screen {
            SaveScreen::Vanilla => {}
            SaveScreen::Lua(entry) if entry == CR_SCREEN_ENTRY => {
                out.push_str("save_screen = \"cr\"\n");
            }
            SaveScreen::Lua(entry) => {
                out.push_str(&format!("save_screen = \"{entry}\"\n"));
            }
        }
        if let Some(language) = &self.language {
            out.push_str(&format!("language = \"{language}\"\n"));
        }
        out
    }
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
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert!(Config::default().save_screen.is_vanilla());
    }

    #[test]
    fn parses_every_key() {
        let config = Config::parse(
            "# a comment\n\
             save_dir = \"/tmp/saves\"   # trailing\n\
             save_screen = \"cr\"\n\
             language = \"ja-JP\"\n",
        )
        .unwrap();
        assert_eq!(config.save_dir.as_deref(), Some(Path::new("/tmp/saves")));
        assert_eq!(config.save_screen, SaveScreen::Lua(CR_SCREEN_ENTRY.into()));
        assert_eq!(config.language.as_deref(), Some("ja-JP"));
    }

    #[test]
    fn screen_names_and_entry_paths_resolve() {
        assert_eq!(SaveScreen::parse("vanilla").unwrap(), SaveScreen::Vanilla);
        assert_eq!(SaveScreen::parse("RUST").unwrap(), SaveScreen::Vanilla);
        assert_eq!(
            SaveScreen::parse("cr").unwrap(),
            SaveScreen::Lua(CR_SCREEN_ENTRY.into())
        );
        assert_eq!(
            SaveScreen::parse("screen/mine.lua").unwrap(),
            SaveScreen::Lua("screen/mine.lua".into())
        );
        assert_eq!(
            SaveScreen::parse("cr").unwrap().entry(),
            Some(CR_SCREEN_ENTRY)
        );
        assert!(SaveScreen::parse("").is_err());
    }

    #[test]
    fn rejects_malformed_lines_and_unknown_keys() {
        for text in [
            "save_dir = /tmp/saves\n",
            "save_dir = \"unterminated\n",
            "save_dir = \"a\" trailing\n",
            "unknown_key = \"x\"\n",
            "save_screen = \"\"\n",
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
            save_screen: SaveScreen::Lua(CR_SCREEN_ENTRY.into()),
            language: Some("en-US".into()),
        }
        .render();
        assert_eq!(
            text,
            "save_dir = \"/tmp/saves\"\nsave_screen = \"cr\"\nlanguage = \"en-US\"\n"
        );
        assert_eq!(
            Config::parse(&text).unwrap().language.as_deref(),
            Some("en-US")
        );
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
        fs::write(&path, "save_screen = \"cr\"\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().save_screen,
            SaveScreen::Lua(CR_SCREEN_ENTRY.into())
        );

        // A directory where the file should be is an error, not a default.
        let blocked = dir.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        assert!(Config::load(&blocked).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_over_cap_file_is_rejected() {
        let text = "x".repeat(budget::MAX_TEXT_BYTES + 1);
        let message = budget::assert_cap_error(Config::parse(&text));
        assert!(message.contains("config file size"), "{message}");
    }
}
