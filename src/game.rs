//! Game state driven by the room's SCD scripts.
//!
//! The command and event VMs dispatch every observable effect to
//! [`ScdGameHost`], which owns this milestone's state: the flag banks and
//! BioCard-like state block that conditions read, the camera cut and lock, the
//! active message, the room's BGM requests, the inventory and the room action
//! table (doors, item pickups, item boxes, events and typewriters).
//!
//! The room action table is the interaction layer: scripts register zones with
//! `door_aot_set`, `aot_set` and `item_aot_set`, and each tick the engine hands
//! the player position and the action key to [`GameState::interact`]. Items and
//! doors act for real; the menu-driven kinds record a placeholder interaction.
//! Entities, models and effects stay recorded placeholders until their systems
//! exist.

use std::collections::BTreeMap;

use crate::scd::host::{ScdHost, StepResult};
use crate::scd::ir::Operand;
use crate::scd::opcode::Op;
use crate::state::{RoomId, RoomState};

/// Number of flag banks the scripts can address.
pub const FLAG_BANK_COUNT: usize = 10;
/// Bytes per flag bank.
pub const FLAG_BANK_BYTES: usize = 32;
/// Bytes in the BioCard-like state block.
pub const STATE_BYTES: usize = 64;
/// Words in the fading-state block.
pub const STATE_WORDS: usize = 32;
/// Number of room action slots the game state tracks.
pub const ROOM_ACTION_SLOTS: usize = 20;
/// Placeholder message id displayed by a door that refuses to open.
pub const LOCKED_MESSAGE: u8 = 200;
/// `room_check_actions` index of the item pickup handler.
const HANDLER_ITEM: u8 = 4;
/// `room_check_actions` index of the key-pickup handler.
const HANDLER_PICKUP_KEY: u8 = 15;
/// `room_check_actions` index of the door handler.
const HANDLER_DOOR: u8 = 1;
/// `room_check_actions` index of the message handler.
const HANDLER_MESSAGE: u8 = 2;
/// `room_check_actions` index of the item-box handler.
const HANDLER_ITEMBOX: u8 = 8;
/// `room_check_actions` index of the room-event handler.
const HANDLER_EVENT: u8 = 9;
/// `room_check_actions` index of the typewriter handler.
const HANDLER_TYPEWRITER: u8 = 16;
/// `main_state_flags` bit 0x400, raised when `give_item` runs.
const MSF_MENU_GOT_ITEM: u8 = 21;
/// `main_state_flags` bit 0x800, toggled by `give_item` (item viewer).
const MSF_MENU_ITEM_VIEW: u8 = 20;

/// One MSB-first flag bank.
///
/// A selector byte chooses a little-endian dword by its high three bits and a
/// bit inside that dword by its low five bits, where bit 0 is the dword's most
/// significant bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlagBank([u8; FLAG_BANK_BYTES]);

impl FlagBank {
    /// A cleared bank.
    pub const fn new() -> Self {
        Self([0; FLAG_BANK_BYTES])
    }

    /// One raw byte of the bank.
    pub fn byte(&self, offset: usize) -> u8 {
        self.0.get(offset).copied().unwrap_or(0)
    }

    /// Whether the bit selected by `sel` is set.
    pub fn bit(&self, sel: u8) -> bool {
        let (offset, mask) = Self::target(sel);
        let value = self.dword(offset);
        value & mask != 0
    }

    /// Apply set (mode 0), clear (mode 1) or toggle (mode 2) to the bit
    /// selected by `sel`. Returns `false` for an unknown mode.
    pub fn apply(&mut self, sel: u8, mode: u8) -> bool {
        let (offset, mask) = Self::target(sel);
        let mut value = self.dword(offset);
        match mode {
            0 => value |= mask,
            1 => value &= !mask,
            2 => value ^= mask,
            _ => return false,
        }
        self.0[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        true
    }

    fn target(sel: u8) -> (usize, u32) {
        let offset = usize::from((sel & 0xE0) >> 3);
        let index = u32::from(sel & 0x1F);
        (offset, 0x8000_0000u32 >> index)
    }

    fn dword(&self, offset: usize) -> u32 {
        u32::from_le_bytes([
            self.0[offset],
            self.0[offset + 1],
            self.0[offset + 2],
            self.0[offset + 3],
        ])
    }
}

/// Camera cut state driven by `cutnext`/`cutcurr`/`cut_auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CameraState {
    /// The cut the host last selected.
    pub current_cut: usize,
    /// The cut saved by `cutnext` for a later `cutcurr`.
    pub saved_cut: Option<usize>,
    /// Whether the scripts own the cut instead of the position zones.
    pub locked: bool,
}

/// The message currently being displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageState {
    /// Message table id, or `None` when no message was requested.
    pub id: Option<u8>,
    /// Pause word passed with the message.
    pub pause: u16,
    /// Whether a message is being displayed.
    pub active: bool,
}

/// BGM channel state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BgmState {
    /// The BGM state byte (`1 << (channel + 3)` per playing channel).
    pub state: u8,
    /// Whether each of the three BGM channels is playing.
    pub channels: [bool; 3],
}

/// One inventory stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryItem {
    /// Item id.
    pub id: u8,
    /// How many are held.
    pub quantity: u8,
}

/// A BGM request queued for the engine to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BgmRequest {
    /// BGM channel the request targets.
    pub channel: u8,
    /// `true` starts the room track, `false` stops it.
    pub start: bool,
}

/// What a room action does when the player triggers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomActionKind {
    /// A door registered by `door_aot_set`.
    Door,
    /// A pickable item registered by `item_aot_set`.
    Item,
    /// An item box (`SCE_ITEMBOX`).
    ItemBox,
    /// A room event trigger (`SCE_EVENT`).
    Event,
    /// A message trigger (`SCE_MESSAGE`).
    Message,
    /// A typewriter (`SCE_TYPEWRITER`).
    Typewriter,
    /// Anything else; stores its parameters but does not act yet.
    Other,
}

impl RoomActionKind {
    /// Classify the SCE/handler byte of a generic `aot_set`.
    fn from_sce(sce: u8) -> Self {
        match sce {
            HANDLER_DOOR => Self::Door,
            HANDLER_ITEM | HANDLER_PICKUP_KEY => Self::Item,
            HANDLER_ITEMBOX => Self::ItemBox,
            HANDLER_EVENT => Self::Event,
            HANDLER_MESSAGE => Self::Message,
            HANDLER_TYPEWRITER => Self::Typewriter,
            _ => Self::Other,
        }
    }
}

/// One entry of the room action table.
///
/// `params` is opaque per kind:
/// - generic actions keep `[handler, flags, word0 (LE), word1, word2]`;
/// - doors keep `[lock, next_room, key, sub_type, open, pad0, latch, door_type]`;
/// - items keep `[item, quantity, model, sca_parent, x (LE), z (LE)]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomAction {
    /// Room action slot.
    pub slot: u8,
    /// What triggering the action does.
    pub kind: RoomActionKind,
    /// Interaction box: `x`, `z`, `width`, `depth`.
    pub zone: [i16; 4],
    /// SCE byte the handler was derived from.
    pub sce: u8,
    /// Room action handler index; `0` is inert.
    pub handler: u8,
    /// Probe flags byte.
    pub flags: u8,
    /// Raw parameter bytes.
    pub params: [u8; 8],
}

