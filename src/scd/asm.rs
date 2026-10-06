//! Text-SCD assembler: `.s` source to SCD IR and standalone `.scd` containers.
//!
//! The assembler consumes exactly the grammar [`crate::scd::disasm::render`]
//! emits: `.version 1`; `.init`/`.main`/`.event event_XX` section headers with
//! events dense ascending from `event_00`; `.block` before every init/main
//! block; `off_XXXX:` label definitions and `off_XXXX + delta` references; `db`
//! trailing chunks; and every mnemonic the disassembler prints, including the
//! named operand constants, the bit-packed `hi | lo` actor forms and the
//! variable-width commands.
//!
//! Encoding is two passes. The first lays every item out, computes the block
//! and payload sizes and collects the label positions; the second re-encodes
//! each instruction with resolved label operands. Every encoded stream is then
//! fed back through the reader and must re-decode to the planned instruction
//! sequence with no lost or invented bytes. Unencodable input fails with the
//! line, column and mnemonic; the assembler never panics and never falls back
//! to raw bytes.
//!
//! [`Assembled::to_container`] lays the streams out behind the RDT data
//! pointer ABI (slots 6/7/8 at 0x48) as a standalone `.scd` the existing
//! reader parses.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::scd::decomp::constants;
use crate::scd::disasm::{self, EventState};
use crate::scd::ir::{Block, Decoded, Insn, Stream, StreamKind};
use crate::scd::opcode::{
    COMMAND_OPS, Op, actor_op, command_width, event_control_op, event_top_op, tween_op,
};
use crate::scd::reader;

/// Offset of the data pointer table in the standalone container (RDT ABI).
const POINTERS_OFFSET: usize = 0x48;
/// Pointer slot of the init SCD container.
const INIT_SLOT: usize = 6;
/// Pointer slot of the main SCD container.
const MAIN_SLOT: usize = 7;
/// Pointer slot of the event SCD table.
const EVENTS_SLOT: usize = 8;
/// Number of data pointers in the RDT pointer table.
const POINTER_COUNT: usize = 19;
/// Header size: the full zeroed pointer table, so no data byte is ever
/// misread as a section pointer (the reader scans all 19 slots).
const CONTAINER_HEADER: usize = POINTERS_OFFSET + POINTER_COUNT * 4;

/// The assembled streams of one `.s` source.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Assembled {
    /// Blocks of the init procedure container.
    pub init: Vec<Block>,
    /// Blocks of the main procedure container.
    pub main: Vec<Block>,
    /// Event scripts in table order.
    pub events: Vec<Stream>,
}

impl Assembled {
    /// Lay the streams out behind the RDT pointer-table ABI (0x48, slots
    /// 6/7/8) and return a standalone container the existing reader parses.
    ///
    /// Sections are 4-byte aligned, the init/main containers end in their own
    /// zero size word and the event table is zero-terminated with offsets
    /// relative to the table itself.
    pub fn to_container(&self) -> Result<Vec<u8>> {
        let init_bodies = self
            .init
            .iter()
            .map(block_body)
            .collect::<Result<Vec<_>>>()?;
        let main_bodies = self
            .main
            .iter()
            .map(block_body)
            .collect::<Result<Vec<_>>>()?;
        let event_bodies = self
            .events
            .iter()
            .map(event_body)
            .collect::<Result<Vec<_>>>()?;
        let layout = container_layout(&init_bodies, &main_bodies, &event_bodies)?;

        let mut out = vec![0u8; layout.total];
        if let Some(base) = layout.init_base {
            write_slot(&mut out, INIT_SLOT, base)?;
            write_container(&mut out, &layout.init_blocks, &init_bodies);
        }
        if let Some(base) = layout.main_base {
            write_slot(&mut out, MAIN_SLOT, base)?;
            write_container(&mut out, &layout.main_blocks, &main_bodies);
        }
        if let Some(base) = layout.events_base {
            write_slot(&mut out, EVENTS_SLOT, base)?;
            for (index, &start) in layout.events.iter().enumerate() {
                let offset = u32::try_from(start - base)
                    .map_err(|_| anyhow::anyhow!("event table offset does not fit 32 bits"))?;
                out[base + index * 4..base + index * 4 + 4].copy_from_slice(&offset.to_le_bytes());
            }
            for (&start, body) in layout.events.iter().zip(&event_bodies) {
                out[start..start + body.len()].copy_from_slice(body);
            }
        }
        Ok(out)
    }
}

/// Assemble `.s` source into the SCD IR.
pub fn assemble(text: &str) -> Result<Assembled> {
    let source = parse_source(text)?;

    let mut finished: Vec<Finished> = Vec::new();
    for block in &source.init {
        finished.push(finish_block(block, false)?);
    }
    let init_count = finished.len();
    for block in &source.main {
        finished.push(finish_block(block, false)?);
    }
    let main_count = finished.len() - init_count;
    for items in &source.events {
        finished.push(finish_block(items, true)?);
    }

    let init_bodies: Vec<Vec<u8>> = finished[..init_count]
        .iter()
        .map(|stream| stream.body.clone())
        .collect();
    let main_bodies: Vec<Vec<u8>> = finished[init_count..init_count + main_count]
        .iter()
        .map(|stream| stream.body.clone())
        .collect();
    let event_bodies: Vec<Vec<u8>> = finished[init_count + main_count..]
        .iter()
        .map(|stream| stream.body.clone())
        .collect();
    let layout = container_layout(&init_bodies, &main_bodies, &event_bodies)?;

    let mut assembled = Assembled::default();
    for (index, stream) in finished[..init_count].iter().enumerate() {
        assembled.init.push(Block {
            offset: layout.init_blocks[index],
            size: u16::try_from(stream.body.len() + 2)
                .map_err(|_| anyhow::anyhow!("init block at line {} is too large", stream.line))?,
            insns: rebase(stream.insns.clone(), layout.init_blocks[index] + 2),
            trailing: stream.trailing.clone(),
        });
    }
    for (index, stream) in finished[init_count..init_count + main_count]
        .iter()
        .enumerate()
    {
        assembled.main.push(Block {
            offset: layout.main_blocks[index],
            size: u16::try_from(stream.body.len() + 2)
                .map_err(|_| anyhow::anyhow!("main block at line {} is too large", stream.line))?,
            insns: rebase(stream.insns.clone(), layout.main_blocks[index] + 2),
            trailing: stream.trailing.clone(),
        });
    }
    if layout.events_base.is_some() {
        for (index, stream) in finished[init_count + main_count..].iter().enumerate() {
            let start = layout.events[index];
            assembled.events.push(Stream {
                kind: StreamKind::Event(u8::try_from(index).map_err(|_| {
                    anyhow::anyhow!("more than 256 event scripts cannot be assembled")
                })?),
                offset: start,
                insns: rebase(stream.insns.clone(), start),
                trailing: stream.trailing.clone(),
            });
        }
    }
    Ok(assembled)
}

/// Add `base` to every instruction offset and jump target of `insns`.
fn rebase(mut insns: Vec<Insn>, base: usize) -> Vec<Insn> {
    for insn in &mut insns {
        insn.offset += base;
        for operand in &mut insn.operands {
            if let Some(target) = &mut operand.target {
                *target += base;
            }
        }
    }
    insns
}

/// The container-relative position of every section, block and event stream.
struct ContainerLayout {
    init_base: Option<usize>,
    init_blocks: Vec<usize>,
    main_base: Option<usize>,
    main_blocks: Vec<usize>,
    events_base: Option<usize>,
    events: Vec<usize>,
    total: usize,
}

/// Lay out the section bodies after the pointer-table header and validate the
/// size words and the event table.
fn container_layout(
    init: &[Vec<u8>],
    main: &[Vec<u8>],
    events: &[Vec<u8>],
) -> Result<ContainerLayout> {
    let mut layout = ContainerLayout {
        init_base: None,
        init_blocks: Vec::new(),
        main_base: None,
        main_blocks: Vec::new(),
        events_base: None,
        events: Vec::new(),
        total: CONTAINER_HEADER,
    };
    let mut pos = CONTAINER_HEADER;
    for (label, bodies) in [("init", init), ("main", main)] {
        if bodies.is_empty() {
            continue;
        }
        align4(&mut pos);
        let base = pos;
        let blocks = if label == "init" {
            &mut layout.init_blocks
        } else {
            &mut layout.main_blocks
        };
        for body in bodies {
            u16::try_from(body.len() + 2)
                .map_err(|_| anyhow::anyhow!("{label} block is larger than a 16-bit size word"))?;
            blocks.push(pos);
            pos += 2 + body.len();
        }
        pos += 2;
        if label == "init" {
            layout.init_base = Some(base);
        } else {
            layout.main_base = Some(base);
        }
    }
    if !events.is_empty() {
        align4(&mut pos);
        let base = pos;
        pos += (events.len() + 1) * 4;
        for body in events {
            if body.is_empty() {
                bail!("an empty event script cannot be represented in a container");
            }
            layout.events.push(pos);
            pos += body.len();
        }
        layout.events_base = Some(base);
    }
    layout.total = pos;
    Ok(layout)
}

/// Pad `pos` up to the next multiple of four.
fn align4(pos: &mut usize) {
    while !pos.is_multiple_of(4) {
        *pos += 1;
    }
}

