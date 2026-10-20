//! The save/load screen's engine side.
//!
//! The screen itself is `games/re1/ui/save_load.lua` (a port of the
//! original's per-tick state machine), or the pack entry a mod's scripts
//! registered with `api:register_save_screen`. Both run as a
//! [`LuaScreen`] in load or save mode: the title's load path opens it in
//! load mode, and a room typewriter (or the next-cycle ending save) opens it
//! in save mode over the frozen gameplay frame.

use anyhow::Result;

use crate::game::InventoryItem;
use crate::items;
use crate::save::{PLAYER_SLOT_COUNT, SaveFile};

use super::Screen;
use super::UiContext;
use super::lua_screen::LuaScreen;

/// The shipped save/load screen script.
const SAVE_LOAD_ENTRY: &str = "ui/save_load.lua";

/// A completed save: the slot that was written and whether the flow consumed
/// an ink ribbon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveOutcome {
    /// The zero-based slot `api:save_slot` wrote; `None` when the screen
    /// named its own file with `api:save`.
    pub slot: Option<usize>,
    /// Whether one ink ribbon must be consumed from the live game state.
    pub ink_ribbon: bool,
}

/// A save/load screen the engine can open.
///
/// The screen writes (or loads) its files itself and reports a completed
/// write through [`SaveScreen::take_outcome`]; the engine mirrors the write
/// into the live state (ink ribbon, save counter).
pub trait SaveScreen: Screen {
    /// The completed save, once the screen left through a write.
    fn take_outcome(&mut self) -> Option<SaveOutcome>;
}

/// Open the load screen: the pack entry its scripts registered, or the
/// shipped screen. A registered screen that cannot load falls back to the
/// shipped one with a warning, so a broken mod never leaves the player
/// without a screen.
pub fn open_load_screen(
    entry: Option<&str>,
    cx: &mut UiContext<'_>,
) -> Result<Box<dyn SaveScreen>> {
    if let Some(entry) = entry {
        match LuaScreen::open_load(entry, cx) {
            Ok(screen) => return Ok(Box::new(screen)),
            Err(err) => eprintln!("warning: lua save screen {entry} unavailable: {err:#}"),
        }
    }
    Ok(Box::new(LuaScreen::open_load(SAVE_LOAD_ENTRY, cx)?))
}

/// Open the save screen for the pending `file`: the pack entry its scripts
/// registered, or the shipped screen. `ink_ribbon` makes the write consume
/// one.
pub fn open_save_screen(
    entry: Option<&str>,
    cx: &mut UiContext<'_>,
    file: SaveFile,
    ink_ribbon: bool,
) -> Result<Box<dyn SaveScreen>> {
    if let Some(entry) = entry {
        match LuaScreen::open_save(entry, cx, file.clone(), ink_ribbon) {
            Ok(screen) => return Ok(Box::new(screen)),
            Err(err) => eprintln!("warning: lua save screen {entry} unavailable: {err:#}"),
        }
    }
    Ok(Box::new(LuaScreen::open_save(
        SAVE_LOAD_ENTRY,
        cx,
        file,
        ink_ribbon,
    )?))
}

/// Remove one ink ribbon from the block the screen is about to write, the way
/// the original's `use_room_action_item` does before assembling the file.
pub(crate) fn consume_snapshot_ribbon(file: &mut SaveFile) {
    let Some(index) = file
        .player_slots
        .iter()
        .position(|stack| stack.id == items::ITEM_INK_RIBBONS && stack.quantity > 0)
    else {
        return;
    };
    file.used_item = items::ITEM_INK_RIBBONS;
    file.player_slots[index].quantity -= 1;
    if file.player_slots[index].quantity == 0 {
        for read in index + 1..PLAYER_SLOT_COUNT {
            file.player_slots[read - 1] = file.player_slots[read];
        }
        file.player_slots[PLAYER_SLOT_COUNT - 1] = InventoryItem::default();
        file.total_held = file.total_held.saturating_sub(1);
    }
}
