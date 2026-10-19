//! The logical action map: every action the game reads and the physical
//! inputs bound to it.
//!
//! The engine never asks about a key, a mouse button or a gamepad button
//! directly. Each input source is looked up in [`Bindings`], which maps it to
//! the [`Action`]s it drives; an action may carry any number of bindings from
//! any device (aim on `C`, the right mouse button and the left trigger at
//! once). The port-only debug inputs (F1 or the gamepad's back button for the
//! debug menu, F9 return to title) are fixed and can never be bound: see
//! [`Source::reserved_bit`].
//!
//! Bindings persist in `arklay.toml`'s `[bindings]` table as one array of
//! source names per action:
//!
//! ```toml
//! [bindings]
//! aim = ["key:C", "mouse:right", "axis:lefttrigger+"]
//! ```
//!
//! A source name is `key:<SDL scancode name>`, `mouse:left|middle|right|x1|x2`,
//! `wheel:up|down|left|right`, `pad:<SDL gamepad button>` (`a`, `b`, `dpup`,
//! `leftshoulder`, `start`, ...) or `axis:<SDL gamepad axis>+|-` (`leftx-`,
//! `lefttrigger+`, ...). An input SDL has no name for is written by number:
//! `key:#N`, `mouse:N`, `pad:#N`, `axis:#N+`. An action the table leaves out
//! keeps its default.

use std::ffi::{CStr, CString};
use std::fmt;

use anyhow::{Result, bail};
use sdl3_sys::gamepad::{
    SDL_GAMEPAD_AXIS_COUNT, SDL_GAMEPAD_AXIS_INVALID, SDL_GAMEPAD_BUTTON_BACK,
    SDL_GAMEPAD_BUTTON_COUNT, SDL_GAMEPAD_BUTTON_INVALID, SDL_GamepadAxis, SDL_GamepadButton,
    SDL_GetGamepadAxisFromString, SDL_GetGamepadButtonFromString, SDL_GetGamepadStringForAxis,
    SDL_GetGamepadStringForButton,
};
use sdl3_sys::keyboard::{SDL_GetScancodeFromName, SDL_GetScancodeName};
use sdl3_sys::scancode::{
    SDL_SCANCODE_BACKSPACE, SDL_SCANCODE_C, SDL_SCANCODE_COUNT, SDL_SCANCODE_DOWN,
    SDL_SCANCODE_ESCAPE, SDL_SCANCODE_F1, SDL_SCANCODE_F9, SDL_SCANCODE_LEFT,
    SDL_SCANCODE_LEFTBRACKET, SDL_SCANCODE_LSHIFT, SDL_SCANCODE_RETURN, SDL_SCANCODE_RIGHT,
    SDL_SCANCODE_RIGHTBRACKET, SDL_SCANCODE_RSHIFT, SDL_SCANCODE_SPACE, SDL_SCANCODE_TAB,
    SDL_SCANCODE_UNKNOWN, SDL_SCANCODE_UP, SDL_SCANCODE_X, SDL_SCANCODE_Z, SDL_Scancode,
};

/// One logical action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    Up,
    Down,
    Left,
    Right,
    /// Confirm / action / dismiss a message (the pad's cross).
    Confirm,
    /// Cancel in menus (the pad's circle).
    Cancel,
    Run,
    /// Aim the equipped weapon (held).
    Aim,
    /// Fire / reload while aiming.
    Fire,
    /// START: the inventory (pause menu).
    Start,
    /// L1: page the item box back.
    PageLeft,
    /// R1: page the item box forward.
    PageRight,
}

/// Every action, in the order the rebind screen and the config file list them.
pub const ACTIONS: [Action; 12] = [
    Action::Up,
    Action::Down,
    Action::Left,
    Action::Right,
    Action::Confirm,
    Action::Cancel,
    Action::Run,
    Action::Aim,
    Action::Fire,
    Action::Start,
    Action::PageLeft,
    Action::PageRight,
];

