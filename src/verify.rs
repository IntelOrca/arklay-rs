//! `arklay verify`: parse every known entry of a game pack and report a
//! per-format table.
//!
//! The classifier is path-driven: a pack path prefix or extension selects the
//! parser the engine would use for that entry. An entry whose format is not in
//! the table is *opaque*: it is counted and never fails, so a pack carrying a
//! future or third-party asset cannot break the command. That includes a
//! `text/*.bin` name with no table parser and a `room/*.rdt` whose stem is not
//! a four-digit room id. `--strict` turns an opaque entry into a failure.
//!
//! For every RDT the command checks that the room has at least one camera
//! cut, that each cut's background (`roomcut/{room}_{camera:03}.bmp`) is
//! present in the merged pack, and that the embedded init/main/event scripts
//! parse, so a room the engine cannot load fails even when every present entry
//! parses.
//!
//! No failure aborts the walk: the report lists every failing path with its
//! error, and the caller exits non-zero when any known-format entry failed.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;

use anyhow::{Context, Result, bail};

use crate::pack::Pack;
use crate::state::RoomId;

/// The parser family a pack entry is validated with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Format {
    /// `manifest.toml`, the pack manifest.
    Manifest,
    /// `room/*.rdt`, a room plus its embedded SCD streams.
    Rdt,
    /// `scd/*.scd`, a standalone script override.
    Scd,
    /// `roomcut/*.bmp`, a camera background.
    Cut,
    /// `roommask/*.bmp`, a camera mask page.
    Mask,
    /// Any other `*.bmp`.
    Bmp,
    /// `*.tim`, mode-dispatched.
    Tim,
    /// `*.ivm`, an item-view model.
    Ivm,
    /// `*.emd`, a character model.
    Emd,
    /// `*.emw`, a weapon model.
    Emw,
    /// `*.tmd`, a held-weapon mesh.
    Tmd,
    /// `*.dor`, a door animation.
    Dor,
    /// `*.wav`, a RIFF/WAVE sample.
    Wav,
    /// `text/*.bin`, an executable text table.
    Text,
    /// `map/tables.bin`, the map tables blob.
    MapTables,
    /// `data/combat.bin`, the shared combat table blob.
    Combat,
    /// `data/bio_card.dat`, the save prefix.
    BioCard,
    /// `movie/*.avi`, a film.
    Avi,
    /// `*.lua`, a sandboxed script source.
    Lua,
    /// Any entry outside the known table.
    Opaque,
}

impl Format {
    /// The stable report name of the format.
    pub fn name(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Rdt => "rdt",
            Self::Scd => "scd",
            Self::Cut => "roomcut",
            Self::Mask => "roommask",
            Self::Bmp => "bmp",
            Self::Tim => "tim",
            Self::Ivm => "ivm",
            Self::Emd => "emd",
            Self::Emw => "emw",
            Self::Tmd => "tmd",
            Self::Dor => "dor",
            Self::Wav => "wav",
            Self::Text => "text",
            Self::MapTables => "maptables",
            Self::Combat => "combat",
            Self::BioCard => "biocard",
            Self::Avi => "avi",
            Self::Lua => "lua",
            Self::Opaque => "opaque",
        }
    }

    /// Whether the format has a parser and a failure is a verification error.
    pub fn known(self) -> bool {
        self != Self::Opaque
    }
}