/// Write a 32-bit little-endian section pointer into the header.
fn write_slot(out: &mut [u8], slot: usize, value: usize) -> Result<()> {
    let value =
        u32::try_from(value).map_err(|_| anyhow::anyhow!("container is larger than 4 GiB"))?;
    let at = POINTERS_OFFSET + slot * 4;
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Copy every block's `u16` size word, body and the chain terminator.
fn write_container(out: &mut [u8], blocks: &[usize], bodies: &[Vec<u8>]) {
    for (&start, body) in blocks.iter().zip(bodies) {
        let size = u16::try_from(body.len() + 2).unwrap_or(0);
        out[start..start + 2].copy_from_slice(&size.to_le_bytes());
        out[start + 2..start + 2 + body.len()].copy_from_slice(body);
    }
    if let Some(&last) = blocks.last() {
        let terminator = last + 2 + bodies.last().map_or(0, Vec::len);
        out[terminator..terminator + 2].copy_from_slice(&0u16.to_le_bytes());
    }
}

/// Concatenate one command-VM block's instruction and trailing bytes.
fn block_body(block: &Block) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for insn in &block.insns {
        body.extend_from_slice(&insn.bytes);
    }
    body.extend_from_slice(&block.trailing);
    u16::try_from(body.len() + 2)
        .map_err(|_| anyhow::anyhow!("a block body is larger than 16 bits"))?;
    Ok(body)
}

/// Concatenate one event stream's instruction and trailing bytes.
fn event_body(stream: &Stream) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for insn in &stream.insns {
        body.extend_from_slice(&insn.bytes);
    }
    body.extend_from_slice(&stream.trailing);
    Ok(body)
}

// ---------------------------------------------------------------------------
// Source parsing
// ---------------------------------------------------------------------------

/// One source-level operand expression.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Arg {
    /// A plain integer (`31`, `0x1F`, `-3`).
    Num(i64),
    /// A bit-packed actor form (`32 | 16`).
    Bits(i64, i64),
    /// A named constant or label.
    Symbol(String),
    /// A label reference with a byte delta (`off_0302 + 3`).
    Label(String, i64),
}

/// One instruction source line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InsnSrc {
    mnemonic: String,
    args: Vec<Arg>,
    line: usize,
    col: usize,
}

/// One parsed line item.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ItemKind {
    Insn(InsnSrc),
    Db(Vec<u8>),
}

/// An item plus the labels defined on its offset.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    labels: Vec<String>,
    kind: ItemKind,
    line: usize,
    col: usize,
}

/// Which section is currently being parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Init,
    Main,
    Event(u8),
}

/// The parsed `.s` source: init/main blocks and event item streams.
#[derive(Debug, Default)]
struct Source {
    init: Vec<Vec<Item>>,
    main: Vec<Vec<Item>>,
    events: Vec<Vec<Item>>,
}

/// Parse every line of `text` into sections, blocks and items.
fn parse_source(text: &str) -> Result<Source> {
    let mut source = Source::default();
    let mut version_seen = false;
    let mut init_seen = false;
    let mut main_seen = false;
    let mut current: Option<Section> = None;
    let mut items: Vec<Item> = Vec::new();
    let mut block_started = false;
    let mut pending_labels: Vec<String> = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let indent = raw.len() - raw.trim_start().len();
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix('.') {
            if !pending_labels.is_empty() {
                bail!(
                    "line {line_no}, column {}: label `{}` is not followed by an instruction",
                    indent + 1,
                    pending_labels[0]
                );
            }
            let (directive, argument) = match rest.split_once(char::is_whitespace) {
                Some((directive, argument)) => (directive, argument.trim()),
                None => (rest, ""),
            };
            match directive {
                "version" => {
                    if version_seen {
                        bail!("line {line_no}, column {}: duplicate .version", indent + 1);
                    }
                    if current.is_some() {
                        bail!(
                            "line {line_no}, column {}: .version must precede every section",
                            indent + 1
                        );
                    }
                    if argument != "1" {
                        bail!(
                            "line {line_no}, column {}: expected `.version 1`, found `{line}`",
                            indent + 1
                        );
                    }
                    version_seen = true;
                }
                "init" => {
                    require_version(version_seen, line_no, indent, line)?;
                    flush_items(&mut source, current, &mut items, block_started);
                    block_started = false;
                    if init_seen {
                        bail!("line {line_no}, column {}: duplicate .init", indent + 1);
                    }
                    init_seen = true;
                    current = Some(Section::Init);
                }
                "main" => {
                    require_version(version_seen, line_no, indent, line)?;
                    flush_items(&mut source, current, &mut items, block_started);
                    block_started = false;
                    if main_seen {
                        bail!("line {line_no}, column {}: duplicate .main", indent + 1);
                    }
                    main_seen = true;
                    current = Some(Section::Main);
                }
                "event" => {
                    require_version(version_seen, line_no, indent, line)?;
                    flush_items(&mut source, current, &mut items, block_started);
                    block_started = false;
                    let expected = u8::try_from(source.events.len())
                        .map_err(|_| anyhow::anyhow!("line {line_no}: too many event scripts"))?;
                    let name = format!("event_{expected:02X}");
                    if argument != name {
                        bail!(
                            "line {line_no}, column {}: expected `.event {name}`, found `{line}`",
                            indent + 1
                        );
                    }
                    current = Some(Section::Event(expected));
                    source.events.push(Vec::new());
                }
                "block" => {
                    require_version(version_seen, line_no, indent, line)?;
                    match current {
                        Some(Section::Init) => {
                            if block_started {
                                source.init.push(std::mem::take(&mut items));
                            }
                            items.clear();
                            block_started = true;
                        }
                        Some(Section::Main) => {
                            if block_started {
                                source.main.push(std::mem::take(&mut items));
                            }
                            items.clear();
                            block_started = true;
                        }
                        Some(Section::Event(_)) => bail!(
                            "line {line_no}, column {}: .block is only valid in .init/.main",
                            indent + 1
                        ),
                        None => bail!(
                            "line {line_no}, column {}: .block before any section header",
                            indent + 1
                        ),
                    }
                }
                other => bail!(
                    "line {line_no}, column {}: unknown directive `.{other}`",
                    indent + 1
                ),
            }
            continue;
        }

        if !version_seen {
            bail!(
                "line {line_no}, column {}: missing `.version 1` before the first instruction",
                indent + 1
            );
        }
        let Some(_) = current else {
            bail!(
                "line {line_no}, column {}: instruction outside .init/.main/.event",
                indent + 1
            );
        };

        // A label definition may share its line with an instruction.
        let mut rest = line;
        let mut rest_col = indent;
        while let Some((name, tail)) = split_label_definition(rest) {
            pending_labels.push(name.to_string());
            let consumed = rest.len() - tail.len();
            rest = tail.trim_start();
            rest_col += consumed + (tail.len() - tail.trim_start().len());
        }
        if rest.is_empty() {
            continue;
        }

        let mnemonic_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let mnemonic = &rest[..mnemonic_end];
        let column = rest_col + 1;
        if matches!(current, Some(Section::Init | Section::Main)) {
            block_started = true;
        }
        if mnemonic == "db" {
            let payload = parse_db_args(rest, mnemonic_end, line_no, rest_col)?;
            items.push(Item {
                labels: std::mem::take(&mut pending_labels),
                kind: ItemKind::Db(payload),
                line: line_no,
                col: column,
            });
            continue;
        }
        if !is_identifier(mnemonic) {
            bail!("line {line_no}, column {column}: invalid mnemonic `{mnemonic}`");
        }
        let args = parse_args(rest, mnemonic_end, line_no, rest_col)?;
        items.push(Item {
            labels: std::mem::take(&mut pending_labels),
            kind: ItemKind::Insn(InsnSrc {
                mnemonic: mnemonic.to_string(),
                args,
                line: line_no,
                col: column,
            }),
            line: line_no,
            col: column,
        });
    }

    if !pending_labels.is_empty() {
        bail!(
            "line {}: label `{}` is not followed by an instruction",
            text.lines().count(),
            pending_labels[0]
        );
    }
    flush_items(&mut source, current, &mut items, block_started);
    if !version_seen {
        bail!("missing `.version 1`");
    }
    Ok(source)
}

/// Move the current section's items into place and reset the accumulator.
fn flush_items(
    source: &mut Source,
    current: Option<Section>,
    items: &mut Vec<Item>,
    block_started: bool,
) {
    match current {
        Some(Section::Init) if block_started || !items.is_empty() => {
            source.init.push(std::mem::take(items));
        }
        Some(Section::Main) if block_started || !items.is_empty() => {
            source.main.push(std::mem::take(items));
        }
        Some(Section::Event(_)) => {
            if let Some(last) = source.events.last_mut() {
                last.append(items);
            }
        }
        _ => {}
    }
}

/// Reject content before the version directive.
fn require_version(seen: bool, line: usize, indent: usize, text: &str) -> Result<()> {
    if !seen {
        bail!(
            "line {line}, column {}: `.version 1` must precede `{text}`",
            indent + 1
        );
    }
    Ok(())
}

/// Split `name:` off the front of a line, if it defines a label.
fn split_label_definition(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let name = &line[..colon];
    if name.is_empty() || !is_identifier(name) {
        return None;
    }
    Some((name, &line[colon + 1..]))
}

/// True when `text` is an identifier (label or constant name).
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Parse the comma-separated argument list starting at byte `start`.
fn parse_args(line: &str, start: usize, line_no: usize, col_base: usize) -> Result<Vec<Arg>> {
    let bytes = line.as_bytes();
    let mut args = Vec::new();
    let mut index = start;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let arg_start = index;
        while index < bytes.len() && bytes[index] != b',' {
            index += 1;
        }
        let piece = &line[arg_start..index];
        args.push(parse_arg(piece, line_no, col_base + arg_start + 1)?);
        if index < bytes.len() {
            index += 1;
            if index >= bytes.len() || line[index..].trim().is_empty() {
                bail!(
                    "line {line_no}, column {}: trailing comma after the last argument",
                    col_base + index + 1
                );
            }
        }
    }
    Ok(args)
}

