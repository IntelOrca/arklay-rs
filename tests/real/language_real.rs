//! Byte-exactness of the shipped language seeds against a converted pack.
//!
//! Needs `ARKLAY_RE1_ROOT` and `ARKLAY_RE1_PACK`; without them the test skips.
//! The ja-JP seed is the install's own text decoded into the language-file
//! grammar, so every room message the seed carries must re-encode with the
//! Rust encoder to exactly the bytes the RDT holds. When the pack still
//! carries the retired `text/*.bin` tables, every table entry with a language
//! key is checked the same way.

use crate::common;

use arklay::language::Language;
use arklay::pack::Pack;
use arklay::rdt;
use arklay::state::RoomId;
use arklay::text::Text;

/// The embedded ja-JP source.
fn japanese_source() -> &'static str {
    arklay::language::LANGUAGE_FILES
        .iter()
        .find(|(entry, _)| entry.eq_ignore_ascii_case("language/ja-JP.txt"))
        .map(|(_, source)| *source)
        .expect("the build embeds language/ja-JP.txt")
}

/// Check one `text/*.bin` table's entries against `lookup`, which returns the
/// language entry's encoded stream for a table index.
///
/// The language stream always starts at the table entry's offset when the two
/// agree, so a prefix match avoids re-deriving every stream's end from the
/// operand grammar.
fn check_table(pack: &Pack, entry: &str, lookup: impl Fn(usize) -> Option<Vec<u8>>) -> usize {
    let Ok(data) = pack.read(entry) else {
        return 0;
    };
    let count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let mut checked = 0;
    for index in 0..count {
        let offset =
            u32::from_le_bytes(data[4 + index * 4..8 + index * 4].try_into().unwrap()) as usize;
        if offset == 0 {
            continue;
        }
        let Some(bytes) = lookup(index) else {
            continue;
        };
        assert!(
            data[offset..].starts_with(&bytes),
            "{entry} entry {index} does not match its language key"
        );
        checked += 1;
    }
    checked
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn the_japanese_seed_matches_the_executable_tables() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).expect("open the pack");
    let file = Language::parse(japanese_source()).expect("parse the embedded ja-JP source");

    let mut checked = 0;
    checked += check_table(&pack, "text/messages.bin", |index| {
        file.get(&format!("MESSAGE_{index:02X}"))
            .map(|entry| entry.encode())
    });
    checked += check_table(&pack, "text/names.bin", |index| {
        file.get(&format!("ITEM_{:02X}_NAME", index + 1))
            .map(|entry| entry.encode())
    });
    checked += check_table(&pack, "text/idesc.bin", |index| {
        file.get(&format!("ITEM_{:02X}_DESCRIPTION", index + 1))
            .map(|entry| entry.encode())
    });
    checked += check_table(&pack, "text/save.bin", |index| {
        let key = match index {
            0 => "UI_SAVE_HEADER_0",
            1 => "UI_SAVE_HEADER_1",
            2 => "UI_SAVE_HEADER_SUFFIX",
            3 => "UI_SAVE_CHARACTER_0_NAME",
            4 => "UI_SAVE_CHARACTER_1_NAME",
            5 => "UI_SAVE_EXIT_0",
            6 => "UI_SAVE_EXIT_1",
            7 => "UI_SAVE_EXIT_SUFFIX",
            8 => "UI_SAVE_SLOT_FILLED",
            9 => "UI_SAVE_SLOT_EMPTY",
            10 => "UI_SAVE_OVERWRITE_PROMPT",
            11 => "UI_SAVE_YES_NO",
            12 => "UI_SAVE_ERROR_0",
            13 => "UI_SAVE_ERROR_1",
            _ => return None,
        };
        file.get(key).map(|entry| entry.encode())
    });
    // A pack converted by this tree carries no text tables; a legacy pack
    // does, and then the check must be substantial.
    if pack.contains("text/messages.bin") {
        assert!(checked > 100, "{checked} table entries checked");
    }
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn the_japanese_seed_matches_every_room_message() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).expect("open the pack");
    let file = Language::parse(japanese_source()).expect("parse the embedded ja-JP source");
    let text = Text::load(&pack, Some("ja-JP"));
    assert_eq!(text.selected_language(), "ja-JP");

    let mut checked = 0;
    for path in pack.paths() {
        let Some(hex) = path
            .strip_prefix("room/")
            .and_then(|name| name.strip_suffix(".rdt"))
        else {
            continue;
        };
        if hex.len() != 4 {
            continue;
        }
        let Ok(id) = RoomId::parse(hex) else {
            continue;
        };
        let data = pack.read(path).expect("read the room");
        let Ok(room) = rdt::parse(&data, id) else {
            continue; // stubs and third-party rooms are not part of the corpus
        };
        // The message table occupies `block[0]` bytes: entry 0's stream starts
        // right after it, the same bound the seed generator uses.
        let Some(messages) = room.messages.as_deref() else {
            continue;
        };
        if messages.len() < 2 {
            continue;
        }
        let first = u16::from_le_bytes([messages[0], messages[1]]) as usize;
        if first == 0 || !first.is_multiple_of(2) || first > messages.len() {
            continue;
        }
        let room3 = format!("{:X}{:02X}", id.stage, id.room);
        for index in 0..(first / 2) as u16 {
            let Some(rdt_bytes) = room.message(index) else {
                continue;
            };
            let variant = format!("ROOM_{room3}{}_TEXT_{index:02X}", id.player_flag);
            let shared = format!("ROOM_{room3}_TEXT_{index:02X}");
            let entry = file.get(&variant).or_else(|| file.get(&shared));
            let Some(entry) = entry else {
                continue; // a room message the seed does not carry
            };
            let encoded = entry.encode();
            // Message streams carry the `0x01` terminator and the action byte;
            // an RDT message block can end right at the text (1170's last
            // message is cut by the section boundary), so allow the stream to
            // stop before them.
            let text_len = encoded.len().saturating_sub(2);
            let matched = if rdt_bytes.len() >= encoded.len() {
                rdt_bytes.starts_with(&encoded)
            } else {
                rdt_bytes == &encoded[..text_len]
            };
            assert!(
                matched,
                "{variant} / {shared}\n  rdt      {:02X?}\n  language {:02X?}",
                &rdt_bytes[..rdt_bytes.len().min(64)],
                &encoded[..encoded.len().min(64)]
            );
            assert_eq!(
                text.message(&room, index),
                Some(encoded.as_slice()),
                "the runtime lookup resolves {variant} / {shared}"
            );
            checked += 1;
        }
    }
    assert!(checked > 1_000, "{checked} room messages checked");
}