/// The fixed F1 (gamepad back) debug-menu bit, above every action bit.
pub const DEBUG_MENU_BIT: u32 = 1 << 16;
/// The fixed F9 return-to-title bit.
pub const RETURN_TITLE_BIT: u32 = 1 << 17;

impl Action {
    /// The action's bit in an input word (the `--input` script word and the
    /// engine's latched held/pressed words).
    pub const fn bit(self) -> u32 {
        1 << self as u32
    }

    /// The action's `[bindings]` key.
    pub const fn name(self) -> &'static str {
        match self {
            Action::Up => "up",
            Action::Down => "down",
            Action::Left => "left",
            Action::Right => "right",
            Action::Confirm => "confirm",
            Action::Cancel => "cancel",
            Action::Run => "run",
            Action::Aim => "aim",
            Action::Fire => "fire",
            Action::Start => "start",
            Action::PageLeft => "page_left",
            Action::PageRight => "page_right",
        }
    }

    /// The action for a `[bindings]` key.
    pub fn from_name(name: &str) -> Option<Self> {
        ACTIONS.into_iter().find(|action| action.name() == name)
    }

    /// The rebind screen's label key.
    pub const fn label(self) -> &'static str {
        match self {
            Action::Up => "UI_CONTROLS_UP",
            Action::Down => "UI_CONTROLS_DOWN",
            Action::Left => "UI_CONTROLS_LEFT",
            Action::Right => "UI_CONTROLS_RIGHT",
            Action::Confirm => "UI_CONTROLS_CONFIRM",
            Action::Cancel => "UI_CONTROLS_CANCEL",
            Action::Run => "UI_CONTROLS_RUN",
            Action::Aim => "UI_CONTROLS_AIM",
            Action::Fire => "UI_CONTROLS_FIRE",
            Action::Start => "UI_CONTROLS_START",
            Action::PageLeft => "UI_CONTROLS_PAGE_LEFT",
            Action::PageRight => "UI_CONTROLS_PAGE_RIGHT",
        }
    }

    /// Whether the rebind screen itself is driven by the action, so it must
    /// always keep a keyboard key of its own (see [`Bindings::lockout`]).
    pub const fn is_essential(self) -> bool {
        matches!(
            self,
            Action::Up
                | Action::Down
                | Action::Left
                | Action::Right
                | Action::Confirm
                | Action::Cancel
                | Action::Start
        )
    }
}

/// A mouse wheel direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Wheel {
    Up,
    Down,
    Left,
    Right,
}

const WHEEL_NAMES: [(Wheel, &str); 4] = [
    (Wheel::Up, "up"),
    (Wheel::Down, "down"),
    (Wheel::Left, "left"),
    (Wheel::Right, "right"),
];

/// SDL's mouse button numbers and their names.
const MOUSE_NAMES: [(u8, &str); 5] = [
    (1, "left"),
    (2, "middle"),
    (3, "right"),
    (4, "x1"),
    (5, "x2"),
];

/// One physical input a binding can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// A keyboard key, by SDL scancode.
    Key(i32),
    /// A mouse button, by SDL button number (1 left .. 5 X2).
    Mouse(u8),
    /// One notch of the mouse wheel: a press with no hold.
    Wheel(Wheel),
    /// A gamepad button, by `SDL_GamepadButton`.
    PadButton(i32),
    /// A gamepad axis pushed past its threshold in one direction, by
    /// `SDL_GamepadAxis` (the triggers only ever read positive).
    PadAxis(i32, bool),
}

impl Source {
    /// A keyboard key.
    pub fn key(scancode: SDL_Scancode) -> Self {
        Source::Key(scancode.0)
    }

    /// The fixed port-only bit a reserved input raises (F1 or the gamepad's
    /// back button for the debug menu, F9), or `None` for a source that can
    /// be bound.
    pub fn reserved_bit(self) -> Option<u32> {
        match self {
            Source::Key(code) if code == SDL_SCANCODE_F1.0 => Some(DEBUG_MENU_BIT),
            Source::Key(code) if code == SDL_SCANCODE_F9.0 => Some(RETURN_TITLE_BIT),
            Source::PadButton(button) if button == SDL_GAMEPAD_BUTTON_BACK.0 => {
                Some(DEBUG_MENU_BIT)
            }
            _ => None,
        }
    }