/// Select the format for one pack path.
pub fn classify(path: &str) -> Format {
    let lower = path.to_ascii_lowercase();
    if lower == crate::manifest::ENTRY {
        return Format::Manifest;
    }
    if lower == "map/tables.bin" {
        return Format::MapTables;
    }
    if lower == "data/combat.bin" {
        return Format::Combat;
    }
    if lower == "data/bio_card.dat" {
        return Format::BioCard;
    }
    if lower.starts_with("room/") && lower.ends_with(".rdt") {
        return match lower
            .strip_prefix("room/")
            .and_then(|rest| rest.strip_suffix(".rdt"))
        {
            Some(stem) if stem.len() == 4 && RoomId::parse(stem).is_ok() => Format::Rdt,
            // A non-four-digit name is not a room the loader can ever ask for,
            // so it stays opaque rather than failing third-party packs.
            _ => Format::Opaque,
        };
    }
    if lower.starts_with("scd/") && lower.ends_with(".scd") {
        return Format::Scd;
    }
    if lower.starts_with("text/") && lower.ends_with(".bin") {
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        return if known_text_table(name) {
            Format::Text
        } else {
            // A future or third-party `text/*.bin` table has no parser; count
            // it opaque so it cannot break the command.
            Format::Opaque
        };
    }
    if lower.starts_with("roomcut/") && lower.ends_with(".bmp") {
        return Format::Cut;
    }
    if lower.starts_with("roommask/") && lower.ends_with(".bmp") {
        return Format::Mask;
    }
    if lower.starts_with("movie/") && lower.ends_with(".avi") {
        return Format::Avi;
    }
    match lower.rsplit_once('.') {
        Some((_, "bmp")) => Format::Bmp,
        Some((_, "tim")) => Format::Tim,
        Some((_, "ivm")) => Format::Ivm,
        Some((_, "emd")) => Format::Emd,
        Some((_, "emw")) => Format::Emw,
        Some((_, "tmd")) => Format::Tmd,
        Some((_, "dor")) => Format::Dor,
        Some((_, "wav")) => Format::Wav,
        Some((_, "avi")) => Format::Avi,
        Some((_, "lua")) => Format::Lua,
        _ => Format::Opaque,
    }
}

/// Whether a `text/*.bin` file name has a table parser.
///
/// The classifier uses this list, so an unrecognized table name never reaches
/// the `Text` arm.
fn known_text_table(name: &str) -> bool {
    matches!(
        name,
        "messages.bin" | "idesc.bin" | "names.bin" | "unknown.bin" | "save.bin"
    )
}

/// Per-format counters in the report.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FormatStats {
    /// Entries classified as this format.
    pub entries: u64,
    /// Entries that parsed.
    pub ok: u64,
    /// Entries that failed to parse.
    pub failed: u64,
    /// Total bytes of the format's entries.
    pub bytes: u64,
}

/// One failed entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Entry path.
    pub path: String,
    /// Classified format.
    pub format: Format,
    /// The parser's error text.
    pub error: String,
}

/// The full verification result.
#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    /// Counters per format, present only when the pack carried one.
    pub formats: BTreeMap<Format, FormatStats>,
    /// Every failed path, in pack order.
    pub failures: Vec<Failure>,
    /// Total entries walked.
    pub entries: u64,
    /// Entries that parsed (opaque entries count as ok when not strict).
    pub ok: u64,
    /// Entries that failed.
    pub failed: u64,
    /// Opaque entries, whether or not they failed strict mode.
    pub opaque: u64,
    /// Total entry bytes.
    pub bytes: u64,
}

impl VerifyReport {
    /// Whether the pack is valid: no known-format entry failed, and no opaque
    /// entry failed either (which only happens under `--strict`).
    pub fn valid(&self) -> bool {
        self.failed == 0
    }

    /// Write the stable report: per-format table, failures, summary.
    pub fn write(&self, pack_path: &std::path::Path, out: &mut impl Write) -> std::io::Result<()> {
        writeln!(out, "pack: {}", pack_path.display())?;
        writeln!(out, "{} entries, {} bytes", self.entries, self.bytes)?;
        writeln!(
            out,
            "{:<10} {:>9} {:>9} {:>9} {:>14}",
            "format", "entries", "ok", "failed", "bytes"
        )?;
        for (format, stats) in &self.formats {
            writeln!(
                out,
                "{:<10} {:>9} {:>9} {:>9} {:>14}",
                format.name(),
                stats.entries,
                stats.ok,
                stats.failed,
                stats.bytes
            )?;
        }
        if !self.failures.is_empty() {
            writeln!(out, "failures:")?;
            for failure in &self.failures {
                writeln!(out, "  {}: {}", failure.path, failure.error)?;
            }
        }
        writeln!(
            out,
            "verify: {} entries, {} ok, {} failed, {} opaque",
            self.entries, self.ok, self.failed, self.opaque
        )
    }
}

