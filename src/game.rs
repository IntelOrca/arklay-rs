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
//! interaction. The scripted characters (`0x20..=0x2E`) allocate entities,
//! render and run their native driver; monsters still allocate nothing and
//! effects stay recorded placeholders until their systems exist.

use std::collections::BTreeMap;

use crate::items;
use crate::message::{MessageAction, MessageInput, MessageWindow};
use crate::player::PlayerState;
use crate::scd::host::{ScdHost, StepResult};
use crate::scd::ir::Operand;
use crate::scd::opcode::Op;
use crate::stairs::{self, StairEntryState, StairZones};
use crate::state::{RoomId, RoomState};
use crate::text::Text;

/// Number of flag banks the scripts can address.
pub const FLAG_BANK_COUNT: usize = 10;
/// Bytes per flag bank.
pub const FLAG_BANK_BYTES: usize = 32;
/// Bytes in the BioCard-like state block.
pub const STATE_BYTES: usize = 64;
/// Words in the fading-state block.
pub const STATE_WORDS: usize = 32;
/// State byte holding the current room camera id (BioCard 0x202).
pub const STATE_BYTE_ROOM_CAMERA: u8 = 2;
/// State byte holding the cut saved by `cutnext` (BioCard 0x204).
pub const STATE_BYTE_CUT: u8 = 4;
/// State byte holding the message-window menu choice (BioCard 0x205).
pub const STATE_BYTE_MENU_CHOICE: u8 = 5;
/// State byte holding the selected inventory item (BioCard 0x206).
pub const STATE_BYTE_SELECTED_ITEM: u8 = 6;
/// State byte holding the number of held inventory slots (BioCard 0x207).
pub const STATE_BYTE_TOTAL_HELD: u8 = 7;
/// State byte holding the action hit by the forward position probe.
pub const STATE_BYTE_FWD_ACTION: u8 = 16;
/// State byte holding the action hit by the entity position probe.
pub const STATE_BYTE_ENT_ACTION: u8 = 17;
/// State byte holding the last used item (BioCard 0x212).
pub const STATE_BYTE_USED_ITEM: u8 = 18;
/// State byte holding the last picked item (BioCard 0x213).
pub const STATE_BYTE_PICKED_ITEM: u8 = 19;
/// State byte holding the save counter (BioCard 0x228).
pub const STATE_BYTE_SAVES: u8 = 40;
/// State byte holding the equipped item (BioCard 0x229).
pub const STATE_BYTE_EQUIPPED: u8 = 41;
/// State byte holding the selected character (BioCard 0x22B).
pub const STATE_BYTE_CHARACTER: u8 = 43;
/// State byte holding the health status flags (BioCard 0x232).
pub const STATE_BYTE_HEALTH_STATUS: u8 = 50;
/// Inventory slots Chris can use (the first half of the 12 slot bytes).
pub const INVENTORY_SLOTS_CHRIS: usize = 6;
/// Inventory slots Jill can use (Chris's six plus two more).
pub const INVENTORY_SLOTS_JILL: usize = 8;
/// Item box slots stored in the save block.
pub const ITEM_BOX_SLOTS: usize = 48;
/// First document item id; ids here are the FILE tab's list entries.
pub const FILE_ITEM_MIN: u8 = 0x5F;
/// Last document item id.
pub const FILE_ITEM_MAX: u8 = 0x6E;
/// Number of collectable documents (the FILE-collected bit block).
pub const FILE_COUNT: usize = 16;
/// Flag bank holding the visited-room, map and file bit blocks
/// (`g_RoomFlags` in the original).
pub const BANK_ROOM_FLAGS: u8 = 8;
/// First file-collected bit in the room-flags bank: `0x82 + (item - 0x5F)`.
pub const ROOM_FLAG_FILE_BASE: u8 = 0x82;
/// Number of room action slots the game state tracks.
pub const ROOM_ACTION_SLOTS: usize = 20;
/// Entity slots: 0 is the player, 1.. are enemies and scripted objects.
pub const ENTITY_COUNT: usize = 32;
/// `selected_entity` value when the event pointed at something this slice does
/// not model as an entity (object models and item models).
pub const ENTITY_NONE: u8 = u8::MAX;
/// `status_flags` bit 0: the entity is active/visible.
pub const ENTITY_STATUS_ACTIVE: u8 = 0x01;
/// Flag bank holding the `enemy` guard bits (`g_EnemiesFlags`).
pub const BANK_ENEMIES: u8 = 3;
/// Flag bank holding the system flags (`g_SysFlags`). The state-8 handlers
/// raise their completion bits here for the event scripts' `bit_test`s.
pub const BANK_SYSTEM: u8 = 4;
/// Collision radius the `enemy` spawn gives an entity before its own init
/// overrides it.
pub const DEFAULT_ENEMY_RADIUS: i16 = 422;
/// Seed the per-frame NPC random sequence starts from. Any non-zero value
/// works; the sequence only has to be deterministic across runs.
const RAND_SEED_INITIAL: u16 = 0xACE1;
/// First entity id that is a scripted character.
pub const CHARACTER_ID_MIN: u8 = 0x20;
/// Last entity id that is a scripted character.
pub const CHARACTER_ID_MAX: u8 = 0x2E;
/// Default per-tick yaw step of an `act_motion` instruction.
const MOTION_DEFAULT_STEP: u8 = 0xC0;
/// Default per-tick pitch step of an `act_motion` instruction.
const MOTION_DEFAULT_PITCH_STEP: u8 = 0x40;
/// Monster animation remap pairs used by `act_action_a`: `(action_state base,
/// animation id)` indexed by the incoming animation id. Characters never take
/// this path; monster entities stay inert this milestone, but the table is
/// kept for the opcode's byte-for-byte behaviour.
const SCD_ANIM_REMAP: [u8; 32] = [
    0, 0, 0, 1, 0, 2, 0, 3, 0, 4, 1, 0, 1, 1, 1, 2, 1, 3, 1, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];
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
/// Item id of the lockpick, which the use action never consumes.
const ITEM_LOCK_PICK: u8 = 0x31;
/// First ammunition item id; anything below it is a weapon.
const ITEM_CLIP: u8 = 0x0B;
/// First door-key item id for the depletion rule.
const ITEM_OIL: u8 = 0x32;
/// Last door-key item id for the depletion rule.
const ITEM_DESK_KEY: u8 = 0x3D;
/// The radio is not an inventory item; taking it raises a scenario flag.
const ITEM_COMM_RADIO: u8 = 0x4D;
/// Scenario flag raised when the radio is taken.
const SCENARIO_FLAG_HAS_RADIO: u8 = 0x7F;
/// Scenario flag raised when Jill has the lockpick.
const SCENARIO_FLAG_HAS_LOCKPICK: u8 = 0x7C;
/// `main_state_flags` bit `0x2000`: the selected key was used up.
const MSF_MENU_KEY_DEPLETED: u8 = 18;
/// `g_message_flags` bit 8: gameplay control. A message's pause word is
/// masked out of [`GameState::message_flags`] while it is displayed; when the
/// bit is clear the original blanks the player's d-pad for the frame.
pub const MESSAGE_FLAG_CONTROLS: u16 = 0x100;
/// `g_message_flags` bit 1: entities may think. This is a different bit from
/// [`MESSAGE_FLAG_CONTROLS`]: a pause word can freeze the characters without
/// locking the player (and vice versa), so the entity tick gates on this one.
pub const MESSAGE_FLAG_ENTITIES: u16 = 0x002;
/// The original's gameplay seed for `g_message_flags` (`game_loop` writes
/// `0xFD3F` when it (re)enters the play state).
pub const MESSAGE_FLAGS_INITIAL: u16 = 0xFD3F;
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
/// `room_check_actions` index of the stair/ladder entry handler.
const HANDLER_STAIRS_ZONE: u8 = 0x0C;
/// `room_check_actions` index of the stair height ramp handler.
const HANDLER_STAIRS_HEIGHT: u8 = 0x11;
/// `main_state_flags` bit 0x400, raised when `give_item` runs.
const MSF_MENU_GOT_ITEM: u8 = 21;
/// `main_state_flags` bit 0x800, toggled by `give_item` (item viewer).
const MSF_MENU_ITEM_VIEW: u8 = 20;
/// Flag bank holding the per-frame item-use flags (`g_itemUseFlags`).
pub const BANK_ITEM_USE: u8 = 9;
/// Scenario flag raised by the chemical combine effect.
pub const SCENARIO_FLAG_CHEMICAL_COMBINE: u8 = 0x16;
/// Scenario flag marking a second (hard) playthrough. While it is clear, a
/// typewriter may be used without an ink ribbon and the save prompt asks
/// "Will you save your progress?" instead of naming the ribbon.
pub const SCENARIO_FLAG_SECOND_PLAYTHROUGH: u8 = 0x7B;
/// Global message shown by a typewriter when no ink ribbon is held.
pub const MESSAGE_TYPEWRITER_NO_RIBBON: u8 = 0xDE;
/// Global prompt shown when a save will consume an ink ribbon.
pub const MESSAGE_TYPEWRITER_RIBBON_PROMPT: u8 = 0xDF;
/// Global prompt shown on a first Jill playthrough, when saving is free.
pub const MESSAGE_TYPEWRITER_SAVE_PROMPT: u8 = 0xE0;
/// Scenario-2 flag cleared when a `0x10` cure removes the poison bit `0x20`.
pub const SCENARIO2_FLAG_YAWN_POISONED: u8 = 0x43;
/// Item-use flag bit of the red book.
pub const ITEM_RED_BOOK_FLAG: u8 = 0x23;
/// Item id of the red book.
pub const ITEM_RED_BOOK: u8 = 0x3E;
/// First herb id (`red herb`); the herb range is `0x43..=0x4B`.
pub const HERB_MIN: u8 = 0x43;
/// Last herb id (`mixed herbs`).
pub const HERB_MAX: u8 = 0x4B;
/// First chemical id (`empty bottle`).
pub const CHEMICAL_MIN: u8 = 0x13;
/// Last chemical id.
pub const CHEMICAL_MAX: u8 = 0x1A;
/// Stage id of the guardhouse (1-based).
pub const STAGE_GUARDHOUSE: u8 = 3;
/// Room id of the drug storehouse in the guardhouse.
pub const ROOM_DRUG_STOREHOUSE: u8 = 9;

/// The character's maximum health: Chris (0) 140, Jill (1) 96. The original
/// derives it as `140 - 44 * (id & 1)` on both the new-game and continue paths.
pub fn character_max_health(character: u8) -> i16 {
    140 - 44 * i16::from(character & 1)
}

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

    /// The bank's raw bytes, for save/load.
    pub fn bytes(&self) -> &[u8; FLAG_BANK_BYTES] {
        &self.0
    }

    /// The bank's raw bytes, mutably, for save/load.
    pub fn bytes_mut(&mut self) -> &mut [u8; FLAG_BANK_BYTES] {
        &mut self.0
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

impl Default for InventoryItem {
    /// The empty slot: item id 0, quantity 0.
    fn default() -> Self {
        Self { id: 0, quantity: 0 }
    }
}

/// The FILE-tab list index of a document item id (`0x5F..=0x6E`), or `None`
/// for anything that is not a document. The index addresses the room-flags
/// file bits at [`ROOM_FLAG_FILE_BASE`].
pub fn file_index(item: u8) -> Option<u8> {
    (FILE_ITEM_MIN..=FILE_ITEM_MAX)
        .contains(&item)
        .then(|| item - FILE_ITEM_MIN)
}

/// Result of a menu USE action on an inventory item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseResult {
    /// The item was used. A heal reports what the EKG flush should show.
    Used {
        /// Health was restored.
        healed: bool,
        /// A poison status was cured.
        cured: bool,
    },
    /// The item has no effect in the current state; the menu shows the
    /// category's refusal message and does not consume it.
    Unusable,
}

/// Result of a menu MOVE/combine attempt between two inventory slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombineResult {
    /// The two items do not mix.
    NoRecipe,
    /// Chemicals can only be mixed in the guardhouse drug store.
    NeedsDrugStore,
    /// The recipe was applied.
    Applied {
        /// The recipe was a herb combination (needs the `0xF5` message).
        herb: bool,
        /// The recipe set the chemical-combine scenario flag.
        chemical: bool,
    },
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
    /// A stair/ladder entry registered by `set_stairs_zone` (handler `0x0C`).
    StairsZone,
    /// A Y ramp registered by `stairs_height_update` (handler `0x11`).
    StairsHeight,
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
            HANDLER_STAIRS_ZONE => Self::StairsZone,
            HANDLER_STAIRS_HEIGHT => Self::StairsHeight,
            _ => Self::Other,
        }
    }
}

/// The typewriter save flow, the part of the original's `g_typewriter_state`
/// the engine observes: idle, or a yes/no save prompt awaiting its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TypewriterFlow {
    /// No prompt is up.
    #[default]
    Idle,
    /// The save prompt is displayed; `ink_ribbon` records whether a confirmed
    /// save consumes one (a ribbon held by Chris, or by Jill on a second
    /// playthrough).
    Prompt { ink_ribbon: bool },
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
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
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

/// A mask-group visibility change requested by `aot_switch` (SCD 0x25).
///
/// The engine applies pending toggles to the current camera cut's
/// [`MaskLayer`](crate::render::MaskLayer) bits after the scripts run; a new
/// camera reload starts from the cut's own all-active bits, matching the
/// original's per-camera sprite table rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskToggle {
    /// One-based mask group id.
    pub group: u8,
    /// Whether the group should be visible.
    pub active: bool,
}

