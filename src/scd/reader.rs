//! RDT SCD section reader.
//!
//! The init and main procedures are chains of little-endian `u16` size words
//! (each size includes its own two bytes) terminated by a zero word. The event
//! section is a zero-terminated table of `u32` offsets relative to the table
//! itself; every entry is an independent event script whose end is the next
//! table offset, or for the last event the next RDT section if one follows.
//!
//! All offsets in the resulting IR are absolute RDT offsets. Malformed file
//! data never panics: structural failures (bad pointer, truncated table,
//! overrunning block size) return an error, while unknown opcodes simply end
//! the surrounding stream and the remaining bytes become trailing data.

use anyhow::{Result, bail};

use crate::scd::ir::{Block, Decoded, Insn, Operand, Scripts, Stream, StreamKind};
use crate::scd::opcode::{
    Op, actor_op, command_op, command_width, event_control_op, event_top_op, tween_op,
};

/// Offset of the data pointer table in an RDT.
const POINTERS_OFFSET: usize = 0x48;
/// Number of data pointers in the RDT header.
const POINTER_COUNT: usize = 19;
/// Pointer slot of the init SCD container.
const INIT_SLOT: usize = 6;
/// Pointer slot of the main SCD container.
const MAIN_SLOT: usize = 7;
/// Pointer slot of the event SCD table.
const EVENTS_SLOT: usize = 8;
/// Smallest RDT prefix needed to read the three SCD pointers.
const HEADER_MIN: usize = POINTERS_OFFSET + (EVENTS_SLOT + 1) * 4;

/// Parse every SCD stream of an RDT file (init, main, events).
pub fn parse(data: &[u8]) -> Result<Scripts> {
    if data.len() < HEADER_MIN {
        bail!(
            "RDT is too short for an SCD pointer table: need at least {HEADER_MIN} bytes, got {}",
            data.len()
        );
    }
    parse_sections(data)
}

/// Parse only the SCD sections of an RDT whose data pointer table starts at
/// 0x48 (init slot 6, main slot 7, events slot 8). Pointers are direct file
/// offsets; a null pointer means an absent section.
pub fn parse_sections(data: &[u8]) -> Result<Scripts> {
    let init = read_pointer(data, INIT_SLOT)?;
    let main = read_pointer(data, MAIN_SLOT)?;
    let events = read_pointer(data, EVENTS_SLOT)?;

    Ok(Scripts {
        init: parse_container(data, init, "init")?,
        main: parse_container(data, main, "main")?,
        events: parse_events(data, events)?,
    })
}

/// Decode one command-VM block stream starting at `start` (the first opcode,
/// after the size word) and ending at `end`.
pub fn decode_command_block(data: &[u8], start: usize, end: usize) -> (Vec<Insn>, Vec<u8>) {
    let end = end.min(data.len());
    let mut insns = Vec::new();
    let mut pc = start;
    while pc < end {
        let Some(insn) = decode_command_at(data, pc, end) else {
            break;
        };
        pc = insn.offset + insn.bytes.len();
        insns.push(insn);
    }
    let trailing = if pc < end {
        data[pc..end].to_vec()
    } else {
        Vec::new()
    };
    (insns, trailing)
}

