//! Locating a Steam installation of Resident Evil (1996).
//!
//! Steam keeps the game in a library folder, which is not necessarily the
//! Steam installation itself. The search reads the registry's `SteamPath`
//! (or `InstallPath`) on Windows, then every library in
//! `steamapps/libraryfolders.vdf`, and finally checks the default install
//! locations. A candidate only counts when `convert-game` recognises its
//! layout, so an unrelated Steam library entry never gets offered as the
//! game.

use std::fs;
use std::path::{Path, PathBuf};

/// The Steam application id of the 1996 Resident Evil release.
pub const APP_ID: u32 = 4_249_100;
/// The app manifest Steam writes for [`APP_ID`].
pub const APP_MANIFEST: &str = "appmanifest_4249100.acf";
/// The `installdir` Steam's manifest names.
pub const INSTALL_DIR: &str = "4249100_Biohazard";

/// A detected, layout-checked Resident Evil installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    /// The folder to pass to the converter.
    pub root: PathBuf,
    /// The Steam installation the library belongs to.
    pub steam: PathBuf,
    /// The library that holds the game.
    pub library: PathBuf,
    /// The Steam build id, when the app manifest carries one.
    pub build_id: Option<String>,
}

/// Find the first usable Steam copy of the game.
pub fn find_re1() -> Option<Install> {
    for steam in steam_roots() {
        if let Some(install) = find_in_steam(&steam) {
            return Some(install);
        }
    }
    None
}

/// Find the game in one Steam installation and its libraries.
pub fn find_in_steam(steam: &Path) -> Option<Install> {
    let mut libraries = vec![steam.to_path_buf()];
    libraries.extend(read_library_folders(steam));
    dedupe(&mut libraries);
    for library in libraries {
        let manifest = library.join("steamapps").join(APP_MANIFEST);
        let Ok(text) = fs::read_to_string(&manifest) else {
            continue;
        };
        let installdir = value(&text, "installdir").unwrap_or_else(|| INSTALL_DIR.to_string());
        let root = library.join("steamapps").join("common").join(installdir);
        if !root.is_dir() || crate::convert::validate_install(&root).is_err() {
            continue;
        }
        return Some(Install {
            root,
            steam: steam.to_path_buf(),
            library,
            build_id: value(&text, "buildid"),
        });
    }
    None
}

/// The default Steam installation roots for this platform.
fn steam_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(windows)]
    {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

        for (hive, key) in [
            (HKEY_CURRENT_USER, "Software\\Valve\\Steam"),
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\WOW6432Node\\Valve\\Steam"),
        ] {
            let Ok(key) = RegKey::predef(hive).open_subkey(key) else {
                continue;
            };
            let path = key
                .get_value::<String, _>("SteamPath")
                .or_else(|_| key.get_value::<String, _>("InstallPath"));
            if let Ok(path) = path {
                roots.push(PathBuf::from(path));
            }
        }
        for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(dir) = std::env::var_os(variable) {
                roots.push(PathBuf::from(dir).join("Steam"));
            }
        }
    }
    if let Some(home) = crate::paths::home_dir() {
        if cfg!(target_os = "macos") {
            roots.push(
                home.join("Library")
                    .join("Application Support")
                    .join("Steam"),
            );
        } else {
            roots.push(home.join(".steam").join("steam"));
            roots.push(home.join(".local").join("share").join("Steam"));
            roots.push(
                home.join(".var")
                    .join("app")
                    .join("com.valvesoftware.Steam")
                    .join("data")
                    .join("Steam"),
            );
        }
    }
    dedupe(&mut roots);
    roots.retain(|root| root.is_dir());
    roots
}

/// The library folders listed in Steam's `libraryfolders.vdf`.
fn read_library_folders(steam: &Path) -> Vec<PathBuf> {
    let path = steam.join("steamapps").join("libraryfolders.vdf");
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    values(&text, "path")
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// Remove duplicate paths, keeping the first occurrence. Windows paths
/// compare case-insensitively.
fn dedupe(paths: &mut Vec<PathBuf>) {
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for path in paths.drain(..) {
        let duplicate = out.iter().any(|seen| {
            if cfg!(windows) {
                seen.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&path.as_os_str().to_string_lossy())
            } else {
                seen == &path
            }
        });
        if !duplicate {
            out.push(path);
        }
    }
    *paths = out;
}

/// The first value of `key` in a Valve KeyValues document.
pub fn value(text: &str, key: &str) -> Option<String> {
    values(text, key).into_iter().next()
}