/// One 3D entity sound cue queued by an NPC handler (the original's
/// `PlayEntitySnd`), already resolved to a room sound name and position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntitySound {
    /// Room sound name resolved from the footstep zone.
    pub name: &'static str,
    /// World position the sound plays at.
    pub pos: [i32; 3],
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
    /// Entity type id; `0x20..=0x2E` are scripted characters.
    pub id: u8,
    /// Room position.
    pub pos: [i32; 3],
    /// 12-bit yaw.
    pub angle: u16,
    /// Rotation X component of the entity's rotation vector.
    pub pitch: u16,
    /// Rotation Z, the companion of `pitch` in the rotation SVECTOR.
    pub roll: u16,
    /// Entity status flags; bit 0 is active/visible.
    pub status_flags: u8,
    /// The spawn record's behaviour/weapon selector.
    pub behavior_flags: u8,
    /// Animation behavior byte (`action_behavior`).
    pub action_behavior: u8,
    /// Animation action state byte.
    pub action_state: u8,
    /// EDD clip id published by the driver.
    pub animation_id: u8,
    /// Scripted animation frame id published by the driver.
    pub animation_frame_id: u8,
    /// Hold counter of the animation clock (`timing_control`).
    pub timing_control: u8,
    /// Animation blend step (`blend_counter`).
    pub blend_counter: u8,
    /// This tick's walk speed.
    pub move_speed_current: u16,
    /// SCD animation timer (frames).
    pub scd_timer: u16,
    /// SCD animation parameter.
    pub scd_anim_param: u8,
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
    /// Animation position offsets (`unk_c6`/`unk_c8`); the state-8 walk
    /// handlers steer at this pair.
    pub unk_c6: u16,
    /// Animation position offset Z.
    pub unk_c8: u16,
    /// Collision callback flags (`collisionFlags`). `act_anim_seq` clears or
    /// sets bit 7; the state-8 walk handlers keep their behaviour running on
    /// arrival while it is set.
    pub collision_flags: u8,
    /// 16-bit frame/phase counter at entity +0xC4 (`action_ticks_counter`):
    /// the state-8 hold counter and the walk deceleration phase count.
    pub action_ticks_counter: u16,
    /// Look-at control byte set by `act_motion`.
    pub look_at_flags: u8,
    /// Look-at target position; for an entity target this is refreshed every
    /// tick from the target's current position.
    pub target: [i32; 3],
    /// Entity slot this entity is moving toward, when the target is an entity.
    pub target_entity: Option<u8>,
    /// Per-tick yaw step of the active `act_motion`.
    pub look_at_yaw_step: u8,
    /// Per-tick pitch step of the active `act_motion`.
    pub look_at_pitch_step: u8,
    /// Interaction/zone state bits (`zone_flags` in the original entity):
    /// `0x01` inside the current camera zone, `0x10` door swings the other way
    /// or ladder variant, `0x20` inside a stairs/ladder/door zone, `0x40` door
    /// direction modifier, `0x80` grabbed. `set_stairs_zone` raises `0x20`
    /// (and `0x10` for the ladder variant).
    pub zone_flags: u8,
    /// Scenario variant packed by the `enemy` spawn: the slot nibble, the
    /// record's selector high nibble and the force-init bit.
    pub variant: u8,
    /// Room event index to raise when the entity dies.
    pub death_event_id: u8,
    /// Collision radius in room units.
    pub sca_radius: i16,
    /// Whether the entity has entered a camera switch zone (behaviour
    /// scratch).
    pub has_enter_switch_zone: u8,
    /// Direction the entity is attacking towards (behaviour scratch).
    pub attacking_direction: u8,
    /// Movement direction-control flags (behaviour scratch).
    pub dir_control_flags: u8,
    /// Texture bank the model uses.
    pub tex_bank: u8,
    /// Action sequence counter/timer (behaviour scratch).
    pub seq_counter: u8,
    /// Turn delta accumulated by the walk behaviours.
    pub angle_turn_delta: u8,
    /// Effect countdown (footsteps/blood) for the current movement.
    pub move_timer: u8,
    /// Whether a walk behaviour is moving the entity.
    pub is_moving: u8,
    /// Maximum steps of the current movement sequence.
    pub move_max_steps: u8,
    /// Blood-splatter flag (behaviour scratch).
    pub splatter_flag: u8,
    /// Signed rotation speed for bobbing/weaving.
    pub bob_speed: u8,
    /// Waypoint X the pathfind behaviours steer at.
    pub player_pos_x: i16,
    /// Waypoint Z the pathfind behaviours steer at.
    pub player_pos_z: i16,
    /// 16-bit countdown at entity +0x176 (hit reaction / damage recovery). The
    /// state-9 walk layer reuses the slot as the walk heading (the target yaw
    /// of the current path step), exactly like the original.
    pub reaction_timer: i16,
    /// Joint visibility bits XORed by `eml_state` sub-command 9: bit `i` is
    /// joint `i`'s flag bit.
    pub joint_flags: u16,
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

    /// Whether the entity's active/visible status bit is set.
    pub fn active(&self) -> bool {
        self.status_flags & ENTITY_STATUS_ACTIVE != 0
    }

    /// Raise or clear the active/visible status bit.
    pub fn set_active(&mut self, active: bool) {
        if active {
            self.status_flags |= ENTITY_STATUS_ACTIVE;
        } else {
            self.status_flags &= !ENTITY_STATUS_ACTIVE;
        }
    }

    /// `tw_set_field`: store one byte of the entity state block at 0x84 plus
    /// `offset`. Returns `false` for an offset with no modelled field.
    fn set_state_byte(&mut self, offset: u8, value: u8) -> bool {
        match offset {
            0 => self.set_state(value),
            1 => self.set_ignore(value),
            2 => self.action_behavior = value,
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
    /// BioCard-like state bytes, indexed by their offset from BioCard 0x200
    /// (`0` stage, `1` room, `2` room camera id, `5` menu choice, `6` selected
    /// item, `7` total held, `18`/`19` used/picked item, ...).
    pub state_bytes: [u8; STATE_BYTES],
    /// Fading-state words.
    pub state_words: [u16; STATE_WORDS],
    /// Camera cut state.
    pub camera: CameraState,
    /// The room action slot a confirmed message's post-action pickup refers
    /// to (the original's `g_pRoomActionEntry`).
    pub message_item_slot: Option<u8>,
    /// The window draws its text on the menu line while the pause menu is up.
    pub message_menu: bool,
    /// The original's `g_message_flags`: [`GameState::show_message`] masks the
    /// request's pause word into it and the dismissal restores the captured
    /// backup. [`MESSAGE_FLAG_CONTROLS`] clear means the room tick must ignore
    /// the player's movement and action input.
    pub message_flags: u16,
    /// The `message_flags` captured when the active message was requested.
    message_flags_backup: u16,
    /// Message state.
    pub message: MessageWindow,
    /// BGM state.
    pub bgm: BgmState,
    /// The player's inventory.
    pub inventory: Vec<InventoryItem>,
    /// The room's action table.
    pub room_actions: [Option<RoomAction>; ROOM_ACTION_SLOTS],
    /// Door records, one per door action slot.
    pub doors: [Option<Door>; ROOM_ACTION_SLOTS],
    /// Stair zones collected from the live room action table.
    pub stair_zones: StairZones,
    /// Height the last `stairs_height_update` probe applied to the player.
    pub stair_height: Option<i32>,
    /// Stair/ladder entry latched by `set_stairs_zone` or a stair door.
    pub stair_entry: Option<StairEntryState>,
    /// The stair/ladder climb behaviour is active: the player's collision pass
    /// is suspended and the facing is held towards the climb target.
    pub stair_climb: bool,
    /// `main_state_flags` bit 4, raised by `set_stairs_zone` (ladder down).
    pub ladder_down: bool,
    /// Scripted entities; slot 0 is the player.
    pub entities: [Entity; ENTITY_COUNT],
    /// Per-slot animation clocks for the scripted entities, parallel to
    /// [`GameState::entities`]. The entity words stay authoritative; the clock
    /// remembers which frame the pose shows for the renderer.
    pub entity_anims: [crate::npc::EntityAnim; ENTITY_COUNT],
    /// Number of character entity slots an `enemy` spawn has allocated.
    pub enemy_count: u8,
    /// The `behavior_flags` read by the last `get_eml_state`.
    pub last_enemy_flags: u8,
    /// Entity slot the current event operates on, or [`ENTITY_NONE`].
    pub selected_entity: u8,
    /// The last item picked up by any path.
    pub last_picked_item: Option<u8>,
    /// The last item used from a menu.
    pub last_used_item: Option<u8>,
    /// The item currently equipped.
    pub equipped: Option<u8>,
    /// The item the inventory cursor last selected.
    pub selected_item: Option<u8>,
    /// The player's maximum health (`max_health >> 2` drives the EKG colour).
    pub max_health: i16,
    /// Health status flags (bit `0x20`/`0x02` poison).
    pub health_status: u8,
    /// The typewriter save prompt state (`g_typewriter_state`).
    pub typewriter: TypewriterFlow,
    /// The 48 item-box slots.
    pub item_box: [InventoryItem; ITEM_BOX_SLOTS],
    /// A room change requested by a door.
    pub transition: Option<RoomTransition>,
    /// The record that requested the pending [`GameState::transition`], so the
    /// engine can play its animation without re-probing the action table.
    pub transition_door: Option<Door>,
    /// Mask-group toggles requested by `aot_switch`, consumed by the engine.
    pub mask_toggles: Vec<MaskToggle>,
    /// Item ids picked up this room, in order, for tests.
    pub item_events: Vec<u8>,
    /// The last placeholder interaction recorded by the action layer.
    pub last_interaction: Option<RoomInteraction>,
    /// Fixed ticks elapsed.
    pub frame: u64,
    /// Call counts of opcodes whose systems do not exist yet.
    pub placeholders: BTreeMap<u8, u64>,
    /// Call counts of state-8 handlers that stay placeholders: the fire
    /// behaviour (`action_behavior` 8) and any out-of-range behaviour the
    /// original would have dispatched through a NULL table slot.
    pub npc_placeholders: BTreeMap<u8, u64>,
    /// BGM requests produced by the scripts.
    pub room_bgm_requests: Vec<BgmRequest>,
    /// Event scripts requested by `evt_exec`, consumed by the engine's event VM.
    pub pending_events: Vec<(u8, u8)>,
    /// 3D entity sound cues queued by the state-8/9 handlers, consumed by the
    /// engine's mixer.
    pub entity_sounds: Vec<EntitySound>,
    /// Per-frame random seed the NPC look-at scheduling reads. The original
    /// reseeds its global from `rand()` at the top of every gameplay frame;
    /// the port advances a deterministic xorshift so headless runs repeat.
    pub rand_seed: u16,
}

/// The initial entity array: only the player slot is spawned.
fn initial_entities() -> [Entity; ENTITY_COUNT] {
    let mut entities = [Entity::default(); ENTITY_COUNT];
    entities[0].set_active(true);
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
            message_item_slot: None,
            message_menu: false,
            message_flags: MESSAGE_FLAGS_INITIAL,
            message_flags_backup: MESSAGE_FLAGS_INITIAL,
            message: MessageWindow::default(),
            bgm: BgmState::default(),
            inventory: Vec::new(),
            room_actions: [None; ROOM_ACTION_SLOTS],
            doors: [None; ROOM_ACTION_SLOTS],
            stair_zones: StairZones::default(),
            stair_height: None,
            stair_entry: None,
            stair_climb: false,
            ladder_down: false,
            entities: initial_entities(),
            entity_anims: std::array::from_fn(|_| crate::npc::EntityAnim::default()),
            enemy_count: 0,
            last_enemy_flags: 0,
            selected_entity: 0,
            last_picked_item: None,
            last_used_item: None,
            equipped: None,
            selected_item: None,
            max_health: 0,
            health_status: 0,
            typewriter: TypewriterFlow::Idle,
            item_box: [InventoryItem::default(); ITEM_BOX_SLOTS],
            transition: None,
            transition_door: None,
            mask_toggles: Vec::new(),
            item_events: Vec::new(),
            last_interaction: None,
            frame: 0,
            placeholders: BTreeMap::new(),
            npc_placeholders: BTreeMap::new(),
            room_bgm_requests: Vec::new(),
            pending_events: Vec::new(),
            entity_sounds: Vec::new(),
            rand_seed: RAND_SEED_INITIAL,
        }
    }
}

