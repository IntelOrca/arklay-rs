//! The laboratory computer terminal (room 5060), the interactive screen the
//! small lab's console opens.
//!
//! The room script's `aot_set` event raises scenario flag `0x20` (the
//! interactive-screen gate) after its message; the room's init script has
//! already set SysFlags bit `0x1e`, which selects this screen out of the three
//! the original dispatches (`check_and_display_interactive_screen`,
//! 0x0042a030). The original's `display_computer_lab` (0x00412390,
//! `ComputerLab.cpp`) is a three-level state machine over the shared
//! interactive-screen block: a boot logo, the login (user JOHN, password ADA),
//! the door menu and the command prompt (MOLE), whose success releases one of
//! the two cell doors through `g_LocksFlags` and raises the lab map's room
//! flag.
//!
//! This port transcribes the *interaction*: the same virtual keyboard (a 4x8
//! cell grid, char code `cell` for `A`..`V`, `cell - 1` for `W`..`Z`, ENTER at
//! cell 0x17, BACKSPACE at 0x1d, EXIT at cell 0), the same expected inputs, the
//! same rejected/accepted holds and the same unlock bits. The original draws
//! its windows and keyboard from the `umb00`-`umb02` texture pages; those TIMs
//! are not packed, so the presentation here is the port's font and simple
//! panels (documented deviation - the flow and the state transitions are the
//! original's).

use crate::font::{Font, Tint};
use crate::render::Framebuffer;
use crate::ui::UiInput;

/// Keyboard grid columns.
const COLS: u8 = 8;
/// Keyboard grid rows.
const ROWS: u8 = 4;
/// The ENTER cell.
const CELL_ENTER: u8 = 0x17;
/// The BACKSPACE cell.
const CELL_BACKSPACE: u8 = 0x1d;
/// The EXIT cell (top-left).
const CELL_EXIT: u8 = 0x00;
/// Longest accepted field, the original's 8-byte text buffers.
const FIELD_LEN: usize = 8;

/// Virtual-keyboard char codes (`1` = `A`), the original's `s_expect*` space.
const EXPECT_USER: &[u8] = &[10, 15, 8, 14]; // JOHN
const EXPECT_PASS: &[u8] = &[1, 4, 1]; // ADA
const EXPECT_CMD: &[u8] = &[13, 15, 12, 5]; // MOLE

/// The two cell doors' `g_LocksFlags` bits, indexed by the menu row.
const DOOR_LOCK_BITS: [u8; 2] = [0x25, 0x26];
/// The lab map's room-flag bit (`ROOM_FLAG_MAP_BASE + MAP_INDEX_LABORATORY`).
pub const LAB_MAP_FLAG: u8 = 0x81;

/// One terminal stage, the original's `ST_FUNC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Boot, login, menu, command and the unlock hold.
    Login,
    Menu,
    Command,
    Unlocked,
}

/// The login/command sub-state within a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    /// Typing the first field.
    User,
    /// Typing the second field (the masked password / command).
    Secret,
    /// The credential was rejected; hold the error, then restart the stage.
    Rejected,
    /// The credential was accepted; hold, then advance.
    Accepted,
}

/// The lab computer terminal screen.
#[derive(Debug, Clone)]
pub struct ComputerLab {
    stage: Stage,
    sub: Sub,
    /// Held-state countdown.
    timer: i16,
    /// Keyboard cursor cell (`row * 8 + col`).
    cell: u8,
    /// The field being typed, as char codes.
    field: Vec<u8>,
    /// The accepted user name.
    user: Vec<u8>,
    /// Menu row (0/1 the two doors, 2 exit).
    menu_row: u8,
    /// `g_LocksFlags` bit to set when the screen closes, once unlocked.
    unlock: Option<u8>,
    /// Whether the lab map flag should be raised on close.
    map: bool,
    done: bool,
}

impl Default for ComputerLab {
    fn default() -> Self {
        Self::new()
    }
}