    /// Whether the source's `device:name` form reads back as the same source,
    /// so it can be saved (SDL names two scancodes `Return`, for one).
    pub fn round_trips(self) -> bool {
        Source::parse(&self.to_string()).is_ok_and(|parsed| parsed == self)
    }

    /// Parse a `[bindings]` source name.
    pub fn parse(text: &str) -> Result<Self> {
        let Some((device, name)) = text.split_once(':') else {
            bail!("`{text}` is not `device:name`");
        };
        let source = match device {
            "key" => {
                if let Some(code) = numbered(name, SDL_SCANCODE_COUNT.0) {
                    if code == SDL_SCANCODE_UNKNOWN.0 {
                        bail!("unknown key `{name}`");
                    }
                    Source::Key(code)
                } else {
                    let scancode = cstr_lookup(name, |c| unsafe { SDL_GetScancodeFromName(c) });
                    match scancode {
                        Some(code) if code != SDL_SCANCODE_UNKNOWN => Source::key(code),
                        _ => bail!("unknown key `{name}`"),
                    }
                }
            }
            "mouse" => match MOUSE_NAMES.iter().find(|(_, n)| *n == name) {
                Some(&(button, _)) => Source::Mouse(button),
                None => match name.parse::<u8>() {
                    Ok(button) if button > 0 => Source::Mouse(button),
                    _ => bail!("unknown mouse button `{name}`"),
                },
            },
            "wheel" => match WHEEL_NAMES.iter().find(|(_, n)| *n == name) {
                Some(&(wheel, _)) => Source::Wheel(wheel),
                None => bail!("unknown wheel direction `{name}`"),
            },
            "pad" => {
                let button = numbered(name, SDL_GAMEPAD_BUTTON_COUNT.0)
                    .map(SDL_GamepadButton)
                    .or_else(|| {
                        cstr_lookup(name, |c| unsafe { SDL_GetGamepadButtonFromString(c) })
                    });
                match button {
                    Some(button) if button != SDL_GAMEPAD_BUTTON_INVALID => {
                        Source::PadButton(button.0)
                    }
                    _ => bail!("unknown gamepad button `{name}`"),
                }
            }
            "axis" => {
                let (axis, positive) = if let Some(axis) = name.strip_suffix('+') {
                    (axis, true)
                } else if let Some(axis) = name.strip_suffix('-') {
                    (axis, false)
                } else {
                    bail!("gamepad axis `{name}` needs a `+` or `-` direction");
                };
                let axis = numbered(axis, SDL_GAMEPAD_AXIS_COUNT.0)
                    .map(SDL_GamepadAxis)
                    .or_else(|| cstr_lookup(axis, |c| unsafe { SDL_GetGamepadAxisFromString(c) }));
                match axis {
                    Some(axis) if axis != SDL_GAMEPAD_AXIS_INVALID => {
                        Source::PadAxis(axis.0, positive)
                    }
                    _ => bail!("unknown gamepad axis `{name}`"),
                }
            }
            other => bail!("unknown input device `{other}`"),
        };
        if source.reserved_bit().is_some() {
            bail!("`{text}` is reserved for the port's debug keys");
        }
        Ok(source)
    }

    /// The short upper-case label the rebind screen draws.
    pub fn label(self) -> String {
        match self {
            Source::Key(_) => self.name().1.to_ascii_uppercase(),
            Source::Mouse(_) | Source::Wheel(_) | Source::PadButton(_) | Source::PadAxis(..) => {
                let (device, name) = self.name();
                format!("{device} {name}").to_ascii_uppercase()
            }
        }
    }