/// One 16-bit xorshift step: the deterministic stand-in for the original's
/// per-frame `rand()` reseed. The zero state is remapped so the sequence never
/// sticks.
///
/// TODO(parity): (scripting) the original stores the frame's `rand()` value in
/// the BioCard randSeed word (state word 3) that scripts roll dice with via
/// `cmpw 3`; the stand-in seed never reaches `state_words`, so those scripts
/// always compare against zero.
fn next_random(seed: u16) -> u16 {
    let mut x = seed;
    x ^= x << 7;
    x ^= x >> 9;
    x ^= x << 8;
    if x == 0 { 0xACE1 } else { x }
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
        state.set_camera_cut(0);
        state.state_bytes[STATE_BYTE_CHARACTER as usize] = id.player_flag & 1;
        // InitializeGame derives the maximum from the character on every path,
        // so a state built without `seed_new_game` still has a real maximum.
        state.max_health = character_max_health(id.player_flag);
        state
    }

    /// Store the current room camera id in both the camera state and the
    /// BioCard byte the scripts read with `setb`/`cmpb`.
    pub fn set_camera_cut(&mut self, cut: usize) {
        self.camera.current_cut = cut;
        self.set_byte(STATE_BYTE_ROOM_CAMERA, cut as u8);
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
    ///
    /// Index [`STATE_BYTE_ROOM_CAMERA`] is the live camera id, so writing it
    /// also moves [`GameState::camera`]; index [`STATE_BYTE_MENU_CHOICE`] is
    /// the window's menu-choice byte, index [`STATE_BYTE_SELECTED_ITEM`] the
    /// item name substitution reads, and [`STATE_BYTE_HEALTH_STATUS`] mirrors
    /// into [`GameState::health_status`].
    ///
    /// TODO(parity): (scripting) the original's `setb` indexes the whole BioCard
    /// unchecked, so scripts writing indices past the state block (57 and 82
    /// occur in shipped rooms) alias scenarioFlags2; this drops out-of-range
    /// indices, and in-range ones (e.g. 57) do not reach the flag banks at all.
    /// The same unbounded-index rule applies to `setw`/`cmpb`/`cmpw`.
    pub fn set_byte(&mut self, index: u8, value: u8) {
        if index == STATE_BYTE_ROOM_CAMERA {
            self.camera.current_cut = usize::from(value);
        }
        if index == STATE_BYTE_MENU_CHOICE {
            self.message.set_menu_choice_id(value);
        }
        if index == STATE_BYTE_SELECTED_ITEM {
            self.selected_item = (value != 0).then_some(value);
        }
        if index == STATE_BYTE_HEALTH_STATUS {
            self.health_status = value;
        }
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

    /// `enemy` (0x1B): allocate or re-initialise one character slot from the
    /// 22-byte record. Returns whether an entity was initialised.
    ///
    /// Operand layout (byte offsets from the opcode): `+1` id, `+2`
    /// behaviour/weapon selector, `+3` guard bit, `+4` force-init, `+5` SCA
    /// hit words, `+6` rotation X, `+8` yaw, `+10` rotation Z, `+12`/`+14`/
    /// `+16` X/Y/Z, `+18` slot low nibble, `+19` animation id, `+20`
    /// animation frame, `+21` variant high nibble.
    ///
    /// A guard bit other than `0xFF` skips the whole record when the bank-3
    /// bit is already set. Ids outside `0x20..=0x2E` are parsed by the reader
    /// but allocate nothing this milestone.
    ///
    /// # Documented deviation
    ///
    /// The original's force-init byte also gates the `FUN_0048f330` saved-state
    /// restore: without it, an occupied slot is only re-initialised when no
    /// saved enemy state matches. The port has no enemy snapshot store yet, so
    /// an occupied slot is left alone instead of being re-initialised. Every
    /// shipped character record sets the force byte, so the corpus never
    /// reaches this path.
    pub fn spawn_enemy(&mut self, operands: &[Operand]) -> bool {
        let guard = operand_u8(operands, 2);
        if guard != 0xFF && self.flags[usize::from(BANK_ENEMIES)].bit(guard) {
            return false;
        }
        let id = operand_u8(operands, 0);
        if !(CHARACTER_ID_MIN..=CHARACTER_ID_MAX).contains(&id) {
            // TODO(parity): (gameplay) the original allocates and runs the
            // monster entity for ids below 0x20 (and the DC-only ids above);
            // the port parses the record but creates nothing, so rooms that
            // spawn zombies alongside a character look emptier and their
            // scripts' get_eml_state/eml_state on those slots are inert.
            return false;
        }
        let slot = 1 + usize::from(operand_u8(operands, 11) & 0x0F);
        let force_init = operand_u8(operands, 3) != 0;
        let occupied = self.entities[slot].active();
        // TODO(parity): (scripting) without the force-init byte the original still
        // re-initialises an occupied slot when no saved enemy state matches
        // (FUN_0048f330); this always leaves the occupied slot untouched.
        if occupied && !force_init {
            // TODO(parity): (gameplay) the original still runs the re-init block
            // whenever the slot's status bit is set, even when the saved-state
            // check suppressed shouldInit: it overwrites state/id/anim and
            // increments g_enemy_count anyway. The port leaves the occupied
            // slot untouched (no enemy snapshot store yet) and only counts a
            // new allocation. Add the saved-state restore before relying on
            // re-spawning an occupied slot.
            return false;
        }

        let entity = &mut self.entities[slot];
        entity.id = id;
        entity.set_active(true);
        entity.behavior_flags = operand_u8(operands, 1);
        entity.pitch = operand_i16(operands, 5) as u16;
        entity.angle = operand_u16(operands, 6);
        entity.roll = operand_u16(operands, 7);
        // The record's X and Z are zero-extended, Y is sign-extended.
        entity.pos = [
            i32::from(operand_u16(operands, 8)),
            i32::from(operand_i16(operands, 9)),
            i32::from(operand_u16(operands, 10)),
        ];
        entity.animation_id = operand_u8(operands, 12);
        entity.animation_frame_id = operand_u8(operands, 13);
        entity.timing_control = 1;
        entity.set_state(0);
        entity.set_ignore(0);
        entity.action_behavior = 0;
        entity.action_state = 0;
        entity.hit_state = 0;
        entity.look_at_flags = 0;
        entity.collision_flags = 0;
        entity.death_event_id = operand_u8(operands, 2);
        entity.variant = (operand_u8(operands, 11) & 0x0F)
            | ((operand_u8(operands, 14) & 0x0F) << 4)
            | if force_init { 0x80 } else { 0 };
        // TODO(parity): (gameplay) the original points the new entity at the
        // SCA record g_scaDataTable[0] (Chris' radius/hit box) and reserves
        // `operand 4 * 6` bytes of the rotated hit-data pool; the port stores a
        // flat DEFAULT_ENEMY_RADIUS and has no SCA hit volumes, so per-character
        // hit boxes and the SCA collision pass differ (see npc/walk.rs
        // resolve_sca_collision).
        entity.sca_radius = DEFAULT_ENEMY_RADIUS;
        if !occupied {
            self.enemy_count = self.enemy_count.saturating_add(1);
        }
        true
    }

    /// `eml_state` (0x28): change one property of the enemy named by
    /// `operands[1]`. An unknown sub-command or slot is inert; the reader
    /// stops the stream before an unknown sub-command is ever decoded.
    pub fn apply_eml_state(&mut self, operands: &[Operand]) {
        let Some(slot) = operand_u8(operands, 1)
            .checked_add(1)
            .filter(|slot| usize::from(*slot) < ENTITY_COUNT)
        else {
            return;
        };
        let sub_command = operand_u8(operands, 2);
        let param = operand_u16(operands, 3);
        let entity = &mut self.entities[usize::from(slot)];
        match sub_command {
            0 => entity.behavior_flags = param as u8,
            1 => {
                entity.set_state(2);
                entity.set_ignore(0);
                entity.action_behavior = 0;
                entity.action_state = 0;
                entity.health = param as i16;
                entity.hit_state = operand_u8(operands, 4);
            }
            2 => {
                entity.action_behavior = param as u8;
                entity.action_state = 0;
            }
            3 => {
                let mode = (param >> 8) as u8;
                let value = param as u8;
                match mode {
                    0 => entity.status_flags = value,
                    1 => entity.status_flags |= value,
                    2 => entity.status_flags ^= value,
                    _ => {}
                }
            }
            5 => entity.angle = param,
            6 => entity.blend_counter = 0,
            8 => {
                entity.set_state(9);
                entity.set_ignore(0);
                entity.action_behavior = 0;
                entity.action_state = 0;
            }
            9 => {
                // TODO(parity): (gameplay) the original XORs each joint's own
                // flag bit (bit i toggles joint i's flags byte); the port only
                // records the bitfield. No renderer support for per-joint
                // hiding yet, so scripts that hide a joint are inert.
                entity.joint_flags ^= param;
            }
            10 => entity.action_state = param as u8,
            _ => {}
        }
    }

    /// `get_eml_state` (0x39): copy an enemy's `behavior_flags` into the byte
    /// scripts read back. An out-of-range slot leaves the byte untouched.
    pub fn read_enemy_flags(&mut self, index: u8) {
        if let Some(slot) = index
            .checked_add(1)
            .filter(|slot| usize::from(*slot) < ENTITY_COUNT)
        {
            self.last_enemy_flags = self.entities[usize::from(slot)].behavior_flags;
        }
    }

    /// One native update per active entity slot (1..), called after the event
    /// VM and before the player's physics mirror. A displayed message that
    /// masks the entity-think bit pauses the characters with the room; that is
    /// a different bit from the player-control one, so the gate is
    /// [`GameState::message_freezes_entities`]. Returns the number of slots
    /// visited.
    pub fn tick_entities(
        &mut self,
        room: &RoomState,
        models: &mut crate::npc::EntityModelCache,
        pack: &crate::pack::Pack,
    ) -> usize {
        // TODO(parity): (gameplay) monster ids 0x00..=0x1F allocate no entity at
        // all and the state-8 weapon-fire handler is inert (counted in
        // `npc_placeholders`); rooms that spawn enemies stay empty and a script
        // waiting on the fire completion bit can stall.
        // The original reseeds its random seed from `rand()` at the top of
        // every gameplay frame, before any entity thinks; the look-at
        // scheduling reads the frame's value.
        // TODO(parity): (gameplay) the port advances a fixed xorshift instead
        // of the platform `rand()` stream, so the state-9 look-at wander and
        // behaviour-0 wait lengths are deterministic but differ from the
        // original's per-frame draws.
        self.rand_seed = next_random(self.rand_seed);
        if self.message_freezes_entities() {
            // TODO(parity): (gameplay) the original still runs the state
            // dispatch when the message bit is set and always updates the
            // switch-zone bit and queues the fade sprite afterwards; the port
            // skips the whole NPC update, so a frozen character's
            // has_enter_switch_zone (and its shadow) stops tracking the camera.
            return 0;
        }
        crate::npc::update_all(self, room, models, pack)
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
            entity.action_behavior = 0;
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
            "act_reset" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.set_ignore(0);
                }
                StepResult::Continue
            }
            "act_end" => StepResult::Continue,
            "act_motion_bitclr" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.look_at_flags &= !0x10;
                }
                StepResult::Continue
            }
            "act_idle" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.state_field = 1;
                    entity.action_behavior = 0;
                    entity.action_state = 0;
                    entity.hit_state = 0;
                }
                StepResult::Continue
            }
            "act_anim_seq" => {
                if let Some(entity) = self.selected_entity_mut() {
                    // Operand 0 is the word at script +1: its low byte is the
                    // behaviour and its bit 4 selects the collision-flag path.
                    let word0 = operand_u16(operands, 0);
                    let word1 = operand_u16(operands, 1);
                    let word2 = operand_u16(operands, 2);
                    let behavior = word0 as u8;
                    entity.set_state(8);
                    entity.set_ignore(0);
                    if behavior & 0x10 == 0 {
                        entity.action_behavior = behavior;
                        entity.action_state = 0;
                        entity.collision_flags &= 0x7F;
                    } else if entity.collision_flags & 0x80 == 0 {
                        entity.action_behavior = behavior & 0x0F;
                        entity.action_state = 0;
                        entity.collision_flags |= 0x80;
                    } else {
                        entity.action_behavior = behavior & 0x0F;
                    }
                    entity.unk_c6 = (word0 >> 8) | ((word1 as u8 as u16) << 8);
                    entity.unk_c8 = (word1 >> 8) | ((word2 as u8 as u16) << 8);
                    entity.scd_anim_param = (word2 >> 8) as u8;
                    entity.scd_timer = 0x28;
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
                    entity.action_behavior = 1;
                    entity.action_state = 0;
                    entity.animation_id = operand_u8(operands, 0);
                    entity.scd_anim_param = operand_u8(operands, 1);
                    entity.flags = (anim_data >> 6) & 0x3FC;
                    entity.scd_timer = 0;
                }
                StepResult::Continue
            }
            "act_anim_set" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.set_state(8);
                    entity.set_ignore(0);
                    entity.action_behavior = operand_u8(operands, 0);
                    entity.action_state = 0;
                    entity.animation_id = operand_u8(operands, 1);
                    entity.scd_anim_param = operand_u8(operands, 2);
                    entity.scd_timer = 0;
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
                    entity.scd_timer =
                        (operand_u16(operands, 0) >> 8) | (u16::from(operand_u8(operands, 1)) << 8);
                }
                StepResult::Continue
            }
            "act_action_a" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.animation_frame_id = operand_u8(operands, 0);
                    entity.timing_control = 0;
                    entity.blend_counter = 7;
                    entity.move_speed_current = 0;
                    if entity.flags & 0x20 != 0 {
                        entity.blend_counter = 0;
                    }
                    if entity.id < CHARACTER_ID_MIN {
                        // Monster ids remap the clip through the original's
                        // (action_state base, animation id) pair table; ids
                        // above 0x0F park in action state 3.
                        if entity.animation_id > 0x0F {
                            entity.action_state = 3;
                        } else {
                            let pair = usize::from(entity.animation_id) * 2;
                            entity.action_state = SCD_ANIM_REMAP[pair].wrapping_add(1);
                            entity.animation_id = SCD_ANIM_REMAP[pair + 1];
                        }
                    } else {
                        entity.action_state = 1;
                    }
                }
                StepResult::Continue
            }
            "act_action_b" => {
                if let Some(entity) = self.selected_entity_mut() {
                    entity.animation_frame_id = operand_u8(operands, 0);
                    entity.timing_control = 0;
                    entity.blend_counter = 7;
                    entity.move_speed_current = 0;
                    if entity.flags & 0x20 != 0 {
                        entity.blend_counter = 0;
                    }
                }
                StepResult::Continue
            }
            _ => {
                self.record_placeholder(op.op);
                StepResult::Placeholder
            }
        }
    }

    /// One actor motion instruction: record the look-at target and step sizes
    /// and return, exactly like the original's state-1 handler. The actor run
    /// never blocks on the motion; the native state-8/9 driver owns the facing
    /// and movement.
    ///
    /// Flag low nibble zero records only `lookAtFlags`. Otherwise the step is
    /// the word at instruction offset +8: its low byte the yaw step (default
    /// 0xC0), its high byte the pitch step (default 0x40). Flag `0x20` wraps
    /// negative target X/Y by +0x1000 before use, and the exact flag byte
    /// `0x93` names a live target (type 0 player, 1 enemy `index`, 2 object
    /// model, 3 item model) instead of a position.
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

        let mut target = None;
        let mut target_entity = None;
        if flags & 0x0F != 0 {
            if flags == 0x93 {
                // The selector is the word at +2 and the index the signed word
                // at +4. Object and item models have no entity this milestone,
                // so only the player and enemy targets latch a live slot.
                // TODO(parity): (gameplay) target types 2 (object model) and 3
                // (item model) resolve to g_omodel_table/g_item_model_table in
                // the original and refresh the look-at from their transforms;
                // the port ignores them, so a script aiming a character at an
                // omodel keeps its previous target.
                target_entity =
                    motion_target_slot(operand_u8(operands, 1), operand_i16(operands, 2));
                target = target_entity.map(|slot| self.entities[usize::from(slot)].pos);
            } else {
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
                target = Some([x, y, z]);
            }
        }

        let Some(entity) = self.selected_entity_mut() else {
            self.record_placeholder(op.op);
            return StepResult::Placeholder;
        };
        entity.look_at_flags = flags;
        if flags & 0x0F == 0 {
            return StepResult::Continue;
        }
        entity.look_at_yaw_step = yaw_step;
        entity.look_at_pitch_step = pitch_step;
        if let Some(target) = target {
            entity.target = target;
        }
        entity.target_entity = target_entity;
        StepResult::Continue
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
        // TODO(parity): (scripting) the original's `room_state_reset` also clears
        // pickedItemId, usedItemId, fwdPosActionId and g_SysFlags[1] on a room
        // change; this keeps the previous room's picked/used item and probe bytes.
        self.id = id;
        self.state_bytes[0] = id.stage;
        self.state_bytes[1] = id.room;
        self.set_camera_cut(0);
        self.room_actions = [None; ROOM_ACTION_SLOTS];
        self.doors = [None; ROOM_ACTION_SLOTS];
        self.clear_stairs();
        let player_entity = self.entities[0];
        self.entities = initial_entities();
        self.entities[0] = player_entity;
        self.entity_anims = std::array::from_fn(|_| crate::npc::EntityAnim::default());
        self.enemy_count = 0;
        self.selected_entity = 0;
        self.transition = None;
        self.transition_door = None;
        self.mask_toggles.clear();
        // A door-animation message is applied while the transition runs and
        // must survive the destination's room boot, so an active window is
        // kept; otherwise the message resets and only the BioCard menu-choice
        // byte's low bit survives.
        if !self.message.active {
            self.message = MessageWindow::default();
            let choice = self.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)] & 0x7F;
            self.message.set_menu_choice_id(choice);
            self.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)] = choice;
            self.message_item_slot = None;
            self.message_flags = MESSAGE_FLAGS_INITIAL;
        }
        self.camera = CameraState::default();
        self.typewriter = TypewriterFlow::Idle;
        self.last_interaction = None;
        self.room_bgm_requests.clear();
        self.pending_events.clear();
        self.entity_sounds.clear();
    }

    /// The number of inventory slots the current character uses.
    pub fn inventory_capacity(&self) -> usize {
        if self.id.player_flag & 1 == 1 {
            INVENTORY_SLOTS_JILL
        } else {
            INVENTORY_SLOTS_CHRIS
        }
    }

    /// Add `quantity` of `item` with the original's merge rules.
    ///
    /// Stackable items (the ammunition range and the ink ribbon) merge into an
    /// existing stack; the ribbon is always taken three at a time. A merge that
    /// would pass [`items::ITEM_QUANTITY_CAP`] caps the stack and spills the
    /// remainder into a new slot, exactly like the original's pickup path.
    pub fn add_item(&mut self, item: u8, quantity: u8) {
        let quantity = if item == items::ITEM_INK_RIBBONS {
            3
        } else {
            quantity
        };
        let capacity = self.inventory_capacity();
        if items::is_stackable(item)
            && let Some(index) = self.inventory.iter().position(|stack| stack.id == item)
        {
            let merged = u16::from(self.inventory[index].quantity) + u16::from(quantity);
            if merged <= u16::from(items::ITEM_QUANTITY_CAP) {
                self.inventory[index].quantity = merged as u8;
                self.rebuild_slots();
                return;
            }
            if self.inventory.len() < capacity {
                self.inventory[index].quantity = items::ITEM_QUANTITY_CAP;
                let spill = (merged as u8).wrapping_add(6);
                self.inventory.push(InventoryItem {
                    id: item,
                    quantity: spill,
                });
                self.rebuild_slots();
                return;
            }
        }
        self.inventory.push(InventoryItem { id: item, quantity });
        self.rebuild_slots();
    }

    /// Rebuild the inventory slot bookkeeping after an add, remove or
    /// rearrange: drop empty stacks and refresh the total-held and selected
    /// state bytes the scripts read.
    pub fn rebuild_slots(&mut self) {
        self.inventory.retain(|stack| stack.id != 0);
        self.state_bytes[usize::from(STATE_BYTE_TOTAL_HELD)] = self.inventory.len() as u8;
        self.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)] = self.selected_item.unwrap_or(0);
    }

    /// Select the inventory item under the cursor.
    pub fn select_item(&mut self, item: Option<u8>) {
        self.selected_item = item;
        self.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)] = item.unwrap_or(0);
    }

    /// Record an item use: the `testitem`/`usedItemId` byte and the typed
    /// field the SCD conditions read.
    pub fn record_used_item(&mut self, item: u8) {
        self.last_used_item = Some(item);
        self.state_bytes[usize::from(STATE_BYTE_USED_ITEM)] = item;
    }

    /// Equip (or clear) an item.
    pub fn set_equipped(&mut self, item: Option<u8>) {
        self.equipped = item;
        self.state_bytes[usize::from(STATE_BYTE_EQUIPPED)] = item.unwrap_or(0);
    }

    /// Whether at least one `item` is held.
    ///
    /// TODO(parity): (scripting) the original's `item_ck`/`get_item_slot` scans
    /// slot ids regardless of quantity, so the starting knife (id 1, quantity 0)
    /// counts as held; requiring `quantity > 0` here makes `testitem`/`item_ck`
    /// for the knife (and any other zero-quantity stack) report false.
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

    /// The room-action typewriter handler (`room_check_actions[0x10]`).
    ///
    /// An ink ribbon starts the save prompt; without one, a first Jill
    /// playthrough may still save (and does not consume anything), while every
    /// other character/playthrough gets the "no ink ribbon" message instead.
    /// Returns whether the prompt was shown.
    pub fn check_typewriter(&mut self) -> bool {
        if self.typewriter != TypewriterFlow::Idle {
            return false;
        }
        let character = self.id.player_flag & 1;
        let second_playthrough =
            self.flags[usize::from(BANK_SCENARIO)].bit(SCENARIO_FLAG_SECOND_PLAYTHROUGH);
        let jill_first_playthrough = character == 1 && !second_playthrough;
        let ribbon = self.has_item(items::ITEM_INK_RIBBONS);
        if !ribbon && !jill_first_playthrough {
            self.show_message(MESSAGE_TYPEWRITER_NO_RIBBON, 0xFF);
            return false;
        }
        let message = if jill_first_playthrough {
            MESSAGE_TYPEWRITER_SAVE_PROMPT
        } else {
            MESSAGE_TYPEWRITER_RIBBON_PROMPT
        };
        self.show_message(message, 0xFF);
        if !self.message.active {
            return false;
        }
        let ink_ribbon = ribbon && (character == 0 || second_playthrough);
        self.typewriter = TypewriterFlow::Prompt { ink_ribbon };
        true
    }

    /// The prompt's answer once the window dismissed it. `Some(ink_ribbon)`
    /// when the player confirmed the save, `None` while the prompt is up or
    /// when it was declined.
    pub fn take_typewriter_confirm(&mut self) -> Option<bool> {
        let TypewriterFlow::Prompt { ink_ribbon } = self.typewriter else {
            return None;
        };
        if self.message.active {
            return None;
        }
        self.typewriter = TypewriterFlow::Idle;
        (self.message.menu_choice_id() & 1 == 0).then_some(ink_ribbon)
    }

    /// Spend one ink ribbon the way the save flow does: record the use and
    /// remove (or decrement) the stack.
    pub fn consume_ink_ribbon(&mut self) {
        self.record_used_item(items::ITEM_INK_RIBBONS);
        let Some(index) = self
            .inventory
            .iter()
            .position(|stack| stack.id == items::ITEM_INK_RIBBONS && stack.quantity > 0)
        else {
            return;
        };
        self.inventory[index].quantity -= 1;
        if self.inventory[index].quantity == 0 {
            self.inventory.remove(index);
        }
        self.rebuild_slots();
    }

    /// The save screen's completion: raise the save counter, clamped at 99.
    pub fn increment_saves(&mut self) {
        let saves = self.state_bytes[usize::from(STATE_BYTE_SAVES)];
        self.state_bytes[usize::from(STATE_BYTE_SAVES)] = if u16::from(saves) + 1 >= 100 {
            99
        } else {
            saves + 1
        };
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
            self.rebuild_slots();
        }
        true
    }

    /// `ck_item_count`: no item family table exists yet, so only the exact item
    /// id is matched. Returns the summed quantity and the matching stack count.
    ///
    /// TODO(parity): (scripting) the original's search id selects an item GROUP
    /// (0x0A any, 0x0B item 2, 0x0C item 3, 0x0D items 4/5, 0x0F item 6,
    /// 0x10..=0x12 items 7/8/9), not an exact item id; only exact ids are summed
    /// here, so group counts read 0.
    pub fn item_family_total(&self, search: u8) -> (u32, u32) {
        let count = self
            .inventory
            .iter()
            .filter(|stack| stack.id == search)
            .count() as u32;
        (self.item_count(search), count)
    }

    /// Whether document `index` (`0..16`) has been collected: the room-flags
    /// bit `0x82 + index`, raised by the pickup and read by the FILE tab.
    pub fn file_collected(&self, index: u8) -> bool {
        self.flags[usize::from(BANK_ROOM_FLAGS)].bit(ROOM_FLAG_FILE_BASE.wrapping_add(index))
    }

    /// Set or clear the FILE-collected bit of document `index`.
    pub fn set_file_collected(&mut self, index: u8, value: bool) {
        let mode = if value { 0 } else { 1 };
        self.flags[usize::from(BANK_ROOM_FLAGS)]
            .apply(ROOM_FLAG_FILE_BASE.wrapping_add(index), mode);
    }

    /// Whether document item `item` has been collected; non-document ids are
    /// always `false`.
    pub fn document_collected(&self, item: u8) -> bool {
        file_index(item).is_some_and(|index| self.file_collected(index))
    }

    /// The item-box confirm action: move the whole inventory stack at
    /// `player_slot` into box slot `box_slot` and bring the box stack back.
    ///
    /// The original swaps the two raw slots directly. With the dense inventory
    /// a withdrawn stack merges into an existing stack of the same stackable
    /// item (up to `items::ITEM_QUANTITY_CAP`) instead of leaving a duplicate,
    /// and a deposited stack frees its slot. The equipped marker is cleared
    /// when the swap takes the equipped item away and no copy remains.
    ///
    /// TODO(parity): (inventory) the original swaps the two raw slots verbatim,
    /// so the withdrawn stack always lands in the vacated player slot and box
    /// slot counts stay one-for-one; the merge/spill here can combine stacks the
    /// original would keep separate.
    ///
    /// Returns whether anything moved.
    pub fn item_box_swap(&mut self, box_slot: usize, player_slot: usize) -> bool {
        if box_slot >= ITEM_BOX_SLOTS {
            return false;
        }
        let box_item = self.item_box[box_slot];
        let player_item = self.inventory.get(player_slot).copied().unwrap_or_default();
        if box_item.id == 0 && player_item.id == 0 {
            return false;
        }

        // Deposit: the player stack takes the box slot.
        self.item_box[box_slot] = player_item;

        // Withdraw: merge a stackable into an existing stack, otherwise place
        // it in the vacated player slot.
        let merge_target = if box_item.id != 0 && items::is_stackable(box_item.id) {
            self.inventory
                .iter()
                .enumerate()
                .find(|(index, stack)| *index != player_slot && stack.id == box_item.id)
                .map(|(index, _)| index)
        } else {
            None
        };
        match merge_target {
            Some(target) => {
                let total =
                    u16::from(self.inventory[target].quantity) + u16::from(box_item.quantity);
                let cap = u16::from(items::ITEM_QUANTITY_CAP);
                if total <= cap {
                    self.inventory[target].quantity = total as u8;
                    if player_slot < self.inventory.len() {
                        self.inventory.remove(player_slot);
                    }
                } else {
                    // The merge overflows the cap: keep the remainder as its
                    // own stack in the vacated player slot.
                    self.inventory[target].quantity = cap as u8;
                    let spill = InventoryItem {
                        id: box_item.id,
                        quantity: (total - cap) as u8,
                    };
                    if player_slot < self.inventory.len() {
                        self.inventory[player_slot] = spill;
                    } else {
                        self.inventory.push(spill);
                    }
                }
            }
            None => {
                if box_item.id != 0 {
                    if player_slot < self.inventory.len() {
                        self.inventory[player_slot] = box_item;
                    } else {
                        self.inventory.push(box_item);
                    }
                } else if player_slot < self.inventory.len() {
                    self.inventory.remove(player_slot);
                }
            }
        }
        self.rebuild_slots();
        if self.equipped.is_some_and(|item| !self.has_item(item)) {
            self.set_equipped(None);
        }
        true
    }

    /// Write the health-status byte and mirror it into the BioCard state byte
    /// the scripts read with `cmpb` 50.
    pub fn set_health_status(&mut self, status: u8) {
        self.health_status = status;
        self.state_bytes[usize::from(STATE_BYTE_HEALTH_STATUS)] = status;
    }

    /// Clear the per-frame item-use flag bank. The original zeroes both
    /// `g_itemUseFlags` words at the top of every game frame, before the room
    /// scripts re-arm the bits this frame's interactions need.
    pub fn clear_item_use_flags(&mut self) {
        self.flags[usize::from(BANK_ITEM_USE)] = FlagBank::new();
    }

    /// The four-byte examined-item bank the item-name lookup reads.
    pub fn examined_flags(&self) -> [u8; 4] {
        self.message.examined
    }

    /// Mark an item examined, exactly like the original's `Flg_on` on
    /// `g_itemExaminedFlags`: only items whose lookup name class is not a real
    /// name (bit `0x80` clear) are tracked, and the class selects the bit.
    pub fn mark_examined(&mut self, item: u8) {
        let Some(record) = items::record(item) else {
            return;
        };
        if record.name_valid() {
            return;
        }
        let bank = u32::from_le_bytes(self.message.examined);
        let bank = bank | (0x8000_0000u32 >> (record.name_class & 0x1F));
        self.message.examined = bank.to_le_bytes();
    }

    /// Whether the player carries the radio (scenario bank 0, bit `0x7F`),
    /// which enables the pause menu's radio tab.
    pub fn has_radio(&self) -> bool {
        self.flags[usize::from(BANK_SCENARIO)].bit(SCENARIO_FLAG_HAS_RADIO)
    }

    /// The per-item use flag (`g_itemUseFlags` bank 9) of `item`. Key, special
    /// and chemical items are only usable while their flag is re-armed by the
    /// room scripts.
    pub fn item_use_flag(&self, item: u8) -> bool {
        self.flags[usize::from(BANK_ITEM_USE)].bit(item.wrapping_sub(0x1B))
    }

    /// Set or clear `item`'s per-item use flag.
    pub fn set_item_use_flag(&mut self, item: u8, value: bool) {
        let mode = if value { 0 } else { 1 };
        self.flags[usize::from(BANK_ITEM_USE)].apply(item.wrapping_sub(0x1B), mode);
    }

    /// Whether the current room is the guardhouse drug store, where chemicals
    /// may be combined.
    pub fn is_drug_store(&self) -> bool {
        self.id.stage == STAGE_GUARDHOUSE && self.id.room == ROOM_DRUG_STOREHOUSE
    }

    /// Apply the menu USE action for `item`, exactly as the original's
    /// category dispatch does:
    ///
    /// - heal items run the heal table and update health/status;
    /// - key, special and chemical items need their per-item use flag;
    /// - books only accept the red book, with flag bit `0x23`;
    /// - weapons, ammo, the empty bottle and unusable ids do nothing.
    ///
    /// It never consumes the item; the caller records and decrements it.
    pub fn use_item(&mut self, item: u8) -> UseResult {
        match items::use_category(item) {
            items::UseCategory::Heal => {
                let effect = items::heal_effect(item);
                let health = self.entities[0].health;
                let max = self.max_health;
                let mut healed = false;
                if health < max {
                    let raised = items::healed_health(effect.amount, health, max);
                    if raised != health {
                        self.entities[0].health = raised;
                        healed = true;
                    }
                }
                let mut cured = false;
                if effect.cure != items::StatusCure::None && self.health_status & 0x22 != 0 {
                    let cleared = items::cured_status(effect.cure, self.health_status);
                    match effect.cure {
                        // The 0x20 cure counts as used whenever either poison
                        // flag is up, even when only the 0x02 bit is set and
                        // nothing is actually cleared.
                        items::StatusCure::Poison20 => {
                            if cleared != self.health_status {
                                self.apply_flag(1, SCENARIO2_FLAG_YAWN_POISONED, 1);
                                self.set_health_status(cleared);
                            }
                            cured = true;
                        }
                        // The 0x02 cure only counts when the 0x02 bit is set.
                        items::StatusCure::Poison02 if self.health_status & 0x02 != 0 => {
                            self.set_health_status(cleared);
                            cured = true;
                        }
                        _ => {}
                    }
                }
                if healed || cured {
                    UseResult::Used { healed, cured }
                } else {
                    UseResult::Unusable
                }
            }
            items::UseCategory::Chemicals
            | items::UseCategory::Special
            | items::UseCategory::Keys => {
                if self.item_use_flag(item) {
                    UseResult::Used {
                        healed: false,
                        cured: false,
                    }
                } else {
                    UseResult::Unusable
                }
            }
            items::UseCategory::Books => {
                if item == ITEM_RED_BOOK
                    && self.flags[usize::from(BANK_ITEM_USE)].bit(ITEM_RED_BOOK_FLAG)
                {
                    UseResult::Used {
                        healed: false,
                        cured: false,
                    }
                } else {
                    UseResult::Unusable
                }
            }
            items::UseCategory::Always => UseResult::Used {
                healed: false,
                cured: false,
            },
            _ => UseResult::Unusable,
        }
    }

    /// Apply the menu MOVE/combine between the cursor and target slots.
    ///
    /// The cursor item's combine table selects the recipe; `new_cursor` and
    /// `new_target` replace the slot ids first and then the record's effect
    /// (ammo transfer, quantity merge, chemical flag) rearranges quantities.
    /// Empty slots are compacted afterwards.
    ///
    /// TODO(parity): (ui) two herbs (0x43..=0x4B) with no recipe return the
    /// original's distinct "cannot combine" result (message 0xF6); this reports
    /// `NoRecipe`.
    pub fn combine_slots(&mut self, cursor_slot: usize, target_slot: usize) -> CombineResult {
        if cursor_slot == target_slot {
            return CombineResult::NoRecipe;
        }
        let Some(cursor) = self.inventory.get(cursor_slot).copied() else {
            return CombineResult::NoRecipe;
        };
        let Some(target) = self.inventory.get(target_slot).copied() else {
            return CombineResult::NoRecipe;
        };
        if cursor.id == 0 || target.id == 0 {
            return CombineResult::NoRecipe;
        }
        let Some(record) = items::combine(cursor.id, target.id) else {
            return CombineResult::NoRecipe;
        };
        // The one-way ammo transfers need an empty destination: effect 6 fills
        // the cursor stack, effect 7 the target stack, and the original refuses
        // the recipe untouched when that slot already holds rounds.
        if (record.effect == 6 && cursor.quantity != 0)
            || (record.effect == 7 && target.quantity != 0)
        {
            return CombineResult::NoRecipe;
        }
        if (CHEMICAL_MIN..=CHEMICAL_MAX).contains(&cursor.id) && !self.is_drug_store() {
            return CombineResult::NeedsDrugStore;
        }
        let herb = (HERB_MIN..=HERB_MAX).contains(&cursor.id);
        self.inventory[cursor_slot].id = record.new_cursor;
        self.inventory[target_slot].id = record.new_target;
        if record.new_target == 0 {
            self.inventory[target_slot].quantity = 0;
        }
        self.apply_combine_effect(cursor_slot, target_slot, record.effect);
        self.rebuild_slots();
        if self.equipped.is_some_and(|item| !self.has_item(item)) {
            self.set_equipped(None);
        }
        CombineResult::Applied {
            herb,
            chemical: record.effect == 4,
        }
    }

    /// The combine record's effect byte: 1/2 ammo transfer with the cursor /
    /// other slot winning, 3 quantity merge (cap `0xFA`), 4 chemical flag,
    /// 5 mode only, 6/7 one-way ammo transfer.
    fn apply_combine_effect(&mut self, cursor_slot: usize, target_slot: usize, effect: u8) {
        let cursor = self.inventory[cursor_slot];
        let target = self.inventory[target_slot];
        match effect {
            1 => {
                let sum = u16::from(target.quantity) + (u16::from(cursor.quantity) & 0x7F);
                let max = u16::from(items::max_quantity(cursor.id));
                if max < sum {
                    self.inventory[cursor_slot].quantity = max as u8;
                    self.inventory[target_slot].quantity = (sum as u8).wrapping_sub(max as u8);
                } else {
                    self.inventory[cursor_slot].quantity = sum as u8;
                    self.inventory[target_slot] = InventoryItem::default();
                }
            }
            2 => {
                let sum = u16::from(cursor.quantity) + (u16::from(target.quantity) & 0x7F);
                let max = u16::from(items::max_quantity(target.id));
                if max < sum {
                    self.inventory[target_slot].quantity = max as u8;
                    self.inventory[cursor_slot].quantity = (sum as u8).wrapping_sub(max as u8);
                } else {
                    self.inventory[target_slot].quantity = sum as u8;
                    self.inventory[cursor_slot] = InventoryItem::default();
                }
            }
            3 => {
                let sum = u16::from(cursor.quantity) + u16::from(target.quantity);
                if sum > u16::from(items::ITEM_QUANTITY_CAP) {
                    self.inventory[cursor_slot].quantity = items::ITEM_QUANTITY_CAP;
                    self.inventory[target_slot].quantity = (sum as u8).wrapping_add(6);
                } else {
                    self.inventory[cursor_slot].quantity = sum as u8;
                    self.inventory[target_slot] = InventoryItem::default();
                }
            }
            4 => {
                self.apply_flag(0, SCENARIO_FLAG_CHEMICAL_COMBINE, 0);
            }
            5 => {}
            6 => {
                let other = target.quantity;
                let max = items::max_quantity(cursor.id);
                if max < other {
                    self.inventory[cursor_slot].quantity = max;
                    self.inventory[target_slot].quantity = other - max;
                } else {
                    self.inventory[cursor_slot].quantity = other;
                    self.inventory[target_slot] = InventoryItem::default();
                }
            }
            7 => {
                let other = cursor.quantity;
                let max = items::max_quantity(target.id);
                if max < other {
                    self.inventory[target_slot].quantity = max;
                    self.inventory[cursor_slot].quantity = other - max;
                } else {
                    self.inventory[target_slot].quantity = other;
                    self.inventory[cursor_slot] = InventoryItem::default();
                }
            }
            _ => {}
        }
    }

    /// Drop the stair/ladder state (room change and transition teardown).
    ///
    /// The original clears the player's zone flags in `player_state_init` when
    /// the destination room loads, so a climb never survives a doorway.
    pub fn clear_stairs(&mut self) {
        self.stair_zones = StairZones::default();
        self.stair_height = None;
        self.stair_entry = None;
        self.stair_climb = false;
        self.ladder_down = false;
        // `player_state_init` clears the zone flags when a room loads.
        self.entities[0].zone_flags = 0;
    }

    /// Copy the stair probe's result onto the visible player: the ramp height
    /// to hold this tick, the latched entry flags and the climb behaviour.
    ///
    /// The height is applied to `player.pos[1]` immediately, exactly where the
    /// original's `stairs_height_update` writes `localMatrix.t[1]` and `posY`
    /// after the frame's movement.
    pub fn apply_stair_state(&self, player: &mut PlayerState) {
        player.stairs.height = self.stair_height;
        player.stairs.in_zone = self.stair_entry.is_some();
        player.stairs.ladder = self.stair_entry.is_some_and(|entry| entry.ladder);
        if let Some(entry) = self.stair_entry {
            player.stairs.base = [entry.base_x, entry.base_z];
            if self.stair_climb {
                player.stairs.locked_angle =
                    angle_between(player.pos, [entry.target_x, 0, entry.target_z]);
            }
        }
        player.stairs.climbing = self.stair_climb;
        if let Some(height) = self.stair_height {
            player.pos[1] = height;
        }
    }

    /// `stairs_height_update`: hold the player's height on the zone's ramp.
    ///
    /// The original measures from the player's own position (`localMatrix`),
    /// not the probe point, so the caller passes the position separately.
    fn apply_stairs_height(&mut self, slot: u8, x: i32, z: i32) -> bool {
        let Some(action) = self.room_actions.get(usize::from(slot)).copied().flatten() else {
            return false;
        };
        let Some(zone) = stairs::StairZone::from_action(&action) else {
            return false;
        };
        let height = stairs::ramp_height(&zone, x, z);
        self.entities[0].pos[1] = height;
        self.stair_height = Some(height);
        true
    }

    /// `set_stairs_zone`: latch the stair/ladder entry on the player entity.
    ///
    /// Raises zone flag `0x20` (plus `0x10` for the ladder variant), stores the
    /// ladder base in `unk_c6`/`unk_c8`, toggles the entry's own low word so a
    /// two-way ladder flips ends, and raises the ladder-down state. The
    /// stair/ladder climb behaviour starts from here.
    ///
    /// TODO(parity): (gameplay) the original starts an eight-state climb
    /// behaviour (walk-up, turn, climb anim 0x33/0x35 with step sounds and a
    /// camera effect, then descent/step-back states); this latches a target and
    /// holds the facing until the player is within 900 units.
    fn apply_stairs_zone(&mut self, slot: u8) -> bool {
        let Some(action) = self.room_actions.get(usize::from(slot)).copied().flatten() else {
            return false;
        };
        let Some(stairs::StairZone::Entry {
            variant,
            base_x,
            base_z,
            ..
        }) = stairs::StairZone::from_action(&action)
        else {
            return false;
        };
        let previous = self.entities[0].zone_flags;
        self.entities[0].zone_flags |= 0x20;
        if variant != 0 {
            self.entities[0].zone_flags = previous | 0x30;
        }
        self.entities[0].unk_c6 = base_x;
        self.entities[0].unk_c8 = base_z;
        self.stair_entry = Some(StairEntryState {
            slot,
            ladder: variant != 0,
            base_x,
            base_z,
            target_x: i32::from(base_x),
            target_z: i32::from(base_z),
        });
        self.ladder_down = true;
        self.stair_climb = true;
        if let Some(action) = self.room_actions[usize::from(slot)].as_mut() {
            action.params[2] ^= 1;
        }
        self.record_interaction(slot, action.kind, None);
        true
    }

    /// Run the `set_stairs_zone` handler for `slot` (the `aot_on` path).
    pub fn fire_stairs_zone(&mut self, slot: u8) -> bool {
        self.apply_stairs_zone(slot)
    }

    /// Run the `stairs_height_update` handler for `slot` at the player's own
    /// position (the `aot_on` path).
    pub fn fire_stairs_height(&mut self, slot: u8) -> bool {
        let pos = self.entities[0].pos;
        self.apply_stairs_height(slot, pos[0], pos[2])
    }

    /// Probe the room action table for the player at `pos` facing `angle`.
    ///
    /// Mirrors the original's two probes: entries without probe bit `0x80` are
    /// tested every frame when their low flag bits intersect the frame masks
    /// (`1` and `4`), and entries with `0x80` only when the action key is held
    /// and their bit `0x01` is set. Probe bit `0x40` tests the player position
    /// itself; otherwise a point 600 units in front is tested. Only the first
    /// action-key entry that matches fires, as in the original. Item and door
    /// actions act for real, stair zones drive the player's height and climb
    /// state, the menu-driven kinds record a placeholder.
    ///
    /// TODO(parity): (scripting) the original probes bit-0 entries every frame
    /// (mask 1) and bit-2 entries only from the collision pass (mask 4); this
    /// merges both masks into one walk, so a bit-2-only zone can fire from the
    /// player probe. Handlers 5/6 (`check_door`/`check_door_side`), which latch
    /// the approach side into zone flags for the door animation, are also not run.
    pub fn interact(&mut self, pos: [i32; 3], angle: u16, action: bool) {
        // The live action table is the stair-zone store; rebuilding it here
        // also picks up `aot_reset`/`aot_on` edits and the entry's own toggled
        // word.
        self.stair_zones = StairZones::collect(&self.room_actions);
        self.stair_height = None;
        // A ladder climb releases once the player reaches its base, matching
        // the original climb's 900-unit arrival test.
        if self.stair_climb
            && let Some(entry) = self.stair_entry
        {
            let dx = pos[0] - entry.target_x;
            let dz = pos[2] - entry.target_z;
            if dx * dx + dz * dz < 900 * 900 {
                self.stair_climb = false;
            }
        }

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
            // Record which entry the probe hit: the forward reach point writes
            // `fwdPosActionId`, the entity-position probe `entPosActionId`.
            // Scripts read both with `cmpb` 16/17.
            let hit = (slot as u8).saturating_add(1);
            if flags & 0x40 != 0 {
                self.state_bytes[usize::from(STATE_BYTE_ENT_ACTION)] = hit;
            } else {
                self.state_bytes[usize::from(STATE_BYTE_FWD_ACTION)] = hit;
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
                RoomActionKind::StairsHeight => {
                    self.apply_stairs_height(room_action.slot, pos[0], pos[2]);
                }
                RoomActionKind::StairsZone => {
                    self.apply_stairs_zone(room_action.slot);
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
    ///
    /// TODO(parity): (scripting) handlers 3 include_key, 7 flag_bank_set,
    /// 0x0B room_action_effect (dust), 0x0D set_room_event_flag and 0x0E
    /// check_desk are not run at all; a script that gates progress on
    /// `flag_bank_set` or the desk flow does nothing.
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
            HANDLER_STAIRS_ZONE => self.fire_stairs_zone(slot),
            HANDLER_STAIRS_HEIGHT => self.fire_stairs_height(slot),
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
    ///
    /// TODO(parity): (progression) map item models use handler 0x0F in the
    /// original: `pickup_key_event` raises the map's RoomFlags bit
    /// (0x7C + id - 0x4E) and does NOT put the map in the inventory; here every
    /// item model becomes an inventory stack and the map-seen bit is never set.
    /// Document pickups also use their own handler (0x0D) and record byte.
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
        // BioCard 0x213 (`pickedItemId`) is what the scripts test with cmpb 19.
        self.state_bytes[usize::from(STATE_BYTE_PICKED_ITEM)] = item;
        self.item_events.push(item);
        // Remember the pickup in the room items flag bank so the item does not
        // come back when the room is re-entered.
        if action.room_items_flag != 0xFF {
            self.apply_flag(7, action.room_items_flag, 0);
        }
        // A document pickup raises its FILE-collected bit (bank 8,
        // `0x82 + (item - 0x5F)`) so the FILE tab lists it.
        if let Some(index) = file_index(item) {
            self.apply_flag(BANK_ROOM_FLAGS, ROOM_FLAG_FILE_BASE.wrapping_add(index), 0);
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
    ///
    /// TODO(parity): (gameplay/audio) the original plays the locked/key-turn
    /// sound effects and defers consuming the key to `check_event_item_usage`
    /// once the prompt message is dismissed; here the key is removed immediately
    /// and no door sound is queued.
    pub fn try_door(&mut self, slot: u8) -> bool {
        let Some(door) = self.doors.get(usize::from(slot)).copied().flatten() else {
            return false;
        };
        // The character-restriction test is inert for the two PC characters:
        // it compares the player id against 3, and ids 0/1 never match. Keep
        // the original's exact test so a record with bit 0x40 behaves the same.
        if door.lock & 0x40 != 0 && self.id.player_flag & 3 == 3 {
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
    ///
    /// Camera-only doors (record byte `+0x0B` bit `0x80`) keep the current room
    /// as the target: the transition re-aims the camera without a reload.
    fn begin_transition(&mut self, door: &Door) -> bool {
        let camera_only = door.camera & 0x80 != 0;
        let target = if camera_only {
            self.id
        } else {
            let dest = door.next_room;
            if dest == 0xFF {
                return false;
            }
            if dest < 0x20 {
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
            }
        };
        self.transition = Some(RoomTransition {
            target,
            pos: door.next_pos,
            angle: door.next_angle as u16 & 0x0FFF,
        });
        self.transition_door = Some(*door);
        // A stairwell or ladder door runs the climb behaviour: the original's
        // `check_door` raises the stair zone flag and `player_door_open_sequence`
        // owns the player's Y while the collision pass is suspended. The climb
        // faces the doorway and is torn down when the destination loads.
        if crate::door::is_stair_type(door.door_type) {
            let target_x = i32::from(door.zone[0]) + i32::from(door.zone[2]) / 2;
            let target_z = i32::from(door.zone[1]) + i32::from(door.zone[3]) / 2;
            self.entities[0].zone_flags |= 0x20;
            self.stair_entry = Some(StairEntryState {
                slot: door.slot,
                ladder: crate::door::is_ladder_type(door.door_type),
                base_x: target_x as u16,
                base_z: target_z as u16,
                target_x,
                target_z,
            });
            self.stair_climb = true;
        }
        true
    }

    /// Request a message by id, exactly like the original's
    /// `set_message_display`: refused while one is already up. On success the
    /// pause word is masked out of [`GameState::message_flags`] and restored
    /// when the window dismisses. The encoded bytes are resolved by
    /// [`GameState::update_message`], once the engine hands over the room and
    /// text tables.
    pub fn show_message(&mut self, id: u8, pause: u16) {
        if self.begin_message(id, pause) {
            self.sync_message_choice();
        }
    }

    /// The request head shared by [`GameState::show_message`] and the
    /// door-animation adapter armed in [`GameState::update_message`].
    fn begin_message(&mut self, id: u8, pause: u16) -> bool {
        if !self.message.request(id, pause, self.message_menu) {
            return false;
        }
        self.message_flags_backup = self.message_flags;
        self.message_flags &= !pause;
        true
    }

    /// Whether the displayed message's pause word masked the control bit, so
    /// the engine must ignore the player's movement and action input for this
    /// tick, exactly like the original's blanked d-pad word.
    pub fn message_locks_controls(&self) -> bool {
        self.message_flags & MESSAGE_FLAG_CONTROLS == 0
    }

    /// Whether the displayed message's pause word masked the entity-think bit,
    /// so the scripted characters freeze with the window. The original gates
    /// `character_npc_update` on this bit, not on the player-control one: a
    /// pause word can hold the characters while the player stays free, or the
    /// other way around.
    pub fn message_freezes_entities(&self) -> bool {
        self.message_flags & MESSAGE_FLAG_ENTITIES == 0
    }

    /// Request a message and arm the room action its post-action pickup takes.
    pub fn show_message_for_action(&mut self, slot: u8, id: u8, pause: u16) {
        self.message_item_slot = Some(slot);
        self.show_message(id, pause);
    }

    /// Drop any displayed message, restore the paused flags and release the
    /// scripts' menu-choice byte.
    pub fn cancel_message(&mut self) {
        if self.message.active {
            self.message_flags = self.message_flags_backup;
        }
        self.message = MessageWindow::default();
        self.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)] = 0;
    }

    /// Mirror the window's menu-choice byte into the BioCard state byte the
    /// scripts observe with `cmpb 5`.
    pub fn sync_message_choice(&mut self) {
        let choice = self.message.menu_choice_id();
        self.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)] = choice;
    }

    /// Drive the message window one fixed tick and run what it asks for.
    ///
    /// The engine calls this once per tick with the held inputs, then draws
    /// with [`crate::message::MessageWindow::draw`] after the gameplay scene
    /// and before fades. Room and global messages are resolved through
    /// [`Text::message`], so a missing global table just reads empty.
    pub fn update_message(&mut self, input: MessageInput, room: &RoomState, text: &Text) {
        // The engine's door-animation adapter writes the window's fields
        // directly; arm that request before looking its bytes up.
        if self.message.active
            && !self.message.has_source()
            && self.message.phase() == crate::message::MessagePhase::Idle
            && self.message.menu_choice_id() & 0x80 == 0
            && let Some(id) = self.message.id
        {
            let pause = self.message.pause;
            self.begin_message(id, pause);
        }
        let was_active = self.message.active;
        if let Some(id) = self.message.id
            && self.message.active
            && !self.message.has_source()
        {
            let bytes = text.message(room, u16::from(id)).unwrap_or(&[]).to_vec();
            self.message.feed_source(&bytes);
        }
        let selected = self.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)];
        self.message.update(input, text, selected);
        // A dismissal (input, timer or an empty source) releases the pause
        // mask before any replacement the post-actions request.
        if was_active && !self.message.active {
            // TODO(parity): (input) the original also blanks the held and
            // previous-held d-pad bits on a state 5/6 dismissal unless
            // message_flags bit 0 is set; only the action press is swallowed
            // here, so a direction held through the dismissal resumes at once.
            self.message_flags = self.message_flags_backup;
        }
        let pause = self.message.pause;
        for action in self.message.take_actions() {
            match action {
                MessageAction::Chain(id) => self.show_message(id, pause),
                MessageAction::TakeItem => {
                    self.take_message_item();
                }
                MessageAction::UseSelectedItem => self.use_selected_item(),
                MessageAction::DiscardSelectedItem => self.discard_selected_item(),
            }
        }
        self.sync_message_choice();
    }

    /// Post-action 0: award the armed room action's item, or the radio's
    /// scenario flag. Returns whether anything was taken.
    pub fn take_message_item(&mut self) -> bool {
        let Some(slot) = self.message_item_slot.take() else {
            return false;
        };
        let Some(action) = self.room_actions.get(usize::from(slot)).copied().flatten() else {
            return false;
        };
        if action.kind != RoomActionKind::Item {
            return false;
        }
        if action.item_id() == ITEM_COMM_RADIO {
            self.apply_flag(BANK_SCENARIO, SCENARIO_FLAG_HAS_RADIO, 0);
            self.room_actions[usize::from(slot)] = None;
            self.doors[usize::from(slot)] = None;
            return true;
        }
        self.pick_up(slot)
    }

    /// Post-action 1: the original's `use_room_action_item`. The lockpick is
    /// exempt, weapons are unequipped and removed, and consumables lose one
    /// unit; a door key that hits zero raises the key-depleted flag and stays
    /// in the inventory as an empty stack.
    pub fn use_selected_item(&mut self) {
        let Some(item) = self.selected_item else {
            return;
        };
        self.record_used_item(item);
        if item == ITEM_LOCK_PICK {
            return;
        }
        let Some(index) = self.inventory.iter().position(|stack| stack.id == item) else {
            return;
        };
        if item < ITEM_CLIP {
            if self.equipped == Some(item) {
                self.set_equipped(None);
            }
            self.inventory.remove(index);
            self.rebuild_slots();
            return;
        }
        let quantity = self.inventory[index].quantity;
        if quantity == 0 {
            return;
        }
        self.inventory[index].quantity = quantity - 1;
        if quantity - 1 == 0 {
            if item > ITEM_OIL && item < ITEM_DESK_KEY {
                self.apply_flag(5, MSF_MENU_KEY_DEPLETED, 0);
                return;
            }
            self.inventory.remove(index);
            self.rebuild_slots();
        }
    }

    /// Post-action 2: drop the selected item's whole stack. The original only
    /// reaches this from script data; there is no player-facing discard.
    pub fn discard_selected_item(&mut self) {
        let Some(item) = self.selected_item else {
            return;
        };
        if let Some(index) = self.inventory.iter().position(|stack| stack.id == item) {
            self.inventory.remove(index);
            self.rebuild_slots();
        }
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
            // TODO(parity): (scripting) the original compares against a byte
            // whose "nothing used/picked" value is 0, so `testitem 0` /
            // `testpickup 0` are true when no item has been used/picked yet;
            // `Option`-based matching here makes those always false.
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
            // TODO(parity): (scripting) conditions 0x38 dpad test, 0x3C player
            // distance, 0x3F player direction and 0x50 costume variant report
            // false here (recorded placeholders), so scripts using them always
            // take the else branch.
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
                let cut = self.state.camera.current_cut;
                self.state.camera.saved_cut = Some(cut);
                self.state.set_byte(STATE_BYTE_CUT, cut as u8);
                self.state
                    .set_camera_cut(usize::from(operand_u8(operands, 0)));
                self.state.camera.locked = true;
                StepResult::Continue
            }
            0x0A => {
                if let Some(saved) = self.state.camera.saved_cut.take() {
                    self.state.set_camera_cut(saved);
                    self.state.set_byte(STATE_BYTE_CUT, saved as u8);
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
            //
            // TODO(parity): (scripting) 0x1C is the original's
            // `room_light_fade_set` (writes specialRoomLightR and the light
            // state/delta words), not an equipped-item condition; 0x3A
            // cut_zone_set, 0x40 light_param_set and 0x46 room_lights_set are
            // also placeholders.
            0x1C => self.equipped_test(operands),
            _ => self.placeholder(op),
        }
    }

    fn on_message(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            0x0B => {
                self.state
                    .show_message(operand_u8(operands, 0), operand_u16(operands, 1));
                StepResult::Continue
            }
            // TODO(parity): (scripting) 0x1E voice play (which raises the
            // voice-playing bit event scripts wait on) and 0x29 FMV request stay
            // placeholders.
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
                // TODO(parity): (scripting/gameplay) the original only arms the
                // room action here (MSF_MENU_MODE_GOT_ITEM + toggled ITEM_VIEW)
                // and awards the item when the item-viewer flow closes; this
                // awards it immediately and never opens the "got item" viewer.
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
            // TODO(parity): (scripting) 0x44 scd_event_kill (deactivate an event
            // slot) stays a placeholder.
            _ => self.placeholder(op),
        }
    }

    fn on_item(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            0x18 => {
                // TODO(parity): (scripting) the original maps item-model ids
                // 0x4E..=0x53 to the key-pickup handler (0x0F) and documents to
                // 0x0D, skips an ink-ribbon model on Jill's first playthrough,
                // and masks the +0x18 flags word it writes back; this always
                // registers an ordinary item pickup (HANDLER_ITEM). The record's
                // Y coordinate is dropped too.
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
            // TODO(parity): (gameplay) 0x2C is the original's `item_remove`, which
            // clears the WHOLE slot holding the item and rearranges the
            // inventory; `remove_item` here only takes one unit off the stack.
            0x2C => {
                let removed = self.state.remove_item(operand_u8(operands, 0));
                condition_result(removed)
            }
            0x1C => self.equipped_test(operands),
            // TODO(parity): (scripting) 0x19 model_flag_set and 0x4C
            // item_record_transfer (moves a pick-up quantity between a room
            // action record and the BioCard bytes 0x20C..0x20E) stay placeholders.
            0x4C => self.placeholder(op),
            _ => self.placeholder(op),
        }
    }

    fn on_enemy(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.op {
            // enemy: the 22-byte character spawn record.
            0x1B => {
                self.state.spawn_enemy(operands);
                StepResult::Continue
            }
            // eml_state: the multi-subcommand enemy property setter.
            0x28 => {
                self.state.apply_eml_state(operands);
                StepResult::Continue
            }
            // get_eml_state: read the enemy's behaviour_flags back.
            0x39 => {
                self.state.read_enemy_flags(operand_u8(operands, 0));
                StepResult::Continue
            }
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
                    entity.set_active(true);
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
                entity.set_active(true);
                StepResult::Continue
            }
            // spd_add: signed byte added to the player posY.
            0x45 => {
                let entity = &mut self.state.entities[0];
                entity.pos[1] = entity.pos[1].wrapping_add(i32::from(operand_i8(operands, 0)));
                StepResult::Continue
            }
            // TODO(parity): (scripting) 0x2B attack_anim_set, 0x33
            // player_prop_set (clear equip, attacked/stunned animation, flags,
            // health-status and joint writes) and 0x4D player_joint_tint stay
            // placeholders.
            _ => self.placeholder(op),
        }
    }

    /// TODO(parity): (scripting) the whole object-model class is inert: 0x1F
    /// omodel_set, 0x30 boundary_set, 0x34 model_tint_set, 0x35 obj_flag_set,
    /// 0x36 obj_field_test, 0x3B obj_rotation_set and 0x47 obj_transform_set
    /// only record a placeholder, so scripted furniture movement, model
    /// visibility flags and object-field conditions do nothing, and the
    /// climbable/pushable object probe (flag 0x40 omodels) has no table to
    /// search.
    fn on_model(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        self.placeholder(op)
    }

    /// TODO(parity): (gameplay/visual) the whole effect pool is inert: 0x2A
    /// effect_spawn and 0x3D bullet_effect_spawn, the clears 0x3E/0x42/0x48 and
    /// 0x4E effect_flags_modify only record placeholders, so muzzle flashes,
    /// blood and dust never appear and scripts waiting on effect state stall.
    fn on_effect(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        self.placeholder(op)
    }

    fn on_sound(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        // TODO(parity): (audio) the sound commands the port does not model fall
        // through to `placeholder`: 0x2F se_volume (per-channel pan/volume),
        // 0x43 se_rate (volume ramp), and 0x4A/0x4B bgm_bank_down/up. The
        // original applies each to the live DirectSound bank immediately, so
        // scripted fades and channel mixing are missing here.
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
            // TODO(parity): (audio) the original only stops the channel when its
            // BGM-state bit was set (and resets its volume); this always clears
            // the bit and queues a stop.
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
            // TODO(parity): (audio) 0x37 writes the live BGM channel byte, but
            // the original stores into the per-stage/room BGM table
            // `g_roomBgmState[stage*32 + room]` (BioCard 0x33C); scripts that set
            // a room's track this way change nothing here.
            0x37 => {
                self.state.bgm.state = operand_u8(operands, 2);
                StepResult::Continue
            }
            // TODO(parity): (audio) 0x17 3D SE, 0x27 sound fade, 0x2F pan/volume,
            // 0x43 volume ramp and 0x4A/0x4B BGM bank shift/restore stay
            // placeholders.
            _ => self.placeholder(op),
        }
    }

    fn on_misc(&mut self, op: &Op, operands: &[Operand]) -> StepResult {
        match op.mnemonic {
            // `aot_switch` (0x25): pad, sprite group id, disable flag. A zero
            // disable byte enables the group, anything else hides it. The
            // engine applies the queued toggles to the current camera cut.
            "aot_switch" => {
                self.state.mask_toggles.push(MaskToggle {
                    group: operand_u8(operands, 1),
                    active: operand_u8(operands, 2) == 0,
                });
                StepResult::Continue
            }
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
            // TODO(parity): (scripting) 0x0F mirror_set and 0x4F costume
            // variant set stay placeholders, and `evt_work_set` types 2/3
            // (object/item models) select no entity, so actor/tween ops aimed
            // at them do nothing.
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
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_ROOM_CAMERA)], 0);
        assert_eq!(state.state_bytes[3], 0);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_CHARACTER)], 1);
        assert_eq!(state.camera, CameraState::default());
        assert_eq!(state.frame, 0);
    }

    #[test]
    fn setb_2_moves_the_camera() {
        let mut state = game();
        state.set_byte(STATE_BYTE_ROOM_CAMERA, 4);
        assert_eq!(state.camera.current_cut, 4);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_ROOM_CAMERA)], 4);
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
            assert_eq!(host.state().message.menu_choice_id(), 0x80);
        }
        assert_eq!(
            state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)],
            0x80,
            "the scripts observe the window through cmpb 5"
        );
    }

    /// Encode room messages in the RDT block format: a `u16` offset table
    /// followed by the streams.
    fn room_message_block(messages: &[&[u8]]) -> Vec<u8> {
        let mut block = Vec::new();
        let mut offset = (messages.len() * 2) as u16;
        for message in messages {
            block.extend_from_slice(&offset.to_le_bytes());
            offset += message.len() as u16;
        }
        for message in messages {
            block.extend_from_slice(message);
        }
        block
    }

    fn room_with_messages(messages: &[&[u8]]) -> RoomState {
        RoomState {
            messages: Some(room_message_block(messages)),
            ..RoomState::default()
        }
    }

    /// Drive a room message to the yes/no prompt and confirm it.
    fn confirm_yes_no(state: &mut GameState, room: &RoomState) {
        let text = Text::default();
        for _ in 0..2_000 {
            state.update_message(MessageInput::default(), room, &text);
            if state.message.phase() == crate::message::MessagePhase::YesNo {
                break;
            }
        }
        assert_eq!(
            state.message.phase(),
            crate::message::MessagePhase::YesNo,
            "message never reached the yes/no prompt"
        );
        state.update_message(
            MessageInput {
                action: true,
                ..MessageInput::default()
            },
            room,
            &text,
        );
    }

    #[test]
    fn update_message_resolves_room_bytes_and_releases_cmpb_five() {
        let mut state = game();
        let room = room_with_messages(&[&[0x0C, 0x01, 0x00]]);
        state.show_message(0, 7);
        assert!(state.message.active);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)], 0x80);
        assert!(state.compare_byte(STATE_BYTE_MENU_CHOICE, 0, 0x80));

        let text = Text::default();
        for _ in 0..2 {
            state.update_message(MessageInput::default(), &room, &text);
        }
        assert_eq!(
            state.message.phase(),
            crate::message::MessagePhase::WaitInput
        );
        assert!(state.compare_byte(STATE_BYTE_MENU_CHOICE, 0, 0x80));

        state.update_message(
            MessageInput {
                action: true,
                ..MessageInput::default()
            },
            &room,
            &text,
        );
        assert!(!state.message.active);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)], 0);
        assert!(state.compare_byte(STATE_BYTE_MENU_CHOICE, 0, 0));
    }

    #[test]
    fn show_message_refuses_while_one_is_up() {
        let mut state = game();
        state.show_message(0xAA, 1);
        state.show_message(0xBB, 2);
        assert_eq!(state.message.id, Some(0xAA));
        assert_eq!(state.message.pause, 1);
    }

    #[test]
    fn message_pause_word_masks_and_restores_the_control_flags() {
        let room = room_with_messages(&[&[0x0C, 0x01, 0x00]]);
        let text = Text::default();
        let mut state = game();
        assert!(!state.message_locks_controls());
        assert_eq!(state.message_flags, MESSAGE_FLAGS_INITIAL);

        // 0x145 includes the control bit: movement locks until the window
        // dismisses, then the flags are restored.
        state.show_message(0, 0x145);
        assert!(state.message_locks_controls());
        assert_eq!(state.message_flags & MESSAGE_FLAG_CONTROLS, 0);
        for _ in 0..4 {
            state.update_message(MessageInput::default(), &room, &text);
        }
        assert_eq!(
            state.message.phase(),
            crate::message::MessagePhase::WaitInput
        );
        state.update_message(MessageInput::default(), &room, &text);
        state.update_message(
            MessageInput {
                action: true,
                ..MessageInput::default()
            },
            &room,
            &text,
        );
        assert!(!state.message.active);
        assert!(!state.message_locks_controls());
        assert_eq!(state.message_flags, MESSAGE_FLAGS_INITIAL);

        // 0xFF only masks the low byte, so the control bit stays set, and a
        // zero pause word changes nothing at all.
        state.show_message(0, 0xFF);
        assert!(!state.message_locks_controls());
        assert_eq!(
            state.message_flags & MESSAGE_FLAG_CONTROLS,
            MESSAGE_FLAG_CONTROLS
        );
        state.cancel_message();
        assert_eq!(state.message_flags, MESSAGE_FLAGS_INITIAL);
        state.show_message(0, 0);
        assert!(!state.message_locks_controls());
    }

    #[test]
    fn enter_room_keeps_an_active_door_message() {
        let mut state = game();
        state.show_message(0x40, 0xFF);
        assert!(state.message.active);
        state.enter_room(RoomId::parse("1010").unwrap(), &RoomState::default());
        assert!(
            state.message.active,
            "a door-animation message survives the room swap"
        );
        assert_eq!(state.message.id, Some(0x40));
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)], 0x80);

        state.cancel_message();
        state.enter_room(RoomId::parse("1011").unwrap(), &RoomState::default());
        assert!(!state.message.active);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)], 0);
    }

    #[test]
    fn door_adapter_direct_fields_are_armed_by_update() {
        let mut state = game();
        let room = room_with_messages(&[&[0x0C, 0x01, 0x00]]);
        // The door-animation adapter writes the window fields directly.
        state.message.id = Some(0);
        state.message.pause = 0xFE;
        state.message.active = true;
        state.update_message(MessageInput::default(), &room, &Text::default());
        assert_eq!(
            state.message.phase(),
            crate::message::MessagePhase::Reveal,
            "the direct request armed and revealed a character"
        );
        assert_eq!(state.message.pause, 0xFE);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)], 0x80);
    }

    #[test]
    fn script_setb_five_feeds_the_window_and_six_the_selected_item() {
        let mut state = game();
        state.set_byte(STATE_BYTE_MENU_CHOICE, 0x81);
        assert_eq!(state.message.menu_choice_id(), 0x81);
        state.set_byte(STATE_BYTE_SELECTED_ITEM, 0x41);
        assert_eq!(state.selected_item, Some(0x41));
        assert_eq!(
            state.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)],
            0x41
        );
    }

    #[test]
    fn message_use_action_consumes_the_selected_item() {
        let room = room_with_messages(&[&[0x0C, 0x08, 0x00, 0x0A, 0x01]]);

        // A consumable loses one unit.
        let mut state = game();
        state.add_item(0x0B, 2);
        state.select_item(Some(0x0B));
        state.show_message(0, 0);
        confirm_yes_no(&mut state, &room);
        assert_eq!(state.item_count(0x0B), 1);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_USED_ITEM)], 0x0B);
        assert_eq!(state.last_used_item, Some(0x0B));

        // A weapon is unequipped and removed.
        let mut state = game();
        state.add_item(1, 1);
        state.set_equipped(Some(1));
        state.select_item(Some(1));
        state.show_message(0, 0);
        confirm_yes_no(&mut state, &room);
        assert!(!state.has_item(1));
        assert_eq!(state.equipped, None);

        // The lockpick is never consumed.
        let mut state = game();
        state.add_item(ITEM_LOCK_PICK, 1);
        state.select_item(Some(ITEM_LOCK_PICK));
        state.show_message(0, 0);
        confirm_yes_no(&mut state, &room);
        assert!(state.has_item(ITEM_LOCK_PICK));
        assert_eq!(
            state.state_bytes[usize::from(STATE_BYTE_USED_ITEM)],
            ITEM_LOCK_PICK
        );

        // A door key that hits zero raises the depletion flag and stays as an
        // empty stack.
        let mut state = game();
        state.add_item(0x35, 1);
        state.select_item(Some(0x35));
        state.show_message(0, 0);
        confirm_yes_no(&mut state, &room);
        assert!(!state.has_item(0x35));
        assert!(
            state
                .inventory
                .iter()
                .any(|stack| stack.id == 0x35 && stack.quantity == 0)
        );
        assert!(state.flags[5].bit(MSF_MENU_KEY_DEPLETED));
    }

    #[test]
    fn message_yes_no_post_action_two_discards_the_stack() {
        let mut state = game();
        let room = room_with_messages(&[&[0x0C, 0x08, 0x00, 0x0A, 0x02]]);
        state.add_item(0x0B, 3);
        state.select_item(Some(0x0B));
        state.show_message(0, 0);
        confirm_yes_no(&mut state, &room);
        assert!(!state.has_item(0x0B));
    }

    #[test]
    fn message_chain_starts_the_next_id() {
        let mut state = game();
        let room = room_with_messages(&[&[0x0C, 0x08, 0x00, 0x09, 0x01], &[0x0D, 0x01, 0x00]]);
        state.show_message(0, 5);
        confirm_yes_no(&mut state, &room);
        assert_eq!(state.message.id, Some(1));
        assert!(state.message.active);
        assert_eq!(state.message.pause, 5, "the chain keeps the pause word");
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)], 0x80);
    }

    #[test]
    fn message_take_item_picks_up_the_armed_action() {
        let mut state = game();
        let room = room_with_messages(&[&[0x0C, 0x08, 0x00, 0x0A, 0x00]]);
        state.room_actions[3] = Some(item_action(3, 0x41, 1, [0, 0, 100, 100]));
        state.show_message_for_action(3, 0, 0xFF);
        confirm_yes_no(&mut state, &room);
        assert!(state.has_item(0x41));
        assert!(state.room_actions[3].is_none(), "the action is consumed");
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_PICKED_ITEM)], 0x41);
    }

    #[test]
    fn message_take_item_radio_raises_the_scenario_flag() {
        let mut state = game();
        let room = room_with_messages(&[&[0x0C, 0x08, 0x00, 0x0A, 0x00]]);
        state.room_actions[3] = Some(item_action(3, ITEM_COMM_RADIO, 1, [0, 0, 100, 100]));
        state.show_message_for_action(3, 0, 0xFF);
        confirm_yes_no(&mut state, &room);
        assert!(state.flag_test(0, SCENARIO_FLAG_HAS_RADIO, false));
        assert!(!state.has_item(ITEM_COMM_RADIO));
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
        assert_eq!(state.placeholders.len(), 7);
        assert_eq!(state.placeholders[&0x2B], 1);
        assert_eq!(state.placeholders[&0x1F], 1);
        assert_eq!(state.placeholders[&0x2A], 1);
        assert_eq!(state.placeholders[&0x29], 1);
        assert_eq!(state.placeholders[&0x40], 1);
        assert_eq!(state.placeholders[&0x27], 1);
        assert_eq!(state.placeholders[&0x4C], 1);
    }

    #[test]
    fn aot_switch_queues_mask_group_toggles() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        // `aot_switch(pad, group, disable)`: a zero high byte enables.
        assert_eq!(
            host.on_misc(op(0x25), &operands(&[0, 3, 0])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_misc(op(0x25), &operands(&[0xAA, 7, 1])),
            StepResult::Continue
        );
        assert_eq!(
            host.on_misc(op(0x25), &operands(&[0, 0, 0])),
            StepResult::Continue
        );
        assert!(host.state().placeholders.is_empty());
        assert_eq!(
            state.mask_toggles,
            [
                MaskToggle {
                    group: 3,
                    active: true
                },
                MaskToggle {
                    group: 7,
                    active: false
                },
                MaskToggle {
                    group: 0,
                    active: true
                },
            ]
        );
    }

    #[test]
    fn camera_only_door_keeps_the_current_room_as_target() {
        let mut state = game();
        let mut door = door(0x41, 0);
        door.camera = 0x80 | 2;
        state.room_actions[0] = Some(door_action(door));
        state.doors[0] = Some(door);
        state.interact([-550, 0, 50], 0, true);
        let transition = state.transition.expect("camera-only transition");
        assert_eq!(transition.target, state.id);
        assert_eq!(state.transition_door.unwrap().camera, 0x80 | 2);
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
        assert!(state.entities[0].active());
    }

    /// Operand list of an `enemy` (0x1B) spawn record, in signature order.
    fn enemy_record(id: u8, behavior: u8, guard: u8, force: u8, slot: u8) -> Vec<Operand> {
        operands(&[
            i64::from(id),
            i64::from(behavior),
            i64::from(guard),
            i64::from(force),
            2,    // SCA hit words
            7,    // rotation X
            2220, // yaw
            9,    // rotation Z
            7280, // X
            0,    // Y
            3920, // Z
            i64::from(slot),
            0x10, // animation id
            0x0C, // animation frame
            3,    // variant high nibble
        ])
    }

    #[test]
    fn enemy_spawn_initialises_the_character_slot() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_enemy(op(0x1B), &enemy_record(0x23, 0x06, 0xFF, 1, 0)),
            StepResult::Continue
        );
        let entity = host.state().entity(1).expect("enemy slot 0 -> entity 1");
        assert_eq!(entity.id, 0x23);
        assert_eq!(entity.behavior_flags, 0x06);
        assert!(entity.active());
        assert_eq!(entity.pos, [7280, 0, 3920]);
        assert_eq!(entity.pitch, 7);
        assert_eq!(entity.angle, 2220);
        assert_eq!(entity.roll, 9);
        assert_eq!(entity.animation_id, 0x10);
        assert_eq!(entity.animation_frame_id, 0x0C);
        assert_eq!(entity.timing_control, 1);
        assert_eq!(entity.death_event_id, 0xFF);
        assert_eq!(entity.variant, 0x30 | 0x80);
        assert_eq!(entity.sca_radius, DEFAULT_ENEMY_RADIUS);
        assert_eq!(entity.state(), 0);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.action_behavior, 0);
        assert_eq!(entity.action_state, 0);
        assert_eq!(entity.hit_state, 0);
        assert_eq!(entity.look_at_flags, 0);
        assert_eq!(host.state().enemy_count, 1);
    }

    #[test]
    fn enemy_spawn_zero_extends_xz_and_sign_extends_y() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        // X and Z are unsigned operands; Y is signed.
        let record = operands(&[0x23, 0, 0xFF, 1, 0, 0, 0, 0, -1, -1, -32768, 0, 0, 0, 0]);
        assert_eq!(host.on_enemy(op(0x1B), &record), StepResult::Continue);
        assert_eq!(host.state().entities[1].pos, [0xFFFF, -1, 0x8000]);
    }

    #[test]
    fn enemy_guard_bit_skips_the_record() {
        let mut state = game();
        state.flags[usize::from(BANK_ENEMIES)].apply(5, 0);
        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_enemy(op(0x1B), &enemy_record(0x23, 0x06, 5, 1, 0)),
            StepResult::Continue
        );
        assert!(!host.state().entities[1].active());
        assert_eq!(host.state().enemy_count, 0);
    }

    #[test]
    fn enemy_guard_ff_ignores_the_enemy_bank() {
        let mut state = game();
        state.flags[usize::from(BANK_ENEMIES)].apply(5, 0);
        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_enemy(op(0x1B), &enemy_record(0x23, 0x06, 0xFF, 1, 0)),
            StepResult::Continue
        );
        assert!(host.state().entities[1].active());
        assert_eq!(host.state().enemy_count, 1);
    }

    /// Documented deviation: the original's no-force path runs the saved-state
    /// restore and re-initialises when it misses; the port has no snapshot
    /// store, so it keeps the occupied slot. Every shipped character record
    /// sets force, so the corpus never reaches this branch.
    #[test]
    fn enemy_spawn_keeps_an_occupied_slot_without_force_init() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        host.on_enemy(op(0x1B), &enemy_record(0x23, 0x06, 0xFF, 1, 0));
        host.on_enemy(op(0x1B), &enemy_record(0x27, 0x02, 0xFF, 0, 0));
        let entity = host.state().entities[1];
        assert_eq!(entity.id, 0x23, "the occupied slot keeps its entity");
        assert_eq!(entity.behavior_flags, 0x06);
        assert_eq!(host.state().enemy_count, 1);
    }

    #[test]
    fn enemy_force_init_reinitialises_an_occupied_slot() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        host.on_enemy(op(0x1B), &enemy_record(0x23, 0x06, 0xFF, 1, 0));
        host.on_enemy(op(0x1B), &enemy_record(0x27, 0x02, 0xFF, 1, 0));
        let entity = host.state().entities[1];
        assert_eq!(entity.id, 0x27);
        assert_eq!(entity.behavior_flags, 0x02);
        assert_eq!(host.state().enemy_count, 1, "re-init does not double count");
    }

    #[test]
    fn enemy_slot_nibble_selects_the_entity_and_counts_allocations() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        host.on_enemy(op(0x1B), &enemy_record(0x23, 0, 0xFF, 1, 0));
        host.on_enemy(op(0x1B), &enemy_record(0x27, 0, 0xFF, 1, 3));
        assert!(host.state().entities[1].active());
        assert!(host.state().entities[4].active());
        assert_eq!(host.state().enemy_count, 2);
    }

    #[test]
    fn enemy_monster_ids_allocate_nothing() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        for id in [0x00, 0x11, 0x15, 0x16, 0x1F, 0x2F] {
            assert_eq!(
                host.on_enemy(op(0x1B), &enemy_record(id, 0, 0xFF, 1, 0)),
                StepResult::Continue
            );
        }
        assert!(
            host.state().entities[1..]
                .iter()
                .all(|entity| !entity.active())
        );
        assert_eq!(host.state().enemy_count, 0);
    }

    #[test]
    fn eml_state_writes_behaviour_and_action_fields() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        let op = op(0x28);
        // Sub 0: behavior_flags.
        assert_eq!(
            host.on_enemy(op, &operands(&[0, 0, 0, 0x42, 0])),
            StepResult::Continue
        );
        assert_eq!(host.state().entities[1].behavior_flags, 0x42);
        // Sub 2: action_behavior, clearing action_state.
        host.state_mut().entities[1].action_state = 7;
        host.on_enemy(op, &operands(&[0, 0, 2, 0x35, 0]));
        assert_eq!(host.state().entities[1].action_behavior, 0x35);
        assert_eq!(host.state().entities[1].action_state, 0);
        // Sub 10: action_state.
        host.on_enemy(op, &operands(&[0, 0, 10, 3, 0]));
        assert_eq!(host.state().entities[1].action_state, 3);
        // Sub 1: state 2 with health and hit state.
        host.state_mut().entities[1].set_state(8);
        host.state_mut().entities[1].set_ignore(4);
        host.on_enemy(op, &operands(&[0, 0, 1, 96, 5]));
        let entity = host.state().entities[1];
        assert_eq!(entity.state(), 2);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.health, 96);
        assert_eq!(entity.hit_state, 5);
        assert_eq!(entity.action_behavior, 0);
        assert_eq!(entity.action_state, 0);
    }

    #[test]
    fn eml_state_status_flags_modes() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        let op = op(0x28);
        // The high byte selects write (0), OR (1) or XOR (2); the low byte is
        // the value.
        host.on_enemy(op, &operands(&[0, 0, 3, 0x0003, 0]));
        assert_eq!(host.state().entities[1].status_flags, 0x03);
        host.on_enemy(op, &operands(&[0, 0, 3, 0x0104, 0]));
        assert_eq!(host.state().entities[1].status_flags, 0x07);
        host.on_enemy(op, &operands(&[0, 0, 3, 0x0202, 0]));
        assert_eq!(host.state().entities[1].status_flags, 0x05);
        host.on_enemy(op, &operands(&[0, 0, 3, 0x0908, 0]));
        assert_eq!(host.state().entities[1].status_flags, 0x05);
    }

    #[test]
    fn eml_state_yaw_blend_follow_state_and_joints() {
        let mut state = game();
        let mut host = ScdGameHost::new(&mut state);
        let op = op(0x28);
        // Sub 5: yaw.
        host.on_enemy(op, &operands(&[0, 0, 5, 0x0ABC, 0]));
        assert_eq!(host.state().entities[1].angle, 0x0ABC);
        // Sub 6: blend counter cleared.
        host.state_mut().entities[1].blend_counter = 9;
        host.on_enemy(op, &operands(&[0, 0, 6, 0, 0]));
        assert_eq!(host.state().entities[1].blend_counter, 0);
        // Sub 8: the follow state entry.
        host.state_mut().entities[1].set_state(1);
        host.state_mut().entities[1].set_ignore(3);
        host.state_mut().entities[1].action_behavior = 5;
        host.state_mut().entities[1].action_state = 5;
        host.on_enemy(op, &operands(&[0, 0, 8, 0, 0]));
        let entity = host.state().entities[1];
        assert_eq!(entity.state(), 9);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.action_behavior, 0);
        assert_eq!(entity.action_state, 0);
        // Sub 9: joint flags XOR.
        host.state_mut().entities[1].joint_flags = 0x0005;
        host.on_enemy(op, &operands(&[0, 0, 9, 0x0003, 0]));
        assert_eq!(host.state().entities[1].joint_flags, 0x0006);
    }

    #[test]
    fn eml_state_ignores_unknown_slots_and_subcommands() {
        let mut state = game();
        state.entities[1].behavior_flags = 3;
        let mut host = ScdGameHost::new(&mut state);
        host.on_enemy(op(0x28), &operands(&[0, 0, 4, 0, 0]));
        host.on_enemy(op(0x28), &operands(&[0, 0xFE, 0, 0, 0]));
        assert_eq!(host.state().entities[1].behavior_flags, 3);
    }

    #[test]
    fn tick_entities_walks_active_slots_and_freezes_with_a_message() {
        let mut state = game();
        state.entities[1].set_active(true);
        state.entities[3].set_active(true);
        let room = RoomState::default();
        let pack =
            crate::pack::Pack::from_bytes(crate::pack::PackWriter::new().to_bytes().unwrap())
                .unwrap();
        let mut models = crate::npc::EntityModelCache::default();
        assert_eq!(state.tick_entities(&room, &mut models, &pack), 2);
        assert!(state.entities[1].active());
        assert!(state.entities[3].active());

        // A message whose pause word masks the entity-think bit freezes the
        // characters, and the entity tick must not run behind the window.
        state.show_message(1, MESSAGE_FLAG_ENTITIES);
        assert!(state.message_freezes_entities());
        assert_eq!(state.tick_entities(&room, &mut models, &pack), 0);

        // The player-control bit is a different one: a message that only masks
        // it keeps the characters running.
        state.cancel_message();
        state.show_message(1, MESSAGE_FLAG_CONTROLS);
        assert!(state.message_locks_controls());
        assert!(!state.message_freezes_entities());
        assert_eq!(state.tick_entities(&room, &mut models, &pack), 2);
    }

    #[test]
    fn get_eml_state_reads_the_behavior_flags() {
        let mut state = game();
        state.entities[2].behavior_flags = 0x5A;
        let mut host = ScdGameHost::new(&mut state);
        assert_eq!(
            host.on_enemy(op(0x39), &operands(&[1])),
            StepResult::Continue
        );
        assert_eq!(host.state().last_enemy_flags, 0x5A);
        // An out-of-range enemy index leaves the byte alone.
        host.on_enemy(op(0x39), &operands(&[0x7F]));
        assert_eq!(host.state().last_enemy_flags, 0x5A);
    }

    /// Decode one actor instruction from its exact bytes, wrapped in the
    /// `evt_actor_begin`/`act_end`/`evt_finish` scaffolding the event stream
    /// needs, and return the decoded actor instruction.
    fn actor_bytes(bytes: &[u8]) -> Insn {
        let mut data = vec![0x01];
        data.extend_from_slice(bytes);
        data.extend_from_slice(&[0x8B, 0xFF]);
        let (insns, trailing) = crate::scd::reader::decode_event_stream(&data, 0, data.len());
        assert!(trailing.is_empty(), "fixture left trailing bytes");
        assert_eq!(insns[1].bytes, bytes, "fixture decoded the wrong width");
        insns[1].clone()
    }

    /// Feed one decoded actor instruction to the host.
    fn dispatch_actor(host: &mut ScdGameHost<'_>, insn: &Insn) -> StepResult {
        let Decoded::Actor(op) = insn.decoded else {
            panic!("fixture is not an actor instruction");
        };
        host.on_misc(op, &insn.operands)
    }

    #[test]
    fn actor_motion_byte_fixture_records_the_target_and_steps() {
        let mut state = game();
        state.selected_entity = 0;
        state.entities[0].pos = [10, 0, 20];
        let mut host = ScdGameHost::new(&mut state);
        // flags 0x13 (yaw+pitch, position mode), target (1000, -2, 0x500),
        // yaw step 0x28, pitch step 0x14.
        let insn = actor_bytes(&[0x81, 0x13, 0xE8, 0x03, 0xFE, 0xFF, 0x00, 0x05, 0x28, 0x14]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        let entity = host.state().entities[0];
        assert_eq!(entity.pos, [10, 0, 20], "the actor run never moves");
        assert_eq!(entity.look_at_flags, 0x13);
        assert_eq!(entity.target, [1000, -2, 0x500]);
        assert_eq!(entity.target_entity, None);
        assert_eq!(entity.look_at_yaw_step, 0x28);
        assert_eq!(entity.look_at_pitch_step, 0x14);
    }

    #[test]
    fn actor_motion_defaults_the_steps_and_keeps_the_flags_byte() {
        let mut state = game();
        state.selected_entity = 0;
        state.entities[0].look_at_flags = 0x10;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x81, 0x11, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        let entity = host.state().entities[0];
        assert_eq!(entity.target, [100, 0, 0]);
        assert_eq!(entity.look_at_yaw_step, 0xC0);
        assert_eq!(entity.look_at_pitch_step, 0x40);
    }

    #[test]
    fn actor_motion_zero_low_nibble_records_only_flags() {
        let mut state = game();
        state.selected_entity = 0;
        state.entities[0].target = [7, 8, 9];
        state.entities[0].look_at_yaw_step = 0x55;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x81, 0x10]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        let entity = host.state().entities[0];
        assert_eq!(entity.look_at_flags, 0x10);
        assert_eq!(entity.target, [7, 8, 9], "no target without a low nibble");
        assert_eq!(entity.look_at_yaw_step, 0x55);
    }

    #[test]
    fn actor_motion_wraps_negative_coordinates() {
        let mut state = game();
        state.selected_entity = 0;
        let mut host = ScdGameHost::new(&mut state);
        // flags 0x33: yaw+pitch plus the +0x1000 wrap for negative x/y.
        let insn = actor_bytes(&[0x81, 0x33, 0x9C, 0xFF, 0x38, 0xFF, 0x00, 0x00, 0x20, 0x20]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
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
        // flags 0x93: target entity type 1, index 0.
        let insn = actor_bytes(&[0x81, 0x93, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        assert_eq!(host.state().entities[0].target, [50, 0, 0]);
        assert_eq!(host.state().entities[0].target_entity, Some(1));
    }

    #[test]
    fn act_end_keeps_the_ignore_flag_while_act_reset_clears_it() {
        let mut state = game();
        state.selected_entity = 0;
        state.entities[0].set_ignore(3);
        let mut host = ScdGameHost::new(&mut state);

        // 0x8B (act_end) leaves the flag alone; 0x80 (act_reset) clears it and
        // falls through to the same state-exit path. Both exit the actor
        // sub-ISA, so they are dispatched directly rather than through the
        // byte-fixture helper.
        let end = actor_op(0x8B).unwrap();
        assert_eq!(host.on_misc(end, &[]), StepResult::Continue);
        assert_eq!(
            host.state().entities[0].ignore(),
            3,
            "act_end must not clear the ignore flag"
        );

        let reset = actor_op(0x80).unwrap();
        assert_eq!(host.on_misc(reset, &[]), StepResult::Continue);
        assert_eq!(
            host.state().entities[0].ignore(),
            0,
            "act_reset clears the ignore flag"
        );
    }

    #[test]
    fn actor_run_completes_in_one_tick() {
        // A whole actor run: motion, animation and end. The VM must execute
        // every opcode in a single step, exactly like the original.
        let scripts = event_scripts(vec![vec![
            event_insn(0x1000, event_top_op(0x01).unwrap(), 1, &[]),
            actor_insn(
                0x1001,
                actor_op(0x81).unwrap(),
                10,
                &[0x13, 100, 0, 0, 0, 0],
            ),
            actor_insn(0x100B, actor_op(0x85).unwrap(), 4, &[7, 0x10, 0x21]),
            actor_insn(0x100F, actor_op(0x8B).unwrap(), 1, &[]),
            control_insn(0x1010, event_control_op(0xFF).unwrap(), 1, &[]),
        ]]);
        let mut state = game();
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = ScdGameHost::new(&mut state);
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0, "the actor run completed in one tick");
        let entity = state.entities[0];
        assert_eq!(entity.target, [100, 0, 0]);
        assert_eq!(entity.state(), 8);
        assert_eq!(entity.action_behavior, 7);
        assert_eq!(entity.animation_id, 0x10);
        assert_eq!(entity.scd_anim_param, 0x21);
    }

    #[test]
    fn act_anim_seq_byte_fixture_maps_behavior_target_and_param() {
        let mut state = game();
        state.selected_entity = 0;
        state.entities[0].collision_flags = 0x80;
        let mut host = ScdGameHost::new(&mut state);
        // ROOM20D0's Richard walk: behaviour 3, target (7380, 2440), param 0x21.
        let insn = actor_bytes(&[0x83, 0x03, 0xD4, 0x1C, 0x88, 0x09, 0x21]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        let entity = host.state().entities[0];
        assert_eq!(entity.state(), 8);
        assert_eq!(entity.ignore(), 0);
        assert_eq!(entity.action_behavior, 3);
        assert_eq!(entity.action_state, 0);
        assert_eq!(entity.unk_c6, 0x1CD4);
        assert_eq!(entity.unk_c8, 0x0988);
        assert_eq!(entity.scd_anim_param, 0x21);
        assert_eq!(entity.scd_timer, 0x28);
        assert_eq!(entity.flags, 0);
        assert_eq!(
            entity.collision_flags, 0,
            "bit 7 is cleared when bit 4 is off"
        );
    }

    #[test]
    fn act_anim_seq_collision_flag_paths() {
        // Behaviour bit 4 set with the collision flag clear: set it and mask
        // the behaviour to its low nibble.
        let mut state = game();
        state.selected_entity = 0;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x83, 0x13, 0xD4, 0x1C, 0x88, 0x09, 0x21]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        assert_eq!(state.entities[0].action_behavior, 3);
        assert_eq!(state.entities[0].collision_flags, 0x80);

        // Already set: keep it and leave action_state untouched.
        state.entities[0].action_state = 5;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x83, 0x13, 0xD4, 0x1C, 0x88, 0x09, 0x21]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        assert_eq!(state.entities[0].action_behavior, 3);
        assert_eq!(state.entities[0].collision_flags, 0x80);
        assert_eq!(state.entities[0].action_state, 5);
    }

    #[test]
    fn act_anim_flags_byte_fixture_maps_id_param_and_flags() {
        let mut state = game();
        state.selected_entity = 0;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x84, 0x20, 0x21, 0x02]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        let entity = host.state().entities[0];
        assert_eq!(entity.state(), 8);
        assert_eq!(entity.action_behavior, 1);
        assert_eq!(entity.action_state, 0);
        assert_eq!(entity.animation_id, 0x20);
        assert_eq!(entity.scd_anim_param, 0x21);
        assert_eq!(entity.flags, 8);
        assert_eq!(entity.scd_timer, 0);
    }

    #[test]
    fn act_anim_set_byte_fixture_maps_behavior_id_and_param() {
        let mut state = game();
        state.selected_entity = 0;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x85, 0x07, 0x10, 0x21]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        let entity = host.state().entities[0];
        assert_eq!(entity.state(), 8);
        assert_eq!(entity.action_behavior, 7);
        assert_eq!(entity.action_state, 0);
        assert_eq!(entity.animation_id, 0x10);
        assert_eq!(entity.scd_anim_param, 0x21);
        assert_eq!(entity.scd_timer, 0);
        assert_eq!(entity.flags, 0);
    }

    #[test]
    fn act_param_set_byte_fixture_writes_the_timer() {
        let mut state = game();
        state.selected_entity = 0;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x88, 0x00, 0x34, 0x12]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        assert_eq!(host.state().entities[0].scd_timer, 0x1234);
    }

    #[test]
    fn act_action_a_byte_fixture_maps_frame_blend_and_state() {
        let mut state = game();
        state.selected_entity = 0;
        state.entities[0].id = 0x27;
        state.entities[0].flags = 0x20;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x89, 0x0F]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        let entity = host.state().entities[0];
        assert_eq!(entity.animation_frame_id, 0x0F);
        assert_eq!(entity.timing_control, 0);
        assert_eq!(entity.blend_counter, 0, "flag 0x20 zeroes the blend");
        assert_eq!(entity.move_speed_current, 0);
        assert_eq!(entity.action_state, 1, "characters take the simple path");

        // Monster ids remap the animation through the pair table.
        let mut state = game();
        state.selected_entity = 0;
        state.entities[0].id = 0x05;
        state.entities[0].animation_id = 5;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x89, 0x0F]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        assert_eq!(state.entities[0].action_state, 2);
        assert_eq!(state.entities[0].animation_id, 0);

        // An id above 0x0F parks in action state 3.
        state.entities[0].animation_id = 0x10;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x89, 0x0F]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        assert_eq!(state.entities[0].action_state, 3);
    }

    #[test]
    fn act_action_b_byte_fixture_never_touches_action_state() {
        let mut state = game();
        state.selected_entity = 0;
        state.entities[0].id = 0x27;
        state.entities[0].action_state = 2;
        let mut host = ScdGameHost::new(&mut state);
        let insn = actor_bytes(&[0x8A, 0x16]);
        assert_eq!(dispatch_actor(&mut host, &insn), StepResult::Continue);
        let entity = host.state().entities[0];
        assert_eq!(entity.animation_frame_id, 0x16);
        assert_eq!(entity.timing_control, 0);
        assert_eq!(entity.blend_counter, 7);
        assert_eq!(entity.move_speed_current, 0);
        assert_eq!(entity.action_state, 2, "0x8A leaves the action state alone");
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
        assert_eq!(entity.action_behavior, 4);
    }

    #[test]
    fn tween_begin_resets_the_selected_entity() {
        let mut state = game();
        state.entities[0].action_behavior = 4;
        state.entities[0].action_state = 9;
        {
            let mut host = ScdGameHost::new(&mut state);
            let op = event_top_op(0x02).unwrap();
            assert_eq!(host.on_misc(op, &operands(&[])), StepResult::Continue);
        }
        assert_eq!(state.entities[0].action_behavior, 0);
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
        let cases: [(u8, RoomActionKind); 10] = [
            (1, RoomActionKind::Door),
            (2, RoomActionKind::Message),
            (4, RoomActionKind::Item),
            (8, RoomActionKind::ItemBox),
            (9, RoomActionKind::Event),
            (10, RoomActionKind::Other),
            (12, RoomActionKind::StairsZone),
            (15, RoomActionKind::Item),
            (16, RoomActionKind::Typewriter),
            (17, RoomActionKind::StairsHeight),
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
    fn stairs_height_update_ramps_the_player_height() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            // `aot_set` slot 4: zone [1000, 2000, 4000, 1000], handler 0x11,
            // flags 0x41 (own position, player pass), high-X edge, length 100,
            // step 36.
            host.on_room_action(
                op(0x0D),
                &operands(&[4, 1000, 2000, 4000, 1000, 0x11, 0x41, 1, 100, 36]),
            );
        }
        assert_eq!(state.stair_zones.zones().len(), 0, "store fills on probe");

        // High-X edge: local = (1000 + 4000) - x = 5000 - x.
        state.entities[0].pos = [5000, 0, 2500];
        state.interact([5000, 0, 2500], 0, false);
        assert_eq!(state.stair_zones.zones().len(), 1);
        assert_eq!(state.stair_height, Some(36));
        assert_eq!(state.entities[0].pos[1], 36);

        state.entities[0].pos = [3000, 0, 2500];
        state.interact([3000, 0, 2500], 0, false);
        assert_eq!(state.stair_height, Some(21 * 36));
        assert_eq!(state.entities[0].pos[1], 21 * 36);

        // Outside the zone nothing is applied and the height clears.
        state.interact([900, 0, 2500], 0, false);
        assert_eq!(state.stair_height, None);

        // `apply_stair_state` copies the ramp onto the visible player.
        let mut player = crate::player::spawn(RoomId::default(), &RoomState::default());
        player.pos = [3000, 0, 2500];
        state.interact([3000, 0, 2500], 0, false);
        state.apply_stair_state(&mut player);
        assert_eq!(player.stairs.height, Some(21 * 36));
        assert_eq!(player.pos[1], 21 * 36);
    }

    #[test]
    fn stairs_height_uses_the_player_position_not_the_probe() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            // Flags 0x01 (no own-position bit): the probe is the 600-unit
            // reach point, but the height still measures the player.
            host.on_room_action(
                op(0x0D),
                &operands(&[0, 1000, 2000, 4000, 1000, 0x11, 0x01, 0, 100, 10]),
            );
        }
        state.entities[0].pos = [2500, 0, 2500];
        // Facing +X the reach point is 3100, inside the zone.
        state.interact([2500, 0, 2500], 0, false);
        assert_eq!(state.stair_height, Some(16 * 10));
        // The probe itself would have measured 3100 -> 22 steps.
        assert_ne!(state.stair_height, Some(22 * 10));
    }

    #[test]
    fn set_stairs_zone_latches_the_ladder_entry() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            // Slot 2, handler 0x0C, action-key probe, ladder variant 1, base
            // (5000, 6000).
            host.on_room_action(
                op(0x0D),
                &operands(&[2, 1000, 2000, 1000, 1000, 0x0C, 0x81, 1, 5000, 6000]),
            );
        }
        state.entities[0].pos = [1300, 0, 2500];
        state.interact([1300, 0, 2500], 0, true);
        let entry = state.stair_entry.expect("entry latched");
        assert_eq!(entry.slot, 2);
        assert!(entry.ladder);
        assert_eq!((entry.base_x, entry.base_z), (5000, 6000));
        assert_eq!((entry.target_x, entry.target_z), (5000, 6000));
        assert_eq!(state.entities[0].zone_flags & 0x30, 0x30);
        assert_eq!(state.entities[0].unk_c6, 5000);
        assert_eq!(state.entities[0].unk_c8, 6000);
        assert!(state.ladder_down);
        assert!(state.stair_climb);
        // The entry's own low word toggled, so a two-way ladder flips ends.
        assert_eq!(state.room_actions[2].unwrap().param_word(0), 0);

        // The action key is required: an idle probe leaves the entry alone.
        let mut fresh = game();
        {
            let mut host = ScdGameHost::new(&mut fresh);
            host.on_room_action(
                op(0x0D),
                &operands(&[2, 1000, 2000, 1000, 1000, 0x0C, 0x81, 1, 5000, 6000]),
            );
        }
        fresh.entities[0].pos = [1300, 0, 2500];
        fresh.interact([1300, 0, 2500], 0, false);
        assert!(fresh.stair_entry.is_none());
        assert_eq!(fresh.entities[0].zone_flags & 0x20, 0);
    }

    #[test]
    fn aot_on_runs_the_stair_handlers() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            host.on_room_action(
                op(0x0D),
                &operands(&[1, 0, 0, 100, 100, 0x11, 0x01, 0, 10, 5]),
            );
            host.on_room_action(
                op(0x0D),
                &operands(&[3, 0, 0, 100, 100, 0x0C, 0x01, 0, 7, 8]),
            );
        }
        state.entities[0].pos = [50, 0, 50];
        assert!(state.run_room_action(1, HANDLER_STAIRS_HEIGHT));
        assert_eq!(state.stair_height, Some(30));
        assert_eq!(state.entities[0].pos[1], 30);

        assert!(state.run_room_action(3, HANDLER_STAIRS_ZONE));
        let entry = state.stair_entry.expect("entry latched");
        assert!(!entry.ladder, "variant 0 is not the ladder variant");
        assert_eq!((entry.base_x, entry.base_z), (7, 8));
        assert_eq!(state.entities[0].zone_flags & 0x20, 0x20);
        assert!(state.ladder_down);
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
        // 0x0B (handgun clip) is in the stackable ammunition range.
        state.room_actions[0] = Some(item_action(0, 0x0B, 3, [0, 0, 100, 100]));
        state.room_actions[1] = Some(item_action(1, 0x0B, 2, [0, 0, 100, 100]));

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
                id: 0x0B,
                quantity: 3
            }]
        );
        assert_eq!(state.last_picked_item, Some(0x0B));
        assert_eq!(state.item_events, vec![0x0B]);

        state.interact([-550, 0, 50], 0, true);
        assert_eq!(
            state.inventory,
            vec![InventoryItem {
                id: 0x0B,
                quantity: 5
            }]
        );
        assert_eq!(state.item_events, vec![0x0B, 0x0B]);
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
        // (The locked message must be read first: a live window refuses the
        // next prompt, exactly like the original's set_message_display.)
        state.cancel_message();
        state.add_item(0x34, 1);
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, Some(0xC3));
        assert!(!state.has_item(0x34), "the key is consumed by the turn");
        assert!(state.flag_test(2, 5, false));

        // The next probe walks through.
        state.cancel_message();
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
        // Lock byte 0x40: bit 7 clear, so the door is not locked. The
        // original's character test compares `id & 3` against 3, which is
        // never true for the PC characters, so the restriction is inert.
        let mut state = game();
        let restricted = door(1, 0x40);
        state.room_actions[0] = Some(door_action(restricted));
        state.doors[0] = Some(restricted);

        // Jill (player flag 1) walks through: 0x40 does not bar her.
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_some());
        assert_ne!(state.message.id, Some(0xD6));

        // Chris walks through too.
        state.id.player_flag = 0;
        state.transition = None;
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_some());

        // The original's literal test still refuses id 3, keeping the branch
        // reachable for non-PC records.
        state.id.player_flag = 3;
        state.transition = None;
        state.interact([-550, 0, 50], 0, true);
        assert!(state.transition.is_none());
        assert_eq!(state.message.id, Some(0xD6));
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
        state.cancel_message();
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
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_ROOM_CAMERA)], 0);
        assert_eq!(state.inventory[0].quantity, 2);
        assert!(state.flags[0].bit(1));
        assert!(state.room_actions.iter().all(Option::is_none));
        assert!(state.doors.iter().all(Option::is_none));
        assert_eq!(state.message.id, None);
        assert_eq!(state.camera.current_cut, 0);
    }

    #[test]
    fn add_item_merges_stackables_and_forces_ribbons_to_three() {
        let mut state = game();
        state.add_item(0x0B, 10);
        state.add_item(0x0B, 4);
        assert_eq!(
            state.inventory,
            vec![InventoryItem {
                id: 0x0B,
                quantity: 14
            }]
        );
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_TOTAL_HELD)], 1);

        state.add_item(0x2F, 1);
        assert_eq!(state.item_count(0x2F), 3, "ribbons are always taken three");
        state.add_item(0x2F, 2);
        assert_eq!(state.item_count(0x2F), 6);

        state.add_item(1, 1);
        state.add_item(1, 1);
        assert_eq!(state.item_count(1), 2, "non-stackables take new slots");
    }

    #[test]
    fn add_item_caps_a_merge_and_spills_the_remainder() {
        let mut state = game();
        state.add_item(0x0B, 0xFA);
        state.add_item(0x0B, 5);
        assert_eq!(state.inventory[0].quantity, 0xFA);
        assert_eq!(
            state.inventory[1],
            InventoryItem {
                id: 0x0B,
                quantity: 5
            }
        );
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_TOTAL_HELD)], 2);
    }

    #[test]
    fn inventory_capacity_follows_the_character() {
        let chris = GameState::new(RoomId::parse("1000").unwrap(), &RoomState::default());
        assert_eq!(chris.inventory_capacity(), INVENTORY_SLOTS_CHRIS);
        let jill = game();
        assert_eq!(jill.inventory_capacity(), INVENTORY_SLOTS_JILL);
    }

    #[test]
    fn document_flags_live_at_the_file_bit_base() {
        assert_eq!(file_index(0x5E), None);
        assert_eq!(file_index(0x5F), Some(0));
        assert_eq!(file_index(0x6E), Some(15));
        assert_eq!(file_index(0x6F), None);

        let mut state = game();
        for index in 0..FILE_COUNT as u8 {
            assert!(!state.file_collected(index), "bit {index} starts clear");
        }
        state.set_file_collected(0, true);
        state.set_file_collected(15, true);
        assert!(state.file_collected(0));
        assert!(!state.file_collected(1));
        assert!(state.file_collected(15));
        // The bits are the room-flags bank's 0x82 block: 0x82 and 0x91.
        assert!(state.flags[usize::from(BANK_ROOM_FLAGS)].bit(0x82));
        assert!(state.flags[usize::from(BANK_ROOM_FLAGS)].bit(0x91));
        assert!(state.document_collected(0x5F));
        assert!(!state.document_collected(0x60));
        assert!(!state.document_collected(1));

        state.set_file_collected(0, false);
        assert!(!state.file_collected(0));
        assert!(!state.flags[usize::from(BANK_ROOM_FLAGS)].bit(0x82));
    }

    #[test]
    fn a_document_pickup_raises_its_file_flag_but_other_items_do_not() {
        let mut state = game();
        let action = item_action(3, 0x60, 1, [0, 0, 100, 100]);
        state.room_actions[3] = Some(action);
        state.pick_up(3);
        assert!(state.file_collected(1));
        assert_eq!(state.last_picked_item, Some(0x60));

        // A non-document pickup leaves the whole file block clear.
        let action = item_action(4, 0x41, 1, [0, 0, 100, 100]);
        state.room_actions[4] = Some(action);
        state.pick_up(4);
        assert!(state.file_collected(1), "the earlier bit stays");
        for index in 0..FILE_COUNT as u8 {
            if index != 1 {
                assert!(!state.file_collected(index));
            }
        }
    }

    #[test]
    fn item_box_swap_deposits_withdraws_and_clears_equipped() {
        let mut state = game();
        state.add_item(0x41, 1); // spray, not stackable
        state.set_equipped(Some(0x41));

        // Deposit the spray into box slot 5.
        assert!(state.item_box_swap(5, 0));
        assert!(state.inventory.is_empty());
        assert_eq!(
            state.item_box[5],
            InventoryItem {
                id: 0x41,
                quantity: 1
            }
        );
        assert_eq!(state.equipped, None, "the equipped item left the inventory");
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_EQUIPPED)], 0);

        // Withdraw it back into the (now empty) slot.
        assert!(state.item_box_swap(5, 0));
        assert_eq!(state.inventory.len(), 1);
        assert_eq!(state.inventory[0].id, 0x41);
        assert_eq!(state.item_box[5].id, 0);

        // An empty-to-empty swap is a no-op.
        assert!(!state.item_box_swap(6, 5));
    }

    #[test]
    fn item_box_swap_merges_withdrawn_stackables_into_an_existing_stack() {
        let mut state = game();
        state.add_item(1, 1); // knife in slot 0
        state.add_item(0x0B, 10); // handgun ammo in slot 1
        state.item_box[0] = InventoryItem {
            id: 0x0B,
            quantity: 20,
        };

        // Deposit the knife from slot 0 and withdraw the ammo, which must
        // merge into the existing ammo stack rather than leaving a duplicate.
        assert!(state.item_box_swap(0, 0));
        assert_eq!(state.item_box[0].id, 1);
        assert_eq!(state.item_count(0x0B), 30);
        assert_eq!(
            state.inventory.len(),
            1,
            "the withdrawn stack merged into the existing one"
        );
        assert_eq!(state.inventory[0].id, 0x0B);
        assert_eq!(state.inventory[0].quantity, 30);
    }

    #[test]
    fn an_item_box_merge_that_overflows_the_cap_spills_into_the_player_slot() {
        let mut state = game();
        state.add_item(1, 1); // knife in slot 0
        state.add_item(0x0B, 0xF0); // ammo in slot 1
        state.item_box[0] = InventoryItem {
            id: 0x0B,
            quantity: 0xF0,
        };

        assert!(state.item_box_swap(0, 0));
        assert_eq!(state.item_box[0].id, 1, "the knife was deposited");
        assert_eq!(state.inventory.len(), 2);
        let ammo: u32 = state.item_count(0x0B);
        assert_eq!(ammo, 0xF0 + 0xF0, "no rounds are lost");
        assert_eq!(
            state.inventory[1].quantity,
            items::ITEM_QUANTITY_CAP,
            "the existing stack caps first"
        );
        assert_eq!(
            state.inventory[0].quantity,
            (0xF0u16 + 0xF0 - u16::from(items::ITEM_QUANTITY_CAP)) as u8,
            "the remainder spills into the vacated slot"
        );
    }

    #[test]
    fn menu_use_applies_heals_cures_and_health_status_mirroring() {
        let mut state = game();
        state.max_health = 96;
        state.entities[0].health = 20;
        assert_eq!(
            state.use_item(0x44),
            UseResult::Used {
                healed: true,
                cured: false
            }
        );
        assert_eq!(state.entities[0].health, 20 + 96 / 3);
        // A spray at full health does nothing and stays in the inventory.
        state.entities[0].health = 96;
        assert_eq!(state.use_item(0x41), UseResult::Unusable);

        // Poison cures clear their bit and reach the BioCard byte.
        state.set_health_status(0x22);
        assert_eq!(
            state.use_item(0x45),
            UseResult::Used {
                healed: false,
                cured: true
            }
        );
        assert_eq!(state.health_status, 0x20);
        assert_eq!(
            state.state_bytes[usize::from(STATE_BYTE_HEALTH_STATUS)],
            0x20
        );

        // The serum clears the 0x20 poison and the scenario-2 marker.
        state.set_health_status(0x22);
        state.apply_flag(1, SCENARIO2_FLAG_YAWN_POISONED, 0);
        assert_eq!(
            state.use_item(0x42),
            UseResult::Used {
                healed: false,
                cured: true
            }
        );
        assert_eq!(state.health_status, 0x02);
        assert!(!state.flag_test(1, SCENARIO2_FLAG_YAWN_POISONED, false));

        // setb on the health-status byte mirrors into the field too.
        state.set_byte(STATE_BYTE_HEALTH_STATUS, 0x10);
        assert_eq!(state.health_status, 0x10);
    }

    #[test]
    fn a_new_state_derives_its_maximum_and_a_heal_changes_health() {
        let chris_id = RoomId::parse("100").unwrap();
        let jill_id = RoomId {
            player_flag: 1,
            ..chris_id
        };
        let mut chris = GameState::new(chris_id, &RoomState::default());
        assert_eq!(chris.max_health, 140);
        let mut jill = GameState::new(jill_id, &RoomState::default());
        assert_eq!(jill.max_health, 96);
        assert_eq!(character_max_health(0), 140);
        assert_eq!(character_max_health(1), 96);

        // The derived maximum makes a heal actually raise the health: a green
        // herb restores one third of 96 from 20.
        jill.entities[0].health = 20;
        assert_eq!(
            jill.use_item(0x44),
            UseResult::Used {
                healed: true,
                cured: false
            }
        );
        assert_eq!(jill.entities[0].health, 20 + 32);

        chris.entities[0].health = 20;
        assert_eq!(
            chris.use_item(0x44),
            UseResult::Used {
                healed: true,
                cured: false
            }
        );
        assert_eq!(chris.entities[0].health, 20 + 140 / 3);
    }

    #[test]
    fn the_serum_counts_as_used_with_only_the_secondary_poison_flag() {
        // The original's 0x10 cure nibble marks the item used whenever either
        // poison flag is up; with only 0x02 set nothing is cleared but the
        // serum is still consumed (blue EKG flush).
        let mut state = game();
        state.entities[0].health = state.max_health;
        state.set_health_status(0x02);
        assert_eq!(
            state.use_item(0x42),
            UseResult::Used {
                healed: false,
                cured: true
            }
        );
        assert_eq!(state.health_status, 0x02, "the 0x02 bit is left alone");

        // The 0x02 cure nibble does not count when only the 0x20 bit is set.
        state.set_health_status(0x20);
        assert_eq!(state.use_item(0x45), UseResult::Unusable);

        // No poison at all: unusable.
        state.set_health_status(0);
        assert_eq!(state.use_item(0x42), UseResult::Unusable);
    }

    #[test]
    fn mark_examined_raises_the_lookup_class_bit() {
        // Item 0x33 (the sword key) has name class 3.
        let mut state = game();
        assert!(!crate::message::examined_bit(&state.examined_flags(), 3));
        state.mark_examined(0x33);
        assert!(crate::message::examined_bit(&state.examined_flags(), 3));
        assert_eq!(state.examined_flags(), [0, 0, 0, 0x10]);

        // Real-name items (name class 0x80) are never tracked.
        state.mark_examined(0x44);
        assert!(!crate::message::examined_bit(&state.examined_flags(), 4));
        assert_eq!(state.examined_flags(), [0, 0, 0, 0x10]);
    }

    #[test]
    fn menu_use_flags_gate_keys_and_the_red_book() {
        let mut state = game();
        assert_eq!(state.use_item(0x33), UseResult::Unusable);
        assert_eq!(state.use_item(0x3E), UseResult::Unusable);
        assert!(!state.item_use_flag(0x33));

        state.set_item_use_flag(0x33, true);
        assert!(state.item_use_flag(0x33));
        assert_eq!(
            state.use_item(0x33),
            UseResult::Used {
                healed: false,
                cured: false
            }
        );

        // Only the red book passes the flag check; the other books do not.
        state.set_item_use_flag(0x3E, true);
        assert_eq!(
            state.use_item(0x3E),
            UseResult::Used {
                healed: false,
                cured: false
            }
        );
        assert_eq!(state.use_item(0x40), UseResult::Unusable);
        assert_eq!(state.use_item(1), UseResult::Unusable, "weapons never use");
    }

    #[test]
    fn combine_effects_transfer_ammo_and_merge_quantities() {
        // Effect 1: the gun takes the clip's rounds up to its maximum.
        let mut state = game();
        state.add_item(0x02, 1);
        state.add_item(0x0B, 30);
        assert!(matches!(
            state.combine_slots(0, 1),
            CombineResult::Applied { .. }
        ));
        assert_eq!(state.inventory[0].quantity, 15);
        assert_eq!(state.inventory[1].quantity, 16);

        // Effect 2: the clip's table takes the gun's rounds instead.
        let mut state = game();
        state.add_item(0x0B, 20);
        state.add_item(0x02, 7);
        assert!(matches!(
            state.combine_slots(0, 1),
            CombineResult::Applied { .. }
        ));
        assert_eq!(state.inventory[0].id, 0x0B);
        // The target (Beretta) fills to its maximum, 20 + 7 - 15 stay.
        assert_eq!(state.inventory[0].quantity, 20 + 7 - 15);
        assert_eq!(state.inventory[1].quantity, 15);

        // Effect 6 fills the cursor weapon from the target ammo, but refuses
        // when the weapon already holds rounds.
        let mut state = game();
        state.add_item(0x07, 1); // loaded weapon
        state.add_item(0x11, 5); // its ammo
        assert_eq!(state.combine_slots(0, 1), CombineResult::NoRecipe);
        assert_eq!(state.inventory[0].id, 0x07);
        assert_eq!(state.inventory[0].quantity, 1, "the loaded rounds stay");
        assert_eq!(state.inventory[1].quantity, 5, "the ammo stays");

        // Empty cursor: the same recipe transfers the target's rounds.
        state.inventory[0].quantity = 0;
        assert!(matches!(
            state.combine_slots(0, 1),
            CombineResult::Applied { .. }
        ));
        assert_eq!(state.inventory[0].quantity, 5);

        // Effect 7 fills the target weapon from the cursor ammo, but refuses
        // when the weapon already holds rounds (the reverse pair).
        let mut state = game();
        state.add_item(0x10, 4); // ammo
        state.add_item(0x08, 2); // loaded weapon
        assert_eq!(state.combine_slots(0, 1), CombineResult::NoRecipe);
        assert_eq!(state.inventory[0].quantity, 4, "the ammo stays");

        // Empty target: the recipe transfers the cursor's rounds into it.
        state.inventory[1].quantity = 0;
        assert!(matches!(
            state.combine_slots(0, 1),
            CombineResult::Applied { .. }
        ));
        let weapon = state
            .inventory
            .iter()
            .find(|slot| slot.id == 0x07)
            .expect("the target weapon stays");
        assert_eq!(weapon.quantity, 4);
        assert!(!state.inventory.iter().any(|slot| slot.id == 0x10));

        // The chemical flag effect.
        let mut state = game();
        state.id = RoomId::parse("309").unwrap();
        state.add_item(0x15, 1);
        state.add_item(0x18, 1);
        assert_eq!(
            state.combine_slots(0, 1),
            CombineResult::Applied {
                herb: false,
                chemical: true
            }
        );
        assert!(state.flag_test(0, SCENARIO_FLAG_CHEMICAL_COMBINE, false));

        // Outside the drug store the same recipe is refused untouched.
        let mut state = game();
        state.add_item(0x15, 1);
        state.add_item(0x18, 1);
        assert_eq!(state.combine_slots(0, 1), CombineResult::NeedsDrugStore);
        assert_eq!(state.inventory[0].id, 0x15);
        assert_eq!(state.inventory[1].id, 0x18);
    }

    #[test]
    fn item_writers_update_their_state_bytes_and_conditions() {
        let mut state = game();
        state.select_item(Some(0x42));
        state.record_used_item(0x42);
        state.set_equipped(Some(0x42));
        assert_eq!(state.selected_item, Some(0x42));
        assert_eq!(state.last_used_item, Some(0x42));
        assert_eq!(state.equipped, Some(0x42));
        assert_eq!(
            state.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)],
            0x42
        );
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_USED_ITEM)], 0x42);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_EQUIPPED)], 0x42);
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_flow(op(0x10), &operands(&[0x42])),
                StepResult::Continue
            );
            assert_eq!(
                host.on_flow(op(0x1D), &operands(&[0x42])),
                StepResult::Continue
            );
        }

        state.select_item(None);
        state.set_equipped(None);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)], 0);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_EQUIPPED)], 0);
        assert_eq!(state.selected_item, None);
        assert_eq!(state.equipped, None);
    }

    #[test]
    fn pickup_writes_the_picked_item_and_total_state_bytes() {
        let mut state = game();
        let mut action = item_action(0, 0x42, 2, [0, 0, 100, 100]);
        action.room_items_flag = 7;
        state.room_actions[0] = Some(action);
        state.interact([-550, 0, 50], 0, true);
        assert_eq!(state.last_picked_item, Some(0x42));
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_PICKED_ITEM)], 0x42);
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_TOTAL_HELD)], 1);
        assert!(state.flag_test(7, 7, false));
    }

    #[test]
    fn cutnext_and_cutcurr_mirror_the_camera_bytes() {
        let mut state = game();
        {
            let mut host = ScdGameHost::new(&mut state);
            assert_eq!(
                host.on_camera(op(0x09), &operands(&[3])),
                StepResult::Continue
            );
            assert_eq!(
                host.state().state_bytes[usize::from(STATE_BYTE_ROOM_CAMERA)],
                3
            );
            assert_eq!(host.state().state_bytes[usize::from(STATE_BYTE_CUT)], 0);
            assert_eq!(
                host.on_camera(op(0x0A), &operands(&[])),
                StepResult::Continue
            );
            assert_eq!(
                host.state().state_bytes[usize::from(STATE_BYTE_ROOM_CAMERA)],
                0
            );
        }
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

    #[test]
    fn typewriter_prompts_follow_the_ribbon_and_playthrough_rules() {
        // Chris without a ribbon: refused with the no-ribbon message.
        let mut chris = GameState::new(RoomId::parse("1000").unwrap(), &RoomState::default());
        assert!(!chris.check_typewriter());
        assert_eq!(chris.message.id, Some(MESSAGE_TYPEWRITER_NO_RIBBON));
        assert_eq!(chris.typewriter, TypewriterFlow::Idle);

        // Chris with a ribbon: the ribbon prompt, and a confirmed save says
        // the write must consume one.
        let mut chris = GameState::new(RoomId::parse("1000").unwrap(), &RoomState::default());
        chris.add_item(items::ITEM_INK_RIBBONS, 1);
        assert!(chris.check_typewriter());
        assert_eq!(chris.message.id, Some(MESSAGE_TYPEWRITER_RIBBON_PROMPT));
        assert_eq!(
            chris.typewriter,
            TypewriterFlow::Prompt { ink_ribbon: true }
        );
        // The answer only lands once the window dismissed.
        assert_eq!(chris.take_typewriter_confirm(), None);
        chris.message = MessageWindow::default();
        chris.message.set_menu_choice_id(0x00);
        assert_eq!(chris.take_typewriter_confirm(), Some(true));
        assert_eq!(chris.typewriter, TypewriterFlow::Idle);

        // Jill's first playthrough saves without a ribbon and gets the
        // progress prompt.
        let mut jill = GameState::new(RoomId::parse("1001").unwrap(), &RoomState::default());
        assert!(jill.check_typewriter());
        assert_eq!(jill.message.id, Some(MESSAGE_TYPEWRITER_SAVE_PROMPT));
        assert_eq!(
            jill.typewriter,
            TypewriterFlow::Prompt { ink_ribbon: false }
        );
        jill.message = MessageWindow::default();
        jill.message.set_menu_choice_id(0x00);
        assert_eq!(jill.take_typewriter_confirm(), Some(false));

        // A decline clears the flow without a save.
        let mut jill = GameState::new(RoomId::parse("1001").unwrap(), &RoomState::default());
        assert!(jill.check_typewriter());
        jill.message = MessageWindow::default();
        jill.message.set_menu_choice_id(0x01);
        assert_eq!(jill.take_typewriter_confirm(), None);
        assert_eq!(jill.typewriter, TypewriterFlow::Idle);

        // On a second playthrough Jill needs a ribbon like Chris.
        let mut jill = GameState::new(RoomId::parse("1001").unwrap(), &RoomState::default());
        jill.apply_flag(0, SCENARIO_FLAG_SECOND_PLAYTHROUGH, 0);
        assert!(!jill.check_typewriter());
        assert_eq!(jill.message.id, Some(MESSAGE_TYPEWRITER_NO_RIBBON));
    }

    #[test]
    fn consuming_a_ribbon_records_the_use_and_compacts_the_inventory() {
        let mut state = game();
        state.add_item(items::ITEM_INK_RIBBONS, 1);
        assert_eq!(state.item_count(items::ITEM_INK_RIBBONS), 3);
        state.consume_ink_ribbon();
        assert_eq!(state.item_count(items::ITEM_INK_RIBBONS), 2);
        assert_eq!(state.last_used_item, Some(items::ITEM_INK_RIBBONS));
        state.consume_ink_ribbon();
        state.consume_ink_ribbon();
        assert!(!state.has_item(items::ITEM_INK_RIBBONS));
        assert!(state.inventory.is_empty());
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_TOTAL_HELD)], 0);
    }

    #[test]
    fn the_save_counter_clamps_at_ninety_nine() {
        let mut state = game();
        state.state_bytes[usize::from(STATE_BYTE_SAVES)] = 98;
        state.increment_saves();
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_SAVES)], 99);
        state.increment_saves();
        assert_eq!(state.state_bytes[usize::from(STATE_BYTE_SAVES)], 99);
    }
}
