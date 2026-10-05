//! RE1 SCD opcode tables.
//!
//! Three separate opcode spaces exist: the command VM (0x00-0x50), the event
//! VM state-0 table (0x00-0x09) with the shared control ops (0xF6-0xFF), and
//! the two event sub-ISAs entered from state 0: actor (state 1) and tween
//! (state 2). Operand signatures use one character per operand:
//!
//! - `u` u8, `U` u16, `I` i16, `b` i8
//! - `l` command jump label (`if`/`else`), `j` event jump label (`evt_do`)
//! - `p` payload length (`evt_block`/`evt_single`)
//! - `r` raw remainder, not decoded as operands

/// One opcode table row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Op {
    /// The opcode byte.
    pub op: u8,
    /// Display mnemonic.
    pub mnemonic: &'static str,
    /// Operand signature, one character per operand.
    pub operands: &'static str,
    /// Command-VM opcode that ends a straight-line run when it returns 0.
    pub condition: bool,
    /// Length in bytes including the opcode; `None` means the width depends on
    /// the operand bytes (0x17, 0x28, 0x33, `evt_block`, `evt_single`,
    /// `evt_do`, `act_motion`) and must be computed by the reader.
    pub width: Option<usize>,
}

const fn op(
    op: u8,
    mnemonic: &'static str,
    operands: &'static str,
    condition: bool,
    width: Option<usize>,
) -> Op {
    Op {
        op,
        mnemonic,
        operands,
        condition,
        width,
    }
}