    /// The `(device, name)` pair of the source's `device:name` form.
    fn name(self) -> (&'static str, String) {
        match self {
            Source::Key(code) => {
                let name = (0..SDL_SCANCODE_COUNT.0)
                    .contains(&code)
                    .then(|| sdl_str(unsafe { SDL_GetScancodeName(SDL_Scancode(code)) }))
                    .flatten()
                    .unwrap_or_else(|| format!("#{code}"));
                ("key", name)
            }
            Source::Mouse(button) => {
                let name = MOUSE_NAMES
                    .iter()
                    .find(|(b, _)| *b == button)
                    .map_or_else(|| format!("{button}"), |(_, n)| n.to_string());
                ("mouse", name)
            }
            Source::Wheel(wheel) => {
                let name = WHEEL_NAMES.iter().find(|(w, _)| *w == wheel).unwrap().1;
                ("wheel", name.to_string())
            }
            Source::PadButton(button) => {
                let name = (0..SDL_GAMEPAD_BUTTON_COUNT.0)
                    .contains(&button)
                    .then(|| {
                        sdl_str(unsafe { SDL_GetGamepadStringForButton(SDL_GamepadButton(button)) })
                    })
                    .flatten()
                    .unwrap_or_else(|| format!("#{button}"));
                ("pad", name)
            }
            Source::PadAxis(axis, positive) => {
                let name = (0..SDL_GAMEPAD_AXIS_COUNT.0)
                    .contains(&axis)
                    .then(|| sdl_str(unsafe { SDL_GetGamepadStringForAxis(SDL_GamepadAxis(axis)) }))
                    .flatten()
                    .unwrap_or_else(|| format!("#{axis}"));
                (
                    "axis",
                    format!("{name}{}", if positive { '+' } else { '-' }),
                )
            }
        }
    }
}

impl fmt::Display for Source {
    /// The `device:name` form [`Source::parse`] reads back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (device, name) = self.name();
        write!(f, "{device}:{name}")
    }
}

/// The `N` of a `#N` name, when below `count`.
fn numbered(name: &str, count: i32) -> Option<i32> {
    let index = name.strip_prefix('#')?.parse::<i32>().ok()?;
    (0..count).contains(&index).then_some(index)
}

/// Run an SDL name lookup on `name` as a C string (`None` for an interior NUL).
fn cstr_lookup<T>(name: &str, lookup: impl FnOnce(*const std::ffi::c_char) -> T) -> Option<T> {
    let name = CString::new(name).ok()?;
    Some(lookup(name.as_ptr()))
}

/// An SDL-owned name, `None` when null or empty.
fn sdl_str(ptr: *const std::ffi::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    (!name.is_empty()).then_some(name)
}

/// Every action's bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bindings {
    sources: [Vec<Source>; ACTIONS.len()],
}

