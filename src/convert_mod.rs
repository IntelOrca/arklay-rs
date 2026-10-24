//! `convert-mod`: turn a Classic REbirth (CR) mod into a mod `.akpak`.
//!
//! A CR mod is the game's own files with arbitrary overrides, shipped either
//! as a game-root overlay (`hires/`, `movie/`, `jpn/PLAYERS/...` extracted
//! over the install) or as a mod folder holding a `manifest.txt` whose paths
//! are relative to the data root (`STAGE1/ROOM106.RDT`, `TEXT/ROOM_105.xml`,
//! ...). The input is a `.zip` or a directory; every file is classified by
//! the directory it sits in, exactly the way `convert-game` names the shipped
//! files, so an override lands on the pack entry it replaces:
//!
//! | CR file | pack entry |
//! |---|---|
//! | `hires/**` (HD textures and `cache/ROOM####.dat`) | `htexture/**` |
//! | `movie/*.mp4`, `movie/*.avi` | `movie/<film>.{mp4,avi}` |
//! | `STAGEn/ROOM####.RDT` | `room/####.rdt` |
//! | `STAGEn/RC*.pak`, `objspr/OSP*.pak` | `roomcut/*.bmp`, `roommask/*.bmp` |
//! | `ENEMY/*.EMD`, `PLAYERS/*` | `enemy/*`, `player/*` |
//! | `sound/*.wav`, `voice/*.wav` | `bgm/*`, `se/*`, `voice/*` |
//! | `DATA/*` | `ui/*`, `item/*`, `data/*`, `demo/*`, `shadow/*` |
//! | `TEXT/ROOM_###.xml` | `language/en-US.txt` room keys |
//! | `init.xml` (BioRand start inventory) | `game/new_game.lua` |
//! | `type.png` (BioRand save background) | `ui/type00.png` |
//!
//! Executables, DLLs, configuration, logs and save data are left out. The
//! mod folder's `manifest.txt` name and `description.txt` become the pack
//! manifest's name and description. Files the engine has no use for yet are
//! listed in the summary instead of being packed.

use std::cell::RefCell;
#[cfg(test)]
use std::collections::HashMap;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::convert::{self, UiKind};
use crate::manifest;
use crate::pack::{Pack, PackWriter};
use crate::state::RoomId;

/// The base pack id every converted CR mod layers over.
const BASE_ID: &str = convert::BASE_PACK_ID;

/// The language file CR room text is written to.
const TEXT_ENTRY: &str = "language/en-US.txt";

/// Directories whose contents are never packed: CR's save folder and the
/// Director's Cut texture set the RE1 HD packs carry beside the main one.
const SKIPPED_DIRS: &[&str] = &["savedata", "_bio1dc"];

/// Extensions of the CR runtime and its tooling, never packed.
const SKIPPED_EXTENSIONS: &[&str] = &[
    "asi", "bat", "cfg", "cmd", "conf", "dgml", "dll", "exe", "ini", "lnk", "log", "md", "pdb",
    "txt", "url",
];

/// Files CR mods carry that only the CR runtime reads: BioRand's executable
/// patch list. They are dropped without a warning.
const SKIPPED_FILES: &[&str] = &["biorand.dat"];

/// Options for [`convert_mod`].
#[derive(Debug, Clone, Default)]
pub struct ConvertModOptions {
    /// The mod: a `.zip` or a directory.
    pub input: PathBuf,
    /// Output pack; `None` writes `<mods dir>/<id>.akpak`.
    pub out: Option<PathBuf>,
    /// Only convert this folder of the input (`re1hd` of a multi-game
    /// archive); paths are taken relative to it.
    pub root: Option<String>,
    /// Base pack for stage 6/7 background copies and unchanged-file
    /// pruning; `None` looks for `games/re1.akpak` under the data roots.
    pub base: Option<PathBuf>,
    /// Pack id override.
    pub id: Option<String>,
    /// Pack name override.
    pub name: Option<String>,
    /// Pack version override.
    pub version: Option<String>,
    /// Replace an existing output pack that belongs to another mod.
    pub force: bool,
    /// Worker threads for compression (0: one per CPU).
    pub jobs: usize,
}

/// The result of [`convert_mod`].
#[derive(Debug, Clone, Default)]
pub struct ConvertModSummary {
    /// The written pack.
    pub out: PathBuf,
    /// The rendered manifest.
    pub manifest: Option<manifest::Manifest>,
    /// Written entries (manifest included).
    pub entries: usize,
    /// Plain bytes of the written entries.
    pub bytes: usize,
    /// Files left out because they are CR runtime, configuration or saves.
    pub skipped: usize,
    /// Files identical to the base pack's entry, left out.
    pub unchanged: usize,
    /// Whether an existing pack was replaced.
    pub replaced: bool,
    /// Non-fatal problems: unmapped files, unsupported tables, ...
    pub warnings: Vec<String>,
}

