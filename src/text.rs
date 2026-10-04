//! Executable text tables: global messages, item names, generic names, item
//! descriptions and the save-screen strings.
//!
//! `convert-game` extracts the tables from the Japanese executable into five
//! `text/*.bin` pack entries. Each file is a little-endian `u32` count,
//! `count` `u32` byte offsets from the start of the file, then the raw encoded
//! streams; an offset of zero marks a missing entry. Global messages and item
//! descriptions end at their `0x01` terminator and keep the action byte that
//! follows it; item names end at `0x07`; the save-screen strings are plain
//! `0x01`-terminated streams.
//!
//! Missing files are a warning rather than a failure: room messages still
//! work and menu strings simply read empty.

use anyhow::{Context, Result, bail};

use crate::pack::Pack;
use crate::state::RoomState;

/// Pack entry of the global message table (64 entries).
pub const MESSAGES_ENTRY: &str = "text/messages.bin";
/// Pack entry of the item-name table (128 entries, item id - 1).
pub const NAMES_ENTRY: &str = "text/names.bin";
/// Pack entry of the generic item-name table (16 entries, by class).
pub const UNKNOWN_ENTRY: &str = "text/unknown.bin";
/// Pack entry of the item-description table (79 entries, item id - 1).
pub const DESCRIPTIONS_ENTRY: &str = "text/idesc.bin";
/// Pack entry of the save-screen strings.
pub const SAVE_ENTRY: &str = "text/save.bin";

/// How one encoded stream is terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Message grammar: stops at the `0x01` terminator and keeps the action
    /// byte (`0` = wait for input, otherwise an auto-dismiss frame count).
    Message,
    /// Item name: stops at the `0x07` return marker.
    Name,
    /// Plain `0x01`-terminated stream with no action byte (save strings).
    Plain,
}

/// One text table: the encoded stream of every entry, or `None` for a missing
/// entry (the file's zero offset).
#[derive(Debug, Clone, Default)]
pub struct Table {
    entries: Vec<Option<Vec<u8>>>,
}

impl Table {
    /// Parse a message-grammar table (`text/messages.bin`, `text/idesc.bin`).
    pub fn parse_message(data: &[u8]) -> Result<Self> {
        Self::parse(data, Kind::Message)
    }

    /// Parse an item-name table (`text/names.bin`, `text/unknown.bin`).
    pub fn parse_name(data: &[u8]) -> Result<Self> {
        Self::parse(data, Kind::Name)
    }

    /// Parse a plain `0x01`-terminated table (`text/save.bin`).
    pub fn parse_plain(data: &[u8]) -> Result<Self> {
        Self::parse(data, Kind::Plain)
    }

    fn parse(data: &[u8], kind: Kind) -> Result<Self> {
        let count = read_u32(data, 0).context("text table is missing its count")? as usize;
        let table_len = count
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(4))
            .context("text table entry count overflows")?;
        if data.len() < table_len {
            bail!(
                "text table declares {count} entries but is only {} bytes",
                data.len()
            );
        }

        let mut entries = Vec::with_capacity(count);
        for index in 0..count {
            let offset = read_u32(data, 4 + index * 4)? as usize;
            if offset == 0 {
                entries.push(None);
                continue;
            }
            let stream = data.get(offset..).with_context(|| {
                format!(
                    "text entry {index} points at offset 0x{offset:X}, past the {}-byte table",
                    data.len()
                )
            })?;
            let end = match kind {
                Kind::Name => scan_terminator(stream, 0x07),
                Kind::Plain => scan_terminator(stream, 0x01),
                Kind::Message => scan_message(stream),
            }
            .with_context(|| {
                format!("text entry {index} at offset 0x{offset:X} is unterminated")
            })?;
            let end = if kind == Kind::Message {
                end.checked_add(1)
                    .filter(|&end| end <= stream.len())
                    .with_context(|| {
                        format!("text entry {index} at offset 0x{offset:X} has no action byte")
                    })?
            } else {
                end
            };
            entries.push(Some(stream[..end].to_vec()));
        }
        Ok(Self { entries })
    }

    /// Number of entries, missing ones included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table has no entries at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The encoded bytes of entry `id`, or `None` when the entry is missing or
    /// out of range.
    pub fn get(&self, id: usize) -> Option<&[u8]> {
        self.entries.get(id)?.as_deref()
    }
}