impl ComputerLab {
    /// Open the terminal at the login, cursor on `A`.
    pub fn new() -> Self {
        Self {
            stage: Stage::Login,
            sub: Sub::User,
            timer: 0,
            // `cl_login_init`: ST_COL = 1, ST_ROW = 0.
            cell: 1,
            field: Vec::new(),
            user: Vec::new(),
            menu_row: 0,
            unlock: None,
            map: false,
            done: false,
        }
    }

    /// Whether the terminal has finished and the engine should close it.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// The `g_LocksFlags` bit the terminal released, if any.
    pub fn unlock(&self) -> Option<u8> {
        self.unlock
    }

    /// Whether the lab map's room flag should be raised.
    pub fn map_flag(&self) -> bool {
        self.map
    }

    /// The keyboard char code of the current cell, `-1` for ENTER, `-2` for
    /// EXIT and `-3` for BACKSPACE (`cl_keyboard_input`, 0x00414400).
    fn cell_code(&self) -> i16 {
        match self.cell {
            CELL_EXIT => -2,
            CELL_ENTER => -1,
            CELL_BACKSPACE => -3,
            0x18..=0x1c => i16::from(self.cell - 1),
            0x1e | 0x1f => 0,
            cell => i16::from(cell),
        }
    }

    /// Move the keyboard cursor; the covered cells 0x1e/0x1f step back one
    /// column like the original.
    fn move_cursor(&mut self, input: UiInput) {
        let mut col = self.cell % COLS;
        let mut row = self.cell / COLS;
        if input.up && row > 0 {
            row -= 1;
        }
        if input.down && row < ROWS - 1 {
            row += 1;
        }
        if input.left {
            col = if col == 0 { COLS - 1 } else { col - 1 };
        }
        if input.right {
            col += 1;
            if col >= COLS {
                col = 0;
            }
        }
        let mut cell = row * COLS + col;
        if cell == 0x1f {
            row -= 1;
            cell = row * COLS + col;
        }
        if cell == 0x1e {
            col -= 1;
            cell = row * COLS + col;
        }
        self.cell = cell;
    }

    /// One `cl_login_typing`/command typing pass. Returns `true` when the
    /// field was submitted (ENTER).
    fn type_field(&mut self, input: UiInput) -> bool {
        self.move_cursor(input);
        if !input.confirm {
            return false;
        }
        match self.cell_code() {
            -1 => true,
            -2 => {
                self.done = true;
                true
            }
            -3 => {
                self.field.pop();
                false
            }
            code if code > 0 => {
                if self.field.len() < FIELD_LEN {
                    self.field.push(code as u8);
                }
                false
            }
            _ => false,
        }
    }

    /// Advance the login stage (`cl_login_*`).
    fn tick_login(&mut self, input: UiInput) {
        match self.sub {
            Sub::User => {
                if self.type_field(input) {
                    if self.done {
                        return;
                    }
                    self.user = std::mem::take(&mut self.field);
                    self.sub = Sub::Secret;
                    self.cell = 1;
                }
            }
            Sub::Secret => {
                if self.type_field(input) {
                    if self.done {
                        return;
                    }
                    if self.user == EXPECT_USER && self.field == EXPECT_PASS {
                        self.sub = Sub::Accepted;
                    } else {
                        self.sub = Sub::Rejected;
                    }
                    self.timer = 90;
                }
            }
            Sub::Rejected => {
                self.timer -= 1;
                if self.timer <= 0 {
                    self.stage = Stage::Login;
                    self.sub = Sub::User;
                    self.user.clear();
                    self.field.clear();
                    self.cell = 1;
                }
            }
            Sub::Accepted => {
                self.timer -= 1;
                if self.timer <= 0 {
                    self.stage = Stage::Menu;
                    self.sub = Sub::User;
                    self.user.clear();
                    self.field.clear();
                    self.menu_row = 0;
                }
            }
        }
    }

