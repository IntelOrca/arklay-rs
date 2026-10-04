//! `.akpak` game pack container.
//!
//! The container is little-endian. A 10-byte header holds the magic `APAK`, a
//! `u16` version, and a `u32` entry count. It is followed by one fixed 32-byte
//! table-of-contents entry per file: `path_offset: u64`, `data_offset: u64`,
//! `length: u64`, `kind: u8`, and seven reserved zero bytes, with offsets
//! measured from the start of the file. The payload area then holds the
//! NUL-terminated UTF-8 paths in entry order, followed by the entry data in
//! entry order with no padding. Writers sort entries by ASCII-lowercased path
//! so output is byte-deterministic; readers key entries by ASCII-lowercased
//! path while preserving the original path strings. Both the writer and the
//! reader reject unsafe paths (absolute, `..` components, backslashes, NUL
//! bytes or empty), so a pack can never name a file outside its pack root.

use std::collections::HashMap;
use std::path::{Component, Path};

use anyhow::{Context, Result, anyhow, bail};

/// Magic bytes at the start of every pack.
pub const MAGIC: [u8; 4] = *b"APAK";

/// Pack format version written and accepted by this module.
pub const VERSION: u16 = 1;

/// Size of the fixed pack header.
const HEADER_LEN: usize = 10;

/// Size of one table-of-contents entry.
const ENTRY_LEN: usize = 32;

/// Builds an `.akpak` pack from named byte blobs.
#[derive(Debug, Default)]
pub struct PackWriter {
    entries: Vec<(String, Vec<u8>)>,
}

impl PackWriter {
    /// Create a writer with no entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one entry.
    ///
    /// Paths must be non-empty and relative, must not contain backslashes,
    /// NUL bytes or `..` components, and must be unique ignoring ASCII case.
    pub fn add(&mut self, path: &str, data: Vec<u8>) -> Result<()> {
        validate_path(path)?;
        if self
            .entries
            .iter()
            .any(|(existing, _)| existing.eq_ignore_ascii_case(path))
        {
            bail!("duplicate pack entry: {path}");
        }
        self.entries.push((path.to_owned(), data));
        Ok(())
    }

    /// Whether no entries have been added.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialize the pack, sorting entries by ASCII-lowercased path.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let count = self.entries.len();
        let entry_count = u32::try_from(count).context("too many pack entries")?;
        let toc_len = count
            .checked_mul(ENTRY_LEN)
            .context("pack table of contents is too large")?;
        let paths_start = HEADER_LEN
            .checked_add(toc_len)
            .context("pack table of contents is too large")?;

        let mut order: Vec<(String, usize)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, (path, _))| (path.to_ascii_lowercase(), index))
            .collect();
        order.sort_by(|a, b| a.0.cmp(&b.0));

        let mut data_start = paths_start;
        let mut total = paths_start;
        for (_, index) in &order {
            let (path, data) = &self.entries[*index];
            data_start = data_start
                .checked_add(path.len() + 1)
                .context("pack paths exceed addressable size")?;
            total = total
                .checked_add(path.len() + 1)
                .and_then(|size| size.checked_add(data.len()))
                .context("pack data exceeds addressable size")?;
        }

        let mut toc = Vec::with_capacity(toc_len);
        let mut path_offset = paths_start;
        let mut data_offset = data_start;
        for (_, index) in &order {
            let (path, data) = &self.entries[*index];
            toc.extend_from_slice(&to_u64(path_offset)?.to_le_bytes());
            toc.extend_from_slice(&to_u64(data_offset)?.to_le_bytes());
            toc.extend_from_slice(&to_u64(data.len())?.to_le_bytes());
            toc.push(0);
            toc.extend_from_slice(&[0; 7]);
            path_offset += path.len() + 1;
            data_offset += data.len();
        }

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&entry_count.to_le_bytes());
        out.extend_from_slice(&toc);
        for (_, index) in &order {
            out.extend_from_slice(self.entries[*index].0.as_bytes());
            out.push(0);
        }
        for (_, index) in &order {
            out.extend_from_slice(&self.entries[*index].1);
        }
        debug_assert_eq!(out.len(), total);
        Ok(out)
    }

    /// Serialize the pack and write it to `path`.
    pub fn write(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_bytes()?)
            .with_context(|| format!("failed to write pack {}", path.display()))
    }
}

