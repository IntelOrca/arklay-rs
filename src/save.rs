//! The original 0x800-byte BioCard save block and the save directory.
//!
//! A save is the original block byte for byte: a 0x200-byte bio-card prefix
//! (magic, timestamp and icon graphics, shipped as `data/bio_card.dat`)
//! followed by the little-endian state fields at their original offsets. The
//! block is 0x800 bytes long; everything past the last field is zero padding.
//!
//! [`SaveFile`] can be built from a [`GameState`], serialized, parsed back and
//! applied to a state, so a load rebuilds the room identity, position, health,
//! inventory, item box, flags, save counter and the room-BGM table.
//!
//! Slots live under a save directory as `savedat1.dat`..`savedat8.dat`
//! (zero-based slot indexes). [`scan_slots`] reports which slots hold data for
//! the load screen; missing or truncated files read as empty.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::budget;
use crate::game::{
    BgmState, CameraState, FLAG_BANK_COUNT, FlagBank, GameState, ITEM_BOX_SLOTS, InventoryItem,
    STATE_BYTE_CHARACTER, STATE_BYTE_CUT, STATE_BYTE_ENT_ACTION, STATE_BYTE_EQUIPPED,
    STATE_BYTE_FWD_ACTION, STATE_BYTE_HEALTH_STATUS, STATE_BYTE_MENU_CHOICE,
    STATE_BYTE_PICKED_ITEM, STATE_BYTE_ROOM_CAMERA, STATE_BYTE_SAVES, STATE_BYTE_SELECTED_ITEM,
    STATE_BYTE_TOTAL_HELD, STATE_BYTE_USED_ITEM, STATE_BYTES, VoiceState, character_max_health,
};
use crate::state::RoomId;

/// Bytes in one save block.
pub const SAVE_BLOCK_SIZE: usize = 0x800;
/// Bytes of the block the field layout actually uses (0x000..=0x41B).
pub const SAVE_LAYOUT_SIZE: usize = 0x41C;
/// Bytes of the bio-card prefix at the start of the block.
pub const PREFIX_LEN: usize = 0x200;
/// BioCard offset the [`GameState`] state image starts at.
pub const STATE_IMAGE_OFFSET: usize = 0x200;
/// Number of save slots.
pub const SAVE_SLOT_COUNT: usize = 8;
/// Bytes of room-BGM state at the end of the layout.
pub const ROOM_BGM_LEN: usize = crate::game::ROOM_BGM_LEN;
/// Player inventory slots in the block (Chris uses 6, Jill 8).
pub const PLAYER_SLOT_COUNT: usize = 12;
/// Default save directory beside the pack.
pub const DEFAULT_SAVE_DIR: &str = "saves";
/// Pack entry the bio-card prefix comes from.
pub const SAVE_PREFIX_ENTRY: &str = "data/bio_card.dat";

/// One parsed save block.
///
/// The struct mirrors the original's BioCard layout field for field; see the
/// module docs for the byte offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFile {
    /// Bio-card prefix copied verbatim from `data/bio_card.dat`.
    pub prefix: [u8; PREFIX_LEN],
    /// The 64-byte state image at BioCard 0x200..0x240, copied verbatim. The
    /// explicit fields below mirror the parts the port owns; the remaining
    /// bytes (0x208..0x20F, 0x214..0x223) survive only here.
    pub state_bytes: [u8; STATE_BYTES],
    /// Stage id (0x200).
    pub stage: u8,
    /// Room id (0x201).
    pub room: u8,
    /// Room camera id (0x202).
    pub camera: u8,
    /// Cut saved by `cutnext` (0x204).
    pub cut: u8,
    /// Message-window menu choice (0x205).
    pub menu_choice: u8,
    /// Selected inventory item id (0x206).
    pub selected_item: u8,
    /// Number of held inventory slots (0x207).
    pub total_held: u8,
    /// Action hit by the forward position probe (0x210).
    pub fwd_action: u8,
    /// Action hit by the entity position probe (0x211).
    pub ent_action: u8,
    /// Last item used (0x212).
    pub used_item: u8,
    /// Last item picked up (0x213).
    pub picked_item: u8,
    /// Player health (0x21E).
    pub health: i16,
    /// Play timer snapshot (0x224).
    pub timer: u32,
    /// Save counter (0x228).
    pub saves: u8,
    /// Equipped item id (0x229).
    pub equipped: u8,
    /// Room-item backup byte (0x22A).
    pub room_item_backup: u8,
    /// Selected character id (0x22B).
    pub character: u8,
    /// Player position X (0x22C).
    pub pos_x: i16,
    /// Player position Z (0x22E).
    pub pos_z: i16,
    /// Player facing angle (0x230).
    pub angle: i16,
    /// Health status flags (0x232).
    pub health_status: u8,
    /// Scenario bank 2, 32 bytes (0x234).
    pub scenario2: [u8; 32],
    /// Door/desk lock flags, 8 bytes (0x254).
    pub locks: [u8; 8],
    /// Enemy flags, 32 bytes (0x25C).
    pub enemies: [u8; 32],
    /// Room-item flags, 32 bytes (0x27C).
    pub room_items: [u8; 32],
    /// Examined-item flags, 4 bytes (0x29C).
    pub examined: [u8; 4],
    /// Scenario bank 0, 16 bytes (0x2A0).
    pub scenario: [u8; 16],
    /// Room flags, 20 bytes (0x2B0).
    pub room_flags: [u8; 20],
    /// The 48 item-box slots (0x2C4).
    pub item_box: [InventoryItem; ITEM_BOX_SLOTS],
    /// The 12 player slots (0x324).
    pub player_slots: [InventoryItem; PLAYER_SLOT_COUNT],
    /// Room-BGM state table, 224 bytes (0x33C).
    pub room_bgm: [u8; ROOM_BGM_LEN],
}