/// Every value following `key` in a Valve KeyValues document.
pub fn values(text: &str, key: &str) -> Vec<String> {
    let tokens = tokenize(text);
    let mut out = Vec::new();
    for index in 0..tokens.len() {
        if tokens[index].as_deref() == Some(key)
            && let Some(Some(value)) = tokens.get(index + 1)
        {
            out.push(value.clone());
        }
    }
    out
}

/// The quoted strings of a KeyValues document, with braces as `None` so a
/// key/value pair can be read positionally.
fn tokenize(text: &str) -> Vec<Option<String>> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' | '}' => tokens.push(None),
            '"' => {
                let mut value = String::new();
                while let Some(ch) = chars.next() {
                    match ch {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => value.push('\n'),
                            Some('t') => value.push('\t'),
                            Some('"') => value.push('"'),
                            Some('\\') => value.push('\\'),
                            Some(other) => {
                                // A hand-written single backslash is data, not
                                // an escape; keep it.
                                value.push('\\');
                                value.push(other);
                            }
                            None => value.push('\\'),
                        },
                        other => value.push(other),
                    }
                }
                tokens.push(Some(value));
            }
            '/' if chars.peek() == Some(&'/') => {
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        break;
                    }
                }
            }
            _ if ch.is_whitespace() => {}
            _ => {
                // Unquoted tokens appear in some hand-edited files; keep
                // them so a key still pairs with its value.
                let mut value = String::from(ch);
                while let Some(&next) = chars.peek() {
                    if next.is_whitespace() || matches!(next, '{' | '}' | '"') {
                        break;
                    }
                    value.push(chars.next().unwrap_or_default());
                }
                tokens.push(Some(value));
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBRARIES: &str = r#"
"libraryfolders"
{
    "0"
    {
        "path"        "C:\\Program Files (x86)\\Steam"
        "label"       ""
        "apps"
        {
            "4249100"   "2492188286"
        }
    }
    "1"
    {
        "path"        "F:\\games"
    }
}
"#;

    #[test]
    fn reads_every_library_path() {
        assert_eq!(
            values(LIBRARIES, "path"),
            [
                r"C:\Program Files (x86)\Steam".to_string(),
                r"F:\games".to_string()
            ]
        );
    }

    #[test]
    fn reads_one_value_and_ignores_comments() {
        let text = "// comment\n\"buildid\" \"21744136\"\n";
        assert_eq!(value(text, "buildid").as_deref(), Some("21744136"));
        assert_eq!(value(text, "missing"), None);
    }

    #[test]
    fn ignores_tokens_inside_strings() {
        let text = "\"path\" \"C:\\path \\\"quoted\\\" tail\"\n";
        assert_eq!(
            value(text, "path").as_deref(),
            Some(r#"C:\path "quoted" tail"#)
        );
    }

    #[test]
    fn finds_a_manifest_in_a_library() {
        let dir = std::env::temp_dir().join(format!("arklay-steam-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let steam = dir.join("Steam");
        let library = dir.join("Library");
        let game = library.join("steamapps").join("common").join(INSTALL_DIR);
        for stage in 1..=7 {
            fs::create_dir_all(
                game.join("english")
                    .join("USA")
                    .join(format!("STAGE{stage}")),
            )
            .unwrap();
        }
        for name in ["sound", "data"] {
            fs::create_dir_all(game.join("english").join("USA").join(name)).unwrap();
        }
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        fs::write(
            steam.join("steamapps").join("libraryfolders.vdf"),
            format!(
                "\"libraryfolders\"\n{{\n\"1\"\n{{\n\"path\" \"{}\"\n}}\n}}\n",
                library.display()
            ),
        )
        .unwrap();
        fs::create_dir_all(library.join("steamapps")).unwrap();
        fs::write(
            library.join("steamapps").join(APP_MANIFEST),
            "\"AppState\"\n{\n\"installdir\" \"4249100_Biohazard\"\n\"buildid\" \"21744136\"\n}\n",
        )
        .unwrap();

        let found = find_in_steam(&steam).expect("the synthetic install");
        assert_eq!(found.root, game);
        assert_eq!(found.library, library);
        assert_eq!(found.build_id.as_deref(), Some("21744136"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "requires a locally installed Steam copy of Resident Evil"]
    fn finds_the_real_install() {
        let install = find_re1().expect("a Steam copy of Resident Evil");
        assert!(install.root.is_dir(), "{}", install.root.display());
        eprintln!("found {}", install.root.display());
    }
}