impl RoomAction {
    /// The original's unsigned box test: coordinates left of or above the box
    /// wrap to a huge value and fail, so the box only extends towards +X/+Z.
    pub fn contains(&self, x: i32, z: i32) -> bool {
        let [origin_x, origin_z, width, depth] = self.zone;
        (x as u32).wrapping_sub(u32::from(origin_x as u16)) <= u32::from(width as u16)
            && (z as u32).wrapping_sub(u32::from(origin_z as u16)) <= u32::from(depth as u16)
    }

    /// Parameter word `index` (0-2) of a generic action.
    pub fn param_word(&self, index: usize) -> u16 {
        let lo = self.params.get(2 + index * 2).copied().unwrap_or(0);
        let hi = self.params.get(3 + index * 2).copied().unwrap_or(0);
        u16::from_le_bytes([lo, hi])
    }

    /// Item id of an item action.
    pub fn item_id(&self) -> u8 {
        self.params[0]
    }

    /// Quantity of an item action.
    pub fn item_quantity(&self) -> u8 {
        self.params[1]
    }

    /// Model slot of an item action.
    pub fn item_model(&self) -> u8 {
        self.params[2]
    }

    /// Item world position. The record's Y is not retained this slice.
    pub fn item_position(&self) -> [i16; 3] {
        let x = i16::from_le_bytes([self.params[4], self.params[5]]);
        let z = i16::from_le_bytes([self.params[6], self.params[7]]);
        [x, 0, z]
    }
}

/// A door record built by `door_aot_set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Door {
    /// Room action slot.
    pub slot: u8,
    /// Interaction box: `x`, `z`, `width`, `depth`.
    pub zone: [i16; 4],
    /// Room number within the current stage the door leads to.
    pub next_room: u8,
    /// Spawn position in the target room.
    pub next_pos: [i16; 3],
    /// Spawn facing in the target room.
    pub next_angle: i16,
    /// Lock descriptor: bit `0x80` start locked, bit `0x40` one character,
    /// low six bits the lock flag index in bank 2.
    pub lock: u8,
    /// Item needed to open the door.
    pub key: u8,
    /// Sub-type byte (the door entry's probe flags).
    pub sub_type: u8,
}

/// A room change requested by a door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomTransition {
    /// Room to load.
    pub target: RoomId,
    /// Player spawn position in the target room.
    pub pos: [i32; 3],
    /// Player spawn facing in the target room.
    pub angle: u16,
}

/// One interaction recorded by the room action layer for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomInteraction {
    /// Room action slot that fired.
    pub slot: u8,
    /// Action kind.
    pub kind: RoomActionKind,
    /// Message id when the interaction opens a message.
    pub message: Option<u16>,
}

/// The game state the SCD scripts read and write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameState {
    /// The room this state belongs to.
    pub id: RoomId,
    /// The ten flag banks addressed by `ck`/`set`.
    pub flags: [FlagBank; FLAG_BANK_COUNT],
    /// BioCard-like state bytes (0 stage, 1 room, 2 player flag).
    pub state_bytes: [u8; STATE_BYTES],
    /// Fading-state words.
    pub state_words: [u16; STATE_WORDS],
    /// Camera cut state.
    pub camera: CameraState,
    /// Message state.
    pub message: MessageState,
    /// BGM state.
    pub bgm: BgmState,
    /// The player's inventory.
    pub inventory: Vec<InventoryItem>,
    /// The room's action table.
    pub room_actions: [Option<RoomAction>; ROOM_ACTION_SLOTS],
    /// Door records, one per door action slot.
    pub doors: [Option<Door>; ROOM_ACTION_SLOTS],
    /// The last item picked up by any path.
    pub last_picked_item: Option<u8>,
    /// The last item used from a menu.
    pub last_used_item: Option<u8>,
    /// The item currently equipped.
    pub equipped: Option<u8>,
    /// A room change requested by a door.
    pub transition: Option<RoomTransition>,
    /// Item ids picked up this room, in order, for tests.
    pub item_events: Vec<u8>,
    /// The last placeholder interaction recorded by the action layer.
    pub last_interaction: Option<RoomInteraction>,
    /// Fixed ticks elapsed.
    pub frame: u64,
    /// Call counts of opcodes whose systems do not exist yet.
    pub placeholders: BTreeMap<u8, u64>,
    /// BGM requests produced by the scripts.
    pub room_bgm_requests: Vec<BgmRequest>,
    /// Event scripts requested by `evt_exec`, consumed by the engine's event VM.
    pub pending_events: Vec<(u8, u8)>,
}

impl Default for GameState {
    fn default() -> Self {
        Self {
            id: RoomId::default(),
            flags: [FlagBank::default(); FLAG_BANK_COUNT],
            state_bytes: [0; STATE_BYTES],
            state_words: [0; STATE_WORDS],
            camera: CameraState::default(),
            message: MessageState::default(),
            bgm: BgmState::default(),
            inventory: Vec::new(),
            room_actions: [None; ROOM_ACTION_SLOTS],
            doors: [None; ROOM_ACTION_SLOTS],
            last_picked_item: None,
            last_used_item: None,
            equipped: None,
            transition: None,
            item_events: Vec::new(),
            last_interaction: None,
            frame: 0,
            placeholders: BTreeMap::new(),
            room_bgm_requests: Vec::new(),
            pending_events: Vec::new(),
        }
    }
}

impl GameState {
    /// Create the state for `id`, seeding the identity bytes from the room id.
    pub fn new(id: RoomId, _room: &RoomState) -> Self {
        let mut state = Self {
            id,
            ..Self::default()
        };
        state.state_bytes[0] = id.stage;
        state.state_bytes[1] = id.room;
        state.state_bytes[2] = id.player_flag;
        state
    }

    /// The `ck` condition: true when the selected bit differs from `expected`.
    /// An out-of-range bank is false, matching the original handler.
    pub fn flag_test(&self, bank: u8, sel: u8, expected: bool) -> bool {
        match self.flags.get(usize::from(bank)) {
            Some(bank) => bank.bit(sel) != expected,
            None => false,
        }
    }

    /// Apply a `set` operation to a flag bank. `false` for a bad bank or mode.
    pub fn apply_flag(&mut self, bank: u8, sel: u8, mode: u8) -> bool {
        match self.flags.get_mut(usize::from(bank)) {
            Some(bank) => bank.apply(sel, mode),
            None => false,
        }
    }

    /// The `cmpb` condition on `state_bytes[index]`.
    pub fn compare_byte(&self, index: u8, mode: u8, value: u8) -> bool {
        let state = self
            .state_bytes
            .get(usize::from(index))
            .copied()
            .unwrap_or(0);
        compare(mode, i64::from(state), i64::from(value))
    }

    /// The `cmpw` condition on `state_words[index]`, comparing unsigned words.
    pub fn compare_word(&self, index: u8, mode: u8, value: i16) -> bool {
        let state = self
            .state_words
            .get(usize::from(index))
            .copied()
            .unwrap_or(0);
        compare(mode, i64::from(state), i64::from(value as u16))
    }

    /// Write one `setb` state byte. Out-of-range indices are dropped.
    pub fn set_byte(&mut self, index: u8, value: u8) {
        if let Some(slot) = self.state_bytes.get_mut(usize::from(index)) {
            *slot = value;
        }
    }

    /// Write one `setw` state word. Out-of-range indices are dropped.
    pub fn set_word(&mut self, index: u8, value: u16) {
        if let Some(slot) = self.state_words.get_mut(usize::from(index)) {
            *slot = value;
        }
    }

    /// Count one execution of a not-yet-implemented opcode.
    pub fn record_placeholder(&mut self, op: u8) {
        *self.placeholders.entry(op).or_insert(0) += 1;
    }

