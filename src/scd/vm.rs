//! The two SCD interpreters: the command VM (init/main) and the event VM.

use std::collections::HashMap;

use crate::scd::host::{ScdHost, StepResult};
use crate::scd::ir::{Block, Decoded, Insn, Operand, Scripts};
use crate::scd::opcode::Op;

/// Resume-address stack depth of the command VM.
const BRANCH_STACK_SIZE: usize = 16;
/// Counter/call stack depth of one event slot.
const EVENT_STACK_SIZE: usize = 4;
/// Number of cooperative event slots.
const SLOT_COUNT: usize = 8;
/// Per-run instruction cap so malformed streams cannot spin forever.
const MAX_STEPS: usize = 100_000;

/// Command-VM state: runs init once and main every tick.
pub struct CommandVm<'a> {
    scripts: &'a Scripts,
    insns: HashMap<usize, &'a Insn>,
    branch_stack: [usize; BRANCH_STACK_SIZE],
    depth: usize,
    pc: usize,
}

impl<'a> CommandVm<'a> {
    pub fn new(scripts: &'a Scripts) -> Self {
        let mut insns = HashMap::new();
        for block in scripts.init.iter().chain(scripts.main.iter()) {
            for insn in &block.insns {
                insns.insert(insn.offset, insn);
            }
        }
        Self {
            scripts,
            insns,
            branch_stack: [0; BRANCH_STACK_SIZE],
            depth: 0,
            pc: 0,
        }
    }

    /// Run all init blocks once.
    pub fn run_init(&mut self, host: &mut impl ScdHost) {
        let scripts = self.scripts;
        self.run_blocks(&scripts.init, host);
    }

    /// Run all main blocks once (per frame).
    pub fn run_main(&mut self, host: &mut impl ScdHost) {
        let scripts = self.scripts;
        self.run_blocks(&scripts.main, host);
    }

    fn run_blocks(&mut self, blocks: &'a [Block], host: &mut impl ScdHost) {
        self.depth = 0;
        for block in blocks {
            let Some(first) = block.insns.first() else {
                continue;
            };
            self.pc = first.offset;
            let block_end = block_end(block);
            let mut steps = 0;
            loop {
                if steps >= MAX_STEPS || self.pc >= block_end {
                    return;
                }
                steps += 1;
                let Some(insn) = self.insns.get(&self.pc).copied() else {
                    return;
                };
                let next = insn.offset + insn.bytes.len();
                let mut block_done = false;
                match &insn.decoded {
                    Decoded::Command(op) => match op.op {
                        0x00 => {
                            if self.depth == 0 {
                                block_done = true;
                            } else {
                                self.depth -= 1;
                                self.pc = self.branch_stack[self.depth];
                            }
                        }
                        0x01 => {
                            self.push_branch(if_target(insn));
                            self.pc = next;
                        }
                        0x02 => {
                            self.pop_branch();
                            self.pc = else_target(insn);
                        }
                        0x03 => {
                            self.pop_branch();
                            self.pc = next;
                        }
                        _ if op.condition => {
                            if eval_condition(host, insn, op) {
                                self.pc = next;
                            } else if self.depth > 0 {
                                self.depth -= 1;
                                self.pc = self.branch_stack[self.depth];
                            } else {
                                block_done = true;
                            }
                        }
                        _ => {
                            dispatch_command(host, op, &insn.operands);
                            self.pc = next;
                        }
                    },
                    _ => return,
                }
                if block_done {
                    break;
                }
            }
        }
    }

    fn push_branch(&mut self, target: usize) {
        if self.depth < BRANCH_STACK_SIZE {
            self.branch_stack[self.depth] = target;
            self.depth += 1;
        }
    }

    fn pop_branch(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }
}

/// One cooperative event-VM slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventSlot {
    state: u8,
    active: bool,
    pc: usize,
    entity: u8,
    return_stack: [usize; EVENT_STACK_SIZE],
    call_stack: [usize; EVENT_STACK_SIZE],
    counter_stack: [i16; EVENT_STACK_SIZE],
    depth: usize,
    /// Offset of the `evt_sleep` instruction this slot is currently counting
    /// down, so a re-executed instruction does not push a second counter.
    sleeping: Option<usize>,
}

impl Default for EventSlot {
    fn default() -> Self {
        Self {
            state: 0,
            active: false,
            pc: 0,
            entity: 0,
            return_stack: [0; EVENT_STACK_SIZE],
            call_stack: [0; EVENT_STACK_SIZE],
            counter_stack: [0; EVENT_STACK_SIZE],
            depth: 0,
            sleeping: None,
        }
    }
}

/// Event-VM state: eight cooperative slots.
pub struct EventVm<'a> {
    scripts: &'a Scripts,
    insns: HashMap<usize, &'a Insn>,
    payloads: HashMap<usize, Vec<&'a Insn>>,
    slots: [EventSlot; SLOT_COUNT],
}

impl<'a> EventVm<'a> {
    pub fn new(scripts: &'a Scripts) -> Self {
        let mut insns = HashMap::new();
        let mut payloads = HashMap::new();
        for stream in &scripts.events {
            for insn in &stream.insns {
                insns.insert(insn.offset, insn);
            }
            for (index, insn) in stream.insns.iter().enumerate() {
                let Decoded::Event(op) = &insn.decoded else {
                    continue;
                };
                if !matches!(op.op, 0x06 | 0x07) {
                    continue;
                }
                let end = insn.offset.saturating_add(payload_len(insn));
                let payload = stream.insns[index + 1..]
                    .iter()
                    .take_while(|later| later.offset < end)
                    .collect();
                payloads.insert(insn.offset, payload);
            }
        }
        Self {
            scripts,
            insns,
            payloads,
            slots: [EventSlot::default(); SLOT_COUNT],
        }
    }

    /// Start event `index` in the first free slot (or the given slot).
    pub fn start(&mut self, slot: usize, event: u8) {
        let target = if slot < SLOT_COUNT {
            slot
        } else {
            self.free_slot()
        };
        self.restart(target, event, false);
    }