    /// Advance the door menu (`cl_menu_select`).
    fn tick_menu(&mut self, input: UiInput) {
        if input.up && self.menu_row > 0 {
            self.menu_row -= 1;
        }
        if input.down && self.menu_row < 2 {
            self.menu_row += 1;
        }
        if input.cancel {
            self.done = true;
            return;
        }
        if input.confirm {
            if self.menu_row == 2 {
                self.done = true;
                return;
            }
            self.stage = Stage::Command;
            self.sub = Sub::User;
            self.field.clear();
            self.cell = 1;
        }
    }

    /// Advance the command stage (`cl_cmd_*`).
    fn tick_command(&mut self, input: UiInput) {
        match self.sub {
            Sub::User => {
                if self.type_field(input) {
                    if self.done {
                        return;
                    }
                    if self.field == EXPECT_CMD {
                        self.sub = Sub::Accepted;
                        self.timer = 90;
                    } else {
                        self.sub = Sub::Rejected;
                        self.timer = 90;
                    }
                }
            }
            Sub::Accepted => {
                self.timer -= 1;
                if self.timer <= 0 {
                    // `cl_cmd_unlock`: release the selected door and raise the
                    // lab map's room flag.
                    self.unlock = Some(DOOR_LOCK_BITS[usize::from(self.menu_row)]);
                    self.map = true;
                    self.stage = Stage::Unlocked;
                    self.sub = Sub::User;
                    self.timer = 90;
                    self.field.clear();
                }
            }
            Sub::Rejected => {
                self.timer -= 1;
                if self.timer <= 0 {
                    self.stage = Stage::Menu;
                    self.sub = Sub::User;
                    self.field.clear();
                }
            }
            // The command stage has no secret field; `Sub::Secret` is only the
            // login's password step.
            Sub::Secret => {}
        }
    }

    /// Advance one fixed tick. Returns `true` once the terminal has finished.
    pub fn update(&mut self, input: UiInput) -> bool {
        if self.done {
            return true;
        }
        match self.stage {
            Stage::Login => self.tick_login(input),
            Stage::Menu => self.tick_menu(input),
            Stage::Command => self.tick_command(input),
            Stage::Unlocked => {
                self.timer -= 1;
                if self.timer <= 0 || input.confirm || input.cancel {
                    self.done = true;
                }
            }
        }
        self.done
    }

    /// The glyph byte for an ASCII letter.
    fn letter(byte: u8) -> u8 {
        // `A`-`Z` encode to 0x1D.. in the game-text font.
        0x1D + (byte - b'A')
    }

    /// Render a field's chars, masking all but the first with blocks.
    fn draw_field(framebuffer: &mut Framebuffer, font: &Font, x: i32, y: i32, field: &[u8]) {
        let mut cursor = x;
        for &code in field {
            if code == 0 {
                cursor += 8;
                continue;
            }
            let byte = Self::letter(b'A' + code - 1);
            font.draw_text(framebuffer, cursor, y, Tint::White, 2, &[byte, 0x01]);
            cursor += 8;
        }
    }

