//! The engine's runtime strings: the pack's language files behind the lookups
//! the message machine, the menus and the overlays use.
//!
//! Every lookup walks the catalog's fallback chain
//! ([`crate::language::Catalog`]): the selected language file, then the same
//! language in any region, then English. A room message no language carries
//! falls back to the RDT's own text and then to its registry id; item names
//! and UI labels a chain does not carry read as empty, exactly like a missing
//! entry in the tables this module replaces.
//!
//! One session draws one byte space. [`Text::script`] is the script the
//! selected language renders in — Japanese for `ja`, the Western sheet for
//! every other tag when the pack carries one, else the Japanese sheet whose
//! tight metrics stand in for the missing Western one. The catalog encodes
//! every language-file entry in that script, and the engine loads the sheet
//! [`crate::font::session_entry`] names for it, so the strings and the font
//! can never disagree.

use crate::charset::Script;
use crate::font;
use crate::language::{self, Catalog};
use crate::pack::Pack;
use crate::state::RoomState;

/// Every string source a session reads.
#[derive(Debug, Clone, Default)]
pub struct Text {
    catalog: Catalog,
    /// The byte space the selected language's strings are encoded in.
    script: Script,
    /// The byte space the pack's own room text (its RDT messages) is in.
    pack_script: Script,
}

impl Text {
    /// Load the pack's language files (every mod layer merged per key) and
    /// resolve the selected language: `language`, else the OS locale, else
    /// `en-US`.
    pub fn load(pack: &Pack, language: Option<&str>) -> Self {
        let script = script_for(pack, language);
        Self {
            catalog: Catalog::load(pack, language, script),
            script,
            pack_script: sheet_script(pack, font::DEFAULT_STEM).unwrap_or_default(),
        }
    }

    /// The tag the fallback chain starts from.
    pub fn selected_language(&self) -> &str {
        self.catalog.selected()
    }

    /// The script the session's text is encoded in, and the one its font
    /// sheet draws ([`crate::font::load_session`] loads that sheet).
    pub fn script(&self) -> Script {
        self.script
    }

    /// Whether the selected language renders with the font sheet's per-glyph
    /// tight metrics (every language except Japanese). The drawing paths pass
    /// this to the font so Latin text takes its own advances; the font itself
    /// ignores it for the Western sheet, which the original draws whole.
    pub fn uses_tight_metrics(&self) -> bool {
        self.catalog.uses_tight_metrics()
    }

    /// `KEY` along the chain, with the registry-id fallback; `None` when no
    /// language knows the key at all.
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.catalog.get(key)
    }

    /// Global message `id` (`0..64`), encoded for the message state machine.
    pub fn global(&self, id: usize) -> Option<&[u8]> {
        self.catalog.get(&format!("MESSAGE_{id:02X}"))
    }

    /// Item-name entry for the `item_id`; `None` when the item has no name.
    pub fn item_name(&self, item_id: u16) -> Option<&[u8]> {
        self.catalog.lookup(&format!("ITEM_{item_id:02X}_NAME"))
    }

    /// The generic name shown before the item is examined. Only items whose
    /// generic name differs from the real one have an entry; the caller falls
    /// back to [`Text::item_name`].
    pub fn item_name_unchecked(&self, item_id: u16) -> Option<&[u8]> {
        self.catalog
            .lookup(&format!("ITEM_{item_id:02X}_NAME_UNCHECKED"))
    }

    /// Item-description entry for the `item_id`.
    pub fn description(&self, item_id: u16) -> Option<&[u8]> {
        self.catalog
            .lookup(&format!("ITEM_{item_id:02X}_DESCRIPTION"))
    }

    /// The message selected by `id`: bit `0x40` picks the global table, and
    /// the low six bits are the index within either table.
    ///
    /// Room messages resolve the RDT variant's key, then the shared room key,
    /// then the RDT's own text, then the registry id.
    pub fn message<'a>(&'a self, room: &'a RoomState, id: u16) -> Option<&'a [u8]> {
        if id & 0x40 != 0 {
            return self.global(usize::from(id & 0x3F));
        }
        let room3 = format!("{:X}{:02X}", room.stage, room.room);
        let variant = format!("ROOM_{room3}{}_TEXT_{id:02X}", room.player_flag);
        if let Some(bytes) = self.catalog.lookup(&variant) {
            return Some(bytes);
        }
        let shared = format!("ROOM_{room3}_TEXT_{id:02X}");
        if let Some(bytes) = self.catalog.lookup(&shared) {
            return Some(bytes);
        }
        // The RDT's own text is encoded in the pack's own script; use it only
        // when the session draws that script, or its bytes would render as
        // the wrong characters. A language that lacks the string then reads
        // as its registry id, like a key no language carries at all.
        if self.script == self.pack_script
            && let Some(bytes) = room.message(id)
        {
            return Some(bytes);
        }
        self.catalog
            .key_display(&variant)
            .or_else(|| self.catalog.key_display(&shared))
    }

    /// The room's display name (`ROOM_<room3>_NAME`), shown by the port-only
    /// F1 overlay.
    pub fn room_name(&self, room3: &str) -> Option<&[u8]> {
        self.catalog.lookup(&format!("ROOM_{room3}_NAME"))
    }

    /// The save screen's shorthand label for a room: `ROOM_<room3>_NAME_SAVE`,
    /// falling back to its display name.
    pub fn room_label(&self, stage: u8, room: u8) -> Option<&[u8]> {
        let room3 = format!("{stage:X}{room:02X}");
        self.catalog
            .lookup(&format!("ROOM_{room3}_NAME_SAVE"))
            .or_else(|| self.room_name(&room3))
    }

    /// The save-screen strings.
    pub fn save(&self) -> SaveStrings<'_> {
        SaveStrings { text: self }
    }

    /// A port-only UI string (`UI_*`); an unknown key reads as empty.
    pub fn ui(&self, key: &str) -> &[u8] {
        self.catalog.lookup(key).unwrap_or(&[])
    }

    /// Test helper: the sources embedded in the binary, selected as `en-US`.
    #[cfg(test)]
    pub(crate) fn embedded() -> Self {
        let pack = crate::pack::Pack::from_bytes(
            crate::pack::PackWriter::new()
                .to_bytes()
                .expect("an empty pack writes"),
        )
        .expect("an empty pack parses");
        Self::load(&pack, Some("en-US"))
    }
}