/// Convert the CR mod at `options.input` into a mod pack.
pub fn convert_mod(options: &ConvertModOptions) -> Result<ConvertModSummary> {
    let input = Input::open(&options.input)?;
    let files = select_root(input.list()?, options.root.as_deref());
    if files.is_empty() {
        match &options.root {
            Some(root) => bail!("{} holds no files under {root}/", options.input.display()),
            None => bail!("{} holds no files", options.input.display()),
        }
    }

    let mut warnings = Vec::new();
    let base = open_base(options.base.as_deref(), &mut warnings)?;
    let info = read_mod_info(&input, &files, &mut warnings)?;

    let mut manifest = manifest::Manifest {
        kind: manifest::PackKind::Mod,
        base: Some(BASE_ID.to_string()),
        ..manifest::Manifest::base("mod")
    };
    let fallback_name = input_stem(&options.input);
    let name = options
        .name
        .clone()
        .or(info.name.clone())
        .unwrap_or_else(|| fallback_name.replace('_', " "));
    manifest.id = match &options.id {
        Some(id) => id.clone(),
        None => slug(&name),
    };
    if manifest.id.is_empty() {
        bail!("cannot derive a pack id from {name:?}; pass --id");
    }
    manifest.name = Some(name);
    manifest.version = options
        .version
        .clone()
        .or_else(|| info.description.as_deref().and_then(version_in))
        .or_else(|| version_in(&fallback_name));
    manifest.description = info.description.clone();

    let out = match &options.out {
        Some(out) => out.clone(),
        None => crate::paths::mods_dir(&crate::paths::primary_data_dir())
            .join(format!("{}.akpak", manifest.id)),
    };
    let replaced = check_output(&out, &manifest.id, options.force)?;

    let mut plan = Plan::default();
    let mut skipped = 0usize;
    let mut unmapped = Vec::new();
    let mut unsupported: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (index, file) in files.iter().enumerate() {
        let (relative, priority) = info.relative(&file.path);
        match classify(relative, &info.mod_roots) {
            Class::Skip => skipped += 1,
            Class::Consumed => {}
            Class::Unmapped => unmapped.push(file.path.clone()),
            Class::Unsupported(reason) => {
                unsupported
                    .entry(reason)
                    .or_default()
                    .push(file.path.clone());
            }
            Class::Map(targets) => {
                for target in targets {
                    plan.insert(target, priority, index, &files, &mut warnings);
                }
            }
        }
    }
    plan.drop_shadowed_films();
    plan.add_late_stage_copies(base.as_ref());

    let mut writer = PackWriter::new();
    let mut unchanged = 0usize;
    let mut texts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut palette = None;
    let mut entries = 0usize;
    let mut bytes = 0usize;
    eprintln!(
        "converting {} file(s) from {}",
        plan.targets.len(),
        options.input.display()
    );
    for (entry, planned) in &plan.targets {
        let file = &files[planned.file];
        let raw = input
            .read(file)
            .with_context(|| format!("failed to read {}", file.path))?;
        let data = match &planned.convert {
            Convert::Raw => raw,
            Convert::Camera => convert::camera_bmp(&file.path, &raw)?,
            Convert::Mask => convert::roommask_bmp(&file.path, &raw)?,
            Convert::Ui(kind) => convert::convert_ui_asset(*kind, &raw)
                .with_context(|| format!("failed to convert {}", file.path))?,
            Convert::ItemAtlas(rows) => {
                if palette.is_none() {
                    palette = Some(item_palette(&input, &files, &plan, base.as_ref()));
                }
                match palette.as_ref().and_then(Option::as_ref) {
                    Some(texture) => convert::bake_item_atlas(&raw, *rows, texture)
                        .with_context(|| format!("failed to bake {}", file.path))?,
                    None => {
                        warnings.push(format!(
                            "{}: no STATUS.TIM palette in the mod or a base pack; item icons left out",
                            file.path
                        ));
                        continue;
                    }
                }
            }
            Convert::RoomText(room3) => {
                let source = String::from_utf8_lossy(&raw);
                let strings = parse_room_text(&source)
                    .with_context(|| format!("failed to parse {}", file.path))?;
                texts.insert(room3.clone(), strings);
                continue;
            }
            Convert::StartInventory => {
                let source = String::from_utf8_lossy(&raw);
                start_inventory_script(&source)
                    .with_context(|| format!("failed to convert {}", file.path))?
                    .into_bytes()
            }
        };
        // An override identical to the base entry changes nothing.
        if let Some(base) = &base
            && base.contains(entry)
            && base.read(entry).is_ok_and(|existing| *existing == *data)
        {
            unchanged += 1;
            continue;
        }
        entries += 1;
        bytes += data.len();
        writer
            .add(entry, data)
            .with_context(|| format!("failed to add {entry}"))?;
    }

    if !texts.is_empty() {
        let language = room_text_language(&texts, &mut warnings);
        entries += 1;
        bytes += language.len();
        writer.add(TEXT_ENTRY, language.into_bytes())?;
    }

    for (reason, paths) in unsupported {
        warnings.push(format!("{}: {reason}", file_list(&paths)));
    }
    if !unmapped.is_empty() {
        warnings.push(format!(
            "{} have no pack entry and were left out",
            file_list(&unmapped)
        ));
    }
    if entries == 0 {
        bail!(
            "{} has nothing to pack: no file overrides a game entry",
            options.input.display()
        );
    }

    let rendered = manifest.render();
    entries += 1;
    bytes += rendered.len();
    writer.add(manifest::ENTRY, rendered.into_bytes())?;

    eprintln!("compressing {} entries", writer.len());
    writer.compress_entries(jobs(options.jobs), convert::is_precompressed)?;
    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    writer.write(&out)?;

    Ok(ConvertModSummary {
        out,
        manifest: Some(manifest),
        entries,
        bytes,
        skipped,
        unchanged,
        replaced,
        warnings,
    })
}

/// `N file(s) (a, b, ...)`: a count with the first few paths.
fn file_list(paths: &[String]) -> String {
    const SHOWN: usize = 6;
    let mut sorted: Vec<&str> = paths.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let more = if sorted.len() > SHOWN { ", ..." } else { "" };
    sorted.truncate(SHOWN);
    format!("{} file(s) ({}{more})", paths.len(), sorted.join(", "))
}

/// The worker count `jobs` asks for (0: one per CPU).
fn jobs(jobs: usize) -> usize {
    if jobs > 0 {
        return jobs;
    }
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// Refuse to overwrite another mod's pack: an existing `out` is replaced only
/// when it declares the same id (a re-conversion) or `force` is set. Returns
/// whether a pack is being replaced.
fn check_output(out: &Path, id: &str, force: bool) -> Result<bool> {
    if !out.exists() {
        return Ok(false);
    }
    if force {
        return Ok(true);
    }
    let existing = Pack::open(out)
        .ok()
        .and_then(|pack| pack.manifest().map(|manifest| manifest.id.clone()));
    match existing {
        Some(existing) if existing == id => Ok(true),
        Some(existing) => bail!(
            "{} already exists and holds mod \"{existing}\", not \"{id}\"; pass --force to replace it or -o to write elsewhere",
            out.display()
        ),
        None => bail!(
            "{} already exists and is not a mod pack; pass --force to replace it or -o to write elsewhere",
            out.display()
        ),
    }
}

/// Open the base pack: the given path, or the first `games/re1.akpak` under
/// the data roots. A missing base only limits the conversion.
fn open_base(path: Option<&Path>, warnings: &mut Vec<String>) -> Result<Option<Pack>> {
    if let Some(path) = path {
        return Pack::open(path)
            .map(Some)
            .with_context(|| format!("failed to open base pack {}", path.display()));
    }
    let found = crate::paths::data_roots()
        .into_iter()
        .map(|root| crate::paths::games_dir(&root).join(format!("{BASE_ID}.akpak")))
        .find(|path| path.is_file());
    match found.map(|path| Pack::open(&path)) {
        Some(Ok(pack)) => Ok(Some(pack)),
        Some(Err(err)) => {
            warnings.push(format!(
                "base pack unreadable ({err:#}); converting without it"
            ));
            Ok(None)
        }
        None => {
            warnings.push(
                "no base pack found (pass --base); stage 6/7 background copies and unchanged-file pruning are skipped"
                    .to_string(),
            );
            Ok(None)
        }
    }
}

/// The input file name without its extension.
fn input_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rsplit_once('.') {
        Some((stem, extension)) if extension.eq_ignore_ascii_case("zip") => stem.to_string(),
        _ => name,
    }
}

/// A pack id from a display name: lower-case ASCII letters and digits, runs
/// of anything else collapsed to one `-`.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The first version-looking token: `v2.0`, `3.3.2`, ...
fn version_in(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let skip_v = matches!(bytes[start], b'v' | b'V')
            && bytes.get(start + 1).is_some_and(u8::is_ascii_digit)
            && (start == 0 || !bytes[start - 1].is_ascii_alphanumeric());
        let digit_start = if skip_v { start + 1 } else { start };
        let boundary =
            digit_start == 0 || !bytes[digit_start - 1].is_ascii_alphanumeric() || skip_v;
        if boundary && bytes[digit_start].is_ascii_digit() {
            let mut end = digit_start;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
                end += 1;
            }
            let token = text[digit_start..end].trim_end_matches('.');
            if token.contains('.') {
                return Some(token.to_string());
            }
            start = end.max(start + 1);
            continue;
        }
        start += 1;
    }
    None
}