/// The command-VM opcode table, indexed by opcode (0x00-0x50).
///
/// 0x26 and 0x2E are dead slots: the original handler returns without
/// consuming anything, so the interpreter spins. They are kept as zero-width
/// "hang" rows so the table stays a direct index.
pub const COMMAND_OPS: [Op; 81] = [
    op(0x00, "end", "u", false, Some(2)),
    op(0x01, "if", "l", false, Some(2)),
    op(0x02, "else", "l", false, Some(2)),
    op(0x03, "endif", "u", false, Some(2)),
    op(0x04, "ck", "uuu", true, Some(4)),
    op(0x05, "set", "uuu", false, Some(4)),
    op(0x06, "cmpb", "uuu", true, Some(4)),
    op(0x07, "cmpw", "uuuI", true, Some(6)),
    op(0x08, "setb", "uuu", false, Some(4)),
    op(0x09, "cutnext", "u", false, Some(2)),
    op(0x0A, "cutcurr", "u", false, Some(2)),
    op(0x0B, "message", "uU", false, Some(4)),
    op(0x0C, "door_aot_set", "uIIIIuuuuuuIIIIuu", false, Some(26)),
    op(0x0D, "aot_set", "uIIIIuuUUU", false, Some(18)),
    op(0x0E, "nop", "u", false, Some(2)),
    op(0x0F, "scene_setup", "uUUU", false, Some(8)),
    op(0x10, "testitem", "u", true, Some(2)),
    op(0x11, "testpickup", "u", true, Some(2)),
    op(0x12, "aot_reset", "uuuUUU", false, Some(10)),
    op(0x13, "aot_delete", "uuu", false, Some(4)),
    op(0x14, "evt_exec", "uuu", false, Some(4)),
    op(0x15, "bgm_play", "u", false, Some(2)),
    op(0x16, "bgm_stop", "u", false, Some(2)),
    op(0x17, "se_play_3d", "uubuuII", false, None),
    op(0x18, "item_aot_set", "uIIIIuuuuIIIUuuU", false, Some(26)),
    op(0x19, "model_flag_set", "uuu", false, Some(4)),
    op(0x1A, "item_ck", "u", true, Some(2)),
    op(0x1B, "enemy", "uuuuuIUUUIUuuuu", false, Some(22)),
    op(0x1C, "timer_setup", "uIU", false, Some(6)),
    op(0x1D, "ck_last_item", "u", true, Some(2)),
    op(0x1E, "voice_play", "uU", false, Some(4)),
    op(0x1F, "obj", "uuuIIIUuuuuuuuuuuuuuuuu", false, Some(28)),
    op(0x20, "dir_set", "uIIIIII", false, Some(14)),
    op(0x21, "pos_set", "bIIIIII", false, Some(14)),
    op(0x22, "ck_item_count", "uuu", true, Some(4)),
    op(0x23, "cut_auto", "u", false, Some(2)),
    op(0x24, "aot_on", "uuu", false, Some(4)),
    op(0x25, "aot_switch", "uuu", false, Some(4)),
    op(0x26, "hang", "", false, Some(0)),
    op(0x27, "snd_fade_set", "u", false, Some(2)),
    op(0x28, "eml_state", "uuuUu", false, None),
    op(0x29, "movie_on", "u", false, Some(2)),
    op(0x2A, "effect", "uuuIIIU", false, Some(12)),
    op(0x2B, "plw_anim", "uuu", false, Some(4)),
    op(0x2C, "remove_item", "u", false, Some(2)),
    op(0x2D, "give_item", "uuu", false, Some(4)),
    op(0x2E, "hang", "", false, Some(0)),
    op(0x2F, "snd_pan_vol_set", "uuu", false, Some(4)),
    op(0x30, "inst_cfg", "uuuUUUU", false, Some(12)),
    op(0x31, "setw", "uU", false, Some(4)),
    op(0x32, "nop_wide", "uuu", false, Some(4)),
    op(0x33, "sys_multi", "uU", false, None),
    op(0x34, "model_op", "ubuuuuu", false, Some(8)),
    op(0x35, "objtbl_b_set", "uuu", false, Some(4)),
    op(0x36, "ck_anim", "uuu", true, Some(4)),
    op(0x37, "tbl37_set", "uuu", false, Some(4)),
    op(0x38, "ck_bits", "uU", true, Some(4)),
    op(0x39, "get_eml_state", "u", false, Some(2)),
    op(0x3A, "msgnode_set", "uuu", false, Some(4)),
    op(0x3B, "eml_rot", "uUU", false, Some(6)),
    op(0x3C, "ck_counter", "uUU", true, Some(6)),
    op(0x3D, "effect_tracked", "uuuUUUU", false, Some(12)),
    op(0x3E, "effect_kill_a", "u", false, Some(2)),
    op(0x3F, "ck_tween", "uUU", true, Some(6)),
    op(0x40, "obj_xfm", "uIIIIIII", false, Some(16)),
    op(0x41, "spd_set", "uU", false, Some(4)),
    op(0x42, "effect_kill_b", "uU", false, Some(4)),
    op(0x43, "bgm_volume_ramp", "ubu", false, Some(4)),
    op(0x44, "task_kill", "u", false, Some(2)),
    op(0x45, "spd_add", "b", false, Some(2)),
    op(
        0x46,
        "msg_list",
        "uIIIuuuuIIIIuuuuIIIIuuuuIIII",
        false,
        Some(44),
    ),
    op(0x47, "eml_pos", "uIIIIII", false, Some(14)),
    op(0x48, "effect_clear", "u", false, Some(2)),
    op(0x49, "bank_bit", "u", false, Some(2)),
    op(0x4A, "bgm_restore", "u", false, Some(2)),
    op(0x4B, "bgm_stop_all", "u", false, Some(2)),
    op(0x4C, "swap_var", "uuu", false, Some(4)),
    op(0x4D, "objs_hide", "u", false, Some(2)),
    op(0x4E, "mass_mask", "uU", false, Some(4)),
    op(0x4F, "costume_set", "u", false, Some(2)),
    op(0x50, "costume_ck", "u", true, Some(2)),
];

/// Event-VM state-0 opcodes (0x00-0x09), indexed by opcode.
pub const EVENT_TOP_OPS: &[Op] = &[
    op(0x00, "evt_nop", "", false, Some(1)),
    op(0x01, "evt_actor_begin", "", false, Some(1)),
    op(0x02, "evt_tween_begin", "", false, Some(1)),
    op(0x03, "evt_tween_begin_alt", "", false, Some(1)),
    op(0x04, "evt_work_set", "uu", false, Some(3)),
    op(0x05, "evt_fork", "uuu", false, Some(4)),
    op(0x06, "evt_block", "p", false, None),
    op(0x07, "evt_single", "p", false, None),
    op(0x08, "evt_chain", "u", false, Some(2)),
    op(0x09, "evt_disable", "u", false, Some(2)),
];