    /// Step every active slot once.
    pub fn step(&mut self, host: &mut impl ScdHost) {
        for index in 0..SLOT_COUNT {
            if !self.slots[index].active {
                continue;
            }
            let mut steps = 0;
            while self.slots[index].active {
                if steps >= MAX_STEPS {
                    break;
                }
                steps += 1;
                if !self.step_slot(index, host) {
                    break;
                }
            }
        }
    }

    /// Number of currently active slots.
    pub fn active_slots(&self) -> usize {
        self.slots.iter().filter(|slot| slot.active).count()
    }

    fn free_slot(&self) -> usize {
        self.slots
            .iter()
            .position(|slot| !slot.active)
            .unwrap_or(SLOT_COUNT - 1)
    }

    fn restart(&mut self, slot: usize, event: u8, keep_entity: bool) {
        if slot >= SLOT_COUNT {
            return;
        }
        let entity = if keep_entity {
            self.slots[slot].entity
        } else {
            0
        };
        let mut state = EventSlot {
            entity,
            ..EventSlot::default()
        };
        if let Some(stream) = self.scripts.events.get(event as usize)
            && let Some(first) = stream.insns.first()
        {
            state.active = true;
            state.pc = first.offset;
        }
        self.slots[slot] = state;
    }

    fn step_slot(&mut self, index: usize, host: &mut impl ScdHost) -> bool {
        let Some(insn) = self.insns.get(&self.slots[index].pc).copied() else {
            self.slots[index].active = false;
            return false;
        };
        let next = insn.offset + insn.bytes.len();
        match &insn.decoded {
            Decoded::Control(op) => match op.op {
                0xF6 => {
                    self.push_depth(index);
                    self.slots[index].pc = next;
                    true
                }
                0xF7 => {
                    let depth = self.slots[index].depth;
                    self.slots[index].depth = depth.saturating_sub(1);
                    self.slots[index].pc = next;
                    false
                }
                0xF8 => {
                    if self.slots[index].sleeping != Some(insn.offset) {
                        let count = self.sleep_count(insn);
                        self.push_counter(index, count);
                        self.slots[index].sleeping = Some(insn.offset);
                    }
                    self.tick_sleep(index, insn)
                }
                0xF9 => {
                    if self.slots[index].depth == 0 {
                        self.slots[index].pc = next;
                        return true;
                    }
                    let top = self.slots[index].depth - 1;
                    let counter = self.slots[index].counter_stack[top].wrapping_sub(1);
                    self.slots[index].counter_stack[top] = counter;
                    if counter == 0 {
                        self.slots[index].pc = insn.offset + 3;
                        self.slots[index].depth = top;
                    }
                    false
                }
                0xFA => {
                    let count = self.sleep_count(insn);
                    self.push_counter(index, count);
                    let top = self.slots[index].depth.saturating_sub(1);
                    self.slots[index].return_stack[top] = next;
                    self.slots[index].pc = next;
                    true
                }
                0xFB => {
                    if self.slots[index].depth == 0 {
                        self.slots[index].pc = next;
                        return true;
                    }
                    let top = self.slots[index].depth - 1;
                    let counter = self.slots[index].counter_stack[top].wrapping_sub(1);
                    self.slots[index].counter_stack[top] = counter;
                    if counter == 0 {
                        self.slots[index].pc = next;
                        self.slots[index].depth = top;
                    } else {
                        self.slots[index].pc = self.slots[index].return_stack[top];
                    }
                    true
                }
                0xFC => {
                    let target = branch_target(insn);
                    self.push_call(index, next, target);
                    self.slots[index].pc = target;
                    true
                }
                0xFD => {
                    if self.slots[index].depth == 0 {
                        self.slots[index].pc = next;
                        return true;
                    }
                    let top = self.slots[index].depth - 1;
                    let call = self.slots[index].call_stack[top];
                    if self.eval_condition_at(call, host) {
                        self.slots[index].pc = self.slots[index].return_stack[top];
                    } else {
                        self.slots[index].pc = next;
                        self.slots[index].depth = top;
                    }
                    true
                }
                0xFE => {
                    self.slots[index].pc = next;
                    false
                }
                0xFF => {
                    self.slots[index].active = false;
                    self.slots[index].pc = next;
                    false
                }
                _ => {
                    self.slots[index].active = false;
                    false
                }
            },
            Decoded::Event(op) if self.slots[index].state == 0 => {
                self.step_state0(index, insn, op, host)
            }
            Decoded::Actor(op) if self.slots[index].state == 1 => {
                self.step_actor(index, insn, op, host)
            }
            Decoded::Tween(op) if self.slots[index].state == 2 => {
                self.step_tween(index, insn, op, host)
            }
            _ => {
                self.slots[index].active = false;
                false
            }
        }
    }

    fn step_state0(
        &mut self,
        index: usize,
        insn: &Insn,
        op: &'static Op,
        host: &mut impl ScdHost,
    ) -> bool {
        let next = insn.offset + insn.bytes.len();
        match op.op {
            0x00 => {
                self.slots[index].pc = next;
                true
            }
            0x01 => {
                self.slots[index].state = 1;
                self.slots[index].pc = next;
                true
            }
            0x02 | 0x03 => {
                self.slots[index].state = 2;
                self.slots[index].pc = next;
                true
            }
            0x04 => {
                self.slots[index].entity = operand_u8(insn, 0);
                self.slots[index].pc = next;
                true
            }
            0x05 => {
                host.on_misc(op, &insn.operands);
                let target = operand_u8(insn, 0) as usize;
                let event = operand_u8(insn, 1);
                self.start(target, event);
                self.slots[index].pc = next;
                true
            }
            0x06 | 0x07 => {
                self.dispatch_payload(insn, host);
                self.slots[index].pc = insn.offset + payload_len(insn);
                true
            }
            0x08 => {
                let event = operand_u8(insn, 0);
                self.restart(index, event, true);
                false
            }
            0x09 => {
                let target = operand_u8(insn, 0) as usize;
                if target < SLOT_COUNT {
                    self.slots[target].active = false;
                }
                self.slots[index].pc = next;
                true
            }
            _ => {
                self.slots[index].active = false;
                false
            }
        }
    }

