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
//! the player position and facing to [`GameState::interact`]. Entries are probed
//! with the original's masks: walk-in zones fire every frame, action-key zones
//! only while the key is held, at a point 600 units in front of the player.
//! Items and doors act for real; the menu-driven kinds record a placeholder
//! interaction. Entities, models and effects stay recorded placeholders until
//! their systems exist.

use std::collections::BTreeMap;

use crate::player::PlayerState;
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
/// Entity slots: 0 is the player, 1.. are enemies and scripted objects.
pub const ENTITY_COUNT: usize = 32;
/// `selected_entity` value when the event pointed at something this slice does
/// not model as an entity (object models and item models).
pub const ENTITY_NONE: u8 = u8::MAX;
/// Default per-tick yaw step of an `act_motion` instruction.
const MOTION_DEFAULT_STEP: u8 = 0xC0;
/// Default per-tick pitch step of an `act_motion` instruction.
const MOTION_DEFAULT_PITCH_STEP: u8 = 0x40;
/// Placeholder message id displayed by a door that refuses to open.
pub const LOCKED_MESSAGE: u8 = 200;
/// Message shown while a key turns in a lock.
const MESSAGE_KEY_TURN: u8 = 0xC3;
/// Message shown by a `0xFE` door (only opens from the far side).
const MESSAGE_OTHER_SIDE: u8 = 0xD4;
/// Message shown by a door locked for good (`0xFF` key).
const MESSAGE_LOCKED_KEY: u8 = 0xD3;
/// Message shown when Jill has no lockpick for a sword-key lock.
const MESSAGE_NO_LOCKPICK: u8 = 0xD5;
/// Message shown by a door restricted to the other character.
const MESSAGE_WRONG_CHARACTER: u8 = 0xD6;
/// Item id of the sword key, which Jill may replace with her lockpick.
const ITEM_SWORD_KEY: u8 = 0x33;
/// Scenario flag raised when Jill has the lockpick.
const SCENARIO_FLAG_HAS_LOCKPICK: u8 = 0x7C;
/// Scenario flag selecting the second-visit stage variants.
const SCENARIO_FLAG_STAGE_VARIANT: u8 = 0x00;
/// Scenario/state flag bank index.
const BANK_SCENARIO: u8 = 0;
/// Door lock flag bank index.
const BANK_LOCKS: u8 = 2;
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
/// - doors keep `[lock, next_room, key, sub_type, direction, sfx, door_type,
///   camera]`;
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
    /// Bank-7 (room items) flag index used to remember a taken item; `0xFF`
    /// when the action does not use one.
    pub room_items_flag: u8,
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
///
/// Field order mirrors the 24-byte record the original stores from the
/// instruction operands: zone x/z/width/depth, door direction, sfx, door type,
/// camera byte, lock descriptor, destination, entry position, entry angle,
/// required item and probe flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Door {
    /// Room action slot.
    pub slot: u8,
    /// Interaction box: `x`, `z`, `width`, `depth`.
    pub zone: [i16; 4],
    /// Door direction byte; selects the opening animation arm.
    pub direction: u8,
    /// Door sound effect id.
    pub sfx: u8,
    /// Door type: indexes the animation data (stairs, elevators, doors...).
    pub door_type: u8,
    /// Entry camera byte: bit `0x80` camera-only, bit `0x40` silent, low six
    /// bits the animation's entry camera.
    pub camera: u8,
    /// Lock descriptor: bit `0x80` start locked, bit `0x40` one character,
    /// low six bits the lock flag index in bank 2.
    pub lock: u8,
    /// Room number within the destination stage; values `>= 0x20` also change
    /// stage.
    pub next_room: u8,
    /// Spawn position in the target room: X and Z are zero-extended room
    /// coordinates, Y is a signed height.
    pub next_pos: [i32; 3],
    /// Spawn facing in the target room.
    pub next_angle: i16,
    /// Item needed to open the door.
    pub key: u8,
    /// Probe flag byte: bit `0x80` needs the action key, bit `0x40` probes the
    /// player position instead of the forward reach point, low bits select
    /// which per-frame probe masks can fire it.
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

/// One scripted entity.
///
/// Slot 0 is the player; slots 1.. are enemies and objects that later
/// milestones spawn. The actor and tween sub-ISAs operate on the entity the
/// current event selected with `evt_work_set`.
///
/// `state_field` packs the two state-block bytes at entity offset 0x84: the
/// low byte is the entity state (1 idle, 8 scripted animation) and the high
/// byte is the player-ignore flag. The tween `tw_set_rot` instruction stores
/// a sign-extended byte into that same word, so it is kept as one u16.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Entity {
    /// Room position.
    pub pos: [i32; 3],
    /// 12-bit yaw.
    pub angle: u16,
    /// Rotation X component of the entity's rotation vector.
    pub pitch: u16,
    /// Rotation Z, the companion of `pitch` in the rotation SVECTOR.
    pub roll: u16,
    /// Animation behavior byte (`action_behavior`).
    pub behavior: u8,
    /// Animation action state byte.
    pub action_state: u8,
    /// Animation id.
    pub anim: u16,
    /// Scripted animation frame id.
    pub anim_frame: u8,
    /// Whether the slot is spawned.
    pub active: bool,
    /// `scd_entity_flags`, OR/SET/XORed by `act_flag_op`.
    pub flags: u16,
    /// Per-tick translation from the tween `tw_set_pos` instruction.
    pub move_speed: [i16; 3],
    /// Per-tick rotation steps from the tween `tw_set_rot` instruction:
    /// `[0]` drives `pitch`, `[2]` drives `angle`.
    pub move_step: [i16; 3],
    /// The entity state word (`state | ignore << 8`).
    pub state_field: u16,
    /// Damage hit state (`tw_set_8a`).
    pub hit_state: u8,
    /// Last `tw_set_sel` selector.
    pub selector: u16,
    /// Health word written by `tw_set_sel` selector 3 and `tw_set_field`.
    pub health: i16,
    /// Animation position offsets (`unk_c6`/`unk_c8`).
    pub unk_c6: u16,
    /// Animation position offset Z.
    pub unk_c8: u16,
    /// SCD animation parameter.
    pub anim_param: u8,
    /// SCD animation timer.
    pub timer: u16,
    /// Animation frame blend counter.
    pub blend: u8,
    /// Look-at control byte set by `act_motion`.
    pub look_at_flags: u8,
    /// Look-at target position; for an entity target this is refreshed every
    /// tick from the target's current position.
    pub target: [i32; 3],
    /// Entity slot this entity is moving toward, when the target is an entity.
    pub target_entity: Option<u8>,
    /// Per-tick yaw step of the active `act_motion`.
    pub step: u8,
    /// Per-tick pitch step of the active `act_motion`.
    pub pitch_step: u8,
}

impl Entity {
    /// The entity state byte (offset 0x84).
    pub fn state(&self) -> u8 {
        self.state_field as u8
    }

    /// Overwrite the entity state byte, keeping the ignore flag.
    pub fn set_state(&mut self, state: u8) {
        self.state_field = (self.state_field & 0xFF00) | u16::from(state);
    }