    /// Count one fixed tick.
    pub fn advance_frame(&mut self) {
        self.frame = self.frame.saturating_add(1);
    }

    /// Move into `id`: the flags, state blocks and inventory survive a room
    /// change; the room action table, message, camera and per-room requests do
    /// not. The identity bytes are reseeded from the new room.
    pub fn enter_room(&mut self, id: RoomId, _room: &RoomState) {
        self.id = id;
        self.state_bytes[0] = id.stage;
        self.state_bytes[1] = id.room;
        self.state_bytes[2] = id.player_flag;
        self.room_actions = [None; ROOM_ACTION_SLOTS];
        self.doors = [None; ROOM_ACTION_SLOTS];
        self.transition = None;
        self.message = MessageState::default();
        self.camera = CameraState::default();
        self.last_interaction = None;
        self.room_bgm_requests.clear();
        self.pending_events.clear();
    }

    /// Add `quantity` of `item`, merging into an existing stack.
    pub fn add_item(&mut self, item: u8, quantity: u8) {
        if let Some(stack) = self.inventory.iter_mut().find(|stack| stack.id == item) {
            stack.quantity = stack.quantity.saturating_add(quantity);
        } else {
            self.inventory.push(InventoryItem { id: item, quantity });
        }
    }

    /// Whether at least one `item` is held.
    pub fn has_item(&self, item: u8) -> bool {
        self.inventory
            .iter()
            .any(|stack| stack.id == item && stack.quantity > 0)
    }

    /// How many `item` are held.
    pub fn item_count(&self, item: u8) -> u32 {
        self.inventory
            .iter()
            .filter(|stack| stack.id == item)
            .map(|stack| u32::from(stack.quantity))
            .sum()
    }

    /// Remove one `item`, reporting whether it was held.
    pub fn remove_item(&mut self, item: u8) -> bool {
        let Some(index) = self
            .inventory
            .iter()
            .position(|stack| stack.id == item && stack.quantity > 0)
        else {
            return false;
        };
        self.inventory[index].quantity -= 1;
        if self.inventory[index].quantity == 0 {
            self.inventory.remove(index);
        }
        true
    }

    /// `ck_item_count`: no item family table exists yet, so only the exact item
    /// id is matched. Returns the summed quantity and the matching stack count.
    pub fn item_family_total(&self, search: u8) -> (u32, u32) {
        let count = self
            .inventory
            .iter()
            .filter(|stack| stack.id == search)
            .count() as u32;
        (self.item_count(search), count)
    }

    /// Probe the room action table for `pos`; only acts when the action key is
    /// held. Item and door actions act for real, the menu-driven kinds record a
    /// placeholder interaction.
    pub fn interact(&mut self, pos: [i32; 3], action: bool) {
        if !action {
            return;
        }
        for slot in 0..ROOM_ACTION_SLOTS {
            let Some(room_action) = self.room_actions[slot] else {
                continue;
            };
            if room_action.handler == 0 || !room_action.contains(pos[0], pos[2]) {
                continue;
            }
            match room_action.kind {
                RoomActionKind::Door => {
                    self.try_door(room_action.slot);
                }
                RoomActionKind::Item => {
                    self.pick_up(room_action.slot);
                }
                RoomActionKind::Message => {
                    let id = room_action.param_word(0);
                    self.show_message(id as u8, room_action.param_word(1));
                    self.record_interaction(room_action.slot, room_action.kind, Some(id));
                }
                RoomActionKind::ItemBox
                | RoomActionKind::Event
                | RoomActionKind::Typewriter
                | RoomActionKind::Other => {
                    self.record_interaction(room_action.slot, room_action.kind, None);
                }
            }
            if self.transition.is_some() {
                break;
            }
        }
    }

    /// Run one room action handler by index, as `aot_on` and `give_item` do.
    ///
    /// Handler `0` and any unimplemented handler are inert. Item handlers pick
    /// the action up, the door handler transitions, the message handler shows
    /// its message, and the menu-driven handlers record a placeholder.
    pub fn run_room_action(&mut self, slot: u8, handler: u8) -> bool {
        let Some(action) = self.room_actions.get(usize::from(slot)).copied().flatten() else {
            return false;
        };
        match handler {
            HANDLER_DOOR => self.try_door(slot),
            HANDLER_ITEM | HANDLER_PICKUP_KEY if action.kind == RoomActionKind::Item => {
                self.pick_up(slot)
            }
            HANDLER_MESSAGE => {
                let id = action.param_word(0);
                self.show_message(id as u8, action.param_word(1));
                self.record_interaction(slot, RoomActionKind::Message, Some(id));
                true
            }
            HANDLER_ITEMBOX | HANDLER_EVENT | HANDLER_TYPEWRITER => {
                self.record_interaction(slot, action.kind, None);
                true
            }
            _ => false,
        }
    }

    /// Pick up the item action in `slot`: add to the inventory, record it and
    /// consume the action.
    pub fn pick_up(&mut self, slot: u8) -> bool {
        let Some(action) = self.room_actions.get(usize::from(slot)).copied().flatten() else {
            return false;
        };
        if action.kind != RoomActionKind::Item {
            return false;
        }
        let item = action.item_id();
        self.add_item(item, action.item_quantity().max(1));
        self.last_picked_item = Some(item);
        self.item_events.push(item);
        self.room_actions[usize::from(slot)] = None;
        self.doors[usize::from(slot)] = None;
        true
    }

    /// Run the door in `slot`: transition when open, otherwise show the locked
    /// placeholder. Uses the lock flag bank (bank 2) as the original does.
    pub fn try_door(&mut self, slot: u8) -> bool {
        let Some(door) = self.doors.get(usize::from(slot)).copied().flatten() else {
            return false;
        };
        if self.door_locked(&door) {
            self.show_message(LOCKED_MESSAGE, 0xFF);
            return false;
        }
        if door.next_room == 0xFF || door.next_room > RoomId::MAX_ROOM {
            return false;
        }
        self.transition = Some(RoomTransition {
            target: RoomId {
                stage: self.id.stage,
                room: door.next_room,
                player_flag: self.id.player_flag,
            },
            pos: [
                i32::from(door.next_pos[0]),
                i32::from(door.next_pos[1]),
                i32::from(door.next_pos[2]),
            ],
            angle: door.next_angle as u16 & 0x0FFF,
        });
        true
    }

    fn door_locked(&self, door: &Door) -> bool {
        door.lock & 0x80 != 0 && !self.flag_test(2, door.lock & 0x3F, false)
    }

    fn show_message(&mut self, id: u8, pause: u16) {
        self.message = MessageState {
            id: Some(id),
            pause,
            active: true,
        };
    }

    fn record_interaction(&mut self, slot: u8, kind: RoomActionKind, message: Option<u16>) {
        self.last_interaction = Some(RoomInteraction {
            slot,
            kind,
            message,
        });
    }
}

/// Compare `state` against `value` with the shared condition mode table.
fn compare(mode: u8, state: i64, value: i64) -> bool {
    match mode {
        0 => value == state,
        1 => state > value,
        2 => state >= value,
        3 => state < value,
        4 => state <= value,
        5 => value != state,
        _ => false,
    }
}

/// The [`ScdHost`] implementation backed by a [`GameState`].
pub struct ScdGameHost<'a> {
    state: &'a mut GameState,
}

impl<'a> ScdGameHost<'a> {
    /// Drive `state`.
    pub fn new(state: &'a mut GameState) -> Self {
        Self { state }
    }

    /// The state being driven.
    pub fn state(&self) -> &GameState {
        self.state
    }

    /// The state being driven, mutably.
    pub fn state_mut(&mut self) -> &mut GameState {
        self.state
    }