impl Default for SaveFile {
    fn default() -> Self {
        Self {
            prefix: [0; PREFIX_LEN],
            state_bytes: [0; STATE_BYTES],
            stage: 0,
            room: 0,
            camera: 0,
            cut: 0,
            menu_choice: 0,
            selected_item: 0,
            total_held: 0,
            fwd_action: 0,
            ent_action: 0,
            used_item: 0,
            picked_item: 0,
            health: 0,
            timer: 0,
            saves: 0,
            equipped: 0,
            room_item_backup: 0,
            character: 0,
            pos_x: 0,
            pos_z: 0,
            angle: 0,
            health_status: 0,
            scenario2: [0; 32],
            locks: [0; 8],
            enemies: [0; 32],
            room_items: [0; 32],
            examined: [0; 4],
            scenario: [0; 16],
            room_flags: [0; 20],
            item_box: [InventoryItem::default(); ITEM_BOX_SLOTS],
            player_slots: [InventoryItem::default(); PLAYER_SLOT_COUNT],
            room_bgm: [0; ROOM_BGM_LEN],
        }
    }
}

impl SaveFile {
    /// Mirror every field the port owns into [`SaveFile::state_bytes`], so the
    /// captured image describes exactly what [`SaveFile::to_bytes`] writes and
    /// a reload followed by a re-capture is identical. The bytes with no
    /// explicit field (0x208..0x20F, 0x214..0x223) are left untouched.
    fn sync_state_image(&mut self) {
        self.state_bytes[0] = self.stage;
        self.state_bytes[1] = self.room;
        self.state_bytes[usize::from(STATE_BYTE_ROOM_CAMERA)] = self.camera;
        self.state_bytes[usize::from(STATE_BYTE_CUT)] = self.cut;
        self.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)] = self.menu_choice;
        self.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)] = self.selected_item;
        self.state_bytes[usize::from(STATE_BYTE_TOTAL_HELD)] = self.total_held;
        self.state_bytes[usize::from(STATE_BYTE_FWD_ACTION)] = self.fwd_action;
        self.state_bytes[usize::from(STATE_BYTE_ENT_ACTION)] = self.ent_action;
        self.state_bytes[usize::from(STATE_BYTE_USED_ITEM)] = self.used_item;
        self.state_bytes[usize::from(STATE_BYTE_PICKED_ITEM)] = self.picked_item;
        put_i16(&mut self.state_bytes, 0x1E, self.health);
        put_u32(&mut self.state_bytes, 0x24, self.timer);
        self.state_bytes[usize::from(STATE_BYTE_SAVES)] = self.saves;
        self.state_bytes[usize::from(STATE_BYTE_EQUIPPED)] = self.equipped;
        self.state_bytes[0x2A] = self.room_item_backup;
        self.state_bytes[usize::from(STATE_BYTE_CHARACTER)] = self.character;
        put_i16(&mut self.state_bytes, 0x2C, self.pos_x);
        put_i16(&mut self.state_bytes, 0x2E, self.pos_z);
        put_i16(&mut self.state_bytes, 0x30, self.angle);
        self.state_bytes[usize::from(STATE_BYTE_HEALTH_STATUS)] = self.health_status;
        // BioCard 0x234 opens the 32-byte scenario bank 2; its first 12 bytes
        // fall inside the 0x200..0x240 image.
        self.state_bytes[0x34..].copy_from_slice(&self.scenario2[..STATE_BYTES - 0x34]);
    }

    /// Capture a game state. The bio-card prefix stays zero until
    /// [`SaveFile::set_prefix`] or [`SaveFile::from_state_with_prefix`].
    ///
    /// The whole BioCard state image is captured: the explicit fields mirror
    /// the parts the port owns and [`SaveFile::state_bytes`] preserves the
    /// rest, including BioCard 0x208..0x20F (special-room light, character
    /// model id, last enemy flags, bullet effect id, pickup quantities A/B/C)
    /// and the 0x214..0x223 words (fade state, light, rand seed, countdown,
    /// health copy, pad), so the quantities opcode 0x4C moves survive a reload.
    pub fn from_state(state: &GameState) -> Self {
        // BioCard 0x224 is the play-timer snapshot the original mirrors every
        // frame; it lives in the state byte image.
        let timer = u32::from_le_bytes(
            state.state_bytes[0x24..0x28]
                .try_into()
                .expect("timer bytes"),
        );
        let mut file = Self {
            state_bytes: state.state_bytes,
            stage: state.id.stage,
            room: state.id.room,
            camera: state.camera.current_cut.min(0xFF) as u8,
            cut: state.state_bytes[usize::from(STATE_BYTE_CUT)],
            menu_choice: state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)],
            selected_item: state.selected_item.unwrap_or(0),
            total_held: state.inventory.len().min(PLAYER_SLOT_COUNT) as u8,
            fwd_action: state.state_bytes[usize::from(STATE_BYTE_FWD_ACTION)],
            ent_action: state.state_bytes[usize::from(STATE_BYTE_ENT_ACTION)],
            used_item: state.last_used_item.unwrap_or(0),
            picked_item: state.last_picked_item.unwrap_or(0),
            health: state.entities[0].health,
            timer,
            saves: state.state_bytes[usize::from(STATE_BYTE_SAVES)],
            equipped: state.equipped.unwrap_or(0),
            room_item_backup: state.state_bytes[0x2A],
            character: state.state_bytes[usize::from(STATE_BYTE_CHARACTER)],
            pos_x: state.entities[0].pos[0] as i16,
            pos_z: state.entities[0].pos[2] as i16,
            angle: state.entities[0].angle as i16,
            health_status: state.health_status,
            examined: state.examined_flags(),
            item_box: state.item_box,
            room_bgm: state.room_bgm,
            ..Self::default()
        };
        file.scenario.copy_from_slice(&state.flags[0].bytes()[..16]);
        file.scenario2.copy_from_slice(state.flags[1].bytes());
        file.locks.copy_from_slice(&state.flags[2].bytes()[..8]);
        file.enemies.copy_from_slice(state.flags[3].bytes());
        file.room_items.copy_from_slice(state.flags[7].bytes());
        file.room_flags
            .copy_from_slice(&state.flags[8].bytes()[..20]);
        for (slot, stack) in file.player_slots.iter_mut().zip(state.inventory.iter()) {
            *slot = *stack;
        }
        file.sync_state_image();
        file
    }

    /// Capture a state with the bio-card prefix copied from `data/bio_card.dat`.
    pub fn from_state_with_prefix(state: &GameState, prefix: &[u8]) -> Result<Self> {
        let mut file = Self::from_state(state);
        file.set_prefix(prefix)?;
        Ok(file)
    }

    /// Copy the 0x200-byte bio-card prefix.
    pub fn set_prefix(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < PREFIX_LEN {
            bail!(
                "bio-card prefix is {} bytes; {PREFIX_LEN} are required",
                data.len()
            );
        }
        self.prefix.copy_from_slice(&data[..PREFIX_LEN]);
        Ok(())
    }

    /// Apply the parsed block to a game state.
    ///
    /// The saved flags replace the ten banks (transient banks 4/5/6/9 are
    /// cleared), the inventory and item box are rebuilt, and the BioCard state
    /// bytes are reseeded from the block.
    pub fn apply_to(&self, state: &mut GameState) {
        state.id = RoomId {
            stage: self.stage,
            room: self.room,
            player_flag: self.character & 1,
        };
        state.flags = [FlagBank::new(); FLAG_BANK_COUNT];
        state.flags[0].bytes_mut()[..16].copy_from_slice(&self.scenario);
        state.flags[1].bytes_mut().copy_from_slice(&self.scenario2);
        state.flags[2].bytes_mut()[..8].copy_from_slice(&self.locks);
        state.flags[3].bytes_mut().copy_from_slice(&self.enemies);
        state.flags[7].bytes_mut().copy_from_slice(&self.room_items);
        state.flags[8].bytes_mut()[..20].copy_from_slice(&self.room_flags);

        state.camera = CameraState {
            current_cut: usize::from(self.camera),
            saved_cut: None,
            locked: false,
        };
        state.entities[0].health = self.health;
        state.entities[0].pos = [
            i32::from(self.pos_x),
            state.entities[0].pos[1],
            i32::from(self.pos_z),
        ];
        state.entities[0].angle = self.angle as u16 & 0x0FFF;
        // The maximum is not stored in the block; InitializeGame re-derives it
        // from the selected character on every path, new game or continue.
        state.max_health = character_max_health(self.character);
        state.health_status = self.health_status;
        state.message.examined = self.examined;
        state.selected_item = (self.selected_item != 0).then_some(self.selected_item);
        state.last_used_item = (self.used_item != 0).then_some(self.used_item);
        state.last_picked_item = (self.picked_item != 0).then_some(self.picked_item);
        state.equipped = (self.equipped != 0).then_some(self.equipped);
        state.inventory = self
            .player_slots
            .iter()
            .take(usize::from(self.total_held).min(PLAYER_SLOT_COUNT))
            .copied()
            .filter(|stack| stack.id != 0)
            .collect();
        state.item_box = self.item_box;
        // The per-room BGM table round-trips through the block; the live state
        // byte is not saved, so it resets to the nothing-playing value and the
        // destination room's entry rebuilds the channels on load.
        state.room_bgm = self.room_bgm;
        state.bgm = BgmState::default();
        state.voice = VoiceState::default();

        // The saved image comes first; the explicit fields below restate the
        // bytes the port owns, exactly like the original's prefix-then-fields
        // memcpy.
        state.state_bytes = self.state_bytes;
        state.state_bytes[0] = self.stage;
        state.state_bytes[1] = self.room;
        state.state_bytes[usize::from(STATE_BYTE_ROOM_CAMERA)] = self.camera;
        state.state_bytes[usize::from(STATE_BYTE_CUT)] = self.cut;
        state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)] = self.menu_choice;
        state.state_bytes[usize::from(STATE_BYTE_SELECTED_ITEM)] = self.selected_item;
        state.state_bytes[usize::from(STATE_BYTE_TOTAL_HELD)] = self.total_held;
        state.state_bytes[usize::from(STATE_BYTE_FWD_ACTION)] = self.fwd_action;
        state.state_bytes[usize::from(STATE_BYTE_ENT_ACTION)] = self.ent_action;
        state.state_bytes[usize::from(STATE_BYTE_USED_ITEM)] = self.used_item;
        state.state_bytes[usize::from(STATE_BYTE_PICKED_ITEM)] = self.picked_item;
        put_i16(&mut state.state_bytes, 0x1E, self.health);
        put_u32(&mut state.state_bytes, 0x24, self.timer);
        state.state_bytes[usize::from(STATE_BYTE_SAVES)] = self.saves;
        state.state_bytes[usize::from(STATE_BYTE_EQUIPPED)] = self.equipped;
        state.state_bytes[0x2A] = self.room_item_backup;
        state.state_bytes[usize::from(STATE_BYTE_CHARACTER)] = self.character;
        put_i16(&mut state.state_bytes, 0x2C, self.pos_x);
        put_i16(&mut state.state_bytes, 0x2E, self.pos_z);
        put_i16(&mut state.state_bytes, 0x30, self.angle);
        state.state_bytes[usize::from(STATE_BYTE_HEALTH_STATUS)] = self.health_status;
    }

    /// Serialize the block: 0x800 bytes, little-endian, zero-padded.
    pub fn to_bytes(&self) -> [u8; SAVE_BLOCK_SIZE] {
        let mut bytes = [0u8; SAVE_BLOCK_SIZE];
        bytes[..PREFIX_LEN].copy_from_slice(&self.prefix);
        // The whole state image goes down first; the explicit field writes
        // below restate the bytes the port owns and the rest survives verbatim.
        bytes[STATE_IMAGE_OFFSET..STATE_IMAGE_OFFSET + STATE_BYTES]
            .copy_from_slice(&self.state_bytes);
        bytes[0x200] = self.stage;
        bytes[0x201] = self.room;
        bytes[0x202] = self.camera;
        bytes[0x204] = self.cut;
        bytes[0x205] = self.menu_choice;
        bytes[0x206] = self.selected_item;
        bytes[0x207] = self.total_held;
        bytes[0x210] = self.fwd_action;
        bytes[0x211] = self.ent_action;
        bytes[0x212] = self.used_item;
        bytes[0x213] = self.picked_item;
        put_i16(&mut bytes, 0x21E, self.health);
        put_u32(&mut bytes, 0x224, self.timer);
        bytes[0x228] = self.saves;
        bytes[0x229] = self.equipped;
        bytes[0x22A] = self.room_item_backup;
        bytes[0x22B] = self.character;
        put_i16(&mut bytes, 0x22C, self.pos_x);
        put_i16(&mut bytes, 0x22E, self.pos_z);
        put_i16(&mut bytes, 0x230, self.angle);
        bytes[0x232] = self.health_status;
        bytes[0x234..0x254].copy_from_slice(&self.scenario2);
        bytes[0x254..0x25C].copy_from_slice(&self.locks);
        bytes[0x25C..0x27C].copy_from_slice(&self.enemies);
        bytes[0x27C..0x29C].copy_from_slice(&self.room_items);
        bytes[0x29C..0x2A0].copy_from_slice(&self.examined);
        bytes[0x2A0..0x2B0].copy_from_slice(&self.scenario);
        bytes[0x2B0..0x2C4].copy_from_slice(&self.room_flags);
        for (index, slot) in self.item_box.iter().enumerate() {
            bytes[0x2C4 + index * 2] = slot.id;
            bytes[0x2C4 + index * 2 + 1] = slot.quantity;
        }
        for (index, slot) in self.player_slots.iter().enumerate() {
            bytes[0x324 + index * 2] = slot.id;
            bytes[0x324 + index * 2 + 1] = slot.quantity;
        }
        bytes[0x33C..SAVE_LAYOUT_SIZE].copy_from_slice(&self.room_bgm);
        bytes
    }

    /// Parse a save block. The layout needs [`SAVE_LAYOUT_SIZE`] bytes; the
    /// full [`SAVE_BLOCK_SIZE`] block is the usual input and
    /// [`budget::MAX_SAVE_BYTES`] is the largest accepted.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < SAVE_LAYOUT_SIZE {
            bail!(
                "save block is {} bytes; at least {SAVE_LAYOUT_SIZE} are required",
                bytes.len()
            );
        }
        budget::check_len(bytes.len(), budget::MAX_SAVE_BYTES, "save block size")?;
        let mut file = Self {
            prefix: bytes[..PREFIX_LEN].try_into().expect("prefix bytes"),
            state_bytes: bytes[STATE_IMAGE_OFFSET..STATE_IMAGE_OFFSET + STATE_BYTES]
                .try_into()
                .expect("state bytes"),
            stage: bytes[0x200],
            room: bytes[0x201],
            camera: bytes[0x202],
            cut: bytes[0x204],
            menu_choice: bytes[0x205],
            selected_item: bytes[0x206],
            total_held: bytes[0x207],
            fwd_action: bytes[0x210],
            ent_action: bytes[0x211],
            used_item: bytes[0x212],
            picked_item: bytes[0x213],
            health: get_i16(bytes, 0x21E),
            timer: get_u32(bytes, 0x224),
            saves: bytes[0x228],
            equipped: bytes[0x229],
            room_item_backup: bytes[0x22A],
            character: bytes[0x22B],
            pos_x: get_i16(bytes, 0x22C),
            pos_z: get_i16(bytes, 0x22E),
            angle: get_i16(bytes, 0x230),
            health_status: bytes[0x232],
            ..Self::default()
        };
        file.scenario2.copy_from_slice(&bytes[0x234..0x254]);
        file.locks.copy_from_slice(&bytes[0x254..0x25C]);
        file.enemies.copy_from_slice(&bytes[0x25C..0x27C]);
        file.room_items.copy_from_slice(&bytes[0x27C..0x29C]);
        file.examined.copy_from_slice(&bytes[0x29C..0x2A0]);
        file.scenario.copy_from_slice(&bytes[0x2A0..0x2B0]);
        file.room_flags.copy_from_slice(&bytes[0x2B0..0x2C4]);
        for (index, slot) in file.item_box.iter_mut().enumerate() {
            slot.id = bytes[0x2C4 + index * 2];
            slot.quantity = bytes[0x2C4 + index * 2 + 1];
        }
        for (index, slot) in file.player_slots.iter_mut().enumerate() {
            slot.id = bytes[0x324 + index * 2];
            slot.quantity = bytes[0x324 + index * 2 + 1];
        }
        file.room_bgm
            .copy_from_slice(&bytes[0x33C..SAVE_LAYOUT_SIZE]);
        Ok(file)
    }
}

