//! SCD decompiler: renders `.bio` pseudo-code from the decoded IR.
//!
//! Layout follows the original tool: `#version 1`, then one `proc init`, one
//! `proc main` and one `event event_XX` block per event, four spaces of
//! indentation per block level. Condition opcodes fold into `if (...)`
//! expressions with `&&` chaining; `end 0` becomes `return;` and the final
//! `return;` line of each procedure/event is dropped. `evt_block` payloads
//! keep their `evt_block_sub N` pseudo-instructions and trailing undecoded
//! bytes are omitted.

use crate::scd::disasm::{EventState, format_insn, next_event_state};
use crate::scd::ir::{Decoded, Insn, Scripts, StreamKind};

/// Command opcodes whose result folds into an `if (...)` expression.
const EXPRESSION_CONDITIONS: &[u8] = &[
    0x04, 0x06, 0x07, 0x10, 0x11, 0x1A, 0x1D, 0x22, 0x36, 0x38, 0x3C, 0x3F, 0x50,
];

/// Render the `.bio` decompilation of every stream.
pub fn render(scripts: &Scripts, _data: &[u8]) -> String {
    let mut lines: Vec<String> = vec!["#version 1".to_string(), String::new()];

    render_section(
        &mut lines,
        "proc init",
        scripts.init.iter().flat_map(|block| block.insns.iter()),
    );
    lines.push(String::new());
    render_section(
        &mut lines,
        "proc main",
        scripts.main.iter().flat_map(|block| block.insns.iter()),
    );
    for stream in &scripts.events {
        let StreamKind::Event(index) = stream.kind else {
            continue;
        };
        lines.push(String::new());
        if index != 0 {
            lines.push(String::new());
        }
        render_section(
            &mut lines,
            &format!("event event_{index:02X}"),
            stream.insns.iter(),
        );
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Render one procedure or event block into `lines`.
fn render_section<'a>(
    lines: &mut Vec<String>,
    header: &str,
    insns: impl Iterator<Item = &'a Insn>,
) {
    let mut decompiler = Decompiler::new();
    decompiler.lines.push(header.to_string());
    decompiler.open_block();
    for insn in insns {
        decompiler.process(insn);
    }
    decompiler.end_section();
    lines.extend(decompiler.lines);
}

/// True when `insn` is a command condition folded into an `if` expression.
fn is_expression_condition(insn: &Insn) -> bool {
    matches!(&insn.decoded, Decoded::Command(op) if EXPRESSION_CONDITIONS.contains(&op.op))
}

/// Incremental `.bio` writer tracking blocks, conditions and `return;` lines.
struct Decompiler {
    lines: Vec<String>,
    indent: usize,
    /// Pending `if` condition text, `None` when not in an expression.
    condition: Option<String>,
    expression_count: usize,
    /// End offsets of open `else` bodies, as pushed by `else`.
    else_ends: Vec<usize>,
    /// Line index of the most recent `return;`, removed at section end.
    last_return: Option<usize>,
    state: EventState,
}

impl Decompiler {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            indent: 0,
            condition: None,
            expression_count: 0,
            else_ends: Vec::new(),
            last_return: None,
            state: EventState::Top,
        }
    }

    /// Push one line at the current indentation.
    fn line(&mut self, text: &str) {
        self.lines
            .push(format!("{}{text}", "    ".repeat(self.indent)));
    }

    fn open_block(&mut self) {
        self.line("{");
        self.indent += 1;
    }

    fn close_block(&mut self) {
        self.indent = self.indent.saturating_sub(1);
        self.line("}");
    }

    /// Finish the pending `if` expression and open its body block.
    fn finish_condition(&mut self) {
        if let Some(condition) = self.condition.take() {
            self.line(&format!("{condition})"));
            self.open_block();
        }
    }

    /// Append a condition to the pending expression.
    fn extend_condition(&mut self, insn: &Insn) {
        let (name, args) = format_insn(insn, self.state, None);
        let condition = self.condition.as_mut().expect("condition is pending");
        if self.expression_count > 0 {
            condition.push_str(" && ");
        }
        condition.push_str(&name);
        condition.push('(');
        condition.push_str(&args.join(", "));
        condition.push(')');
        self.expression_count += 1;
    }

    /// Render one instruction as a `name(args);` statement.
    fn statement(&mut self, insn: &Insn) {
        let (name, args) = format_insn(insn, self.state, None);
        self.line(&format!("{name}({});", args.join(", ")));
    }

    /// Process one instruction in stream order.
    fn process(&mut self, insn: &Insn) {
        if self.condition.is_some() && !is_expression_condition(insn) {
            self.finish_condition();
        }
        while self.else_ends.last().is_some_and(|&end| end <= insn.offset) {
            self.close_block();
            self.else_ends.pop();
        }

        if self.condition.is_some() {
            self.extend_condition(insn);
        } else {
            match &insn.decoded {
                Decoded::Command(op) => match op.op {
                    0x00 => {
                        self.line("return;");
                        self.last_return = Some(self.lines.len() - 1);
                    }
                    0x01 => {
                        self.condition = Some("if (".to_string());
                        self.expression_count = 0;
                    }
                    0x02 => {
                        self.close_block();
                        if let Some(target) = insn.operands.first().and_then(|op| op.target) {
                            self.else_ends.push(target);
                        }
                        self.line("else");
                        self.open_block();
                    }
                    0x03 => self.close_block(),
                    _ => self.statement(insn),
                },
                _ => self.statement(insn),
            }
        }
        self.state = next_event_state(self.state, insn);
    }

    /// Close any open blocks and drop the section's final `return;`.
    fn end_section(&mut self) {
        while !self.else_ends.is_empty() {
            self.close_block();
            self.else_ends.pop();
        }
        self.close_block();
        if let Some(index) = self.last_return.take()
            && index < self.lines.len()
        {
            self.lines.remove(index);
        }
    }
}