impl Default for Bindings {
    /// The shipped layout: the README's keyboard controls plus a gamepad
    /// layout (d-pad and left stick move, south confirms, east cancels, west
    /// runs, the triggers aim and fire, the shoulders page, start opens the
    /// inventory). No mouse input is bound by default.
    fn default() -> Self {
        use sdl3_sys::gamepad::{
            SDL_GAMEPAD_AXIS_LEFT_TRIGGER, SDL_GAMEPAD_AXIS_LEFTX, SDL_GAMEPAD_AXIS_LEFTY,
            SDL_GAMEPAD_AXIS_RIGHT_TRIGGER, SDL_GAMEPAD_BUTTON_DPAD_DOWN,
            SDL_GAMEPAD_BUTTON_DPAD_LEFT, SDL_GAMEPAD_BUTTON_DPAD_RIGHT,
            SDL_GAMEPAD_BUTTON_DPAD_UP, SDL_GAMEPAD_BUTTON_EAST, SDL_GAMEPAD_BUTTON_LEFT_SHOULDER,
            SDL_GAMEPAD_BUTTON_RIGHT_SHOULDER, SDL_GAMEPAD_BUTTON_SOUTH, SDL_GAMEPAD_BUTTON_START,
            SDL_GAMEPAD_BUTTON_WEST,
        };
        let key = Source::key;
        let pad = |button: SDL_GamepadButton| Source::PadButton(button.0);
        let axis = |axis: SDL_GamepadAxis, positive| Source::PadAxis(axis.0, positive);
        let mut bindings = Self::empty();
        for (action, sources) in [
            (
                Action::Up,
                vec![
                    key(SDL_SCANCODE_UP),
                    pad(SDL_GAMEPAD_BUTTON_DPAD_UP),
                    axis(SDL_GAMEPAD_AXIS_LEFTY, false),
                ],
            ),
            (
                Action::Down,
                vec![
                    key(SDL_SCANCODE_DOWN),
                    pad(SDL_GAMEPAD_BUTTON_DPAD_DOWN),
                    axis(SDL_GAMEPAD_AXIS_LEFTY, true),
                ],
            ),
            (
                Action::Left,
                vec![
                    key(SDL_SCANCODE_LEFT),
                    pad(SDL_GAMEPAD_BUTTON_DPAD_LEFT),
                    axis(SDL_GAMEPAD_AXIS_LEFTX, false),
                ],
            ),
            (
                Action::Right,
                vec![
                    key(SDL_SCANCODE_RIGHT),
                    pad(SDL_GAMEPAD_BUTTON_DPAD_RIGHT),
                    axis(SDL_GAMEPAD_AXIS_LEFTX, true),
                ],
            ),
            (
                Action::Confirm,
                vec![
                    key(SDL_SCANCODE_SPACE),
                    key(SDL_SCANCODE_RETURN),
                    pad(SDL_GAMEPAD_BUTTON_SOUTH),
                ],
            ),
            (
                Action::Cancel,
                vec![
                    key(SDL_SCANCODE_X),
                    key(SDL_SCANCODE_BACKSPACE),
                    key(SDL_SCANCODE_ESCAPE),
                    pad(SDL_GAMEPAD_BUTTON_EAST),
                ],
            ),
            (
                Action::Run,
                vec![
                    key(SDL_SCANCODE_LSHIFT),
                    key(SDL_SCANCODE_RSHIFT),
                    pad(SDL_GAMEPAD_BUTTON_WEST),
                ],
            ),
            (
                Action::Aim,
                vec![
                    key(SDL_SCANCODE_C),
                    axis(SDL_GAMEPAD_AXIS_LEFT_TRIGGER, true),
                ],
            ),
            (
                Action::Fire,
                vec![
                    key(SDL_SCANCODE_Z),
                    axis(SDL_GAMEPAD_AXIS_RIGHT_TRIGGER, true),
                ],
            ),
            (
                Action::Start,
                vec![key(SDL_SCANCODE_TAB), pad(SDL_GAMEPAD_BUTTON_START)],
            ),
            (
                Action::PageLeft,
                vec![
                    key(SDL_SCANCODE_LEFTBRACKET),
                    pad(SDL_GAMEPAD_BUTTON_LEFT_SHOULDER),
                ],
            ),
            (
                Action::PageRight,
                vec![
                    key(SDL_SCANCODE_RIGHTBRACKET),
                    pad(SDL_GAMEPAD_BUTTON_RIGHT_SHOULDER),
                ],
            ),
        ] {
            bindings.sources[action as usize] = sources;
        }
        bindings
    }
}

impl Bindings {
    /// No action bound at all.
    pub fn empty() -> Self {
        Self {
            sources: Default::default(),
        }
    }

    /// The sources bound to `action`, in order.
    pub fn get(&self, action: Action) -> &[Source] {
        &self.sources[action as usize]
    }

    /// Replace `action`'s bindings (duplicates and reserved keys dropped).
    pub fn set(&mut self, action: Action, sources: Vec<Source>) {
        let list = &mut self.sources[action as usize];
        list.clear();
        for source in sources {
            if source.reserved_bit().is_none() && !list.contains(&source) {
                list.push(source);
            }
        }
    }

    /// Bind `source` to `action` at `slot`: replace that binding, or append
    /// when `slot` is past the end. Returns `false` (and changes nothing) for
    /// a reserved input, one that cannot be saved ([`Source::round_trips`])
    /// or a source the action already has.
    pub fn bind(&mut self, action: Action, slot: usize, source: Source) -> bool {
        let list = &mut self.sources[action as usize];
        if source.reserved_bit().is_some() || !source.round_trips() || list.contains(&source) {
            return false;
        }
        match list.get_mut(slot) {
            Some(existing) => *existing = source,
            None => list.push(source),
        }
        true
    }

