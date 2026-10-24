//! The startup launcher shown when no game pack is named on the command line.
//!
//! The launcher is not a game screen: it opens before any pack exists, so it
//! carries its own embedded face and draws an 80x25 character grid centred in
//! the configured window. Every row and column is derived from that grid, so
//! selecting a row, changing a status or resizing the window never shifts a
//! table column.
//!
//! M1 is the shell: navigation, the visual language, and the real data-root
//! paths with placeholder pack/mod/update data. Detection, conversion, play
//! and updates arrive in the following checkpoints.

mod text;

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
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
use crate::config::Config;
use crate::engine::Display;
use crate::paths;
use crate::video::RendererKind;

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

/// The screens the launcher can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Main,
    Play,
    Install,
    Update,
    Mods,
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

/// One row of the (placeholder) play list.
#[derive(Clone)]
struct PackRow {
    name: String,
    file: String,
    version: String,
    source: String,
}

/// One row of the (placeholder) mods list.
#[derive(Clone)]
struct ModRow {
    name: String,
    id: String,
    version: String,
    enabled: bool,
}

/// A modal message with a way back.
struct Message {
    title: String,
    lines: Vec<String>,
    back: Screen,
}

/// The launcher state.
struct Launcher {
    screen: Screen,
    main_index: usize,
    play_index: usize,
    update_index: usize,
    mods_index: usize,
    packs: Vec<PackRow>,
    mods: Vec<ModRow>,
    channel_nightly: bool,
    auto_check: bool,
    fake_update: bool,
    target: String,
    message: Option<Message>,
    frame: Frame,
    grid: Grid,
    hits: Vec<(usize, usize)>,
}