    /// The engine's interaction check: probe the room action table at `pos`
    /// when the action key is held.
    pub fn interact(&mut self, pos: [i32; 3], action: bool) {
        self.state.interact(pos, action);
    }

    fn placeholder(&mut self, op: &Op) -> StepResult {
        self.state.record_placeholder(op.op);
        StepResult::Placeholder
    }

    fn equipped_test(&self, operands: &[Operand]) -> StepResult {
        let item = operand_u8(operands, 0);
        condition_result(self.state.equipped == Some(item))
    }

    fn store_action(&mut self, action: RoomAction) {
        if let Some(slot) = self.state.room_actions.get_mut(usize::from(action.slot)) {
            *slot = Some(action);
        }
    }

    fn store_door(&mut self, door: Door) {
        if let Some(slot) = self.state.doors.get_mut(usize::from(door.slot)) {
            *slot = Some(door);
        }
    }
}

/// Build the generic action parameter block: handler, flags and three words.
fn action_params(handler: u8, flags: u8, operands: &[Operand], word_start: usize) -> [u8; 8] {
    let mut params = [0u8; 8];
    params[0] = handler;
    params[1] = flags;
    for index in 0..3 {
        let bytes = operand_u16(operands, word_start + index).to_le_bytes();
        params[2 + index * 2] = bytes[0];
        params[3 + index * 2] = bytes[1];
    }
    params
}

impl ScdHost for ScdGameHost<'_> {
    fn on_flow(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            0x0E | 0x32 => StepResult::Continue,
            0x10 => {
                let item = operand_u8(operands, 0);
                condition_result(self.state.last_used_item == Some(item))
            }
            0x11 => {
                let item = operand_u8(operands, 0);
                condition_result(self.state.last_picked_item == Some(item))
            }
            0x1A => condition_result(self.state.has_item(operand_u8(operands, 0))),
            0x1D => self.equipped_test(operands),
            0x22 => {
                let search = operand_u8(operands, 0);
                let mode = operand_u8(operands, 1);
                let value = operand_u8(operands, 2);
                let (total, stacks) = self.state.item_family_total(search);
                condition_result(stacks > 0 && compare(mode, i64::from(total), i64::from(value)))
            }
            _ => self.placeholder(op),
        }
    }

    fn on_flags(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            0x04 => {
                let bank = operand_u8(operands, 0);
                let sel = operand_u8(operands, 1);
                let expected = operand_u8(operands, 2) != 0;
                condition_result(self.state.flag_test(bank, sel, expected))
            }
            0x05 => {
                let bank = operand_u8(operands, 0);
                let sel = operand_u8(operands, 1);
                let mode = operand_u8(operands, 2);
                if self.state.apply_flag(bank, sel, mode) {
                    StepResult::Continue
                } else {
                    self.placeholder(op)
                }
            }
            0x06 => {
                let index = operand_u8(operands, 0);
                let mode = operand_u8(operands, 1);
                let value = operand_u8(operands, 2);
                condition_result(self.state.compare_byte(index, mode, value))
            }
            0x07 => {
                let index = operand_u8(operands, 1);
                let mode = operand_u8(operands, 2);
                let value = operand_i16(operands, 3);
                condition_result(self.state.compare_word(index, mode, value))
            }
            0x08 => {
                self.state
                    .set_byte(operand_u8(operands, 0), operand_u8(operands, 1));
                StepResult::Continue
            }
            0x31 => {
                self.state
                    .set_word(operand_u8(operands, 0), operand_u16(operands, 1));
                StepResult::Continue
            }
            _ => self.placeholder(op),
        }
    }

    fn on_camera(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            0x09 => {
                self.state.camera.saved_cut = Some(self.state.camera.current_cut);
                self.state.camera.current_cut = usize::from(operand_u8(operands, 0));
                self.state.camera.locked = true;
                StepResult::Continue
            }
            0x0A => {
                if let Some(saved) = self.state.camera.saved_cut.take() {
                    self.state.camera.current_cut = saved;
                }
                self.state.camera.locked = false;
                StepResult::Continue
            }
            0x23 => {
                self.state.camera.locked = operand_u8(operands, 0) != 0;
                StepResult::Continue
            }
            // This slice maps 0x1C to the equipped item test alongside 0x1D;
            // the real 0x1C (room light fade setup) stays unimplemented.
            0x1C => self.equipped_test(operands),
            _ => self.placeholder(op),
        }
    }

    fn on_message(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            0x0B => {
                self.state.message.id = Some(operand_u8(operands, 0));
                self.state.message.pause = operand_u16(operands, 1);
                self.state.message.active = true;
                StepResult::Continue
            }
            _ => self.placeholder(op),
        }
    }

    fn on_room_action(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            0x0C => {
                let slot = operand_u8(operands, 0);
                let zone = [
                    operand_i16(operands, 1),
                    operand_i16(operands, 2),
                    operand_i16(operands, 3),
                    operand_i16(operands, 4),
                ];
                let door = Door {
                    slot,
                    zone,
                    next_room: operand_u8(operands, 10),
                    next_pos: [
                        operand_i16(operands, 11),
                        operand_i16(operands, 12),
                        operand_i16(operands, 13),
                    ],
                    next_angle: operand_i16(operands, 14),
                    lock: operand_u8(operands, 9),
                    key: operand_u8(operands, 15),
                    sub_type: operand_u8(operands, 16),
                };
                let action = RoomAction {
                    slot,
                    kind: RoomActionKind::Door,
                    zone,
                    sce: HANDLER_DOOR,
                    handler: HANDLER_DOOR,
                    flags: door.sub_type,
                    params: [
                        door.lock,
                        door.next_room,
                        door.key,
                        door.sub_type,
                        operand_u8(operands, 5),
                        operand_u8(operands, 6),
                        operand_u8(operands, 7),
                        operand_u8(operands, 8),
                    ],
                };
                self.store_action(action);
                self.store_door(door);
                StepResult::Continue
            }
            0x0D => {
                let slot = operand_u8(operands, 0);
                let handler = operand_u8(operands, 5);
                let flags = operand_u8(operands, 6);
                let zone = [
                    operand_i16(operands, 1),
                    operand_i16(operands, 2),
                    operand_i16(operands, 3),
                    operand_i16(operands, 4),
                ];
                let params = action_params(handler, flags, operands, 7);
                let action = RoomAction {
                    slot,
                    kind: RoomActionKind::from_sce(handler),
                    zone,
                    sce: handler,
                    handler,
                    flags,
                    params,
                };
                self.store_action(action);
                StepResult::Continue
            }
            0x12 => {
                let slot = operand_u8(operands, 0);
                let handler = operand_u8(operands, 1);
                let flags = operand_u8(operands, 2);
                if let Some(action) = self
                    .state
                    .room_actions
                    .get_mut(usize::from(slot))
                    .and_then(Option::as_mut)
                {
                    action.handler = handler;
                    action.flags = flags;
                    action.params = action_params(handler, flags, operands, 3);
                }
                StepResult::Continue
            }
            0x13 => {
                let slot = operand_u8(operands, 0);
                let handler = operand_u8(operands, 1);
                let flags = operand_u8(operands, 2);
                if let Some(action) = self
                    .state
                    .room_actions
                    .get_mut(usize::from(slot))
                    .and_then(Option::as_mut)
                {
                    action.handler = handler;
                    action.flags = flags;
                    action.params[0] = handler;
                    action.params[1] = flags;
                }
                StepResult::Continue
            }
            0x24 => {
                self.state
                    .run_room_action(operand_u8(operands, 0), operand_u8(operands, 1));
                StepResult::Continue
            }
            0x2D => {
                let slot = operand_u8(operands, 0);
                let handler = operand_u8(operands, 1);
                self.state.run_room_action(slot, handler);
                if self
                    .state
                    .room_actions
                    .get(usize::from(slot))
                    .is_some_and(|action| {
                        action.is_some_and(|action| action.kind == RoomActionKind::Item)
                    })
                {
                    self.state.pick_up(slot);
                }
                self.state.apply_flag(5, MSF_MENU_GOT_ITEM, 0);
                self.state.apply_flag(5, MSF_MENU_ITEM_VIEW, 2);
                StepResult::Finished
            }
            _ => self.placeholder(op),
        }
    }

    fn on_item(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            0x18 => {
                let slot = operand_u8(operands, 0);
                let item = operand_u8(operands, 5);
                let quantity = operand_u8(operands, 6);
                let model = operand_u8(operands, 7);
                let parent = operand_u8(operands, 8);
                let x = operand_i16(operands, 9).to_le_bytes();
                let z = operand_i16(operands, 11).to_le_bytes();
                let action = RoomAction {
                    slot,
                    kind: RoomActionKind::Item,
                    zone: [
                        operand_i16(operands, 1),
                        operand_i16(operands, 2),
                        operand_i16(operands, 3),
                        operand_i16(operands, 4),
                    ],
                    sce: HANDLER_ITEM,
                    handler: HANDLER_ITEM,
                    flags: operand_u8(operands, 14),
                    params: [item, quantity, model, parent, x[0], x[1], z[0], z[1]],
                };
                self.store_action(action);
                StepResult::Continue
            }
            0x2C => {
                let removed = self.state.remove_item(operand_u8(operands, 0));
                condition_result(removed)
            }
            0x1C => self.equipped_test(operands),
            0x4C => self.placeholder(op),
            _ => self.placeholder(op),
        }
    }

    fn on_enemy(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        self.placeholder(op)
    }

    fn on_player(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        self.placeholder(op)
    }

    fn on_model(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        self.placeholder(op)
    }

    fn on_effect(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        self.placeholder(op)
    }

    fn on_sound(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            0x15 => {
                let channel = operand_u8(operands, 0);
                self.state.bgm.state |= channel_bit(channel);
                if let Some(flag) = self.state.bgm.channels.get_mut(usize::from(channel)) {
                    *flag = true;
                }
                self.state.room_bgm_requests.push(BgmRequest {
                    channel,
                    start: true,
                });
                StepResult::Continue
            }
            0x16 => {
                let channel = operand_u8(operands, 0);
                self.state.bgm.state &= !channel_bit(channel);
                if let Some(flag) = self.state.bgm.channels.get_mut(usize::from(channel)) {
                    *flag = false;
                }
                self.state.room_bgm_requests.push(BgmRequest {
                    channel,
                    start: false,
                });
                StepResult::Continue
            }
            0x37 => {
                self.state.bgm.state = operand_u8(operands, 2);
                StepResult::Continue
            }
            _ => self.placeholder(op),
        }
    }

    fn on_misc(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            // evt_exec: pad, slot, script index. `slot` 8 or more means "first
            // free slot"; the engine maps that when it starts the event.
            0x14 => {
                let slot = operand_u8(operands, 1);
                let event = operand_u8(operands, 2);
                self.state.pending_events.push((slot, event));
                StepResult::Continue
            }
            _ => self.placeholder(op),
        }
    }

    fn flag_test(&mut self, bank: u8, bit: u8, expected: bool) -> bool {
        self.state.flag_test(bank, bit, expected)
    }
}