/// The block's 1-based file name for a zero-based slot index.
pub fn slot_name(index: usize) -> Option<String> {
    (index < SAVE_SLOT_COUNT).then(|| format!("savedat{}.dat", index + 1))
}

/// The path of slot `index` in `dir`.
pub fn slot_path(dir: &Path, index: usize) -> Option<PathBuf> {
    slot_name(index).map(|name| dir.join(name))
}

/// The default save directory beside the pack.
pub fn default_save_dir() -> PathBuf {
    PathBuf::from(DEFAULT_SAVE_DIR)
}

/// The default save directory: `saves/` beside `pack`, or `saves/` in the
/// working directory when the pack path has no parent.
pub fn default_save_dir_for_pack(pack: &Path) -> PathBuf {
    match pack.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(DEFAULT_SAVE_DIR),
        _ => PathBuf::from(DEFAULT_SAVE_DIR),
    }
}

/// Header fields the save/load screen shows on a slot row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveSlotInfo {
    /// Character id (`0` Chris, `1` Jill).
    pub character: u8,
    /// Save counter.
    pub saves: u8,
    /// Stage id.
    pub stage: u8,
    /// Room id.
    pub room: u8,
}

impl SaveSlotInfo {
    /// Read the header fields out of a parsed block.
    pub fn from_file(file: &SaveFile) -> Self {
        Self {
            character: file.character,
            saves: file.saves,
            stage: file.stage,
            room: file.room,
        }
    }
}