/// The save-screen strings, in their documented table order.
///
/// Read the fields, or use the indexing accessors, which return an empty slice
/// for a missing entry.
#[derive(Debug, Clone, Default)]
pub struct SaveStrings {
    /// `SAVE`, `LOAD` (indexed by screen mode).
    pub headers: Vec<Vec<u8>>,
    /// The shared `     GAME` suffix of both headers.
    pub header_suffix: Vec<u8>,
    /// Chris, Jill (indexed by character id).
    pub char_names: Vec<Vec<u8>>,
    /// The exit-row verbs: `セーブ`, `ロード` (indexed by screen mode).
    pub exits: Vec<Vec<u8>>,
    /// The `   しない` suffix drawn over the exit verb.
    pub exit_suffix: Vec<u8>,
    /// The filled-slot row template (`   /  /     /  `).
    pub filled_slot: Vec<u8>,
    /// The empty-slot row template (`ーーー/ーー/ーーーーー/ーー`).
    pub empty_slot: Vec<u8>,
    /// The overwrite confirmation line.
    pub overwrite_prompt: Vec<u8>,
    /// The `はい  いいえ` line the confirm cursor sits on.
    pub yes_no: Vec<u8>,
    /// The two error lines (`no free space` and its blank second line).
    pub errors: Vec<Vec<u8>>,
    /// The seven stage/room location names.
    pub locations: Vec<Vec<u8>>,
}

impl SaveStrings {
    fn from_table(table: &Table) -> Self {
        let entry = |id: usize| table.get(id).unwrap_or(&[]).to_vec();
        Self {
            headers: vec![entry(0), entry(1)],
            header_suffix: entry(2),
            char_names: vec![entry(3), entry(4)],
            exits: vec![entry(5), entry(6)],
            exit_suffix: entry(7),
            filled_slot: entry(8),
            empty_slot: entry(9),
            overwrite_prompt: entry(10),
            yes_no: entry(11),
            errors: vec![entry(12), entry(13)],
            locations: (14..21).map(entry).collect(),
        }
    }

    /// The header of screen mode `0` (save) or `1` (load).
    pub fn header(&self, mode: usize) -> &[u8] {
        self.headers.get(mode).map_or(&[], Vec::as_slice)
    }

    /// The name of character `0` (Chris) or `1` (Jill).
    pub fn char_name(&self, character: usize) -> &[u8] {
        self.char_names.get(character).map_or(&[], Vec::as_slice)
    }

    /// The exit verb of screen mode `0` (save) or `1` (load).
    pub fn exit(&self, mode: usize) -> &[u8] {
        self.exits.get(mode).map_or(&[], Vec::as_slice)
    }

    /// Error line `0` or `1`.
    pub fn error(&self, line: usize) -> &[u8] {
        self.errors.get(line).map_or(&[], Vec::as_slice)
    }

    /// Location name `index` (`0..7`).
    pub fn location(&self, index: usize) -> &[u8] {
        self.locations.get(index).map_or(&[], Vec::as_slice)
    }
}

/// Every text table the engine reads, loaded from a game pack.
#[derive(Debug, Clone, Default)]
pub struct Text {
    messages: Table,
    names: Table,
    unknown: Table,
    descriptions: Table,
    save: SaveStrings,
}