/// Keep the files under `root` (matched ignoring case), relative to it.
fn select_root(files: Vec<InputFile>, root: Option<&str>) -> Vec<InputFile> {
    let Some(root) = root
        .map(|root| root.trim_matches('/'))
        .filter(|root| !root.is_empty())
    else {
        return files;
    };
    let prefix = format!("{}/", root.to_ascii_lowercase());
    files
        .into_iter()
        .filter_map(|mut file| {
            let lower = file.path.to_ascii_lowercase();
            lower.starts_with(&prefix).then(|| {
                file.path = file.path[prefix.len()..].to_string();
                file
            })
        })
        .collect()
}

/// One file of the input.
#[derive(Debug, Clone)]
struct InputFile {
    /// Path relative to the input root, `/`-separated, original case.
    path: String,
    /// Where the bytes live.
    handle: Handle,
}

#[derive(Debug, Clone)]
enum Handle {
    Path(PathBuf),
    Zip(usize),
}

/// The mod being converted: a directory or a zip archive.
enum Input {
    Dir(PathBuf),
    Zip(RefCell<zip::ZipArchive<fs::File>>),
}

impl Input {
    fn open(path: &Path) -> Result<Self> {
        if path.is_dir() {
            return Ok(Self::Dir(path.to_path_buf()));
        }
        let file =
            fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let archive = zip::ZipArchive::new(file)
            .with_context(|| format!("{} is neither a directory nor a zip", path.display()))?;
        Ok(Self::Zip(RefCell::new(archive)))
    }

    /// Every file, sorted by lowercased path.
    fn list(&self) -> Result<Vec<InputFile>> {
        let mut files = Vec::new();
        match self {
            Self::Dir(root) => walk(root, "", &mut files)?,
            Self::Zip(archive) => {
                let mut archive = archive.borrow_mut();
                for index in 0..archive.len() {
                    let file = archive.by_index_raw(index)?;
                    if file.is_dir() {
                        continue;
                    }
                    // `enclosed_name` rejects absolute and `..` paths.
                    let Some(name) = file.enclosed_name() else {
                        continue;
                    };
                    let path = name
                        .components()
                        .map(|part| part.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    files.push(InputFile {
                        path,
                        handle: Handle::Zip(index),
                    });
                }
            }
        }
        files.sort_by(|a, b| {
            a.path
                .to_ascii_lowercase()
                .cmp(&b.path.to_ascii_lowercase())
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(files)
    }

    fn read(&self, file: &InputFile) -> Result<Vec<u8>> {
        match (&file.handle, self) {
            (Handle::Path(path), _) => {
                fs::read(path).with_context(|| format!("failed to read {}", path.display()))
            }
            (Handle::Zip(index), Self::Zip(archive)) => {
                let mut archive = archive.borrow_mut();
                let mut entry = archive.by_index(*index)?;
                let mut data = Vec::with_capacity(entry.size().min(1 << 30) as usize);
                entry.read_to_end(&mut data)?;
                Ok(data)
            }
            (Handle::Zip(_), Self::Dir(_)) => unreachable!("zip handle from a directory input"),
        }
    }
}

/// Recursively list the files under `dir`.
fn walk(dir: &Path, prefix: &str, files: &mut Vec<InputFile>) -> Result<()> {
    let read = fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?;
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            walk(&path, &relative, files)?;
        } else if path.is_file() {
            files.push(InputFile {
                path: relative,
                handle: Handle::Path(path),
            });
        }
    }
    Ok(())
}

/// What the mod folder files say about the mod.
#[derive(Debug, Default)]
struct ModInfo {
    /// Directories (input-relative, lowercased, with a trailing `/`, or the
    /// empty root) that hold a CR `manifest.txt`.
    mod_roots: Vec<String>,
    /// `Name` from the first `manifest.txt`.
    name: Option<String>,
    /// The first mod folder's `description.txt`.
    description: Option<String>,
}

impl ModInfo {
    /// The path relative to its mod folder and its layer priority: files in
    /// a mod folder override the game-root overlay, like CR applies them.
    fn relative<'a>(&self, path: &'a str) -> (&'a str, u8) {
        let lower = path.to_ascii_lowercase();
        for root in &self.mod_roots {
            if !root.is_empty() && lower.starts_with(root.as_str()) {
                return (&path[root.len()..], 1);
            }
        }
        let priority = u8::from(self.mod_roots.iter().any(String::is_empty));
        (path, priority)
    }
}

/// Find the CR mod folders and read their name and description.
fn read_mod_info(
    input: &Input,
    files: &[InputFile],
    warnings: &mut Vec<String>,
) -> Result<ModInfo> {
    let mut info = ModInfo::default();
    for file in files {
        let lower = file.path.to_ascii_lowercase();
        let (dir, name) = match lower.rsplit_once('/') {
            Some((dir, name)) => (format!("{dir}/"), name.to_string()),
            None => (String::new(), lower.clone()),
        };
        if name != "manifest.txt" {
            continue;
        }
        let text = String::from_utf8_lossy(&input.read(file)?).into_owned();
        if !text.to_ascii_lowercase().contains("[mod]") {
            continue;
        }
        if info.mod_roots.is_empty() {
            info.name = ini_value(&text, "Name").filter(|name| !name.is_empty());
            let description = files.iter().find(|other| {
                other
                    .path
                    .eq_ignore_ascii_case(&format!("{}description.txt", &file.path[..dir.len()]))
            });
            if let Some(description) = description {
                let text = String::from_utf8_lossy(&input.read(description)?)
                    .replace("\r\n", "\n")
                    .trim()
                    .to_string();
                info.description = (!text.is_empty()).then_some(text);
            }
        } else {
            warnings.push(format!(
                "several CR mod folders; the pack name comes from {}",
                info.mod_roots[0]
            ));
        }
        info.mod_roots.push(dir);
    }
    Ok(info)
}

/// `key = value` from an INI-style text, the key matched ignoring case.
fn ini_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case(key)
            .then(|| value.trim().to_string())
    })
}

/// How one input file maps onto the pack.
#[derive(Debug)]
enum Class {
    /// CR runtime, configuration or save data: dropped silently.
    Skip,
    /// Read for the manifest (`manifest.txt`, `description.txt`).
    Consumed,
    /// Nothing in the engine reads it.
    Unmapped,
    /// Something arklay does not convert yet; the reason is reported.
    Unsupported(&'static str),
    /// The pack entries it produces.
    Map(Vec<Target>),
}

/// One pack entry a file produces.
#[derive(Debug, Clone)]
struct Target {
    entry: String,
    convert: Convert,
}

/// How a file's bytes become the entry's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Convert {
    Raw,
    Camera,
    Mask,
    Ui(UiKind),
    ItemAtlas(usize),
    RoomText(String),
    StartInventory,
}

fn raw(entry: impl Into<String>) -> Class {
    Class::Map(vec![Target {
        entry: entry.into(),
        convert: Convert::Raw,
    }])
}

fn converted(entry: impl Into<String>, convert: Convert) -> Class {
    Class::Map(vec![Target {
        entry: entry.into(),
        convert,
    }])
}