    fn step_actor(
        &mut self,
        index: usize,
        insn: &Insn,
        op: &'static Op,
        host: &mut impl ScdHost,
    ) -> bool {
        let result = host.on_misc(op, &insn.operands);
        if matches!(op.op, 0x80 | 0x8B) {
            self.slots[index].state = 0;
        }
        self.slots[index].pc = insn.offset + insn.bytes.len();
        result != StepResult::Yield
    }

    fn step_tween(
        &mut self,
        index: usize,
        insn: &Insn,
        op: &'static Op,
        host: &mut impl ScdHost,
    ) -> bool {
        let result = host.on_misc(op, &insn.operands);
        if op.op == 0x01 {
            self.slots[index].state = 0;
        }
        self.slots[index].pc = insn.offset + insn.bytes.len();
        result != StepResult::Yield
    }

    fn dispatch_payload(&self, evt: &Insn, host: &mut impl ScdHost) {
        let Some(payload) = self.payloads.get(&evt.offset) else {
            return;
        };
        // An `evt_block` payload is a chain of command-VM blocks; run each
        // block's flow (if/else/endif/end and conditions) independently.
        let mut start = 0usize;
        while start < payload.len() {
            if matches!(payload[start].decoded, Decoded::SubBlockHeader { .. }) {
                start += 1;
                continue;
            }
            let end = payload[start..]
                .iter()
                .position(|insn| matches!(insn.decoded, Decoded::SubBlockHeader { .. }))
                .map_or(payload.len(), |position| start + position);
            self.run_command_flow(&payload[start..end], host);
            start = end;
        }
    }

    /// Run one straight-line command block with the command VM's flow rules.
    fn run_command_flow(&self, insns: &[&Insn], host: &mut impl ScdHost) {
        let mut pc = 0usize;
        let mut branch_stack: Vec<usize> = Vec::new();
        let mut steps = 0usize;
        while pc < insns.len() {
            steps += 1;
            if steps > MAX_STEPS {
                return;
            }
            let insn = insns[pc];
            let Decoded::Command(op) = &insn.decoded else {
                pc += 1;
                continue;
            };
            let next = pc + 1;
            match op.op {
                0x00 => {
                    if let Some(resume) = branch_stack.pop() {
                        pc = resume;
                        continue;
                    }
                    return;
                }
                0x01 => {
                    let target = if_target(insn);
                    branch_stack.push(self.index_of(insns, target).unwrap_or(insns.len()));
                    pc = next;
                }
                0x02 => {
                    branch_stack.pop();
                    pc = self
                        .index_of(insns, else_target(insn))
                        .unwrap_or(insns.len());
                }
                0x03 => {
                    branch_stack.pop();
                    pc = next;
                }
                _ if op.condition => {
                    if eval_condition(host, insn, op) {
                        pc = next;
                    } else if let Some(resume) = branch_stack.pop() {
                        pc = resume;
                    } else {
                        return;
                    }
                }
                _ => {
                    dispatch_command(host, op, &insn.operands);
                    pc = next;
                }
            }
        }
    }

    fn index_of(&self, insns: &[&Insn], offset: usize) -> Option<usize> {
        insns.iter().position(|insn| insn.offset == offset)
    }

    /// One tick of the `[F8][F9][count]` sleep idiom: the instruction stays at
    /// the same pc and decrements its counter until it reaches zero.
    fn tick_sleep(&mut self, index: usize, insn: &Insn) -> bool {
        if self.slots[index].depth == 0 {
            self.slots[index].sleeping = None;
            self.slots[index].pc = insn.offset + insn.bytes.len();
            return true;
        }
        let top = self.slots[index].depth - 1;
        let counter = self.slots[index].counter_stack[top];
        if counter <= 0 {
            self.slots[index].depth = top;
            self.slots[index].sleeping = None;
            self.slots[index].pc = insn.offset + insn.bytes.len();
            return true;
        }
        let counter = counter - 1;
        self.slots[index].counter_stack[top] = counter;
        if counter == 0 {
            self.slots[index].depth = top;
            self.slots[index].sleeping = None;
            self.slots[index].pc = insn.offset + insn.bytes.len();
            true
        } else {
            self.slots[index].pc = insn.offset;
            false
        }
    }

    fn eval_condition_at(&self, pc: usize, host: &mut impl ScdHost) -> bool {
        match self.insns.get(&pc).copied() {
            Some(insn) => match &insn.decoded {
                Decoded::Command(op) => eval_condition(host, insn, op),
                _ => false,
            },
            None => false,
        }
    }

    fn sleep_count(&self, insn: &Insn) -> i16 {
        if let Some(last) = insn.operands.last() {
            return i16::try_from(last.value).unwrap_or(0);
        }
        if insn.bytes.len() >= 4 {
            return i16::from_le_bytes([insn.bytes[2], insn.bytes[3]]);
        }
        if let Some(next) = self.insns.get(&(insn.offset + 1))
            && next.op == 0xF9
        {
            if let Some(first) = next.operands.first() {
                return i16::try_from(first.value).unwrap_or(0);
            }
            if next.bytes.len() >= 3 {
                return i16::from_le_bytes([next.bytes[1], next.bytes[2]]);
            }
        }
        0
    }

    fn push_depth(&mut self, index: usize) {
        if self.slots[index].depth < EVENT_STACK_SIZE {
            self.slots[index].depth += 1;
        }
    }

    fn push_counter(&mut self, index: usize, value: i16) {
        if self.slots[index].depth < EVENT_STACK_SIZE {
            let depth = self.slots[index].depth;
            self.slots[index].counter_stack[depth] = value;
            self.slots[index].depth = depth + 1;
        }
    }

    fn push_call(&mut self, index: usize, call: usize, ret: usize) {
        if self.slots[index].depth < EVENT_STACK_SIZE {
            let depth = self.slots[index].depth;
            self.slots[index].call_stack[depth] = call;
            self.slots[index].return_stack[depth] = ret;
            self.slots[index].depth = depth + 1;
        }
    }
}

fn eval_condition(host: &mut impl ScdHost, insn: &Insn, op: &'static Op) -> bool {
    if op.op == 0x04 {
        let bank = operand_u8(insn, 0);
        let bit = operand_u8(insn, 1);
        let expected = operand_u8(insn, 2) != 0;
        host.flag_test(bank, bit, expected)
    } else {
        !matches!(
            dispatch_command(host, op, &insn.operands),
            StepResult::Finished
        )
    }
}