/// Event-VM control opcodes (0xF6-0xFF), indexed from 0xF6.
pub const EVENT_CONTROL_OPS: &[Op] = &[
    op(0xF6, "evt_push_cond", "", false, Some(1)),
    op(0xF7, "evt_skip_if", "", false, Some(1)),
    op(0xF8, "evt_sleep", "uU", false, Some(4)),
    op(0xF9, "evt_sleep_tick", "", false, Some(1)),
    op(0xFA, "evt_for", "uU", false, Some(4)),
    op(0xFB, "evt_fornext", "", false, Some(1)),
    op(0xFC, "evt_do", "j", false, Some(2)),
    op(0xFD, "evt_dountil", "", false, Some(1)),
    op(0xFE, "evt_next", "", false, Some(1)),
    op(0xFF, "evt_finish", "", false, Some(1)),
];

/// Event-VM state-1 (actor) opcodes, indexed as 0x00 followed by 0x80-0x8B.
pub const ACTOR_OPS: &[Op] = &[
    op(0x00, "act_nop", "", false, Some(1)),
    op(0x80, "act_reset", "", false, Some(1)),
    op(0x81, "act_motion", "uUUUuu", false, None),
    op(0x82, "act_motion_bitclr", "", false, Some(1)),
    op(0x83, "act_anim_seq", "UUU", false, Some(7)),
    op(0x84, "act_anim_flags", "bbb", false, Some(4)),
    op(0x85, "act_anim_set", "bbb", false, Some(4)),
    op(0x86, "act_idle", "", false, Some(1)),
    op(0x87, "act_flag_op", "uU", false, Some(4)),
    op(0x88, "act_param_set", "Uu", false, Some(4)),
    op(0x89, "act_action_a", "u", false, Some(2)),
    op(0x8A, "act_action_b", "u", false, Some(2)),
    op(0x8B, "act_end", "", false, Some(1)),
];

/// Event-VM state-2 (tween) opcodes (0x00-0x0B), indexed by opcode.
pub const TWEEN_OPS: &[Op] = &[
    op(0x00, "tw_nop", "", false, Some(1)),
    op(0x01, "tw_end", "", false, Some(1)),
    op(0x02, "tw_pos_add", "", false, Some(1)),
    op(0x03, "tw_rot_add", "", false, Some(1)),
    op(0x04, "tw_pos_rot_add", "", false, Some(1)),
    op(0x05, "tw_set_pos", "uuu", false, Some(4)),
    op(0x06, "tw_set_rot", "uuu", false, Some(4)),
    op(0x07, "tw_abs_pos", "UUUu", false, Some(8)),
    op(0x08, "tw_set_field", "ub", false, Some(3)),
    op(0x09, "tw_set_8a", "u", false, Some(2)),
    op(0x0A, "tw_set_sel", "uuu", false, Some(4)),
    op(0x0B, "tw_abs_rot", "UUUu", false, Some(8)),
];

/// Look up a command-VM opcode.
pub fn command_op(op: u8) -> Option<&'static Op> {
    COMMAND_OPS.get(usize::from(op))
}

/// Look up an event-VM state-0 opcode.
pub fn event_top_op(op: u8) -> Option<&'static Op> {
    EVENT_TOP_OPS.get(usize::from(op))
}

/// Look up an event-VM control opcode (0xF6-0xFF).
pub fn event_control_op(op: u8) -> Option<&'static Op> {
    op.checked_sub(0xF6)
        .and_then(|index| EVENT_CONTROL_OPS.get(usize::from(index)))
}

/// Look up an event-VM state-1 (actor) opcode.
pub fn actor_op(op: u8) -> Option<&'static Op> {
    match op {
        0x00 => ACTOR_OPS.first(),
        0x80..=0x8B => ACTOR_OPS.get(usize::from(op - 0x80) + 1),
        _ => None,
    }
}

/// Look up an event-VM state-2 (tween) opcode.
pub fn tween_op(op: u8) -> Option<&'static Op> {
    TWEEN_OPS.get(usize::from(op))
}

