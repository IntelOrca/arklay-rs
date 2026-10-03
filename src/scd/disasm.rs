//! SCD disassembler: renders `.s` assembly and `.lst` listings.
//!
//! Column layout (0-based character indices), matching the original tool:
//!
//! - `.s`: mnemonic at column 4, arguments at column 28.
//! - `.lst`: `%04X:` offset, hex bytes at column 8, mnemonic at column 96,
//!   arguments at column 120.
//!
//! Labels are `off_XXXX` (uppercase hex of the absolute RDT offset) and are
//! emitted on their own line only where referenced: command `if` labels are
//! two bytes before the VM resume target stored in the IR operand, `else` and
//! `evt_do` labels sit at their target. A reference into the middle of an
//! instruction is redirected to that instruction's start plus ` + delta`.
//! Undecoded trailing bytes render as `db 0xAA, 0xBB` lines, 16 per line.

use std::collections::{BTreeMap, BTreeSet};

use crate::scd::decomp::constants;
use crate::scd::ir::{Decoded, Insn, Scripts, StreamKind};

/// Column where `.s` mnemonics start.
const ASM_MNEMONIC_COLUMN: usize = 4;
/// Column where `.s` arguments start.
const ASM_ARGS_COLUMN: usize = 28;
/// Column where `.lst` hex bytes start.
const LST_BYTES_COLUMN: usize = 8;
/// Column where `.lst` mnemonics start.
const LST_MNEMONIC_COLUMN: usize = 96;
/// Column where `.lst` arguments start.
const LST_ARGS_COLUMN: usize = 120;
/// Trailing data is emitted in chunks of this many bytes.
const DB_CHUNK: usize = 16;

/// Original-tool display signatures for the command VM (0x00-0x50).
///
/// A signature without a `:` means the operand bytes are printed raw; an empty
/// entry is an undecodable dead slot. This table matches the operand split of
/// the original tool, which differs from the decoder's own signature for some
/// opcodes (e.g. `aot_set` splits its trailing words into bytes).
const COMMAND_SIG: [&str; 81] = [
    "end:u",
    "if:l",
    "else:l",
    "endif:u",
    "ck:fuu",
    "set:fuu",
    "cmpb:uuu",
    "cmpw:uuuI",
    "setb:uuu",
    "cutnext:u",
    "cutcurr:u",
    "message:uU",
    "door_aot_set:uIIIIuuuuurIIIItu",
    "aot_set:uIIIIsuuuuuuu",
    "nop:u",
    "scene_setup:uUUU",
    "testitem:t",
    "testpickup:t",
    "aot_reset:usuIII",
    "aot_delete:usu",
    "evt_exec:uup",
    "bgm_play:u",
    "bgm_stop:u",
    "se_play_3d:uubuuII",
    "item_aot_set:uIIIItuuuuuuuuuuuuuuu",
    "setbyte:uuu",
    "item_ck:t",
    "enemy:euuuuuuIuuIIIuuuu",
    "timer_setup",
    "ck_last_item",
    "xa_on",
    "obj:uuuIIIIuuuuuuuuuuuuuuuu",
    "dir_set:uIIIIII",
    "pos_set:uIIIIII",
    "ck_item_count",
    "cut_auto:u",
    "aot_on:usu",
    "aot_switch",
    "",
    "snd_fadeout:u",
    "eml_state:uuuUu",
    "movie_on:u",
    "effect",
    "plw_anim",
    "remove_item:t",
    "give_item:uuu",
    "",
    "se_volume",
    "inst_cfg",
    "setw",
    "nop_wide",
    "sys_multi:uU",
    "model_op",
    "objtbl_b_set",
    "ck_anim",
    "tbl37_set:uuu",
    "ck_bits",
    "get_eml_state:u",
    "msgnode_set",
    "eml_rot",
    "ck_counter",
    "effect_tracked",
    "effect_kill_a:u",
    "ck_tween",
    "obj_xfm",
    "spd_set",
    "effect_kill_b",
    "se_rate",
    "task_kill",
    "spd_add",
    "msg_list",
    "eml_pos",
    "effect_clear:u",
    "bank_bit:u",
    "bgm_bank_down:u",
    "bgm_bank_up:u",
    "swap_var",
    "objs_hide:u",
    "mass_mask",
    "xa_flag:u",
    "xa_flag_ck",
];

