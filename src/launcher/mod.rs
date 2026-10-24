//! The startup launcher shown when no game pack is named on the command line.
//!
//! The launcher is not a game screen: it opens before any pack exists, so it
//! carries its own embedded face and draws an 80x25 character grid centred in
//! the configured window. Every row and column is derived from that grid, so
//! selecting a row, changing a status or resizing the window never shifts a
//! table column.
//!
//! Play hands this window to the game in-process; closing the game window
//! ends the process. Mods are discovered from every data root, filtered by
//! the pack's base id, and toggled or reordered through `[mods]` in
//! `arklay.toml`. Installs and rebuilds run on a worker thread with a live
//! progress screen and cancellation.

mod text;

use std::collections::HashSet;
use std::env;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sdl3_sys::dialog::SDL_ShowOpenFolderDialog;
use sdl3_sys::events::{
    SDL_EVENT_KEY_DOWN, SDL_EVENT_MOUSE_BUTTON_DOWN, SDL_EVENT_QUIT, SDL_EVENT_WINDOW_EXPOSED,
    SDL_EVENT_WINDOW_FOCUS_GAINED, SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED, SDL_EVENT_WINDOW_RESIZED,
    SDL_Event, SDL_PollEvent, SDL_WaitEventTimeout,
};
use sdl3_sys::keycode::{SDL_KMOD_ALT, SDL_KMOD_CTRL, SDL_Keymod};
use sdl3_sys::mouse::SDL_BUTTON_LEFT;
use sdl3_sys::scancode::{
    SDL_SCANCODE_A, SDL_SCANCODE_B, SDL_SCANCODE_D, SDL_SCANCODE_DOWN, SDL_SCANCODE_EQUALS,
    SDL_SCANCODE_ESCAPE, SDL_SCANCODE_KP_ENTER, SDL_SCANCODE_LEFT, SDL_SCANCODE_MINUS,
    SDL_SCANCODE_O, SDL_SCANCODE_RETURN, SDL_SCANCODE_RIGHT, SDL_SCANCODE_S, SDL_SCANCODE_SPACE,
    SDL_SCANCODE_UP, SDL_SCANCODE_W, SDL_Scancode,
};

use crate::build_info;
use crate::config::{Config, Mods};
use crate::convert::{self, CONVERT_VERSION};
use crate::engine::{self, Display, ModOptions};
use crate::logging;
use crate::manifest;
use crate::modding;
use crate::pack;
use crate::paths;
use crate::player;
use crate::progress::ProgressSnapshot;
use crate::steam;
use crate::update::{self, UpdateInfo};
use crate::video::{self, RendererKind};

/// A blended text colour.
#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const BLACK: Self = Self::rgb(0x00, 0x00, 0x00);
    pub const BODY: Self = Self::rgb(0xD0, 0xD0, 0xD0);
    pub const DIM: Self = Self::rgb(0x78, 0x78, 0x78);
    pub const RULE: Self = Self::rgb(0x40, 0x40, 0x40);
    pub const AMBER: Self = Self::rgb(0xFF, 0xB0, 0x00);

    const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// The pixel frame the launcher draws into.
struct Frame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Frame {
    fn new(width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let mut frame = Self {
            width,
            height,
            rgba: vec![0; width as usize * height as usize * 4],
        };
        frame.clear();
        frame
    }

    fn clear(&mut self) {
        for pixel in self.rgba.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0, 0, 0, 255]);
        }
    }
}

/// The exact character grid: 80 columns, 25 rows, centred in the window.
struct Grid {
    text: text::Text,
    origin_x: i32,
    origin_y: i32,
    cell_w: i32,
    cell_h: i32,
}

impl Grid {
    fn new(width: u32, height: u32) -> Result<Self> {
        let text = text::Text::new(width, height)?;
        let cell_w = text.cell_w() as i32;
        let cell_h = text.cell_h() as i32;
        let grid_w = text::COLS as i32 * cell_w;
        let grid_h = text::ROWS as i32 * cell_h;
        Ok(Self {
            text,
            origin_x: ((width as i32 - grid_w) / 2).max(0),
            origin_y: ((height as i32 - grid_h) / 2).max(0),
            cell_w,
            cell_h,
        })
    }

    fn row_top(&self, row: usize) -> i32 {
        self.origin_y + row as i32 * self.cell_h
    }

    fn col_left(&self, col: usize) -> i32 {
        self.origin_x + col as i32 * self.cell_w
    }

    fn grid_width(&self) -> i32 {
        self.cell_w * text::COLS as i32
    }

    fn draw(&mut self, frame: &mut Frame, row: usize, col: usize, value: &str, color: Color) {
        let x = self.col_left(col);
        let baseline = self.text.baseline(self.row_top(row));
        self.text.draw(
            &mut frame.rgba,
            frame.width,
            frame.height,
            x,
            baseline,
            value,
            color,
        );
    }

    /// Draw `value` so its last character lands on column 77.
    fn draw_right(&mut self, frame: &mut Frame, row: usize, value: &str, color: Color) {
        let len = value.chars().count();
        let col = (78usize.saturating_sub(len)).max(2);
        self.draw(frame, row, col, value, color);
    }

    fn bar(&self, frame: &mut Frame, row: usize, color: Color) {
        fill_rect(
            frame,
            self.col_left(0),
            self.row_top(row),
            self.grid_width(),
            self.cell_h,
            color,
        );
    }

    fn rule(&self, frame: &mut Frame, row: usize, color: Color) {
        fill_rect(
            frame,
            self.col_left(0),
            self.row_top(row + 1) - 1,
            self.grid_width(),
            1,
            color,
        );
    }
}

/// Fill a rectangle in the RGBA frame, clipped to its bounds.
fn fill_rect(frame: &mut Frame, x: i32, y: i32, width: i32, height: i32, color: Color) {
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + width).min(frame.width as i32);
    let y1 = (y + height).min(frame.height as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for py in y0..y1 {
        for px in x0..x1 {
            let index = (py as usize * frame.width as usize + px as usize) * 4;
            frame.rgba[index] = color.r;
            frame.rgba[index + 1] = color.g;
            frame.rgba[index + 2] = color.b;
            frame.rgba[index + 3] = 255;
        }
    }
}

/// Trim `value` to at most `max` characters, with a trailing ellipsis.
fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max <= 3 {
        return ".".repeat(max);
    }
    let mut out: String = value.chars().take(max - 3).collect();
    out.push_str("...");
    out
}

/// Thousands-separated integer, for the progress counters.
fn format_count(value: u64) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (index, ch) in text.chars().enumerate() {
        if index > 0 && (text.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `mm:ss` for the progress screen.
fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

/// An ASCII progress bar, `width` cells between the brackets.
fn progress_bar(fraction: f64, width: usize) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * width as f64).round() as usize;
    let mut bar = String::with_capacity(width + 2);
    bar.push('[');
    bar.extend(std::iter::repeat_n('#', filled.min(width)));
    bar.extend(std::iter::repeat_n('-', width.saturating_sub(filled)));
    bar.push(']');
    bar
}

/// The screens the launcher can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Main,
    Play,
    Install,
    Update,
    Mods,
    Progress,
    Message,
}

const MAIN_ITEMS: [&str; 5] = ["PLAY GAME", "INSTALL / REBUILD", "UPDATE", "MODS", "EXIT"];

const UPDATE_ITEMS: [&str; 6] = [
    "CHECK FOR UPDATES",
    "UPDATE ENGINE",
    "REBUILD GAME PACK",
    "ROLL BACK LAST UPDATE",
    "CHANNEL",
    "AUTO-CHECK",
];

/// One installed pack found under a data root.
#[derive(Clone)]
struct PackRow {
    id: String,
    name: String,
    file: String,
    version: String,
    source: String,
    path: PathBuf,
    stale: bool,
    error: Option<String>,
}

/// The remembered game installation.
#[derive(Clone)]
struct Source {
    path: PathBuf,
    build: Option<String>,
}

/// One discovered mod for the selected game.
#[derive(Clone)]
struct ModRow {
    name: String,
    id: String,
    version: String,
    enabled: bool,
    load_order: i32,
    path: PathBuf,
}

/// A modal message with a way back.
struct Message {
    title: String,
    lines: Vec<String>,
    back: Screen,
}

/// The latest progress snapshot, copied out of the worker's lock.
#[derive(Clone, Default)]
struct ProgressState {
    label: String,
    item: String,
    done: u64,
    total: u64,
    phases_done: u64,
    phases_total: u64,
    overall: f64,
    elapsed: Duration,
}