    /// Draw the screen. `font` is the pack font; a missing font draws only the
    /// panels.
    pub fn draw(&self, framebuffer: &mut Framebuffer, font: Option<&Font>) {
        framebuffer.fill_rect([0, 0, 320, 240], [8, 10, 20, 255]);
        let Some(font) = font else {
            return;
        };
        let line = |framebuffer: &mut Framebuffer, y: i32, text: &str| {
            let bytes: Vec<u8> = text
                .bytes()
                .map(|byte| match byte {
                    b'A'..=b'Z' => 0x1D + (byte - b'A'),
                    b'a'..=b'z' => 0x1D + (byte - b'a'),
                    b'0'..=b'9' => 0x0C + (byte - b'0'),
                    b' ' => 0x00,
                    b':' => 0x16,
                    b'.' => 0x79,
                    b'-' => 0x3B,
                    _ => 0x1B,
                })
                .collect();
            font.draw_text(framebuffer, 16, y, Tint::Green, 2, &bytes);
        };
        match self.stage {
            Stage::Login => {
                line(framebuffer, 8, "UMBRELLA LAB TERMINAL");
                line(framebuffer, 32, "USER");
                Self::draw_field(framebuffer, font, 72, 32, &self.field);
                line(framebuffer, 48, "PASSWORD");
                if self.sub != Sub::User {
                    for index in 0..self.field.len() {
                        framebuffer
                            .fill_rect([104 + index as i32 * 8, 48, 6, 10], [200, 255, 200, 255]);
                    }
                }
                self.draw_keyboard(framebuffer, font);
                match self.sub {
                    Sub::Rejected => line(framebuffer, 208, "ACCESS DENIED"),
                    Sub::Accepted => line(framebuffer, 208, "ACCESS GRANTED"),
                    _ => line(framebuffer, 208, "ENTER USER AND PASSWORD"),
                }
            }
            Stage::Menu => {
                line(framebuffer, 8, "SELECT A DOOR TO RELEASE");
                for (row, text) in ["RELEASE DOOR 1", "RELEASE DOOR 2", "EXIT"]
                    .iter()
                    .enumerate()
                {
                    let y = 48 + row as i32 * 20;
                    line(framebuffer, y, text);
                    if self.menu_row == row as u8 {
                        framebuffer.fill_rect([10, y + 2, 4, 10], [80, 255, 80, 255]);
                    }
                }
            }
            Stage::Command => {
                line(framebuffer, 8, "COMMAND");
                for index in 0..self.field.len() {
                    framebuffer.fill_rect([80 + index as i32 * 8, 32, 6, 10], [200, 255, 200, 255]);
                }
                self.draw_keyboard(framebuffer, font);
                match self.sub {
                    Sub::Rejected => line(framebuffer, 208, "INVALID COMMAND"),
                    Sub::Accepted => line(framebuffer, 208, "COMMAND ACCEPTED"),
                    _ => line(framebuffer, 208, "TYPE THE RELEASE COMMAND"),
                }
            }
            Stage::Unlocked => {
                line(framebuffer, 96, "DOOR RELEASED");
            }
        }
    }

    /// The virtual keyboard: 4 rows x 8 cells, `A`..`Z` around ENTER, with the
    /// cursor cell highlighted.
    fn draw_keyboard(&self, framebuffer: &mut Framebuffer, font: &Font) {
        for row in 0..ROWS {
            for col in 0..COLS {
                let cell = row * COLS + col;
                let x = 40 + i32::from(col) * 24;
                let y = 96 + i32::from(row) * 20;
                let (label, byte): (&str, u8) = match cell {
                    CELL_EXIT => ("EXIT", 0),
                    CELL_ENTER => ("ENT", 0),
                    CELL_BACKSPACE => ("DEL", 0),
                    0x1e | 0x1f => ("", 0),
                    1..=0x16 => ("", Self::letter(b'A' + cell - 1)),
                    0x18..=0x1b => ("", Self::letter(b'W' + cell - 0x18)),
                    _ => ("", 0),
                };
                if self.cell == cell {
                    framebuffer.fill_rect([x - 2, y - 2, 20, 16], [60, 120, 60, 255]);
                }
                if byte != 0 {
                    font.draw_text(framebuffer, x, y, Tint::White, 2, &[byte, 0x01]);
                } else if !label.is_empty() {
                    let bytes: Vec<u8> = label.bytes().map(|b| 0x1D + (b - b'A')).collect();
                    font.draw_text(framebuffer, x, y, Tint::Yellow, 2, &bytes);
                }
            }
        }
    }
}

/// The scene light the terminal installs over the room's own (`s_labLights`).
pub const LAB_LIGHTS: [[i32; 3]; 3] = [
    [14300, -1950, 9650],
    [4450, -2650, 12000],
    [14700, -1600, 5600],
];