/// The BGM state bit for `channel`, or zero when the channel is out of range.
fn channel_bit(channel: u8) -> u8 {
    1u8.checked_shl(u32::from(channel) + 3).unwrap_or(0)
}

fn condition_result(value: bool) -> StepResult {
    if value {
        StepResult::Continue
    } else {
        StepResult::Finished
    }
}

fn operand_u8(operands: &[Operand], index: usize) -> u8 {
    operands.get(index).map_or(0, |operand| operand.value as u8)
}

fn operand_u16(operands: &[Operand], index: usize) -> u16 {
    operands
        .get(index)
        .map_or(0, |operand| operand.value as u16)
}

fn operand_i16(operands: &[Operand], index: usize) -> i16 {
    operands
        .get(index)
        .map_or(0, |operand| operand.value as i16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scd::opcode::command_op;

    fn operands(values: &[i64]) -> Vec<Operand> {
        values
            .iter()
            .map(|&value| Operand {
                value,
                target: None,
            })
            .collect()
    }

    fn op(code: u8) -> &'static Op {
        command_op(code).unwrap()
    }

    fn game() -> GameState {
        GameState::new(RoomId::parse("1001").unwrap(), &RoomState::default())
    }

    #[test]
    fn constructor_seeds_the_identity_bytes() {
        let state = game();
        assert_eq!(state.state_bytes[0], 1);
        assert_eq!(state.state_bytes[1], 0);
        assert_eq!(state.state_bytes[2], 1);
        assert_eq!(state.state_bytes[3], 0);
        assert_eq!(state.camera, CameraState::default());
        assert_eq!(state.frame, 0);
    }

    #[test]
    fn flag_bits_are_msb_first() {
        let mut bank = FlagBank::new();
        assert!(!bank.bit(0));
        assert!(bank.apply(0, 0));
        assert_eq!(bank.byte(3), 0x80);
        assert!(bank.bit(0));
        assert!(bank.apply(0, 1));
        assert_eq!(bank.byte(3), 0x00);
        assert!(!bank.bit(0));
        assert!(bank.apply(0, 2));
        assert!(bank.bit(0));
        assert!(bank.apply(0, 2));
        assert!(!bank.bit(0));

        assert!(bank.apply(31, 0));
        assert_eq!(bank.byte(0), 0x01);
        assert!(bank.bit(31));

        assert!(bank.apply(0x20, 0));
        assert_eq!(bank.byte(7), 0x80);
        assert!(bank.bit(0x20));

        assert!(bank.apply(0xE0, 0));
        assert_eq!(bank.byte(31), 0x80);
        assert!(bank.bit(0xE0));

        assert!(!bank.apply(0, 3));
        assert_eq!(bank.byte(3), 0x00);
    }

    #[test]
    fn ck_is_true_when_the_bit_differs_from_expected() {
        let mut state = game();
        assert!(!state.flag_test(0, 0, false));
        assert!(state.flag_test(0, 0, true));
        assert!(state.apply_flag(0, 0, 0));
        assert!(state.flag_test(0, 0, false));
        assert!(!state.flag_test(0, 0, true));
        assert!(!state.flag_test(10, 0, true));
        assert!(!state.flag_test(10, 0, false));
    }

    #[test]
    fn cmpb_uses_the_shared_mode_table() {
        let mut state = game();
        state.state_bytes[5] = 10;
        assert!(state.compare_byte(5, 0, 10));
        assert!(!state.compare_byte(5, 0, 11));
        assert!(state.compare_byte(5, 1, 9));
        assert!(!state.compare_byte(5, 1, 10));
        assert!(state.compare_byte(5, 2, 10));
        assert!(!state.compare_byte(5, 2, 11));
        assert!(state.compare_byte(5, 3, 11));
        assert!(!state.compare_byte(5, 3, 10));
        assert!(state.compare_byte(5, 4, 10));
        assert!(!state.compare_byte(5, 4, 9));
        assert!(state.compare_byte(5, 5, 9));
        assert!(!state.compare_byte(5, 5, 10));
        assert!(!state.compare_byte(5, 6, 10));
    }

    #[test]
    fn cmpw_compares_unsigned_state_words() {
        let mut state = game();
        state.state_words[4] = 300;
        assert!(state.compare_word(4, 0, 300));
        assert!(!state.compare_word(4, 0, 299));
        assert!(state.compare_word(4, 1, 299));
        assert!(!state.compare_word(4, 1, 300));
        assert!(state.compare_word(4, 2, 300));
        assert!(state.compare_word(4, 3, 301));
        assert!(state.compare_word(4, 4, 300));
        assert!(state.compare_word(4, 5, 299));

        state.state_words[6] = 1;
        assert!(!state.compare_word(6, 1, -1));
        assert!(state.compare_word(6, 3, -1));
    }

    #[test]
    fn conditions_report_continue_and_finished() {
        let mut state = game();
        state.state_bytes[1] = 5;
        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_flags(op(0x06), &operands(&[1, 0, 5])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flags(op(0x06), &operands(&[1, 0, 4])),
            StepResult::Finished
        );
        assert_eq!(
            host.on_flags(op(0x04), &operands(&[0, 3, 0])),
            StepResult::Finished
        );
        assert_eq!(
            host.on_flags(op(0x04), &operands(&[0, 3, 1])),
            StepResult::Continue
        );
    }

    #[test]
    fn setb_and_setw_write_the_state_blocks() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_flags(op(0x08), &operands(&[7, 42, 0])),
                StepResult::Continue
            );
            assert_eq!(host.state().state_bytes[7], 42);
            assert_eq!(
                host.on_flags(op(0x31), &operands(&[9, 0x1234])),
                StepResult::Continue
            );
            assert_eq!(host.state().state_words[9], 0x1234);
            host.on_flags(op(0x08), &operands(&[200, 1, 0]));
            host.on_flags(op(0x31), &operands(&[40, 1]));
        }
        assert_eq!(state.state_bytes[7], 42);
        assert_eq!(state.state_words[9], 0x1234);
    }

    #[test]
    fn cutnext_cutcurr_and_cut_auto_move_and_lock_the_camera() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_camera(op(0x09), &operands(&[3])),
                StepResult::Continue
            );
            assert_eq!(host.state().camera.current_cut, 3);
            assert_eq!(host.state().camera.saved_cut, Some(0));
            assert!(host.state().camera.locked);

            assert_eq!(
                host.on_camera(op(0x0A), &operands(&[0])),
                StepResult::Continue
            );
            assert_eq!(host.state().camera.current_cut, 0);
            assert_eq!(host.state().camera.saved_cut, None);
            assert!(!host.state().camera.locked);

            host.on_camera(op(0x23), &operands(&[1]));
            assert!(host.state().camera.locked);
            host.on_camera(op(0x23), &operands(&[0]));
            assert!(!host.state().camera.locked);
        }
        assert_eq!(state.camera.current_cut, 0);
    }

    #[test]
    fn message_sets_id_pause_and_active() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_message(op(0x0B), &operands(&[170, 79])),
                StepResult::Continue
            );
            assert_eq!(host.state().message.id, Some(170));
            assert_eq!(host.state().message.pause, 79);
            assert!(host.state().message.active);
        }
        assert_eq!(
            state.message,
            MessageState {
                id: Some(170),
                pause: 79,
                active: true,
            }
        );
    }

    #[test]
    fn bgm_play_and_stop_update_bits_and_queue_requests() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_sound(op(0x15), &operands(&[0])),
                StepResult::Continue
            );
            assert_eq!(
                host.on_sound(op(0x15), &operands(&[2])),
                StepResult::Continue
            );
            assert!(host.state().bgm.channels[0]);
            assert!(!host.state().bgm.channels[1]);
            assert!(host.state().bgm.channels[2]);
            assert_eq!(host.state().bgm.state, 0x08 | 0x20);

            assert_eq!(
                host.on_sound(op(0x16), &operands(&[0])),
                StepResult::Continue
            );
            assert!(!host.state().bgm.channels[0]);
            assert_eq!(host.state().bgm.state, 0x20);
        }
        assert_eq!(
            state.room_bgm_requests,
            vec![
                BgmRequest {
                    channel: 0,
                    start: true,
                },
                BgmRequest {
                    channel: 2,
                    start: true,
                },
                BgmRequest {
                    channel: 0,
                    start: false,
                },
            ]
        );
    }

    #[test]
    fn room_bgm_state_writes_the_state_byte() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_sound(op(0x37), &operands(&[0, 0, 0x40])),
            StepResult::Continue
        );
        assert_eq!(host.state().bgm.state, 0x40);
    }

    #[test]
    fn nops_do_not_record_placeholders() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_flow(op(0x0E), &operands(&[0])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x32), &operands(&[0, 0, 0])),
            StepResult::Continue
        );
        assert!(host.state().placeholders.is_empty());
    }

    #[test]
    fn unimplemented_classes_record_placeholder_counts() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_enemy(op(0x1B), &operands(&[0])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_player(op(0x20), &operands(&[0])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_model(op(0x1F), &operands(&[0])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_effect(op(0x2A), &operands(&[0])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_misc(op(0x25), &operands(&[0])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_message(op(0x29), &operands(&[0])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_camera(op(0x40), &operands(&[0])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_sound(op(0x27), &operands(&[0])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_item(op(0x4C), &operands(&[0, 0, 0])),
                StepResult::Placeholder
            );
        }
        assert_eq!(state.placeholders.len(), 9);
        assert_eq!(state.placeholders[&0x1B], 1);
        assert_eq!(state.placeholders[&0x20], 1);
        assert_eq!(state.placeholders[&0x1F], 1);
        assert_eq!(state.placeholders[&0x2A], 1);
        assert_eq!(state.placeholders[&0x25], 1);
        assert_eq!(state.placeholders[&0x29], 1);
        assert_eq!(state.placeholders[&0x40], 1);
        assert_eq!(state.placeholders[&0x27], 1);
        assert_eq!(state.placeholders[&0x4C], 1);
    }

    fn item_action(slot: u8, item: u8, quantity: u8, zone: [i16; 4]) -> RoomAction {
        RoomAction {
            slot,
            kind: RoomActionKind::Item,
            zone,
            sce: HANDLER_ITEM,
            handler: HANDLER_ITEM,
            flags: 0x81,
            params: [item, quantity, 1, 0xFF, 0, 0, 0, 0],
        }
    }

    fn door_action(door: Door) -> RoomAction {
        RoomAction {
            slot: door.slot,
            kind: RoomActionKind::Door,
            zone: door.zone,
            sce: HANDLER_DOOR,
            handler: HANDLER_DOOR,
            flags: door.sub_type,
            params: [
                door.lock,
                door.next_room,
                door.key,
                door.sub_type,
                0,
                0,
                0,
                0,
            ],
        }
    }

    fn door(next_room: u8, lock: u8) -> Door {
        Door {
            slot: 0,
            zone: [0, 0, 100, 100],
            next_room,
            next_pos: [555, 0, 666],
            next_angle: 1024,
            lock,
            key: 0,
            sub_type: 0x81,
        }
    }

    #[test]
    fn action_zone_containment_is_inclusive_and_unsigned() {
        let action = RoomAction {
            slot: 0,
            kind: RoomActionKind::Other,
            zone: [100, 200, 300, 400],
            sce: 0,
            handler: 1,
            flags: 0,
            params: [0; 8],
        };
        assert!(action.contains(100, 200));
        assert!(action.contains(400, 600));
        assert!(!action.contains(99, 200));
        assert!(!action.contains(100, 199));
        assert!(!action.contains(401, 200));
        assert!(!action.contains(100, 601));

        let wrapped = RoomAction {
            zone: [-1, -1, 1, 1],
            ..action
        };
        assert!(wrapped.contains(65535, 65535));
        assert!(!wrapped.contains(-1, -1));
    }

    #[test]
    fn door_aot_set_stores_the_door_record() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            let result = host.on_room_action(
                op(0x0C),
                &operands(&[
                    0, 2700, 500, 1700, 1800, 3, 0, 0, 4, 0, 1, 8700, 0, 7900, 1024, 0, 129,
                ]),
            );
            assert_eq!(result, StepResult::Continue);
        }
        let action = state.room_actions[0].expect("door action");
        assert_eq!(action.kind, RoomActionKind::Door);
        assert_eq!(action.zone, [2700, 500, 1700, 1800]);
        assert_eq!(action.handler, HANDLER_DOOR);
        let stored = state.doors[0].expect("door record");
        assert_eq!(stored.next_room, 1);
        assert_eq!(stored.next_pos, [8700, 0, 7900]);
        assert_eq!(stored.next_angle, 1024);
        assert_eq!(stored.lock, 0);
        assert_eq!(stored.key, 0);
        assert_eq!(stored.sub_type, 129);
    }

    #[test]
    fn aot_set_classifies_the_sce_byte() {
        let cases: [(u8, RoomActionKind); 8] = [
            (1, RoomActionKind::Door),
            (2, RoomActionKind::Message),
            (4, RoomActionKind::Item),
            (8, RoomActionKind::ItemBox),
            (9, RoomActionKind::Event),
            (10, RoomActionKind::Other),
            (15, RoomActionKind::Item),
            (16, RoomActionKind::Typewriter),
        ];
        for (sce, kind) in cases {
            let mut state = game();
            {
                let mut host = ScdGameHost::new(&mut state);
                assert_eq!(
                    host.on_room_action(
                        op(0x0D),
                        &operands(&[2, 100, 200, 300, 400, i64::from(sce), 0x81, 7, 8, 9,]),
                    ),
                    StepResult::Continue
                );
            }
            let action = state.room_actions[2].expect("action");
            assert_eq!(action.kind, kind, "sce {sce}");
            assert_eq!(action.zone, [100, 200, 300, 400]);
            assert_eq!(action.handler, sce);
            assert_eq!(action.flags, 0x81);
            assert_eq!(action.param_word(0), 7);
            assert_eq!(action.param_word(1), 8);
            assert_eq!(action.param_word(2), 9);
        }
    }

    #[test]
    fn aot_reset_and_arm_update_existing_slots_only() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            host.on_room_action(op(0x0D), &operands(&[1, 0, 0, 10, 10, 8, 0x81, 1, 2, 3]));
            assert_eq!(
                host.on_room_action(op(0x12), &operands(&[1, 9, 0x80, 4, 5, 6])),
                StepResult::Continue
            );
            assert_eq!(host.state().room_actions[1].unwrap().handler, 9);
            assert_eq!(host.state().room_actions[1].unwrap().flags, 0x80);
            assert_eq!(host.state().room_actions[1].unwrap().param_word(2), 6);

            assert_eq!(
                host.on_room_action(op(0x13), &operands(&[1, 16, 0x01])),
                StepResult::Continue
            );
            assert_eq!(host.state().room_actions[1].unwrap().handler, 16);
            assert_eq!(host.state().room_actions[1].unwrap().flags, 0x01);

            assert_eq!(
                host.on_room_action(op(0x12), &operands(&[7, 9, 0, 0, 0, 0])),
                StepResult::Continue
            );
            assert!(host.state().room_actions[7].is_none());
        }
    }

    #[test]
    fn item_aot_set_stores_item_fields() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_item(
                    op(0x18),
                    &operands(&[
                        1, 4140, 7730, 1800, 1800, 0x2F, 3, 0, 255, 5040, -910, 8630, 0, 22, 0x81,
                        0,
                    ]),
                ),
                StepResult::Continue
            );
        }
        let action = state.room_actions[1].expect("item action");
        assert_eq!(action.kind, RoomActionKind::Item);
        assert_eq!(action.zone, [4140, 7730, 1800, 1800]);
        assert_eq!(action.item_id(), 0x2F);
        assert_eq!(action.item_quantity(), 3);
        assert_eq!(action.item_model(), 0);
        assert_eq!(action.item_position(), [5040, 0, 8630]);
        assert_eq!(action.flags, 0x81);
    }

    #[test]
    fn item_pickup_stacks_and_consumes_the_action() {
        let mut state = game();
        state.room_actions[0] = Some(item_action(0, 10, 3, [0, 0, 100, 100]));
        state.room_actions[1] = Some(item_action(1, 10, 2, [0, 0, 100, 100]));

        state.interact([50, 0, 50], false);
        assert!(
            state.inventory.is_empty(),
            "no pickup without the action key"
        );

        state.interact([50, 0, 50], true);
        assert_eq!(
            state.inventory,
            vec![InventoryItem {
                id: 10,
                quantity: 5
            }]
        );
        assert_eq!(state.last_picked_item, Some(10));
        assert_eq!(state.item_events, vec![10, 10]);
        assert!(state.room_actions.iter().all(Option::is_none));

        state.interact([50, 0, 50], true);
        assert_eq!(
            state.inventory[0].quantity, 5,
            "consumed actions do not fire again"
        );
    }

    #[test]
    fn placeholder_kinds_record_an_interaction() {
        let mut state = game();
        state.room_actions[3] = Some(RoomAction {
            slot: 3,
            kind: RoomActionKind::ItemBox,
            zone: [0, 0, 100, 100],
            sce: HANDLER_ITEMBOX,
            handler: HANDLER_ITEMBOX,
            flags: 0x81,
            params: [0; 8],
        });
        state.interact([50, 0, 50], true);
        assert!(state.inventory.is_empty());
        assert_eq!(
            state.last_interaction,
            Some(RoomInteraction {
                slot: 3,
                kind: RoomActionKind::ItemBox,
                message: None,
            })
        );
    }

    #[test]
    fn message_action_sets_the_message() {
        let mut state = game();
        state.room_actions[4] = Some(RoomAction {
            slot: 4,
            kind: RoomActionKind::Message,
            zone: [0, 0, 100, 100],
            sce: HANDLER_MESSAGE,
            handler: HANDLER_MESSAGE,
            flags: 0x81,
            params: [HANDLER_MESSAGE, 0x81, 170, 0, 79, 0, 0, 0],
        });
        state.interact([50, 0, 50], true);
        assert_eq!(state.message.id, Some(170));
        assert_eq!(state.message.pause, 79);
        assert!(state.message.active);
        assert_eq!(
            state.last_interaction,
            Some(RoomInteraction {
                slot: 4,
                kind: RoomActionKind::Message,
                message: Some(170),
            })
        );
    }

    #[test]
    fn locked_door_blocks_and_unlock_opens_it() {
        let unlock = 0x80 | 5;
        let mut state = game();
        state.room_actions[0] = Some(door_action(door(1, unlock)));
        state.doors[0] = Some(door(1, unlock));

        state.interact([50, 0, 50], true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, Some(LOCKED_MESSAGE));
        assert!(state.message.active);

        assert!(state.apply_flag(2, 5, 0));
        state.interact([50, 0, 50], true);
        assert_eq!(
            state.transition,
            Some(RoomTransition {
                target: RoomId {
                    stage: state.id.stage,
                    room: 1,
                    player_flag: state.id.player_flag,
                },
                pos: [555, 0, 666],
                angle: 1024,
            })
        );
    }

    #[test]
    fn unlocked_door_transitions_without_a_flag() {
        let mut state = game();
        state.room_actions[0] = Some(door_action(door(0x1F, 0)));
        state.doors[0] = Some(door(0x1F, 0));
        state.interact([50, 0, 50], true);
        let transition = state.transition.expect("transition");
        assert_eq!(transition.target.room, 0x1F);
        assert_eq!(transition.pos, [555, 0, 666]);
    }

    #[test]
    fn out_of_range_door_target_is_ignored() {
        let mut state = game();
        state.room_actions[0] = Some(door_action(door(0xFF, 0)));
        state.doors[0] = Some(door(0xFF, 0));
        state.interact([50, 0, 50], true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, None);
    }

    #[test]
    fn run_room_action_handler_zero_is_inert() {
        let mut state = game();
        state.room_actions[1] = Some(item_action(1, 7, 1, [0, 0, 100, 100]));
        state.room_actions[1].as_mut().unwrap().handler = 0;
        state.interact([50, 0, 50], true);
        assert!(state.inventory.is_empty());

        assert!(!state.run_room_action(1, 0));
        assert!(!state.run_room_action(9, 4));
    }

    #[test]
    fn give_item_runs_the_action_and_stops() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            host.on_item(
                op(0x18),
                &operands(&[2, 0, 0, 1, 1, 0x42, 1, 1, 255, 0, 0, 0, 0, 23, 0, 0]),
            );
            assert_eq!(
                host.on_room_action(op(0x2D), &operands(&[2, i64::from(HANDLER_ITEM), 0])),
                StepResult::Finished
            );
        }
        assert_eq!(
            state.inventory,
            vec![InventoryItem {
                id: 0x42,
                quantity: 1
            }]
        );
        assert_eq!(state.last_picked_item, Some(0x42));
        assert!(state.room_actions[2].is_none());
    }

    #[test]
    fn aot_on_picks_up_an_item_action() {
        let mut state = game();
        state.room_actions[3] = Some(item_action(3, 0x42, 1, [0, 0, 1, 1]));
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_room_action(op(0x24), &operands(&[3, i64::from(HANDLER_ITEM), 0])),
                StepResult::Continue
            );
        }
        assert_eq!(state.last_picked_item, Some(0x42));
        assert!(state.room_actions[3].is_none());
    }

    #[test]
    fn test_and_remove_item_opcodes() {
        let mut state = game();
        state.add_item(7, 2);
        state.last_used_item = Some(7);
        state.last_picked_item = Some(9);
        state.equipped = Some(5);

        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_flow(op(0x10), &operands(&[7])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x10), &operands(&[8])),
            StepResult::Finished
        );
        assert_eq!(
            host.on_flow(op(0x11), &operands(&[9])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x11), &operands(&[7])),
            StepResult::Finished
        );
        assert_eq!(
            host.on_flow(op(0x1A), &operands(&[7])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x1A), &operands(&[8])),
            StepResult::Finished
        );
        assert_eq!(
            host.on_flow(op(0x1D), &operands(&[5])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_camera(op(0x1C), &operands(&[5])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_item(op(0x1C), &operands(&[6])),
            StepResult::Finished
        );
        assert_eq!(
            host.on_item(op(0x2C), &operands(&[7])),
            StepResult::Continue
        );
        assert!(host.state().has_item(7));
        assert_eq!(host.state().item_count(7), 1);
        assert_eq!(
            host.on_item(op(0x2C), &operands(&[7])),
            StepResult::Continue
        );
        assert!(!host.state().has_item(7));
        assert_eq!(
            host.on_item(op(0x2C), &operands(&[7])),
            StepResult::Finished
        );
    }

    #[test]
    fn ck_item_count_uses_the_compare_modes() {
        let mut state = game();
        state.add_item(0x0B, 10);

        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0B, 0, 10])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0B, 0, 9])),
            StepResult::Finished
        );
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0B, 1, 9])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0B, 2, 10])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0B, 3, 11])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0B, 4, 10])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0B, 5, 9])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0B, 5, 10])),
            StepResult::Finished
        );
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0C, 0, 0])),
            StepResult::Finished
        );
        assert_eq!(
            host.on_flow(op(0x22), &operands(&[0x0B, 6, 10])),
            StepResult::Finished
        );
    }

    #[test]
    fn enter_room_preserves_inventory_and_flags() {
        let mut state = game();
        state.add_item(3, 2);
        state.apply_flag(0, 1, 0);
        state.room_actions[0] = Some(item_action(0, 3, 1, [0, 0, 1, 1]));
        state.doors[0] = Some(door(1, 0));
        state.message.id = Some(4);
        state.camera.current_cut = 2;

        let next = RoomId {
            stage: 2,
            room: 3,
            player_flag: 1,
        };
        state.enter_room(next, &RoomState::default());

        assert_eq!(state.id, next);
        assert_eq!(state.state_bytes[0], 2);
        assert_eq!(state.state_bytes[1], 3);
        assert_eq!(state.state_bytes[2], 1);
        assert_eq!(state.inventory[0].quantity, 2);
        assert!(state.flags[0].bit(1));
        assert!(state.room_actions.iter().all(Option::is_none));
        assert!(state.doors.iter().all(Option::is_none));
        assert_eq!(state.message.id, None);
        assert_eq!(state.camera.current_cut, 0);
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn room_1001_init_registers_its_room_actions() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let path = std::path::Path::new(&root).join("JPN/STAGE1/ROOM1001.RDT");
        let data = std::fs::read(&path).expect("read ROOM1001.RDT");
        let id = RoomId::parse("1001").unwrap();
        let room = crate::rdt::parse(&data, id).unwrap();
        let scripts = crate::scd::reader::parse(&data).unwrap();

        let mut state = GameState::new(id, &room);
        {
            let mut command_vm = crate::scd::vm::CommandVm::new(&scripts);
            let mut host = ScdGameHost::new(&mut state);
            command_vm.run_init(&mut host);
        }

        let occupied: Vec<u8> = (0..ROOM_ACTION_SLOTS)
            .filter(|&slot| state.room_actions[slot].is_some())
            .map(|slot| slot as u8)
            .collect();
        assert_eq!(occupied, vec![0, 1, 2, 3, 4, 5, 7]);
        let kinds: Vec<RoomActionKind> = occupied
            .iter()
            .map(|&slot| state.room_actions[usize::from(slot)].unwrap().kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                RoomActionKind::Door,
                RoomActionKind::Item,
                RoomActionKind::ItemBox,
                RoomActionKind::Item,
                RoomActionKind::Item,
                RoomActionKind::Event,
                RoomActionKind::Typewriter,
            ]
        );

        let stored = state.doors[0].expect("door slot 0");
        assert_eq!(stored.zone, [2700, 500, 1700, 1800]);
        assert_eq!(stored.next_room, 1);
        assert_eq!(stored.next_pos, [8700, 0, 7900]);
        assert_eq!(stored.next_angle, 1024);
        assert_eq!(stored.lock, 0);
        assert_eq!(stored.key, 0);

        let ribbon = state.room_actions[1].unwrap();
        assert_eq!(ribbon.item_id(), 0x2F);
        assert_eq!(ribbon.item_quantity(), 3);
        let serum = state.room_actions[3].unwrap();
        assert_eq!(serum.item_id(), 0x42);
        assert_eq!(serum.item_quantity(), 1);
        assert_eq!(serum.item_model(), 1);
    }
}