/// Classify one file by its mod-relative path.
fn classify(relative: &str, mod_roots: &[String]) -> Class {
    let parts: Vec<&str> = relative
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let Some(&file) = parts.last() else {
        return Class::Skip;
    };
    let lower: Vec<String> = parts.iter().map(|part| part.to_ascii_lowercase()).collect();
    let name = lower[lower.len() - 1].as_str();
    let (stem, extension) = name.rsplit_once('.').unwrap_or((name, ""));

    if lower
        .iter()
        .any(|part| SKIPPED_DIRS.contains(&part.as_str()))
    {
        return Class::Skip;
    }
    // A nested mod folder's own manifest files are read by `read_mod_info`.
    if (name == "manifest.txt" || name == "description.txt")
        && (parts.len() == 1 || !mod_roots.is_empty())
    {
        return Class::Consumed;
    }
    if SKIPPED_FILES.contains(&name) || SKIPPED_EXTENSIONS.contains(&extension) {
        return Class::Skip;
    }

    // HD textures: everything under `hires/`, wherever the folder sits.
    if let Some(index) = lower.iter().position(|part| part == "hires")
        && index + 1 < lower.len()
    {
        return raw(format!("htexture/{}", lower[index + 1..].join("/")));
    }

    let parent = lower
        .len()
        .checked_sub(2)
        .map(|index| lower[index].as_str());
    let mapped = match parent {
        Some(dir) if convert::stage_dir_digit(dir).is_some() => classify_stage(name),
        Some("objspr") => classify_mask(name),
        Some("enemy") => classify_enemy(name),
        Some("players") => classify_players(name),
        Some("sound") => classify_sound(file),
        Some("voice") => classify_voice(name),
        Some("movie") => classify_movie(stem, extension),
        Some("data") => classify_data(name, stem, extension),
        Some("effspr") => classify_effect(stem, extension),
        Some("item_m1") => classify_door(stem, extension),
        Some("text") => classify_text(stem, extension),
        // Room sound tables only exist to give a room's swapped-in enemies
        // their sounds (BioRand rewrites one per room); arklay's enemies carry
        // their own sounds, so the tables are not needed.
        Some("tables") if extension == "xml" => Class::Skip,
        _ => Class::Unmapped,
    };
    if !matches!(mapped, Class::Unmapped) {
        return mapped;
    }

    // Files a mod folder carries at its own root.
    match name {
        "init.xml" => converted(crate::lua::NEW_GAME_ENTRY, Convert::StartInventory),
        "bgm_tbl.xml" => Class::Unsupported(
            "the room music table is not converted yet; rooms keep the base game's music",
        ),
        "type.png" => raw("ui/type00.png"),
        _ => mapped,
    }
}

/// `STAGEn/ROOM####.RDT` and `STAGEn/RC*.pak`.
fn classify_stage(name: &str) -> Class {
    if let Some(Ok(id)) = convert::rdt_id_from_file_name(name) {
        return raw(id.rdt_entry());
    }
    // `RC{stage}{room:02X}{camera:X}.pak`.
    if let Some(digits) = name
        .strip_prefix("rc")
        .and_then(|rest| rest.strip_suffix(".pak"))
        && digits.len() == 4
        && let (Some(stage), Ok(room), Ok(camera)) = (
            digits[..1].parse::<u8>().ok(),
            u8::from_str_radix(&digits[1..3], 16),
            usize::from_str_radix(&digits[3..], 16),
        )
        && (1..=5).contains(&stage)
    {
        let id = RoomId {
            stage,
            room,
            player_flag: 0,
        };
        return converted(id.cut_entry(camera), Convert::Camera);
    }
    Class::Unmapped
}

/// `objspr/OSP0{stage-1}{room:02}{camera}.pak`.
fn classify_mask(name: &str) -> Class {
    let Some(digits) = name
        .strip_prefix("osp0")
        .and_then(|rest| rest.strip_suffix(".pak"))
    else {
        return Class::Unmapped;
    };
    if digits.len() < 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Class::Unmapped;
    }
    let (Ok(stage), Ok(room), Ok(camera)) = (
        digits[..1].parse::<u8>(),
        digits[1..3].parse::<u8>(),
        digits[3..].parse::<usize>(),
    ) else {
        return Class::Unmapped;
    };
    if stage > 4 {
        return Class::Unmapped;
    }
    let id = RoomId {
        stage: stage + 1,
        room,
        player_flag: 0,
    };
    converted(id.roommask_entry(camera), Convert::Mask)
}

/// `ENEMY/`: player, costume, scripted-character and monster models.
fn classify_enemy(name: &str) -> Class {
    let Some(stem) = name.strip_suffix(".emd") else {
        return Class::Unmapped;
    };
    if let Some(digit) = stem.strip_prefix("char1")
        && let Ok(id) = digit.parse::<u8>()
        && id < 4
    {
        return raw(format!("player/{id:02}.emd"));
    }
    let costume = match stem {
        "em1030" => Some((0, 0)),
        "em1031" => Some((1, 0)),
        "em1032" => Some((0, 1)),
        "em1033" => Some((1, 1)),
        _ => None,
    };
    if let Some((character, variant)) = costume {
        return raw(convert::costume_entry(character, variant));
    }
    let Some(digits) = stem.strip_prefix("em1") else {
        return Class::Unmapped;
    };
    if digits.len() != 3 {
        return Class::Unmapped;
    }
    let (Ok(player), Ok(id)) = (
        digits[..1].parse::<u8>(),
        u8::from_str_radix(&digits[1..], 16),
    ) else {
        return Class::Unmapped;
    };
    if player == 0
        && let Some(path) = crate::enemy::model_path(id)
    {
        return raw(path);
    }
    if player <= 1 && id <= 0x15 {
        return raw(format!("enemy/em{id:02x}{player}.emd"));
    }
    Class::Unmapped
}

/// `PLAYERS/`: the locomotion clips, weapon animations and held models.
fn classify_players(name: &str) -> Class {
    match name {
        "w00.emw" => return raw("player/00.emw"),
        "w10.emw" => return raw("player/01.emw"),
        _ => {}
    }
    if let Some(stem) = name.strip_prefix('w').and_then(|s| s.strip_suffix(".emw"))
        && stem.len() == 2
        && let Ok(id) = u8::from_str_radix(stem, 16)
    {
        return raw(format!("player/w{id:02x}.emw"));
    }
    if let Some(stem) = name.strip_prefix("ws").and_then(|s| s.strip_suffix(".tmd"))
        && stem.len() == 3
        && let Ok(id) = u16::from_str_radix(stem, 16)
    {
        return raw(format!("player/ws{id:03x}.tmd"));
    }
    Class::Unmapped
}

/// `sound/`: BGM tracks and the named effects.
fn classify_sound(file: &str) -> Class {
    if let Some(path) = crate::music::pack_path(file) {
        return raw(path);
    }
    let lower = file.to_ascii_lowercase();
    let Some(stem) = lower.strip_suffix(".wav") else {
        return Class::Unmapped;
    };
    let known = crate::sfx::SE_NAMES
        .iter()
        .copied()
        .chain(crate::music::se_track_names())
        .any(|name| name.eq_ignore_ascii_case(stem));
    if !known {
        return Class::Unmapped;
    }
    // The room tables' copy, plus every enemy sound slot that plays it.
    let enemy = crate::sfx::ENEMY_SOUNDS.iter().flat_map(|(set, slots)| {
        slots
            .iter()
            .filter(|(_, name)| name.eq_ignore_ascii_case(stem))
            .map(move |(slot, _)| format!("{}/{set}_{slot:02x}.wav", crate::sfx::ENEMY_DIR))
    });
    Class::Map(
        std::iter::once(format!("se/{stem}.wav"))
            .chain(enemy)
            .map(|entry| Target {
                entry,
                convert: Convert::Raw,
            })
            .collect(),
    )
}