/// Original-tool display signatures for event state 0 (0x00-0x09).
const EVENT_TOP_SIG: [&str; 10] = [
    "evt_nop",
    "evt_actor_begin",
    "evt_tween_begin",
    "evt_tween_begin_alt",
    "evt_work_set:wu",
    "evt_fork:upu",
    "evt_block:u",
    "evt_single:u",
    "evt_chain",
    "evt_disable:u",
];

/// Original-tool display signatures for control opcodes (0xF6-0xFF).
const CONTROL_SIG: [&str; 10] = [
    "evt_push_cond",
    "evt_skip_if",
    "evt_sleep",
    "evt_sleep_tick",
    "evt_for:uU",
    "evt_fornext",
    "evt_do:u",
    "evt_dountil",
    "evt_next",
    "evt_finish",
];

/// Original-tool display signatures for actor opcodes (state 1).
const ACTOR_SIG: [&str; 13] = [
    "act_nop",
    "act_reset",
    "act_motion:u",
    "act_motion_bitclr",
    "act_anim_seq:UUU",
    "act_anim_flags:bbb",
    "act_anim_set:bbb",
    "act_idle",
    "act_flag_op",
    "act_param_set:Uu",
    "act_action_a:u",
    "act_action_b:u",
    "act_end",
];

/// Original-tool display signatures for tween opcodes (state 2).
const TWEEN_SIG: [&str; 12] = [
    "tw_nop",
    "tw_end",
    "tw_pos_add",
    "tw_rot_add",
    "tw_pos_rot_add",
    "tw_set_pos:uuu",
    "tw_set_rot:uuu",
    "tw_abs_pos:UUUu",
    "tw_set_field:ub",
    "tw_set_8a:u",
    "tw_set_sel:uuu",
    "tw_abs_rot:UUUu",
];

/// Which event sub-ISA the renderer is currently inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum EventState {
    #[default]
    Top,
    Actor,
    Tween,
}

/// The sub-ISA state after `insn` has been executed.
pub(super) fn next_event_state(state: EventState, insn: &Insn) -> EventState {
    match &insn.decoded {
        Decoded::Event(op) => match op.op {
            0x01 => EventState::Actor,
            0x02 | 0x03 => EventState::Tween,
            _ => EventState::Top,
        },
        Decoded::Actor(op) => match op.op {
            0x80 | 0x8B => EventState::Top,
            _ => EventState::Actor,
        },
        Decoded::Tween(op) => match op.op {
            0x01 => EventState::Top,
            _ => EventState::Tween,
        },
        Decoded::Control(op) if op.op == 0xFF => EventState::Top,
        _ => state,
    }
}

/// Label references for one stream, keyed by the referenced target offset.
#[derive(Default)]
pub(super) struct Labels {
    names: BTreeMap<usize, String>,
}

impl Labels {
    fn get(&self, target: usize) -> Option<&str> {
        self.names.get(&target).map(String::as_str)
    }
}

/// Render the `.s` disassembly of every stream.
pub fn render(scripts: &Scripts, _data: &[u8]) -> String {
    render_with(scripts, Style::Assembly)
}

/// Render the `.lst` listing (offsets, hex bytes and mnemonic columns).
pub fn render_listing(scripts: &Scripts, _data: &[u8]) -> String {
    render_with(scripts, Style::Listing)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Assembly,
    Listing,
}

fn render_with(scripts: &Scripts, style: Style) -> String {
    let mut emitter = Emitter::new(style);

    emitter.raw(".version 1");
    emitter.blank();
    emitter.raw(".init");
    for block in &scripts.init {
        emit_insns(
            &mut emitter,
            &block.insns,
            &block.trailing,
            block.offset + 2,
        );
    }

    emitter.blank();
    emitter.raw(".main");
    for block in &scripts.main {
        emit_insns(
            &mut emitter,
            &block.insns,
            &block.trailing,
            block.offset + 2,
        );
    }

    for stream in &scripts.events {
        let StreamKind::Event(index) = stream.kind else {
            continue;
        };
        emitter.blank();
        if index != 0 {
            emitter.blank();
        }
        emitter.raw(&format!(".event event_{index:02X}"));
        emit_insns(&mut emitter, &stream.insns, &stream.trailing, stream.offset);
    }

    emitter.finish()
}

