//! Host interface for the SCD virtual machines.
//!
//! The VMs own decoding, flow control and per-slot state; every observable
//! game effect is a method on [`ScdHost`]. The defaults record a placeholder
//! so scripts can run end to end before the game systems exist.

use crate::scd::ir::Operand;
use crate::scd::opcode::Op;

/// Result of one VM step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepResult {
    /// The instruction finished; keep running the stream.
    Continue,
    /// Stop this stream for the frame and resume it later.
    Yield,
    /// The stream or condition finished.
    Finished,
    /// No game system is implemented for this instruction yet.
    Placeholder,
}

/// Host interface: every game effect is a method with a default that records a
/// placeholder. M4 fills these in without touching the decoder.
pub trait ScdHost {
    fn on_flow(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_flags(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_camera(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_message(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_room_action(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_item(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_enemy(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_player(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_model(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_effect(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_sound(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    fn on_misc(&mut self, _op: &Op, _operands: &[Operand]) -> StepResult {
        StepResult::Placeholder
    }

    /// Bit test used by the condition opcodes (bank, bit, expected).
    fn flag_test(&mut self, _bank: u8, _bit: u8, _expected: bool) -> bool {
        false
    }
}

/// A host with only the placeholder defaults.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlaceholderHost;

impl ScdHost for PlaceholderHost {}