/// `voice/`: every table record that plays the file.
fn classify_voice(name: &str) -> Class {
    let targets: Vec<Target> = crate::voice_table::entries()
        .filter(|(_, _, file)| format!("{}.wav", file.to_ascii_lowercase()) == name)
        .map(|(row, id, _)| Target {
            entry: crate::voice::entry_path(row, id),
            convert: Convert::Raw,
        })
        .collect();
    if targets.is_empty() {
        Class::Unmapped
    } else {
        Class::Map(targets)
    }
}

/// `movie/`: films by their table name (either release spelling).
fn classify_movie(stem: &str, extension: &str) -> Class {
    if extension != "mp4" && extension != "avi" {
        return Class::Unmapped;
    }
    let canonical = (0..=u8::MAX)
        .map_while(|id| (usize::from(id) < 64).then(|| crate::movie::name(id)))
        .flatten()
        .find(|name| crate::movie::release_names(name).any(|spelling| spelling == stem));
    match canonical {
        Some(name) => raw(format!("{}.{extension}", crate::movie::stem(name))),
        None => Class::Unmapped,
    }
}

/// `DATA/`: UI art, item atlases, the new-game card, demos and effects.
fn classify_data(name: &str, stem: &str, extension: &str) -> Class {
    for &(entry, file, kind) in convert::UI_ASSETS {
        let file = file.to_ascii_lowercase();
        if file == name {
            return converted(entry, Convert::Ui(kind));
        }
        // A PNG/WebP/BMP named after a UI file replaces it at its stem.
        let entry_stem = crate::assets::stem(entry);
        if matches!(extension, "png" | "webp" | "bmp")
            && file.rsplit_once('.').map(|(file_stem, _)| file_stem) == Some(stem)
        {
            return raw(format!("{entry_stem}.{extension}"));
        }
    }
    for &(entry, file, rows) in convert::ITEM_ASSETS {
        if file.eq_ignore_ascii_case(name) {
            return converted(entry, Convert::ItemAtlas(rows));
        }
    }
    if name.eq_ignore_ascii_case(convert::BIO_CARD_FILE) {
        return raw(convert::BIO_CARD_ENTRY);
    }
    if let Some(index) = convert::DEMO_FILES
        .iter()
        .position(|file| file.eq_ignore_ascii_case(name))
    {
        return raw(format!("demo/{index:02x}.dat"));
    }
    if name.eq_ignore_ascii_case(convert::KAGE_FILE) {
        return raw(crate::shadow::KAGE_ENTRY);
    }
    if name.eq_ignore_ascii_case(convert::CORE_ESP_FILE) {
        return raw(crate::effects::room::CORE_ESP_ENTRY);
    }
    if name.eq_ignore_ascii_case(convert::CORE_ETM_FILE) {
        return raw(crate::effects::room::CORE_ETM_ENTRY);
    }
    Class::Unmapped
}

/// `effspr/`: the effect sheets.
fn classify_effect(stem: &str, extension: &str) -> Class {
    if extension == "tim"
        && convert::EFFECT_SHEET_FILES
            .iter()
            .any(|name| name.eq_ignore_ascii_case(stem))
    {
        raw(format!("effspr/{stem}.tim"))
    } else {
        Class::Unmapped
    }
}

/// `ITEM_M1/`: door animations.
fn classify_door(stem: &str, extension: &str) -> Class {
    let known = (0..=0x21u8).any(|door_type| crate::door::type_name(door_type) == stem);
    if extension == "dor" && known {
        raw(format!("door/{stem}.dor"))
    } else {
        Class::Unmapped
    }
}

/// `TEXT/ROOM_###.xml`: CR's room text replacement.
fn classify_text(stem: &str, extension: &str) -> Class {
    let Some(room3) = stem.strip_prefix("room_") else {
        return Class::Unmapped;
    };
    if extension != "xml" || room3.len() != 3 || !room3.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Class::Unmapped;
    }
    converted(TEXT_ENTRY, Convert::RoomText(room3.to_ascii_uppercase()))
}

/// The planned entries, keyed by entry path.
#[derive(Debug, Default)]
struct Plan {
    /// Room text files are keyed `text/<room>`: they merge into one
    /// language file instead of owning an entry each.
    targets: BTreeMap<String, Planned>,
}

#[derive(Debug, Clone)]
struct Planned {
    file: usize,
    priority: u8,
    convert: Convert,
}

impl Plan {
    fn insert(
        &mut self,
        target: Target,
        priority: u8,
        file: usize,
        files: &[InputFile],
        warnings: &mut Vec<String>,
    ) {
        let planned = Planned {
            file,
            priority,
            convert: target.convert,
        };
        if matches!(planned.convert, Convert::RoomText(_)) {
            // Keyed per room so a mod folder's file wins over the overlay's.
            let room = |p: &Planned| match &p.convert {
                Convert::RoomText(room) => room.clone(),
                _ => String::new(),
            };
            let key = format!("text/{}", room(&planned));
            return self.insert_keyed(key, planned, files, warnings);
        }
        self.insert_keyed(target.entry, planned, files, warnings);
    }

    fn insert_keyed(
        &mut self,
        key: String,
        planned: Planned,
        files: &[InputFile],
        warnings: &mut Vec<String>,
    ) {
        match self.targets.get(&key) {
            Some(existing) if existing.priority > planned.priority => {}
            Some(existing) if existing.priority == planned.priority => warnings.push(format!(
                "{} and {} both map to {key}; keeping the first",
                files[existing.file].path, files[planned.file].path
            )),
            _ => {
                self.targets.insert(key, planned);
            }
        }
    }

    /// An AVI next to an MP4 of the same film is never played (the stem
    /// lookup prefers MP4), so it is left out.
    fn drop_shadowed_films(&mut self) {
        let mp4: HashSet<String> = self
            .targets
            .keys()
            .filter_map(|entry| entry.strip_suffix(".mp4"))
            .filter(|stem| stem.starts_with("movie/"))
            .map(str::to_owned)
            .collect();
        self.targets.retain(|entry, _| {
            entry
                .strip_suffix(".avi")
                .is_none_or(|stem| !mp4.contains(stem))
        });
    }

    /// Stages 6 and 7 reuse the stage 1 and 2 background files: copy a cut or
    /// mask override to the late stage when the base (or the mod) has that
    /// room there.
    fn add_late_stage_copies(&mut self, base: Option<&Pack>) {
        let rooms: HashSet<String> = self
            .targets
            .keys()
            .filter_map(|entry| entry.strip_prefix("room/"))
            .map(|rdt| rdt[..3.min(rdt.len())].to_ascii_uppercase())
            .collect();
        let mut copies = Vec::new();
        for (entry, planned) in &self.targets {
            let Some((dir, rest)) = entry.split_once('/') else {
                continue;
            };
            if !matches!(planned.convert, Convert::Camera | Convert::Mask) {
                continue;
            }
            let late = match rest.as_bytes().first() {
                Some(b'1') => '6',
                Some(b'2') => '7',
                _ => continue,
            };
            let late_rest = format!("{late}{}", &rest[1..]);
            let late_entry = format!("{dir}/{late_rest}");
            let room3 = late_rest[..3].to_ascii_uppercase();
            let in_base = base.is_some_and(|base| {
                base.has_asset(
                    crate::assets::stem(&late_entry),
                    crate::pack::AssetClass::Image,
                )
            });
            if in_base || rooms.contains(&room3) {
                copies.push((late_entry, planned.clone()));
            }
        }
        for (entry, planned) in copies {
            self.targets.entry(entry).or_insert(planned);
        }
    }
}