/// One entry of a pack, as reported by [`Pack::entries`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackEntry<'a> {
    path: &'a str,
    size: usize,
}

impl<'a> PackEntry<'a> {
    /// The entry path with its original case and `/` separators.
    pub fn path(self) -> &'a str {
        self.path
    }

    /// The entry size in bytes.
    pub fn size(self) -> usize {
        self.size
    }
}

/// One table-of-contents entry resolved against the pack data.
#[derive(Debug)]
struct Entry {
    path: String,
    offset: usize,
    length: usize,
}

/// A parsed `.akpak` pack.
#[derive(Debug)]
pub struct Pack {
    data: Vec<u8>,
    entries: Vec<Entry>,
    lookup: HashMap<String, usize>,
}

impl Pack {
    /// Read and parse a pack from disk.
    pub fn open(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)
            .with_context(|| format!("failed to read pack {}", path.display()))?;
        Self::from_bytes(data)
    }

    /// Parse a pack from an in-memory image.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        if data.len() < HEADER_LEN {
            bail!("pack is smaller than the header");
        }
        if data[..4] != MAGIC {
            bail!("pack magic is not APAK");
        }
        let version = u16::from_le_bytes([data[4], data[5]]);
        if version != VERSION {
            bail!("unsupported pack version {version}");
        }
        let entry_count = u32::from_le_bytes([data[6], data[7], data[8], data[9]]) as usize;
        let toc_end = entry_count
            .checked_mul(ENTRY_LEN)
            .and_then(|size| size.checked_add(HEADER_LEN))
            .context("pack entry count overflows the address space")?;
        if toc_end > data.len() {
            bail!("pack table of contents is truncated");
        }

        let mut entries = Vec::with_capacity(entry_count);
        let mut lookup = HashMap::with_capacity(entry_count);
        for index in 0..entry_count {
            let toc = &data[HEADER_LEN + index * ENTRY_LEN..HEADER_LEN + (index + 1) * ENTRY_LEN];
            let path_offset = read_u64(toc, 0)?;
            let data_offset = read_u64(toc, 8)?;
            let length = read_u64(toc, 16)?;
            let kind = toc[24];
            if kind != 0 {
                bail!("entry {index} has unsupported kind {kind}");
            }
            let path_offset =
                usize::try_from(path_offset).context("pack path offset does not fit in memory")?;
            let data_offset =
                usize::try_from(data_offset).context("pack data offset does not fit in memory")?;
            let length =
                usize::try_from(length).context("pack entry length does not fit in memory")?;
            if path_offset >= data.len() {
                bail!("entry {index} path offset {path_offset} is out of bounds");
            }
            let nul = data[path_offset..]
                .iter()
                .position(|&byte| byte == 0)
                .with_context(|| format!("entry {index} path is not NUL-terminated"))?;
            let path = std::str::from_utf8(&data[path_offset..path_offset + nul])
                .with_context(|| format!("entry {index} path is not valid UTF-8"))?;
            if path.is_empty() {
                bail!("entry {index} has an empty path");
            }
            validate_path(path)
                .map_err(|err| anyhow!("entry {index} has an unsafe path: {err}"))?;
            let key = path.to_ascii_lowercase();
            if lookup.contains_key(&key) {
                bail!("duplicate pack entry: {path}");
            }
            let end = data_offset
                .checked_add(length)
                .with_context(|| format!("entry {index} data range overflows"))?;
            if end > data.len() {
                bail!("entry {index} data range {data_offset}..{end} is out of bounds");
            }
            lookup.insert(key, entries.len());
            entries.push(Entry {
                path: path.to_owned(),
                offset: data_offset,
                length,
            });
        }

        Ok(Self {
            data,
            entries,
            lookup,
        })
    }

    /// Whether an entry with this path exists, ignoring ASCII case.
    pub fn contains(&self, path: &str) -> bool {
        self.lookup.contains_key(&path.to_ascii_lowercase())
    }

    /// Read an entry's data, looking the path up ignoring ASCII case.
    pub fn read(&self, path: &str) -> Result<&[u8]> {
        let index = self
            .lookup
            .get(&path.to_ascii_lowercase())
            .with_context(|| format!("no pack entry named {path}"))?;
        let entry = &self.entries[*index];
        Ok(&self.data[entry.offset..entry.offset + entry.length])
    }

    /// Original-case entry paths in table-of-contents order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.path.as_str())
    }

    /// Every entry in table-of-contents order, with its path and size.
    pub fn entries(&self) -> impl Iterator<Item = PackEntry<'_>> {
        self.entries.iter().map(|entry| PackEntry {
            path: entry.path.as_str(),
            size: entry.length,
        })
    }

    /// Number of entries in the pack.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the pack has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Validate an entry path for safe use on disk.