/// The script a session for `requested` draws in.
///
/// The language decides: `ja` (in any region) keeps the Japanese sheet, every
/// other tag prefers the Western one. When the pack does not carry that
/// sheet, the default sheet's own script decides — the same sheet
/// [`crate::font::session_entry`] loads — so the text and the font agree even
/// for a pack converted from a single tree.
pub fn script_for(pack: &Pack, requested: Option<&str>) -> Script {
    let script = if language::is_japanese(&language::resolve_tag(requested)) {
        Script::Japanese
    } else {
        Script::Latin
    };
    sheet_script(pack, font::session_entry(pack, script)).unwrap_or_default()
}

/// The script a sheet's stem selects, read from its header where it has one.
fn sheet_script(pack: &Pack, stem: &str) -> Option<Script> {
    let asset = pack
        .read_asset(stem, crate::pack::AssetClass::Image)
        .ok()
        .flatten()?;
    let (width, _) = crate::assets::image_dimensions(&asset).ok()?;
    Some(if width >= font::JPN_SHEET_WIDTH {
        Script::Japanese
    } else {
        Script::Latin
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::PackWriter;

    fn pack_with(files: &[(&str, &str)]) -> Pack {
        let mut writer = PackWriter::new();
        for (path, source) in files {
            writer.add(path, source.as_bytes().to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    /// A minimal, decodable 4bpp TIM whose image block declares `width_units`
    /// 16-bit units (four pixels each): 192 units is the 768-wide Japanese
    /// sheet, 64 units the 256-wide Western one.
    fn font_tim(width_units: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(0x10u32.to_le_bytes()); // magic
        out.extend(0x08u32.to_le_bytes()); // 4bpp + CLUT
        let clut_len = 12 + 16 * 2;
        out.extend((clut_len as u32).to_le_bytes());
        out.extend([0u8; 4]); // CLUT x, y
        out.extend(16u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend([0u8; 16 * 2]); // palette
        let width = width_units as usize * 4;
        out.extend(((12 + width / 2) as u32).to_le_bytes());
        out.extend([0u8; 4]); // image x, y
        out.extend(width_units.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(vec![0u8; width / 2]); // pixels
        out
    }

    fn pack_with_bytes(files: &[(&str, &[u8])]) -> Pack {
        let mut writer = PackWriter::new();
        for (path, bytes) in files {
            writer.add(path, bytes.to_vec()).unwrap();
        }
        Pack::from_bytes(writer.to_bytes().unwrap()).unwrap()
    }

    #[test]
    fn the_script_follows_the_language_and_the_packs_sheets() {
        let jpn = font_tim(192);
        let latin = font_tim(64);

        // A pack converted from the Japanese tree keeps its sheet for every
        // language: non-Japanese tags fall back to it (and its tight metrics).
        let pack = pack_with_bytes(&[("font/font.tim", &jpn)]);
        assert_eq!(Text::load(&pack, Some("en-US")).script(), Script::Japanese);
        assert_eq!(Text::load(&pack, Some("ja-JP")).script(), Script::Japanese);

        // With the Western sheet packed as the override, every non-Japanese
        // tag draws with it while `ja` keeps the Japanese one.
        let pack = pack_with_bytes(&[("font/font.tim", &jpn), ("font/latin.tim", &latin)]);
        assert_eq!(Text::load(&pack, Some("en-US")).script(), Script::Latin);
        assert_eq!(Text::load(&pack, Some("fr-FR")).script(), Script::Latin);
        assert_eq!(Text::load(&pack, Some("ja-JP")).script(), Script::Japanese);
        assert_eq!(Text::load(&pack, Some("ja")).script(), Script::Japanese);

        // A pack converted from a Western tree has the Western sheet as its
        // default; a Japanese tag then falls back to it too.
        let pack = pack_with_bytes(&[("font/font.tim", &latin)]);
        assert_eq!(Text::load(&pack, Some("en-US")).script(), Script::Latin);
        assert_eq!(Text::load(&pack, Some("ja-JP")).script(), Script::Latin);

        // Both sheets as script-specific entries with a Japanese default.
        let pack = pack_with_bytes(&[
            ("font/font.tim", &jpn),
            ("font/jpn.tim", &jpn),
            ("font/latin.tim", &latin),
        ]);
        assert_eq!(Text::load(&pack, Some("en-US")).script(), Script::Latin);
        assert_eq!(Text::load(&pack, Some("ja-JP")).script(), Script::Japanese);
    }

    #[test]
    fn the_rdt_fallback_needs_the_packs_own_script() {
        // A Japanese pack with the Western sheet packed for English: the RDT
        // text is Japanese bytes, so an untranslated message must not be
        // drawn with the Western font. It reads as its registry id instead.
        let jpn = font_tim(192);
        let latin = font_tim(64);
        let pack = pack_with_bytes(&[
            ("font/font.tim", &jpn),
            ("font/latin.tim", &latin),
            ("language/en-US.txt", b"ROOM_FFF_TEXT_00 = \"A\"\n"),
            // A third language's file carries the key's name, so the
            // registry-id fallback has something to draw; neither selected
            // chain carries the entry itself.
            ("language/fr-FR.txt", b"ROOM_FFF_TEXT_01 = \"A\"\n"),
        ]);
        let text = Text::load(&pack, Some("en-US"));
        assert_eq!(text.script(), Script::Latin);
        let room = RoomState {
            stage: 0xF,
            room: 0xFF,
            player_flag: 1,
            // Offset table with no id 0 and `0xCC` at id 1 (offset 4).
            messages: Some(vec![0x00, 0x00, 0x04, 0x00, 0xCC, 0x01, 0x00]),
            ..RoomState::default()
        };
        // The translated key is encoded in the session's script.
        assert_eq!(text.message(&room, 0), Some([0x1D, 0x01, 0x00].as_slice()));
        // The untranslated one falls back to its key name, not to the RDT.
        let mut key = Vec::new();
        crate::charset::encode("ROOM_FFF_TEXT_01", Script::Latin, &mut key);
        assert_eq!(text.message(&room, 1), Some(key.as_slice()));

        // The same pack selected as Japanese draws the RDT's own bytes.
        let text = Text::load(&pack, Some("ja-JP"));
        assert_eq!(text.script(), Script::Japanese);
        assert_eq!(text.message(&room, 1), Some([0xCC, 0x01, 0x00].as_slice()));
    }

    #[test]
    fn room_messages_prefer_the_variant_then_the_shared_key_then_the_rdt() {
        // A room no shipped language knows, so only the pack's keys and the
        // RDT can answer.
        let pack = pack_with(&[(
            "language/en-US.txt",
            "ROOM_FFF_TEXT_00 = \"A\"\nROOM_FFF1_TEXT_00 = \"B\"\n",
        )]);
        let text = Text::load(&pack, Some("en-US"));
        let room = RoomState {
            stage: 0xF,
            room: 0xFF,
            player_flag: 1,
            // Offset table with no id 0 and `0xCC` at id 1 (offset 4).
            messages: Some(vec![0x00, 0x00, 0x04, 0x00, 0xCC, 0x01, 0x00]),
            ..RoomState::default()
        };
        // The RDT variant's key wins.
        assert_eq!(text.message(&room, 0), Some([0x1E, 0x01, 0x00].as_slice()));
        // No language key: the RDT stream is used.
        assert_eq!(text.message(&room, 1), Some([0xCC, 0x01, 0x00].as_slice()));
    }

    #[test]
    fn item_names_and_descriptions_are_keyed_by_item_id() {
        // Item ids no shipped language defines.
        let pack = pack_with(&[(
            "language/en-US.txt",
            "ITEM_FE_NAME = \"A\"\nITEM_FE_NAME_UNCHECKED = \"B\"\nITEM_FE_DESCRIPTION = \"C\"\n",
        )]);
        let text = Text::load(&pack, Some("en-US"));
        assert_eq!(text.item_name(0xFE), Some([0x1D, 0x07].as_slice()));
        assert_eq!(
            text.item_name_unchecked(0xFE),
            Some([0x1E, 0x07].as_slice())
        );
        assert_eq!(text.description(0xFE), Some([0x1F, 0x01, 0x00].as_slice()));
        assert_eq!(text.item_name(0xFF), None);
    }

    #[test]
    fn room_labels_fall_back_to_the_display_name() {
        // Stage `F` rooms exist in no language file.
        let pack = pack_with(&[(
            "language/en-US.txt",
            "ROOM_F00_NAME = \"A\"\nROOM_F01_NAME_SAVE = \"B\"\n",
        )]);
        let text = Text::load(&pack, Some("en-US"));
        assert_eq!(text.room_name("F00"), Some([0x1D, 0x01].as_slice()));
        assert_eq!(text.room_label(0xF, 0x00), Some([0x1D, 0x01].as_slice()));
        assert_eq!(text.room_label(0xF, 0x01), Some([0x1E, 0x01].as_slice()));
        assert_eq!(text.room_label(0xF, 0x02), None);
    }

    #[test]
    fn the_save_strings_read_their_keys() {
        let pack = pack_with(&[(
            "language/en-US.txt",
            "UI_SAVE_HEADER_0 = \"A\"\nUI_SAVE_SLOT_EMPTY = \"B\"\n",
        )]);
        let text = Text::load(&pack, Some("en-US"));
        let save = text.save();
        assert_eq!(save.header(0), [0x1D, 0x01]);
        assert!(!save.header(1).is_empty(), "the embedded file fills it");
        assert_eq!(save.slot_empty(), [0x1E, 0x01]);
    }
}

/// The save-screen strings, in their documented table order.
#[derive(Debug, Clone, Copy)]
pub struct SaveStrings<'a> {
    text: &'a Text,
}

impl SaveStrings<'_> {
    /// The header of screen mode `0` (save) or `1` (load).
    pub fn header(&self, mode: usize) -> &[u8] {
        self.text.ui(if mode == 0 {
            "UI_SAVE_HEADER_0"
        } else {
            "UI_SAVE_HEADER_1"
        })
    }

    /// The shared `     GAME` suffix of both headers.
    pub fn header_suffix(&self) -> &[u8] {
        self.text.ui("UI_SAVE_HEADER_SUFFIX")
    }

    /// The name of character `0` (Chris) or `1` (Jill).
    pub fn character_name(&self, character: usize) -> &[u8] {
        self.text.ui(if character == 0 {
            "UI_SAVE_CHARACTER_0_NAME"
        } else {
            "UI_SAVE_CHARACTER_1_NAME"
        })
    }

    /// The exit verb of screen mode `0` (save) or `1` (load).
    pub fn exit(&self, mode: usize) -> &[u8] {
        self.text.ui(if mode == 0 {
            "UI_SAVE_EXIT_0"
        } else {
            "UI_SAVE_EXIT_1"
        })
    }

    /// The `   しない` suffix drawn over the exit verb.
    pub fn exit_suffix(&self) -> &[u8] {
        self.text.ui("UI_SAVE_EXIT_SUFFIX")
    }

    /// The filled-slot row template (`   /  /     /  `).
    pub fn slot_filled(&self) -> &[u8] {
        self.text.ui("UI_SAVE_SLOT_FILLED")
    }

    /// The empty-slot row template (`ーーー/ーー/ーーーーー/ーー`).
    pub fn slot_empty(&self) -> &[u8] {
        self.text.ui("UI_SAVE_SLOT_EMPTY")
    }

    /// The overwrite confirmation line.
    pub fn overwrite_prompt(&self) -> &[u8] {
        self.text.ui("UI_SAVE_OVERWRITE_PROMPT")
    }

    /// The `はい  いいえ` line the confirm cursor sits on.
    pub fn yes_no(&self) -> &[u8] {
        self.text.ui("UI_SAVE_YES_NO")
    }

    /// Error line `0` or `1`.
    pub fn error(&self, line: usize) -> &[u8] {
        self.text.ui(if line == 0 {
            "UI_SAVE_ERROR_0"
        } else {
            "UI_SAVE_ERROR_1"
        })
    }

    /// The shorthand location label of the room a save was made in.
    pub fn location(&self, stage: u8, room: u8) -> &[u8] {
        self.text.room_label(stage, room).unwrap_or(&[])
    }
}