/// One instruction or trailing chunk covered by the label planner.
struct Atom {
    offset: usize,
    len: usize,
}

/// Build the atoms of a stream: every instruction plus 16-byte `db` chunks.
fn collect_atoms(insns: &[Insn], trailing: &[u8], fallback_start: usize) -> Vec<Atom> {
    let mut atoms: Vec<Atom> = insns
        .iter()
        .map(|insn| Atom {
            offset: insn.offset,
            len: insn.bytes.len(),
        })
        .collect();
    let start = atoms
        .last()
        .map(|atom| atom.offset + atom.len)
        .unwrap_or(fallback_start);
    let mut pos = start;
    for chunk in trailing.chunks(DB_CHUNK) {
        atoms.push(Atom {
            offset: pos,
            len: chunk.len(),
        });
        pos += chunk.len();
    }
    atoms
}

/// Collect the label offsets referenced by jump operands of `insns`.
fn referenced_targets(insns: &[Insn]) -> Vec<usize> {
    let mut targets = Vec::new();
    for insn in insns {
        match &insn.decoded {
            Decoded::Command(op) if op.op == 0x01 => {
                // The `if` label is `op + skip`, two bytes before the resume
                // target the reader stores on the operand.
                if let Some(target) = insn
                    .operands
                    .first()
                    .and_then(|operand| operand.target)
                    .and_then(|target| target.checked_sub(2))
                {
                    targets.push(target);
                }
            }
            Decoded::Command(op) if op.op == 0x02 => {
                targets.extend(insn.operands.first().and_then(|operand| operand.target));
            }
            Decoded::Control(op) if op.op == 0xFC => {
                targets.extend(insn.operands.first().and_then(|operand| operand.target));
            }
            _ => {}
        }
    }
    targets
}

/// Resolve referenced targets to label names and definition offsets.
fn plan_labels(atoms: &[Atom], targets: impl Iterator<Item = usize>) -> (Labels, BTreeSet<usize>) {
    let mut names = BTreeMap::new();
    let mut definitions = BTreeSet::new();
    for target in targets {
        let Some(atom) = atoms
            .iter()
            .find(|atom| target >= atom.offset && target < atom.offset + atom.len)
        else {
            continue;
        };
        definitions.insert(atom.offset);
        let name = if target == atom.offset {
            format!("off_{target:04X}")
        } else {
            format!("off_{:04X} + {}", atom.offset, target - atom.offset)
        };
        names.insert(target, name);
    }
    (Labels { names }, definitions)
}

/// Emit every instruction and trailing chunk of one block or event stream.
fn emit_insns(emitter: &mut Emitter, insns: &[Insn], trailing: &[u8], fallback_start: usize) {
    let atoms = collect_atoms(insns, trailing, fallback_start);
    let end = atoms
        .last()
        .map(|atom| atom.offset + atom.len)
        .unwrap_or(fallback_start);
    let targets = referenced_targets(insns)
        .into_iter()
        .filter(|&target| target >= fallback_start && target < end);
    let (labels, definitions) = plan_labels(&atoms, targets);

    let mut state = EventState::Top;
    for insn in insns {
        if definitions.contains(&insn.offset) {
            emitter.pending_label = Some(format!("off_{:04X}", insn.offset));
        }
        let (name, args) = format_insn(insn, state, Some(&labels));
        emitter.op(insn.offset, &insn.bytes, name, args);
        state = next_event_state(state, insn);
    }

    let mut position = insns
        .last()
        .map(|insn| insn.offset + insn.bytes.len())
        .unwrap_or(fallback_start);
    for chunk in trailing.chunks(DB_CHUNK) {
        if definitions.contains(&position) {
            emitter.pending_label = Some(format!("off_{position:04X}"));
        }
        let text = chunk
            .iter()
            .map(|byte| format!("0x{byte:02X}"))
            .collect::<Vec<_>>()
            .join(", ");
        emitter.op(position, chunk, "db".to_string(), vec![text]);
        position += chunk.len();
    }
}