    /// The player-ignore flag (offset 0x85).
    pub fn ignore(&self) -> u8 {
        (self.state_field >> 8) as u8
    }

    /// Overwrite the player-ignore flag, keeping the entity state.
    pub fn set_ignore(&mut self, ignore: u8) {
        self.state_field = (self.state_field & 0x00FF) | (u16::from(ignore) << 8);
    }

    /// `tw_set_field`: store one byte of the entity state block at 0x84 plus
    /// `offset`. Returns `false` for an offset with no modelled field.
    fn set_state_byte(&mut self, offset: u8, value: u8) -> bool {
        match offset {
            0 => self.set_state(value),
            1 => self.set_ignore(value),
            2 => self.behavior = value,
            3 => self.action_state = value,
            4 => self.health = (self.health & !0x00FF) | i16::from(value),
            5 => self.health = (self.health & 0x00FF) | (i16::from(value) << 8),
            6 => self.hit_state = value,
            _ => return false,
        }
        true
    }
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
    /// Scripted entities; slot 0 is the player.
    pub entities: [Entity; ENTITY_COUNT],
    /// Entity slot the current event operates on, or [`ENTITY_NONE`].
    pub selected_entity: u8,
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

/// The initial entity array: only the player slot is spawned.
fn initial_entities() -> [Entity; ENTITY_COUNT] {
    let mut entities = [Entity::default(); ENTITY_COUNT];
    entities[0].active = true;
    entities
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
            entities: initial_entities(),
            selected_entity: 0,
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

    /// `evt_work_set`: turn an entity type/index pair into a slot. Type 0 is
    /// the player, type 1 an enemy (`index` becomes slot `index + 1`), and
    /// object/item models have no entity this slice and select
    /// [`ENTITY_NONE`]. Returns whether a slot was selected.
    pub fn select_entity(&mut self, entity_type: u8, index: u8) -> bool {
        let slot = match entity_type {
            0 => Some(0usize),
            1 => 1usize.checked_add(usize::from(index)),
            _ => None,
        }
        .filter(|slot| *slot < ENTITY_COUNT);
        match slot {
            Some(slot) => {
                self.selected_entity = slot as u8;
                true
            }
            None => {
                self.selected_entity = ENTITY_NONE;
                false
            }
        }
    }

    /// One entity by slot.
    pub fn entity(&self, slot: u8) -> Option<&Entity> {
        self.entities.get(usize::from(slot))
    }

    /// One entity by slot, mutably.
    pub fn entity_mut(&mut self, slot: u8) -> Option<&mut Entity> {
        self.entities.get_mut(usize::from(slot))
    }

    /// The entity the current event selected.
    pub fn selected_entity(&self) -> Option<&Entity> {
        if self.selected_entity == ENTITY_NONE {
            return None;
        }
        self.entity(self.selected_entity)
    }

    /// The entity the current event selected, mutably.
    pub fn selected_entity_mut(&mut self) -> Option<&mut Entity> {
        if self.selected_entity == ENTITY_NONE {
            return None;
        }
        self.entity_mut(self.selected_entity)
    }

    /// Mirror entity 0 into the engine's player state when the scripts moved
    /// it, leaving the player alone otherwise. Called after the scripts run
    /// each tick so `dir_set` and actor motions move the visible player.
    pub fn sync_player(&self, player: &mut PlayerState) {
        let entity = &self.entities[0];
        if entity.pos != player.pos {
            player.pos = entity.pos;
        }
        let angle = entity.angle & 0x0FFF;
        if player.angle != angle {
            player.angle = angle;
        }
    }

    /// Mirror the visible player back onto entity 0 after physics moved it, so
    /// the next script tick sees the current position.
    pub fn sync_entity_from_player(&mut self, player: &PlayerState) {
        let entity = &mut self.entities[0];
        entity.pos = player.pos;
        entity.angle = player.angle & 0x0FFF;
    }

    /// `evt_tween_begin`: reset the selected entity as it enters the movement
    /// state (ignore flag 2, behavior and action state cleared).
    pub fn reset_tween_entity(&mut self) {
        if let Some(entity) = self.selected_entity_mut() {
            entity.set_ignore(2);
            entity.behavior = 0;
            entity.action_state = 0;
        }
    }

    /// Apply one actor sub-ISA instruction to the selected entity.
    ///
    /// The event VM reads the result: `Yield` keeps the slot on the same
    /// instruction for another tick and `Finished` drops it back to state 0.
    fn apply_actor_op(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.mnemonic {
            "act_motion" | "act_motion_path" => self.apply_act_motion(op, operands),
            "act_nop" => StepResult::Continue,
            "act_reset" | "act_end" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.set_ignore(0);
                }
                StepResult::Continue
            }
            "act_motion_bitclr" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.look_at_flags &= !0x10;
                }
                StepResult::Continue
            }
            "act_idle" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.state_field = 1;
                    entity.behavior = 0;
                    entity.action_state = 0;
                    entity.hit_state = 0;
                }
                StepResult::Continue
            }
            "act_anim_seq" => {
                if let Some(entity) = self.selected_entity_mut() {
                    let word0 = operand_u16(operands, 0);
                    let word1 = operand_u16(operands, 1);
                    let word2 = operand_u16(operands, 2);
                    entity.set_state(8);
                    entity.set_ignore(0);
                    entity.behavior = word0 as u8;
                    entity.action_state = 0;
                    entity.unk_c6 = (word0 >> 8) | ((word1 as u8 as u16) << 8);
                    entity.unk_c8 = (word1 >> 8) | ((word2 as u8 as u16) << 8);
                    entity.anim_param = (word2 >> 8) as u8;
                    entity.timer = 0x28;
                    entity.flags = 0;
                }
                StepResult::Continue
            }
            "act_anim_flags" => {
                if let Some(entity) = self.selected_entity_mut() {
                    let anim_data = u16::from(operand_u8(operands, 1))
                        | (u16::from(operand_u8(operands, 2)) << 8);
                    entity.set_state(8);
                    entity.set_ignore(0);
                    entity.behavior = 1;
                    entity.action_state = 0;
                    entity.anim = u16::from(operand_u8(operands, 0));
                    entity.anim_param = operand_u8(operands, 1);
                    entity.flags = (anim_data >> 6) & 0x3FC;
                    entity.timer = 0;
                }
                StepResult::Continue
            }
            "act_anim_set" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.set_state(8);
                    entity.set_ignore(0);
                    entity.behavior = operand_u8(operands, 0);
                    entity.action_state = 0;
                    entity.anim = u16::from(operand_u8(operands, 1));
                    entity.anim_param = operand_u8(operands, 2);
                    entity.timer = 0;
                    entity.flags = 0;
                }
                StepResult::Continue
            }
            "act_flag_op" => {
                if let Some(entity) = self.selected_entity_mut() {
                    let mode = operand_u8(operands, 0);
                    let value = operand_u16(operands, 1);
                    match mode {
                        0 => entity.flags |= value,
                        1 => entity.flags = value,
                        2 => entity.flags ^= value,
                        _ => {}
                    }
                }
                StepResult::Continue
            }
            "act_param_set" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.timer =
                        (operand_u16(operands, 0) >> 8) | (u16::from(operand_u8(operands, 1)) << 8);
                }
                StepResult::Continue
            }
            "act_action_a" | "act_action_b" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.anim_frame = operand_u8(operands, 0);
                    entity.blend = if entity.flags & 0x20 != 0 { 0 } else { 7 };
                    entity.action_state = 1;
                }
                StepResult::Continue
            }
            _ => {
                self.record_placeholder(op.op);
                StepResult::Placeholder
            }
        }
    }

    /// One actor motion instruction. While the target is out of reach the
    /// result is `Yield`, so the event VM re-runs the instruction next tick;
    /// on arrival the result is `Finished` and the actor state ends.
    ///
    /// Convention: the entity turns toward the target by at most the yaw step
    /// and translates by that same step along the target direction, on the XZ
    /// plane. The step is the low byte of the word at instruction offset +8
    /// (default 0xC0); the high byte is the pitch step (default 0x40). Flag
    /// 0x20 wraps negative target coordinates by +0x1000 before use, and flag
    /// set `0x93` targets another entity instead of a fixed point.
    fn apply_act_motion(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        let flags = operand_u8(operands, 0);
        let step_word =
            u16::from(operand_u8(operands, 4)) | (u16::from(operand_u8(operands, 5)) << 8);
        let yaw_step = match step_word as u8 {
            0 => MOTION_DEFAULT_STEP,
            step => step,
        };
        let pitch_step = match (step_word >> 8) as u8 {
            0 => MOTION_DEFAULT_PITCH_STEP,
            step => step,
        };

        if flags & 0x0F == 0 {
            if let Some(entity) = self.selected_entity_mut() {
                entity.look_at_flags = flags;
            }
            return StepResult::Continue;
        }

        let target_entity = if flags == 0x93 {
            let Some(slot) = motion_target_slot(operand_u8(operands, 1), operand_i16(operands, 2))
            else {
                self.record_placeholder(op.op);
                return StepResult::Placeholder;
            };
            Some(slot)
        } else {
            None
        };
        let target = match target_entity {
            Some(slot) => self.entities[usize::from(slot)].pos,
            None => {
                let mut x = i32::from(operand_i16(operands, 1));
                let mut y = i32::from(operand_i16(operands, 2));
                let z = i32::from(operand_i16(operands, 3));
                if flags & 0x20 != 0 {
                    if x < 0 {
                        x += 0x1000;
                    }
                    if y < 0 {
                        y += 0x1000;
                    }
                }
                [x, y, z]
            }
        };

        let Some(entity) = self.selected_entity_mut() else {
            self.record_placeholder(op.op);
            return StepResult::Placeholder;
        };
        entity.look_at_flags = flags;
        entity.step = yaw_step;
        entity.pitch_step = pitch_step;
        entity.target = target;
        entity.target_entity = target_entity;

        let dx = target[0] - entity.pos[0];
        let dz = target[2] - entity.pos[2];
        let step = i32::from(yaw_step);
        let distance = xz_distance(dx, dz);
        if distance <= step {
            entity.pos[0] = target[0];
            entity.pos[2] = target[2];
            entity.target_entity = None;
            // Arrival does not leave the actor state; only act_reset/act_end do.
            return StepResult::Continue;
        }
        let target_angle = angle_between(entity.pos, target);
        entity.angle = rotate_toward(entity.angle, target_angle, u16::from(yaw_step));
        entity.pos[0] += step * dx / distance;
        entity.pos[2] += step * dz / distance;
        StepResult::Yield
    }

    /// Apply one tween sub-ISA instruction to the selected entity. The tween
    /// state never yields.
    fn apply_tween_op(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.mnemonic {
            "tw_nop" | "tw_end" => StepResult::Continue,
            "tw_pos_add" => {
                if let Some(entity) = self.selected_entity_mut() {
                    for axis in 0..3 {
                        entity.pos[axis] =
                            entity.pos[axis].wrapping_add(i32::from(entity.move_speed[axis]));
                    }
                }
                StepResult::Continue
            }
            "tw_rot_add" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.pitch = entity.pitch.wrapping_add(entity.move_step[0] as u16);
                    entity.angle = entity.angle.wrapping_add(entity.move_step[2] as u16);
                    entity.roll = entity.roll.wrapping_add(entity.state_field);
                }
                StepResult::Continue
            }
            "tw_pos_rot_add" => {
                if let Some(entity) = self.selected_entity_mut() {
                    for axis in 0..3 {
                        entity.pos[axis] =
                            entity.pos[axis].wrapping_add(i32::from(entity.move_speed[axis]));
                    }
                    entity.pitch = entity.pitch.wrapping_add(entity.move_step[0] as u16);
                    entity.angle = entity.angle.wrapping_add(entity.move_step[2] as u16);
                    entity.roll = entity.roll.wrapping_add(entity.state_field);
                }
                StepResult::Continue
            }
            "tw_set_pos" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.move_speed = [
                        i16::from(operand_i8(operands, 0)),
                        i16::from(operand_i8(operands, 1)),
                        i16::from(operand_i8(operands, 2)),
                    ];
                }
                StepResult::Continue
            }
            "tw_set_rot" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.move_step[0] = i16::from(operand_i8(operands, 0));
                    entity.move_step[2] = i16::from(operand_i8(operands, 1));
                    entity.state_field = operand_i8(operands, 2) as i16 as u16;
                }
                StepResult::Continue
            }
            "tw_abs_pos" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.pos = [
                        i32::from(operand_i16(operands, 0)),
                        i32::from(operand_i16(operands, 1)),
                        i32::from(operand_i16(operands, 2)),
                    ];
                }
                StepResult::Continue
            }
            "tw_set_field" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.set_state_byte(operand_u8(operands, 0), operand_u8(operands, 1));
                }
                StepResult::Continue
            }
            "tw_set_8a" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.hit_state = operand_u8(operands, 0);
                }
                StepResult::Continue
            }
            "tw_set_sel" => {
                if let Some(entity) = self.selected_entity_mut() {
                    let selector = operand_u8(operands, 0);
                    let value = operand_u16(operands, 1);
                    entity.selector = u16::from(selector);
                    match selector {
                        0..=2 => entity.pos[usize::from(selector)] = i32::from(value as i16),
                        3 => entity.health = value as i16,
                        4 => entity.unk_c6 = value,
                        5 => entity.unk_c8 = value,
                        _ => {}
                    }
                }
                StepResult::Continue
            }
            "tw_abs_rot" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.pitch = operand_u16(operands, 0);
                    entity.angle = operand_u16(operands, 1);
                    entity.roll = operand_u16(operands, 2);
                }
                StepResult::Continue
            }
            _ => {
                self.record_placeholder(op.op);
                StepResult::Placeholder
            }
        }
    }

    /// Move into `id`: the flags, state blocks and inventory survive a room
    /// change; the room action table, message, camera and per-room requests do
    /// not. The player entity slot survives; the other entity slots reset. The
    /// identity bytes are reseeded from the new room.
    pub fn enter_room(&mut self, id: RoomId, _room: &RoomState) {
        self.id = id;
        self.state_bytes[0] = id.stage;
        self.state_bytes[1] = id.room;
        self.state_bytes[2] = id.player_flag;
        self.room_actions = [None; ROOM_ACTION_SLOTS];
        self.doors = [None; ROOM_ACTION_SLOTS];
        let player_entity = self.entities[0];
        self.entities = initial_entities();
        self.entities[0] = player_entity;
        self.selected_entity = 0;
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

    /// Probe the room action table for the player at `pos` facing `angle`.
    ///
    /// Mirrors the original's two probes: entries without probe bit `0x80` are
    /// tested every frame when their low flag bits intersect the frame masks
    /// (`1` and `4`), and entries with `0x80` only when the action key is held
    /// and their bit `0x01` is set. Probe bit `0x40` tests the player position
    /// itself; otherwise a point 600 units in front is tested. Only the first
    /// action-key entry that matches fires, as in the original. Item and door
    /// actions act for real, the menu-driven kinds record a placeholder.
    pub fn interact(&mut self, pos: [i32; 3], angle: u16, action: bool) {
        let (dx, dz) = crate::player::reach_offset(angle);
        let reach = [pos[0] + dx, pos[1], pos[2] + dz];
        let mut action_fired = false;
        for slot in 0..ROOM_ACTION_SLOTS {
            let Some(room_action) = self.room_actions[slot] else {
                continue;
            };
            if room_action.handler == 0 {
                continue;
            }
            let flags = room_action.flags;
            if flags & 0x80 != 0 {
                if !action || flags & 0x01 == 0 || action_fired {
                    continue;
                }
            } else if flags & 0x01 == 0 && flags & 0x04 == 0 {
                continue;
            }
            let probe = if flags & 0x40 != 0 { pos } else { reach };
            if !room_action.contains(probe[0], probe[2]) {
                continue;
            }
            match room_action.kind {
                RoomActionKind::Door => {
                    self.try_door(room_action.slot);
                }
                RoomActionKind::Item => {
                    self.pick_up(room_action.slot);
                }
                RoomActionKind::Event => {
                    self.start_room_event(room_action.slot);
                }
                RoomActionKind::Message => {
                    let id = room_action.param_word(0);
                    self.show_message(id as u8, room_action.param_word(1));
                    self.record_interaction(room_action.slot, room_action.kind, Some(id));
                }
                RoomActionKind::ItemBox | RoomActionKind::Typewriter | RoomActionKind::Other => {
                    self.record_interaction(room_action.slot, room_action.kind, None);
                }
            }
            if flags & 0x80 != 0 {
                action_fired = true;
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
            HANDLER_EVENT => self.start_room_event(slot),
            HANDLER_ITEMBOX | HANDLER_TYPEWRITER => {
                self.record_interaction(slot, action.kind, None);
                true
            }
            _ => false,
        }
    }

    /// Start the event script named by a room-event action (handler 9).
    ///
    /// The action's first word is the requested event slot (`>= 8` means "any
    /// free slot") and its second word the event index, matching the original's
    /// `ScdEventEntry_Create(entry[2], entry[4])`.
    pub fn start_room_event(&mut self, slot: u8) -> bool {
        let Some(action) = self.room_actions.get(usize::from(slot)).copied().flatten() else {
            return false;
        };
        if action.handler != HANDLER_EVENT {
            return false;
        }
        let event_slot = action.params[2];
        let event = action.params[4];
        self.pending_events.push((event_slot, event));
        true
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
        // Remember the pickup in the room items flag bank so the item does not
        // come back when the room is re-entered.
        if action.room_items_flag != 0xFF {
            self.apply_flag(7, action.room_items_flag, 0);
        }
        self.room_actions[usize::from(slot)] = None;
        self.doors[usize::from(slot)] = None;
        true
    }

    /// Run the door in `slot`, with the original's full interaction flow:
    /// character restriction, lock flag, required key (with Jill's lockpick
    /// substituting for the sword key) and the special `0xFE`/`0xFF` keys.
    ///
    /// A key turn only raises the lock flag and plays the message; the door
    /// transitions on the next probe, exactly as in the original. Returns
    /// whether a room transition was requested.
    pub fn try_door(&mut self, slot: u8) -> bool {
        let Some(door) = self.doors.get(usize::from(slot)).copied().flatten() else {
            return false;
        };
        // Some doors are barred to one of the two characters.
        if door.lock & 0x40 != 0 && self.id.player_flag & 1 == 1 {
            self.show_message(MESSAGE_WRONG_CHARACTER, 0xFF);
            return false;
        }
        // Unlocked outright, or its lock flag is already raised.
        if door.lock & 0x80 == 0 || self.flag_test(BANK_LOCKS, door.lock & 0x3F, false) {
            return self.begin_transition(&door);
        }
        let need = door.key;
        if need == 0xFE {
            // Opens from the far side: show the message, then raise the flag so
            // the next probe walks through.
            self.show_message(MESSAGE_OTHER_SIDE, 0xFF);
            self.apply_flag(BANK_LOCKS, door.lock & 0x3F, 0);
            return false;
        }
        if need == 0xFF {
            self.show_message(MESSAGE_LOCKED_KEY, 0xFF);
            return false;
        }
        if need == ITEM_SWORD_KEY && self.id.player_flag & 1 == 1 {
            // Jill substitutes her lockpick for the sword key, but only once
            // she has picked it up.
            if !self.flag_test(BANK_SCENARIO, SCENARIO_FLAG_HAS_LOCKPICK, false) {
                self.show_message(MESSAGE_NO_LOCKPICK, 0xFF);
                return false;
            }
        } else if !self.has_item(need) {
            // "It's locked": keyed message indexed from the sword key.
            let index = need.wrapping_sub(ITEM_SWORD_KEY).min(10);
            self.show_message(LOCKED_MESSAGE.saturating_add(index), 0xFF);
            return false;
        } else {
            // The key is consumed by the turn.
            self.remove_item(need);
        }
        self.show_message(MESSAGE_KEY_TURN, 0xFF);
        self.apply_flag(BANK_LOCKS, door.lock & 0x3F, 0);
        false
    }

    /// Arm the transition described by `door`, decoding the destination's
    /// stage change when the room byte is `>= 0x20`.
    fn begin_transition(&mut self, door: &Door) -> bool {
        let dest = door.next_room;
        if dest == 0xFF {
            return false;
        }
        let target = if dest < 0x20 {
            RoomId {
                stage: self.id.stage,
                room: dest,
                player_flag: self.id.player_flag,
            }
        } else {
            let mut stage = (dest >> 5) - 1;
            if stage < 2 && self.flag_test(BANK_SCENARIO, SCENARIO_FLAG_STAGE_VARIANT, false) {
                stage += 5;
            }
            RoomId {
                stage: stage + 1,
                room: dest & 0x1F,
                player_flag: self.id.player_flag,
            }
        };
        self.transition = Some(RoomTransition {
            target,
            pos: door.next_pos,
            angle: door.next_angle as u16 & 0x0FFF,
        });
        true
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

    /// The engine's interaction check: probe the room action table for the
    /// player at `pos` facing `angle`; only entries matching the original's
    /// probe masks act.
    pub fn interact(&mut self, pos: [i32; 3], angle: u16, action: bool) {
        self.state.interact(pos, angle, action);
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
                    direction: operand_u8(operands, 5),
                    sfx: operand_u8(operands, 6),
                    door_type: operand_u8(operands, 7),
                    camera: operand_u8(operands, 8),
                    next_room: operand_u8(operands, 10),
                    // The original zero-extends X and Z and sign-extends Y.
                    next_pos: [
                        i32::from(operand_i16(operands, 11) as u16),
                        i32::from(operand_i16(operands, 12)),
                        i32::from(operand_i16(operands, 13) as u16),
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
                    room_items_flag: 0xFF,
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
                    room_items_flag: 0xFF,
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
                    action.kind = RoomActionKind::from_sce(handler);
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
                    action.kind = RoomActionKind::from_sce(handler);
                    action.params[0] = handler;
                    action.params[1] = flags;
                }
                StepResult::Continue
            }
            // `evt_exec`: queue an event script. The event VM starts it on the
            // next tick, exactly like the original's `cmd_scd_event_create`.
            0x14 => {
                let slot = operand_u8(operands, 1);
                let event = operand_u8(operands, 2);
                self.state.pending_events.push((slot, event));
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
                // Byte layout: slot/rot, zone x4, item type, entry flags, model
                // index, sca parent, model xyz, anim, roomItems flag, entry
                // flags, flags word.
                let slot = operand_u8(operands, 0) & 0x7F;
                let item = operand_u8(operands, 5);
                let quantity = operand_u8(operands, 6);
                let model = operand_u8(operands, 7);
                let parent = operand_u8(operands, 8);
                let x = operand_i16(operands, 9).to_le_bytes();
                let z = operand_i16(operands, 11).to_le_bytes();
                let room_items_flag = operand_u8(operands, 13);
                // A set roomItems flag means the item was already taken.
                if self.state.flag_test(7, room_items_flag, false) {
                    return StepResult::Continue;
                }
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
                    room_items_flag,
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

    fn on_enemy(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            // pos_set: enemyIdx, position.pad, yaw, roll, x, y, z.
            0x21 => {
                let index = operand_u8(operands, 0);
                if let Some(entity) = self.state.entities.get_mut(usize::from(index) + 1) {
                    entity.pos = [
                        i32::from(operand_i16(operands, 4)),
                        i32::from(operand_i16(operands, 5)),
                        i32::from(operand_i16(operands, 6)),
                    ];
                    entity.pitch = operand_i16(operands, 1) as u16;
                    entity.angle = operand_i16(operands, 2) as u16 & 0x0FFF;
                    entity.roll = operand_i16(operands, 3) as u16;
                    entity.flags &= 0xFFF3;
                    entity.active = true;
                }
                StepResult::Continue
            }
            // spd_set: entity index (0 player), new posY word.
            0x41 => {
                let slot = usize::from(operand_u8(operands, 0));
                if let Some(entity) = self.state.entities.get_mut(slot) {
                    entity.pos[1] = i32::from(operand_i16(operands, 1));
                }
                StepResult::Continue
            }
            _ => self.placeholder(op),
        }
    }

    fn on_player(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            // dir_set: pad, position.pad (pitch), yaw, speed.x, x, y, z.
            0x20 => {
                let entity = &mut self.state.entities[0];
                entity.pos = [
                    i32::from(operand_i16(operands, 4)),
                    i32::from(operand_i16(operands, 5)),
                    i32::from(operand_i16(operands, 6)),
                ];
                entity.pitch = operand_i16(operands, 1) as u16;
                entity.angle = operand_i16(operands, 2) as u16 & 0x0FFF;
                entity.move_speed[0] = operand_i16(operands, 3);
                entity.flags &= 0xFFF3;
                entity.active = true;
                StepResult::Continue
            }
            // spd_add: signed byte added to the player posY.
            0x45 => {
                let entity = &mut self.state.entities[0];
                entity.pos[1] = entity.pos[1].wrapping_add(i32::from(operand_i8(operands, 0)));
                StepResult::Continue
            }
            _ => self.placeholder(op),
        }
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
        match op.mnemonic {
            "evt_work_set" => {
                self.state
                    .select_entity(operand_u8(operands, 0), operand_u8(operands, 1));
                StepResult::Continue
            }
            "evt_tween_begin" => {
                self.state.reset_tween_entity();
                StepResult::Continue
            }
            mnemonic if mnemonic.starts_with("act_") => self.state.apply_actor_op(op, operands),
            mnemonic if mnemonic.starts_with("tw_") => self.state.apply_tween_op(op, operands),
            _ => self.placeholder(op),
        }
    }

    fn on_select_entity(&mut self, entity_type: u8, index: u8) {
        self.state.select_entity(entity_type, index);
    }

    fn flag_test(&mut self, bank: u8, bit: u8, expected: bool) -> bool {
        self.state.flag_test(bank, bit, expected)
    }
}

/// The BGM state bit for `channel`, or zero when the channel is out of range.
fn channel_bit(channel: u8) -> u8 {
    1u8.checked_shl(u32::from(channel) + 3).unwrap_or(0)
}

/// The entity slot an `act_motion` target pair resolves to: type 0 the player,
/// type 1 enemy `index` (slot `index + 1`). Object and item models have no
/// entity this slice.
fn motion_target_slot(entity_type: u8, index: i16) -> Option<u8> {
    match entity_type {
        0 => Some(0),
        1 => u8::try_from(index)
            .ok()
            .and_then(|index| 1u8.checked_add(index))
            .filter(|slot| usize::from(*slot) < ENTITY_COUNT),
        _ => None,
    }
}

/// Distance between two XZ offsets, rounded down.
fn xz_distance(dx: i32, dz: i32) -> i32 {
    let dx = i64::from(dx);
    let dz = i64::from(dz);
    ((dx * dx + dz * dz) as f64).sqrt() as i32
}

/// 12-bit angle from `from` to `to` on the XZ plane. Angle 0 faces +X and
/// increasing yaw turns towards -Z, the engine's movement convention.
fn angle_between(from: [i32; 3], to: [i32; 3]) -> u16 {
    let dx = (to[0] as i16).wrapping_sub(from[0] as i16);
    let dz = (to[2] as i16).wrapping_sub(from[2] as i16);
    if dx != 0 {
        let slope = (i32::from(dz) * 4096) / i32::from(dx);
        let angle = (f64::from(slope) / 4096.0).atan() * (2048.0 / std::f64::consts::PI);
        let quadrant = if dx < 0 { 0x800 } else { 0 };
        return ((-(quadrant + angle as i32)) as u32 & 0x0FFF) as u16;
    }
    ((if dz > 0 { 0x800 } else { 0 }) + 0x400) as u16
}

/// Step `angle` toward `target` by at most `step`, using unsigned 12-bit
/// angle arithmetic: snap when the remaining turn is under two steps,
/// otherwise take one step in the shorter direction.
fn rotate_toward(angle: u16, target: u16, step: u16) -> u16 {
    let delta = step.wrapping_sub(angle).wrapping_add(target) & 0x0FFF;
    if i32::from(delta) < i32::from(step as i16) * 2 {
        return target & 0x0FFF;
    }
    let mut turned = angle.wrapping_sub(step) & 0x0FFF;
    if delta < 0x801 {
        turned = turned.wrapping_add(step.wrapping_mul(2)) & 0x0FFF;
    }
    turned
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

fn operand_i8(operands: &[Operand], index: usize) -> i8 {
    operands.get(index).map_or(0, |operand| operand.value as i8)
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
    use crate::scd::ir::{Decoded, Insn, Scripts, Stream, StreamKind};
    use crate::scd::opcode::{actor_op, command_op, event_control_op, event_top_op, tween_op};
    use crate::scd::vm::EventVm;

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

    fn insn(offset: usize, op: &'static Op, decoded: Decoded, len: usize, values: &[i64]) -> Insn {
        Insn {
            offset,
            op: op.op,
            bytes: vec![op.op; len],
            decoded,
            operands: operands(values),
        }
    }

    fn event_insn(offset: usize, op: &'static Op, len: usize, values: &[i64]) -> Insn {
        insn(offset, op, Decoded::Event(op), len, values)
    }

    fn actor_insn(offset: usize, op: &'static Op, len: usize, values: &[i64]) -> Insn {
        insn(offset, op, Decoded::Actor(op), len, values)
    }

    fn tween_insn(offset: usize, op: &'static Op, len: usize, values: &[i64]) -> Insn {
        insn(offset, op, Decoded::Tween(op), len, values)
    }

    fn control_insn(offset: usize, op: &'static Op, len: usize, values: &[i64]) -> Insn {
        insn(offset, op, Decoded::Control(op), len, values)
    }

    fn event_scripts(events: Vec<Vec<Insn>>) -> Scripts {
        Scripts {
            events: events
                .into_iter()
                .enumerate()
                .map(|(index, insns)| Stream {
                    kind: StreamKind::Event(index as u8),
                    offset: 0,
                    insns,
                    trailing: Vec::new(),
                })
                .collect(),
            ..Scripts::default()
        }
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
                host.on_player(op(0x2B), &operands(&[0])),
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
        assert_eq!(state.placeholders[&0x2B], 1);
        assert_eq!(state.placeholders[&0x1F], 1);
        assert_eq!(state.placeholders[&0x2A], 1);
        assert_eq!(state.placeholders[&0x25], 1);
        assert_eq!(state.placeholders[&0x29], 1);
        assert_eq!(state.placeholders[&0x40], 1);
        assert_eq!(state.placeholders[&0x27], 1);
        assert_eq!(state.placeholders[&0x4C], 1);
    }

    #[test]
    fn evt_work_set_selects_the_entity_slot() {
        let mut state = game();
        let op = event_top_op(0x04).unwrap();
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_misc(op, &operands(&[1, 2, 0])),
                StepResult::Continue
            );
            assert_eq!(host.state().selected_entity, 3);
            assert_eq!(
                host.on_misc(op, &operands(&[0, 9, 0])),
                StepResult::Continue
            );
            assert_eq!(host.state().selected_entity, 0);
            assert_eq!(
                host.on_misc(op, &operands(&[2, 0, 0])),
                StepResult::Continue
            );
            assert_eq!(host.state().selected_entity, ENTITY_NONE);
        }
    }

    #[test]
    fn dir_set_writes_the_player_entity() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_player(op(0x20), &operands(&[0, 0, 2048, 0, 6800, 0, 9000])),
                StepResult::Continue
            );
        }
        assert_eq!(state.entities[0].pos, [6800, 0, 9000]);
        assert_eq!(state.entities[0].angle, 2048);
        assert!(state.entities[0].active);
    }

    #[test]
    fn actor_motion_walks_the_entity_and_ends_the_state() {
        let scripts = event_scripts(vec![vec![
            event_insn(0x1000, event_top_op(0x01).unwrap(), 1, &[]),
            actor_insn(0x1001, actor_op(0x81).unwrap(), 10, &[1, 100, 0, 0, 10, 0]),
            control_insn(0x100B, event_control_op(0xFF).unwrap(), 1, &[]),
        ]]);
        let mut state = game();
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        for _ in 0..20 {
            let mut host = ScdGameHost::new(&mut state);
            vm.step(&mut host);
        }
        assert_eq!(vm.active_slots(), 0);
        assert_eq!(state.entities[0].pos, [100, 0, 0]);
        assert_eq!(state.entities[0].target_entity, None);
    }

    #[test]
    fn actor_motion_wraps_negative_coordinates() {
        let mut state = game();
        state.selected_entity = 0;
        let op = actor_op(0x81).unwrap();
        let mut host = ScdGameHost::new(&mut state);
        // flags 0x21: motion plus the +0x1000 wrap for negative x/y.
        assert_eq!(
            host.on_misc(op, &operands(&[0x21, -100, -200, 0, 4, 0])),
            StepResult::Yield
        );
        assert_eq!(
            host.state().entities[0].target,
            [0x1000 - 100, 0x1000 - 200, 0]
        );
    }

    #[test]
    fn actor_target_entity_uses_its_position() {
        let mut state = game();
        state.selected_entity = 0;
        state.entities[1].pos = [50, 0, 0];
        let mut host = ScdGameHost::new(&mut state);
        let op = actor_op(0x81).unwrap();
        // flags 0x93: target entity type 1, index 0.
        assert_eq!(
            host.on_misc(op, &operands(&[0x93, 1, 0, 0, 4, 0])),
            StepResult::Yield
        );
        assert_eq!(host.state().entities[0].target, [50, 0, 0]);
        assert_eq!(host.state().entities[0].target_entity, Some(1));
    }

    #[test]
    fn tween_position_integrates_over_ticks() {
        let scripts = event_scripts(vec![vec![
            event_insn(0x2000, event_top_op(0x03).unwrap(), 1, &[]),
            tween_insn(0x2001, tween_op(0x05).unwrap(), 4, &[0, 0, 10]),
            tween_insn(0x2005, tween_op(0x07).unwrap(), 8, &[0, 0, 0, 0]),
            tween_insn(0x200D, tween_op(0x02).unwrap(), 1, &[]),
            control_insn(0x200E, event_control_op(0xFE).unwrap(), 1, &[]),
            tween_insn(0x200F, tween_op(0x02).unwrap(), 1, &[]),
            control_insn(0x2010, event_control_op(0xFE).unwrap(), 1, &[]),
            tween_insn(0x2011, tween_op(0x02).unwrap(), 1, &[]),
            tween_insn(0x2012, tween_op(0x01).unwrap(), 1, &[]),
            control_insn(0x2013, event_control_op(0xFF).unwrap(), 1, &[]),
        ]]);
        let mut state = game();
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        for _ in 0..6 {
            let mut host = ScdGameHost::new(&mut state);
            vm.step(&mut host);
        }
        assert_eq!(vm.active_slots(), 0);
        assert_eq!(state.entities[0].pos, [0, 0, 30]);
    }

    #[test]
    fn tween_rotation_adds_the_step_fields() {
        let mut state = game();
        state.entities[0].move_step = [5, 0, 7];
        state.entities[0].state_field = 9;
        let mut host = ScdGameHost::new(&mut state);
        let op = tween_op(0x03).unwrap();
        assert_eq!(host.on_misc(op, &operands(&[])), StepResult::Continue);
        let entity = host.state().entities[0];
        assert_eq!(entity.pitch, 5);
        assert_eq!(entity.angle, 7);
        assert_eq!(entity.roll, 9);
    }

    #[test]
    fn tween_absolute_and_selector_ops_write_the_entity() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_misc(tween_op(0x07).unwrap(), &operands(&[10, 20, 30, 0])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_misc(tween_op(0x0B).unwrap(), &operands(&[1, 2, 3, 0])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_misc(tween_op(0x0A).unwrap(), &operands(&[3, 96, 0])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_misc(tween_op(0x09).unwrap(), &operands(&[7])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_misc(tween_op(0x08).unwrap(), &operands(&[2, 4])),
            StepResult::Continue
        );
        let entity = host.state().entities[0];
        assert_eq!(entity.pos, [10, 20, 30]);
        assert_eq!(entity.pitch, 1);
        assert_eq!(entity.angle, 2);
        assert_eq!(entity.roll, 3);
        assert_eq!(entity.health, 96);
        assert_eq!(entity.selector, 3);
        assert_eq!(entity.hit_state, 7);
        assert_eq!(entity.behavior, 4);
    }

    #[test]
    fn tween_begin_resets_the_selected_entity() {
        let mut state = game();
        state.entities[0].behavior = 4;
        state.entities[0].action_state = 9;
        {
            let mut host = ScdGameHost::new(&mut state);
            let op = event_top_op(0x02).unwrap();
            assert_eq!(host.on_misc(op, &operands(&[])), StepResult::Continue);
        }
        assert_eq!(state.entities[0].behavior, 0);
        assert_eq!(state.entities[0].action_state, 0);
        assert_eq!(state.entities[0].ignore(), 2);
    }

    #[test]
    fn sync_player_mirrors_entity_zero_into_the_player() {
        let mut state = game();
        let mut player =
            crate::player::spawn(RoomId::parse("1001").unwrap(), &RoomState::default());
        state.entities[0].pos = [100, 0, 200];
        state.entities[0].angle = 0x456;
        state.sync_player(&mut player);
        assert_eq!(player.pos, [100, 0, 200]);
        assert_eq!(player.angle, 0x456);

        state.sync_player(&mut player);
        assert_eq!(player.pos, [100, 0, 200]);
        assert_eq!(player.angle, 0x456);
    }

    #[test]
    fn angle_and_rotation_helpers_follow_the_engine_convention() {
        assert_eq!(angle_between([0, 0, 0], [100, 0, 0]), 0);
        assert_eq!(angle_between([0, 0, 0], [0, 0, -100]), 0x400);
        assert_eq!(angle_between([0, 0, 0], [0, 0, 100]), 0xC00);
        assert_eq!(angle_between([50, 0, 50], [50, 0, 50]), 0x400);

        assert_eq!(rotate_toward(0, 0x400, 0x100), 0x100);
        assert_eq!(rotate_toward(0x400, 0, 0x100), 0x300);
        assert_eq!(rotate_toward(0, 0xC00, 0x100), 0xF00);
        let mut angle = 0x3F0;
        for _ in 0..4 {
            angle = rotate_toward(angle, 0, 0x100);
        }
        assert_eq!(angle, 0);
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
            room_items_flag: 0xFF,
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
            room_items_flag: 0xFF,
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
            direction: 0,
            sfx: 0,
            door_type: 0,
            camera: 0,
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
            room_items_flag: 0xFF,
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
    fn aot_reset_to_event_queues_the_script() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            host.on_room_action(op(0x0D), &operands(&[0, 0, 0, 10, 10, 9, 0xC1, 9, 7, 0]));
            host.on_room_action(op(0x12), &operands(&[0, 9, 0xC1, 9, 7, 0]));
        }
        let action = state.room_actions[0].expect("event action");
        assert_eq!(action.kind, RoomActionKind::Event);
        state.interact([5, 0, 5], 0, true);
        assert_eq!(state.pending_events, vec![(9, 7)]);
    }

    #[test]
    fn picked_up_items_stay_taken() {
        let mut state = game();
        let item_operands = [
            3, 0, 0, 100, 100, 0x42, 1, 1, 0xFF, 50, 0, 50, 0, 23, 0x81, 0,
        ];
        {
            let mut host = ScdGameHost::new(&mut state);
            host.on_item(op(0x18), &operands(&item_operands));
        }
        assert!(state.room_actions[3].is_some());
        state.interact([-550, 0, 50], 0, true);
        assert_eq!(state.last_picked_item, Some(0x42));
        assert!(
            state.flag_test(7, 23, false),
            "a pickup must set its room-items flag"
        );

        // Re-running the init script must not bring the item back.
        {
            let mut host = ScdGameHost::new(&mut state);
            host.on_item(op(0x18), &operands(&item_operands));
        }
        assert!(state.room_actions[3].is_none());
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

        state.interact([-550, 0, 50], 0, false);
        assert!(
            state.inventory.is_empty(),
            "no pickup without the action key"
        );

        // Action-key entries fire the first match only, as in the original;
        // the next press reaches the second stack.
        state.interact([-550, 0, 50], 0, true);
        assert_eq!(
            state.inventory,
            vec![InventoryItem {
                id: 10,
                quantity: 3
            }]
        );
        assert_eq!(state.last_picked_item, Some(10));
        assert_eq!(state.item_events, vec![10]);

        state.interact([-550, 0, 50], 0, true);
        assert_eq!(
            state.inventory,
            vec![InventoryItem {
                id: 10,
                quantity: 5
            }]
        );
        assert_eq!(state.item_events, vec![10, 10]);
        assert!(state.room_actions.iter().all(Option::is_none));

        state.interact([-550, 0, 50], 0, true);
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
            room_items_flag: 0xFF,
        });
        state.interact([-550, 0, 50], 0, true);
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
            room_items_flag: 0xFF,
        });
        state.interact([-550, 0, 50], 0, true);
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
    fn locked_door_needs_the_key_and_unlocks_for_good() {
        let mut state = game();
        let mut locked = door(1, 0x80 | 5);
        locked.key = 0x34;
        state.room_actions[0] = Some(door_action(locked));
        state.doors[0] = Some(locked);

        // Without the key: "it's locked" (message 200 + key index).
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, Some(LOCKED_MESSAGE + 1));
        assert!(state.message.active);

        // With the key: the key turns, is consumed and raises the lock flag.
        state.add_item(0x34, 1);
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, Some(0xC3));
        assert!(!state.has_item(0x34), "the key is consumed by the turn");
        assert!(state.flag_test(2, 5, false));

        // The next probe walks through.
        state.interact([-550, 0, 50], 0, true);
        assert_eq!(
            state.transition,
            Some(RoomTransition {
                target: RoomId {
                    stage: 1,
                    room: 1,
                    player_flag: 1,
                },
                pos: [555, 0, 666],
                angle: 1024,
            })
        );
    }

    #[test]
    fn door_lock_flag_already_set_opens_without_a_key() {
        let mut state = game();
        let locked = door(1, 0x80 | 5);
        state.room_actions[0] = Some(door_action(locked));
        state.doors[0] = Some(locked);

        assert!(state.apply_flag(2, 5, 0));
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_some());
    }

    #[test]
    fn unlocked_door_with_a_character_restriction() {
        // Lock byte 0x40: bit 7 clear, so the door is not locked, but only
        // Chris may use it. The old code treated the low bits as a lock flag.
        let mut state = game();
        let restricted = door(1, 0x40);
        state.room_actions[0] = Some(door_action(restricted));
        state.doors[0] = Some(restricted);

        // Jill (player flag 1) is turned away.
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, Some(0xD6));

        // Chris walks through.
        state.id.player_flag = 0;
        state.transition = None;
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_some());
    }

    #[test]
    fn sword_key_lock_accepts_jills_lockpick() {
        let mut state = game();
        let mut locked = door(1, 0x80 | 7);
        locked.key = ITEM_SWORD_KEY;
        state.room_actions[0] = Some(door_action(locked));
        state.doors[0] = Some(locked);

        // Jill without the lockpick: the special "no lockpick" message.
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, Some(0xD5));

        // With the scenario flag: the lock turns without an inventory item.
        assert!(state.apply_flag(0, SCENARIO_FLAG_HAS_LOCKPICK, 0));
        state.interact([-550, 0, 50], 0, true);
        assert_eq!(state.message.id, Some(0xC3));
        assert!(state.flag_test(2, 7, false));
    }

    #[test]
    fn other_side_key_unlocks_on_the_first_probe() {
        let mut state = game();
        let mut locked = door(1, 0x80 | 9);
        locked.key = 0xFE;
        state.room_actions[0] = Some(door_action(locked));
        state.doors[0] = Some(locked);

        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, Some(0xD4));
        assert!(state.flag_test(2, 9, false));

        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_some());
    }

    #[test]
    fn dead_key_door_stays_locked() {
        let mut state = game();
        let mut locked = door(1, 0x80 | 9);
        locked.key = 0xFF;
        state.room_actions[0] = Some(door_action(locked));
        state.doors[0] = Some(locked);

        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, Some(MESSAGE_LOCKED_KEY));
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
    }

    #[test]
    fn unlocked_door_transitions_without_a_flag() {
        let mut state = game();
        state.room_actions[0] = Some(door_action(door(0x1F, 0)));
        state.doors[0] = Some(door(0x1F, 0));
        state.interact([-550, 0, 50], 0, true);
        let transition = state.transition.expect("transition");
        assert_eq!(transition.target.room, 0x1F);
        assert_eq!(transition.pos, [555, 0, 666]);
    }

    #[test]
    fn cross_stage_door_destination_encoding() {
        let mut state = game();
        // 0x41 = stage index (0x41 >> 5) - 1 = 1 -> stage 2, room 1.
        state.room_actions[0] = Some(door_action(door(0x41, 0)));
        state.doors[0] = Some(door(0x41, 0));
        state.interact([-550, 0, 50], 0, true);
        let target = state.transition.expect("transition").target;
        assert_eq!(target.stage, 2);
        assert_eq!(target.room, 1);

        // With the stage-variant scenario flag, a stage 0/1 destination maps to
        // the +5 return variants: 0x20 -> stage index 5 -> stage digit 6.
        let mut state = game();
        assert!(state.apply_flag(0, SCENARIO_FLAG_STAGE_VARIANT, 0));
        state.room_actions[0] = Some(door_action(door(0x20, 0)));
        state.doors[0] = Some(door(0x20, 0));
        state.interact([-550, 0, 50], 0, true);
        let target = state.transition.expect("transition").target;
        assert_eq!(target.stage, 6);
        assert_eq!(target.room, 0);
    }

    #[test]
    fn door_probe_flags_select_the_trigger() {
        // 0x41: probed every frame at the player position, no action key.
        let mut state = game();
        let mut auto = door(1, 0);
        auto.sub_type = 0x41;
        state.room_actions[0] = Some(door_action(auto));
        state.doors[0] = Some(auto);
        state.interact([50, 0, 50], 0, false);
        assert!(state.transition.is_some(), "0x41 doors trigger on walk-in");

        // 0xC1: action key, player position (not the reach point).
        let mut state = game();
        let mut pos_key = door(1, 0);
        pos_key.sub_type = 0xC1;
        state.room_actions[0] = Some(door_action(pos_key));
        state.doors[0] = Some(pos_key);
        state.interact([-550, 0, 50], 0, true);
        assert!(
            state.transition.is_none(),
            "the player, not the reach point, must be in the zone"
        );
        state.interact([50, 0, 50], 0, true);
        assert!(state.transition.is_some());

        // 0x00: neither probe fires it.
        let mut state = game();
        let mut dead = door(1, 0);
        dead.sub_type = 0x00;
        state.room_actions[0] = Some(door_action(dead));
        state.doors[0] = Some(dead);
        state.interact([50, 0, 50], 0, true);
        assert!(state.transition.is_none());
    }

    #[test]
    fn out_of_range_door_target_is_ignored() {
        let mut state = game();
        state.room_actions[0] = Some(door_action(door(0xFF, 0)));
        state.doors[0] = Some(door(0xFF, 0));
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, None);
    }

    #[test]
    fn run_room_action_handler_zero_is_inert() {
        let mut state = game();
        state.room_actions[1] = Some(item_action(1, 7, 1, [0, 0, 100, 100]));
        state.room_actions[1].as_mut().unwrap().handler = 0;
        state.interact([-550, 0, 50], 0, true);
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

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn room_1001_events_run_and_dir_set_moves_the_player() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let path = std::path::Path::new(&root).join("JPN/STAGE1/ROOM1001.RDT");
        let data = std::fs::read(&path).expect("read ROOM1001.RDT");
        let id = RoomId::parse("1001").unwrap();
        let room = crate::rdt::parse(&data, id).unwrap();
        let scripts = crate::scd::reader::parse(&data).unwrap();

        let mut state = GameState::new(id, &room);
        let mut command_vm = crate::scd::vm::CommandVm::new(&scripts);
        let mut event_vm = EventVm::new(&scripts);
        {
            let mut host = ScdGameHost::new(&mut state);
            command_vm.run_init(&mut host);
        }
        // A fresh state has the flags that gate `evt_exec` clear, so start
        // event_00 directly in a free slot.
        event_vm.start(9, 0);
        for _ in 0..30 {
            {
                let mut host = ScdGameHost::new(&mut state);
                command_vm.run_main(&mut host);
            }
            for (slot, event) in std::mem::take(&mut state.pending_events) {
                event_vm.start(usize::from(slot), event);
            }
            {
                let mut host = ScdGameHost::new(&mut state);
                event_vm.step(&mut host);
            }
            state.advance_frame();
        }

        let entity = state.entities[0];
        assert_eq!(entity.pos, [6800, 0, 9000], "dir_set position applied");
        assert_eq!(entity.angle, 2048);
        assert_eq!(entity.flags & 0x40, 0x40, "act_flag_op ran");
        assert_eq!(entity.health, 96, "tw_set_sel ran");
    }
}