/// Decode one event stream `[start, end)`.
///
/// State 0 uses the top-level table, state 1 the actor table and state 2 the
/// tween table; control opcodes 0xF6-0xFF are recognised in every state. An
/// unknown opcode ends the stream and the remaining bytes become trailing.
pub fn decode_event_stream(data: &[u8], start: usize, end: usize) -> (Vec<Insn>, Vec<u8>) {
    let end = end.min(data.len());
    let mut insns = Vec::new();
    let mut pc = start;
    let mut state = 0u8;

    while pc < end {
        let op_byte = data[pc];

        if let Some(control) = event_control_op(op_byte) {
            if op_byte == 0xFC {
                if !decode_evt_do(data, pc, end, control, &mut insns) {
                    break;
                }
                pc += usize::from(data[pc + 1]);
                continue;
            }
            let width = control.width.unwrap_or(1);
            if pc + width > end {
                break;
            }
            let finish = op_byte == 0xFF;
            insns.push(make_insn(
                data,
                pc,
                width,
                control,
                Decoded::Control(control),
            ));
            pc += width;
            // Control ops are valid in every state and keep the current
            // actor/tween sub-ISA; only `finish` returns to state 0.
            if finish {
                state = 0;
                if data[pc..end].iter().all(|&byte| byte == 0) {
                    return (insns, data[pc..end].to_vec());
                }
            }
            continue;
        }

        let table = match state {
            0 => event_top_op(op_byte),
            1 => actor_op(op_byte),
            _ => tween_op(op_byte),
        };
        let Some(op) = table else {
            if state != 0 {
                state = 0;
                continue;
            }
            break;
        };

        if state == 0 && op_byte == 0x06 {
            if !decode_evt_block(data, pc, end, op, &mut insns) {
                break;
            }
            pc += usize::from(data[pc + 1]);
            continue;
        }
        if state == 0 && op_byte == 0x07 {
            if !decode_evt_single(data, pc, end, op, &mut insns) {
                break;
            }
            pc += usize::from(data[pc + 1]);
            continue;
        }

        let width = if state == 1 && op_byte == 0x81 {
            match data.get(pc + 1) {
                Some(flags) if flags & 0x0F != 0 => 10,
                Some(_) => 2,
                None => break,
            }
        } else {
            match op.width {
                Some(width) => width,
                None => break,
            }
        };
        if pc + width > end {
            break;
        }
        let decoded = match state {
            0 => Decoded::Event(op),
            1 => Decoded::Actor(op),
            _ => Decoded::Tween(op),
        };
        insns.push(make_insn(data, pc, width, op, decoded));
        pc += width;
        match (state, op_byte) {
            (0, 0x01) => state = 1,
            (0, 0x02 | 0x03) => state = 2,
            (1, 0x80 | 0x8B) => state = 0,
            (2, 0x01) => state = 0,
            _ => {}
        }
    }

    let trailing = if pc < end {
        data[pc..end].to_vec()
    } else {
        Vec::new()
    };
    (insns, trailing)
}

/// Read pointer slot `slot` from the RDT data pointer table.
fn read_pointer(data: &[u8], slot: usize) -> Result<usize> {
    read_u32(data, POINTERS_OFFSET + slot * 4)
}

/// Read a little-endian `u32` at `at`, failing with the offset when truncated.
fn read_u32(data: &[u8], at: usize) -> Result<usize> {
    let Some(bytes) = data.get(at..at + 4) else {
        bail!("RDT data pointer table is truncated at 0x{at:X}");
    };
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
}

/// Walk a `u16` size-word chain and decode every block until the zero
/// terminator.
fn parse_container(data: &[u8], base: usize, name: &str) -> Result<Vec<Block>> {
    if base == 0 {
        return Ok(Vec::new());
    }
    if base >= data.len() {
        bail!(
            "{name} SCD pointer 0x{base:X} is out of bounds (file size 0x{:X})",
            data.len()
        );
    }
    let mut blocks = Vec::new();
    let mut pos = base;
    loop {
        if pos + 2 > data.len() {
            bail!("{name} SCD size chain at 0x{pos:X} is truncated");
        }
        let size = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if size == 0 {
            break;
        }
        if size < 2 || pos + usize::from(size) > data.len() {
            bail!("{name} SCD block at 0x{pos:X} has invalid size {size}");
        }
        let end = pos + usize::from(size);
        let (insns, trailing) = decode_command_block(data, pos + 2, end);
        blocks.push(Block {
            offset: pos,
            size,
            insns,
            trailing,
        });
        pos = end;
    }
    Ok(blocks)
}

/// Read the event offset table and decode every event script.
fn parse_events(data: &[u8], base: usize) -> Result<Vec<Stream>> {
    if base == 0 {
        return Ok(Vec::new());
    }
    if base >= data.len() {
        bail!(
            "event SCD table pointer 0x{base:X} is out of bounds (file size 0x{:X})",
            data.len()
        );
    }

    let mut starts = Vec::new();
    let mut index = 0usize;
    loop {
        let at = base + index * 4;
        if at + 4 > data.len() {
            bail!("event SCD table at 0x{at:X} is truncated");
        }
        let offset = read_u32(data, at)?;
        if offset == 0 {
            break;
        }
        let Some(start) = base.checked_add(offset).filter(|&start| start < data.len()) else {
            bail!("event SCD offset {offset:#X} at 0x{at:X} is out of bounds");
        };
        if let Some(&previous) = starts.last()
            && start <= previous
        {
            bail!("event SCD offset {offset:#X} at 0x{at:X} is not increasing");
        }
        starts.push(start);
        index += 1;
    }

    if starts.len() > usize::from(u8::MAX) + 1 {
        bail!("event SCD table at 0x{base:X} holds more than 256 events");
    }

    let last_end = starts
        .last()
        .and_then(|&start| next_section_after(data, start))
        .unwrap_or(data.len());

    let mut events = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let end = if index + 1 < starts.len() {
            starts[index + 1]
        } else {
            last_end
        };
        let (insns, trailing) = decode_event_stream(data, start, end);
        events.push(Stream {
            kind: StreamKind::Event(index as u8),
            offset: start,
            insns,
            trailing,
        });
    }
    Ok(events)
}