/// Format one instruction as a mnemonic plus rendered arguments.
pub(super) fn format_insn(
    insn: &Insn,
    state: EventState,
    labels: Option<&Labels>,
) -> (String, Vec<String>) {
    if let Decoded::SubBlockHeader { size } = insn.decoded {
        return ("evt_block_sub".to_string(), vec![size.to_string()]);
    }
    if let Decoded::Control(op) = insn.decoded
        && op.op == 0xFF
    {
        let name = match state {
            EventState::Actor => "act_finish",
            EventState::Tween => "tw_finish",
            EventState::Top => "evt_finish",
        };
        return (name.to_string(), Vec::new());
    }
    if let Decoded::Actor(op) = insn.decoded
        && op.op == 0x81
        && insn.bytes.len() == 10
    {
        return parse_params(insn, "act_motion_path:uUUUuu", labels);
    }
    if let Decoded::Control(op) = insn.decoded
        && op.op == 0xFC
        && let Some(labels) = labels
        && let Some(target) = insn.operands.first().and_then(|operand| operand.target)
        && let Some(name) = labels.get(target)
    {
        return ("evt_do".to_string(), vec![name.to_string()]);
    }

    // Variable-width commands were decoded with the real operand split; render
    // those values instead of re-reading a fixed display signature, which would
    // invent operands for the shorter sub-commands.
    if let Decoded::Command(op) = insn.decoded
        && op.width.is_none()
    {
        let args = insn
            .operands
            .iter()
            .map(|operand| operand.value.to_string())
            .collect();
        return (op.mnemonic.to_string(), args);
    }

    match display_signature(insn) {
        Some(signature) if signature.contains(':') => parse_params(insn, signature, labels),
        Some(signature) => (signature.to_string(), raw_args(insn)),
        None => ("unk".to_string(), unknown_args(insn)),
    }
}

/// The original-tool display signature, `None` for undecodable opcodes.
fn display_signature(insn: &Insn) -> Option<&'static str> {
    let signature = match &insn.decoded {
        Decoded::Command(op) => COMMAND_SIG.get(usize::from(op.op)).copied(),
        Decoded::Event(op) => EVENT_TOP_SIG.get(usize::from(op.op)).copied(),
        Decoded::Control(op) => op
            .op
            .checked_sub(0xF6)
            .and_then(|index| CONTROL_SIG.get(usize::from(index)))
            .copied(),
        Decoded::Actor(op) => match op.op {
            0x00 => ACTOR_SIG.first().copied(),
            0x80..=0x8B => ACTOR_SIG.get(usize::from(op.op - 0x80) + 1).copied(),
            _ => None,
        },
        Decoded::Tween(op) => TWEEN_SIG.get(usize::from(op.op)).copied(),
        _ => None,
    };
    signature.filter(|signature| !signature.is_empty())
}

/// Parse a `name:params` display signature into a mnemonic and arguments.
fn parse_params(
    insn: &Insn,
    signature: &'static str,
    labels: Option<&Labels>,
) -> (String, Vec<String>) {
    let (name, params) = signature
        .split_once(':')
        .expect("parameterised display signature has a colon");
    let mut args = Vec::new();
    let mut position = 1usize;
    for (index, param) in params.bytes().enumerate() {
        if let Some(text) = special_constant(insn.op, index, &insn.bytes) {
            args.push(text);
        } else {
            let value = byte_at(insn, position);
            args.push(match param {
                b'l' => label_or_value(insn.op, insn, position, labels),
                b'u' => value.to_string(),
                b'U' => u16_at(insn, position).to_string(),
                b'I' => i16_at(insn, position).to_string(),
                b'b' => format!("{} | {}", (value >> 5) << 5, value & 0x1F),
                b'f' => named_or_decimal(constants::flag_group(value), value),
                b's' => named_or_decimal(constants::sce(value), value),
                b'w' => named_or_decimal(constants::work(value), value),
                b'e' => constants::enemy(value),
                b't' => constants::key(value),
                b'p' => constants::event(value),
                b'r' => constants::rdt(value),
                _ => value.to_string(),
            });
        }
        position += match param {
            b'U' | b'I' => 2,
            _ => 1,
        };
    }
    (name.to_string(), args)
}

/// `aot_set`/`aot_reset` rewrite one byte to `event_XX` when the SCE type is
/// `SCE_EVENT`; the original tool reads that byte from a fixed offset.
fn special_constant(opcode: u8, index: usize, bytes: &[u8]) -> Option<String> {
    match (opcode, index) {
        (0x0D, 9) if bytes.get(10) == Some(&9) => {
            bytes.get(14).map(|&value| constants::event(value))
        }
        (0x12, 4) if bytes.get(2) == Some(&9) => bytes.get(6).map(|&value| constants::event(value)),
        _ => None,
    }
}