/// The item-atlas palette: the mod's own `STATUS.TIM`, else the base's.
fn item_palette(
    input: &Input,
    files: &[InputFile],
    plan: &Plan,
    base: Option<&Pack>,
) -> Option<crate::model::Texture8> {
    let status = plan
        .targets
        .get("ui/status.tim")
        .and_then(|planned| input.read(&files[planned.file]).ok());
    let status = status.or_else(|| {
        base.and_then(|base| {
            base.read("ui/status.tim")
                .ok()
                .map(|data| data.into_owned())
        })
    })?;
    crate::tim::decode_8bpp(&status).ok()
}

/// The `<Text>` strings of a CR `ROOM_###.xml`, in message-id order.
fn parse_room_text(source: &str) -> Result<Vec<String>> {
    let mut strings = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("<Text") {
        let after = &rest[start + 5..];
        let Some(close) = after.find('>') else {
            bail!("unterminated <Text> element");
        };
        // `<Texts>` or another tag that only starts with `Text`.
        if !after[..close].is_empty()
            && !after[..close].starts_with(char::is_whitespace)
            && !after[..close].starts_with('/')
        {
            rest = &after[close + 1..];
            continue;
        }
        if after[..close].trim_end().ends_with('/') {
            strings.push(String::new());
            rest = &after[close + 1..];
            continue;
        }
        let body = &after[close + 1..];
        let Some(end) = body.find("</Text>") else {
            bail!("<Text> element {} is not closed", strings.len());
        };
        strings.push(decode_entities(&body[..end]));
        rest = &body[end + 7..];
    }
    Ok(strings)
}

/// Decode the five XML entities and numeric character references.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find(';').filter(|end| *end <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let name = &after[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Render the room strings as a language file, dropping (and reporting) any
/// string the language grammar rejects.
fn room_text_language(texts: &BTreeMap<String, Vec<String>>, warnings: &mut Vec<String>) -> String {
    let mut out =
        String::from("# Room text converted from the CR mod's TEXT/ROOM_###.xml files.\n");
    for (room3, strings) in texts {
        for (id, text) in strings.iter().enumerate() {
            let line = format!(
                "ROOM_{room3}_TEXT_{id:02X} = \"{}\"\n",
                language_value(text)
            );
            if let Err(err) = crate::language::Language::parse(&line) {
                warnings.push(format!(
                    "TEXT/ROOM_{room3}.xml string {id}: {err:#}; the base text stays"
                ));
                continue;
            }
            out.push_str(&line);
        }
    }
    out
}

/// A CR string as a language-file value: CR already spells line breaks as
/// `\n` and tags as `{scroll n}`; quotes and raw line breaks are escaped.
fn language_value(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => out.push_str("\\\""),
            '\r' => {}
            '\n' => out.push_str("\\n"),
            '\\' => match chars.peek() {
                // Keep the escapes both grammars share; a lone backslash is
                // literal.
                Some('n' | '"' | '\\' | 'x') => out.push('\\'),
                _ => out.push_str("\\\\"),
            },
            other => out.push(other),
        }
    }
    out
}

/// One `<Entry id count/>` of a BioRand `init.xml`.
type Stack = (u8, u8);

/// Convert BioRand's `init.xml` start inventory into a `game/new_game.lua`.
///
/// BioRand writes one `<Player>` per character, Jill's first: Jill's holds
/// her eight slots, Chris's his six followed by Rebecca's six. An empty
/// `<Player/>` keeps that character's shipped items.
fn start_inventory_script(source: &str) -> Result<String> {
    let players = parse_init_players(source)?;
    let mut chris: Option<Vec<Stack>> = None;
    let mut jill: Option<Vec<Stack>> = None;
    for (index, stacks) in players.into_iter().enumerate() {
        let slot = match stacks.len() {
            12 => &mut chris,
            8 => &mut jill,
            0 => continue,
            _ if index == 0 => &mut jill,
            _ => &mut chris,
        };
        if slot.is_some() {
            bail!("init.xml lists the same character twice");
        }
        *slot = Some(stacks);
    }

    let mut out = String::from(
        "-- The new-game start inventory, generated by `arklay convert-mod` from\n\
         -- the mod's init.xml. See games/re1/game/new_game.lua for the API.\n\n\
         local api, character = ...\n\n",
    );
    let list = |stacks: &[Stack]| -> String {
        let items: Vec<String> = stacks
            .iter()
            .map(|(id, count)| format!("{{ {id}, {count} }}"))
            .collect();
        format!("{{ {} }}", items.join(", "))
    };
    out.push_str("local START = {\n");
    if let Some(stacks) = &chris {
        let (player, partner) = stacks.split_at(crate::game::INVENTORY_SLOTS_CHRIS);
        let _ = writeln!(
            out,
            "    [0] = {{ player = {}, partner = {} }},",
            list(player),
            list(partner)
        );
    }
    if let Some(stacks) = &jill {
        let _ = writeln!(out, "    [1] = {{ player = {} }},", list(stacks));
    }
    out.push_str("}\n\n");
    out.push_str(
        "local start = START[character]\n\
         if start == nil then\n\
         \x20   -- Not listed by the mod: the shipped items.\n\
         \x20   if character == 0 then\n\
         \x20       api:set_item_slot(0, 0x01, 0)\n\
         \x20       api:set_item_slot(1, 0x41, 1)\n\
         \x20       api:set_partner_slot(0, 0x02, 15)\n\
         \x20   else\n\
         \x20       api:set_item_slot(0, 0x01, 0)\n\
         \x20       api:set_item_slot(1, 0x02, 15)\n\
         \x20       api:set_item_slot(2, 0x41, 1)\n\
         \x20   end\n\
         \x20   return\n\
         end\n\n\
         local slot = 0\n\
         for _, stack in ipairs(start.player) do\n\
         \x20   if stack[1] ~= 0 then\n\
         \x20       api:set_item_slot(slot, stack[1], stack[2])\n\
         \x20       slot = slot + 1\n\
         \x20   end\n\
         end\n\
         if start.partner then\n\
         \x20   for index, stack in ipairs(start.partner) do\n\
         \x20       api:set_partner_slot(index - 1, stack[1], stack[2])\n\
         \x20   end\n\
         end\n",
    );
    crate::lua::verify_source(out.as_bytes())?;
    Ok(out)
}