/// The lowest non-null data pointer greater than `after`, if any. Used to
/// bound the last event script, which has no following event offset.
fn next_section_after(data: &[u8], after: usize) -> Option<usize> {
    let mut best = None;
    for slot in 0..POINTER_COUNT {
        let at = POINTERS_OFFSET + slot * 4;
        let Some(bytes) = data.get(at..at + 4) else {
            break;
        };
        let pointer = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if pointer != 0 && pointer > after && pointer <= data.len() {
            best = Some(match best {
                Some(current) => usize::min(current, pointer),
                None => pointer,
            });
        }
    }
    best
}

/// Decode one command-VM instruction at `pc`, or `None` when the opcode is
/// unknown, is a zero-width dead slot, or the instruction overruns `end`.
fn decode_command_at(data: &[u8], pc: usize, end: usize) -> Option<Insn> {
    let op_byte = *data.get(pc)?;
    let op = command_op(op_byte)?;
    let width = command_width(&data[pc..end])?;
    if width == 0 || pc + width > end {
        return None;
    }
    Some(make_insn(data, pc, width, op, Decoded::Command(op)))
}

/// Build an instruction from `width` bytes at `pc` and decode its operands.
fn make_insn(data: &[u8], pc: usize, width: usize, op: &'static Op, decoded: Decoded) -> Insn {
    let bytes = data[pc..pc + width].to_vec();
    let operands = decode_operands(&bytes, pc, width, op.operands);
    Insn {
        offset: pc,
        op: op.op,
        bytes,
        decoded,
        operands,
    }
}

/// Decode `signature` operands from an instruction whose full byte range is
/// already bounds-checked. Operands that do not fit in `width` are skipped, so
/// a signature may describe the maximum form of a variable-width opcode.
fn decode_operands(bytes: &[u8], pc: usize, width: usize, signature: &str) -> Vec<Operand> {
    let mut operands = Vec::new();
    let mut pos = 1usize;
    for byte in signature.bytes() {
        let size = match byte {
            b'u' | b'b' | b'l' | b'j' | b'p' => 1,
            b'U' | b'I' => 2,
            b'r' => break,
            _ => break,
        };
        if pos + size > width {
            break;
        }
        let value = match byte {
            b'u' | b'p' => i64::from(bytes[pos]),
            b'b' => i64::from(bytes[pos] as i8),
            b'U' => i64::from(u16::from_le_bytes([bytes[pos], bytes[pos + 1]])),
            b'I' => i64::from(i16::from_le_bytes([bytes[pos], bytes[pos + 1]])),
            b'l' | b'j' => i64::from(bytes[pos]),
            _ => break,
        };
        let target = match byte {
            b'l' => Some(if bytes[0] == 0x01 {
                pc + 2 + usize::try_from(value).unwrap_or(0)
            } else {
                pc + usize::try_from(value).unwrap_or(0)
            }),
            b'j' => Some(pc + usize::try_from(value).unwrap_or(0)),
            _ => None,
        };
        operands.push(Operand { value, target });
        pos += size;
    }
    operands
}

/// `evt_block` (0x06): walk the inner `u16` size chain, emit a
/// `SubBlockHeader` for every size word and decode each inner block as
/// command code.
///
/// The operand is one byte short of the chain end, so the chain's terminating
/// zero word is still fully readable but the byte just past it is left for the
/// main loop to decode as the one-byte `evt_nop` the original VM executes.
/// Returns `false` on malformed content so the caller stops with trailing data.
fn decode_evt_block(
    data: &[u8],
    pc: usize,
    end: usize,
    op: &'static Op,
    insns: &mut Vec<Insn>,
) -> bool {
    let Some(&operand) = data.get(pc + 1) else {
        return false;
    };
    let operand = usize::from(operand);
    if operand < 2 {
        return false;
    }
    let Some(chain_end) = pc
        .checked_add(operand)
        .filter(|&chain_end| chain_end <= end)
    else {
        return false;
    };

    let mut decoded = vec![make_insn(data, pc, 2, op, Decoded::Event(op))];
    let mut cursor = pc + 2;
    loop {
        if cursor + 2 > end {
            return false;
        }
        let size = u16::from_le_bytes([data[cursor], data[cursor + 1]]);
        if size == 0 {
            decoded.push(Insn {
                offset: cursor,
                op: data[cursor],
                bytes: vec![data[cursor]],
                decoded: Decoded::SubBlockHeader { size: 0 },
                operands: Vec::new(),
            });
            break;
        }
        if size < 2 || cursor + usize::from(size) >= chain_end {
            return false;
        }
        decoded.push(Insn {
            offset: cursor,
            op: data[cursor],
            bytes: data[cursor..cursor + 2].to_vec(),
            decoded: Decoded::SubBlockHeader { size },
            operands: Vec::new(),
        });
        let body_end = cursor + usize::from(size);
        let (commands, trailing) = decode_command_block(data, cursor + 2, body_end);
        if !trailing.is_empty() {
            return false;
        }
        decoded.extend(commands);
        cursor = body_end;
    }
    insns.append(&mut decoded);
    true
}