/// `if`/`else` label rendering with a raw-value fallback.
fn label_or_value(opcode: u8, insn: &Insn, position: usize, labels: Option<&Labels>) -> String {
    let value = byte_at(insn, position);
    let target = insn.operands.first().and_then(|operand| operand.target);
    let target = match opcode {
        0x01 => target.and_then(|target| target.checked_sub(2)),
        _ => target,
    };
    match (labels, target) {
        (Some(labels), Some(target)) => labels
            .get(target)
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string()),
        _ => value.to_string(),
    }
}

/// Raw operand bytes rendered as decimals (signatures without a colon).
fn raw_args(insn: &Insn) -> Vec<String> {
    insn.bytes
        .get(1..)
        .unwrap_or(&[])
        .iter()
        .map(u8::to_string)
        .collect()
}

/// Undecodable opcodes print `unk` plus the opcode and remaining bytes.
fn unknown_args(insn: &Insn) -> Vec<String> {
    let mut args = vec![insn.op.to_string()];
    args.extend(raw_args(insn));
    args
}

fn byte_at(insn: &Insn, position: usize) -> u8 {
    insn.bytes.get(position).copied().unwrap_or(0)
}

fn u16_at(insn: &Insn, position: usize) -> u16 {
    u16::from_le_bytes([byte_at(insn, position), byte_at(insn, position + 1)])
}

fn i16_at(insn: &Insn, position: usize) -> i16 {
    i16::from_le_bytes([byte_at(insn, position), byte_at(insn, position + 1)])
}