/// Walk every entry of `pack` through its format's parser.
///
/// The walk is exhaustive and never stops at the first failure. `strict` turns
/// an unknown entry into a failure.
pub fn verify_pack(pack: &Pack, strict: bool) -> VerifyReport {
    let mut report = VerifyReport::default();
    for entry in pack.entries() {
        let path = entry.path();
        let format = classify(path);
        let stats = report.formats.entry(format).or_default();
        stats.entries += 1;
        stats.bytes += entry.size() as u64;
        report.entries += 1;
        report.bytes += entry.size() as u64;

        let opaque = format == Format::Opaque;
        if opaque {
            report.opaque += 1;
        }
        let result = match format {
            Format::Opaque if strict => {
                Err(anyhow::anyhow!("unknown entry format (--strict): {}", path))
            }
            Format::Opaque => Ok(()),
            _ => verify_entry(pack, path, format)
                .with_context(|| format!("{} ({})", path, format.name())),
        };
        match result {
            Ok(()) => {
                stats.ok += 1;
                report.ok += 1;
            }
            Err(err) => {
                stats.failed += 1;
                report.failed += 1;
                report.failures.push(Failure {
                    path: path.to_owned(),
                    format,
                    error: format!("{err:#}"),
                });
            }
        }
    }
    report
}

/// Parse one entry with the parser for `format`.
fn verify_entry(pack: &Pack, path: &str, format: Format) -> Result<()> {
    let data = pack.read(path)?;
    match format {
        Format::Manifest => {
            let text = std::str::from_utf8(data).context("manifest.toml is not valid UTF-8")?;
            crate::manifest::Manifest::parse(text)?;
        }
        Format::Rdt => {
            let id = room_id_from_path(path)?;
            let room = crate::rdt::parse(data, id)?;
            // The engine refuses a room with no camera cuts, so verification
            // must reject it too.
            if room.cuts.is_empty() {
                bail!("room has no camera cuts");
            }
            for cut in &room.cuts {
                let cut_path = id.cut_entry(cut.index);
                if !pack.contains(&cut_path) {
                    bail!("RDT references missing background {cut_path}");
                }
            }
            // The engine prefers a standalone `scd/{id}.scd` override and only
            // falls back to the RDT's embedded scripts, so a corrupt embedded
            // table is not a failure when the merged pack carries an override
            // the reader accepts.
            match pack.read(&id.scd_entry()) {
                Ok(bytes) => {
                    crate::scd::reader::parse(bytes).context("invalid SCD override")?;
                }
                Err(_) => {
                    crate::scd::reader::parse(data).context("invalid room SCD scripts")?;
                }
            }
        }
        Format::Scd => {
            crate::scd::reader::parse(data)?;
        }
        Format::Cut => {
            crate::bmp::decode(data)?;
        }
        Format::Mask => {
            crate::bmp::decode_mask(data)?;
        }
        Format::Bmp => {
            crate::bmp::decode(data)?;
        }
        Format::Tim => {
            let flags = data
                .get(4..8)
                .context("TIM is truncated before its flags")?;
            let flags = u32::from_le_bytes(flags.try_into().unwrap());
            match flags & 7 {
                0 => {
                    crate::tim::decode_4bpp(data)?;
                }
                1 => {
                    crate::tim::decode_8bpp(data)?;
                }
                2 => {
                    crate::tim::decode(data)?;
                }
                mode => bail!("unsupported TIM color mode {mode}"),
            }
        }
        Format::Ivm => {
            crate::ivm::parse(data)?;
        }
        Format::Emd => {
            crate::emd::parse(data)?;
        }
        Format::Emw => {
            crate::emd::parse_emw(data)?;
        }
        Format::Tmd => {
            crate::tmd::parse(data)?;
        }
        Format::Dor => {
            crate::door::parse(data)?;
        }
        Format::Wav => {
            crate::audio::parse_wav(data)?;
        }
        Format::Text => {
            let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
            match name.as_str() {
                "messages.bin" | "idesc.bin" => {
                    crate::text::Table::parse_message(data)?;
                }
                "names.bin" | "unknown.bin" => {
                    crate::text::Table::parse_name(data)?;
                }
                "save.bin" => {
                    crate::text::Table::parse_plain(data)?;
                }
                other => bail!("no text-table parser for {other}"),
            }
        }
        Format::MapTables => {
            crate::ui::map::MapTables::parse(data)?;
        }
        Format::Combat => {
            crate::combat::CombatTables::parse(data)?;
        }
        Format::BioCard => {
            if data.len() < crate::save::PREFIX_LEN {
                bail!(
                    "bio-card prefix is {} bytes; {} are required",
                    data.len(),
                    crate::save::PREFIX_LEN
                );
            }
        }
        Format::Avi => {
            crate::avi::Avi::parse(data.to_vec())?;
        }
        Format::Lua => {
            crate::lua::verify_source(data)?;
        }
        Format::Opaque => unreachable!("opaque entries are handled by the caller"),
    }
    Ok(())
}