impl From<ProgressSnapshot> for ProgressState {
    fn from(snapshot: ProgressSnapshot) -> Self {
        Self {
            label: snapshot.label,
            item: snapshot.item,
            done: snapshot.done,
            total: snapshot.total,
            phases_done: snapshot.phases_done,
            phases_total: snapshot.phases_total,
            overall: snapshot.overall,
            elapsed: snapshot.elapsed,
        }
    }
}

/// A background conversion and the UI-side handles to it.
struct Job {
    receiver: mpsc::Receiver<Result<(), String>>,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<ProgressState>>,
    source: PathBuf,
    build: Option<String>,
    target: PathBuf,
    rebuild: bool,
}

/// A background engine download.
struct UpdateJob {
    receiver: mpsc::Receiver<Result<(), String>>,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<ProgressState>>,
}

/// The result of one folder-dialog invocation, filled in by the SDL callback.
struct DialogState {
    done: AtomicBool,
    path: Mutex<Option<PathBuf>>,
    /// Kept alive for the dialog's lifetime in case SDL uses the pointer
    /// after the call returns.
    _default: Option<CString>,
}

unsafe extern "C" fn folder_callback(
    userdata: *mut c_void,
    filelist: *const *const c_char,
    _filter: c_int,
) {
    // SAFETY: `userdata` is the `Arc<DialogState>` kept alive in the launcher
    // until this callback marks it done.
    let state = unsafe { &*(userdata as *const DialogState) };
    let path = if filelist.is_null() {
        None
    } else {
        // SAFETY: SDL passes a NULL-terminated array of UTF-8 paths.
        let first = unsafe { *filelist };
        if first.is_null() {
            None
        } else {
            // SAFETY: the first entry is a NUL-terminated C string.
            unsafe { CStr::from_ptr(first) }
                .to_str()
                .ok()
                .map(PathBuf::from)
        }
    };
    if let Ok(mut slot) = state.path.lock() {
        *slot = path;
    }
    state.done.store(true, Ordering::Relaxed);
}

/// The launcher state.
struct Launcher {
    screen: Screen,
    main_index: usize,
    play_index: usize,
    update_index: usize,
    mods_index: usize,
    config: Config,
    packs: Vec<PackRow>,
    mods: Vec<ModRow>,
    source: Option<Source>,
    target: PathBuf,
    message: Option<Message>,
    job: Option<Job>,
    update_job: Option<UpdateJob>,
    checking: bool,
    check_rx: Option<mpsc::Receiver<Result<Option<UpdateInfo>, String>>>,
    auto_update: bool,
    confirm_cancel: bool,
    dialog: Option<Arc<DialogState>>,
    capture_progress: Option<ProgressState>,
    frame: Frame,
    grid: Grid,
    hits: Vec<(usize, usize)>,
}

impl Launcher {
    fn new(config: &Config, width: u32, height: u32) -> Result<Self> {
        let width = width.max(1);
        let height = height.max(1);
        let packs = scan_packs();
        let source = config
            .games
            .re1_source
            .as_ref()
            .map(|path| Source {
                path: path.clone(),
                build: config.games.re1_build.clone(),
            })
            .or_else(|| {
                steam::find_re1().map(|install| Source {
                    path: install.root,
                    build: install.build_id,
                })
            });
        let target = target_for(&packs);
        let mods = scan_mods(
            packs
                .iter()
                .find(|pack| pack.id.eq_ignore_ascii_case("re1")),
            config,
            &mod_dirs(),
        );
        Ok(Self {
            screen: if packs.is_empty() {
                Screen::Install
            } else {
                Screen::Main
            },
            main_index: 0,
            play_index: 0,
            update_index: 0,
            mods_index: 0,
            config: config.clone(),
            packs,
            mods,
            source,
            target,
            message: None,
            job: None,
            update_job: None,
            checking: false,
            check_rx: None,
            auto_update: false,
            confirm_cancel: false,
            dialog: None,
            capture_progress: None,
            frame: Frame::new(width, height),
            grid: Grid::new(width, height)?,
            hits: Vec::new(),
        })
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.frame = Frame::new(width, height);
        self.grid = Grid::new(width, height)?;
        self.hits.clear();
        Ok(())
    }

    fn re1_pack(&self) -> Option<&PackRow> {
        self.packs
            .iter()
            .find(|pack| pack.id.eq_ignore_ascii_case("re1"))
    }

    /// Draw the current screen from scratch.
    fn draw(&mut self) {
        self.frame.clear();
        self.hits.clear();
        self.draw_header();
        match self.screen {
            Screen::Main => self.draw_main(),
            Screen::Play => self.draw_play(),
            Screen::Install => self.draw_install(),
            Screen::Update => self.draw_update(),
            Screen::Mods => self.draw_mods(),
            Screen::Progress => self.draw_progress(),
            Screen::Message => self.draw_message(),
        }
        self.draw_footer();
    }

    fn draw_header(&mut self) {
        self.grid.draw(&mut self.frame, 0, 2, "ARKLAY", Color::BODY);
        let label = build_info::label();
        self.grid.draw_right(&mut self.frame, 0, &label, Color::DIM);
        self.grid.rule(&mut self.frame, 1, Color::RULE);
    }

    fn draw_footer(&mut self) {
        let hint = match self.screen {
            Screen::Main => "UP/DOWN SELECT   ENTER OPEN   ESC QUIT",
            Screen::Play => "ENTER PLAY   ESC BACK",
            Screen::Install => {
                if self.re1_pack().is_some() {
                    "ENTER REBUILD   B CHANGE SOURCE   ESC BACK"
                } else {
                    "ENTER INSTALL   B CHANGE SOURCE   ESC BACK"
                }
            }
            Screen::Update => "ENTER SELECT   LEFT/RIGHT CHANGE   ESC BACK",
            Screen::Mods => "SPACE TOGGLE   CTRL+UP/DOWN MOVE   O FOLDER   ESC BACK",
            Screen::Progress => {
                if self.confirm_cancel {
                    "ENTER CANCEL INSTALL   ESC KEEP GOING"
                } else if self
                    .job
                    .as_ref()
                    .is_some_and(|job| job.cancel.load(Ordering::Relaxed))
                    || self
                        .update_job
                        .as_ref()
                        .is_some_and(|job| job.cancel.load(Ordering::Relaxed))
                {
                    "CANCELLING..."
                } else {
                    "ESC CANCEL"
                }
            }
            Screen::Message => "ENTER BACK",
        };
        self.grid.draw(&mut self.frame, 24, 2, hint, Color::DIM);
    }

    fn draw_main(&mut self) {
        for (index, label) in MAIN_ITEMS.iter().enumerate() {
            let stale = self.re1_pack().is_some_and(|pack| pack.stale);
            let update = self.config.updates.available_version.is_some();
            let (status, color) = match index {
                0 => (
                    format!(
                        "{} PACK{}",
                        self.packs.len(),
                        if self.packs.len() == 1 { "" } else { "S" }
                    ),
                    Color::DIM,
                ),
                1 => (
                    if self.source.is_none() {
                        "SOURCE NEEDED".to_string()
                    } else if self.re1_pack().is_none() {
                        "REQUIRED".to_string()
                    } else {
                        String::new()
                    },
                    Color::AMBER,
                ),
                2 => (
                    match (update, stale) {
                        (true, true) => "UPDATE + REBUILD".to_string(),
                        (true, false) => "UPDATE AVAILABLE".to_string(),
                        (false, true) => "PAK REBUILD".to_string(),
                        (false, false) => String::new(),
                    },
                    Color::AMBER,
                ),
                3 => (
                    format!(
                        "{} MOD{}",
                        self.mods.len(),
                        if self.mods.len() == 1 { "" } else { "S" }
                    ),
                    Color::DIM,
                ),
                _ => (String::new(), Color::DIM),
            };
            let row = 4 + index;
            self.menu_row(row, label, &status, color, index == self.main_index);
            self.hits.push((row, index));
        }
    }