fn dispatch_command(host: &mut impl ScdHost, op: &'static Op, operands: &[Operand]) -> StepResult {
    match op.op {
        0x04..=0x08 | 0x31 => host.on_flags(op, operands),
        0x09 | 0x0A | 0x1C | 0x23 | 0x3A | 0x40 | 0x46 => host.on_camera(op, operands),
        0x0B | 0x1E | 0x29 => host.on_message(op, operands),
        0x0C | 0x0D | 0x12 | 0x13 | 0x14 | 0x24 | 0x2D | 0x44 => host.on_room_action(op, operands),
        0x18 | 0x19 | 0x2C | 0x4C => host.on_item(op, operands),
        0x1B | 0x21 | 0x28 | 0x39 | 0x41 => host.on_enemy(op, operands),
        0x20 | 0x2B | 0x33 | 0x45 | 0x4D => host.on_player(op, operands),
        0x1F | 0x30 | 0x34 | 0x35 | 0x36 | 0x3B | 0x47 => host.on_model(op, operands),
        0x2A | 0x3D | 0x3E | 0x42 | 0x48 | 0x4E => host.on_effect(op, operands),
        0x15..=0x17 | 0x27 | 0x2F | 0x37 | 0x43 | 0x4A | 0x4B => host.on_sound(op, operands),
        0x0E | 0x10 | 0x11 | 0x1A | 0x1D | 0x22 | 0x32 | 0x38 | 0x3C | 0x3F | 0x50 => {
            host.on_flow(op, operands)
        }
        _ => host.on_misc(op, operands),
    }
}

fn operand_u8(insn: &Insn, index: usize) -> u8 {
    insn.operands
        .get(index)
        .map(|operand| operand.value.clamp(0, i64::from(u8::MAX)) as u8)
        .unwrap_or_else(|| insn.bytes.get(index + 1).copied().unwrap_or(0))
}

fn if_target(insn: &Insn) -> usize {
    if let Some(target) = insn.operands.first().and_then(|operand| operand.target) {
        return target;
    }
    let skip = insn.operands.first().map_or(0, |operand| operand.value);
    insn.offset.wrapping_add_signed((2 + skip) as isize)
}

fn else_target(insn: &Insn) -> usize {
    if let Some(target) = insn.operands.first().and_then(|operand| operand.target) {
        return target;
    }
    branch_target(insn)
}

fn branch_target(insn: &Insn) -> usize {
    let delta = insn.operands.first().map_or(0, |operand| operand.value);
    insn.offset.wrapping_add_signed(delta as isize)
}

fn payload_len(insn: &Insn) -> usize {
    if let Some(value) = insn.operands.first().map(|operand| operand.value)
        && value > 0
    {
        return value as usize;
    }
    insn.bytes.get(1).map_or(2, |byte| usize::from(*byte))
}