///
/// Paths must be non-empty and relative, must not contain backslashes, NUL
/// bytes or `..` components. Both the writer and the reader enforce these
/// rules, so an untrusted pack can never escape an extraction root.
fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("pack entry path is empty");
    }
    if path.contains('\0') {
        bail!("pack entry path contains a NUL byte");
    }
    if path.contains('\\') {
        bail!("pack entry path contains a backslash: {path}");
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        bail!("pack entry path must be relative: {path}");
    }
    for component in candidate.components() {
        match component {
            Component::ParentDir => bail!("pack entry path contains a `..` component: {path}"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("pack entry path must be relative: {path}");
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Convert an in-memory offset to its on-disk `u64` representation.
fn to_u64(value: usize) -> Result<u64> {
    u64::try_from(value).context("pack offset does not fit in a u64")
}

/// Read a little-endian `u64` from a fixed-offset slice.
fn read_u64(bytes: &[u8], at: usize) -> Result<u64> {
    let raw: [u8; 8] = bytes[at..at + 8]
        .try_into()
        .context("table-of-contents entry is truncated")?;
    Ok(u64::from_le_bytes(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_pack() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    fn single_entry_pack(path: &str, data: &[u8]) -> Vec<u8> {
        let header_len = HEADER_LEN + ENTRY_LEN;
        let path_offset = header_len as u64;
        let data_offset = (header_len + path.len() + 1) as u64;
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&path_offset.to_le_bytes());
        out.extend_from_slice(&data_offset.to_le_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.push(0);
        out.extend_from_slice(&[0; 7]);
        out.extend_from_slice(path.as_bytes());
        out.push(0);
        out.extend_from_slice(data);
        out
    }

    fn write_u64(bytes: &mut [u8], at: usize, value: u64) {
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn multi_entry_pack(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let toc_len = entries.len() * ENTRY_LEN;
        let paths_start = HEADER_LEN + toc_len;
        let data_start = paths_start
            + entries
                .iter()
                .map(|(path, _)| path.len() + 1)
                .sum::<usize>();
        let mut toc = Vec::new();
        let mut path_offset = paths_start;
        let mut data_offset = data_start;
        for (path, data) in entries {
            toc.extend_from_slice(&(path_offset as u64).to_le_bytes());
            toc.extend_from_slice(&(data_offset as u64).to_le_bytes());
            toc.extend_from_slice(&(data.len() as u64).to_le_bytes());
            toc.push(0);
            toc.extend_from_slice(&[0; 7]);
            path_offset += path.len() + 1;
            data_offset += data.len();
        }

        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&toc);
        for (path, _) in entries {
            out.extend_from_slice(path.as_bytes());
            out.push(0);
        }
        for (_, data) in entries {
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn roundtrip_several_entries() {
        let mut writer = PackWriter::new();
        writer.add("room/1000.rdt", vec![1, 2, 3]).unwrap();
        writer.add("roomcut/100_000.bmp", vec![9; 64]).unwrap();
        writer.add("MixEd.TxT", b"hello".to_vec()).unwrap();
        assert!(!writer.is_empty());

        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.len(), 3);
        assert!(!pack.is_empty());
        assert_eq!(pack.read("room/1000.rdt").unwrap(), &[1, 2, 3]);
        assert_eq!(pack.read("roomcut/100_000.bmp").unwrap(), &[9; 64]);
        assert_eq!(pack.read("MixEd.TxT").unwrap(), b"hello");
        let paths: Vec<&str> = pack.paths().collect();
        assert_eq!(paths, ["MixEd.TxT", "room/1000.rdt", "roomcut/100_000.bmp"]);
    }

    #[test]
    fn writer_header_layout() {
        let mut writer = PackWriter::new();
        writer.add("a.txt", vec![1]).unwrap();
        writer.add("b.txt", vec![2, 3]).unwrap();
        let bytes = writer.to_bytes().unwrap();
        assert_eq!(&bytes[..4], b"APAK");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), VERSION);
        assert_eq!(u32::from_le_bytes(bytes[6..10].try_into().unwrap()), 2);
    }

    #[test]
    fn writer_golden_bytes() {
        let mut writer = PackWriter::new();
        writer.add("b.txt", vec![0xDE, 0xAD]).unwrap();
        writer.add("a.bin", vec![0x01, 0x02, 0x03]).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(b"APAK");
        expected.extend_from_slice(&VERSION.to_le_bytes());
        expected.extend_from_slice(&2u32.to_le_bytes());
        // Entry 0 (a.bin): path at 74, data at 86, length 3.
        expected.extend_from_slice(&74u64.to_le_bytes());
        expected.extend_from_slice(&86u64.to_le_bytes());
        expected.extend_from_slice(&3u64.to_le_bytes());
        expected.push(0);
        expected.extend_from_slice(&[0; 7]);
        // Entry 1 (b.txt): path at 80, data at 89, length 2.
        expected.extend_from_slice(&80u64.to_le_bytes());
        expected.extend_from_slice(&89u64.to_le_bytes());
        expected.extend_from_slice(&2u64.to_le_bytes());
        expected.push(0);
        expected.extend_from_slice(&[0; 7]);
        expected.extend_from_slice(b"a.bin\0b.txt\0");
        expected.extend_from_slice(&[0x01, 0x02, 0x03, 0xDE, 0xAD]);

        assert_eq!(writer.to_bytes().unwrap(), expected);
    }

    #[test]
    fn sorted_toc_is_deterministic() {
        let mut first = PackWriter::new();
        first.add("z.txt", vec![1]).unwrap();
        first.add("Alpha.txt", vec![2]).unwrap();
        first.add("beta.txt", vec![3]).unwrap();

        let mut second = PackWriter::new();
        second.add("beta.txt", vec![3]).unwrap();
        second.add("z.txt", vec![1]).unwrap();
        second.add("Alpha.txt", vec![2]).unwrap();

        assert_eq!(first.to_bytes().unwrap(), second.to_bytes().unwrap());
        let pack = Pack::from_bytes(first.to_bytes().unwrap()).unwrap();
        let paths: Vec<&str> = pack.paths().collect();
        assert_eq!(paths, ["Alpha.txt", "beta.txt", "z.txt"]);
    }

    #[test]
    fn lookups_are_case_insensitive() {
        let mut writer = PackWriter::new();
        writer.add("Data/Room.RDT", b"room".to_vec()).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert!(pack.contains("data/room.rdt"));
        assert!(pack.contains("DATA/ROOM.RDT"));
        assert!(!pack.contains("data/other.rdt"));
        assert_eq!(pack.read("dAtA/rOoM.rdT").unwrap(), b"room");
        assert!(pack.read("missing").is_err());
    }

    #[test]
    fn duplicate_paths_rejected() {
        let mut writer = PackWriter::new();
        writer.add("room/1000.rdt", vec![]).unwrap();
        assert!(writer.add("ROOM/1000.RDT", vec![]).is_err());
        assert!(writer.add("room/1000.rdt", vec![]).is_err());
    }

    #[test]
    fn reader_rejects_empty_path() {
        let bytes = multi_entry_pack(&[("", b"x")]);
        let err = Pack::from_bytes(bytes).unwrap_err().to_string();
        assert!(err.contains("empty path"), "{err}");
    }

    #[test]
    fn reader_rejects_case_insensitive_duplicates() {
        let bytes = multi_entry_pack(&[("room/a.bin", b"a"), ("ROOM/A.BIN", b"b")]);
        let err = Pack::from_bytes(bytes).unwrap_err().to_string();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn invalid_paths_rejected() {
        let mut writer = PackWriter::new();
        assert!(writer.add("", vec![]).is_err());
        assert!(writer.add("/absolute", vec![]).is_err());
        assert!(writer.add("back\\slash", vec![]).is_err());
        assert!(writer.add("nul\0byte", vec![]).is_err());
        assert!(writer.add("..", vec![]).is_err());
        assert!(writer.add("../escape", vec![]).is_err());
        assert!(writer.add("room/../../escape", vec![]).is_err());
        assert!(writer.is_empty());
    }

    #[test]
    fn reader_rejects_parent_directory_paths() {
        for path in ["..", "../up.bin", "room/../../escape.bin"] {
            let err = Pack::from_bytes(multi_entry_pack(&[(path, b"x")]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("`..` component"), "{path}: {err}");
        }
    }

    #[test]
    fn reader_rejects_absolute_and_backslash_paths() {
        for path in ["/etc/passwd", "back\\slash.bin"] {
            let err = Pack::from_bytes(multi_entry_pack(&[(path, b"x")]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("unsafe path"), "{path}: {err}");
        }
    }

    #[test]
    fn entries_expose_paths_and_sizes() {
        let mut writer = PackWriter::new();
        writer.add("room/1000.rdt", vec![1, 2, 3]).unwrap();
        writer.add("bgm/013.wav", vec![0; 100]).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let entries: Vec<(&str, usize)> = pack
            .entries()
            .map(|entry| (entry.path(), entry.size()))
            .collect();
        assert_eq!(entries, [("bgm/013.wav", 100), ("room/1000.rdt", 3)]);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = empty_pack();
        bytes[0] = 0;
        assert!(Pack::from_bytes(bytes).is_err());
    }

    #[test]
    fn rejects_wrong_version() {
        let mut bytes = empty_pack();
        bytes[4] = 2;
        assert!(Pack::from_bytes(bytes).is_err());
    }

    #[test]
    fn rejects_unknown_kind() {
        let mut bytes = single_entry_pack("a.txt", b"x");
        bytes[HEADER_LEN + 24] = 1;
        assert!(Pack::from_bytes(bytes).is_err());
    }

    #[test]
    fn rejects_truncated_toc() {
        let bytes = single_entry_pack("a.txt", b"x");
        assert!(Pack::from_bytes(bytes[..20].to_vec()).is_err());

        let mut counted = empty_pack();
        counted[6..10].copy_from_slice(&3u32.to_le_bytes());
        assert!(Pack::from_bytes(counted).is_err());
    }

    #[test]
    fn rejects_out_of_bounds_data() {
        let mut bytes = single_entry_pack("a.txt", b"xyz");
        let len = bytes.len() as u64;
        write_u64(&mut bytes, HEADER_LEN + 8, len - 2);
        assert!(Pack::from_bytes(bytes).is_err());

        let mut bytes = single_entry_pack("a.txt", b"xyz");
        let len = bytes.len() as u64;
        write_u64(&mut bytes, HEADER_LEN + 16, len + 1);
        assert!(Pack::from_bytes(bytes).is_err());

        let mut bytes = single_entry_pack("a.txt", b"xyz");
        write_u64(&mut bytes, HEADER_LEN + 8, u64::MAX);
        write_u64(&mut bytes, HEADER_LEN + 16, u64::MAX);
        assert!(Pack::from_bytes(bytes).is_err());
    }

    #[test]
    fn rejects_path_without_nul_terminator() {
        let mut bytes = single_entry_pack("a.txt", b"xyz");
        let data_offset = (HEADER_LEN + ENTRY_LEN + "a.txt".len() + 1) as u64;
        write_u64(&mut bytes, HEADER_LEN, data_offset);
        assert!(Pack::from_bytes(bytes).is_err());
    }

    #[test]
    fn rejects_invalid_utf8_path() {
        let mut bytes = single_entry_pack("a.txt", b"x");
        bytes[HEADER_LEN + ENTRY_LEN] = 0xFF;
        assert!(Pack::from_bytes(bytes).is_err());
    }

    #[test]
    fn rejects_huge_u64_offsets() {
        let mut bytes = single_entry_pack("a.txt", b"xyz");
        write_u64(&mut bytes, HEADER_LEN, u64::MAX);
        assert!(Pack::from_bytes(bytes).is_err());

        let mut bytes = single_entry_pack("a.txt", b"xyz");
        write_u64(&mut bytes, HEADER_LEN + 8, u64::MAX - 1);
        write_u64(&mut bytes, HEADER_LEN + 16, 8);
        assert!(Pack::from_bytes(bytes).is_err());
    }

    #[test]
    fn write_then_open() {
        let path = std::env::temp_dir().join(format!(
            "arklay-pack-{}-write-then-open.akpak",
            std::process::id()
        ));
        let mut writer = PackWriter::new();
        writer.add("a.bin", vec![7, 8, 9]).unwrap();
        writer.write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();
        assert_eq!(pack.read("A.BIN").unwrap(), &[7, 8, 9]);
        std::fs::remove_file(&path).unwrap();
    }
}