/// Parse one operand expression.
fn parse_arg(piece: &str, line_no: usize, column: usize) -> Result<Arg> {
    let text = piece.trim();
    if text.is_empty() {
        bail!("line {line_no}, column {column}: empty argument");
    }
    if let Some(value) = parse_int(text) {
        return Ok(Arg::Num(value));
    }
    if let Some((left, right)) = text.split_once('|') {
        let hi = parse_int(left.trim()).ok_or_else(|| {
            anyhow::anyhow!("line {line_no}, column {column}: invalid bit field `{left}`")
        })?;
        let lo = parse_int(right.trim()).ok_or_else(|| {
            anyhow::anyhow!("line {line_no}, column {column}: invalid bit field `{right}`")
        })?;
        return Ok(Arg::Bits(hi, lo));
    }
    if let Some((name, rest)) = split_symbol_and_operator(text) {
        let delta = parse_int(rest.trim()).ok_or_else(|| {
            anyhow::anyhow!("line {line_no}, column {column}: invalid label delta `{rest}`")
        })?;
        return Ok(Arg::Label(name.to_string(), delta));
    }
    if is_identifier(text) {
        return Ok(Arg::Symbol(text.to_string()));
    }
    bail!("line {line_no}, column {column}: invalid argument `{text}`")
}

/// Split `off_XXXX + 3` into its identifier and the operator + delta text.
fn split_symbol_and_operator(text: &str) -> Option<(&str, &str)> {
    let end = text.find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))?;
    let name = &text[..end];
    if name.is_empty() || !is_identifier(name) {
        return None;
    }
    let rest = text[end..].trim_start();
    if rest.starts_with('+') || rest.starts_with('-') {
        Some((name, rest))
    } else {
        None
    }
}

/// Parse a decimal or `0x`-prefixed integer, optionally negative.
fn parse_int(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let rest = rest.trim();
    let magnitude = if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        i64::from_str_radix(hex.trim(), 16).ok()?
    } else {
        rest.parse::<i64>().ok()?
    };
    Some(if negative { -magnitude } else { magnitude })
}

/// Parse the byte list of a `db` pseudo-instruction.
fn parse_db_args(line: &str, start: usize, line_no: usize, col_base: usize) -> Result<Vec<u8>> {
    let args = parse_args(line, start, line_no, col_base)?;
    if args.is_empty() {
        bail!(
            "line {line_no}, column {}: `db` needs at least one byte",
            col_base + 1
        );
    }
    let mut bytes = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            Arg::Num(value) => bytes.push(range_u8(value, line_no, col_base + 1)?),
            _ => bail!(
                "line {line_no}, column {}: `db` takes byte values",
                col_base + 1
            ),
        }
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// A planned instruction or data item with its offset contribution.
#[derive(Debug, Clone)]
struct PlannedItem {
    labels: Vec<String>,
    line: usize,
    col: usize,
    kind: PlannedKind,
}

/// The encoded form of one source item.
#[derive(Debug, Clone)]
enum PlannedKind {
    /// A command-VM or event-VM instruction, re-encoded with labels later.
    Insn {
        src: InsnSrc,
        decoded: Decoded,
        limit: Option<usize>,
        size: usize,
    },
    /// An `evt_block` header whose payload length was rebuilt from its chain.
    EvtBlock { src: InsnSrc, chain: usize },
    /// An `evt_single` header; the following item is the inline command.
    EvtSingle { src: InsnSrc, payload: usize },
    /// An `evt_do` header plus the label it jumps over.
    EvtDo {
        src: InsnSrc,
        label: String,
        delta: i64,
    },
    /// One `evt_block_sub` size word (or its one-byte zero terminator).
    SubBlock { size: u16 },
    /// Raw trailing bytes.
    Db(Vec<u8>),
}

impl PlannedItem {
    fn size(&self) -> usize {
        match &self.kind {
            PlannedKind::Insn { size, .. } => *size,
            PlannedKind::EvtBlock { .. }
            | PlannedKind::EvtSingle { .. }
            | PlannedKind::EvtDo { .. } => 2,
            PlannedKind::SubBlock { size } => {
                if *size == 0 {
                    1
                } else {
                    2
                }
            }
            PlannedKind::Db(bytes) => bytes.len(),
        }
    }

    fn decoded(&self) -> Option<Decoded> {
        match &self.kind {
            PlannedKind::Insn { decoded, .. } => Some(*decoded),
            PlannedKind::EvtBlock { .. } => Some(Decoded::Event(
                event_top_op(0x06).expect("evt_block opcode exists"),
            )),
            PlannedKind::EvtSingle { .. } => Some(Decoded::Event(
                event_top_op(0x07).expect("evt_single opcode exists"),
            )),
            PlannedKind::EvtDo { .. } => Some(Decoded::Control(
                event_control_op(0xFC).expect("evt_do opcode exists"),
            )),
            PlannedKind::SubBlock { size } => Some(Decoded::SubBlockHeader { size: *size }),
            PlannedKind::Db(_) => None,
        }
    }
}

/// A planned stream with prefix offsets and resolved label positions.
#[derive(Debug, Clone)]
struct PlannedStream {
    items: Vec<PlannedItem>,
    offsets: Vec<usize>,
    labels: HashMap<String, usize>,
    total: usize,
    event: bool,
    line: usize,
}

/// An emitted stream plus the reader's decode of its bytes.
struct Finished {
    body: Vec<u8>,
    insns: Vec<Insn>,
    trailing: Vec<u8>,
    line: usize,
}

/// Plan, emit and verify one init/main block or event stream.
fn finish_block(items: &[Item], event: bool) -> Result<Finished> {
    let planned = build_plan(items, event)?;
    emit_planned(&planned)
}

/// Plan one block or event stream.
fn build_plan(items: &[Item], event: bool) -> Result<PlannedStream> {
    let planned_items = if event {
        plan_event_items(items)?
    } else {
        plan_command_items(items)?
    };

    let mut offsets = Vec::with_capacity(planned_items.len());
    let mut cursor = 0usize;
    for item in &planned_items {
        offsets.push(cursor);
        cursor += item.size();
    }
    let total = cursor;

    let mut labels: HashMap<String, usize> = HashMap::new();
    for (index, item) in planned_items.iter().enumerate() {
        for name in &item.labels {
            if labels.insert(name.clone(), offsets[index]).is_some() {
                bail!(
                    "line {}, column {}: label `{name}` is defined more than once",
                    item.line,
                    item.col
                );
            }
        }
    }

    let line = planned_items.first().map_or(1, |item| item.line);
    let planned = PlannedStream {
        items: planned_items,
        offsets,
        labels,
        total,
        event,
        line,
    };
    validate_structure(&planned)?;
    Ok(planned)
}

/// Plan a straight run of command-VM items (init/main block or `evt_block`
/// body).
fn plan_command_items(items: &[Item]) -> Result<Vec<PlannedItem>> {
    let mut planned = Vec::new();
    let mut saw_db = false;
    for item in items {
        match &item.kind {
            ItemKind::Db(bytes) => {
                saw_db = true;
                planned.push(PlannedItem {
                    labels: item.labels.clone(),
                    line: item.line,
                    col: item.col,
                    kind: PlannedKind::Db(bytes.clone()),
                });
            }
            ItemKind::Insn(src) => {
                if saw_db {
                    bail!(
                        "line {}, column {}: `db` must be the last item of a block",
                        item.line,
                        item.col
                    );
                }
                let op = resolve_command(&src.mnemonic).ok_or_else(|| {
                    anyhow::anyhow!(
                        "line {}, column {}: unknown command mnemonic `{}`",
                        item.line,
                        item.col,
                        src.mnemonic
                    )
                })?;
                let bytes = encode_insn(Table::Command, op, src, &OperandContext::placeholder())?;
                planned.push(PlannedItem {
                    labels: item.labels.clone(),
                    line: item.line,
                    col: item.col,
                    kind: PlannedKind::Insn {
                        src: src.clone(),
                        decoded: Decoded::Command(op),
                        limit: None,
                        size: bytes.len(),
                    },
                });
            }
        }
    }
    Ok(planned)
}