/// `evt_single` (0x07): decode exactly one command instruction from the inline
/// payload and advance past the instruction.
fn decode_evt_single(
    data: &[u8],
    pc: usize,
    end: usize,
    op: &'static Op,
    insns: &mut Vec<Insn>,
) -> bool {
    let Some(&operand) = data.get(pc + 1) else {
        return false;
    };
    let operand = usize::from(operand);
    if operand < 2 || pc.checked_add(operand).is_none_or(|next| next > end) {
        return false;
    }
    let bound = pc + operand;
    let Some(mut command) = decode_command_at(data, pc + 2, end) else {
        return false;
    };
    // The VM advances by `operand`, so an inline command whose natural width
    // overruns the payload would overlap the next instruction. Keep the full
    // decoded operands (the original handler reads past the advance) but clamp
    // the stored bytes so instruction ranges never overlap.
    let available = bound - (pc + 2);
    if command.bytes.len() > available {
        command.bytes.truncate(available);
    }
    insns.push(make_insn(data, pc, 2, op, Decoded::Event(op)));
    insns.push(command);
    true
}

/// `evt_do` (0xFC): a forward jump whose target is the instruction after the
/// inline condition command(s) in `[pc + 2, pc + operand)`. The condition code
/// is command-VM code that `evt_dountil` runs from its call stack, so decode it
/// with the command tables. Inline commands that overrun the jump bound keep
/// their decoded operands but have their stored bytes clamped, so instruction
/// byte ranges never overlap.
fn decode_evt_do(
    data: &[u8],
    pc: usize,
    end: usize,
    op: &'static Op,
    insns: &mut Vec<Insn>,
) -> bool {
    let Some(&operand) = data.get(pc + 1) else {
        return false;
    };
    let operand = usize::from(operand);
    if operand < 2 || pc.checked_add(operand).is_none_or(|next| next > end) {
        return false;
    }
    let next = pc + operand;
    insns.push(make_insn(data, pc, 2, op, Decoded::Control(op)));
    let mut cursor = pc + 2;
    while cursor < next {
        let Some(mut command) = decode_command_at(data, cursor, end) else {
            break;
        };
        let available = next - cursor;
        if command.bytes.len() > available {
            command.bytes.truncate(available);
        }
        let consumed = command.bytes.len();
        insns.push(command);
        cursor += consumed;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const INIT_SLOT_OFFSET: usize = POINTERS_OFFSET + INIT_SLOT * 4;
    const MAIN_SLOT_OFFSET: usize = POINTERS_OFFSET + MAIN_SLOT * 4;
    const EVENT_SLOT_OFFSET: usize = POINTERS_OFFSET + EVENTS_SLOT * 4;

    /// Minimal RDT builder: a 0x94-byte header with the section pointers
    /// patched in as data is appended.
    struct RdtBuilder {
        data: Vec<u8>,
    }

    impl RdtBuilder {
        fn new() -> Self {
            Self {
                data: vec![0u8; 0x94],
            }
        }

        fn set_pointer(&mut self, at: usize, value: usize) {
            self.data[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
        }

        fn align4(&mut self) {
            while !self.data.len().is_multiple_of(4) {
                self.data.push(0);
            }
        }

        fn push_section(&mut self, slot_offset: usize, bytes: &[u8]) -> usize {
            self.align4();
            let at = self.data.len();
            self.data.extend_from_slice(bytes);
            self.set_pointer(slot_offset, at);
            at
        }

        fn finish_with_event_sentinel(&mut self) -> Vec<u8> {
            self.align4();
            let at = self.data.len();
            self.data.push(0xAA);
            self.set_pointer(POINTERS_OFFSET + 9 * 4, at);
            self.data.clone()
        }
    }

    fn container(blocks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for body in blocks {
            let size = u16::try_from(body.len() + 2).unwrap();
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(body);
        }
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn event_table(bodies: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![0u8; (bodies.len() + 1) * 4];
        let mut offset = out.len();
        for (index, body) in bodies.iter().enumerate() {
            out[index * 4..index * 4 + 4].copy_from_slice(&(offset as u32).to_le_bytes());
            out.extend_from_slice(body);
            offset += body.len();
        }
        out
    }

    fn bytes(value: &[u8]) -> Vec<u8> {
        value.to_vec()
    }

    #[test]
    fn command_widths_and_labels() {
        let body = bytes(&[
            0x17, 0x01, 0x02, 0x03, 0x03, 0x00, 0x00, 0x00, 0x00, 0x11, // 10 bytes
            0x17, 0x01, 0x02, 0x03, 0x04, 0x00, // 6 bytes
            0x28, 0x00, 0x01, 0x00, 0xAA, 0xBB, // sub 0: 6
            0x28, 0x00, 0x01, 0x01, 0xAA, 0xBB, 0xCC, 0xDD, // sub 1: 8
            0x28, 0x00, 0x01, 0x06, // sub 6: 4
            0x33, 0x00, // sub 0: 2
            0x33, 0x03, 0x04, 0x02, // sub 3: 4
            0x01, 0x04, // if skip=4 -> target = pc + 6
            0x04, 0x00, 0x01, 0x00, // condition
            0x02, 0x02, // else jump=2 -> target = pc + 2
            0x03, 0x00, // endif
        ]);
        let mut builder = RdtBuilder::new();
        builder.push_section(INIT_SLOT_OFFSET, &container(&[&body]));
        builder.push_section(MAIN_SLOT_OFFSET, &container(&[]));
        builder.push_section(EVENT_SLOT_OFFSET, &event_table(&[]));
        let data = builder.finish_with_event_sentinel();

        let scripts = parse(&data).unwrap();
        assert_eq!(scripts.main.len(), 0);
        assert_eq!(scripts.events.len(), 0);
        assert_eq!(scripts.init.len(), 1);

        let block = &scripts.init[0];
        assert_eq!(block.size, body.len() as u16 + 2);
        assert!(block.trailing.is_empty());
        let base = block.offset + 2;
        assert_eq!(block.insns.len(), 11);

        let insn = &block.insns[0];
        assert_eq!(insn.offset, base);
        assert_eq!(
            insn.bytes,
            [0x17, 0x01, 0x02, 0x03, 0x03, 0x00, 0x00, 0x00, 0x00, 0x11]
        );
        assert_eq!(insn.operands.len(), 7);
        assert_eq!(insn.operands[2].value, 3);

        let insn = &block.insns[1];
        assert_eq!(insn.offset, base + 10);
        assert_eq!(insn.bytes.len(), 6);
        assert_eq!(insn.operands.len(), 5);

        assert_eq!(block.insns[2].bytes.len(), 6);
        assert_eq!(block.insns[2].operands.len(), 4);
        assert_eq!(block.insns[3].bytes.len(), 8);
        assert_eq!(block.insns[4].bytes.len(), 4);
        assert_eq!(block.insns[4].operands.len(), 3);
        assert_eq!(block.insns[5].bytes, [0x33, 0x00]);
        assert_eq!(block.insns[5].operands.len(), 1);
        assert_eq!(block.insns[6].bytes, [0x33, 0x03, 0x04, 0x02]);
        assert_eq!(block.insns[6].operands[1].value, 0x0204);

        let if_insn = &block.insns[7];
        assert_eq!(if_insn.decoded, Decoded::Command(command_op(0x01).unwrap()));
        assert_eq!(if_insn.operands[0].value, 4);
        assert_eq!(if_insn.operands[0].target, Some(if_insn.offset + 6));

        let condition = &block.insns[8];
        assert_eq!(condition.operands.len(), 3);
        assert!(matches!(condition.decoded, Decoded::Command(op) if op.condition));

        let else_insn = &block.insns[9];
        assert_eq!(else_insn.operands[0].value, 2);
        assert_eq!(else_insn.operands[0].target, Some(else_insn.offset + 2));
        assert_eq!(block.insns[10].offset, else_insn.offset + 2);
    }

    #[test]
    fn unknown_command_opcode_stops_the_block() {
        let body = bytes(&[0x0E, 0x00, 0xEE, 0x01, 0x02, 0x03]);
        let mut builder = RdtBuilder::new();
        builder.push_section(INIT_SLOT_OFFSET, &container(&[&body]));
        builder.push_section(MAIN_SLOT_OFFSET, &container(&[]));
        builder.push_section(EVENT_SLOT_OFFSET, &event_table(&[]));
        let data = builder.finish_with_event_sentinel();

        let scripts = parse(&data).unwrap();
        let block = &scripts.init[0];
        assert_eq!(block.insns.len(), 1);
        assert_eq!(block.insns[0].op, 0x0E);
        assert_eq!(block.trailing, [0xEE, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn block_chain_walks_every_block() {
        let first = bytes(&[0x0E, 0x00]);
        let second = bytes(&[0x03, 0x00]);
        let mut builder = RdtBuilder::new();
        builder.push_section(INIT_SLOT_OFFSET, &container(&[&first, &second]));
        builder.push_section(MAIN_SLOT_OFFSET, &container(&[]));
        builder.push_section(EVENT_SLOT_OFFSET, &event_table(&[]));
        let data = builder.finish_with_event_sentinel();

        let scripts = parse(&data).unwrap();
        assert_eq!(scripts.init.len(), 2);
        assert_eq!(scripts.init[0].size, 4);
        assert_eq!(scripts.init[1].size, 4);
    }

    #[test]
    fn event_block_decodes_inner_chain_and_stray_nop() {
        let event = bytes(&[
            0x06, 0x0D, // evt_block operand 13
            0x04, 0x00, 0x0E, 0x00, // sub-block size 4, command nop
            0x06, 0x00, 0x0E, 0x00, 0x03, 0x00, // sub-block size 6, nop, endif
            0x00, 0x00, // chain terminator (second byte is the stray nop)
            0xFF, // evt_finish
            0x00, 0x00, // zero padding
        ]);
        let mut builder = RdtBuilder::new();
        builder.push_section(MAIN_SLOT_OFFSET, &container(&[]));
        builder.push_section(EVENT_SLOT_OFFSET, &event_table(&[&event]));
        let data = builder.finish_with_event_sentinel();

        let scripts = parse(&data).unwrap();
        assert_eq!(scripts.events.len(), 1);
        let stream = &scripts.events[0];
        assert_eq!(stream.kind, StreamKind::Event(0));
        assert!(stream.trailing.len() >= 2);
        assert!(stream.trailing.iter().all(|&byte| byte == 0));

        let base = stream.offset;
        let kinds: Vec<(usize, &Decoded)> = stream
            .insns
            .iter()
            .map(|insn| (insn.offset - base, &insn.decoded))
            .collect();
        assert!(matches!(kinds[0].1, Decoded::Event(op) if op.op == 0x06));
        assert_eq!(stream.insns[0].operands[0].value, 13);
        assert_eq!(stream.insns[1].decoded, Decoded::SubBlockHeader { size: 4 });
        assert_eq!(stream.insns[1].offset, base + 2);
        assert_eq!(stream.insns[1].bytes, [0x04, 0x00]);
        assert_eq!(stream.insns[2].offset, base + 4);
        assert_eq!(
            stream.insns[2].decoded,
            Decoded::Command(command_op(0x0E).unwrap())
        );
        assert_eq!(stream.insns[3].decoded, Decoded::SubBlockHeader { size: 6 });
        assert_eq!(stream.insns[3].offset, base + 6);
        assert_eq!(stream.insns[4].offset, base + 8);
        assert_eq!(stream.insns[5].offset, base + 10);
        assert_eq!(
            stream.insns[5].decoded,
            Decoded::Command(command_op(0x03).unwrap())
        );
        assert_eq!(stream.insns[6].decoded, Decoded::SubBlockHeader { size: 0 });
        assert_eq!(stream.insns[6].offset, base + 12);
        assert_eq!(stream.insns[6].bytes, [0x00]);
        assert_eq!(stream.insns[7].offset, base + 13);
        assert_eq!(
            stream.insns[7].decoded,
            Decoded::Event(event_top_op(0x00).unwrap())
        );
        assert_eq!(stream.insns[8].offset, base + 14);
        assert_eq!(
            stream.insns[8].decoded,
            Decoded::Control(event_control_op(0xFF).unwrap())
        );
        assert_eq!(stream.insns.len(), 9);
    }

    #[test]
    fn event_single_do_and_state_transitions() {
        let event = bytes(&[
            0x07, 0x04, 0x0E, 0x00, // evt_single: one nop command
            0xFC, 0x06, 0x04, 0x00, 0x01, 0x00, // evt_do: one condition command
            0x01, // actor begin
            0x84, 0x30, 0x3F, 0x00, // act_anim_flags
            0x80, // act_reset -> state 0
            0x02, // tween begin
            0x05, 0x01, 0x02, 0x03, // tw_set_pos
            0x01, // tw_end -> state 0
            0xFF, // evt_finish
        ]);
        let mut builder = RdtBuilder::new();
        builder.push_section(MAIN_SLOT_OFFSET, &container(&[]));
        builder.push_section(EVENT_SLOT_OFFSET, &event_table(&[&event]));
        let data = builder.finish_with_event_sentinel();

        let scripts = parse(&data).unwrap();
        let stream = &scripts.events[0];
        let base = stream.offset;

        assert_eq!(stream.insns[0].offset, base);
        assert_eq!(stream.insns[0].operands[0].value, 4);
        assert_eq!(stream.insns[1].offset, base + 2);
        assert_eq!(
            stream.insns[1].decoded,
            Decoded::Command(command_op(0x0E).unwrap())
        );

        let do_insn = &stream.insns[2];
        assert_eq!(do_insn.offset, base + 4);
        assert_eq!(do_insn.operands[0].value, 6);
        assert_eq!(do_insn.operands[0].target, Some(base + 10));
        assert_eq!(stream.insns[3].offset, base + 6);
        assert_eq!(
            stream.insns[3].decoded,
            Decoded::Command(command_op(0x04).unwrap())
        );
        assert_eq!(stream.insns[3].operands.len(), 3);

        assert_eq!(stream.insns[4].offset, base + 10);
        assert!(matches!(stream.insns[4].decoded, Decoded::Event(op) if op.op == 0x01));
        assert_eq!(stream.insns[5].offset, base + 11);
        assert!(matches!(stream.insns[5].decoded, Decoded::Actor(op) if op.op == 0x84));
        assert_eq!(stream.insns[5].operands.len(), 3);
        assert_eq!(stream.insns[6].offset, base + 15);
        assert!(matches!(stream.insns[6].decoded, Decoded::Actor(op) if op.op == 0x80));
        assert_eq!(stream.insns[7].offset, base + 16);
        assert!(matches!(stream.insns[7].decoded, Decoded::Event(op) if op.op == 0x02));
        assert_eq!(stream.insns[8].offset, base + 17);
        assert!(matches!(stream.insns[8].decoded, Decoded::Tween(op) if op.op == 0x05));
        assert_eq!(stream.insns[9].offset, base + 21);
        assert!(matches!(stream.insns[9].decoded, Decoded::Tween(op) if op.op == 0x01));
        assert_eq!(stream.insns[10].offset, base + 22);
        assert!(matches!(stream.insns[10].decoded, Decoded::Control(op) if op.op == 0xFF));
        assert_eq!(stream.insns.len(), 11);
    }

    #[test]
    fn actor_motion_uses_dynamic_width() {
        let event = bytes(&[
            0x01, // actor begin
            0x81, 0x01, 0xAA, 0xAA, 0xBB, 0xBB, 0xCC, 0xCC, 0x01, 0x01, // act_motion 10 bytes
            0x81, 0x00, // act_motion 2 bytes
            0x8B, // act_end
            0xFF,
        ]);
        let mut builder = RdtBuilder::new();
        builder.push_section(MAIN_SLOT_OFFSET, &container(&[]));
        builder.push_section(EVENT_SLOT_OFFSET, &event_table(&[&event]));
        let data = builder.finish_with_event_sentinel();

        let scripts = parse(&data).unwrap();
        let stream = &scripts.events[0];
        assert_eq!(stream.insns[1].bytes.len(), 10);
        assert_eq!(stream.insns[1].operands.len(), 6);
        assert_eq!(stream.insns[2].bytes.len(), 2);
        assert_eq!(stream.insns[2].operands.len(), 1);
        assert!(matches!(stream.insns[3].decoded, Decoded::Actor(op) if op.op == 0x8B));
    }

    #[test]
    fn unknown_event_opcode_stops_the_stream() {
        let event = bytes(&[0x04, 0x01, 0x02, 0x0A, 0xEE, 0xEE]);
        let mut builder = RdtBuilder::new();
        builder.push_section(MAIN_SLOT_OFFSET, &container(&[]));
        builder.push_section(EVENT_SLOT_OFFSET, &event_table(&[&event]));
        let data = builder.finish_with_event_sentinel();

        let scripts = parse(&data).unwrap();
        let stream = &scripts.events[0];
        assert_eq!(stream.insns.len(), 1);
        assert_eq!(&stream.trailing[..3], [0x0A, 0xEE, 0xEE]);
        assert!(stream.trailing[3..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn actor_unknown_byte_falls_back_to_state_zero() {
        let event = bytes(&[
            0x01, // actor begin
            0x84, 0x01, 0x02, 0x03, // act_anim_flags
            0x07, 0x04, 0x0E, 0x00, // unknown actor op -> top-level evt_single
            0xFF,
        ]);
        let mut builder = RdtBuilder::new();
        builder.push_section(MAIN_SLOT_OFFSET, &container(&[]));
        builder.push_section(EVENT_SLOT_OFFSET, &event_table(&[&event]));
        let data = builder.finish_with_event_sentinel();

        let scripts = parse(&data).unwrap();
        let stream = &scripts.events[0];
        assert!(stream.trailing.iter().all(|&byte| byte == 0));
        assert!(matches!(stream.insns[2].decoded, Decoded::Event(op) if op.op == 0x07));
        assert!(matches!(stream.insns[3].decoded, Decoded::Command(op) if op.op == 0x0E));
    }

    #[test]
    fn rejects_stub_and_truncated_pointers() {
        assert!(parse(&[0, 0, 0, 0]).is_err());
        assert!(parse_sections(&[0; HEADER_MIN - 1]).is_err());

        let mut builder = RdtBuilder::new();
        builder.set_pointer(INIT_SLOT_OFFSET, 0x1000);
        let data = builder.finish_with_event_sentinel();
        assert!(parse(&data).is_err());
    }

    #[test]
    fn event_offsets_are_relative_to_the_table() {
        let event = bytes(&[0x00, 0xFF]);
        let mut builder = RdtBuilder::new();
        builder.push_section(MAIN_SLOT_OFFSET, &container(&[]));
        builder.push_section(EVENT_SLOT_OFFSET, &event_table(&[&event]));
        let data = builder.finish_with_event_sentinel();

        let scripts = parse(&data).unwrap();
        let base = read_pointer(&data, EVENTS_SLOT).unwrap();
        assert_eq!(scripts.events[0].offset, base + 8);
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn parses_real_rooms_1000_and_1001() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let stage = Path::new(&root).join("JPN/STAGE1");

        let data = std::fs::read(stage.join("ROOM1000.RDT")).unwrap();
        let scripts = parse(&data).unwrap();
        assert_eq!(scripts.init.len(), 1);
        assert_eq!(scripts.init[0].size, 786);
        assert_eq!(
            scripts.init[0].offset + usize::from(scripts.init[0].size),
            0x10F66
        );
        assert!(scripts.main.is_empty());
        assert_eq!(scripts.events.len(), 59);

        let data = std::fs::read(stage.join("ROOM1001.RDT")).unwrap();
        let scripts = parse(&data).unwrap();
        assert_eq!(scripts.events.len(), 3);
        assert_eq!(scripts.main.len(), 1);
        assert_eq!(scripts.main[0].size, 22);
        assert_eq!(
            scripts.main[0].offset + usize::from(scripts.main[0].size),
            0x10D9E
        );
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn parses_every_real_rdt() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let mut files = Vec::new();
        collect_rdts(Path::new(&root), &mut files);
        files.sort();
        assert!(!files.is_empty(), "no RDT files found under {root}");

        let mut parsed = 0usize;
        let mut stubs = 0usize;
        let mut block_count = 0usize;
        let mut event_count = 0usize;
        let mut insn_count = 0usize;

        for path in &files {
            let data = std::fs::read(path).unwrap();
            if data.len() <= 4 {
                stubs += 1;
                continue;
            }
            let scripts =
                parse(&data).unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
            for block in scripts.init.iter().chain(&scripts.main) {
                assert!(block.offset + usize::from(block.size) <= data.len());
                assert!(
                    block.trailing.is_empty(),
                    "{}: block at 0x{:X} has trailing bytes",
                    path.display(),
                    block.offset
                );
                for insn in &block.insns {
                    assert!(insn.offset + insn.bytes.len() <= data.len());
                    assert!(insn.offset >= block.offset + 2);
                }
                block_count += 1;
                insn_count += block.insns.len();
            }
            for stream in &scripts.events {
                for insn in &stream.insns {
                    assert!(insn.offset + insn.bytes.len() <= data.len());
                    assert!(insn.offset >= stream.offset);
                }
                event_count += 1;
                insn_count += stream.insns.len();
            }
            parsed += 1;
        }

        println!(
            "parsed {parsed} RDTs ({stubs} stubs skipped): {block_count} blocks, \
             {event_count} events, {insn_count} instructions"
        );
    }

    fn collect_rdts(dir: &Path, out: &mut Vec<PathBuf>) {
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