/// Width in bytes including the opcode, applying the dynamic rules for the
/// variable-width command opcodes 0x17, 0x28 and 0x33.
///
/// Returns `None` when the deciding operand byte is not present in `bytes`.
/// An unknown 0x28/0x33 sub-command returns `Some(0)`: the original handler
/// consumes nothing and the interpreter spins.
pub fn command_width(bytes: &[u8]) -> Option<usize> {
    let entry = command_op(*bytes.first()?)?;
    match entry.op {
        0x17 => {
            let pos_type = *bytes.get(4)?;
            Some(if pos_type <= 3 { 10 } else { 6 })
        }
        0x28 => match *bytes.get(3)? {
            0 | 2 | 3 | 5 | 9 | 10 => Some(6),
            1 => Some(8),
            6 | 8 => Some(4),
            _ => Some(0),
        },
        0x33 => match *bytes.get(1)? {
            1 | 3 | 5 | 8 | 9 | 10 => Some(4),
            0 | 4 | 6 | 7 => Some(2),
            _ => Some(0),
        },
        _ => entry.width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operand_bytes(signature: &str) -> usize {
        signature
            .bytes()
            .map(|byte| match byte {
                b'U' | b'I' => 2,
                _ => 1,
            })
            .sum()
    }

    #[test]
    fn command_table_indexes_every_opcode() {
        assert_eq!(COMMAND_OPS.len(), 81);
        for (index, entry) in COMMAND_OPS.iter().enumerate() {
            assert_eq!(usize::from(entry.op), index);
            assert_eq!(command_op(entry.op), Some(entry));
        }
        assert_eq!(command_op(0x51), None);
    }

    #[test]
    fn fixed_width_matches_operand_signature() {
        for entry in &COMMAND_OPS {
            let Some(width) = entry.width else {
                continue;
            };
            if width == 0 {
                assert_eq!(entry.operands, "");
                continue;
            }
            assert_eq!(
                operand_bytes(entry.operands) + 1,
                width,
                "opcode {:#04X} ({})",
                entry.op,
                entry.mnemonic
            );
        }
    }

    #[test]
    fn condition_set_is_exactly_the_thirteen_handlers() {
        let conditions: Vec<u8> = COMMAND_OPS
            .iter()
            .filter(|entry| entry.condition)
            .map(|entry| entry.op)
            .collect();
        assert_eq!(
            conditions,
            vec![
                0x04, 0x06, 0x07, 0x10, 0x11, 0x1A, 0x1D, 0x22, 0x36, 0x38, 0x3C, 0x3F, 0x50
            ]
        );
    }

    #[test]
    fn dynamic_command_widths() {
        assert_eq!(command_width(&[0x17, 0, 0, 0, 3]), Some(10));
        assert_eq!(command_width(&[0x17, 0, 0, 0, 4]), Some(6));
        assert_eq!(command_width(&[0x17, 0, 0, 0]), None);

        assert_eq!(command_width(&[0x28, 0, 0, 0]), Some(6));
        assert_eq!(command_width(&[0x28, 0, 0, 1]), Some(8));
        assert_eq!(command_width(&[0x28, 0, 0, 6]), Some(4));
        assert_eq!(command_width(&[0x28, 0, 0, 8]), Some(4));
        assert_eq!(command_width(&[0x28, 0, 0, 4]), Some(0));

        assert_eq!(command_width(&[0x33, 0]), Some(2));
        assert_eq!(command_width(&[0x33, 3]), Some(4));
        assert_eq!(command_width(&[0x33, 10]), Some(4));
        assert_eq!(command_width(&[0x33, 2]), Some(0));

        assert_eq!(command_width(&[0x26]), Some(0));
        assert_eq!(command_width(&[0x2E]), Some(0));
        assert_eq!(command_width(&[0x00]), Some(2));
        assert_eq!(command_width(&[0xFF]), None);
    }

    #[test]
    fn event_tables_lookup_by_opcode() {
        assert_eq!(event_top_op(0x00).unwrap().mnemonic, "evt_nop");
        assert_eq!(event_top_op(0x09).unwrap().mnemonic, "evt_disable");
        assert_eq!(event_top_op(0x0A), None);

        assert_eq!(event_control_op(0xF6).unwrap().mnemonic, "evt_push_cond");
        assert_eq!(event_control_op(0xFF).unwrap().mnemonic, "evt_finish");
        assert_eq!(event_control_op(0xF5), None);

        assert_eq!(actor_op(0x00).unwrap().mnemonic, "act_nop");
        assert_eq!(actor_op(0x80).unwrap().mnemonic, "act_reset");
        assert_eq!(actor_op(0x8B).unwrap().mnemonic, "act_end");
        assert_eq!(actor_op(0x01), None);
        assert_eq!(actor_op(0x8C), None);

        assert_eq!(tween_op(0x00).unwrap().mnemonic, "tw_nop");
        assert_eq!(tween_op(0x0B).unwrap().mnemonic, "tw_abs_rot");
        assert_eq!(tween_op(0x0C), None);
    }
}