    fn draw_play(&mut self) {
        self.grid.draw(&mut self.frame, 3, 4, "PLAY", Color::BODY);
        self.table_header(
            5,
            &[(4, "GAME"), (34, "PACK"), (52, "VERSION"), (64, "SOURCE")],
        );
        if self.packs.is_empty() {
            self.grid
                .draw(&mut self.frame, 7, 4, "NO GAME PACKS FOUND.", Color::DIM);
            self.grid.draw(
                &mut self.frame,
                9,
                4,
                "USE INSTALL / REBUILD TO CREATE ONE.",
                Color::DIM,
            );
            return;
        }
        if self.play_index >= self.packs.len() {
            self.play_index = self.packs.len() - 1;
        }
        let packs = self.packs.clone();
        for (index, pack) in packs.iter().enumerate() {
            let row = 7 + index;
            let selected = index == self.play_index;
            let version = if pack.error.is_some() {
                "ERROR".to_string()
            } else if pack.stale {
                format!(
                    "{} *",
                    if pack.version.is_empty() {
                        "-"
                    } else {
                        &pack.version
                    }
                )
            } else if pack.version.is_empty() {
                "-".to_string()
            } else {
                pack.version.clone()
            };
            self.table_row(
                row,
                selected,
                &[
                    (4, &truncate(&pack.name, 28)),
                    (34, &truncate(&pack.file, 16)),
                    (52, &truncate(&version, 10)),
                    (64, &truncate(&pack.source, 12)),
                ],
            );
            self.hits.push((row, index));
        }
        if let Some(pack) = self.packs.get(self.play_index) {
            if let Some(error) = &pack.error {
                self.grid.draw(
                    &mut self.frame,
                    20,
                    4,
                    &truncate(&format!("THIS PACK COULD NOT BE READ: {error}"), 72),
                    Color::AMBER,
                );
            } else if pack.stale {
                self.grid.draw(
                    &mut self.frame,
                    20,
                    4,
                    "THIS PACK NEEDS A REBUILD BEFORE IT MATCHES THIS ENGINE.",
                    Color::AMBER,
                );
            }
        }
    }

    fn draw_install(&mut self) {
        let pack = self.re1_pack().cloned();
        let title = if pack.is_none() {
            "FIRST-TIME SETUP"
        } else {
            "INSTALL / REBUILD"
        };
        self.grid.draw(&mut self.frame, 3, 4, title, Color::BODY);
        let source = self
            .source
            .as_ref()
            .map(|source| source.path.display().to_string())
            .unwrap_or_else(|| "NOT FOUND".to_string());
        let target = self.target.display().to_string();
        let pack_value = match &pack {
            Some(pack) => format!(
                "{}  ({})",
                pack.file,
                if pack.version.is_empty() {
                    "-"
                } else {
                    &pack.version
                }
            ),
            None => "NOT INSTALLED".to_string(),
        };
        let (status, status_color) = if self.dialog.is_some() {
            ("WAITING FOR FOLDER PICKER...", Color::DIM)
        } else if self.source.is_none() {
            ("RESIDENT EVIL NOT FOUND - PRESS B", Color::AMBER)
        } else if !self
            .source
            .as_ref()
            .is_some_and(|source| source.path.is_dir())
        {
            ("SOURCE FOLDER IS MISSING - PRESS B", Color::AMBER)
        } else if pack.is_none() {
            ("READY - ABOUT 486 MB", Color::BODY)
        } else if pack.as_ref().is_some_and(|pack| pack.stale) {
            ("REBUILD REQUIRED", Color::AMBER)
        } else {
            ("UP TO DATE - REBUILD AVAILABLE", Color::BODY)
        };
        for (row, label, value, color) in [
            (5, "SOURCE", source.as_str(), Color::BODY),
            (6, "TARGET", target.as_str(), Color::BODY),
            (7, "PACK", pack_value.as_str(), Color::BODY),
            (8, "STATUS", status, status_color),
        ] {
            self.grid.draw(&mut self.frame, row, 4, label, Color::DIM);
            self.grid
                .draw(&mut self.frame, row, 16, &truncate(value, 62), color);
        }
    }

    fn draw_update(&mut self) {
        self.grid.draw(&mut self.frame, 3, 4, "UPDATE", Color::BODY);
        let pack_stale = self.re1_pack().map(|pack| pack.stale);
        let published = build_info::is_published();
        let available = self.config.updates.available_version.clone();
        for (index, label) in UPDATE_ITEMS.iter().enumerate() {
            let (status, color) = match index {
                0 => {
                    if !published {
                        ("DEV BUILD".to_string(), Color::DIM)
                    } else if self.checking {
                        ("CHECKING...".to_string(), Color::AMBER)
                    } else if available.is_some() {
                        ("UPDATE AVAILABLE".to_string(), Color::AMBER)
                    } else if self.config.updates.last_check > 0 {
                        ("UP TO DATE".to_string(), Color::DIM)
                    } else {
                        (String::new(), Color::DIM)
                    }
                }
                1 => {
                    if !published {
                        ("DEV BUILD".to_string(), Color::DIM)
                    } else if let Some(version) = &available {
                        (version.clone(), Color::AMBER)
                    } else {
                        ("UP TO DATE".to_string(), Color::DIM)
                    }
                }
                2 => match pack_stale {
                    None => ("NOT INSTALLED".to_string(), Color::DIM),
                    Some(true) => ("REQUIRED".to_string(), Color::AMBER),
                    Some(false) => ("UP TO DATE".to_string(), Color::DIM),
                },
                3 => (
                    if update::rollback_available() {
                        "AVAILABLE".to_string()
                    } else {
                        "NONE".to_string()
                    },
                    Color::BODY,
                ),
                4 => (
                    match self.config.updates.channel {
                        crate::config::UpdateChannel::Release => "RELEASE",
                        crate::config::UpdateChannel::Nightly => "NIGHTLY",
                    }
                    .to_string(),
                    Color::BODY,
                ),
                _ => (
                    if self.config.updates.check {
                        "ON".to_string()
                    } else {
                        "OFF".to_string()
                    },
                    Color::BODY,
                ),
            };
            let row = 5 + index;
            self.menu_row(row, label, &status, color, index == self.update_index);
            self.hits.push((row, index));
        }
    }

    fn draw_mods(&mut self) {
        self.grid
            .draw(&mut self.frame, 3, 4, "MODS - RESIDENT EVIL", Color::BODY);
        self.table_header(5, &[(4, "ON"), (9, "MOD"), (46, "ID"), (68, "VERSION")]);
        if self.mods.is_empty() {
            self.grid
                .draw(&mut self.frame, 7, 4, "NO MODS DISCOVERED.", Color::DIM);
            let dir = paths::mods_dir(&paths::primary_data_dir());
            self.grid.draw(
                &mut self.frame,
                9,
                4,
                &truncate(&format!("PUT MODS IN {}", dir.display()), 72),
                Color::DIM,
            );
            return;
        }
        if self.mods_index >= self.mods.len() {
            self.mods_index = self.mods.len() - 1;
        }
        let mods = self.mods.clone();
        for (index, row) in mods.iter().enumerate() {
            let line = 7 + index;
            let selected = index == self.mods_index;
            let mark = if row.enabled { "[x]" } else { "[ ]" };
            self.table_row(
                line,
                selected,
                &[
                    (4, mark),
                    (9, &truncate(&row.name, 34)),
                    (46, &truncate(&row.id, 20)),
                    (68, &truncate(&row.version, 9)),
                ],
            );
            self.hits.push((line, index));
        }
        if let Some(row) = self.mods.get(self.mods_index) {
            self.grid.draw(
                &mut self.frame,
                20,
                4,
                &truncate(&row.path.display().to_string(), 72),
                Color::DIM,
            );
        }
    }

    fn draw_progress(&mut self) {
        let state = if let Some(job) = &self.job {
            job.state
                .lock()
                .map(|state| state.clone())
                .unwrap_or_default()
        } else if let Some(job) = &self.update_job {
            job.state
                .lock()
                .map(|state| state.clone())
                .unwrap_or_default()
        } else {
            self.capture_progress.clone().unwrap_or_default()
        };
        let rebuild = self.job.as_ref().is_some_and(|job| job.rebuild);
        let title = if self.update_job.is_some() {
            "UPDATING ENGINE"
        } else if rebuild {
            "REBUILDING GAME PACK"
        } else {
            "INSTALLING RESIDENT EVIL"
        };
        self.grid.draw(&mut self.frame, 3, 4, title, Color::BODY);
        let label = if state.label.is_empty() {
            "STARTING".to_string()
        } else {
            state.label.to_ascii_uppercase()
        };
        self.grid
            .draw(&mut self.frame, 5, 4, &truncate(&label, 40), Color::BODY);
        if state.total > 0 {
            self.grid.draw_right(
                &mut self.frame,
                5,
                &format!(
                    "{} / {}",
                    format_count(state.done),
                    format_count(state.total)
                ),
                Color::DIM,
            );
        }
        let percent = (state.overall.clamp(0.0, 1.0) * 100.0).round() as u32;
        let color = if state.overall > 0.0 {
            Color::AMBER
        } else {
            Color::DIM
        };
        self.grid.draw(
            &mut self.frame,
            7,
            4,
            &progress_bar(state.overall, 44),
            color,
        );
        self.grid
            .draw_right(&mut self.frame, 7, &format!("{percent:>3}%"), Color::BODY);
        if !state.item.is_empty() {
            self.grid.draw(
                &mut self.frame,
                9,
                4,
                &truncate(&state.item, 70),
                Color::DIM,
            );
        }
        self.grid.draw(
            &mut self.frame,
            11,
            4,
            &format!("ELAPSED {}", format_elapsed(state.elapsed)),
            Color::DIM,
        );
        if state.phases_total > 0 {
            self.grid.draw(
                &mut self.frame,
                12,
                4,
                &format!("STEP {} / {}", state.phases_done, state.phases_total),
                Color::DIM,
            );
        }
        if self.confirm_cancel {
            self.grid
                .draw(&mut self.frame, 14, 4, "CANCEL THE INSTALL?", Color::AMBER);
            self.grid.draw(
                &mut self.frame,
                15,
                4,
                "NO PACK WILL BE WRITTEN.",
                Color::DIM,
            );
        }
    }