impl Launcher {
    fn new(width: u32, height: u32) -> Result<Self> {
        let width = width.max(1);
        let height = height.max(1);
        let target = paths::games_dir(&paths::primary_data_dir())
            .join("re1.akpak")
            .display()
            .to_string();
        Ok(Self {
            screen: Screen::Main,
            main_index: 0,
            play_index: 0,
            update_index: 0,
            mods_index: 0,
            packs: vec![PackRow {
                name: "Resident Evil".to_string(),
                file: "re1.akpak".to_string(),
                version: "0.1.0".to_string(),
                source: "LOCAL".to_string(),
            }],
            mods: vec![
                ModRow {
                    name: "HD replacement demo".to_string(),
                    id: "hd".to_string(),
                    version: "0.1.0".to_string(),
                    enabled: true,
                },
                ModRow {
                    name: "Balance tweaks".to_string(),
                    id: "balance".to_string(),
                    version: "0.1.0".to_string(),
                    enabled: false,
                },
                ModRow {
                    name: "Demo reels".to_string(),
                    id: "demo".to_string(),
                    version: "0.1.0".to_string(),
                    enabled: true,
                },
            ],
            channel_nightly: false,
            auto_check: true,
            fake_update: true,
            target,
            message: None,
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
            Screen::Install => "ENTER REBUILD   B CHANGE SOURCE   ESC BACK",
            Screen::Update => "ENTER SELECT   LEFT/RIGHT CHANGE   ESC BACK",
            Screen::Mods => "SPACE TOGGLE   CTRL+UP/DOWN MOVE   O FOLDER   ESC BACK",
            Screen::Message => "ENTER BACK",
        };
        self.grid.draw(&mut self.frame, 24, 2, hint, Color::DIM);
        self.grid
            .draw_right(&mut self.frame, 24, "M1 PREVIEW", Color::DIM);
    }

    fn draw_main(&mut self) {
        for (index, label) in MAIN_ITEMS.iter().enumerate() {
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
                    if self.packs.is_empty() {
                        "REQUIRED".to_string()
                    } else {
                        String::new()
                    },
                    Color::AMBER,
                ),
                2 => (
                    if self.fake_update {
                        "UPDATE AVAILABLE".to_string()
                    } else {
                        String::new()
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
            self.table_row(
                row,
                selected,
                &[
                    (4, &truncate(&pack.name, 28)),
                    (34, &truncate(&pack.file, 16)),
                    (52, &truncate(&pack.version, 10)),
                    (64, &truncate(&pack.source, 12)),
                ],
            );
            self.hits.push((row, index));
        }
    }

    fn draw_install(&mut self) {
        self.grid
            .draw(&mut self.frame, 3, 4, "INSTALL / REBUILD", Color::BODY);
        let source = "F:\\GAMES\\STEAMAPPS\\COMMON\\4249100_BIOHAZARD";
        let pack = if self.packs.is_empty() {
            "NOT INSTALLED".to_string()
        } else {
            format!("re1.akpak  ({})", self.packs[0].version)
        };
        let status = if self.packs.is_empty() {
            "READY - ABOUT 486 MB"
        } else {
            "INSTALLED - REBUILD AVAILABLE"
        };
        for (row, label, value) in [
            (5, "SOURCE", source),
            (6, "TARGET", self.target.as_str()),
            (7, "PACK", pack.as_str()),
            (8, "STATUS", status),
        ] {
            self.grid.draw(&mut self.frame, row, 4, label, Color::DIM);
            self.grid
                .draw(&mut self.frame, row, 16, &truncate(value, 62), Color::BODY);
        }
    }

    fn draw_update(&mut self) {
        self.grid.draw(&mut self.frame, 3, 4, "UPDATE", Color::BODY);
        for (index, label) in UPDATE_ITEMS.iter().enumerate() {
            let (status, color) = match index {
                0 => (String::new(), Color::DIM),
                1 => (
                    if self.fake_update {
                        "0.1.0 -> 0.1.1".to_string()
                    } else {
                        "UP TO DATE".to_string()
                    },
                    if self.fake_update {
                        Color::AMBER
                    } else {
                        Color::DIM
                    },
                ),
                2 => ("REQUIRED".to_string(), Color::AMBER),
                3 => ("NONE".to_string(), Color::DIM),
                4 => (
                    if self.channel_nightly {
                        "NIGHTLY".to_string()
                    } else {
                        "RELEASE".to_string()
                    },
                    Color::BODY,
                ),
                _ => (
                    if self.auto_check {
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
        self.table_header(5, &[(4, "ON"), (9, "MOD"), (46, "ID"), (62, "VERSION")]);
        if self.mods.is_empty() {
            self.grid
                .draw(&mut self.frame, 7, 4, "NO MODS DISCOVERED.", Color::DIM);
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
                    (46, &truncate(&row.id, 14)),
                    (62, &truncate(&row.version, 12)),
                ],
            );
            self.hits.push((line, index));
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
    fn key(&mut self, scancode: SDL_Scancode, modifiers: SDL_Keymod) -> bool {
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
            SDL_SCANCODE_RETURN | SDL_SCANCODE_KP_ENTER => return self.activate(),
            SDL_SCANCODE_SPACE => {
                if self.screen == Screen::Mods {
                    self.toggle_mod();
                } else if self.screen == Screen::Update && self.update_index >= 4 {
                    self.toggle_update();
                } else {
                    return self.activate();
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
                    self.show_message(
                        "CHANGE SOURCE",
                        &["Folder picking arrives with Steam detection in M2."],
                        Screen::Install,
                    );
                }
            }
            SDL_SCANCODE_O => {
                if self.screen == Screen::Mods {
                    self.show_message(
                        "MODS FOLDER",
                        &["The mods folder and its contents arrive in M2."],
                        Screen::Mods,
                    );
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
            Screen::Install | Screen::Message => {}
        }
    }

    /// Enter/Space on the current screen. Returns true to close the launcher.
    fn activate(&mut self) -> bool {
        match self.screen {
            Screen::Main => return self.activate_main(),
            Screen::Play => {
                if let Some(pack) = self.packs.get(self.play_index) {
                    self.show_message(
                        "PLAY",
                        &[&format!("{} is wired up in M3.", pack.name)],
                        Screen::Play,
                    );
                }
            }
            Screen::Install => {
                self.show_message(
                    "INSTALL / REBUILD",
                    &["Conversion is wired up in M2."],
                    Screen::Install,
                );
            }
            Screen::Update => self.activate_update(),
            Screen::Mods => self.toggle_mod(),
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
            0 => self.show_message(
                "CHECK FOR UPDATES",
                &["The release/nightly check arrives in M4."],
                Screen::Update,
            ),
            1 => self.show_message(
                "UPDATE ENGINE",
                &["Engine downloads and the in-place swap arrive in M4."],
                Screen::Update,
            ),
            2 => self.show_message(
                "REBUILD GAME PACK",
                &["Pack rebuilds arrive with conversion in M2."],
                Screen::Update,
            ),
            3 => self.show_message(
                "ROLL BACK LAST UPDATE",
                &["Rollback arrives with the updater in M4."],
                Screen::Update,
            ),
            4 => self.channel_nightly = !self.channel_nightly,
            5 => self.auto_check = !self.auto_check,
            _ => {}
        }
    }

    fn toggle_update(&mut self) {
        if self.update_index == 4 {
            self.channel_nightly = !self.channel_nightly;
        } else if self.update_index == 5 {
            self.auto_check = !self.auto_check;
        }
    }

    fn toggle_mod(&mut self) {
        if let Some(row) = self.mods.get_mut(self.mods_index) {
            row.enabled = !row.enabled;
        }
    }

    fn move_mod(&mut self, delta: isize) {
        let len = self.mods.len();
        if len < 2 {
            return;
        }
        let target = (self.mods_index as isize + delta).rem_euclid(len as isize) as usize;
        self.mods.swap(self.mods_index, target);
        self.mods_index = target;
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

    /// Turn a mouse click into the row it landed on and activate it.
    fn click(&mut self, x: i32, y: i32) {
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
            Screen::Install | Screen::Message => return,
        }
        self.activate();
    }

    /// Pick a screen for a headless preview capture.
    fn set_capture_screen(&mut self, name: &str) {
        self.screen = match name {
            "play" => Screen::Play,
            "install" => Screen::Install,
            "update" => Screen::Update,
            "mods" => Screen::Mods,
            "message" => {
                self.show_message("PREVIEW", &["A sample message screen."], Screen::Main);
                Screen::Message
            }
            _ => Screen::Main,
        };
    }
}

/// A preview size from `ARKLAY_LAUNCHER_CAPTURE_SIZE=WxH`, for checking the
/// layout at window sizes other than the capture default.
fn capture_size() -> Option<(u32, u32)> {
    let value = env::var("ARKLAY_LAUNCHER_CAPTURE_SIZE").ok()?;
    let (width, height) = value.split_once(['x', 'X', ','])?;
    let width = width.trim().parse().ok()?;
    let height = height.trim().parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
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
    let (config, warnings) = Config::load_default();
    for warning in warnings {
        crate::logging::write(&format!("warning: {warning}"));
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
    let mut launcher = Launcher::new(width, height)?;
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
    crate::logging::write("launcher: ready");
    let result = launcher.run_loop(&mut display);
    crate::logging::write("launcher: exit");
    result
}

impl Launcher {
    fn run_loop(&mut self, display: &mut Display) -> Result<()> {
        let mut dirty = true;
        let mut event = SDL_Event::default();
        loop {
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
                        } else if self.key(scancode, modifiers) {
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
                        self.click(x, y);
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
    fn a_capture_screen_is_selectable() {
        let mut launcher = Launcher::new(960, 720).unwrap();
        launcher.set_capture_screen("mods");
        assert_eq!(launcher.screen, Screen::Mods);
        launcher.set_capture_screen("message");
        assert_eq!(launcher.screen, Screen::Message);
    }
}