impl Text {
    /// Load the five `text/*.bin` entries. A missing or malformed file is
    /// reported as a warning and reads as an empty table.
    pub fn load(pack: &Pack) -> Self {
        Self {
            messages: load_table(pack, MESSAGES_ENTRY, Kind::Message),
            names: load_table(pack, NAMES_ENTRY, Kind::Name),
            unknown: load_table(pack, UNKNOWN_ENTRY, Kind::Name),
            descriptions: load_table(pack, DESCRIPTIONS_ENTRY, Kind::Message),
            save: SaveStrings::from_table(&load_table(pack, SAVE_ENTRY, Kind::Plain)),
        }
    }

    /// Global message `id` (`0..64`), encoded for the message state machine.
    pub fn global(&self, id: usize) -> Option<&[u8]> {
        self.messages.get(id)
    }

    /// Item-name entry for the 1-based `item_id`; `0` has no name.
    pub fn item_name(&self, item_id: u16) -> Option<&[u8]> {
        self.names.get(usize::from(item_id).checked_sub(1)?)
    }

    /// Generic name for item class `class` (`0..16`).
    pub fn unknown_name(&self, class: usize) -> Option<&[u8]> {
        self.unknown.get(class)
    }

    /// Item-description entry for the 1-based `item_id`; `0` has none.
    pub fn description(&self, item_id: u16) -> Option<&[u8]> {
        self.descriptions.get(usize::from(item_id).checked_sub(1)?)
    }

    /// The save-screen strings.
    pub fn save(&self) -> &SaveStrings {
        &self.save
    }

    /// The message selected by `id`: bit `0x40` picks the global table, and the
    /// low six bits are the index within either table.
    pub fn message<'a>(&'a self, room: &'a RoomState, id: u16) -> Option<&'a [u8]> {
        if id & 0x40 != 0 {
            self.global(usize::from(id & 0x3F))
        } else {
            room.message(id)
        }
    }

    /// Number of global message entries (missing ones included).
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Number of item-name entries (missing ones included).
    pub fn item_name_count(&self) -> usize {
        self.names.len()
    }

    /// Number of generic-name entries (missing ones included).
    pub fn unknown_count(&self) -> usize {
        self.unknown.len()
    }

    /// Number of item-description entries (missing ones included).
    pub fn description_count(&self) -> usize {
        self.descriptions.len()
    }
}

fn load_table(pack: &Pack, path: &str, kind: Kind) -> Table {
    match pack.read(path) {
        Ok(data) => match Table::parse(data, kind) {
            Ok(table) => table,
            Err(error) => {
                eprintln!("warning: invalid text table {path}: {error:#}");
                Table::default()
            }
        },
        Err(error) => {
            eprintln!("warning: missing text table {path}: {error:#}");
            Table::default()
        }
    }
}

/// Encode entries in the `text/*.bin` format: a `u32` count, one file-relative
/// `u32` offset per entry (zero for a missing entry) and the raw streams.
pub(crate) fn encode_table(entries: &[Option<Vec<u8>>]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    let mut offset = 4 + entries.len() as u32 * 4;
    for entry in entries {
        match entry {
            Some(bytes) => {
                data.extend_from_slice(&offset.to_le_bytes());
                offset += bytes.len() as u32;
            }
            None => data.extend_from_slice(&0u32.to_le_bytes()),
        }
    }
    for entry in entries.iter().flatten() {
        data.extend_from_slice(entry);
    }
    data
}

/// Scan one message stream up to and including its `0x01` terminator,
/// consuming the operand bytes of the tags the message state machine consumes.
pub(crate) fn scan_message(data: &[u8]) -> Option<usize> {
    let mut position = 0usize;
    loop {
        let byte = *data.get(position)?;
        match byte {
            0x01 => return Some(position + 1),
            0x03 | 0x05 | 0x06 | 0xF8 | 0xF9 | 0xFA => position += 2,
            0x04 => {
                position += 1;
                if *data.get(position)? == 0 {
                    position += 1;
                    loop {
                        match *data.get(position)? {
                            0x04 => break,
                            0x05 | 0x06 | 0xF8 | 0xF9 | 0xFA => position += 1,
                            _ => {}
                        }
                        position += 1;
                    }
                    position += 1;
                }
                position += 1;
            }
            _ => position += 1,
        }
    }
}