/// Read a slot file after checking its size against the save cap.
fn read_slot_bytes(path: &Path) -> Result<Vec<u8>> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    budget::check_len_u64(
        metadata.len(),
        budget::MAX_SAVE_BYTES as u64,
        "save slot file size",
    )?;
    fs::read(path).with_context(|| format!("failed to read {}", path.display()))
}

/// Read one slot's header, or `None` when the file is missing or too short to
/// be a save.
pub fn read_slot_info(dir: &Path, index: usize) -> Option<SaveSlotInfo> {
    let path = slot_path(dir, index)?;
    let bytes = read_slot_bytes(&path).ok()?;
    let file = SaveFile::from_bytes(&bytes).ok()?;
    Some(SaveSlotInfo::from_file(&file))
}

/// Scan all eight slots for the load screen.
pub fn scan_slots(dir: &Path) -> [Option<SaveSlotInfo>; SAVE_SLOT_COUNT] {
    std::array::from_fn(|index| read_slot_info(dir, index))
}

/// Write `file` to slot `index` under `dir`, creating the directory.
pub fn save(dir: &Path, index: usize, file: &SaveFile) -> Result<()> {
    let path =
        slot_path(dir, index).with_context(|| format!("save slot {index} is out of range"))?;
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    fs::write(&path, file.to_bytes()).with_context(|| format!("failed to write {}", path.display()))
}

