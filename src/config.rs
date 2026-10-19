//! The user configuration file and the default save location.
//!
//! `arklay.toml` lives in the per-user configuration directory
//! (`$XDG_CONFIG_HOME/arklay`, else `~/.config/arklay`, else
//! `%APPDATA%\arklay`) and is optional: a missing file is the default
//! configuration. The grammar is the same small TOML subset the pack manifest
//! uses — comments, blank lines and `key = "value"`, plus TOML's literal
//! `key = 'value'` strings so a Windows path needs no escaping — with two
//! keys and one table:
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
//! ```
//!
//! Saved games default to `<config dir>/save` (the `savedat1..8.dat` slot
//! names and the 2690-byte block are unchanged), so a run from any pack
//! location reads the same saves.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::budget;
use crate::controls::{ACTIONS, Action, Bindings, Source};

/// The configuration file's name inside the configuration directory.
pub const FILE_NAME: &str = "arklay.toml";
/// The per-user directory name under the platform's configuration root.
pub const DIR_NAME: &str = "arklay";
/// The environment variable that relocates the configuration directory (and
/// with it the default save directory).
pub const DIR_ENV: &str = "ARKLAY_CONFIG_DIR";
/// The default save directory's name inside the configuration directory.
pub const SAVE_DIR_NAME: &str = "save";

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
        let mut in_bindings = false;
        let mut bound = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = strip_comment(raw).trim();
            if content.is_empty() {
                continue;
            }
            if let Some(table) = content.strip_prefix('[') {
                match table.strip_suffix(']').map(str::trim) {
                    Some("bindings") if config.bindings.is_none() => {
                        in_bindings = true;
                        config.bindings = Some(Bindings::default());
                    }
                    Some("bindings") => bail!("line {line}: duplicate [bindings] table"),
                    Some(other) => bail!("line {line}: unknown table `[{other}]`"),
                    None => bail!("line {line}: unterminated table header"),
                }
                continue;
            }
            let Some((key, value)) = content.split_once('=') else {
                bail!("line {line}: expected `key = \"value\"`");
            };
            let key = key.trim();
            let value = value.trim();
            if in_bindings {
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
                continue;
            }
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
        }
        if let Some(action) = config.bindings.as_ref().and_then(Bindings::lockout) {
            bail!(
                "[bindings]: `{}` needs a keyboard key bound to none of up, down, left, right, \
                 confirm, cancel and start",
                action.name()
            );
        }
        Ok(config)
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
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let mut config =
            Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        config.bindings = (*bindings != Bindings::default()).then(|| bindings.clone());
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        crate::atomic::write(path, config.render().as_bytes())
    }
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
        assert_eq!(Config::load(&path).unwrap(), Config::default());
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
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.language.as_deref(), Some("ja-JP"), "other keys kept");
        assert_eq!(loaded.bindings_or_default(), bindings);
        Config::save_bindings(&path, &Bindings::default()).unwrap();
        assert_eq!(Config::load(&path).unwrap().bindings, None);
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
        assert_eq!(
            Config::load(&path).unwrap().language.as_deref(),
            Some("ja-JP")
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