    fn draw_message(&mut self) {
        let Some(message) = &self.message else {
            self.grid
                .draw(&mut self.frame, 3, 4, "MESSAGE", Color::BODY);
            return;
        };
        self.grid
            .draw(&mut self.frame, 3, 4, &message.title, Color::BODY);
        for (index, line) in message.lines.iter().enumerate() {
            self.grid.draw(
                &mut self.frame,
                5 + index,
                4,
                &truncate(line, 72),
                Color::BODY,
            );
        }
    }

    fn table_header(&mut self, row: usize, columns: &[(usize, &str)]) {
        for (col, label) in columns {
            self.grid
                .draw(&mut self.frame, row, *col, label, Color::DIM);
        }
        self.grid.rule(&mut self.frame, row, Color::RULE);
    }

    fn menu_row(
        &mut self,
        row: usize,
        label: &str,
        status: &str,
        status_color: Color,
        selected: bool,
    ) {
        if selected {
            self.grid.bar(&mut self.frame, row, Color::AMBER);
        }
        let color = if selected { Color::BLACK } else { Color::BODY };
        let marker = if selected { ">" } else { " " };
        self.grid.draw(&mut self.frame, row, 2, marker, color);
        self.grid
            .draw(&mut self.frame, row, 4, &truncate(label, 42), color);
        if !status.is_empty() {
            let color = if selected { Color::BLACK } else { status_color };
            self.grid
                .draw_right(&mut self.frame, row, &truncate(status, 30), color);
        }
    }

    fn table_row(&mut self, row: usize, selected: bool, cells: &[(usize, &str)]) {
        if selected {
            self.grid.bar(&mut self.frame, row, Color::AMBER);
        }
        let color = if selected { Color::BLACK } else { Color::BODY };
        let marker = if selected { ">" } else { " " };
        self.grid.draw(&mut self.frame, row, 2, marker, color);
        for (col, value) in cells {
            self.grid.draw(&mut self.frame, row, *col, value, color);
        }
    }

    /// Handle one key press. Returns true when the launcher should close.
    fn key(
        &mut self,
        scancode: SDL_Scancode,
        modifiers: SDL_Keymod,
        display: &mut Display,
    ) -> bool {
        if self.screen == Screen::Progress {
            if scancode == SDL_SCANCODE_ESCAPE {
                self.confirm_cancel = !self.confirm_cancel;
            } else if matches!(scancode, SDL_SCANCODE_RETURN | SDL_SCANCODE_KP_ENTER)
                && self.confirm_cancel
            {
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                if let Some(job) = &self.update_job {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                self.confirm_cancel = false;
            }
            return false;
        }
        match scancode {
            SDL_SCANCODE_ESCAPE => {
                if self.screen == Screen::Main {
                    return true;
                }
                if self.screen == Screen::Message {
                    self.dismiss_message();
                } else {
                    self.screen = Screen::Main;
                }
            }
            SDL_SCANCODE_RETURN | SDL_SCANCODE_KP_ENTER => return self.activate(display),
            SDL_SCANCODE_SPACE => {
                if self.screen == Screen::Mods {
                    self.toggle_mod();
                } else if self.screen == Screen::Update && self.update_index >= 4 {
                    self.toggle_update();
                } else {
                    return self.activate(display);
                }
            }
            SDL_SCANCODE_UP | SDL_SCANCODE_W => {
                if self.screen == Screen::Mods && modifiers & SDL_KMOD_CTRL != 0 {
                    self.move_mod(-1);
                } else {
                    self.step_selection(-1);
                }
            }
            SDL_SCANCODE_DOWN | SDL_SCANCODE_S => {
                if self.screen == Screen::Mods && modifiers & SDL_KMOD_CTRL != 0 {
                    self.move_mod(1);
                } else {
                    self.step_selection(1);
                }
            }
            SDL_SCANCODE_LEFT | SDL_SCANCODE_A => {
                if self.screen == Screen::Update {
                    self.toggle_update();
                }
            }
            SDL_SCANCODE_RIGHT | SDL_SCANCODE_D => {
                if self.screen == Screen::Update {
                    self.toggle_update();
                }
            }
            SDL_SCANCODE_EQUALS => {
                if self.screen == Screen::Mods {
                    self.move_mod(-1);
                }
            }
            SDL_SCANCODE_MINUS => {
                if self.screen == Screen::Mods {
                    self.move_mod(1);
                }
            }
            SDL_SCANCODE_B => {
                if self.screen == Screen::Install {
                    self.browse(display);
                }
            }
            SDL_SCANCODE_O => {
                if self.screen == Screen::Mods {
                    let dir = match self.mods.get(self.mods_index) {
                        Some(row) if row.path.is_dir() => row.path.clone(),
                        Some(row) => row
                            .path
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| row.path.clone()),
                        None => paths::mods_dir(&paths::primary_data_dir()),
                    };
                    open_folder(&dir);
                }
            }
            _ => {}
        }
        false
    }

    fn step_selection(&mut self, delta: isize) {
        match self.screen {
            Screen::Main => step(&mut self.main_index, MAIN_ITEMS.len(), delta),
            Screen::Play => step(&mut self.play_index, self.packs.len(), delta),
            Screen::Update => step(&mut self.update_index, UPDATE_ITEMS.len(), delta),
            Screen::Mods => step(&mut self.mods_index, self.mods.len(), delta),
            Screen::Install | Screen::Progress | Screen::Message => {}
        }
    }

    /// Enter/Space on the current screen. Returns true to close the launcher.
    fn activate(&mut self, display: &mut Display) -> bool {
        match self.screen {
            Screen::Main => return self.activate_main(),
            Screen::Play => return self.play(display),
            Screen::Install => self.install(),
            Screen::Update => self.activate_update(),
            Screen::Mods => self.toggle_mod(),
            Screen::Progress => {}
            Screen::Message => self.dismiss_message(),
        }
        false
    }

    fn activate_main(&mut self) -> bool {
        match self.main_index {
            0 => {
                self.screen = Screen::Play;
                self.play_index = 0;
            }
            1 => self.screen = Screen::Install,
            2 => {
                self.screen = Screen::Update;
                self.update_index = 0;
            }
            3 => {
                self.refresh_mods();
                self.screen = Screen::Mods;
                self.mods_index = 0;
            }
            4 => return true,
            _ => {}
        }
        false
    }

    fn activate_update(&mut self) {
        match self.update_index {
            0 => self.start_check(true),
            1 => self.start_update(),
            2 => self.install(),
            3 => self.rollback(),
            4 => self.toggle_channel(),
            5 => self.toggle_auto_check(),
            _ => {}
        }
    }

    fn toggle_update(&mut self) {
        if self.update_index == 4 {
            self.toggle_channel();
        } else if self.update_index == 5 {
            self.toggle_auto_check();
        }
    }

    /// Check on startup when enabled and the six-hour cache has expired.
    fn maybe_start_check(&mut self) {
        if !build_info::is_published()
            || !self.config.updates.check
            || self.checking
            || self.config.updates.last_check.saturating_add(6 * 60 * 60) > unix_now()
        {
            return;
        }
        self.start_check(false);
    }