/// The `<Entry>` stacks of each `<Player>` in `init.xml`, in file order.
fn parse_init_players(source: &str) -> Result<Vec<Vec<Stack>>> {
    let mut players = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("<Player") {
        let after = &rest[start + 7..];
        let Some(close) = after.find('>') else {
            bail!("unterminated <Player> element");
        };
        if after[..close].trim_end().ends_with('/') {
            players.push(Vec::new());
            rest = &after[close + 1..];
            continue;
        }
        let body = &after[close + 1..];
        let end = body.find("</Player>").unwrap_or(body.len());
        let mut stacks = Vec::new();
        let mut entries = &body[..end];
        while let Some(entry) = entries.find("<Entry") {
            let tail = &entries[entry..];
            let close = tail.find('>').unwrap_or(tail.len());
            let element = &tail[..close];
            let id = attribute(element, "id").context("an <Entry> has no id")?;
            let count = attribute(element, "count").unwrap_or(0);
            stacks.push((id, count));
            entries = &tail[close.min(tail.len())..];
            if entries.is_empty() {
                break;
            }
            entries = &entries[1..];
        }
        players.push(stacks);
        rest = &body[end.min(body.len())..];
    }
    if players.is_empty() {
        bail!("no <Player> elements");
    }
    Ok(players)
}

/// A numeric `name="value"` attribute of one XML element.
fn attribute(element: &str, name: &str) -> Option<u8> {
    let key = format!(" {name}=");
    let start = element.find(&key)? + key.len();
    let value = &element[start..];
    let quote = value
        .chars()
        .next()
        .filter(|ch| *ch == '"' || *ch == '\'')?;
    let value = &value[1..];
    let end = value.find(quote)?;
    let value = value[..end].trim();
    // Counts above a byte (BioRand never writes them) saturate.
    value
        .parse::<u32>()
        .ok()
        .map(|value| u8::try_from(value).unwrap_or(u8::MAX))
}