/// Constant names and value formatting used by the operand renderer.
///
/// The tables mirror the original tool's name lists so `.s` and `.bio` output
/// is identical; out-of-range values fall back to decimal.
pub(super) mod constants {
    const FLAG_GROUPS: [&str; 10] = [
        "FG_SCENARIO",
        "FG_COMMON",
        "FG_LOCK",
        "FG_ENEMY",
        "FG_ROOM",
        "FG_STATUS",
        "FG_6",
        "FG_ITEM",
        "FG_MAP",
        "FG_9",
    ];

    const SCE_NAMES: [&str; 17] = [
        "SCE_NONE",
        "SCE_DOOR",
        "SCE_MESSAGE",
        "SCE_3",
        "SCE_ITEM",
        "SCE_5",
        "SCE_6",
        "SCE_USEITEM",
        "SCE_ITEMBOX",
        "SCE_EVENT",
        "SCE_SAVE",
        "SCE_B",
        "SCE_C",
        "SCE_DOCUMENT",
        "SCE_HIKIDASHI",
        "SCE_F",
        "SCE_TYPEWRITER",
    ];

    const WORK_VARS: [&str; 4] = ["WK_PLAYER", "WK_ENEMY", "WK_OBJ", "WK_AOT"];

    const ITEM_NAMES: [&str; 76] = [
        "Nothing",
        "Combat Knife",
        "Beretta",
        "Shotgun",
        "DumDum Colt",
        "Colt Python",
        "FlameThrower",
        "Bazooka Acid",
        "Bazooka Explosive",
        "Bazooka Flame",
        "Rocket Launcher",
        "Clip",
        "Shells",
        "DumDum Rounds",
        "Magnum Rounds",
        "FlameThrower Fuel",
        "Explosive Rounds",
        "Acid Rounds",
        "Flame Rounds",
        "Empty Bottle",
        "Water",
        "Umb No. 2",
        "Umb No. 4",
        "Umb No. 7",
        "Umb No. 13",
        "Yellow 6",
        "NP-003",
        "V-Jolt",
        "Broken Shotgun",
        "Square Crank",
        "Hex Crank",
        "Wood Emblem",
        "Gold Emblem",
        "Blue Jewel",
        "Red Jewel",
        "Music Notes",
        "Wolf Medal",
        "Eagle Medal",
        "Chemical",
        "Battery",
        "MO Disk",
        "Wind Crest",
        "Flare",
        "Slides",
        "Moon Crest",
        "Star Crest",
        "Sun Crest",
        "Ink Ribbon",
        "Lighter",
        "Lock Pick",
        "Nameless (Can of Oil)",
        "Sword Key",
        "Armor Key",
        "Sheild Key",
        "Helmet Key",
        "Lab Key (1)",
        "Special Key",
        "Dorm Key (002)",
        "Dorm Key (003)",
        "C. Room Key",
        "Lab Key (2)",
        "Small Key",
        "Red Book",
        "Doom Book (2)",
        "Doom Book (1)",
        "F-Aid Spray",
        "Serum",
        "Red Herb",
        "Green Herb",
        "Blue Herb",
        "Mixed (Red+Green)",
        "Mixed (2 Green)",
        "Mixed (Blue + Green)",
        "Mixed (All)",
        "Mixed (Silver Color)",
        "Mixed (Bright Blue-Green)",
    ];