    /// Remove `action`'s binding at `slot`; `false` when there is none.
    pub fn unbind(&mut self, action: Action, slot: usize) -> bool {
        let list = &mut self.sources[action as usize];
        if slot < list.len() {
            list.remove(slot);
            true
        } else {
            false
        }
    }

    /// The action bits `source` drives (plus the fixed bit of a reserved
    /// key).
    pub fn mask(&self, source: Source) -> u32 {
        let mut mask = source.reserved_bit().unwrap_or(0);
        for action in ACTIONS {
            if self.get(action).contains(&source) {
                mask |= action.bit();
            }
        }
        mask
    }

    /// The first essential action ([`Action::is_essential`]) left without a
    /// keyboard key of its own (bound to no other essential action), or
    /// `None` when every one has one. Such bindings could leave the menus,
    /// the rebind screen included, undrivable: a key shared with cancel
    /// cancels before it confirms, and a gamepad can be unplugged.
    pub fn lockout(&self) -> Option<Action> {
        let essentials = || ACTIONS.into_iter().filter(|action| action.is_essential());
        essentials().find(|&action| {
            !self.get(action).iter().any(|&source| {
                matches!(source, Source::Key(_))
                    && essentials()
                        .all(|other| other == action || !self.get(other).contains(&source))
            })
        })
    }