/// Room text keys per CR file, used by the tests.
#[cfg(test)]
fn mapped_entries(paths: &[&str], mod_roots: &[&str]) -> HashMap<String, Vec<String>> {
    let roots: Vec<String> = mod_roots.iter().map(|root| root.to_string()).collect();
    let info = ModInfo {
        mod_roots: roots.clone(),
        ..ModInfo::default()
    };
    paths
        .iter()
        .map(|path| {
            let (relative, _) = info.relative(path);
            let entries = match classify(relative, &roots) {
                Class::Map(targets) => targets.into_iter().map(|target| target.entry).collect(),
                Class::Skip => vec!["<skip>".to_string()],
                Class::Consumed => vec!["<consumed>".to_string()],
                Class::Unmapped => vec!["<unmapped>".to_string()],
                Class::Unsupported(_) => vec!["<unsupported>".to_string()],
            };
            (path.to_string(), entries)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(map: &HashMap<String, Vec<String>>, path: &str) -> String {
        let entries = &map[path];
        assert_eq!(entries.len(), 1, "{path} maps to {entries:?}");
        entries[0].clone()
    }

    #[test]
    fn game_root_overlay_files_land_on_their_entries() {
        let paths = [
            "hires/bgd/0a1b2c3d.webp",
            "hires/cache/ROOM1000.dat",
            "movie/OJ.mp4",
            "movie/Capcom.mp4",
            "movie/Dme_PSX.mp4",
            "jpn/movie/oj.avi",
            "jpn/PLAYERS/w03.EMW",
            "_bio1dc/hires/bgd/0a1b2c3d.webp",
            "savedata/save00.dat",
            "Biohazard.exe",
            "dgVoodoo.conf",
            "config.ini",
            "bio1hd.asi",
            "README.txt",
        ];
        let map = mapped_entries(&paths, &[]);
        assert_eq!(
            one(&map, "hires/bgd/0a1b2c3d.webp"),
            "htexture/bgd/0a1b2c3d.webp"
        );
        assert_eq!(
            one(&map, "hires/cache/ROOM1000.dat"),
            "htexture/cache/room1000.dat"
        );
        assert_eq!(one(&map, "movie/OJ.mp4"), "movie/oj.mp4");
        assert_eq!(one(&map, "movie/Capcom.mp4"), "movie/capcom.mp4");
        assert_eq!(one(&map, "movie/Dme_PSX.mp4"), "<unmapped>");
        assert_eq!(one(&map, "jpn/movie/oj.avi"), "movie/oj.avi");
        assert_eq!(one(&map, "jpn/PLAYERS/w03.EMW"), "player/w03.emw");
        for skipped in [
            "_bio1dc/hires/bgd/0a1b2c3d.webp",
            "savedata/save00.dat",
            "Biohazard.exe",
            "dgVoodoo.conf",
            "config.ini",
            "bio1hd.asi",
            "README.txt",
        ] {
            assert_eq!(one(&map, skipped), "<skip>", "{skipped}");
        }
    }

    #[test]
    fn biorand_mod_folder_files_land_on_their_entries() {
        let root = "mod_biorand/";
        let paths = [
            "mod_biorand/manifest.txt",
            "mod_biorand/description.txt",
            "mod_biorand/STAGE1/ROOM1060.RDT",
            "mod_biorand/STAGE6/ROOM6011.RDT",
            "mod_biorand/STAGE1/RC1060.pak",
            "mod_biorand/objspr/OSP00061.pak",
            "mod_biorand/enemy/EM1000.EMD",
            "mod_biorand/enemy/EM1115.EMD",
            "mod_biorand/enemy/em1021.emd",
            "mod_biorand/enemy/CHAR11.EMD",
            "mod_biorand/players/w03.emw",
            "mod_biorand/players/w10.emw",
            "mod_biorand/players/ws210.tmd",
            "mod_biorand/sound/BGM_13.wav",
            "mod_biorand/sound/BGM_24A.wav",
            "mod_biorand/data/statface.tim",
            "mod_biorand/data/title.pix",
            "mod_biorand/type.png",
            "mod_biorand/init.xml",
            "mod_biorand/bgm_tbl.xml",
            "mod_biorand/tables/room_106.xml",
            "mod_biorand/biorand.dat",
            "mod_biorand/biorand.dll",
            "mod_biorand/log_pl0.txt",
            "mod_biorand/graph_pl0.dgml",
        ];
        let map = mapped_entries(&paths, &[root]);
        assert_eq!(one(&map, "mod_biorand/manifest.txt"), "<consumed>");
        assert_eq!(one(&map, "mod_biorand/description.txt"), "<consumed>");
        assert_eq!(
            one(&map, "mod_biorand/STAGE1/ROOM1060.RDT"),
            "room/1060.rdt"
        );
        assert_eq!(
            one(&map, "mod_biorand/STAGE6/ROOM6011.RDT"),
            "room/6011.rdt"
        );
        assert_eq!(
            one(&map, "mod_biorand/STAGE1/RC1060.pak"),
            "roomcut/106_000.bmp"
        );
        assert_eq!(
            one(&map, "mod_biorand/objspr/OSP00061.pak"),
            "roommask/106_001.bmp"
        );
        assert_eq!(one(&map, "mod_biorand/enemy/EM1000.EMD"), "enemy/em000.emd");
        assert_eq!(one(&map, "mod_biorand/enemy/EM1115.EMD"), "enemy/em151.emd");
        assert_eq!(
            one(&map, "mod_biorand/enemy/em1021.emd"),
            crate::enemy::model_path(0x21).unwrap()
        );
        assert_eq!(one(&map, "mod_biorand/enemy/CHAR11.EMD"), "player/01.emd");
        assert_eq!(one(&map, "mod_biorand/players/w03.emw"), "player/w03.emw");
        assert_eq!(one(&map, "mod_biorand/players/w10.emw"), "player/01.emw");
        assert_eq!(
            one(&map, "mod_biorand/players/ws210.tmd"),
            "player/ws210.tmd"
        );
        assert_eq!(one(&map, "mod_biorand/sound/BGM_13.wav"), "bgm/013.wav");
        assert_eq!(one(&map, "mod_biorand/sound/BGM_24A.wav"), "bgm/024_00.wav");
        assert_eq!(
            one(&map, "mod_biorand/data/statface.tim"),
            "ui/statface.tim"
        );
        assert_eq!(one(&map, "mod_biorand/data/title.pix"), "ui/title.bmp");
        assert_eq!(one(&map, "mod_biorand/type.png"), "ui/type00.png");
        assert_eq!(
            one(&map, "mod_biorand/init.xml"),
            crate::lua::NEW_GAME_ENTRY
        );
        assert_eq!(one(&map, "mod_biorand/bgm_tbl.xml"), "<unsupported>");
        for skipped in [
            "mod_biorand/tables/room_106.xml",
            "mod_biorand/biorand.dat",
            "mod_biorand/biorand.dll",
            "mod_biorand/log_pl0.txt",
            "mod_biorand/graph_pl0.dgml",
        ] {
            assert_eq!(one(&map, skipped), "<skip>", "{skipped}");
        }
    }

    #[test]
    fn enemy_sound_files_also_replace_every_enemy_slot_that_plays_them() {
        let paths = ["sound/Z_ISI02.WAV", "sound/ft_wdA.wav"];
        let map = mapped_entries(&paths, &[]);
        assert_eq!(
            map["sound/Z_ISI02.WAV"],
            ["se/z_isi02.wav", "enemy/em00_02_04.wav"]
        );
        assert_eq!(map["sound/ft_wdA.wav"], ["se/ft_wda.wav"]);
    }

    #[test]
    fn voice_files_fan_out_to_every_record_that_plays_them() {
        let (_, _, name) = crate::voice_table::entries().next().unwrap();
        let path = format!("voice/{}.WAV", name.to_ascii_uppercase());
        let map = mapped_entries(&[&path], &[]);
        let expected: Vec<String> = crate::voice_table::entries()
            .filter(|(_, _, other)| other.eq_ignore_ascii_case(name))
            .map(|(row, id, _)| crate::voice::entry_path(row, id))
            .collect();
        assert_eq!(map[&path], expected);
    }

    #[test]
    fn room_text_becomes_language_keys() {
        let source = "<Strings>\r\n    <Text>{scroll 2}Somebody &amp; \"me\"\\nwith blood...</Text>\r\n    <Text />\r\n    <Text>{scroll 2}\\nThe door knob is broken.</Text>\r\n</Strings>";
        let strings = parse_room_text(source).unwrap();
        assert_eq!(strings.len(), 3);
        assert_eq!(strings[0], "{scroll 2}Somebody & \"me\"\\nwith blood...");
        assert_eq!(strings[1], "");
        let mut texts = BTreeMap::new();
        texts.insert("207".to_string(), strings);
        let mut warnings = Vec::new();
        let language = room_text_language(&texts, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let parsed = crate::language::Language::parse(&language).unwrap();
        assert!(parsed.get("ROOM_207_TEXT_00").is_some());
        assert!(parsed.get("ROOM_207_TEXT_02").is_some());
        assert!(language.contains("Somebody & \\\"me\\\"\\nwith"));
    }

    #[test]
    fn biorand_init_xml_becomes_a_start_inventory_script() {
        let source = r#"<Init>
  <Player>
    <Entry id="1" count="1" />
    <Entry id="68" count="1" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
  </Player>
  <Player>
    <Entry id="1" count="1" />
    <Entry id="6" count="240" />
    <Entry id="15" count="39" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
    <Entry id="2" count="15" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
    <Entry id="0" count="0" />
  </Player>
</Init>"#;
        let script = start_inventory_script(source).unwrap();
        assert!(script.contains("[0] = { player = { { 1, 1 }, { 6, 240 }, { 15, 39 }"));
        assert!(script.contains("partner = { { 2, 15 }, { 0, 0 }"));
        assert!(script.contains("[1] = { player = { { 1, 1 }, { 68, 1 }"));

        for character in 0..=1u8 {
            let mut game = crate::game::GameState::default();
            game.id = RoomId {
                stage: 1,
                room: 6,
                player_flag: character,
            };
            game.rebuild_slots();
            crate::lua::run_chunk(&mut game, script.as_bytes(), "init.xml", character).unwrap();
            let held: Vec<(u8, u8)> = game
                .inventory
                .iter()
                .map(|stack| (stack.id, stack.quantity))
                .collect();
            if character == 0 {
                assert_eq!(held, [(1, 1), (6, 240), (15, 39)]);
                assert_eq!(game.rebecca_inventory[0].id, 2);
                assert_eq!(game.rebecca_inventory[0].quantity, 15);
            } else {
                assert_eq!(held, [(1, 1), (68, 1)]);
            }
        }
    }

    #[test]
    fn slugs_and_versions() {
        assert_eq!(
            slug("BioRand: A Resident Evil Randomizer"),
            "biorand-a-resident-evil-randomizer"
        );
        assert_eq!(slug("RE-ENHANCE_RE1_v2.0_GOG"), "re-enhance-re1-v2-0-gog");
        assert_eq!(
            version_in("RE-ENHANCE_RE1_v2.0_GOG").as_deref(),
            Some("2.0")
        );
        assert_eq!(
            version_in("BioRand 3.3.2 (3.3.2)\nSeed: R810").as_deref(),
            Some("3.3.2")
        );
        assert_eq!(version_in("RE1 HD"), None);
    }

    #[test]
    fn late_stages_copy_shared_backgrounds_only_when_the_room_exists() {
        let mut plan = Plan::default();
        for (entry, convert) in [
            ("roomcut/106_000.bmp", Convert::Camera),
            ("roomcut/203_000.bmp", Convert::Camera),
            ("room/6060.rdt", Convert::Raw),
        ] {
            plan.targets.insert(
                entry.to_string(),
                Planned {
                    file: 0,
                    priority: 0,
                    convert,
                },
            );
        }
        plan.add_late_stage_copies(None);
        assert!(plan.targets.contains_key("roomcut/606_000.bmp"));
        assert!(!plan.targets.contains_key("roomcut/703_000.bmp"));
    }

    #[test]
    fn films_shadowed_by_an_mp4_are_dropped() {
        let mut plan = Plan::default();
        for entry in ["movie/oj.mp4", "movie/oj.avi", "movie/pj.avi"] {
            plan.targets.insert(
                entry.to_string(),
                Planned {
                    file: 0,
                    priority: 0,
                    convert: Convert::Raw,
                },
            );
        }
        plan.drop_shadowed_films();
        let kept: Vec<&str> = plan.targets.keys().map(String::as_str).collect();
        assert_eq!(kept, ["movie/oj.mp4", "movie/pj.avi"]);
    }
}
