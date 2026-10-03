//! Game state driven by the room's SCD scripts.
//!
//! The command and event VMs dispatch every observable effect to
//! [`ScdGameHost`], which owns this milestone's state: the flag banks and
//! BioCard-like state block that conditions read, the camera cut and lock, the
//! active message, and the room's BGM requests. Doors, items, entities and
//! effects stay recorded placeholders until their systems exist.

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
    /// Fixed ticks elapsed.
    pub frame: u64,
    /// Call counts of opcodes whose systems do not exist yet.
    pub placeholders: BTreeMap<u8, u64>,
    /// BGM requests produced by the scripts.
    pub room_bgm_requests: Vec<BgmRequest>,
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
            frame: 0,
            placeholders: BTreeMap::new(),
            room_bgm_requests: Vec::new(),
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

    fn placeholder(&mut self, op: &Op) -> StepResult {
        self.state.record_placeholder(op.op);
        StepResult::Placeholder
    }
}

impl ScdHost for ScdGameHost<'_> {
    fn on_flow(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        match op.op {
            0x0E | 0x32 => StepResult::Continue,
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

    fn on_room_action(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        self.placeholder(op)
    }

    fn on_item(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        self.placeholder(op)
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

    fn on_misc(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
        self.placeholder(op)
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
                host.on_room_action(op(0x0C), &operands(&[0])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_room_action(op(0x0C), &operands(&[1])),
                StepResult::Placeholder
            );
            assert_eq!(
                host.on_item(op(0x18), &operands(&[0])),
                StepResult::Placeholder
            );
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
                host.on_flow(op(0x10), &operands(&[0])),
                StepResult::Placeholder
            );
        }
        assert_eq!(state.placeholders.len(), 11);
        assert_eq!(state.placeholders[&0x0C], 2);
        assert_eq!(state.placeholders[&0x18], 1);
        assert_eq!(state.placeholders[&0x1B], 1);
        assert_eq!(state.placeholders[&0x20], 1);
        assert_eq!(state.placeholders[&0x1F], 1);
        assert_eq!(state.placeholders[&0x2A], 1);
        assert_eq!(state.placeholders[&0x25], 1);
        assert_eq!(state.placeholders[&0x29], 1);
        assert_eq!(state.placeholders[&0x40], 1);
        assert_eq!(state.placeholders[&0x27], 1);
        assert_eq!(state.placeholders[&0x10], 1);
    }
}