/// Plan an event stream, rebuilding the `evt_block`/`evt_single`/`evt_do`
/// payload structures from the surrounding items.
fn plan_event_items(items: &[Item]) -> Result<Vec<PlannedItem>> {
    let mut planned: Vec<PlannedItem> = Vec::new();
    let mut state = EventState::Top;
    let mut index = 0usize;
    let mut saw_db = false;

    while index < items.len() {
        let item = &items[index];
        match &item.kind {
            ItemKind::Db(bytes) => {
                saw_db = true;
                planned.push(PlannedItem {
                    labels: item.labels.clone(),
                    line: item.line,
                    col: item.col,
                    kind: PlannedKind::Db(bytes.clone()),
                });
                index += 1;
            }
            ItemKind::Insn(src) => {
                if saw_db {
                    bail!(
                        "line {}, column {}: `db` must be the last item of an event stream",
                        item.line,
                        item.col
                    );
                }
                let (byte, decoded, next_state) =
                    resolve_event(state, &src.mnemonic).ok_or_else(|| {
                        anyhow::anyhow!(
                            "line {}, column {}: mnemonic `{}` is not valid here",
                            item.line,
                            item.col,
                            src.mnemonic
                        )
                    })?;
                match (&decoded, byte) {
                    (Decoded::Event(op), 0x06) => {
                        index = plan_evt_block(items, index, src, op, &mut planned)?;
                        // The chain does not change the VM state.
                    }
                    (Decoded::Event(_), 0x07) => {
                        index = plan_evt_single(items, index, src, &mut planned)?;
                        state = next_state;
                    }
                    (Decoded::Control(_), 0xFC) => {
                        index = plan_evt_do(items, index, src, &mut planned)?;
                        state = next_state;
                    }
                    _ => {
                        let bytes = encode_insn(
                            decoded_table(&decoded),
                            decoded_op(&decoded),
                            src,
                            &OperandContext::placeholder(),
                        )?;
                        planned.push(PlannedItem {
                            labels: item.labels.clone(),
                            line: item.line,
                            col: item.col,
                            kind: PlannedKind::Insn {
                                src: src.clone(),
                                decoded,
                                limit: None,
                                size: bytes.len(),
                            },
                        });
                        state = next_state;
                        index += 1;
                    }
                }
            }
        }
    }
    Ok(planned)
}

/// Plan an `evt_block` header and its inner `u16` size chain.
fn plan_evt_block(
    items: &[Item],
    index: usize,
    src: &InsnSrc,
    _op: &'static Op,
    planned: &mut Vec<PlannedItem>,
) -> Result<usize> {
    let declared = single_int(src)?;
    let mut chain = 2usize;
    let mut cursor = index + 1;
    let mut terminated = false;
    let mut body_items: Vec<PlannedItem> = Vec::new();
    while cursor < items.len() {
        let item = &items[cursor];
        let ItemKind::Insn(sub) = &item.kind else {
            bail!(
                "line {}, column {}: expected `evt_block_sub` in the `evt_block` chain",
                item.line,
                item.col
            );
        };
        if sub.mnemonic != "evt_block_sub" {
            break;
        }
        let size = single_int(sub)?;
        if size < 0 || size > i64::from(u16::MAX) {
            bail!(
                "line {}, column {}: `evt_block_sub` size {size} does not fit 16 bits",
                sub.line,
                sub.col
            );
        }
        let size = size as u16;
        if size == 0 {
            body_items.push(PlannedItem {
                labels: item.labels.clone(),
                line: sub.line,
                col: sub.col,
                kind: PlannedKind::SubBlock { size: 0 },
            });
            chain += 1;
            cursor += 1;
            terminated = true;
            break;
        }

        // The sub-block body is every following item up to the next
        // `evt_block_sub`.
        let body_start = cursor + 1;
        let mut body_end = body_start;
        while body_end < items.len() && !is_sub_block(&items[body_end]) {
            body_end += 1;
        }
        body_items.push(PlannedItem {
            labels: item.labels.clone(),
            line: sub.line,
            col: sub.col,
            kind: PlannedKind::SubBlock { size },
        });
        chain += 2;
        let body = plan_command_items(&items[body_start..body_end])?;
        let body_len: usize = body.iter().map(PlannedItem::size).sum();
        if usize::from(size) != body_len + 2 {
            bail!(
                "line {}, column {}: `evt_block_sub` size {size} does not match its \
                 {body_len}-byte body",
                sub.line,
                sub.col
            );
        }
        chain += body_len;
        body_items.extend(body);
        cursor = body_end;
    }
    if !terminated {
        bail!(
            "line {}, column {}: `evt_block` has no `evt_block_sub 0` terminator",
            src.line,
            src.col
        );
    }
    if declared < 0 || declared as usize != chain {
        bail!(
            "line {}, column {}: `evt_block` payload {declared} does not match its \
             rebuilt chain of {chain}",
            src.line,
            src.col
        );
    }
    planned.push(PlannedItem {
        labels: items[index].labels.clone(),
        line: src.line,
        col: src.col,
        kind: PlannedKind::EvtBlock {
            src: src.clone(),
            chain,
        },
    });
    planned.extend(body_items);
    Ok(cursor)
}

/// True when `item` is an `evt_block_sub` instruction.
fn is_sub_block(item: &Item) -> bool {
    matches!(&item.kind, ItemKind::Insn(src) if src.mnemonic == "evt_block_sub")
}

/// Plan an `evt_single` header and clamp its inline command.
fn plan_evt_single(
    items: &[Item],
    index: usize,
    src: &InsnSrc,
    planned: &mut Vec<PlannedItem>,
) -> Result<usize> {
    let payload = single_int(src)?;
    if !(4..=255).contains(&payload) {
        bail!(
            "line {}, column {}: `evt_single` payload {payload} must be 4-255 to hold \
             one command",
            src.line,
            src.col
        );
    }
    let payload = usize::try_from(payload).unwrap_or(0);
    let Some(next) = items.get(index + 1) else {
        bail!(
            "line {}, column {}: `evt_single` is not followed by a command",
            src.line,
            src.col
        );
    };
    let ItemKind::Insn(inline) = &next.kind else {
        bail!(
            "line {}, column {}: `evt_single` payload is not a command",
            next.line,
            next.col
        );
    };
    let op = resolve_command(&inline.mnemonic).ok_or_else(|| {
        anyhow::anyhow!(
            "line {}, column {}: unknown command mnemonic `{}` in the `evt_single` payload",
            inline.line,
            inline.col,
            inline.mnemonic
        )
    })?;
    let natural = encode_insn(Table::Command, op, inline, &OperandContext::placeholder())?;
    let available = payload - 2;
    if available > natural.len() {
        bail!(
            "line {}, column {}: `evt_single` payload of {payload} bytes does not match \
             its {} byte command",
            src.line,
            src.col,
            natural.len()
        );
    }
    planned.push(PlannedItem {
        labels: items[index].labels.clone(),
        line: src.line,
        col: src.col,
        kind: PlannedKind::EvtSingle {
            src: src.clone(),
            payload,
        },
    });
    planned.push(PlannedItem {
        labels: next.labels.clone(),
        line: inline.line,
        col: inline.col,
        kind: PlannedKind::Insn {
            src: inline.clone(),
            decoded: Decoded::Command(op),
            limit: Some(available),
            size: available,
        },
    });
    Ok(index + 2)
}

/// Plan an `evt_do` header and its inline condition commands.
fn plan_evt_do(
    items: &[Item],
    index: usize,
    src: &InsnSrc,
    planned: &mut Vec<PlannedItem>,
) -> Result<usize> {
    let (label, delta) = match src.args.as_slice() {
        [Arg::Label(name, delta)] => (name.clone(), *delta),
        [Arg::Symbol(name)] => (name.clone(), 0),
        _ => bail!(
            "line {}, column {}: `evt_do` needs exactly one label reference",
            src.line,
            src.col
        ),
    };
    if delta != 0 {
        bail!(
            "line {}, column {}: `evt_do` target `{label} + {delta}` is inside an instruction",
            src.line,
            src.col
        );
    }
    // The inline run ends at the item carrying the label definition.
    let mut end = index + 1;
    let mut found = false;
    while end < items.len() {
        if items[end].labels.iter().any(|name| name == &label) {
            found = true;
            break;
        }
        end += 1;
    }
    if !found {
        bail!(
            "line {}, column {}: `evt_do` target label `{label}` is not defined",
            src.line,
            src.col
        );
    }
    planned.push(PlannedItem {
        labels: items[index].labels.clone(),
        line: src.line,
        col: src.col,
        kind: PlannedKind::EvtDo {
            src: src.clone(),
            label,
            delta,
        },
    });
    let body = plan_command_items(&items[index + 1..end])?;
    for item in body {
        match &item.kind {
            PlannedKind::Insn { .. } => planned.push(item),
            _ => bail!(
                "line {}: `evt_do` may only hold command instructions",
                src.line
            ),
        }
    }
    Ok(end)
}