/// The ambient the terminal installs (`0x708` per channel).
pub const LAB_AMBIENT: i16 = 0x708;

#[cfg(test)]
mod tests {
    use super::*;

    /// Type a char-code sequence through the keyboard: navigate to the cell
    /// for each code, then commit it.
    fn type_codes(screen: &mut ComputerLab, codes: &[u8]) {
        for &code in codes {
            let cell = if code <= 0x16 { code } else { code + 1 };
            // Navigate from the current cell to the target.
            let mut guard = 0;
            while screen.cell != cell && guard < 64 {
                let (cur_row, cur_col) = (screen.cell / COLS, screen.cell % COLS);
                let (row, col) = (cell / COLS, cell % COLS);
                let input = if cur_row < row {
                    UiInput {
                        down: true,
                        ..UiInput::default()
                    }
                } else if cur_row > row {
                    UiInput {
                        up: true,
                        ..UiInput::default()
                    }
                } else if cur_col < col {
                    UiInput {
                        right: true,
                        ..UiInput::default()
                    }
                } else {
                    UiInput {
                        left: true,
                        ..UiInput::default()
                    }
                };
                screen.move_cursor(input);
                guard += 1;
            }
            assert_eq!(screen.cell, cell, "cursor reached cell {cell:#x}");
            screen.type_field(UiInput {
                confirm: true,
                ..UiInput::default()
            });
        }
    }

    fn submit() -> UiInput {
        UiInput {
            confirm: true,
            ..UiInput::default()
        }
    }

    #[test]
    fn the_login_accepts_john_and_ada_then_opens_the_menu() {
        let mut screen = ComputerLab::new();
        // The user field is typed through the keyboard; ENTER submits it.
        type_codes(&mut screen, &[10, 15, 8, 14]);
        screen.cell = CELL_ENTER;
        screen.tick_login(submit());
        assert_eq!(screen.sub, Sub::Secret);
        type_codes(&mut screen, &[1, 4, 1]);
        screen.cell = CELL_ENTER;
        screen.tick_login(submit());
        assert_eq!(screen.sub, Sub::Accepted);
        for _ in 0..90 {
            screen.tick_login(UiInput::default());
        }
        assert_eq!(screen.stage, Stage::Menu);
    }

    #[test]
    fn the_wrong_password_is_rejected() {
        let mut screen = ComputerLab::new();
        type_codes(&mut screen, &[10, 15, 8, 14]);
        screen.cell = CELL_ENTER;
        screen.tick_login(submit());
        type_codes(&mut screen, &[1, 4, 2]);
        screen.cell = CELL_ENTER;
        screen.tick_login(submit());
        assert_eq!(screen.sub, Sub::Rejected);
    }

    #[test]
    fn mole_releases_the_selected_door_and_raises_the_map_flag() {
        let mut screen = ComputerLab::new();
        screen.stage = Stage::Menu;
        screen.menu_row = 1;
        screen.tick_menu(submit());
        assert_eq!(screen.stage, Stage::Command);
        type_codes(&mut screen, &[13, 15, 12, 5]);
        screen.cell = CELL_ENTER;
        screen.tick_command(submit());
        assert_eq!(screen.sub, Sub::Accepted);
        for _ in 0..90 {
            screen.tick_command(UiInput::default());
        }
        assert_eq!(screen.stage, Stage::Unlocked);
        assert_eq!(screen.unlock(), Some(DOOR_LOCK_BITS[1]));
        assert!(screen.map_flag());
        // The unlock hold then closes the terminal.
        for _ in 0..90 {
            screen.update(UiInput::default());
        }
        assert!(screen.is_done());
    }

    #[test]
    fn cancel_leaves_the_terminal_from_the_menu() {
        let mut screen = ComputerLab::new();
        screen.stage = Stage::Menu;
        screen.update(UiInput {
            cancel: true,
            ..UiInput::default()
        });
        assert!(screen.is_done());
    }
}