    fn start_check(&mut self, manual: bool) {
        if self.checking {
            return;
        }
        if !build_info::is_published() {
            if manual {
                self.show_message(
                    "CHECK FOR UPDATES",
                    &["DEVELOPMENT BUILDS DO NOT UPDATE THEMSELVES."],
                    Screen::Update,
                );
            }
            return;
        }
        let channel = self.config.updates.channel;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(update::check(channel).map_err(|err| format!("{err:#}")));
        });
        self.check_rx = Some(receiver);
        self.checking = true;
    }

    fn poll_check(&mut self) -> bool {
        let Some(receiver) = &self.check_rx else {
            return false;
        };
        let received = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("the update check stopped".to_string()),
        };
        self.check_rx = None;
        self.checking = false;
        self.config.updates.last_check = unix_now();
        match received {
            Ok(Some(info)) => {
                self.config.updates.available_channel =
                    crate::config::UpdateChannel::parse(&info.channel);
                self.config.updates.available_version = Some(info.version);
                self.config.updates.available_sha = Some(info.sha);
            }
            Ok(None) => {
                self.config.updates.available_channel = None;
                self.config.updates.available_version = None;
                self.config.updates.available_sha = None;
            }
            Err(err) => {
                logging::write(&format!("update check failed: {err}"));
            }
        }
        self.save_updates();
        true
    }

    fn clear_available_if_current(&mut self) {
        let updates = &self.config.updates;
        let Some(sha) = updates.available_sha.as_deref() else {
            return;
        };
        let current = match updates.available_channel {
            Some(crate::config::UpdateChannel::Nightly) => {
                !sha.is_empty() && sha == build_info::COMMIT
            }
            _ => updates.available_version.as_deref() == Some(build_info::VERSION),
        };
        if current {
            self.config.updates.available_channel = None;
            self.config.updates.available_version = None;
            self.config.updates.available_sha = None;
            self.save_updates();
        }
    }

    fn save_updates(&self) {
        if let Err(err) = Config::save_updates(&Config::default_path(), &self.config.updates) {
            logging::write(&format!("warning: update settings not saved: {err:#}"));
        }
    }

    fn toggle_channel(&mut self) {
        self.config.updates.channel = match self.config.updates.channel {
            crate::config::UpdateChannel::Release => crate::config::UpdateChannel::Nightly,
            crate::config::UpdateChannel::Nightly => crate::config::UpdateChannel::Release,
        };
        self.config.updates.available_channel = None;
        self.config.updates.available_version = None;
        self.config.updates.available_sha = None;
        self.save_updates();
        self.start_check(true);
    }

    fn toggle_auto_check(&mut self) {
        self.config.updates.check = !self.config.updates.check;
        self.save_updates();
    }

    fn start_update(&mut self) {
        if !build_info::is_published() {
            self.show_message(
                "UPDATE ENGINE",
                &["DEVELOPMENT BUILDS DO NOT UPDATE THEMSELVES."],
                Screen::Update,
            );
            return;
        }
        let (Some(version), Some(sha)) = (
            self.config.updates.available_version.clone(),
            self.config.updates.available_sha.clone(),
        ) else {
            self.show_message(
                "UPDATE ENGINE",
                &["THERE IS NO NEWER PUBLISHED BUILD."],
                Screen::Update,
            );
            return;
        };
        let channel = self.config.updates.channel;
        // The feed is re-fetched so the download URL and checksum come from
        // the same JSON as the offered version.
        let (sender, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProgressState {
            label: "download".to_string(),
            item: "arklay".to_string(),
            ..ProgressState::default()
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                let Some(info) = update::check(channel)? else {
                    bail!("there is no newer published build");
                };
                if info.version != version || info.sha != sha {
                    bail!("the update feed changed; check for updates again");
                }
                let exe = update::current_exe()?;
                let dest = update::staging_path(&exe);
                let start = std::time::Instant::now();
                update::download(&info, &dest, &thread_cancel, |done, total| {
                    if let Ok(mut state) = thread_state.lock() {
                        state.done = done;
                        state.total = total;
                        state.overall = if total > 0 {
                            done.min(total) as f64 / total as f64
                        } else {
                            0.0
                        };
                        state.elapsed = start.elapsed();
                    }
                })
            })();
            let _ = sender.send(result.map_err(|err| format!("{err:#}")));
        });
        self.update_job = Some(UpdateJob {
            receiver,
            cancel,
            state,
        });
        self.job = None;
        self.screen = Screen::Progress;
        self.confirm_cancel = false;
    }

    fn poll_update_job(&mut self) -> bool {
        let Some(job) = &self.update_job else {
            return false;
        };
        let result = match job.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("the update download stopped".to_string()),
        };
        let job = self.update_job.take().expect("an update job");
        if job.cancel.load(Ordering::Relaxed) {
            logging::write("update cancelled");
            self.show_message(
                "UPDATE CANCELLED",
                &["THE ENGINE WAS NOT CHANGED."],
                Screen::Update,
            );
            return true;
        }
        match result {
            Ok(()) => {
                logging::write("update: installing the downloaded engine");
                if let Err(err) = update::install_staged() {
                    self.show_message(
                        "UPDATE FAILED",
                        &[&truncate(&format!("{err:#}"), 72)],
                        Screen::Update,
                    );
                    return true;
                }
                if let Err(err) = update::restart() {
                    self.show_message(
                        "RESTART FAILED",
                        &[
                            &truncate(&format!("{err:#}"), 72),
                            "RESTART ARKLAY MANUALLY.",
                        ],
                        Screen::Update,
                    );
                    return true;
                }
                std::process::exit(0);
            }
            Err(err) => {
                logging::write(&format!("update failed: {err}"));
                self.show_message(
                    "UPDATE FAILED",
                    &[&truncate(&err, 72), "SEE THE LOG FOR DETAILS."],
                    Screen::Update,
                );
            }
        }
        true
    }

    fn rollback(&mut self) {
        if !update::rollback_available() {
            self.show_message(
                "ROLL BACK LAST UPDATE",
                &["THERE IS NO PREVIOUS BUILD TO RESTORE."],
                Screen::Update,
            );
            return;
        }
        match update::rollback() {
            Ok(()) => {
                logging::write("update: rolling back");
                if let Err(err) = update::restart() {
                    self.show_message(
                        "RESTART FAILED",
                        &[
                            &truncate(&format!("{err:#}"), 72),
                            "RESTART ARKLAY MANUALLY.",
                        ],
                        Screen::Update,
                    );
                    return;
                }
                std::process::exit(0);
            }
            Err(err) => self.show_message(
                "ROLL BACK FAILED",
                &[&truncate(&format!("{err:#}"), 72)],
                Screen::Update,
            ),
        }
    }

    fn refresh_mods(&mut self) {
        let pack = self.re1_pack().cloned();
        self.mods = scan_mods(pack.as_ref(), &self.config, &mod_dirs());
        if self.mods_index >= self.mods.len() {
            self.mods_index = self.mods.len().saturating_sub(1);
        }
    }

    fn toggle_mod(&mut self) {
        let Some((id, enabled)) = self
            .mods
            .get(self.mods_index)
            .map(|row| (row.id.clone(), !row.enabled))
        else {
            return;
        };
        if let Some(row) = self.mods.get_mut(self.mods_index) {
            row.enabled = enabled;
        }
        set_mod_enabled(&mut self.config.mods, &id, enabled);
        self.save_mods();
    }

    fn move_mod(&mut self, delta: isize) {
        let len = self.mods.len();
        if len < 2 {
            return;
        }
        let target = (self.mods_index as isize + delta).rem_euclid(len as isize) as usize;
        self.mods.swap(self.mods_index, target);
        self.mods_index = target;
        self.config.mods.order = self.mods.iter().map(|row| row.id.clone()).collect();
        self.save_mods();
    }

    fn save_mods(&self) {
        if let Err(err) = Config::save_mods(&Config::default_path(), &self.config.mods) {
            logging::write(&format!("warning: mod selection not saved: {err:#}"));
        }
    }

    fn show_message(&mut self, title: &str, lines: &[&str], back: Screen) {
        self.message = Some(Message {
            title: title.to_string(),
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
            back,
        });
        self.screen = Screen::Message;
    }

    fn dismiss_message(&mut self) {
        if let Some(message) = self.message.take() {
            self.screen = message.back;
        } else {
            self.screen = Screen::Main;
        }
    }

    /// Start (or restart) the conversion for the current source and target.
    fn install(&mut self) {
        if self.job.is_some() {
            return;
        }
        let Some(source) = self.source.clone() else {
            self.show_message(
                "RESIDENT EVIL NOT FOUND",
                &["Use B to choose the folder that holds the game."],
                self.screen,
            );
            return;
        };
        let path = match resolve_source(&source.path) {
            Ok(path) => path,
            Err(err) => {
                self.show_message(
                    "NOT RESIDENT EVIL",
                    &[&format!("{err:#}")],
                    if self.screen == Screen::Update {
                        Screen::Update
                    } else {
                        Screen::Install
                    },
                );
                return;
            }
        };
        let pack = self.re1_pack().cloned();
        let target = pack
            .as_ref()
            .map(|pack| pack.path.clone())
            .unwrap_or_else(default_target);
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let state = Arc::new(Mutex::new(ProgressState::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let thread_state = Arc::clone(&state);
        let thread_cancel = Arc::clone(&cancel);
        let thread_source = path.clone();
        let thread_target = target.clone();
        std::thread::spawn(move || {
            let observer = Box::new(move |snapshot: ProgressSnapshot| {
                if let Ok(mut state) = thread_state.lock() {
                    *state = ProgressState::from(snapshot);
                }
            });
            let result = convert::convert_game_observed(
                &thread_source,
                &thread_target,
                0,
                false,
                Some(observer),
                Some(thread_cancel),
            );
            let _ = sender.send(result.map_err(|err| format!("{err:#}")));
        });
        logging::write(&format!(
            "install: {} -> {}",
            path.display(),
            target.display()
        ));
        self.source = Some(Source {
            path,
            build: source.build,
        });
        self.target = target.clone();
        self.job = Some(Job {
            receiver,
            cancel,
            state,
            source: self
                .source
                .as_ref()
                .map(|source| source.path.clone())
                .unwrap_or_default(),
            build: self.source.as_ref().and_then(|source| source.build.clone()),
            target,
            rebuild: pack.is_some(),
        });
        self.screen = Screen::Progress;
        self.confirm_cancel = false;
        self.message = None;
    }

    /// Hand the launcher's window to the game. Returning true means the
    /// process should now exit, which is what closing the game window does.
    fn play(&mut self, display: &mut Display) -> bool {
        let Some(pack) = self.packs.get(self.play_index).cloned() else {
            return false;
        };
        if let Some(error) = &pack.error {
            self.show_message("PACK ERROR", &[error], Screen::Play);
            return false;
        }
        display.note_window_size();
        let mut graphics = self.config.graphics;
        if let Some(current) = display.graphics().cloned() {
            graphics.fullscreen = current.fullscreen;
            graphics.display = current.display;
            graphics.window_width = current.window_width;
            graphics.window_height = current.window_height;
        }
        video::set_aspect(graphics.aspect);
        if let Err(err) = display.set_renderer(graphics) {
            self.show_message("GAME FAILED", &[&format!("{err:#}")], Screen::Play);
            return false;
        }
        let save_dir = self
            .config
            .save_dir
            .clone()
            .unwrap_or_else(Config::default_save_dir);
        let mods = ModOptions {
            explicit: Vec::new(),
            no_mods: false,
            layers: crate::pack::LayerOptions {
                enabled: self.config.mods.enabled.clone(),
                disabled: self.config.mods.disabled.clone(),
                order: self.config.mods.order.clone(),
            },
            global_dirs: mod_dirs(),
        };
        let player_options = player::Options {
            quick_turn: self.config.game.quick_turn,
            tactical_reload: self.config.game.tactical_reload,
        };
        logging::write(&format!("play: {}", pack.path.display()));
        let result = engine::run_root_with_display(
            &pack.path,
            None,
            &save_dir,
            &mods,
            false,
            self.config.language.as_deref(),
            graphics,
            player_options,
            display,
        );
        match result {
            Ok(()) => true,
            Err(err) => {
                logging::write(&format!("game failed: {err:#}"));
                let mut software = graphics;
                software.renderer = RendererKind::Software;
                let _ = display.set_renderer(software);
                self.show_message("GAME FAILED", &[&format!("{err:#}")], Screen::Play);
                false
            }
        }
    }

    /// Open the native folder picker for a manually installed copy.
    fn browse(&mut self, display: &Display) {
        if self.dialog.is_some() {
            return;
        }
        let default = self
            .source
            .as_ref()
            .and_then(|source| CString::new(source.path.to_string_lossy().as_bytes()).ok());
        let state = Arc::new(DialogState {
            done: AtomicBool::new(false),
            path: Mutex::new(None),
            _default: default,
        });
        let userdata = Arc::as_ptr(&state).cast_mut().cast::<c_void>();
        unsafe {
            SDL_ShowOpenFolderDialog(
                Some(folder_callback),
                userdata,
                display.window_ptr(),
                state
                    ._default
                    .as_ref()
                    .map_or(std::ptr::null(), |path| path.as_ptr()),
                false,
            );
        }
        self.dialog = Some(state);
        logging::write("folder picker opened");
    }

    /// Finish a folder-dialog invocation and adopt a validated choice.
    fn poll_dialog(&mut self) -> bool {
        let Some(dialog) = &self.dialog else {
            return false;
        };
        if !dialog.done.load(Ordering::Relaxed) {
            return false;
        }
        let path = dialog.path.lock().ok().and_then(|mut path| path.take());
        self.dialog = None;
        let Some(path) = path else {
            return true;
        };
        match resolve_source(&path) {
            Ok(path) => {
                logging::write(&format!("source: {}", path.display()));
                self.source = Some(Source { path, build: None });
            }
            Err(err) => {
                logging::write(&format!("source rejected: {err:#}"));
                self.show_message("NOT RESIDENT EVIL", &[&format!("{err:#}")], Screen::Install);
            }
        }
        true
    }

    /// Collect a finished conversion.
    fn poll_job(&mut self) -> bool {
        let Some(job) = &self.job else {
            return false;
        };
        let received = match job.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                Some(Err("the conversion thread stopped unexpectedly".to_string()))
            }
        };
        let Some(result) = received else {
            return false;
        };
        let job = self.job.take().expect("a running job");
        let back = if job.rebuild {
            Screen::Update
        } else {
            Screen::Install
        };
        let cancelled = job.cancel.load(Ordering::Relaxed);
        match result {
            Ok(()) => {
                if let Err(err) = Config::save_games(
                    &Config::default_path(),
                    Some(&job.source),
                    job.build.as_deref(),
                ) {
                    logging::write(&format!("warning: game source not saved: {err:#}"));
                }
                self.source = Some(Source {
                    path: job.source.clone(),
                    build: job.build.clone(),
                });
                self.packs = scan_packs();
                self.target = target_for(&self.packs);
                self.refresh_mods();
                self.screen = Screen::Main;
                self.main_index = 0;
                let title = if job.rebuild {
                    "PACK REBUILT"
                } else {
                    "PACK READY"
                };
                self.show_message(
                    title,
                    &[
                        &job.target.display().to_string(),
                        "THE PLAY SCREEN LISTS IT.",
                    ],
                    Screen::Main,
                );
            }
            Err(_) if cancelled => {
                logging::write("install cancelled");
                self.show_message("INSTALL CANCELLED", &["NO PACK WAS WRITTEN."], back);
            }
            Err(err) => {
                logging::write(&format!("install failed: {err}"));
                self.show_message(
                    "INSTALL FAILED",
                    &[&truncate(&err, 72), "SEE THE LOG FOR DETAILS."],
                    back,
                );
            }
        }
        self.confirm_cancel = false;
        true
    }

    /// Turn a mouse click into the row it landed on and activate it.
    fn click(&mut self, x: i32, y: i32, display: &mut Display) {
        let x0 = self.grid.col_left(0);
        let x1 = x0 + self.grid.grid_width();
        if x < x0 || x >= x1 {
            return;
        }
        let hit = self.hits.iter().find(|(row, _)| {
            let top = self.grid.row_top(*row);
            y >= top && y < top + self.grid.cell_h
        });
        let Some((_, index)) = hit.copied() else {
            return;
        };
        match self.screen {
            Screen::Main => self.main_index = index,
            Screen::Play => self.play_index = index,
            Screen::Update => self.update_index = index,
            Screen::Mods => self.mods_index = index,
            Screen::Install | Screen::Progress | Screen::Message => return,
        }
        self.activate(display);
    }

    /// Pick a screen for a headless preview capture.
    fn set_capture_screen(&mut self, name: &str) {
        self.screen = match name {
            "play" => Screen::Play,
            "install" => Screen::Install,
            "update" => Screen::Update,
            "mods" => Screen::Mods,
            "progress" => {
                self.capture_progress = Some(ProgressState {
                    label: "roomcut".to_string(),
                    item: "roomcut/600_123.bmp".to_string(),
                    done: 1204,
                    total: 2058,
                    phases_done: 6,
                    phases_total: 15,
                    overall: 0.58,
                    elapsed: Duration::from_secs(42),
                });
                Screen::Progress
            }
            "message" => {
                self.show_message("PREVIEW", &["A sample message screen."], Screen::Main);
                Screen::Message
            }
            _ => Screen::Main,
        };
    }
}