/// Validate the resolved structure of every special item.
fn validate_structure(planned: &PlannedStream) -> Result<()> {
    for (index, item) in planned.items.iter().enumerate() {
        match &item.kind {
            PlannedKind::EvtDo { src, label, .. } => {
                let Some(&target) = planned.labels.get(label) else {
                    bail!(
                        "line {}, column {}: `evt_do` target label `{label}` is not defined",
                        src.line,
                        src.col
                    );
                };
                let start = planned.offsets[index] + 2;
                if target < start {
                    bail!(
                        "line {}, column {}: `evt_do` target `{label}` is behind its payload",
                        src.line,
                        src.col
                    );
                }
                if target - start > 255 {
                    bail!(
                        "line {}, column {}: `evt_do` jump of {} bytes does not fit a byte",
                        src.line,
                        src.col,
                        target - start
                    );
                }
            }
            PlannedKind::EvtSingle { src, payload } => {
                let inline = planned.items.get(index + 1).ok_or_else(|| {
                    anyhow::anyhow!(
                        "line {}, column {}: `evt_single` has no inline command",
                        src.line,
                        src.col
                    )
                })?;
                let end = planned.offsets[index] + 2 + inline.size();
                if end != planned.offsets[index] + payload {
                    bail!(
                        "line {}, column {}: `evt_single` payload ends at 0x{:X} but its \
                         command ends at 0x{end:X}",
                        src.line,
                        src.col,
                        planned.offsets[index] + payload
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Emitting
// ---------------------------------------------------------------------------

/// Resolved label positions for the second encoding pass.
struct OperandContext<'a> {
    labels: Option<&'a HashMap<String, usize>>,
    offset: usize,
}

impl OperandContext<'_> {
    fn placeholder() -> Self {
        Self {
            labels: None,
            offset: 0,
        }
    }

    fn resolved(&self, name: &str, line: usize, col: usize) -> Result<usize> {
        let Some(labels) = self.labels else {
            return Ok(0);
        };
        labels.get(name).copied().ok_or_else(|| {
            anyhow::anyhow!("line {line}, column {col}: label `{name}` is not defined")
        })
    }
}

/// Emit every planned item with resolved label operands.
fn emit_planned(planned: &PlannedStream) -> Result<Finished> {
    let mut body = Vec::with_capacity(planned.total);
    for (index, item) in planned.items.iter().enumerate() {
        let offset = planned.offsets[index];
        match &item.kind {
            PlannedKind::Insn {
                src,
                decoded,
                limit,
                size,
                ..
            } => {
                let context = OperandContext {
                    labels: Some(&planned.labels),
                    offset,
                };
                let mut bytes =
                    encode_insn(decoded_table(decoded), decoded_op(decoded), src, &context)?;
                if let Some(limit) = limit {
                    bytes.truncate(*limit);
                }
                if bytes.len() != *size {
                    bail!(
                        "line {}, column {}: `{}` changes width while resolving labels",
                        src.line,
                        src.col,
                        src.mnemonic
                    );
                }
                body.extend_from_slice(&bytes);
            }
            PlannedKind::EvtBlock { src, chain } => {
                let byte = u8::try_from(*chain).map_err(|_| {
                    anyhow::anyhow!(
                        "line {}, column {}: `evt_block` chain is longer than 255 bytes",
                        src.line,
                        src.col
                    )
                })?;
                body.extend_from_slice(&[0x06, byte]);
            }
            PlannedKind::EvtSingle { src, payload } => {
                let byte = u8::try_from(*payload).map_err(|_| {
                    anyhow::anyhow!(
                        "line {}, column {}: `evt_single` payload does not fit a byte",
                        src.line,
                        src.col
                    )
                })?;
                body.extend_from_slice(&[0x07, byte]);
            }
            PlannedKind::EvtDo { src, label, delta } => {
                let target = OperandContext {
                    labels: Some(&planned.labels),
                    offset,
                }
                .resolved(label, src.line, src.col)?
                    + *delta as usize;
                let value = target
                    .checked_sub(offset)
                    .filter(|_| target >= offset)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "line {}, column {}: `evt_do` target `{label}` is behind the \
                             instruction",
                            src.line,
                            src.col
                        )
                    })?;
                if !(2..=255).contains(&value) {
                    bail!(
                        "line {}, column {}: `evt_do` jump of {value} bytes does not fit a byte",
                        src.line,
                        src.col
                    );
                }
                body.extend_from_slice(&[0xFC, value as u8]);
            }
            PlannedKind::SubBlock { size } => {
                if *size == 0 {
                    body.push(0);
                } else {
                    body.extend_from_slice(&size.to_le_bytes());
                }
            }
            PlannedKind::Db(bytes) => body.extend_from_slice(bytes),
        }
    }
    if body.len() != planned.total {
        bail!("internal error: emitted stream length mismatch");
    }

    let event = planned.event;
    let (insns, trailing) = if event {
        reader::decode_event_stream(&body, 0, body.len())
    } else {
        reader::decode_command_block(&body, 0, body.len())
    };
    verify_decode(planned, &body, &insns, &trailing)?;
    let line = planned.line;
    Ok(Finished {
        body,
        insns,
        trailing,
        line,
    })
}

/// Assert the reader re-decodes the emitted bytes to the planned sequence.
fn verify_decode(
    planned: &PlannedStream,
    body: &[u8],
    insns: &[Insn],
    trailing: &[u8],
) -> Result<()> {
    let expected: Vec<Decoded> = planned
        .items
        .iter()
        .filter_map(PlannedItem::decoded)
        .collect();
    let covered: usize = insns.iter().map(|insn| insn.bytes.len()).sum();
    if covered + trailing.len() != body.len() {
        bail!(
            "line {}: the encoded stream does not re-decode cleanly \
             ({} of {} bytes covered)",
            planned.line,
            covered + trailing.len(),
            body.len()
        );
    }
    if insns.len() != expected.len() {
        bail!(
            "line {}: the encoded stream re-decodes to {} instructions, expected {}",
            planned.line,
            insns.len(),
            expected.len()
        );
    }
    for (index, (insn, decoded)) in insns.iter().zip(&expected).enumerate() {
        if &insn.decoded != decoded {
            let item = planned
                .items
                .iter()
                .filter(|item| item.decoded().is_some())
                .nth(index)
                .expect("instruction index exists");
            bail!(
                "line {}, column {}: `{}` re-decodes as a different instruction",
                item.line,
                item.col,
                item_mnemonic(item)
            );
        }
    }
    let expected_trailing: Vec<u8> = planned
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            PlannedKind::Db(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    if trailing != expected_trailing.as_slice() {
        bail!(
            "line {}: the encoded stream re-decodes with different trailing bytes",
            planned.line
        );
    }
    Ok(())
}

/// The source mnemonic of a planned instruction item.
fn item_mnemonic(item: &PlannedItem) -> String {
    match &item.kind {
        PlannedKind::Insn { src, .. }
        | PlannedKind::EvtBlock { src, .. }
        | PlannedKind::EvtSingle { src, .. }
        | PlannedKind::EvtDo { src, .. } => src.mnemonic.clone(),
        PlannedKind::SubBlock { size: 0 } => "evt_block_sub 0".to_string(),
        PlannedKind::SubBlock { .. } => "evt_block_sub".to_string(),
        PlannedKind::Db(_) => "db".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Instruction encoding
// ---------------------------------------------------------------------------

/// The opcode an instruction decoded to, for a `Decoded` value.
fn decoded_op(decoded: &Decoded) -> &'static Op {
    match decoded {
        Decoded::Command(op)
        | Decoded::Event(op)
        | Decoded::Control(op)
        | Decoded::Actor(op)
        | Decoded::Tween(op) => op,
        Decoded::SubBlockHeader { .. } => panic!("sub-block headers are not encoded as opcodes"),
        Decoded::Raw => panic!("raw instructions cannot be encoded"),
    }
}

/// Look up a command-VM mnemonic, accepting the original-tool display names.
fn resolve_command(mnemonic: &str) -> Option<&'static Op> {
    COMMAND_OPS.iter().find(|op| {
        if op.width == Some(0) {
            return false;
        }
        let signature = disasm::COMMAND_SIG[usize::from(op.op)];
        let display = signature.split(':').next().unwrap_or(signature);
        op.mnemonic == mnemonic || (!display.is_empty() && display == mnemonic)
    })
}

/// Find the byte that decodes to `mnemonic` in `state`, mirroring the reader.
fn resolve_event(state: EventState, mnemonic: &str) -> Option<(u8, Decoded, EventState)> {
    if mnemonic == "act_motion_path" {
        return resolve_event(state, "act_motion");
    }
    for byte in 0u8..=u8::MAX {
        let Some((decoded, next)) = mirror_decode(state, byte) else {
            continue;
        };
        if event_mnemonic(&decoded, state) == Some(mnemonic) {
            return Some((byte, decoded, next));
        }
    }
    None
}

/// The reader's decode of one byte in one event state.
fn mirror_decode(state: EventState, byte: u8) -> Option<(Decoded, EventState)> {
    if let Some(control) = event_control_op(byte) {
        let next = if byte == 0xFF { EventState::Top } else { state };
        return Some((Decoded::Control(control), next));
    }
    let mut current = state;
    loop {
        let op = match current {
            EventState::Top => event_top_op(byte),
            EventState::Actor => actor_op(byte),
            EventState::Tween => tween_op(byte),
        };
        if let Some(op) = op {
            let decoded = match current {
                EventState::Top => Decoded::Event(op),
                EventState::Actor => Decoded::Actor(op),
                EventState::Tween => Decoded::Tween(op),
            };
            let next = match (current, byte) {
                (EventState::Top, 0x01) => EventState::Actor,
                (EventState::Top, 0x02 | 0x03) => EventState::Tween,
                (EventState::Actor, 0x80 | 0x8B) => EventState::Top,
                (EventState::Tween, 0x01) => EventState::Top,
                _ => current,
            };
            return Some((decoded, next));
        }
        if current == EventState::Top {
            return None;
        }
        current = EventState::Top;
    }
}

/// The mnemonic the disassembler prints for a decoded event instruction.
fn event_mnemonic(decoded: &Decoded, state: EventState) -> Option<&'static str> {
    match decoded {
        Decoded::Event(op) | Decoded::Actor(op) | Decoded::Tween(op) => Some(op.mnemonic),
        Decoded::Control(op) if op.op == 0xFF => Some(match state {
            EventState::Top => "evt_finish",
            EventState::Actor => "act_finish",
            EventState::Tween => "tw_finish",
        }),
        Decoded::Control(op) => Some(op.mnemonic),
        _ => None,
    }
}

/// Which opcode table an instruction belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Command,
    Event,
    Control,
    Actor,
    Tween,
}