    /// The other actions `source` is also bound to, besides `action`.
    pub fn conflicts(&self, action: Action, source: Source) -> Vec<Action> {
        ACTIONS
            .into_iter()
            .filter(|&other| other != action && self.get(other).contains(&source))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keyboard_matches_the_readme_controls() {
        let bindings = Bindings::default();
        let keys = |action| -> Vec<String> {
            bindings
                .get(action)
                .iter()
                .filter(|source| matches!(source, Source::Key(_)))
                .map(|source| source.to_string())
                .collect()
        };
        assert_eq!(keys(Action::Up), ["key:Up"]);
        assert_eq!(keys(Action::Confirm), ["key:Space", "key:Return"]);
        assert_eq!(
            keys(Action::Cancel),
            ["key:X", "key:Backspace", "key:Escape"]
        );
        assert_eq!(keys(Action::Run), ["key:Left Shift", "key:Right Shift"]);
        assert_eq!(keys(Action::Aim), ["key:C"]);
        assert_eq!(keys(Action::Fire), ["key:Z"]);
        assert_eq!(keys(Action::Start), ["key:Tab"]);
        assert_eq!(keys(Action::PageLeft), ["key:["]);
        assert_eq!(keys(Action::PageRight), ["key:]"]);
        assert!(
            ACTIONS.iter().all(|&action| bindings
                .get(action)
                .iter()
                .all(|&source| { bindings.conflicts(action, source).is_empty() })),
            "the shipped layout has no conflicts"
        );
    }

    #[test]
    fn every_default_source_name_round_trips() {
        let bindings = Bindings::default();
        for action in ACTIONS {
            for &source in bindings.get(action) {
                let name = source.to_string();
                assert_eq!(Source::parse(&name).unwrap(), source, "{name}");
            }
        }
        for name in [
            "mouse:left",
            "mouse:x2",
            "wheel:up",
            "wheel:right",
            "pad:a",
            "pad:guide",
            "axis:righty-",
            "mouse:6",
        ] {
            assert_eq!(Source::parse(name).unwrap().to_string(), name);
        }
        assert_eq!(
            Source::parse("axis:lefttrigger+").unwrap().label(),
            "AXIS LEFTTRIGGER+"
        );
        assert_eq!(Source::parse("key:Space").unwrap().label(), "SPACE");
    }

    #[test]
    fn bad_and_reserved_names_are_rejected() {
        for name in [
            "Space",
            "key:NoSuchKey",
            "mouse:thumb",
            "wheel:sideways",
            "pad:nope",
            "axis:leftx",
            "axis:nope+",
            "joy:a",
            "key:F1",
            "key:F9",
            "pad:back",
            "key:#0",
            "key:#99999",
            "mouse:0",
            "pad:#99",
        ] {
            assert!(Source::parse(name).is_err(), "{name} must be rejected");
        }
    }

    #[test]
    fn nameless_inputs_are_saved_by_number_or_refused() {
        // Scancode 1 has no SDL name; it is written and read back by number.
        let nameless = Source::Key(1);
        assert_eq!(nameless.to_string(), "key:#1");
        assert!(nameless.round_trips());
        assert_eq!(Source::parse("pad:#0").unwrap(), Source::PadButton(0));
        assert_eq!(
            Source::parse("axis:#1-").unwrap(),
            Source::PadAxis(1, false)
        );
        let mut bindings = Bindings::default();
        assert!(bindings.bind(Action::Aim, usize::MAX, nameless));
        assert!(bindings.bind(Action::Aim, usize::MAX, Source::Mouse(9)));
        // SDL names RETURN2 `Return` too, which would reload as RETURN: such
        // an input is refused rather than saved as a different key.
        let return2 = Source::Key(158);
        assert_eq!(return2.to_string(), "key:Return");
        assert!(!return2.round_trips());
        assert!(!bindings.bind(Action::Aim, usize::MAX, return2));
    }

    #[test]
    fn an_essential_action_needs_a_key_of_its_own() {
        let mut bindings = Bindings::default();
        assert_eq!(bindings.lockout(), None);
        // Gamepad-only confirm: unplugging the pad would strand the menus.
        bindings.set(Action::Confirm, vec![Source::PadButton(0)]);
        assert_eq!(bindings.lockout(), Some(Action::Confirm));
        // A key shared with cancel cancels before it confirms.
        bindings.set(Action::Confirm, vec![Source::key(SDL_SCANCODE_ESCAPE)]);
        assert_eq!(bindings.lockout(), Some(Action::Confirm));
        // Sharing with a non-essential action is fine.
        bindings.set(Action::Confirm, vec![Source::key(SDL_SCANCODE_Z)]);
        assert_eq!(bindings.lockout(), None);
        bindings.set(Action::Start, Vec::new());
        assert_eq!(bindings.lockout(), Some(Action::Start));
    }

    #[test]
    fn one_action_takes_bindings_from_every_device() {
        let mut bindings = Bindings::default();
        let mouse = Source::Mouse(3);
        assert!(bindings.bind(Action::Aim, usize::MAX, mouse));
        assert!(
            !bindings.bind(Action::Aim, usize::MAX, mouse),
            "no duplicates"
        );
        assert_eq!(bindings.get(Action::Aim).len(), 3);
        assert_eq!(bindings.mask(mouse), Action::Aim.bit());
        // The same source on two actions drives both and is a conflict.
        assert!(bindings.bind(Action::Fire, 0, mouse));
        assert_eq!(bindings.mask(mouse), Action::Aim.bit() | Action::Fire.bit());
        assert_eq!(bindings.conflicts(Action::Aim, mouse), [Action::Fire]);
        assert!(
            !bindings
                .get(Action::Fire)
                .contains(&Source::key(SDL_SCANCODE_Z)),
            "slot 0 replaced Z"
        );
        assert!(bindings.unbind(Action::Fire, 0));
        assert!(!bindings.unbind(Action::Fire, 9));
        assert!(!bindings.bind(Action::Fire, 0, Source::key(SDL_SCANCODE_F1)));
        assert_eq!(
            bindings.mask(Source::key(SDL_SCANCODE_F1)),
            DEBUG_MENU_BIT,
            "F1 stays fixed"
        );
        let back = Source::PadButton(SDL_GAMEPAD_BUTTON_BACK.0);
        assert!(!bindings.bind(Action::Fire, 0, back));
        assert_eq!(bindings.mask(back), DEBUG_MENU_BIT, "pad back stays fixed");
    }
}