impl Launcher {
    fn run_loop(&mut self, display: &mut Display) -> Result<()> {
        let mut dirty = true;
        let mut event = SDL_Event::default();
        loop {
            let dialog_changed = self.poll_dialog();
            let job_changed = self.poll_job();
            let update_changed = self.poll_update_job();
            let check_changed = self.poll_check();
            if self.auto_update
                && !self.checking
                && self.update_job.is_none()
                && self.job.is_none()
                && self.config.updates.available_version.is_some()
            {
                self.auto_update = false;
                self.start_update();
            }
            if dialog_changed
                || job_changed
                || update_changed
                || check_changed
                || self.job.is_some()
                || self.update_job.is_some()
                || self.checking
            {
                dirty = true;
            }
            if dirty {
                let (width, height) = display.drawable_size();
                if width > 0
                    && height > 0
                    && (width != self.frame.width || height != self.frame.height)
                {
                    self.resize(width, height)?;
                }
                self.draw();
                display.present_ui(self.frame.width, self.frame.height, &self.frame.rgba)?;
                display.show()?;
                dirty = false;
            }
            if !unsafe { SDL_WaitEventTimeout(&mut event, 100) } {
                continue;
            }
            loop {
                let kind = unsafe { event.r#type };
                if kind == SDL_EVENT_QUIT {
                    // A conversion's temporary pack is only removed by the
                    // worker; give it a moment to see the cancel flag before
                    // the process exits.
                    if let Some(job) = &self.job {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                    if let Some(job) = &self.update_job {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                    let deadline = std::time::Instant::now() + Duration::from_secs(3);
                    while self.job.is_some() && std::time::Instant::now() < deadline {
                        self.poll_job();
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    self.save_window(display);
                    return Ok(());
                } else if kind == SDL_EVENT_KEY_DOWN {
                    let scancode = unsafe { event.key.scancode };
                    let modifiers = unsafe { event.key.r#mod };
                    let repeat = unsafe { event.key.repeat };
                    if !repeat {
                        if modifiers & SDL_KMOD_ALT != 0
                            && matches!(scancode, SDL_SCANCODE_RETURN | SDL_SCANCODE_KP_ENTER)
                        {
                            display.toggle_fullscreen();
                        } else if self.key(scancode, modifiers, display) {
                            self.save_window(display);
                            return Ok(());
                        }
                        dirty = true;
                    }
                } else if kind == SDL_EVENT_MOUSE_BUTTON_DOWN {
                    if unsafe { event.button.button } == SDL_BUTTON_LEFT as u8 {
                        // Mouse events are in window coordinates; the frame
                        // is in drawable pixels (they differ under a display
                        // scale above 100%).
                        let (window_w, window_h) = display.window_size();
                        let (pixel_w, pixel_h) = display.drawable_size();
                        let scale_x = pixel_w as f32 / window_w.max(1) as f32;
                        let scale_y = pixel_h as f32 / window_h.max(1) as f32;
                        let x = (unsafe { event.button.x } * scale_x) as i32;
                        let y = (unsafe { event.button.y } * scale_y) as i32;
                        self.click(x, y, display);
                        dirty = true;
                    }
                } else if kind == SDL_EVENT_WINDOW_RESIZED
                    || kind == SDL_EVENT_WINDOW_PIXEL_SIZE_CHANGED
                    || kind == SDL_EVENT_WINDOW_EXPOSED
                    || kind == SDL_EVENT_WINDOW_FOCUS_GAINED
                {
                    dirty = true;
                }
                if !unsafe { SDL_PollEvent(&mut event) } {
                    break;
                }
            }
        }
    }

    fn save_window(&self, display: &mut Display) {
        display.note_window_size();
        display.save_window();
    }
}

/// Find every pack under the data roots, first root winning a duplicate id.
fn scan_packs() -> Vec<PackRow> {
    let roots = paths::data_roots();
    let custom = env::var_os(paths::DATA_ENV).is_some();
    let source_hash = convert::packed_source_hash();
    let mut seen: HashSet<String> = HashSet::new();
    let mut packs = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let dir = paths::games_dir(root);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("akpak"))
            })
            .collect();
        files.sort();
        for path in files {
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let read = pack::read_manifest(&path);
            let (manifest, error) = match read {
                Ok(manifest) => (manifest, None),
                Err(err) => (None, Some(format!("{err:#}"))),
            };
            let id = manifest
                .as_ref()
                .map(|manifest| manifest.id.clone())
                .unwrap_or_else(|| stem.clone());
            if !seen.insert(id.to_ascii_lowercase()) {
                continue;
            }
            let name = manifest
                .as_ref()
                .and_then(|manifest| manifest.name.clone())
                .unwrap_or_else(|| stem.clone());
            let version = manifest
                .as_ref()
                .and_then(|manifest| manifest.version.clone())
                .unwrap_or_default();
            let stale = manifest.as_ref().is_none_or(|manifest| {
                manifest.convert_version != Some(CONVERT_VERSION)
                    || manifest.source_hash.as_deref() != Some(source_hash.as_str())
            });
            packs.push(PackRow {
                id,
                name,
                file: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                version,
                source: root_label(index, custom),
                path,
                stale,
                error,
            });
        }
    }
    packs.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.file.cmp(&b.file))
    });
    packs
}