/// Encode one instruction of any VM into its exact byte form.
fn encode_insn(
    table: Table,
    op: &'static Op,
    src: &InsnSrc,
    context: &OperandContext,
) -> Result<Vec<u8>> {
    if table == Table::Command && op.width.is_none() {
        return encode_dynamic_command(op, src);
    }
    let signature = signature_for(table, op, &src.mnemonic);
    if signature.is_empty() {
        bail!(
            "line {}, column {}: mnemonic `{}` is not encodable",
            src.line,
            src.col,
            src.mnemonic
        );
    }
    match signature.split_once(':') {
        Some((_, params)) => encode_parameters(op, params, src, context),
        None => {
            let width = op.width.unwrap_or(0);
            if width <= 1 {
                if !src.args.is_empty() {
                    bail!(
                        "line {}, column {}: `{}` takes no arguments",
                        src.line,
                        src.col,
                        src.mnemonic
                    );
                }
                return Ok(vec![op.op]);
            }
            encode_raw_arguments(op, width, src)
        }
    }
}

/// The display signature for an instruction, including the length-aware actor
/// forms.
fn signature_for(table: Table, op: &'static Op, mnemonic: &str) -> &'static str {
    match table {
        Table::Command => disasm::COMMAND_SIG
            .get(usize::from(op.op))
            .copied()
            .unwrap_or(""),
        Table::Event => disasm::EVENT_TOP_SIG
            .get(usize::from(op.op))
            .copied()
            .unwrap_or(""),
        Table::Control => op
            .op
            .checked_sub(0xF6)
            .and_then(|index| disasm::CONTROL_SIG.get(usize::from(index)).copied())
            .unwrap_or(""),
        Table::Actor => {
            if op.op == 0x81 && mnemonic == "act_motion_path" {
                return "act_motion_path:uUUUuu";
            }
            let index = match op.op {
                0x00 => 0,
                0x80..=0x8B => usize::from(op.op - 0x80) + 1,
                _ => return "",
            };
            disasm::ACTOR_SIG.get(index).copied().unwrap_or("")
        }
        Table::Tween => disasm::TWEEN_SIG
            .get(usize::from(op.op))
            .copied()
            .unwrap_or(""),
    }
}

/// The table a decoded event instruction came from.
fn decoded_table(decoded: &Decoded) -> Table {
    match decoded {
        Decoded::Command(_) => Table::Command,
        Decoded::Event(_) => Table::Event,
        Decoded::Control(_) => Table::Control,
        Decoded::Actor(_) => Table::Actor,
        Decoded::Tween(_) => Table::Tween,
        Decoded::SubBlockHeader { .. } | Decoded::Raw => Table::Command,
    }
}

/// Encode the variable-width command sub-commands (0x17/0x28/0x33).
///
/// The disassembler prints the reader's decoded operands followed by any raw
/// byte the operand signature did not name; rebuild those bytes and validate
/// the width against [`command_width`].
fn encode_dynamic_command(op: &'static Op, src: &InsnSrc) -> Result<Vec<u8>> {
    let mut bytes = vec![op.op];
    let mut index = 0usize;
    for kind in op.operands.bytes() {
        let Some(arg) = src.args.get(index) else {
            break;
        };
        match kind {
            b'u' => bytes.push(range_u8(argument_int(arg, op, src)?, src.line, src.col)?),
            b'U' => bytes.extend_from_slice(
                &range_u16(argument_int(arg, op, src)?, src.line, src.col)?.to_le_bytes(),
            ),
            b'I' => bytes.extend_from_slice(
                &range_i16(argument_int(arg, op, src)?, src.line, src.col)?.to_le_bytes(),
            ),
            b'b' => bytes.push(bitfield(arg, src)?),
            b'l' | b'j' => bail!(
                "line {}, column {}: `{}` has an unexpected jump operand",
                src.line,
                src.col,
                src.mnemonic
            ),
            _ => bail!(
                "line {}, column {}: unsupported operand in `{}`",
                src.line,
                src.col,
                src.mnemonic
            ),
        }
        index += 1;
    }
    for arg in &src.args[index..] {
        bytes.push(range_u8(argument_int(arg, op, src)?, src.line, src.col)?);
    }
    let width = command_width(&bytes);
    if width != Some(bytes.len()) {
        bail!(
            "line {}, column {}: `{}` encodes to {} bytes but its sub-command decodes \
             to {}",
            src.line,
            src.col,
            src.mnemonic,
            bytes.len(),
            width.map_or("an invalid width".to_string(), |value| value.to_string())
        );
    }
    Ok(bytes)
}

/// Encode a command whose display signature has no parameter list.
///
/// The disassembler prints exactly the bytes an instruction stores, so a
/// clamped inline command carries fewer arguments than the opcode's natural
/// width; the caller's payload limit truncates the re-encoded bytes.
fn encode_raw_arguments(op: &'static Op, width: usize, src: &InsnSrc) -> Result<Vec<u8>> {
    let expected = width - 1;
    if src.args.len() > expected {
        bail!(
            "line {}, column {}: `{}` takes at most {expected} arguments, found {}",
            src.line,
            src.col,
            src.mnemonic,
            src.args.len()
        );
    }
    let mut bytes = vec![op.op];
    for arg in &src.args {
        bytes.push(range_u8(argument_int(arg, op, src)?, src.line, src.col)?);
    }
    Ok(bytes)
}

/// Encode one `name:params` operand list.
fn encode_parameters(
    op: &'static Op,
    params: &str,
    src: &InsnSrc,
    context: &OperandContext,
) -> Result<Vec<u8>> {
    if src.args.len() > params.len() {
        bail!(
            "line {}, column {}: `{}` takes {} arguments, found {}",
            src.line,
            src.col,
            src.mnemonic,
            params.len(),
            src.args.len()
        );
    }
    let mut bytes = vec![op.op];
    for (index, kind) in params.bytes().enumerate() {
        let Some(arg) = src.args.get(index) else {
            bail!(
                "line {}, column {}: `{}` is missing argument {} of {}",
                src.line,
                src.col,
                src.mnemonic,
                index + 1,
                params.len()
            );
        };
        if matches!(kind, b'u' | b'U' | b'I')
            && let Arg::Symbol(name) = arg
            && is_event_parameter(op.op, index)
        {
            let value = constant_value('p', &Arg::Symbol(name.clone()))?;
            match kind {
                b'u' => bytes.push(value),
                _ => {
                    bytes.push(value);
                    bytes.push(0);
                }
            }
            continue;
        }
        match kind {
            b'u' => bytes.push(range_u8(argument_int(arg, op, src)?, src.line, src.col)?),
            b'U' => bytes.extend_from_slice(
                &range_u16(argument_int(arg, op, src)?, src.line, src.col)?.to_le_bytes(),
            ),
            b'I' => bytes.extend_from_slice(
                &range_i16(argument_int(arg, op, src)?, src.line, src.col)?.to_le_bytes(),
            ),
            b'b' => bytes.push(bitfield(arg, src)?),
            b'f' => bytes.push(constant_value('f', arg)?),
            b's' => bytes.push(constant_value('s', arg)?),
            b'w' => bytes.push(constant_value('w', arg)?),
            b'e' => bytes.push(constant_value('e', arg)?),
            b't' => bytes.push(constant_value('t', arg)?),
            b'p' => bytes.push(constant_value('p', arg)?),
            b'r' => bytes.push(constant_value('r', arg)?),
            b'l' => bytes.push(encode_label(arg, context, src)?),
            other => bail!(
                "line {}, column {}: unsupported operand `{}` in `{}`",
                src.line,
                src.col,
                other as char,
                src.mnemonic
            ),
        }
    }
    if op.op == 0x81 {
        let flags = bytes.get(1).copied().unwrap_or(0);
        let long = bytes.len() == 10;
        if long && flags & 0x0F == 0 {
            bail!(
                "line {}, column {}: `{}` with a zero motion nibble would decode as the \
                 2-byte `act_motion`",
                src.line,
                src.col,
                src.mnemonic
            );
        }
        if !long && flags & 0x0F != 0 {
            bail!(
                "line {}, column {}: `{}` with a set motion nibble would decode as the \
                 10-byte `act_motion_path`",
                src.line,
                src.col,
                src.mnemonic
            );
        }
    }
    Ok(bytes)
}

/// True when `index` is a disassembler special-constant parameter.
fn is_event_parameter(op: u8, index: usize) -> bool {
    matches!((op, index), (0x0D, 9) | (0x12, 4))
}

/// Encode a jump label operand as a forward byte delta.
fn encode_label(arg: &Arg, context: &OperandContext, src: &InsnSrc) -> Result<u8> {
    match arg {
        Arg::Num(value) => range_u8(*value, src.line, src.col),
        Arg::Label(name, delta) => encode_label_target(name, *delta, context, src),
        Arg::Symbol(name) => encode_label_target(name, 0, context, src),
        _ => bail!(
            "line {}, column {}: invalid jump operand in `{}`",
            src.line,
            src.col,
            src.mnemonic
        ),
    }
}

/// Resolve one label reference to a forward byte delta.
fn encode_label_target(
    name: &str,
    delta: i64,
    context: &OperandContext,
    src: &InsnSrc,
) -> Result<u8> {
    let base = context.resolved(name, src.line, src.col)?;
    let target = base as i128 + delta as i128;
    let value = target - context.offset as i128;
    if value < 0 {
        bail!(
            "line {}, column {}: jump to `{name}` points backwards",
            src.line,
            src.col
        );
    }
    if value > 255 {
        bail!(
            "line {}, column {}: jump to `{name}` is {value} bytes and does not fit",
            src.line,
            src.col
        );
    }
    Ok(value as u8)
}

/// Decode an integer argument.
fn argument_int(arg: &Arg, _op: &Op, src: &InsnSrc) -> Result<i64> {
    match arg {
        Arg::Num(value) => Ok(*value),
        _ => bail!(
            "line {}, column {}: `{}` needs a numeric argument, found `{arg:?}`",
            src.line,
            src.col,
            src.mnemonic
        ),
    }
}

/// Encode a bit-packed actor field.
fn bitfield(arg: &Arg, src: &InsnSrc) -> Result<u8> {
    match arg {
        Arg::Num(value) => range_u8(*value, src.line, src.col),
        Arg::Bits(hi, lo) => {
            if !(0..=224).contains(hi) || hi % 32 != 0 {
                bail!(
                    "line {}, column {}: bit field `{hi}` is not a multiple of 32",
                    src.line,
                    src.col
                );
            }
            if !(0..=31).contains(lo) {
                bail!(
                    "line {}, column {}: bit field `{lo}` is out of range",
                    src.line,
                    src.col
                );
            }
            Ok((*hi | *lo) as u8)
        }
        _ => bail!(
            "line {}, column {}: invalid bit field in `{}`",
            src.line,
            src.col,
            src.mnemonic
        ),
    }
}

/// Resolve a `db`-style or raw byte argument.
fn range_u8(value: i64, line: usize, col: usize) -> Result<u8> {
    u8::try_from(value).map_err(|_| {
        anyhow::anyhow!("line {line}, column {col}: byte value {value} is out of range")
    })
}

fn range_u16(value: i64, line: usize, col: usize) -> Result<u16> {
    u16::try_from(value).map_err(|_| {
        anyhow::anyhow!("line {line}, column {col}: word value {value} is out of range")
    })
}

fn range_i16(value: i64, line: usize, col: usize) -> Result<i16> {
    i16::try_from(value).map_err(|_| {
        anyhow::anyhow!("line {line}, column {col}: signed word value {value} is out of range")
    })
}

/// One integer argument of a single-argument pseudo instruction.
fn single_int(src: &InsnSrc) -> Result<i64> {
    match src.args.as_slice() {
        [Arg::Num(value)] => Ok(*value),
        _ => bail!(
            "line {}, column {}: `{}` takes exactly one numeric argument",
            src.line,
            src.col,
            src.mnemonic
        ),
    }
}

// ---------------------------------------------------------------------------
// Named constants
// ---------------------------------------------------------------------------

/// Reverse names to values for every named-operand family.
struct ConstantMaps {
    flag: HashMap<String, u8>,
    sce: HashMap<String, u8>,
    work: HashMap<String, u8>,
    item: HashMap<String, u8>,
    enemy: HashMap<String, u8>,
    key: HashMap<String, u8>,
    rdt: HashMap<String, u8>,
    event: HashMap<String, u8>,
}

/// Build the reverse maps once; first value wins on a name collision.
fn constant_maps() -> &'static ConstantMaps {
    use std::sync::OnceLock;
    static MAPS: OnceLock<ConstantMaps> = OnceLock::new();
    MAPS.get_or_init(|| {
        let mut maps = ConstantMaps {
            flag: HashMap::new(),
            sce: HashMap::new(),
            work: HashMap::new(),
            item: HashMap::new(),
            enemy: HashMap::new(),
            key: HashMap::new(),
            rdt: HashMap::new(),
            event: HashMap::new(),
        };
        for value in 0u8..=u8::MAX {
            let insert = |map: &mut HashMap<String, u8>, name: Option<String>| {
                if let Some(name) = name
                    && !name.is_empty()
                {
                    map.entry(name).or_insert(value);
                }
            };
            insert(
                &mut maps.flag,
                constants::flag_group(value).map(str::to_string),
            );
            insert(&mut maps.sce, constants::sce(value).map(str::to_string));
            insert(&mut maps.work, constants::work(value).map(str::to_string));
            insert(&mut maps.item, Some(constants::item(value)));
            insert(&mut maps.enemy, Some(constants::enemy(value)));
            insert(&mut maps.key, Some(constants::key(value)));
            insert(&mut maps.rdt, Some(constants::rdt(value)));
            insert(&mut maps.event, Some(constants::event(value)));
        }
        maps
    })
}