/// Scan a plain stream up to and including the first `terminator` byte.
pub(crate) fn scan_terminator(data: &[u8], terminator: u8) -> Option<usize> {
    data.iter()
        .position(|&byte| byte == terminator)
        .map(|position| position + 1)
}

/// Read a little-endian `u32` from `data`.
fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let raw = data
        .get(offset..offset + 4)
        .context("text table is truncated")?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode entries in the `text/*.bin` format.
    fn encode(entries: &[Option<&[u8]>]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let mut offset = 4 + entries.len() as u32 * 4;
        for entry in entries {
            match entry {
                Some(bytes) => {
                    data.extend_from_slice(&offset.to_le_bytes());
                    offset += bytes.len() as u32;
                }
                None => data.extend_from_slice(&0u32.to_le_bytes()),
            }
        }
        for entry in entries.iter().flatten() {
            data.extend_from_slice(entry);
        }
        data
    }

    #[test]
    fn parses_messages_and_keeps_the_action_byte() {
        // Item-name placeholder, a glyph, an extended glyph, a skip block,
        // then the terminator and its auto-dismiss action byte.
        let message: &[u8] = &[
            0x05, 0x01, 0x06, 0x00, 0x05, 0x00, 0x83, 0xF9, 0x52, 0x04, 0x00, 0x0C, 0x04, 0x02,
            0x08, 0x02, 0x01, 0x2A,
        ];
        let data = encode(&[Some(message)]);

        let table = Table::parse_message(&data).unwrap();

        assert_eq!(table.len(), 1);
        assert_eq!(table.get(0), Some(message));
    }

    #[test]
    fn parses_names_up_to_the_return_marker() {
        let name: &[u8] = &[0xB0, 0xD4, 0xE4, 0x07];
        let data = encode(&[Some(name)]);

        let table = Table::parse_name(&data).unwrap();

        assert_eq!(table.get(0), Some(name));
    }

    #[test]
    fn zero_offsets_read_as_missing_entries() {
        let data = encode(&[Some(&[0x0C, 0x01, 0x00][..]), None]);

        let table = Table::parse_message(&data).unwrap();

        assert_eq!(table.len(), 2);
        assert_eq!(table.get(0), Some(&[0x0C, 0x01, 0x00][..]));
        assert_eq!(table.get(1), None);
        assert_eq!(table.get(2), None);
    }

    #[test]
    fn plain_streams_stop_at_the_terminator() {
        let data = encode(&[Some(&[0x00, 0xFB, 0x38, 0x01][..])]);

        let table = Table::parse_plain(&data).unwrap();

        assert_eq!(table.get(0), Some(&[0x00, 0xFB, 0x38, 0x01][..]));
    }

    #[test]
    fn unterminated_streams_are_rejected() {
        let data = encode(&[Some(&[0x0C, 0x0D][..])]);
        assert!(Table::parse_message(&data).is_err());
        assert!(Table::parse_name(&data).is_err());
        assert!(Table::parse_plain(&data).is_err());

        assert!(Table::parse_message(&[0x01]).is_err());
    }

    fn sample_text() -> Text {
        let messages = Table::parse_message(&encode(&[
            Some(&[0x0C, 0x01, 0x00][..]),
            Some(&[0x0D, 0x01, 0x05][..]),
        ]))
        .unwrap();
        let names = Table::parse_name(&encode(&[Some(&[0xAA, 0x07][..]), Some(&[0xBB, 0x07][..])]))
            .unwrap();
        let unknown = Table::parse_name(&encode(&[Some(&[0xCC, 0x07][..])])).unwrap();
        let descriptions = Table::parse_message(&encode(&[
            Some(&[0xDD, 0x01, 0x00][..]),
            Some(&[0xEE, 0x01, 0x00][..]),
        ]))
        .unwrap();
        let save_entries: Vec<Vec<u8>> = (0..21)
            .map(|index| vec![0x41 + index as u8, 0x01])
            .collect();
        let save_refs: Vec<Option<&[u8]>> = save_entries
            .iter()
            .map(|entry| Some(entry.as_slice()))
            .collect();
        let save = SaveStrings::from_table(&Table::parse_plain(&encode(&save_refs)).unwrap());
        Text {
            messages,
            names,
            unknown,
            descriptions,
            save,
        }
    }

    #[test]
    fn item_lookups_are_one_based() {
        let text = sample_text();

        assert_eq!(text.item_name(1), Some(&[0xAA, 0x07][..]));
        assert_eq!(text.item_name(2), Some(&[0xBB, 0x07][..]));
        assert_eq!(text.item_name(0), None);
        assert_eq!(text.item_name(3), None);
        assert_eq!(text.unknown_name(0), Some(&[0xCC, 0x07][..]));
        assert_eq!(text.unknown_name(1), None);
        assert_eq!(text.description(1), Some(&[0xDD, 0x01, 0x00][..]));
        assert_eq!(text.description(2), Some(&[0xEE, 0x01, 0x00][..]));
        assert_eq!(text.description(0), None);
        assert_eq!(text.description(3), None);
        assert_eq!(text.global(0), Some(&[0x0C, 0x01, 0x00][..]));
        assert_eq!(text.global(2), None);
        assert_eq!(text.message_count(), 2);
        assert_eq!(text.item_name_count(), 2);
        assert_eq!(text.unknown_count(), 1);
        assert_eq!(text.description_count(), 2);
    }

    #[test]
    fn save_strings_follow_the_documented_order() {
        let save = sample_text().save;
        assert_eq!(save.header(0), &[0x41, 0x01]);
        assert_eq!(save.header(1), &[0x42, 0x01]);
        assert_eq!(save.header_suffix, &[0x43, 0x01]);
        assert_eq!(save.char_name(0), &[0x44, 0x01]);
        assert_eq!(save.char_name(1), &[0x45, 0x01]);
        assert_eq!(save.exit(0), &[0x46, 0x01]);
        assert_eq!(save.exit(1), &[0x47, 0x01]);
        assert_eq!(save.exit_suffix, &[0x48, 0x01]);
        assert_eq!(save.filled_slot, &[0x49, 0x01]);
        assert_eq!(save.empty_slot, &[0x4A, 0x01]);
        assert_eq!(save.overwrite_prompt, &[0x4B, 0x01]);
        assert_eq!(save.yes_no, &[0x4C, 0x01]);
        assert_eq!(save.error(0), &[0x4D, 0x01]);
        assert_eq!(save.error(1), &[0x4E, 0x01]);
        for index in 0..7 {
            assert_eq!(save.location(index), &[0x4F + index as u8, 0x01]);
        }
        assert_eq!(save.header(2), &[0u8; 0]);
        assert_eq!(save.location(7), &[0u8; 0]);
    }

    #[test]
    fn message_id_bit_6_selects_the_global_table() {
        let text = sample_text();
        let mut block = Vec::new();
        block.extend_from_slice(&4u16.to_le_bytes());
        block.extend_from_slice(&9u16.to_le_bytes());
        block.extend_from_slice(&[0x0C, 0x01, 0x00]);
        block.extend_from_slice(&[0x0D, 0x01, 0x00]);
        let room = RoomState {
            messages: Some(block),
            ..RoomState::default()
        };

        // Room message 0 runs to the end of the block; the global table slice
        // is exact.
        assert!(
            text.message(&room, 0)
                .unwrap()
                .starts_with(&[0x0C, 0x01, 0x00])
        );
        assert_eq!(text.message(&room, 0x40), Some(&[0x0C, 0x01, 0x00][..]));
        assert_eq!(text.message(&room, 0x41), Some(&[0x0D, 0x01, 0x05][..]));
        assert_eq!(text.message(&room, 0x42), None);
    }
}