/// The extra mod directories scanned for every game, before the pack's own
/// sibling `mods/` directory.
fn mod_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = paths::data_roots()
        .iter()
        .map(|root| paths::mods_dir(root))
        .collect();
    let legacy = Config::default_mods_dir();
    if !dirs.iter().any(|dir| dir == &legacy) {
        dirs.push(legacy);
    }
    dirs
}

/// Discover the mods for `pack`, filtered to its base id and ordered by the
/// config's explicit order then each manifest's load order.
fn scan_mods(pack: Option<&PackRow>, config: &Config, dirs: &[PathBuf]) -> Vec<ModRow> {
    let Some(pack) = pack else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for path in modding::discover_mods(&pack.path, dirs) {
        let manifest = match read_mod_manifest(&path) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => continue,
            Err(err) => {
                logging::write(&format!("warning: mod {}: {err:#}", path.display()));
                continue;
            }
        };
        if manifest.kind != manifest::PackKind::Mod {
            continue;
        }
        if manifest.base.as_deref() != Some(pack.id.as_str()) {
            continue;
        }
        if !seen.insert(manifest.id.to_ascii_lowercase()) {
            continue;
        }
        rows.push(ModRow {
            enabled: mod_enabled(&config.mods, &manifest.id),
            name: manifest.name.unwrap_or_else(|| manifest.id.clone()),
            version: manifest.version.unwrap_or_default(),
            load_order: manifest.load_order,
            id: manifest.id,
            path,
        });
    }
    rows.sort_by(|a, b| {
        let rank = |id: &str| {
            config
                .mods
                .order
                .iter()
                .position(|ordered| ordered == id)
                .unwrap_or(usize::MAX)
        };
        rank(&a.id)
            .cmp(&rank(&b.id))
            .then_with(|| a.load_order.cmp(&b.load_order))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

/// Read a mod image's manifest, or a source directory's `manifest.toml`.
fn read_mod_manifest(path: &Path) -> Result<Option<manifest::Manifest>> {
    if path.is_dir() {
        let file = path.join(manifest::ENTRY);
        let text = fs::read_to_string(&file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        return manifest::Manifest::parse(&text)
            .map(Some)
            .with_context(|| format!("invalid {}", file.display()));
    }
    pack::read_manifest(path)
}

fn mod_enabled(mods: &Mods, id: &str) -> bool {
    if mods.disabled.iter().any(|disabled| disabled == id) {
        return false;
    }
    mods.enabled
        .as_ref()
        .is_none_or(|enabled| enabled.iter().any(|listed| listed == id))
}

fn set_mod_enabled(mods: &mut Mods, id: &str, enabled: bool) {
    mods.disabled.retain(|listed| listed != id);
    if let Some(list) = &mut mods.enabled {
        list.retain(|listed| listed != id);
    }
    if enabled {
        if let Some(list) = &mut mods.enabled {
            list.push(id.to_string());
        }
    } else {
        mods.disabled.push(id.to_string());
    }
}

/// Ask the desktop to open `path`.
fn open_folder(path: &Path) {
    let _ = fs::create_dir_all(path);
    let command = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(err) = std::process::Command::new(command).arg(path).spawn() {
        logging::write(&format!(
            "warning: could not open {}: {err}",
            path.display()
        ));
    }
}

/// A short label for a data root in the pack table.
fn root_label(index: usize, custom: bool) -> String {
    if custom {
        return format!("DATA {}", index + 1);
    }
    match index {
        0 => "LOCAL".to_string(),
        1 => "ROAMING".to_string(),
        2 => "INSTALL".to_string(),
        other => format!("PATH {}", other + 1),
    }
}

/// The pack file a first install writes.
fn default_target() -> PathBuf {
    paths::games_dir(&paths::primary_data_dir()).join("re1.akpak")
}

/// The resolved Resident Evil pack: the first `re1`-id pack in precedence
/// order, else the default target.
fn target_for(packs: &[PackRow]) -> PathBuf {
    packs
        .iter()
        .find(|pack| pack.id.eq_ignore_ascii_case("re1"))
        .map(|pack| pack.path.clone())
        .unwrap_or_else(default_target)
}

/// Validate a chosen folder, accepting a Steam library or `common` folder as
/// well as the game root itself.
fn resolve_source(path: &Path) -> Result<PathBuf> {
    let candidates = [
        path.to_path_buf(),
        path.join(steam::INSTALL_DIR),
        path.join("common").join(steam::INSTALL_DIR),
        path.join("steamapps")
            .join("common")
            .join(steam::INSTALL_DIR),
    ];
    let mut last = None;
    for candidate in candidates {
        match convert::validate_install(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("not a Resident Evil installation")))
}

/// A preview size from `ARKLAY_LAUNCHER_CAPTURE_SIZE=WxH`.
fn capture_size() -> Option<(u32, u32)> {
    let value = env::var("ARKLAY_LAUNCHER_CAPTURE_SIZE").ok()?;
    let (width, height) = value.split_once(['x', 'X', ','])?;
    let width = width.trim().parse().ok()?;
    let height = height.trim().parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

/// Unix seconds, for the update-check cache.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Move a selection index by `delta`, wrapping.
fn step(index: &mut usize, len: usize, delta: isize) {
    if len == 0 {
        *index = 0;
        return;
    }
    *index = (*index as isize + delta).rem_euclid(len as isize) as usize;
}

/// Run the launcher. With `ARKLAY_LAUNCHER_CAPTURE` set it renders one frame
/// to that BMP and exits, which is how the layout is previewed headlessly.
pub fn run() -> Result<()> {
    update::cleanup();
    let (config, warnings) = Config::load_default();
    for warning in warnings {
        logging::write(&format!("warning: {warning}"));
        eprintln!("warning: {warning}");
    }
    let capture = env::var_os("ARKLAY_LAUNCHER_CAPTURE").map(PathBuf::from);
    let mut graphics = config.graphics;
    // The launcher is 2D text; the game backend is chosen when Play hands the
    // window over.
    graphics.renderer = RendererKind::Software;
    let mut display = Display::new("Arklay", capture.is_some(), graphics)?;
    if capture.is_none() {
        display.set_resizable(true);
    }
    let (width, height) = if capture.is_some() {
        capture_size().unwrap_or_else(|| display.drawable_size())
    } else {
        display.drawable_size()
    };
    let mut launcher = Launcher::new(&config, width, height)?;
    launcher.clear_available_if_current();
    launcher.maybe_start_check();
    // Headless checkpoints use this to exercise the install path without a
    // keyboard; it never runs for an ordinary launch.
    if capture.is_none() && env::var_os("ARKLAY_LAUNCHER_AUTOINSTALL").is_some() {
        launcher.screen = Screen::Install;
        launcher.install();
    }
    if capture.is_none() && env::var_os("ARKLAY_LAUNCHER_AUTOUPDATE").is_some() {
        launcher.auto_update = true;
        launcher.screen = Screen::Update;
        launcher.update_index = 1;
        launcher.start_check(true);
    }
    if capture.is_none() && env::var_os("ARKLAY_LAUNCHER_AUTOROLLBACK").is_some() {
        launcher.rollback();
    }
    if let Some(path) = capture {
        if let Ok(name) = env::var("ARKLAY_LAUNCHER_SCREEN") {
            launcher.set_capture_screen(&name);
        }
        launcher.draw();
        let image = crate::state::Image {
            width: launcher.frame.width,
            height: launcher.frame.height,
            rgba: launcher.frame.rgba.clone(),
        };
        let bytes = crate::bmp::encode_to_vec(&image)?;
        std::fs::write(&path, bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
        return Ok(());
    }
    logging::write("launcher: ready");
    let result = launcher.run_loop(&mut display);
    logging::write("launcher: exit");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_never_exceeds_the_column() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a long value", 8), "a lon...");
        assert_eq!(truncate("abcdef", 3), "...");
    }

    #[test]
    fn selections_wrap() {
        let mut index = 0;
        step(&mut index, 5, -1);
        assert_eq!(index, 4);
        step(&mut index, 5, 1);
        assert_eq!(index, 0);
        step(&mut index, 0, 1);
        assert_eq!(index, 0);
    }

    #[test]
    fn counts_are_grouped() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1204), "1,204");
        assert_eq!(format_count(486_480_423), "486,480,423");
    }

    #[test]
    fn progress_bars_are_bounded() {
        assert_eq!(progress_bar(0.0, 4), "[----]");
        assert_eq!(progress_bar(0.5, 4), "[##--]");
        assert_eq!(progress_bar(1.0, 4), "[####]");
        assert_eq!(progress_bar(2.0, 4), "[####]");
    }

    #[test]
    fn mods_are_filtered_by_base_and_ordered_by_config_then_load_order() {
        use crate::manifest::{Manifest, PackKind};
        use crate::pack::PackWriter;

        let dir = std::env::temp_dir().join(format!("arklay-launcher-mods-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let games = dir.join("games");
        let mods_dir = dir.join("mods");
        fs::create_dir_all(&games).unwrap();
        fs::create_dir_all(&mods_dir).unwrap();

        let write_pack = |path: &Path, manifest: Manifest| {
            let mut writer = PackWriter::new();
            writer
                .add(manifest::ENTRY, manifest.render().into_bytes())
                .unwrap();
            writer.write(path).unwrap();
        };
        let base = games.join("re1.akpak");
        write_pack(&base, Manifest::base("re1"));
        write_pack(
            &mods_dir.join("hd.akpak"),
            Manifest {
                kind: PackKind::Mod,
                base: Some("re1".to_string()),
                name: Some("HD".to_string()),
                load_order: 20,
                ..Manifest::base("hd")
            },
        );
        write_pack(
            &mods_dir.join("early.akpak"),
            Manifest {
                kind: PackKind::Mod,
                base: Some("re1".to_string()),
                load_order: 5,
                ..Manifest::base("early")
            },
        );
        write_pack(
            &mods_dir.join("other.akpak"),
            Manifest {
                kind: PackKind::Mod,
                base: Some("re2".to_string()),
                load_order: 1,
                ..Manifest::base("other")
            },
        );
        let demo = mods_dir.join("demo");
        fs::create_dir_all(&demo).unwrap();
        fs::write(
            demo.join(manifest::ENTRY),
            Manifest {
                kind: PackKind::Mod,
                base: Some("re1".to_string()),
                name: Some("Demo reels".to_string()),
                load_order: 10,
                ..Manifest::base("demo")
            }
            .render(),
        )
        .unwrap();

        let pack = PackRow {
            id: "re1".to_string(),
            name: "Resident Evil".to_string(),
            file: "re1.akpak".to_string(),
            version: "0.1.0".to_string(),
            source: "LOCAL".to_string(),
            path: base,
            stale: false,
            error: None,
        };
        let config = Config::default();
        let rows = scan_mods(Some(&pack), &config, std::slice::from_ref(&mods_dir));
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["early", "demo", "hd"]
        );

        let mut config = Config::default();
        config.mods.order = vec!["hd".to_string()];
        let rows = scan_mods(Some(&pack), &config, std::slice::from_ref(&mods_dir));
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["hd", "early", "demo"]
        );

        let mut mods = Mods::default();
        assert!(mod_enabled(&mods, "hd"));
        set_mod_enabled(&mut mods, "hd", false);
        assert!(!mod_enabled(&mods, "hd"));
        set_mod_enabled(&mut mods, "hd", true);
        assert!(mod_enabled(&mods, "hd"));
        assert!(mods.disabled.is_empty());
        let mut mods = Mods {
            enabled: Some(vec!["early".to_string()]),
            ..Mods::default()
        };
        assert!(!mod_enabled(&mods, "hd"));
        set_mod_enabled(&mut mods, "hd", true);
        assert_eq!(
            mods.enabled.as_deref(),
            Some(["early".to_string(), "hd".to_string()].as_slice())
        );
        set_mod_enabled(&mut mods, "early", false);
        assert_eq!(mods.disabled, vec!["early".to_string()]);
        assert_eq!(mods.enabled.as_deref(), Some(["hd".to_string()].as_slice()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_capture_screen_is_selectable() {
        let mut launcher = Launcher::new(&Config::default(), 960, 720).unwrap();
        launcher.set_capture_screen("progress");
        assert_eq!(launcher.screen, Screen::Progress);
        launcher.set_capture_screen("message");
        assert_eq!(launcher.screen, Screen::Message);
    }
}
