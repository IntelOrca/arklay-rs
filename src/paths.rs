//! Where the engine and launcher look for user data.
//!
//! Configuration goes to the roaming configuration directory, while games,
//! mods and logs default to the local data directory. Both can be relocated:
//! [`CONFIG_ENV`] replaces the configuration directory, and [`DATA_ENV`]
//! replaces the whole data-root list with `;`-separated paths, first one
//! winning.
//!
//! Without an override the data roots are the local data directory, the
//! roaming directory and the directory holding the executable. A pack or mod
//! is resolved by name in that order, so a local `re1.akpak` shadows a copy
//! beside the executable; a local `arklay.toml` likewise takes over the
//! configuration when one already exists.
//!
//! On Windows the local directory is `%LOCALAPPDATA%\arklay`, the roaming
//! one `%APPDATA%\arklay`; on Linux they are `$XDG_DATA_HOME/arklay` and
//! `$XDG_CONFIG_HOME/arklay` (with the usual `~/.local/share` and `~/.config`
//! fallbacks); macOS uses `~/Library/Application Support/arklay` for both.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The per-user directory name under the platform's data/config roots.
pub const APP_DIR: &str = "arklay";
/// The environment variable that relocates the configuration directory.
pub const CONFIG_ENV: &str = "ARKLAY_CONFIG_DIR";
/// The environment variable that replaces the data-root search list.
pub const DATA_ENV: &str = "ARKLAY_DATA";
/// The configuration file's name inside the configuration directory.
pub const CONFIG_FILE: &str = "arklay.toml";

/// The platform's local data directory for this application.
pub fn local_data_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(dir) = env_path("LOCALAPPDATA") {
            return dir.join(APP_DIR);
        }
        if let Some(home) = home_dir() {
            return home.join("AppData").join("Local").join(APP_DIR);
        }
    } else if cfg!(target_os = "macos") {
        if let Some(home) = home_dir() {
            return home
                .join("Library")
                .join("Application Support")
                .join(APP_DIR);
        }
    } else {
        if let Some(dir) = env_path("XDG_DATA_HOME") {
            return dir.join(APP_DIR);
        }
        if let Some(home) = home_dir() {
            return home.join(".local").join("share").join(APP_DIR);
        }
    }
    PathBuf::from(APP_DIR)
}

/// The platform's roaming (configuration) directory for this application.
pub fn roaming_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(dir) = env_path("APPDATA") {
            return dir.join(APP_DIR);
        }
        if let Some(home) = home_dir() {
            return home.join("AppData").join("Roaming").join(APP_DIR);
        }
    } else if cfg!(target_os = "macos") {
        if let Some(home) = home_dir() {
            return home
                .join("Library")
                .join("Application Support")
                .join(APP_DIR);
        }
    } else {
        if let Some(dir) = env_path("XDG_CONFIG_HOME") {
            return dir.join(APP_DIR);
        }
        if let Some(home) = home_dir() {
            return home.join(".config").join(APP_DIR);
        }
    }
    PathBuf::from(APP_DIR)
}

/// The configuration directory: [`CONFIG_ENV`], else an existing
/// `arklay.toml` in the local data directory, else the roaming directory.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = env_path(CONFIG_ENV) {
        return dir;
    }
    let local = local_data_dir();
    if local.join(CONFIG_FILE).is_file() {
        return local;
    }
    roaming_dir()
}

/// The configuration file's path.
pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE)
}

/// The directory holding the running executable, which is also the portable
/// install root scanned last for data.
pub fn install_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .filter(|dir| !dir.as_os_str().is_empty())
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The data roots in search order.
///
/// [`DATA_ENV`] replaces the list completely; otherwise it is the local data
/// directory, the roaming directory and [`install_dir`]. An override with no
/// usable entries falls back to the default list.
pub fn data_roots() -> Vec<PathBuf> {
    if let Some(value) = env::var_os(DATA_ENV) {
        let roots = split_data_paths(&value);
        if !roots.is_empty() {
            return roots;
        }
    }
    dedupe(vec![local_data_dir(), roaming_dir(), install_dir()])
}

/// The first data root: where new game packs, mods and logs are written.
pub fn primary_data_dir() -> PathBuf {
    data_roots()
        .into_iter()
        .next()
        .unwrap_or_else(local_data_dir)
}

/// The games directory under `root`.
pub fn games_dir(root: &Path) -> PathBuf {
    root.join("games")
}

/// The mods directory under `root`.
pub fn mods_dir(root: &Path) -> PathBuf {
    root.join("mods")
}

/// The log directory under the primary data root.
pub fn log_dir() -> PathBuf {
    primary_data_dir().join("logs")
}

/// The launcher and engine log file.
pub fn log_path() -> PathBuf {
    log_dir().join("arklay.log")
}

/// Split a [`DATA_ENV`] value into trimmed, non-empty paths, expanding a
/// leading `~`.
pub fn split_data_paths(value: &OsStr) -> Vec<PathBuf> {
    value
        .to_string_lossy()
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| expand_tilde(Path::new(part)))
        .collect()
}

/// Remove duplicate roots, keeping the first occurrence.
fn dedupe(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        let duplicate = out.iter().any(|seen| same_path(seen, &root));
        if !duplicate {
            out.push(root);
        }
    }
    out
}

/// Whether two paths name the same root for precedence purposes. Windows
/// paths compare case-insensitively.
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    } else {
        a == b
    }
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\"))
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// A non-empty environment path.
fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The user's home directory.
pub fn home_dir() -> Option<PathBuf> {
    env_path("HOME").or_else(|| env_path("USERPROFILE"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_paths_are_trimmed_and_empty_entries_are_skipped() {
        let paths = split_data_paths(OsStr::new(" one ; two ;;  three "));
        assert_eq!(
            paths,
            [
                PathBuf::from("one"),
                PathBuf::from("two"),
                PathBuf::from("three")
            ]
        );
    }

    #[test]
    fn an_empty_override_yields_no_roots() {
        assert!(split_data_paths(OsStr::new(" ; ; ")).is_empty());
    }

    #[test]
    fn duplicates_keep_the_first_root() {
        let roots = dedupe(vec![
            PathBuf::from("a"),
            PathBuf::from("b"),
            PathBuf::from("a"),
        ]);
        assert_eq!(roots, [PathBuf::from("a"), PathBuf::from("b")]);
    }

    #[test]
    fn the_primary_root_is_the_first_one() {
        let roots = data_roots();
        assert!(!roots.is_empty());
        assert_eq!(primary_data_dir(), roots[0]);
    }

    #[test]
    fn a_trailing_separator_does_not_add_an_empty_root() {
        let paths = split_data_paths(OsStr::new("one;"));
        assert_eq!(paths, [PathBuf::from("one")]);
    }
}