    const ENEMY_NAMES: [&str; 51] = [
        "Zombie (Groundskeeper)",
        "Zombie (Naked)",
        "Cerberus",
        "Web Spinner",
        "Black Tiger",
        "Crow",
        "Hunter",
        "Wasp",
        "Plant 42",
        "Chimera",
        "Adder",
        "Neptune",
        "Tyrant 1",
        "Yawn 1",
        "Plant42 (roots)",
        "Fountain Plant",
        "Tyrant 2",
        "Zombie (Researcher)",
        "Yawn 2",
        "Cobweb",
        "Computer Hands (left)",
        "Computer Hands (right)",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        "Chris (Stars)",
        "Jill (Stars)",
        "Barry (Stars)",
        "Rebecca (Stars)",
        "Wesker (Stars)",
        "Kenneth 1",
        "Forrest",
        "Richard",
        "Enrico",
        "Kenneth 2",
        "Barry 2",
        "Barry 2 (Stars)",
        "Rebecca 2 (Stars)",
        "Barry 3",
        "Wesker 2 (Stars)",
        "Chris (Jacket)",
        "Jill (Black Shirt)",
        "Chris 2 (Jacket)",
        "Jill (Red Shirt)",
    ];

    /// `FG_*` flag bank name, or `None` when out of range.
    pub(crate) fn flag_group(value: u8) -> Option<&'static str> {
        FLAG_GROUPS.get(usize::from(value)).copied()
    }

    /// `SCE_*` room action type name, or `None` when out of range.
    pub(crate) fn sce(value: u8) -> Option<&'static str> {
        SCE_NAMES.get(usize::from(value)).copied()
    }

    /// `WK_*` event work variable name, or `None` when out of range.
    pub(crate) fn work(value: u8) -> Option<&'static str> {
        WORK_VARS.get(usize::from(value)).copied()
    }

    /// Item id name, e.g. `ITEM_SWORD_KEY`.
    pub(crate) fn item(value: u8) -> String {
        namify("ITEM_", &ITEM_NAMES, value)
    }

    /// Enemy type name, e.g. `ENEMY_REBECCA_STARS_`.
    pub(crate) fn enemy(value: u8) -> String {
        namify("ENEMY_", &ENEMY_NAMES, value)
    }

    /// Door/`t` operand name: key states collapse to `UNLOCKED`/`LOCK`/`LOCKED`.
    pub(crate) fn key(value: u8) -> String {
        match value {
            0 => "UNLOCKED".to_string(),
            254 => "LOCK".to_string(),
            255 => "LOCKED".to_string(),
            _ => item(value),
        }
    }

    /// Cross-room door target, e.g. `RDT_001`.
    pub(crate) fn rdt(value: u8) -> String {
        format!("RDT_{:X}{:02X}", value >> 5, value & 0x1F)
    }

    /// Event script reference, e.g. `event_0B`.
    pub(crate) fn event(index: u8) -> String {
        format!("event_{index:02X}")
    }

    /// Turn a display name into an identifier: non-alphanumerics become `_`,
    /// runs collapse and the result is uppercase; unknown ids use the hex value.
    fn namify(prefix: &str, table: &[&str], value: u8) -> String {
        let name = table.get(usize::from(value)).copied().unwrap_or("");
        if name.is_empty() {
            return format!("{prefix}{value:02X}");
        }
        let source = format!("{prefix}{name}");
        let mut out = String::with_capacity(source.len());
        for ch in source.chars() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                out.push(ch);
            } else {
                out.push('_');
            }
        }
        while out.contains("__") {
            out = out.replace("__", "_");
        }
        out.to_ascii_uppercase()
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

    #[test]
    fn folds_if_else_endif_into_blocks() {
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

        // The original tool's `endif` closes one extra level when an `else`
        // forced a block close first, so the procedure's own closing brace is
        // duplicated; mirror that shape exactly.
        let expected = "\
#version 1

proc init
{
    if (ck(FG_SCENARIO, 31, 1))
    {
        set(FG_SCENARIO, 1, 0);
    }
    else
    {
        set(FG_LOCK, 2, 0);
    }
}
}

proc main
{
}
";
        assert_eq!(render(&scripts, &[]), expected);
    }

    #[test]
    fn event_block_subheaders_and_final_return_removal() {
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
#version 1

proc init
{
}

proc main
{
}

event event_00
{
    evt_block(13);
    evt_block_sub(4);
    evt_block_sub(6);
    nop(0);
    nop(0);
    evt_block_sub(0);
    evt_nop();
    evt_sleep(249, 1, 0);
    evt_finish();
}
";
        assert_eq!(render(&scripts, &[]), expected);
    }

    #[test]
    fn actor_ops_keep_the_sub_isa_finish_name() {
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
#version 1

proc init
{
}

proc main
{
}

event event_00
{
    evt_actor_begin();
    act_anim_flags(32 | 16, 32 | 31, 0 | 0);
    act_flag_op(0, 64, 0);
    act_finish();
}
";
        assert_eq!(render(&scripts, &[]), expected);
    }
}