fn named_or_decimal(name: Option<&'static str>, value: u8) -> String {
    name.map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

/// A rendered output line body.
enum Body {
    Raw(String),
    Op { name: String, args: Vec<String> },
}

/// One pending output line, optionally carrying a label definition.
struct Line {
    label: Option<String>,
    offset: Option<(usize, Vec<u8>)>,
    body: Body,
}

/// Line buffer that serialises to `.s` or `.lst`.
struct Emitter {
    style: Style,
    lines: Vec<Line>,
    pending_label: Option<String>,
}

impl Emitter {
    fn new(style: Style) -> Self {
        Self {
            style,
            lines: Vec::new(),
            pending_label: None,
        }
    }

    fn raw(&mut self, text: &str) {
        self.lines.push(Line {
            label: self.pending_label.take(),
            offset: None,
            body: Body::Raw(text.to_string()),
        });
    }

    fn blank(&mut self) {
        self.raw("");
    }

    fn op(&mut self, offset: usize, bytes: &[u8], name: String, args: Vec<String>) {
        self.lines.push(Line {
            label: self.pending_label.take(),
            offset: Some((offset, bytes.to_vec())),
            body: Body::Op { name, args },
        });
    }

    fn finish(self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            if let Some(label) = &line.label {
                out.push('\n');
                out.push_str(label);
                out.push_str(":\n");
            }
            match &line.body {
                Body::Raw(text) => {
                    out.push_str(text);
                    out.push('\n');
                }
                Body::Op { name, args } => {
                    let mut text = String::new();
                    if self.style == Style::Listing {
                        if let Some((offset, bytes)) = &line.offset {
                            text.push_str(&format!("{offset:04X}:"));
                            pad_to(&mut text, LST_BYTES_COLUMN);
                            for byte in bytes {
                                text.push_str(&format!("{byte:02X}"));
                            }
                        }
                        pad_to(&mut text, LST_MNEMONIC_COLUMN);
                    } else {
                        pad_to(&mut text, ASM_MNEMONIC_COLUMN);
                    }
                    text.push_str(name);
                    if !args.is_empty() {
                        let column = if self.style == Style::Listing {
                            LST_ARGS_COLUMN
                        } else {
                            ASM_ARGS_COLUMN
                        };
                        pad_to(&mut text, column);
                        text.push_str(&args.join(", "));
                    }
                    out.push_str(&text);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// Pad `text` with spaces until it reaches `column` characters.
fn pad_to(text: &mut String, column: usize) {
    while text.len() < column {
        text.push(' ');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scd::ir::{Block, Operand, Stream};
    use crate::scd::opcode::{actor_op, command_op, event_control_op, event_top_op};

    fn operand(value: i64, target: Option<usize>) -> Operand {
        Operand { value, target }
    }

    fn command_insn(offset: usize, bytes: &[u8], operands: Vec<Operand>) -> Insn {
        Insn {
            offset,
            op: bytes[0],
            decoded: Decoded::Command(command_op(bytes[0]).unwrap()),
            operands,
            bytes: bytes.to_vec(),
        }
    }

    fn event_insn(offset: usize, bytes: &[u8]) -> Insn {
        Insn {
            offset,
            op: bytes[0],
            decoded: Decoded::Event(event_top_op(bytes[0]).unwrap()),
            operands: Vec::new(),
            bytes: bytes.to_vec(),
        }
    }

    fn control_insn(offset: usize, bytes: &[u8]) -> Insn {
        Insn {
            offset,
            op: bytes[0],
            decoded: Decoded::Control(event_control_op(bytes[0]).unwrap()),
            operands: Vec::new(),
            bytes: bytes.to_vec(),
        }
    }

    fn actor_insn(offset: usize, bytes: &[u8]) -> Insn {
        Insn {
            offset,
            op: bytes[0],
            decoded: Decoded::Actor(actor_op(bytes[0]).unwrap()),
            operands: Vec::new(),
            bytes: bytes.to_vec(),
        }
    }

    fn sub_block(offset: usize, size: u16, bytes: &[u8]) -> Insn {
        Insn {
            offset,
            op: bytes[0],
            decoded: Decoded::SubBlockHeader { size },
            operands: Vec::new(),
            bytes: bytes.to_vec(),
        }
    }

    fn block(offset: usize, insns: Vec<Insn>) -> Block {
        let len: usize = insns.iter().map(|insn| insn.bytes.len()).sum();
        Block {
            offset,
            size: u16::try_from(len + 2).unwrap(),
            insns,
            trailing: Vec::new(),
        }
    }

    /// ROOM1001's first `door_aot_set` record, with every constant table in play.
    const DOOR: [u8; 26] = [
        0x0C, 0x00, 0x8C, 0x0A, 0xF4, 0x01, 0xA4, 0x06, 0x08, 0x07, 0x03, 0x00, 0x00, 0x04, 0x00,
        0x01, 0xFC, 0x21, 0x00, 0x00, 0xDC, 0x1E, 0x00, 0x04, 0x00, 0x81,
    ];

    fn door_scripts() -> Scripts {
        let insn = command_insn(
            0x100,
            &DOOR,
            DOOR[1..]
                .iter()
                .map(|&byte| operand(i64::from(byte), None))
                .collect(),
        );
        Scripts {
            init: vec![block(0x0FE, vec![insn])],
            main: Vec::new(),
            events: Vec::new(),
        }
    }

    #[test]
    fn init_block_renders_door_constants() {
        let expected = "\
.version 1

.init
    door_aot_set            0, 2700, 500, 1700, 1800, 3, 0, 0, 4, 0, RDT_001, 8700, 0, 7900, 1024, UNLOCKED, 129

.main
";
        assert_eq!(render(&door_scripts(), &[]), expected);
    }

    #[test]
    fn listing_places_offsets_bytes_and_columns() {
        let listing = render_listing(&door_scripts(), &[]);
        let line = listing
            .lines()
            .find(|line| line.contains("door_aot_set"))
            .expect("door line is listed");

        assert_eq!(&line[0..5], "0100:");
        assert_eq!(&line[5..LST_BYTES_COLUMN], "   ");
        assert_eq!(
            &line[LST_BYTES_COLUMN..LST_BYTES_COLUMN + DOOR.len() * 2],
            "0C008C0AF401A4060807030000040001FC210000DC1E00040081"
        );
        assert_eq!(
            &line[LST_MNEMONIC_COLUMN..LST_MNEMONIC_COLUMN + "door_aot_set".len()],
            "door_aot_set"
        );
        assert_eq!(
            &line[LST_ARGS_COLUMN..],
            "0, 2700, 500, 1700, 1800, 3, 0, 0, 4, 0, RDT_001, 8700, 0, 7900, 1024, UNLOCKED, 129"
        );
    }

    #[test]
    fn if_else_labels_are_emitted_where_referenced() {
        let insns = vec![
            command_insn(0x200, &[0x01, 0x0A], vec![operand(0x0A, Some(0x20C))]),
            command_insn(
                0x202,
                &[0x04, 0x00, 0x1F, 0x01],
                vec![operand(0, None), operand(0x1F, None), operand(1, None)],
            ),
            command_insn(
                0x206,
                &[0x05, 0x00, 0x01, 0x00],
                vec![operand(0, None), operand(1, None), operand(0, None)],
            ),
            command_insn(0x20A, &[0x02, 0x06], vec![operand(0x06, Some(0x210))]),
            command_insn(
                0x20C,
                &[0x05, 0x02, 0x02, 0x00],
                vec![operand(2, None), operand(2, None), operand(0, None)],
            ),
            command_insn(0x210, &[0x03, 0x00], vec![operand(0, None)]),
        ];
        let scripts = Scripts {
            init: vec![block(0x100, insns)],
            main: Vec::new(),
            events: Vec::new(),
        };
        let expected = "\
.version 1

.init
    if                      off_020A
    ck                      FG_SCENARIO, 31, 1
    set                     FG_SCENARIO, 1, 0

off_020A:
    else                    off_0210
    set                     FG_LOCK, 2, 0

off_0210:
    endif                   0

.main
";
        assert_eq!(render(&scripts, &[]), expected);
    }

    #[test]
    fn event_subheaders_raw_ops_and_trailing_db() {
        let insns = vec![
            event_insn(0x400, &[0x06, 0x0D]),
            sub_block(0x402, 4, &[0x04, 0x00]),
            command_insn(0x404, &[0x00, 0x00], vec![operand(0, None)]),
            sub_block(0x406, 6, &[0x06, 0x00]),
            command_insn(0x408, &[0x0E, 0x00], vec![operand(0, None)]),
            command_insn(0x40A, &[0x0E, 0x00], vec![operand(0, None)]),
            sub_block(0x40C, 0, &[0x00]),
            event_insn(0x40D, &[0x00]),
            control_insn(0x40E, &[0xF8, 0xF9, 0x01, 0x00]),
            control_insn(0x412, &[0xFF]),
        ];
        let scripts = Scripts {
            init: Vec::new(),
            main: Vec::new(),
            events: vec![Stream {
                kind: StreamKind::Event(0),
                offset: 0x400,
                insns,
                trailing: vec![0xAA, 0xBB],
            }],
        };
        let expected = "\
.version 1

.init

.main

.event event_00
    evt_block               13
    evt_block_sub           4
    end                     0
    evt_block_sub           6
    nop                     0
    nop                     0
    evt_block_sub           0
    evt_nop
    evt_sleep               249, 1, 0
    evt_finish
    db                      0xAA, 0xBB
";
        assert_eq!(render(&scripts, &[]), expected);
    }

    #[test]
    fn actor_ops_render_bitfields_and_raw_operands() {
        let insns = vec![
            event_insn(0x500, &[0x01]),
            actor_insn(0x501, &[0x84, 0x30, 0x3F, 0x00]),
            actor_insn(0x505, &[0x87, 0x00, 0x40, 0x00]),
            control_insn(0x509, &[0xFF]),
        ];
        let scripts = Scripts {
            init: Vec::new(),
            main: Vec::new(),
            events: vec![Stream {
                kind: StreamKind::Event(0),
                offset: 0x500,
                insns,
                trailing: Vec::new(),
            }],
        };
        let expected = "\
.version 1

.init

.main

.event event_00
    evt_actor_begin
    act_anim_flags          32 | 16, 32 | 31, 0 | 0
    act_flag_op             0, 64, 0
    act_finish
";
        assert_eq!(render(&scripts, &[]), expected);
    }

    #[test]
    fn labels_inside_an_instruction_use_plus_delta() {
        let insns = vec![
            command_insn(0x300, &[0x01, 0x05], vec![operand(0x05, Some(0x307))]),
            command_insn(
                0x302,
                &[0x04, 0x00, 0x1F, 0x01],
                vec![operand(0, None), operand(0x1F, None), operand(1, None)],
            ),
            command_insn(
                0x306,
                &[0x05, 0x00, 0x01, 0x00],
                vec![operand(0, None), operand(1, None), operand(0, None)],
            ),
        ];
        let scripts = Scripts {
            init: vec![block(0x100, insns)],
            main: Vec::new(),
            events: Vec::new(),
        };
        let expected = "\
.version 1

.init
    if                      off_0302 + 3

off_0302:
    ck                      FG_SCENARIO, 31, 1
    set                     FG_SCENARIO, 1, 0

.main
";
        assert_eq!(render(&scripts, &[]), expected);
    }

    #[test]
    fn evt_do_targets_are_labelled() {
        let insns = vec![
            control_insn(0x600, &[0xFC, 0x06]),
            command_insn(
                0x602,
                &[0x06, 0x02, 0x80, 0x00],
                vec![operand(2, None), operand(0x80, None), operand(0, None)],
            ),
            control_insn(0x606, &[0xFE]),
            control_insn(0x607, &[0xFD]),
        ];
        let mut scripts = Scripts {
            init: Vec::new(),
            main: Vec::new(),
            events: vec![Stream {
                kind: StreamKind::Event(0),
                offset: 0x600,
                insns,
                trailing: Vec::new(),
            }],
        };
        scripts.events[0].insns[0].operands = vec![operand(0x06, Some(0x606))];
        let expected = "\
.version 1

.init

.main

.event event_00
    evt_do                  off_0606
    cmpb                    2, 128, 0

off_0606:
    evt_next
    evt_dountil
";
        assert_eq!(render(&scripts, &[]), expected);
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn renders_real_room1001() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let path = std::path::Path::new(&root).join("JPN/STAGE1/ROOM1001.RDT");
        let Ok(data) = std::fs::read(&path) else {
            return;
        };
        let scripts = crate::scd::reader::parse(&data).expect("ROOM1001 parses");

        let assembly = render(&scripts, &data);
        assert!(assembly.contains(
            "    door_aot_set            0, 2700, 500, 1700, 1800, 3, 0, 0, 4, 0, \
             RDT_001, 8700, 0, 7900, 1024, UNLOCKED, 129\n"
        ));
        assert!(assembly.contains("    evt_block               29\n"));
        assert!(assembly.contains("    dir_set                 0, 0, 2048, 0, 6800, 0, 9000\n"));
        assert!(assembly.contains("    evt_finish\n"));

        let listing = render_listing(&scripts, &data);
        assert!(listing.lines().any(|line| {
            line.len() > LST_MNEMONIC_COLUMN
                && line[LST_MNEMONIC_COLUMN..].starts_with("door_aot_set")
        }));

        let bio = crate::scd::decomp::render(&scripts, &data);
        assert!(bio.contains("        bgm_play(0);\n"));
        assert!(bio.contains("    evt_block(29);\n"));
        assert!(bio.contains("    dir_set(0, 0, 2048, 0, 6800, 0, 9000);\n"));
        assert!(bio.contains("    evt_finish();\n"));
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn renders_real_room1000() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let path = std::path::Path::new(&root).join("JPN/STAGE1/ROOM1000.RDT");
        let Ok(data) = std::fs::read(&path) else {
            return;
        };
        let scripts = crate::scd::reader::parse(&data).expect("ROOM1000 parses");
        let assembly = render(&scripts, &data);
        assert!(assembly.contains("off_10CA8"));
        assert_eq!(assembly.matches(".event ").count(), 59);
        assert_eq!(
            render(&scripts, &data),
            assembly,
            "rendering is deterministic"
        );
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn corpus_renders_without_panicking() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let mut files = Vec::new();
        collect_rdts(std::path::Path::new(&root), &mut files);
        files.sort();
        assert!(!files.is_empty(), "no RDT files found under {root}");

        let mut rendered = 0usize;
        for path in &files {
            let data = std::fs::read(path).expect("read RDT");
            if data.len() <= 4 {
                continue;
            }
            let scripts = crate::scd::reader::parse(&data)
                .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
            assert_eq!(
                render(&scripts, &data),
                render(&scripts, &data),
                "{}: .s is not deterministic",
                path.display()
            );
            assert!(!render_listing(&scripts, &data).is_empty());
            assert!(!crate::scd::decomp::render(&scripts, &data).is_empty());
            rendered += 1;
        }
        assert!(rendered > 0, "no non-stub RDTs found under {root}");
    }

    fn collect_rdts(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rdts(&path, out);
            } else if path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .to_ascii_uppercase()
                    .ends_with(".RDT")
            }) {
                out.push(path);
            }
        }
    }
}