/// The [`RoomId`] named by a `room/{id}.rdt` path.
fn room_id_from_path(path: &str) -> Result<RoomId> {
    let name = path
        .rsplit('/')
        .next()
        .with_context(|| format!("`{path}` is not a room path"))?;
    let stem = name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .with_context(|| format!("`{path}` is not a room/<id>.rdt path"))?;
    if stem.len() != 4 {
        bail!("`{path}` does not name a four-digit room id");
    }
    RoomId::parse(stem).with_context(|| format!("`{path}` has no valid room id"))
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;
    use crate::pack::PackWriter;
    use crate::state::Image;

    fn tiny_tim() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0x10u32.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&[0; 8]);
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data
    }

    fn tiny_bmp() -> Vec<u8> {
        crate::bmp::encode_to_vec(&Image {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        })
        .unwrap()
    }

    #[test]
    fn classify_maps_prefixes_and_extensions() {
        assert_eq!(classify("manifest.toml"), Format::Manifest);
        assert_eq!(classify("room/1000.rdt"), Format::Rdt);
        assert_eq!(classify("ROOM/1000.RDT"), Format::Rdt);
        assert_eq!(classify("roomcut/100_000.bmp"), Format::Cut);
        assert_eq!(classify("roommask/100_000.bmp"), Format::Mask);
        assert_eq!(classify("ui/blue.tim"), Format::Tim);
        assert_eq!(classify("item/i00v.ivm"), Format::Ivm);
        assert_eq!(classify("enemy/em20.emd"), Format::Emd);
        assert_eq!(classify("player/00.emw"), Format::Emw);
        assert_eq!(classify("player/ws202.tmd"), Format::Tmd);
        assert_eq!(classify("door/door00.dor"), Format::Dor);
        assert_eq!(classify("se/a_mcn03.wav"), Format::Wav);
        assert_eq!(classify("text/messages.bin"), Format::Text);
        assert_eq!(classify("text/save.bin"), Format::Text);
        assert_eq!(classify("map/tables.bin"), Format::MapTables);
        assert_eq!(classify("data/combat.bin"), Format::Combat);
        assert_eq!(classify("data/bio_card.dat"), Format::BioCard);
        assert_eq!(classify("movie/00.avi"), Format::Avi);
        assert_eq!(classify("enemy/em13.lua"), Format::Lua);
        assert_eq!(classify("lua/demo.lua"), Format::Lua);
        assert_eq!(classify("data/core00.esp"), Format::Opaque);
        // A future text table or a room the loader can never request stays
        // opaque instead of breaking the command.
        assert_eq!(classify("text/credits.bin"), Format::Opaque);
        assert_eq!(classify("room/bonus.rdt"), Format::Opaque);
        assert_eq!(classify("room/100.rdt"), Format::Opaque);
        assert_eq!(classify("room/zzzz.rdt"), Format::Opaque);
    }

    #[test]
    fn future_text_tables_and_odd_room_names_are_opaque() {
        let mut writer = PackWriter::new();
        writer.add("text/credits.bin", vec![1, 2, 3]).unwrap();
        writer.add("room/bonus.rdt", vec![1, 2, 3]).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let lenient = verify_pack(&pack, false);
        assert!(lenient.valid());
        assert_eq!(lenient.opaque, 2);
        assert_eq!(lenient.failed, 0);
        assert!(!lenient.formats.contains_key(&Format::Text));
        assert!(!lenient.formats.contains_key(&Format::Rdt));

        let strict = verify_pack(&pack, true);
        assert!(!strict.valid());
        assert_eq!(strict.failed, 2);
        assert_eq!(strict.opaque, 2);
    }

    #[test]
    fn a_room_without_camera_cuts_fails_like_the_engine() {
        // Parses as an RDT and its (empty) SCD parses, but the engine refuses
        // a room with no camera cuts on load.
        let rdt = vec![0u8; 0x94];
        let mut writer = PackWriter::new();
        writer.add("room/1000.rdt", rdt).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let report = verify_pack(&pack, false);

        assert!(!report.valid());
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0].error.contains("no camera cuts"),
            "{}",
            report.failures[0].error
        );
    }

    #[test]
    fn a_corrupt_script_pointer_fails_an_otherwise_loadable_rdt() {
        // The room header and its camera parse and the background is present,
        // but the event script pointer is out of bounds: the engine's SCD
        // reader rejects the room on load, so verification must too.
        let mut rdt = vec![0u8; 0x94];
        rdt[0x01] = 1;
        rdt.extend_from_slice(&[0u8; 44]);
        let events = 0x48 + 8 * 4;
        rdt[events..events + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut writer = PackWriter::new();
        writer.add("room/1000.rdt", rdt).unwrap();
        writer.add("roomcut/100_000.bmp", tiny_bmp()).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let report = verify_pack(&pack, false);

        assert!(!report.valid(), "a corrupt script table must fail");
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, "room/1000.rdt");
        assert!(
            report.failures[0].error.contains("event SCD table pointer"),
            "{}",
            report.failures[0].error
        );
    }

    #[test]
    fn a_held_weapon_tmd_entry_parses() {
        // An empty TMD: magic, flags, zero objects.
        let mut tmd = Vec::new();
        tmd.extend_from_slice(&0x41u32.to_le_bytes());
        tmd.extend_from_slice(&0u32.to_le_bytes());
        tmd.extend_from_slice(&0u32.to_le_bytes());
        let mut writer = PackWriter::new();
        writer.add("player/ws202.tmd", tmd).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let report = verify_pack(&pack, false);
        assert!(report.valid(), "failures: {:?}", report.failures);
        assert_eq!(report.formats[&Format::Tmd].ok, 1);

        // A truncated TMD fails.
        let mut writer = PackWriter::new();
        writer.add("player/ws202.tmd", vec![0x41, 0, 0]).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let report = verify_pack(&pack, false);
        assert!(!report.valid());
        assert_eq!(report.formats[&Format::Tmd].failed, 1);
    }

    #[test]
    fn a_good_pack_reports_every_format_and_zero_failures() {
        let mut writer = PackWriter::new();
        writer
            .add(
                manifest::ENTRY,
                manifest::Manifest::base("re1").render().into_bytes(),
            )
            .unwrap();
        writer.add("ui/blue.tim", tiny_tim()).unwrap();
        writer.add("roomcut/100_000.bmp", tiny_bmp()).unwrap();
        writer.add("roommask/100_000.bmp", tiny_bmp()).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let report = verify_pack(&pack, false);

        assert!(report.valid(), "failures: {:?}", report.failures);
        assert_eq!(report.entries, 4);
        assert_eq!(report.ok, 4);
        assert_eq!(report.failed, 0);
        assert_eq!(report.opaque, 0);
        assert_eq!(report.formats[&Format::Manifest].ok, 1);
        assert_eq!(report.formats[&Format::Tim].entries, 1);
        assert_eq!(report.formats[&Format::Cut].entries, 1);
        assert_eq!(report.formats[&Format::Mask].entries, 1);
    }

    #[test]
    fn a_bad_entry_fails_with_its_path_and_format() {
        let mut writer = PackWriter::new();
        writer.add("ui/blue.tim", b"not a tim".to_vec()).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let report = verify_pack(&pack, false);

        assert!(!report.valid());
        assert_eq!(report.failed, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, "ui/blue.tim");
        assert_eq!(report.failures[0].format, Format::Tim);
        assert_eq!(report.formats[&Format::Tim].failed, 1);
    }

    #[test]
    fn unknown_entries_are_opaque_unless_strict() {
        let mut writer = PackWriter::new();
        writer.add("data/core00.esp", vec![1, 2, 3]).unwrap();
        writer.add("lua/demo.lua", b"return 1".to_vec()).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let lenient = verify_pack(&pack, false);
        assert!(lenient.valid());
        assert_eq!(lenient.opaque, 1);
        assert_eq!(
            lenient.ok, 2,
            "the Lua source compiles and the opaque entry passes leniently"
        );

        let strict = verify_pack(&pack, true);
        assert!(!strict.valid());
        assert_eq!(strict.failed, 1);
        assert_eq!(strict.opaque, 1);
        assert!(
            strict
                .failures
                .iter()
                .all(|failure| failure.error.contains("--strict"))
        );
    }

    #[test]
    fn an_rdt_with_a_missing_background_fails_the_reference_check() {
        // Build the smallest RDT the parser accepts: one camera cut and no
        // other tables.
        let mut rdt = vec![0u8; 0x94];
        rdt[0x01] = 1;
        rdt.extend_from_slice(&[0u8; 44]);
        while !rdt.len().is_multiple_of(4) {
            rdt.push(0);
        }
        let mut writer = PackWriter::new();
        writer.add("room/1000.rdt", rdt).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let report = verify_pack(&pack, false);

        assert!(!report.valid(), "a missing roomcut must fail verification");
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0].error.contains("roomcut/100_000.bmp"),
            "{}",
            report.failures[0].error
        );
    }

    #[test]
    fn an_scd_override_rescues_a_room_with_corrupt_embedded_scripts() {
        // The RDT's embedded event pointer is corrupt, but the merged pack
        // carries a valid standalone override: the engine loads the override,
        // so verification must accept the room.
        let mut rdt = vec![0u8; 0x94];
        rdt[0x01] = 1;
        rdt.extend_from_slice(&[0u8; 44]);
        let events = 0x48 + 8 * 4;
        rdt[events..events + 4].copy_from_slice(&u32::MAX.to_le_bytes());

        let mut scd = vec![0u8; 0x94];
        let init = scd.len();
        scd.extend_from_slice(&4u16.to_le_bytes());
        scd.extend_from_slice(&[0x0E, 0x00]);
        scd.extend_from_slice(&0u16.to_le_bytes());
        scd[0x48 + 6 * 4..0x48 + 7 * 4].copy_from_slice(&(init as u32).to_le_bytes());

        let mut writer = PackWriter::new();
        writer.add("room/1000.rdt", rdt).unwrap();
        writer.add("roomcut/100_000.bmp", tiny_bmp()).unwrap();
        writer.add("scd/1000.scd", scd).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();

        let report = verify_pack(&pack, false);

        assert!(report.valid(), "failures: {:?}", report.failures);
    }

    #[test]
    fn the_report_writes_the_table_and_failure_lines() {
        let mut writer = PackWriter::new();
        writer.add("ui/blue.tim", tiny_tim()).unwrap();
        writer.add("data/core00.esp", vec![1]).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let report = verify_pack(&pack, false);

        let mut out = Vec::new();
        report
            .write(std::path::Path::new("pack.akpak"), &mut out)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("pack: pack.akpak"), "{text}");
        assert!(text.contains("format"), "{text}");
        let tim: Vec<&str> = text
            .lines()
            .find(|line| line.starts_with("tim "))
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(tim, ["tim", "1", "1", "0", "26"]);
        assert!(
            text.contains("verify: 2 entries, 2 ok, 0 failed, 1 opaque"),
            "{text}"
        );
    }
}