/// Read slot `index` from `dir`.
pub fn load(dir: &Path, index: usize) -> Result<SaveFile> {
    let path =
        slot_path(dir, index).with_context(|| format!("save slot {index} is out of range"))?;
    let bytes = read_slot_bytes(&path)?;
    SaveFile::from_bytes(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn put_i16(buf: &mut [u8], offset: usize, value: i16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn get_i16(buf: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn get_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::state::RoomState;

    /// Self-deleting temporary directory unique to this process and label.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("arklay-save-{}-{label}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn filled() -> SaveFile {
        let mut file = SaveFile::default();
        for (index, byte) in file.prefix.iter_mut().enumerate() {
            *byte = index as u8;
        }
        file.stage = 0x12;
        file.room = 0x34;
        file.camera = 0x56;
        file.cut = 0x78;
        file.menu_choice = 0x9A;
        file.selected_item = 0xBC;
        file.total_held = 0xDE;
        file.fwd_action = 0x11;
        file.ent_action = 0x22;
        file.used_item = 0x33;
        file.picked_item = 0x44;
        file.health = 0x1234;
        file.timer = 0x89AB_CDEF;
        file.saves = 0x5A;
        file.equipped = 0x6B;
        file.room_item_backup = 0x7C;
        file.character = 0x01;
        file.pos_x = 0x0102;
        file.pos_z = 0x0304;
        file.angle = 0x0506;
        file.health_status = 0x08;
        file.scenario2 = [0xA1; 32];
        file.locks = [0xB2; 8];
        file.enemies = [0xC3; 32];
        file.room_items = [0xD4; 32];
        file.examined = [0xE5; 4];
        file.scenario = [0xF6; 16];
        file.room_flags = [0x17; 20];
        file.item_box[0] = InventoryItem {
            id: 0x0B,
            quantity: 0x0F,
        };
        file.item_box[47] = InventoryItem {
            id: 0x42,
            quantity: 1,
        };
        file.player_slots[0] = InventoryItem {
            id: 0x2F,
            quantity: 3,
        };
        file.player_slots[11] = InventoryItem {
            id: 0x41,
            quantity: 1,
        };
        for (index, byte) in file.room_bgm.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(3);
        }
        file.sync_state_image();
        file
    }

    #[test]
    fn every_field_lands_at_its_table_offset() {
        let file = filled();
        let bytes = file.to_bytes();

        assert_eq!(&bytes[..PREFIX_LEN], &file.prefix);
        assert_eq!(bytes[0x200], 0x12);
        assert_eq!(bytes[0x201], 0x34);
        assert_eq!(bytes[0x202], 0x56);
        assert_eq!(bytes[0x203], 0x00, "the attract camera stays zeroed");
        assert_eq!(bytes[0x204], 0x78);
        assert_eq!(bytes[0x205], 0x9A);
        assert_eq!(bytes[0x206], 0xBC);
        assert_eq!(bytes[0x207], 0xDE);
        assert_eq!(bytes[0x210], 0x11);
        assert_eq!(bytes[0x211], 0x22);
        assert_eq!(bytes[0x212], 0x33);
        assert_eq!(bytes[0x213], 0x44);
        assert_eq!(&bytes[0x21E..0x220], &0x1234i16.to_le_bytes());
        assert_eq!(&bytes[0x224..0x228], &0x89AB_CDEFu32.to_le_bytes());
        assert_eq!(bytes[0x228], 0x5A);
        assert_eq!(bytes[0x229], 0x6B);
        assert_eq!(bytes[0x22A], 0x7C);
        assert_eq!(bytes[0x22B], 0x01);
        assert_eq!(&bytes[0x22C..0x22E], &0x0102i16.to_le_bytes());
        assert_eq!(&bytes[0x22E..0x230], &0x0304i16.to_le_bytes());
        assert_eq!(&bytes[0x230..0x232], &0x0506i16.to_le_bytes());
        assert_eq!(bytes[0x232], 0x08);
        assert_eq!(&bytes[0x234..0x254], &[0xA1; 32]);
        assert_eq!(&bytes[0x254..0x25C], &[0xB2; 8]);
        assert_eq!(&bytes[0x25C..0x27C], &[0xC3; 32]);
        assert_eq!(&bytes[0x27C..0x29C], &[0xD4; 32]);
        assert_eq!(&bytes[0x29C..0x2A0], &[0xE5; 4]);
        assert_eq!(&bytes[0x2A0..0x2B0], &[0xF6; 16]);
        assert_eq!(&bytes[0x2B0..0x2C4], &[0x17; 20]);
        assert_eq!(bytes[0x2C4], 0x0B);
        assert_eq!(bytes[0x2C5], 0x0F);
        assert_eq!(bytes[0x2C4 + 47 * 2], 0x42);
        assert_eq!(bytes[0x2C4 + 47 * 2 + 1], 0x01);
        assert_eq!(bytes[0x324], 0x2F);
        assert_eq!(bytes[0x325], 3);
        assert_eq!(bytes[0x324 + 11 * 2], 0x41);
        assert_eq!(&bytes[0x33C..SAVE_LAYOUT_SIZE], &file.room_bgm);
        assert!(
            bytes[SAVE_LAYOUT_SIZE..].iter().all(|&byte| byte == 0),
            "the tail must stay zero-padded"
        );
    }

    #[test]
    fn bytes_round_trip() {
        let file = filled();
        let bytes = file.to_bytes();
        assert_eq!(SaveFile::from_bytes(&bytes).unwrap(), file);
    }

    #[test]
    fn state_image_offsets_match_the_bio_card_layout() {
        assert_eq!(STATE_IMAGE_OFFSET, 0x200);
        assert_eq!(STATE_IMAGE_OFFSET + 0x0C, 0x20C, "pickup quantity A");
        assert_eq!(STATE_IMAGE_OFFSET + 0x0E, 0x20E, "pickup quantity C");
        assert_eq!(STATE_IMAGE_OFFSET + 0x14, 0x214, "first dropped word");
        assert_eq!(STATE_IMAGE_OFFSET + 0x23, 0x223, "last dropped word");
        assert_eq!(STATE_IMAGE_OFFSET + STATE_BYTES, 0x240);
    }

    #[test]
    fn state_image_round_trips_the_dropped_bio_card_bytes() {
        let mut state = GameState::new(RoomId::parse("11C1").unwrap(), &RoomState::default());
        // 0x208..0x20F: special-room light, character model id, last enemy
        // flags, bullet effect id and the three pickup quantities.
        state.state_bytes[0x08..0x10].copy_from_slice(&[0x21, 0x02, 0x43, 0x04, 0x05, 7, 240, 240]);
        // 0x214..0x21D and 0x220..0x223: fade state, light, rand seed,
        // countdown and the held/pressed pad words (0x21E is the health copy,
        // owned by the explicit field).
        for (index, byte) in state.state_bytes[0x14..0x1E].iter_mut().enumerate() {
            *byte = 0x30 + index as u8;
        }
        for (index, byte) in state.state_bytes[0x20..0x24].iter_mut().enumerate() {
            *byte = 0x50 + index as u8;
        }
        state.entities[0].health = 0x1234;
        state.state_bytes[0x2A] = 0x5C;

        let file = SaveFile::from_state(&state);
        assert_eq!(
            &file.state_bytes[0x08..0x10],
            &state.state_bytes[0x08..0x10]
        );
        let bytes = file.to_bytes();
        assert_eq!(
            &bytes[0x208..0x210],
            &state.state_bytes[0x08..0x10],
            "the prefix rides the block verbatim"
        );
        assert_eq!(&bytes[0x214..0x21E], &state.state_bytes[0x14..0x1E]);
        assert_eq!(&bytes[0x220..0x224], &state.state_bytes[0x20..0x24]);
        assert_eq!(
            &bytes[0x21E..0x220],
            &0x1234i16.to_le_bytes(),
            "health copy"
        );
        assert_eq!(bytes[0x22A], 0x5C, "room-item backup");

        let parsed = SaveFile::from_bytes(&bytes).unwrap();
        let mut restored = GameState::default();
        parsed.apply_to(&mut restored);
        assert_eq!(
            &restored.state_bytes[0x08..0x10],
            &state.state_bytes[0x08..0x10]
        );
        assert_eq!(
            &restored.state_bytes[0x14..0x1E],
            &state.state_bytes[0x14..0x1E]
        );
        assert_eq!(
            &restored.state_bytes[0x20..0x24],
            &state.state_bytes[0x20..0x24]
        );
        assert_eq!(restored.state_bytes[0x2A], 0x5C);
        assert_eq!(SaveFile::from_state(&restored), file);
    }

    #[test]
    fn old_slots_load_every_state_image_byte_as_written() {
        // A pre-M15 writer never touched 0x208..0x20F and 0x214..0x223, so an
        // old slot loads them zero instead of failing.
        let file = filled();
        let mut bytes = file.to_bytes();
        bytes[0x208..0x210].fill(0);
        bytes[0x214..0x21E].fill(0);
        bytes[0x220..0x224].fill(0);
        let parsed = SaveFile::from_bytes(&bytes).unwrap();
        assert!(parsed.state_bytes[0x08..0x10].iter().all(|&byte| byte == 0));
        assert!(parsed.state_bytes[0x14..0x1E].iter().all(|&byte| byte == 0));
        assert!(parsed.state_bytes[0x20..0x24].iter().all(|&byte| byte == 0));
        assert_eq!(parsed.prefix, file.prefix);
    }

    #[test]
    fn from_bytes_rejects_a_short_block() {
        let bytes = filled().to_bytes();
        assert!(SaveFile::from_bytes(&bytes[..SAVE_LAYOUT_SIZE - 1]).is_err());
        assert!(SaveFile::from_bytes(&[]).is_err());
        assert!(SaveFile::from_bytes(&bytes).is_ok());
    }

    #[test]
    fn from_bytes_rejects_a_block_over_the_cap() {
        let bytes = vec![0u8; budget::MAX_SAVE_BYTES + 1];
        let message = budget::assert_cap_error(SaveFile::from_bytes(&bytes));
        assert!(message.contains("save block size"), "{message}");
    }

    #[test]
    fn state_round_trips_through_the_block() {
        let mut state = GameState::new(RoomId::parse("11C1").unwrap(), &RoomState::default());
        state.entities[0].health = 96;
        state.entities[0].pos = [1234, 7, -2345];
        state.entities[0].angle = 0x456;
        // Jill's derived maximum; the block does not carry it.
        state.max_health = 96;
        state.health_status = 0x20;
        state.camera.current_cut = 3;
        state.state_bytes[0x24..0x28].copy_from_slice(&123456u32.to_le_bytes());
        state.state_bytes[usize::from(STATE_BYTE_SAVES)] = 4;
        state.state_bytes[usize::from(STATE_BYTE_CUT)] = 1;
        state.state_bytes[usize::from(STATE_BYTE_MENU_CHOICE)] = 1;
        state.state_bytes[usize::from(STATE_BYTE_FWD_ACTION)] = 5;
        state.state_bytes[usize::from(STATE_BYTE_ENT_ACTION)] = 6;
        state.add_item(0x0B, 15);
        state.add_item(0x2F, 1);
        state.select_item(Some(0x0B));
        state.record_used_item(0x41);
        state.set_equipped(Some(0x02));
        state.mark_examined(0x33);
        state.item_box[3] = InventoryItem {
            id: 0x41,
            quantity: 1,
        };
        for bank in [0u8, 1, 2, 3, 7, 8] {
            state.apply_flag(bank, 0, 0);
        }

        let file = SaveFile::from_state(&state);
        assert_eq!(file.stage, 1);
        assert_eq!(file.room, 0x1C);
        assert_eq!(file.character, 1);
        assert_eq!(file.health, 96);
        assert_eq!(file.timer, 123456);
        assert_eq!(file.saves, 4);
        assert_eq!(file.pos_x, 1234);
        assert_eq!(file.pos_z, -2345);
        assert_eq!(file.angle, 0x456);
        assert_eq!(file.health_status, 0x20);
        assert_eq!(file.total_held, 2);
        assert_eq!(file.selected_item, 0x0B);
        assert_eq!(file.used_item, 0x41);
        assert_eq!(file.equipped, 0x02);
        assert_eq!(file.item_box[3].id, 0x41);
        // The examined bank comes from the item-name lookup's own store (item
        // 0x33's class-3 bit) and round-trips at BioCard 0x29C.
        assert_eq!(file.examined[3] & 0x10, 0x10);
        assert_eq!(file.to_bytes()[0x29F] & 0x10, 0x10);

        let parsed = SaveFile::from_bytes(&file.to_bytes()).unwrap();
        let mut restored = GameState::default();
        parsed.apply_to(&mut restored);

        assert_eq!(restored.id, state.id);
        // The maximum is not in the block; apply_to re-derives it from the
        // saved character exactly as InitializeGame does (Jill here).
        assert_eq!(restored.max_health, 96);
        assert_eq!(restored.entities[0].health, 96);
        assert_eq!(restored.entities[0].pos[0], 1234);
        assert_eq!(restored.entities[0].pos[2], -2345);
        assert_eq!(restored.entities[0].angle, 0x456);
        assert_eq!(restored.health_status, 0x20);
        assert_eq!(restored.examined_flags(), state.examined_flags());
        assert_eq!(restored.inventory, state.inventory);
        assert_eq!(restored.item_box, state.item_box);
        assert_eq!(restored.selected_item, Some(0x0B));
        assert_eq!(restored.last_used_item, Some(0x41));
        assert_eq!(restored.equipped, Some(0x02));
        assert_eq!(restored.state_bytes[usize::from(STATE_BYTE_SAVES)], 4);
        for bank in 0..FLAG_BANK_COUNT {
            assert_eq!(
                restored.flags[bank].bytes(),
                state.flags[bank].bytes(),
                "bank {bank}"
            );
        }
        assert_eq!(SaveFile::from_state(&restored), file);
    }

    #[test]
    fn room_bgm_table_round_trips_the_live_table() {
        let mut state = GameState::new(RoomId::parse("1000").unwrap(), &RoomState::default());
        // A script's `room_bgm_state_set` write must survive the block, not the shipped
        // constant.
        state.room_bgm[7] = 0x40;
        let file = SaveFile::from_state(&state);
        assert_eq!(file.room_bgm[7], 0x40);

        let parsed = SaveFile::from_bytes(&file.to_bytes()).unwrap();
        let mut restored = GameState::default();
        parsed.apply_to(&mut restored);
        assert_eq!(restored.room_bgm, state.room_bgm);
        // The live state byte is not in the block; it resets so the room entry
        // rebuilds the channels from the table.
        assert_eq!(restored.bgm.state, 0xFF);
    }

    #[test]
    fn slot_names_and_paths_are_one_based() {
        assert_eq!(slot_name(0).unwrap(), "savedat1.dat");
        assert_eq!(slot_name(7).unwrap(), "savedat8.dat");
        assert!(slot_name(8).is_none());
        assert_eq!(
            slot_path(Path::new("saves"), 2).unwrap(),
            Path::new("saves").join("savedat3.dat")
        );
        assert_eq!(default_save_dir(), PathBuf::from("saves"));
        assert_eq!(
            default_save_dir_for_pack(Path::new("/games/re1.akpak")),
            Path::new("/games/saves")
        );
        assert_eq!(
            default_save_dir_for_pack(Path::new("re1.akpak")),
            Path::new("saves")
        );
    }

    #[test]
    fn directory_io_scans_and_reloads_slots() {
        let dir = TempDir::new("io");
        let file = filled();
        save(&dir.path, 0, &file).unwrap();
        save(&dir.path, 4, &file).unwrap();

        let scan = scan_slots(&dir.path);
        let info = SaveSlotInfo::from_file(&file);
        assert_eq!(scan[0], Some(info));
        assert_eq!(scan[4], Some(info));
        assert_eq!(scan[1], None);
        assert_eq!(scan[7], None);

        let loaded = load(&dir.path, 0).unwrap();
        assert_eq!(loaded, file);
        assert_eq!(
            fs::read(dir.path.join("savedat1.dat")).unwrap(),
            file.to_bytes()
        );

        // A slot past the range is an error, not a panic.
        assert!(load(&dir.path, 8).is_err());
        assert!(save(&dir.path, 8, &file).is_err());
    }

    #[test]
    fn a_truncated_slot_reads_as_empty() {
        let dir = TempDir::new("truncated");
        fs::write(dir.path.join("savedat2.dat"), [0u8; 8]).unwrap();
        assert_eq!(read_slot_info(&dir.path, 1), None);
        assert!(load(&dir.path, 1).is_err());
    }

    #[test]
    fn prefix_must_be_at_least_the_prefix_length() {
        let mut file = SaveFile::default();
        assert!(file.set_prefix(&[0u8; PREFIX_LEN]).is_ok());
        assert!(file.set_prefix(&[0u8; 8]).is_err());
        assert_eq!(
            SaveFile::from_state_with_prefix(&GameState::default(), &[0xAA; SAVE_LAYOUT_SIZE])
                .unwrap()
                .prefix,
            [0xAA; PREFIX_LEN]
        );
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn real_prefix_writes_and_reloads_through_a_directory() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let card =
            fs::read(Path::new(&root).join("JPN/DATA/bio_card.dat")).expect("read bio_card.dat");
        let mut state = GameState::new(RoomId::parse("1001").unwrap(), &RoomState::default());
        state.entities[0].health = 96;
        state.add_item(0x0B, 15);
        let file = SaveFile::from_state_with_prefix(&state, &card).unwrap();
        assert_eq!(&file.prefix[..], &card[..PREFIX_LEN]);
        assert_eq!(&file.to_bytes()[..PREFIX_LEN], &card[..PREFIX_LEN]);

        let dir = TempDir::new("real-io");
        save(&dir.path, 0, &file).unwrap();
        let scan = scan_slots(&dir.path);
        assert_eq!(scan[0], Some(SaveSlotInfo::from_file(&file)));
        let loaded = load(&dir.path, 0).unwrap();
        assert_eq!(loaded, file);
        let mut restored = GameState::default();
        loaded.apply_to(&mut restored);
        assert_eq!(restored.entities[0].health, 96);
        assert_eq!(restored.item_count(0x0B), 15);
    }
}