/// Resolve one named-constant argument.
fn constant_value(kind: char, arg: &Arg) -> Result<u8> {
    let maps = constant_maps();
    match arg {
        Arg::Num(value) => u8::try_from(*value)
            .map_err(|_| anyhow::anyhow!("constant value {value} is out of range")),
        Arg::Symbol(name) => {
            let map = match kind {
                'f' => &maps.flag,
                's' => &maps.sce,
                'w' => &maps.work,
                'e' => &maps.enemy,
                't' => &maps.key,
                'p' => &maps.event,
                'r' => &maps.rdt,
                _ => return Err(anyhow::anyhow!("unsupported constant family `{kind}`")),
            };
            map.get(name)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("unknown constant `{name}`"))
        }
        _ => Err(anyhow::anyhow!("expected a constant name or value")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Assembled {
        assemble(text).expect("source assembles")
    }

    const DEMO: &str = "\
.version 1

.init
.block
    nop                     0

.main
.block
    nop                     0

.event event_00
    evt_finish
";

    #[test]
    fn assembles_the_disassembler_shape() {
        let assembled = parse(DEMO);
        assert_eq!(assembled.init.len(), 1);
        assert_eq!(assembled.main.len(), 1);
        assert_eq!(assembled.events.len(), 1);
        assert_eq!(assembled.init[0].insns[0].bytes, [0x0E, 0x00]);
        assert_eq!(assembled.events[0].insns[0].bytes, [0xFF]);

        let container = assembled.to_container().unwrap();
        let scripts = reader::parse(&container).unwrap();
        assert_eq!(scripts.init.len(), 1);
        assert_eq!(scripts.main.len(), 1);
        assert_eq!(scripts.events.len(), 1);
        assert_eq!(scripts.init[0].insns[0].bytes, [0x0E, 0x00]);
        assert_eq!(scripts.events[0].insns[0].bytes, [0xFF]);
    }

    #[test]
    fn container_uses_the_pointer_table_abi() {
        let container = parse(DEMO).to_container().unwrap();
        let slot = |index: usize| {
            let at = POINTERS_OFFSET + index * 4;
            u32::from_le_bytes(container[at..at + 4].try_into().unwrap()) as usize
        };
        assert_eq!(slot(6), 0x94);
        assert!(slot(7) > slot(6));
        assert!(slot(8) > slot(7));
        assert_eq!(slot(0), 0);
        assert!(container.len() >= 0x94);
        assert_eq!(&container[0x94..0x96], &[0x04, 0x00]);
        assert_eq!(&container[0x96..0x98], &[0x0E, 0x00]);
        assert_eq!(&container[0x98..0x9A], &[0x00, 0x00]);
    }

    #[test]
    fn labels_and_forward_jumps_relocate() {
        let assembled = parse(
            "\
.version 1

.init
.block
    if                      off_0002 + 1
off_0002:
    ck                      FG_SCENARIO, 31, 1
    else                    off_000A
    set                     FG_SCENARIO, 1, 0
off_000A:
    endif                   0

.main
",
        );
        let init = &assembled.init[0];
        assert_eq!(init.insns[0].bytes, [0x01, 0x03]);
        assert_eq!(init.insns[2].bytes, [0x02, 0x06]);
        assert_eq!(init.insns[3].offset, 0x94 + 2 + 8);
        // The reader stores the `if` resume target, two bytes after the label.
        assert_eq!(init.insns[0].operands[0].target, Some(0x94 + 2 + 3 + 2));
    }

    #[test]
    fn backward_jumps_and_bad_labels_error() {
        let backward = assemble(
            "\
.version 1

.init
.block
off_0002:
    nop                     0
    if                      off_0002

.main
",
        )
        .unwrap_err()
        .to_string();
        assert!(backward.contains("backwards"), "{backward}");

        let undefined = assemble(
            "\
.version 1

.init
.block
    if                      off_9999

.main
",
        )
        .unwrap_err()
        .to_string();
        assert!(undefined.contains("off_9999"), "{undefined}");

        let duplicate = assemble(
            "\
.version 1

.init
.block
off_0002:
    nop                     0
off_0002:
    nop                     0

.main
",
        )
        .unwrap_err()
        .to_string();
        assert!(duplicate.contains("defined more than once"), "{duplicate}");
    }

    #[test]
    fn db_chunks_round_trip_as_trailing() {
        let assembled = parse(
            "\
.version 1

.init
.block
    nop                     0
    db                      0xAA, 0xBB, 0xCC

.main
",
        );
        assert_eq!(assembled.init[0].trailing, [0xAA, 0xBB, 0xCC]);
        let container = assembled.to_container().unwrap();
        let scripts = reader::parse(&container).unwrap();
        assert_eq!(scripts.init[0].trailing, [0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn every_constant_family_reverses() {
        let assembled = parse(
            "\
.version 1

.init
.block
    ck                      FG_SCENARIO, 1, 2
    scene_setup             1, 2, 3, 4
    aot_set                 0, 0, 0, 0, 0, SCE_EVENT, 0, 0, 0, event_0B, 0, 0, 0
    aot_reset               1, SCE_EVENT, 2, 0, event_0C, 0
    testitem                ITEM_SWORD_KEY
    door_aot_set            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, RDT_001, 0, 0, 0, 0, UNLOCKED, 0
    enemy                   ENEMY_CERBERUS, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0

.main
",
        );
        let bytes: Vec<Vec<u8>> = assembled.init[0]
            .insns
            .iter()
            .map(|insn| insn.bytes.clone())
            .collect();
        assert_eq!(bytes[0], [0x04, 0x00, 0x01, 0x02]);
        assert_eq!(
            bytes[2],
            [0x0D, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0x0B, 0, 0, 0]
        );
        assert_eq!(bytes[3], [0x12, 1, 9, 2, 0, 0, 0x0C, 0, 0, 0]);
        assert_eq!(bytes[4], [0x10, 0x33]);
        assert_eq!(bytes[5][15], 0x01);
        assert_eq!(bytes[5][24], 0);
        assert_eq!(bytes[6][1], 0x02);
    }

    #[test]
    fn work_and_event_constants_reverse() {
        let assembled = parse(
            "\
.version 1

.init
.block
    evt_exec                0, 0, event_0B

.main

.event event_00
    evt_work_set            WK_PLAYER, 1
    evt_finish
",
        );
        assert_eq!(assembled.init[0].insns[0].bytes, [0x14, 0x00, 0x00, 0x0B]);
        let event = &assembled.events[0];
        assert_eq!(event.insns[0].bytes, [0x04, 0x00, 0x01]);
    }

    #[test]
    fn bitfield_actor_forms_reverse() {
        let assembled = parse(
            "\
.version 1

.main

.event event_00
    evt_actor_begin
    act_anim_flags          32 | 16, 32 | 31, 0 | 0
    act_finish
",
        );
        let event = &assembled.events[0];
        assert_eq!(event.insns[1].bytes, [0x84, 0x30, 0x3F, 0x00]);
    }

    #[test]
    fn variable_width_commands_pick_their_sub_form() {
        let assembled = parse(
            "\
.version 1

.init
.block
    se_play_3d              1, 2, 3, 4, 5
    se_play_3d              1, 2, 3, 3, 5, 6, 7
    eml_state               0, 0, 2, 0
    eml_state               0, 0, 3, 300
    eml_state               0, 0, 1, 1, 0, 7
    sys_multi               6
    sys_multi               3, 0x1234

.main
",
        );
        let sizes: Vec<usize> = assembled.init[0]
            .insns
            .iter()
            .map(|insn| insn.bytes.len())
            .collect();
        assert_eq!(sizes, [6, 10, 6, 6, 8, 2, 4]);
        assert_eq!(assembled.init[0].insns[4].bytes[7], 7);
    }

    #[test]
    fn variable_width_rejects_bad_sub_commands() {
        let bad = assemble(
            "\
.version 1

.init
.block
    se_play_3d              1, 2, 3, 99, 5, 6, 7

.main
",
        )
        .unwrap_err()
        .to_string();
        assert!(bad.contains("sub-command"), "{bad}");
    }

    #[test]
    fn every_fixed_command_display_signature_covers_its_width() {
        for op in &COMMAND_OPS {
            let Some(width) = op.width else {
                continue;
            };
            if width == 0 {
                continue;
            }
            let signature = disasm::COMMAND_SIG[usize::from(op.op)];
            if signature.is_empty() {
                continue;
            }
            let Some((_, params)) = signature.split_once(':') else {
                continue;
            };
            let footprint: usize = params
                .bytes()
                .map(|kind| match kind {
                    b'U' | b'I' => 2,
                    _ => 1,
                })
                .sum();
            assert_eq!(
                1 + footprint,
                width,
                "{} display signature covers the wrong byte count",
                op.mnemonic
            );
        }
    }

    #[test]
    fn eml_state_width_eight_keeps_its_raw_trailing_byte() {
        let assembled = parse(
            "\
.version 1

.init
.block
    eml_state               0, 0, 1, 0, 0, 7

.main
",
        );
        assert_eq!(
            assembled.init[0].insns[0].bytes,
            [0x28, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x07]
        );
        let container = assembled.to_container().unwrap();
        let scripts = reader::parse(&container).unwrap();
        let rendered = disasm::render(&scripts, &container);
        assert!(
            rendered.contains("eml_state               0, 0, 1, 0, 0, 7"),
            "{rendered}"
        );
    }

    #[test]
    fn act_motion_short_and_path_forms() {
        let assembled = parse(
            "\
.version 1

.main

.event event_00
    evt_actor_begin
    act_motion              0
    act_motion_path         1, 2, 3, 4, 5, 6
    act_finish
",
        );
        let event = &assembled.events[0];
        assert_eq!(event.insns[1].bytes, [0x81, 0x00]);
        assert_eq!(
            event.insns[2].bytes,
            [0x81, 0x01, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x05, 0x06]
        );

        let bad = assemble(
            "\
.version 1

.main

.event event_00
    evt_actor_begin
    act_motion              1
    act_finish
",
        )
        .unwrap_err()
        .to_string();
        assert!(bad.contains("act_motion_path"), "{bad}");
    }

    #[test]
    fn evt_block_chain_is_rebuilt() {
        let assembled = parse(
            "\
.version 1

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
    evt_finish
",
        );
        let event = &assembled.events[0];
        assert_eq!(event.insns[0].bytes, [0x06, 0x0D]);
        assert_eq!(event.insns[1].decoded, Decoded::SubBlockHeader { size: 4 });
        assert_eq!(event.insns[3].decoded, Decoded::SubBlockHeader { size: 6 });
        assert_eq!(event.insns[6].decoded, Decoded::SubBlockHeader { size: 0 });
        assert_eq!(event.insns[6].bytes, [0x00]);
        assert_eq!(event.insns[7].bytes, [0x00]);
    }

    #[test]
    fn evt_block_mismatch_errors_name_the_line() {
        let mismatch = assemble(
            "\
.version 1

.main

.event event_00
    evt_block               12
    evt_block_sub           4
    end                     0
    evt_block_sub           0
    evt_nop
",
        )
        .unwrap_err()
        .to_string();
        assert!(mismatch.contains("rebuilt chain"), "{mismatch}");

        let body = assemble(
            "\
.version 1

.main

.event event_00
    evt_block               13
    evt_block_sub           5
    end                     0
    evt_block_sub           0
    evt_nop
",
        )
        .unwrap_err()
        .to_string();
        assert!(body.contains("2-byte body"), "{body}");
    }

    #[test]
    fn evt_single_clamps_its_inline_command() {
        let assembled = parse(
            "\
.version 1

.main

.event event_00
    evt_single              12
    effect                  9, 24, 0, 21, 19, 152, 254, 68, 28, 0
    evt_nop
    evt_finish
",
        );
        let event = &assembled.events[0];
        assert_eq!(event.insns[0].bytes, [0x07, 0x0C]);
        assert_eq!(event.insns[1].bytes.len(), 10);

        let overrun = assemble(
            "\
.version 1

.main

.event event_00
    evt_single              12
    scene_setup             1, 2, 3, 4
",
        )
        .unwrap_err()
        .to_string();
        assert!(overrun.contains("does not match"), "{overrun}");
    }

    #[test]
    fn evt_do_targets_are_labelled() {
        let assembled = parse(
            "\
.version 1

.main

.event event_00
    evt_do                  off_0006
    cmpb                    2, 128, 0
off_0006:
    evt_next
    evt_dountil
",
        );
        let event = &assembled.events[0];
        assert_eq!(event.insns[0].bytes, [0xFC, 0x06]);
        assert_eq!(event.insns[2].offset, event.offset + 6);
    }

    #[test]
    fn evt_do_jumps_over_255_bytes_are_rejected() {
        // The reader's `evt_do` operand is one byte, so a target more than 255
        // bytes past the inline payload cannot be encoded; the planner must
        // reject it instead of truncating.
        let mut source = String::from(
            "\
.version 1

.main

.event event_00
    evt_do                  far
    nop                     0
",
        );
        for _ in 0..200 {
            source.push_str("    nop                     0\n");
        }
        source.push_str("far:\n    evt_next\n    evt_dountil\n");
        let error = assemble(&source).unwrap_err().to_string();
        assert!(error.contains("does not fit a byte"), "{error}");
    }

    #[test]
    fn evt_single_payloads_below_one_command_are_rejected() {
        // The reader tolerates payloads 2 and 3 (an inline command truncated
        // to less than its two-byte minimum), but a valid `evt_single` carries
        // one complete command, so the assembler's floor is 4. Documented in
        // docs/m15-deviations.md.
        for payload in ["2", "3"] {
            let error = assemble(&format!(
                "\
.version 1

.main

.event event_00
    evt_single              {payload}
    evt_finish
"
            ))
            .unwrap_err()
            .to_string();
            assert!(error.contains("4-255"), "payload {payload}: {error}");
        }
    }

    #[test]
    fn odd_events_must_be_dense_and_ascending() {
        let gap = assemble(
            "\
.version 1

.main

.event event_01
    evt_finish
",
        )
        .unwrap_err()
        .to_string();
        assert!(gap.contains("event_00"), "{gap}");
    }

    #[test]
    fn size_overflow_is_an_error() {
        let mut source = String::from(".version 1\n\n.init\n.block\n");
        for _ in 0..40000 {
            source.push_str("    nop                     0\n");
        }
        source.push_str("\n.main\n");
        let error = assemble(&source).unwrap_err().to_string();
        assert!(error.contains("16-bit"), "{error}");
    }

    #[test]
    fn unknown_mnemonic_names_line_and_column() {
        let error = assemble(
            "\
.version 1

.init
.block
    frobnicate              1

.main
",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("line 5"), "{error}");
        assert!(error.contains("frobnicate"), "{error}");
    }

    #[test]
    fn missing_version_is_an_error() {
        let error = assemble(".init\n.block\n    nop 0\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains(".version"), "{error}");
    }
}