fn block_end(block: &Block) -> usize {
    if block.size > 0 {
        block.offset.saturating_add(usize::from(block.size))
    } else {
        block
            .insns
            .last()
            .map_or(usize::MAX, |insn| insn.offset + insn.bytes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scd::host::PlaceholderHost;
    use crate::scd::ir::{Block, Operand, Scripts, Stream, StreamKind};
    use std::collections::VecDeque;

    macro_rules! op {
        ($name:ident, $value:expr, $mnemonic:expr, $condition:expr) => {
            static $name: Op = Op {
                op: $value,
                mnemonic: $mnemonic,
                operands: "",
                condition: $condition,
                width: None,
            };
        };
    }

    op!(END, 0x00, "end", false);
    op!(IF, 0x01, "if", false);
    op!(ELSE, 0x02, "else", false);
    op!(ENDIF, 0x03, "endif", false);
    op!(CK, 0x04, "ck", true);
    op!(SET, 0x05, "set", false);
    op!(CMPB, 0x06, "cmpb", true);
    op!(CMPW, 0x07, "cmpw", true);
    op!(CUTNEXT, 0x09, "cutnext", false);
    op!(MESSAGE, 0x0B, "message", false);
    op!(DOOR, 0x0C, "door_aot_set", false);
    op!(NOP, 0x0E, "nop", false);
    op!(TESTITEM, 0x10, "testitem", true);
    op!(TESTPICKUP, 0x11, "testpickup", true);
    op!(BGM_PLAY, 0x15, "bgm_play", false);
    op!(ITEM_AOT, 0x18, "item_aot_set", false);
    op!(ITEM_CK, 0x1A, "item_ck", true);
    op!(ENEMY, 0x1B, "enemy", false);
    op!(CK_LAST_ITEM, 0x1D, "ck_last_item", true);
    op!(OBJ, 0x1F, "obj", false);
    op!(DIR_SET, 0x20, "dir_set", false);
    op!(CK_ITEM_COUNT, 0x22, "ck_item_count", true);
    op!(AOT_SWITCH, 0x25, "aot_switch", false);
    op!(EFFECT, 0x2A, "effect", false);
    op!(CK_ANIM, 0x36, "ck_anim", true);
    op!(CK_BITS, 0x38, "ck_bits", true);
    op!(CK_COUNTER, 0x3C, "ck_counter", true);
    op!(CK_TWEEN, 0x3F, "ck_tween", true);
    op!(XA_FLAG_CK, 0x50, "xa_flag_ck", true);

    op!(EVT_NOP, 0x00, "evt_nop", false);
    op!(EVT_ACTOR_BEGIN, 0x01, "evt_actor_begin", false);
    op!(EVT_TWEEN_BEGIN, 0x02, "evt_tween_begin", false);
    op!(EVT_TWEEN_BEGIN_ALT, 0x03, "evt_tween_begin_alt", false);
    op!(EVT_WORK_SET, 0x04, "evt_work_set", false);
    op!(EVT_FORK, 0x05, "evt_fork", false);
    op!(EVT_BLOCK, 0x06, "evt_block", false);
    op!(EVT_SINGLE, 0x07, "evt_single", false);
    op!(EVT_CHAIN, 0x08, "evt_chain", false);
    op!(EVT_DISABLE, 0x09, "evt_disable", false);
    op!(EVT_UNKNOWN, 0x0A, "evt_unknown", false);
    op!(EVT_PUSH_COND, 0xF6, "evt_push_cond", false);
    op!(EVT_SKIP_IF, 0xF7, "evt_skip_if", false);
    op!(EVT_SLEEP, 0xF8, "evt_sleep", false);
    op!(EVT_SLEEP_TICK, 0xF9, "evt_sleep_tick", false);
    op!(EVT_FOR, 0xFA, "evt_for", false);
    op!(EVT_FORNEXT, 0xFB, "evt_fornext", false);
    op!(EVT_DO, 0xFC, "evt_do", false);
    op!(EVT_DOUNTIL, 0xFD, "evt_dountil", false);
    op!(EVT_NEXT, 0xFE, "evt_next", false);
    op!(EVT_FINISH, 0xFF, "evt_finish", false);

    op!(ACT_FLAG_OP, 0x87, "act_flag_op", false);
    op!(ACT_END, 0x8B, "act_end", false);
    op!(TW_NOP, 0x00, "tw_nop", false);
    op!(TW_SET_8A, 0x09, "tw_set_8a", false);
    op!(TW_END, 0x01, "tw_end", false);

    fn command(offset: usize, op: &'static Op, len: usize, operands: Vec<Operand>) -> Insn {
        Insn {
            offset,
            op: op.op,
            bytes: vec![op.op; len],
            decoded: Decoded::Command(op),
            operands,
        }
    }

    fn event(offset: usize, op: &'static Op, len: usize, operands: Vec<Operand>) -> Insn {
        Insn {
            offset,
            op: op.op,
            bytes: vec![op.op; len],
            decoded: Decoded::Event(op),
            operands,
        }
    }

    fn control(offset: usize, op: &'static Op, len: usize, operands: Vec<Operand>) -> Insn {
        Insn {
            offset,
            op: op.op,
            bytes: vec![op.op; len],
            decoded: Decoded::Control(op),
            operands,
        }
    }

    fn actor(offset: usize, op: &'static Op, len: usize, operands: Vec<Operand>) -> Insn {
        Insn {
            offset,
            op: op.op,
            bytes: vec![op.op; len],
            decoded: Decoded::Actor(op),
            operands,
        }
    }

    fn tween(offset: usize, op: &'static Op, len: usize, operands: Vec<Operand>) -> Insn {
        Insn {
            offset,
            op: op.op,
            bytes: vec![op.op; len],
            decoded: Decoded::Tween(op),
            operands,
        }
    }

    fn sub_block(offset: usize, size: u16) -> Insn {
        Insn {
            offset,
            op: 0,
            bytes: vec![0; 2],
            decoded: Decoded::SubBlockHeader { size },
            operands: Vec::new(),
        }
    }

    fn value(value: i64) -> Operand {
        Operand {
            value,
            target: None,
        }
    }

    fn jump(value: i64, target: usize) -> Operand {
        Operand {
            value,
            target: Some(target),
        }
    }

    fn sub_header(offset: usize, size: u16) -> Insn {
        Insn {
            offset,
            op: 0,
            bytes: vec![0, 0],
            decoded: Decoded::SubBlockHeader { size },
            operands: Vec::new(),
        }
    }

    /// One `evt_block` payload block:
    /// `[header][ck][if][bgm_play][endif][end]`.
    fn block_with_if(block: u8, block_offset: usize) -> Vec<Insn> {
        vec![
            event(block_offset, &EVT_BLOCK, 2, vec![value(0x0E)]),
            sub_header(block_offset + 2, 0x0E),
            command(
                block_offset + 4,
                &CK,
                4,
                vec![value(i64::from(block)), value(1), value(0)],
            ),
            command(block_offset + 8, &IF, 2, vec![jump(4, block_offset + 0x0E)]),
            command(block_offset + 0x0A, &BGM_PLAY, 2, vec![value(0)]),
            command(block_offset + 0x0C, &ENDIF, 2, vec![value(0)]),
            command(block_offset + 0x0E, &END, 2, vec![value(0)]),
        ]
    }

    #[test]
    fn event_sleep_tick_without_counter_advances() {
        let scripts = event_scripts(vec![vec![
            control(0x6000, &EVT_SLEEP_TICK, 1, Vec::new()),
            control(0x6001, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        let mut host = RecordingHost::default();
        vm.start(0, 0);
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_block_runs_inline_if_flow() {
        let mut insns = block_with_if(0, 0x5000);
        insns.push(control(0x5010, &EVT_FINISH, 1, Vec::new()));
        let scripts = event_scripts(vec![insns]);
        let mut vm = EventVm::new(&scripts);
        let mut host = RecordingHost::default();
        host.flag_results.push_back(false);
        vm.start(0, 0);
        vm.step(&mut host);
        assert!(
            host.class_names("sound").is_empty(),
            "a false inline condition must skip the body"
        );

        let mut insns = block_with_if(0, 0x5000);
        insns.push(control(0x5010, &EVT_FINISH, 1, Vec::new()));
        let scripts = event_scripts(vec![insns]);
        let mut vm = EventVm::new(&scripts);
        let mut host = RecordingHost::default();
        host.flag_results.push_back(true);
        vm.start(0, 0);
        vm.step(&mut host);
        assert_eq!(host.class_names("sound"), vec!["bgm_play"]);
    }

    fn init_scripts(insns: Vec<Insn>) -> Scripts {
        Scripts {
            init: vec![Block {
                offset: 0,
                size: 0x4000,
                insns,
                trailing: Vec::new(),
            }],
            ..Scripts::default()
        }
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

    #[derive(Default)]
    struct RecordingHost {
        calls: Vec<(&'static str, &'static str)>,
        flag_calls: Vec<(u8, u8, bool)>,
        flag_results: VecDeque<bool>,
        results: VecDeque<StepResult>,
    }

    impl RecordingHost {
        fn class_names(&self, class: &str) -> Vec<&'static str> {
            self.calls
                .iter()
                .filter(|(kind, _)| *kind == class)
                .map(|(_, name)| *name)
                .collect()
        }
    }

    macro_rules! host_method {
        ($name:ident, $class:literal) => {
            fn $name(&mut self, op: &Op, _operands: &[Operand]) -> StepResult {
                self.calls.push(($class, op.mnemonic));
                self.results.pop_front().unwrap_or(StepResult::Placeholder)
            }
        };
    }

    impl ScdHost for RecordingHost {
        host_method!(on_flow, "flow");
        host_method!(on_flags, "flags");
        host_method!(on_camera, "camera");
        host_method!(on_message, "message");
        host_method!(on_room_action, "room_action");
        host_method!(on_item, "item");
        host_method!(on_enemy, "enemy");
        host_method!(on_player, "player");
        host_method!(on_model, "model");
        host_method!(on_effect, "effect");
        host_method!(on_sound, "sound");
        host_method!(on_misc, "misc");

        fn flag_test(&mut self, bank: u8, bit: u8, expected: bool) -> bool {
            self.flag_calls.push((bank, bit, expected));
            self.flag_results.pop_front().unwrap_or(false)
        }
    }

    fn if_else_block() -> Vec<Insn> {
        vec![
            command(0x1000, &IF, 2, vec![jump(0x0A, 0x100C)]),
            command(0x1002, &CK, 4, vec![value(0), value(1), value(0)]),
            command(0x1006, &SET, 4, vec![value(0), value(1), value(0)]),
            command(0x100A, &ELSE, 2, vec![jump(6, 0x1010)]),
            command(0x100C, &BGM_PLAY, 2, vec![value(0)]),
            command(0x100E, &ENDIF, 2, vec![value(0)]),
            command(0x1010, &NOP, 2, vec![value(0)]),
            command(0x1012, &END, 2, vec![value(0)]),
        ]
    }

    #[test]
    fn if_else_true_runs_then_branch() {
        let scripts = init_scripts(if_else_block());
        let mut vm = CommandVm::new(&scripts);
        let mut host = RecordingHost::default();
        host.flag_results.push_back(true);
        vm.run_init(&mut host);
        assert_eq!(host.flag_calls, vec![(0, 1, false)]);
        assert_eq!(host.class_names("flags"), vec!["set"]);
        assert!(host.class_names("sound").is_empty());
        assert_eq!(host.class_names("flow"), vec!["nop"]);
    }

    #[test]
    fn if_else_false_runs_else_branch() {
        let scripts = init_scripts(if_else_block());
        let mut vm = CommandVm::new(&scripts);
        let mut host = RecordingHost::default();
        host.flag_results.push_back(false);
        vm.run_init(&mut host);
        assert!(host.class_names("flags").is_empty());
        assert_eq!(host.class_names("sound"), vec!["bgm_play"]);
        assert_eq!(host.class_names("flow"), vec!["nop"]);
    }

    fn nested_if_block() -> Vec<Insn> {
        vec![
            command(0x1000, &IF, 2, vec![jump(0x14, 0x1016)]),
            command(0x1002, &CK, 4, vec![value(0), value(1), value(0)]),
            command(0x1006, &IF, 2, vec![jump(0x0A, 0x1012)]),
            command(0x1008, &CK, 4, vec![value(0), value(2), value(0)]),
            command(0x100C, &NOP, 2, vec![value(0)]),
            command(0x100E, &NOP, 2, vec![value(0)]),
            command(0x1010, &ENDIF, 2, vec![value(0)]),
            command(0x1012, &NOP, 2, vec![value(0)]),
            command(0x1014, &ENDIF, 2, vec![value(0)]),
            command(0x1016, &END, 2, vec![value(0)]),
        ]
    }

    #[test]
    fn nested_if_both_true_runs_both_bodies() {
        let scripts = init_scripts(nested_if_block());
        let mut vm = CommandVm::new(&scripts);
        let mut host = RecordingHost::default();
        host.flag_results.push_back(true);
        host.flag_results.push_back(true);
        vm.run_init(&mut host);
        assert_eq!(host.class_names("flow"), vec!["nop", "nop", "nop"]);
        assert_eq!(host.flag_calls.len(), 2);
    }

    #[test]
    fn nested_if_inner_false_skips_inner_body() {
        let scripts = init_scripts(nested_if_block());
        let mut vm = CommandVm::new(&scripts);
        let mut host = RecordingHost::default();
        host.flag_results.push_back(true);
        host.flag_results.push_back(false);
        vm.run_init(&mut host);
        assert_eq!(host.class_names("flow"), vec!["nop"]);
    }

    #[test]
    fn nested_if_outer_false_skips_everything() {
        let scripts = init_scripts(nested_if_block());
        let mut vm = CommandVm::new(&scripts);
        let mut host = RecordingHost::default();
        host.flag_results.push_back(false);
        vm.run_init(&mut host);
        assert!(host.calls.is_empty());
        assert_eq!(host.flag_calls.len(), 1);
    }

    #[test]
    fn end_unwinds_branch_stack() {
        let insns = vec![
            command(0x1000, &IF, 2, vec![jump(0x08, 0x100A)]),
            command(0x1002, &CK, 4, vec![value(0), value(1), value(0)]),
            command(0x1006, &END, 2, vec![value(0)]),
            command(0x1008, &SET, 4, vec![value(0), value(1), value(0)]),
            command(0x100A, &NOP, 2, vec![value(0)]),
            command(0x100C, &END, 2, vec![value(0)]),
        ];
        let scripts = init_scripts(insns);
        let mut vm = CommandVm::new(&scripts);
        let mut host = RecordingHost::default();
        host.flag_results.push_back(true);
        vm.run_init(&mut host);
        assert!(host.class_names("flags").is_empty());
        assert_eq!(host.class_names("flow"), vec!["nop"]);
    }

    #[test]
    fn end_at_top_level_ends_block() {
        let insns = vec![
            command(0x1000, &NOP, 2, vec![value(0)]),
            command(0x1002, &END, 2, vec![value(0)]),
            command(0x1004, &SET, 4, vec![value(0), value(1), value(0)]),
        ];
        let scripts = init_scripts(insns);
        let mut vm = CommandVm::new(&scripts);
        let mut host = RecordingHost::default();
        vm.run_init(&mut host);
        assert_eq!(host.class_names("flow"), vec!["nop"]);
        assert!(host.class_names("flags").is_empty());
    }

    #[test]
    fn every_condition_opcode_dispatches() {
        let cases: &[(&'static Op, &str)] = &[
            (&CK, "flag_test"),
            (&CMPB, "flags"),
            (&CMPW, "flags"),
            (&TESTITEM, "flow"),
            (&TESTPICKUP, "flow"),
            (&ITEM_CK, "flow"),
            (&CK_LAST_ITEM, "flow"),
            (&CK_ITEM_COUNT, "flow"),
            (&CK_ANIM, "model"),
            (&CK_BITS, "flow"),
            (&CK_COUNTER, "flow"),
            (&CK_TWEEN, "flow"),
            (&XA_FLAG_CK, "flow"),
        ];
        for (op, class) in cases {
            let insns = vec![
                command(0x1000, &IF, 2, vec![jump(4, 0x1008)]),
                command(0x1002, op, 4, vec![value(0), value(1), value(0)]),
                command(0x1006, &ENDIF, 2, vec![value(0)]),
                command(0x1008, &END, 2, vec![value(0)]),
            ];
            let scripts = init_scripts(insns);
            let mut vm = CommandVm::new(&scripts);
            let mut host = RecordingHost::default();
            host.flag_results.push_back(true);
            host.results.push_back(StepResult::Continue);
            vm.run_init(&mut host);
            if *class == "flag_test" {
                assert_eq!(
                    host.flag_calls.len(),
                    1,
                    "{} should use flag_test",
                    op.mnemonic
                );
                assert!(
                    host.calls.is_empty(),
                    "{} must not dispatch a class",
                    op.mnemonic
                );
            } else {
                assert!(
                    host.calls
                        .iter()
                        .any(|(kind, name)| kind == class && *name == op.mnemonic),
                    "{} should dispatch to {class}",
                    op.mnemonic
                );
                assert!(
                    host.flag_calls.is_empty(),
                    "{} must not use flag_test",
                    op.mnemonic
                );
            }
        }
    }

    #[test]
    fn dispatch_mapping_by_class() {
        let insns = vec![
            command(0x1000, &SET, 4, vec![value(0), value(1), value(0)]),
            command(0x1004, &CUTNEXT, 2, vec![value(0)]),
            command(0x1006, &MESSAGE, 4, vec![value(0), value(0), value(0)]),
            command(0x100A, &DOOR, 26, vec![value(0)]),
            command(0x1024, &ITEM_AOT, 26, vec![value(0)]),
            command(0x103E, &ENEMY, 22, vec![value(0)]),
            command(0x1054, &DIR_SET, 14, vec![value(0)]),
            command(0x1062, &OBJ, 28, vec![value(0)]),
            command(0x107E, &EFFECT, 12, vec![value(0)]),
            command(0x108A, &BGM_PLAY, 2, vec![value(0)]),
            command(0x108C, &AOT_SWITCH, 4, vec![value(0)]),
            command(0x1090, &CK_BITS, 4, vec![value(0), value(0), value(0)]),
            command(0x1094, &NOP, 2, vec![value(0)]),
            command(0x1096, &END, 2, vec![value(0)]),
        ];
        let scripts = init_scripts(insns);
        let mut vm = CommandVm::new(&scripts);
        let mut host = RecordingHost::default();
        vm.run_init(&mut host);
        assert_eq!(
            host.calls,
            vec![
                ("flags", "set"),
                ("camera", "cutnext"),
                ("message", "message"),
                ("room_action", "door_aot_set"),
                ("item", "item_aot_set"),
                ("enemy", "enemy"),
                ("player", "dir_set"),
                ("model", "obj"),
                ("effect", "effect"),
                ("sound", "bgm_play"),
                ("misc", "aot_switch"),
                ("flow", "ck_bits"),
                ("flow", "nop"),
            ]
        );
        assert!(host.flag_calls.is_empty());
    }

    #[test]
    fn missing_jump_target_ends_run_without_panic() {
        let insns = vec![
            command(0x1000, &IF, 2, vec![jump(0x0A, 0xDEAD)]),
            command(0x1002, &CK, 4, vec![value(0), value(1), value(0)]),
        ];
        let scripts = init_scripts(insns);
        let mut vm = CommandVm::new(&scripts);
        let mut host = PlaceholderHost;
        vm.run_init(&mut host);
    }

    #[test]
    fn runaway_loop_is_capped() {
        let insns = vec![
            command(0x1000, &IF, 2, vec![jump(2, 0x1004)]),
            command(0x1002, &ELSE, 2, vec![jump(0, 0x1000)]),
        ];
        let scripts = init_scripts(insns);
        let mut vm = CommandVm::new(&scripts);
        let mut host = PlaceholderHost;
        vm.run_init(&mut host);
    }

    #[test]
    fn event_start_step_finish() {
        let scripts = event_scripts(vec![vec![
            event(0x2000, &EVT_NOP, 1, Vec::new()),
            control(0x2001, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        assert_eq!(vm.active_slots(), 1);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_sleep_counts_down() {
        let scripts = event_scripts(vec![vec![
            control(0x2000, &EVT_SLEEP, 4, vec![value(0xF9), value(2)]),
            control(0x2004, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        // count 2: the first tick decrements to 1 and yields, the second
        // reaches zero and continues into the finish opcode.
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 1);
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_wide_sleep_reads_operand() {
        let scripts = event_scripts(vec![vec![
            control(0x2000, &EVT_SLEEP, 4, vec![value(0xF9), value(1)]),
            control(0x2004, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        // count 1: the single tick reaches zero and continues into finish.
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_for_loop_runs_counter_times() {
        let scripts = event_scripts(vec![vec![
            control(0x3000, &EVT_FOR, 4, vec![value(3)]),
            event(0x3004, &EVT_SINGLE, 2, vec![value(4)]),
            command(0x3006, &NOP, 2, vec![value(0)]),
            control(0x3008, &EVT_FORNEXT, 1, Vec::new()),
            control(0x3009, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
        assert_eq!(host.class_names("flow"), vec!["nop", "nop", "nop"]);
    }

    #[test]
    fn event_do_until_loops_on_true_condition() {
        let scripts = event_scripts(vec![vec![
            control(0x4000, &EVT_DO, 2, vec![jump(6, 0x4006)]),
            command(0x4002, &CK, 4, vec![value(0), value(1), value(0)]),
            event(0x4006, &EVT_SINGLE, 2, vec![value(4)]),
            command(0x4008, &NOP, 2, vec![value(0)]),
            control(0x400A, &EVT_DOUNTIL, 1, Vec::new()),
            control(0x400B, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        host.flag_results.push_back(true);
        host.flag_results.push_back(false);
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
        assert_eq!(host.class_names("flow"), vec!["nop", "nop"]);
        assert_eq!(host.flag_calls.len(), 2);
    }

    #[test]
    fn event_fork_starts_another_slot() {
        let scripts = event_scripts(vec![
            vec![
                event(0x5000, &EVT_FORK, 4, vec![value(1), value(1), value(0)]),
                control(0x5004, &EVT_FINISH, 1, Vec::new()),
            ],
            vec![
                control(0x5100, &EVT_NEXT, 1, Vec::new()),
                control(0x5101, &EVT_FINISH, 1, Vec::new()),
            ],
        ]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 1);
        assert_eq!(host.class_names("misc"), vec!["evt_fork"]);
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_chain_restarts_slot() {
        let scripts = event_scripts(vec![
            vec![
                event(0x6000, &EVT_CHAIN, 2, vec![value(1)]),
                control(0x6002, &EVT_FINISH, 1, Vec::new()),
            ],
            vec![
                event(0x6100, &EVT_NOP, 1, Vec::new()),
                control(0x6101, &EVT_FINISH, 1, Vec::new()),
            ],
        ]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 1);
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_disable_deactivates_named_slot() {
        let scripts = event_scripts(vec![
            vec![
                event(0x6200, &EVT_DISABLE, 2, vec![value(1)]),
                control(0x6202, &EVT_FINISH, 1, Vec::new()),
            ],
            vec![
                event(0x6300, &EVT_NOP, 1, Vec::new()),
                control(0x6301, &EVT_FINISH, 1, Vec::new()),
            ],
        ]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        vm.start(1, 1);
        assert_eq!(vm.active_slots(), 2);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_work_set_stores_entity() {
        let scripts = event_scripts(vec![vec![
            event(0x6400, &EVT_WORK_SET, 3, vec![value(1), value(2), value(0)]),
            control(0x6403, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_actor_state_exits_on_act_end() {
        let scripts = event_scripts(vec![vec![
            event(0x7000, &EVT_ACTOR_BEGIN, 1, Vec::new()),
            actor(0x7001, &ACT_FLAG_OP, 4, vec![value(0), value(64), value(0)]),
            actor(0x7005, &ACT_END, 1, Vec::new()),
            actor(0x7006, &ACT_FLAG_OP, 4, vec![value(0), value(1), value(0)]),
            event(0x700A, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
        assert_eq!(host.class_names("misc"), vec!["act_flag_op", "act_end"]);
    }

    #[test]
    fn event_tween_state_exits_on_tw_end() {
        let scripts = event_scripts(vec![vec![
            event(0x7100, &EVT_TWEEN_BEGIN_ALT, 1, Vec::new()),
            tween(0x7101, &TW_SET_8A, 2, vec![value(96)]),
            tween(0x7103, &TW_END, 1, Vec::new()),
            tween(0x7104, &TW_NOP, 1, Vec::new()),
            event(0x7105, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
        assert_eq!(host.class_names("misc"), vec!["tw_set_8a", "tw_end"]);
    }

    #[test]
    fn event_tween_begin_enters_state_two() {
        let scripts = event_scripts(vec![vec![
            event(0x7200, &EVT_TWEEN_BEGIN, 1, Vec::new()),
            tween(0x7201, &TW_NOP, 1, Vec::new()),
            tween(0x7202, &TW_END, 1, Vec::new()),
            event(0x7203, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
        assert_eq!(host.class_names("misc"), vec!["tw_nop", "tw_end"]);
    }

    #[test]
    fn event_unknown_state0_deactivates() {
        let scripts = event_scripts(vec![vec![event(0x8000, &EVT_UNKNOWN, 1, Vec::new())]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
        assert!(host.calls.is_empty());
    }

    #[test]
    fn event_push_cond_skip_if_yields() {
        let scripts = event_scripts(vec![vec![
            control(0xA100, &EVT_PUSH_COND, 1, Vec::new()),
            control(0xA101, &EVT_SKIP_IF, 1, Vec::new()),
            event(0xA102, &EVT_NOP, 1, Vec::new()),
            control(0xA103, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 1);
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_next_yields_frame() {
        let scripts = event_scripts(vec![vec![
            control(0xA200, &EVT_NEXT, 1, Vec::new()),
            control(0xA201, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 1);
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_block_dispatches_inline_commands() {
        let scripts = event_scripts(vec![vec![
            event(0x9000, &EVT_BLOCK, 2, vec![value(8)]),
            sub_block(0x9002, 4),
            command(0x9004, &SET, 4, vec![value(0), value(1), value(0)]),
            control(0x9008, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
        assert_eq!(host.class_names("flags"), vec!["set"]);
        assert!(host.class_names("misc").is_empty());
    }

    #[test]
    fn event_single_dispatches_inline_command() {
        let scripts = event_scripts(vec![vec![
            event(0xA000, &EVT_SINGLE, 2, vec![value(4)]),
            command(0xA002, &BGM_PLAY, 2, vec![value(0)]),
            control(0xA004, &EVT_FINISH, 1, Vec::new()),
        ]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = RecordingHost::default();
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
        assert_eq!(host.class_names("sound"), vec!["bgm_play"]);
    }

    #[test]
    fn event_missing_pc_deactivates() {
        let scripts = event_scripts(vec![vec![control(
            0xB000,
            &EVT_DO,
            2,
            vec![jump(4, 0xDEAD)],
        )]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(0, 0);
        let mut host = PlaceholderHost;
        vm.step(&mut host);
        assert_eq!(vm.active_slots(), 0);
    }

    #[test]
    fn event_start_invalid_event_is_inert() {
        let scripts = event_scripts(vec![vec![control(0xC000, &EVT_FINISH, 1, Vec::new())]]);
        let mut vm = EventVm::new(&scripts);
        vm.start(3, 9);
        assert_eq!(vm.active_slots(), 0);
        vm.start(9, 0);
        assert_eq!(vm.active_slots(), 1);
    }
}
