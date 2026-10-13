//! `.akpak` game pack container.
//!
//! The container is little-endian. A 10-byte header holds the magic `APAK`, a
//! `u16` version, and a `u32` entry count. It is followed by one fixed 32-byte
//! table-of-contents entry per file: `path_offset: u64`, `data_offset: u64`,
//! `length: u64`, `compression: u8`, and seven reserved zero bytes, with
//! offsets measured from the start of the file. `length` is the stored length,
//! so for a compressed entry it is the size of the zstd frame, not of the
//! decompressed bytes (the frame embeds that size). The compression byte is 0
//! for a raw entry and 1 for a zstd frame; it was reserved zero in the
//! original v1 layout, so a raw pack is byte-for-byte unchanged and only a
//! pack that actually uses mode 1 needs a reader that knows it.
//! [`PackWriter::compress_entries`] zstd-compresses the entries a caller does
//! not exempt, on a worker pool; an entry never grows, so a frame smaller than
//! the raw bytes is required. The payload area then holds the NUL-terminated
//! UTF-8 paths in entry order, followed by the entry data in entry order with
//! no padding. Writers sort entries by ASCII-lowercased path so output is
//! byte-deterministic for a given libzstd build; readers key entries by
//! ASCII-lowercased path while preserving the original path strings. Both the
//! writer and the reader reject unsafe paths (absolute, `..` components,
//! backslashes, NUL bytes or empty), so a pack can never name a file outside
//! its pack root.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};

use crate::budget;
use crate::manifest;

/// Magic bytes at the start of every pack.
pub const MAGIC: [u8; 4] = *b"APAK";

/// Pack format version written and accepted by this module.
pub const VERSION: u16 = 1;

/// The zstd compression level `compress_entries` uses.
///
/// Fixed from an experiment over the RE1 pack's 518 MB of compressible
/// entries on the 4-core reference machine. Full-conversion wall clock: 2.0 s
/// raw (770.2 MB), 2.5 s at level 3 (487.7 MB), 3.8 s at level 6 (479.9 MB)
/// and 5.0 s at level 9 (478.5 MB). Higher levels buy almost nothing until
/// level 19, whose larger window saves 16 MB more for over a minute extra, so
/// six sits at the knee of the size/time curve. The container exposes no
/// strength knob.
pub const ZSTD_LEVEL: i32 = 6;

/// Size of the fixed pack header.
const HEADER_LEN: usize = 10;

/// Size of one table-of-contents entry.
const ENTRY_LEN: usize = 32;

/// How one entry's bytes are stored in the pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    /// The entry is stored as-is (table-of-contents mode 0).
    None,
    /// The entry is a zstd frame (table-of-contents mode 1).
    Zstd,
}

impl Compression {
    /// The mode byte written to the table of contents.
    fn mode(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Zstd => 1,
        }
    }

    /// Parse a table-of-contents mode byte.
    fn from_mode(mode: u8) -> Result<Self> {
        match mode {
            0 => Ok(Self::None),
            1 => Ok(Self::Zstd),
            other => bail!("unsupported compression mode {other}"),
        }
    }
}

/// One entry's compression result: its index and, when the frame paid for
/// itself, the compressed bytes. An `Err` aborts the compression pass.
type CompressResult = (usize, Result<Option<Vec<u8>>>);

/// Builds an `.akpak` pack from named byte blobs.
#[derive(Debug, Default)]
pub struct PackWriter {
    entries: Vec<(String, Vec<u8>, Compression)>,
    /// ASCII-lowercased entry paths, for O(1) duplicate detection.
    seen: HashSet<String>,
}

/// Entry order and offsets of a serialized pack.
struct SerializeLayout {
    /// Number of entries, as written in the header.
    entry_count: u32,
    /// Lowercased path and entry index per output position.
    order: Vec<(String, usize)>,
    /// The fixed-size table of contents.
    toc: Vec<u8>,
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
        if !self.seen.insert(path.to_ascii_lowercase()) {
            bail!("duplicate pack entry: {path}");
        }
        self.entries
            .push((path.to_owned(), data, Compression::None));
        Ok(())
    }

    /// Compress every raw entry `keep_raw` does not exempt with zstd at
    /// [`ZSTD_LEVEL`], on up to `jobs` scoped workers (clamped to 1..=entries;
    /// entries already stored zstd are skipped).
    ///
    /// `keep_raw` names the paths whose data is already compressed (films,
    /// PNG and WebP images, `ogg` audio) and would only waste conversion time;
    /// it runs on the worker threads and is consulted only for raw entries. An
    /// entry is stored compressed only when the frame is smaller than the raw
    /// bytes and the plain data fits [`budget::MAX_ZSTD_OUTPUT`], the reader's
    /// decompression cap, so an entry can never grow and every written pack
    /// reads back. Returns how many entries ended up compressed.
    ///
    /// Workers run to completion before any entry is replaced, and the results
    /// are applied in entry order, so a failed pass leaves every entry raw and
    /// a successful one is byte-deterministic for a given libzstd build
    /// whatever the worker count or scheduling. The pass holds the raw entries
    /// and every compressed frame at once, so its peak memory is the plain
    /// pack size plus the compressed pack size.
    pub fn compress_entries(
        &mut self,
        jobs: usize,
        keep_raw: impl Fn(&str) -> bool + Sync,
    ) -> Result<usize> {
        self.compress_entries_capped(jobs, keep_raw, budget::MAX_ZSTD_OUTPUT)
    }

    /// [`PackWriter::compress_entries`] with an explicit plain-size cap, for
    /// the unit tests.
    fn compress_entries_capped(
        &mut self,
        jobs: usize,
        keep_raw: impl Fn(&str) -> bool + Sync,
        cap: usize,
    ) -> Result<usize> {
        if self.entries.is_empty() {
            return Ok(0);
        }
        let jobs = jobs.clamp(1, self.entries.len());
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<CompressResult>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..jobs {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some((path, data, compression)) = self.entries.get(index) else {
                            return;
                        };
                        if *compression != Compression::None || keep_raw(path) {
                            continue;
                        }
                        // The reader refuses to decompress beyond its cap, so
                        // an over-cap entry stays raw and keeps reading back.
                        if data.len() > cap {
                            continue;
                        }
                        let compressed = zstd::bulk::compress(data, ZSTD_LEVEL)
                            .with_context(|| format!("failed to zstd-compress {path}"));
                        // Keep an entry raw when the frame does not pay for
                        // itself, whatever the policy says.
                        let compressed = compressed.map(|frame| {
                            if frame.len() < data.len() {
                                Some(frame)
                            } else {
                                None
                            }
                        });
                        results
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((index, compressed));
                    }
                });
            }
        });

        let mut count = 0;
        let mut results = results
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        results.sort_by_key(|(index, _)| *index);
        // Move the frames out first: a failed pass leaves every entry exactly
        // as it was, because nothing is replaced until every result succeeded.
        let mut frames = Vec::with_capacity(results.len());
        for (index, result) in results {
            frames.push((index, result?));
        }
        for (index, frame) in frames {
            if let Some(frame) = frame {
                self.entries[index].1 = frame;
                self.entries[index].2 = Compression::Zstd;
                count += 1;
            }
        }
        Ok(count)
    }

    /// The number of entries added so far.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entries have been added.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total size in bytes of the serialized pack.
    pub fn pack_size(&self) -> Result<usize> {
        let count = self.entries.len();
        u32::try_from(count).context("too many pack entries")?;
        let toc_len = count
            .checked_mul(ENTRY_LEN)
            .context("pack table of contents is too large")?;
        let mut total = HEADER_LEN
            .checked_add(toc_len)
            .context("pack table of contents is too large")?;
        for (path, data, _) in &self.entries {
            total = total
                .checked_add(path.len() + 1)
                .and_then(|size| size.checked_add(data.len()))
                .context("pack data exceeds addressable size")?;
        }
        Ok(total)
    }

    /// Compute the sorted entry order and table of contents.
    fn serialize_layout(&self) -> Result<SerializeLayout> {
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
            .map(|(index, (path, _, _))| (path.to_ascii_lowercase(), index))
            .collect();
        order.sort_by(|a, b| a.0.cmp(&b.0));

        let mut data_start = paths_start;
        for (_, index) in &order {
            data_start = data_start
                .checked_add(self.entries[*index].0.len() + 1)
                .context("pack paths exceed addressable size")?;
        }

        let mut toc = Vec::with_capacity(toc_len);
        let mut path_offset = paths_start;
        let mut data_offset = data_start;
        for (_, index) in &order {
            let (path, data, compression) = &self.entries[*index];
            toc.extend_from_slice(&to_u64(path_offset)?.to_le_bytes());
            toc.extend_from_slice(&to_u64(data_offset)?.to_le_bytes());
            toc.extend_from_slice(&to_u64(data.len())?.to_le_bytes());
            toc.push(compression.mode());
            toc.extend_from_slice(&[0; 7]);
            path_offset += path.len() + 1;
            data_offset += data.len();
        }

        Ok(SerializeLayout {
            entry_count,
            order,
            toc,
        })
    }

    /// Write a prepared layout to `out`.
    fn write_layout(&self, layout: &SerializeLayout, out: &mut impl Write) -> Result<()> {
        out.write_all(&MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&layout.entry_count.to_le_bytes())?;
        out.write_all(&layout.toc)?;
        for (_, index) in &layout.order {
            out.write_all(self.entries[*index].0.as_bytes())?;
            out.write_all(&[0])?;
        }
        for (_, index) in &layout.order {
            out.write_all(&self.entries[*index].1)?;
        }
        Ok(())
    }

    /// Write the pack to `out`, sorting entries by ASCII-lowercased path.
    ///
    /// Unlike [`PackWriter::to_bytes`] this never materializes a second copy
    /// of the entry data.
    pub fn stream_to(&self, out: &mut impl Write) -> Result<()> {
        let layout = self.serialize_layout()?;
        self.write_layout(&layout, out)
    }

    /// Serialize the pack, sorting entries by ASCII-lowercased path.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let size = self.pack_size()?;
        let mut out = Vec::with_capacity(size);
        self.stream_to(&mut out)?;
        debug_assert_eq!(out.len(), size);
        Ok(out)
    }

    /// Serialize the pack and write it to `path`.
    ///
    /// The layout is prepared before the file is created and the bytes land in
    /// a sibling temporary file that is renamed into place once complete, so a
    /// serialization or I/O error cannot leave a truncated or partial pack
    /// behind.
    pub fn write(&self, path: &Path) -> Result<()> {
        let layout = self.serialize_layout()?;
        let temp = crate::atomic::temp_path(path)?;
        let file = std::fs::File::create(&temp)
            .with_context(|| format!("failed to write pack {}", path.display()))?;
        let mut out = BufWriter::new(file);
        let result = self
            .write_layout(&layout, &mut out)
            .with_context(|| format!("failed to write pack {}", path.display()))
            .and_then(|()| {
                out.flush()
                    .with_context(|| format!("failed to write pack {}", path.display()))
            });
        drop(out);
        if let Err(err) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
        std::fs::rename(&temp, path).with_context(|| {
            let _ = std::fs::remove_file(&temp);
            format!("failed to move the written pack into {}", path.display())
        })
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

    /// The entry size in bytes as stored in the pack: the zstd frame size for
    /// a compressed entry, not the decompressed size.
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
    compression: Compression,
}

/// A parsed `.akpak` pack, optionally layered over by mod packs.
///
/// A plain [`Pack::open`] holds one pack image; [`Pack::open_layered`] adds an
/// owned list of mod layers. Every lookup then resolves the overlays last to
/// first and the base last, so a mod can shadow any entry without any loader
/// changing. A pack with no overlays follows exactly the single-pack path.
#[derive(Debug)]
pub struct Pack {
    data: Vec<u8>,
    entries: Vec<Entry>,
    lookup: HashMap<String, usize>,
    /// Parsed `manifest.toml`, when the pack carries one.
    manifest: Option<manifest::Manifest>,
    /// Filesystem origin, when opened from disk.
    source: Option<PathBuf>,
    /// Mod layers in applied order; the last one wins.
    overlays: Vec<Pack>,
    /// Non-fatal problems found while layering.
    warnings: Vec<String>,
}

impl Pack {
    /// Read and parse a pack from disk.
    ///
    /// The file size is checked against [`budget::MAX_PACK_BYTES`] before the
    /// image is read, so an oversized pack is rejected without allocating it.
    pub fn open(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("failed to read pack {}", path.display()))?;
        budget::check_len_u64(metadata.len(), budget::MAX_PACK_BYTES, "pack file size")
            .with_context(|| format!("failed to read pack {}", path.display()))?;
        let data = std::fs::read(path)
            .with_context(|| format!("failed to read pack {}", path.display()))?;
        let mut pack = Self::from_bytes(data)?;
        pack.source = Some(path.to_path_buf());
        Ok(pack)
    }

    /// Open `base` and layer the `mods` packs over it.
    ///
    /// Each mod must carry a valid mod manifest whose `base` matches the
    /// base's declared id (when the base declares one). Mods are applied in
    /// `(load_order, id)` order, so later layers win a shared entry. Duplicate
    /// mod ids and duplicate layer paths are rejected. A base without a
    /// manifest is tolerated as `id = <pack stem>`, `kind = base`, RE1
    /// dialects, with a warning.
    pub fn open_layered(base: &Path, mods: &[PathBuf]) -> Result<Self> {
        let mut pack = Self::open(base)?;
        let mut warnings = Vec::new();
        let base_declared = pack.manifest.is_some();
        if !base_declared {
            let id = pack_stem(base);
            warnings.push(format!(
                "base pack {} has no {}; treating it as id \"{id}\", kind base",
                base.display(),
                manifest::ENTRY
            ));
            pack.manifest = Some(manifest::Manifest::base(id));
        }
        let base_id = pack
            .manifest
            .as_ref()
            .expect("a base manifest always exists here")
            .id
            .clone();

        let mut layers = Vec::with_capacity(mods.len());
        let mut ids: HashMap<String, PathBuf> = HashMap::new();
        let mut paths: HashSet<PathBuf> = HashSet::new();
        for path in mods {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            if !paths.insert(canonical) {
                bail!("duplicate mod layer path {}", path.display());
            }
            let layer = Self::open(path)
                .with_context(|| format!("failed to open mod {}", path.display()))?;
            let Some(manifest) = layer.manifest.clone() else {
                bail!(
                    "mod {} has no {} entry and cannot be layered",
                    path.display(),
                    manifest::ENTRY
                );
            };
            if manifest.kind != manifest::PackKind::Mod {
                bail!(
                    "mod {} declares kind {}, expected mod",
                    path.display(),
                    manifest.kind
                );
            }
            // The declared base must match the base pack's id whether that id
            // was declared or derived from the pack's stem, exactly like the
            // mod builder's check.
            if let Some(declared) = &manifest.base
                && declared != &base_id
            {
                bail!(
                    "mod {} declares base \"{declared}\" but the base pack declares \"{base_id}\"",
                    path.display()
                );
            }
            if let Some(previous) = ids.insert(manifest.id.clone(), path.clone()) {
                bail!(
                    "duplicate mod id \"{}\": {} and {}",
                    manifest.id,
                    previous.display(),
                    path.display()
                );
            }
            layers.push((manifest, layer));
        }
        layers.sort_by(|a, b| {
            a.0.load_order
                .cmp(&b.0.load_order)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        pack.overlays = layers.into_iter().map(|(_, layer)| layer).collect();
        pack.warnings = warnings;
        Ok(pack)
    }

    /// Parse a pack from an in-memory image.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        budget::check_len_u64(data.len() as u64, budget::MAX_PACK_BYTES, "pack size")?;
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
        budget::check_len(entry_count, budget::MAX_PACK_ENTRIES, "pack entry count")?;
        let toc_end = entry_count
            .checked_mul(ENTRY_LEN)
            .and_then(|size| size.checked_add(HEADER_LEN))
            .context("pack entry count overflows the address space")?;
        if toc_end > data.len() {
            bail!("pack table of contents is truncated");
        }

        let mut entries = budget::alloc::<Entry>(entry_count, "pack table of contents")?;
        let mut lookup = HashMap::new();
        budget::reserve_map(&mut lookup, entry_count, "pack lookup table")?;
        for index in 0..entry_count {
            let toc = &data[HEADER_LEN + index * ENTRY_LEN..HEADER_LEN + (index + 1) * ENTRY_LEN];
            let path_offset = read_u64(toc, 0)?;
            let data_offset = read_u64(toc, 8)?;
            let length = budget::check_len_u64(
                read_u64(toc, 16)?,
                budget::MAX_ENTRY_BYTES,
                &format!("entry {index} length"),
            )?;
            let compression = Compression::from_mode(toc[24])
                .map_err(|err| anyhow!("entry {index} has {err}"))?;
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
                compression,
            });
        }

        let manifest = match lookup.get(&manifest::ENTRY.to_ascii_lowercase()) {
            Some(&index) => {
                let entry = &entries[index];
                let bytes = entry_bytes(&data, entry)?;
                let text =
                    std::str::from_utf8(&bytes).context("manifest.toml is not valid UTF-8")?;
                Some(manifest::Manifest::parse(text).context("invalid manifest.toml")?)
            }
            None => None,
        };

        Ok(Self {
            data,
            entries,
            lookup,
            manifest,
            source: None,
            overlays: Vec::new(),
            warnings: Vec::new(),
        })
    }

    /// The parsed `manifest.toml`, when the pack carries one.
    pub fn manifest(&self) -> Option<&manifest::Manifest> {
        self.manifest.as_ref()
    }

    /// The mod layers in applied order (first applied first, winner last).
    pub fn overrides(&self) -> impl Iterator<Item = &Pack> {
        self.overlays.iter()
    }

    /// Whether any mod layer is applied.
    pub fn is_layered(&self) -> bool {
        !self.overlays.is_empty()
    }

    /// Non-fatal problems found while layering, e.g. a base without a
    /// manifest defaulting to its pack stem.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The filesystem pack that provides `path` in the merged view, for
    /// diagnostics. A pack parsed from memory has no path.
    pub fn layer_of(&self, path: &str) -> Option<&Path> {
        self.winner(path).and_then(|layer| layer.source.as_deref())
    }

    /// The filesystem path this pack was opened from, or `None` when it was
    /// parsed from memory.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// The layer whose entry wins `path`: the last overlay that has it, or the
    /// base, or nothing.
    fn winner(&self, path: &str) -> Option<&Pack> {
        let key = path.to_ascii_lowercase();
        for layer in self.overlays.iter().rev() {
            if layer.lookup.contains_key(&key) {
                return Some(layer);
            }
        }
        self.lookup.contains_key(&key).then_some(self)
    }

    /// Whether an entry with this path exists, ignoring ASCII case.
    pub fn contains(&self, path: &str) -> bool {
        self.winner(path).is_some()
    }

    /// Read an entry's data, looking the path up ignoring ASCII case.
    ///
    /// A raw entry borrows directly from the pack image; a zstd entry is
    /// decompressed into an owned buffer, so it costs CPU and an allocation
    /// per call. The decompressed size is capped by
    /// [`budget::MAX_ZSTD_OUTPUT`] whatever the frame claims.
    pub fn read(&self, path: &str) -> Result<Cow<'_, [u8]>> {
        let Some(layer) = self.winner(path) else {
            bail!("no pack entry named {path}");
        };
        let index = *layer
            .lookup
            .get(&path.to_ascii_lowercase())
            .expect("the winning layer holds the entry");
        entry_bytes(&layer.data, &layer.entries[index])
    }

    /// The merged entries: shadowed base entries hidden, later layers winning,
    /// in lowercased-path order. A single pack keeps its table-of-contents
    /// order.
    fn merged_entries(&self) -> Vec<(&str, usize)> {
        if self.overlays.is_empty() {
            return self
                .entries
                .iter()
                .map(|entry| (entry.path.as_str(), entry.length))
                .collect();
        }
        let mut merged: HashMap<String, (&str, usize)> = HashMap::with_capacity(self.entries.len());
        for entry in &self.entries {
            merged.insert(
                entry.path.to_ascii_lowercase(),
                (entry.path.as_str(), entry.length),
            );
        }
        for layer in &self.overlays {
            for entry in &layer.entries {
                merged.insert(
                    entry.path.to_ascii_lowercase(),
                    (entry.path.as_str(), entry.length),
                );
            }
        }
        let mut merged: Vec<(String, &str, usize)> = merged
            .into_iter()
            .map(|(key, (path, size))| (key, path, size))
            .collect();
        merged.sort_by(|a, b| a.0.cmp(&b.0));
        merged
            .into_iter()
            .map(|(_, path, size)| (path, size))
            .collect()
    }

    /// Original-case entry paths in table-of-contents order, or the merged
    /// view in lowercased-path order when layered.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.merged_entries().into_iter().map(|(path, _)| path)
    }

    /// Every entry in table-of-contents order, or the merged view in
    /// lowercased-path order when layered.
    pub fn entries(&self) -> impl Iterator<Item = PackEntry<'_>> {
        self.merged_entries()
            .into_iter()
            .map(|(path, size)| PackEntry { path, size })
    }

    /// Number of entries in the pack, or in the merged view when layered.
    pub fn len(&self) -> usize {
        if self.overlays.is_empty() {
            return self.entries.len();
        }
        self.merged_entries().len()
    }

    /// Whether the pack (or merged view) has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The default id of a pack without a manifest: its file stem, or `pack`.
fn pack_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("pack")
        .to_string()
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

/// The readable bytes of one entry: borrowed when raw, decompressed when zstd.
fn entry_bytes<'a>(data: &'a [u8], entry: &Entry) -> Result<Cow<'a, [u8]>> {
    let stored = &data[entry.offset..entry.offset + entry.length];
    match entry.compression {
        Compression::None => Ok(Cow::Borrowed(stored)),
        Compression::Zstd => Ok(Cow::Owned(decompress_zstd(stored, &entry.path)?)),
    }
}

/// Decompress one stored zstd entry, bounded by [`budget::MAX_ZSTD_OUTPUT`].
///
/// A frame that declares a larger decompressed size is refused before any
/// buffer grows, and one that produces more is cut off at the cap and
/// rejected, so a malicious pack cannot expand a small entry without bound.
/// The output is reserved with `try_reserve` in 64 KiB steps, so the transient
/// allocation never exceeds the cap.
fn decompress_zstd(stored: &[u8], path: &str) -> Result<Vec<u8>> {
    decompress_zstd_with_cap(stored, path, budget::MAX_ZSTD_OUTPUT)
}

/// [`decompress_zstd`] with an explicit output cap, for the unit tests.
fn decompress_zstd_with_cap(stored: &[u8], path: &str, cap: usize) -> Result<Vec<u8>> {
    if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(stored)
        && size > cap as u64
    {
        bail!("entry {path} declares {size} decompressed bytes, over the {cap} byte limit");
    }
    let mut decoder = zstd::stream::read::Decoder::new(stored)
        .with_context(|| format!("entry {path} is not a valid zstd frame"))?;
    let mut out = Vec::new();
    let mut chunk = [0u8; 64 << 10];
    loop {
        let read = decoder
            .read(&mut chunk)
            .with_context(|| format!("failed to decompress entry {path}"))?;
        if read == 0 {
            return Ok(out);
        }
        if out.len() + read > cap {
            bail!("entry {path} decompresses beyond the {cap} byte limit");
        }
        out.try_reserve(read)
            .map_err(|_| anyhow!("entry {path}: failed to reserve decompression space"))?;
        out.extend_from_slice(&chunk[..read]);
    }
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
        assert_eq!(&*pack.read("room/1000.rdt").unwrap(), &[1, 2, 3]);
        assert_eq!(&*pack.read("roomcut/100_000.bmp").unwrap(), &[9; 64]);
        assert_eq!(&*pack.read("MixEd.TxT").unwrap(), b"hello");
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
    fn streaming_matches_in_memory_serialization() {
        let mut writer = PackWriter::new();
        writer.add("room/1000.rdt", vec![1, 2, 3]).unwrap();
        writer.add("roomcut/100_000.bmp", vec![9; 64]).unwrap();
        writer.add("MixEd.TxT", b"hello".to_vec()).unwrap();

        let bytes = writer.to_bytes().unwrap();
        assert_eq!(writer.pack_size().unwrap(), bytes.len());

        let mut streamed = Vec::new();
        writer.stream_to(&mut streamed).unwrap();
        assert_eq!(streamed, bytes);
    }

    #[test]
    fn streaming_an_empty_pack_is_just_the_header() {
        let writer = PackWriter::new();
        let mut out = Vec::new();
        writer.stream_to(&mut out).unwrap();
        assert_eq!(out, empty_pack());
        assert_eq!(writer.pack_size().unwrap(), HEADER_LEN);
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
        assert_eq!(&*pack.read("dAtA/rOoM.rdT").unwrap(), b"room");
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
    fn rejects_unsupported_compression_mode() {
        let mut bytes = single_entry_pack("a.txt", b"x");
        bytes[HEADER_LEN + 24] = 2;
        let err = Pack::from_bytes(bytes).unwrap_err().to_string();
        assert!(err.contains("compression mode"), "{err}");
    }

    #[test]
    fn compress_entries_round_trips() {
        let rdt = vec![7u8; 64 * 1024];
        let film = vec![1u8, 2, 3, 4];
        let mut writer = PackWriter::new();
        writer.add("room/1000.rdt", rdt.clone()).unwrap();
        writer.add("movie/oj.avi", film.clone()).unwrap();

        let compressed = writer
            .compress_entries(2, |path| path.ends_with(".avi"))
            .unwrap();
        assert_eq!(compressed, 1);

        let bytes = writer.to_bytes().unwrap();
        // Sorted order is movie/oj.avi then room/1000.rdt: the film is mode 0
        // (exempt), the RDT is mode 1.
        assert_eq!(bytes[HEADER_LEN + 24], 0);
        assert_eq!(bytes[HEADER_LEN + ENTRY_LEN + 24], 1);

        let pack = Pack::from_bytes(bytes).unwrap();
        assert_eq!(&*pack.read("room/1000.rdt").unwrap(), rdt.as_slice());
        assert_eq!(&*pack.read("movie/oj.avi").unwrap(), film.as_slice());
    }

    #[test]
    fn compress_entries_keeps_small_entries_raw() {
        let mut writer = PackWriter::new();
        writer.add("a.txt", b"x".to_vec()).unwrap();
        assert_eq!(writer.compress_entries(1, |_| false).unwrap(), 0);
        let bytes = writer.to_bytes().unwrap();
        assert_eq!(bytes[HEADER_LEN + 24], 0);
        let pack = Pack::from_bytes(bytes).unwrap();
        assert_eq!(&*pack.read("a.txt").unwrap(), b"x");
    }

    #[test]
    fn compress_entries_is_deterministic() {
        let entries = [
            ("room/1000.rdt", vec![3u8; 9_000]),
            (
                "roomcut/100_000.bmp",
                (0..=255u8).cycle().take(4_096).collect(),
            ),
            ("bgm/013.wav", vec![0u8; 1_000]),
        ];
        let mut one = PackWriter::new();
        let mut many = PackWriter::new();
        for (path, data) in &entries {
            one.add(path, data.clone()).unwrap();
            many.add(path, data.clone()).unwrap();
        }
        assert_eq!(one.compress_entries(1, |_| false).unwrap(), 3);
        assert_eq!(many.compress_entries(4, |_| false).unwrap(), 3);
        assert_eq!(one.to_bytes().unwrap(), many.to_bytes().unwrap());
    }

    #[test]
    fn reader_rejects_a_corrupt_zstd_entry() {
        let mut bytes = single_entry_pack("a.txt", b"not a zstd frame");
        bytes[HEADER_LEN + 24] = 1;
        let pack = Pack::from_bytes(bytes).unwrap();
        let err = pack.read("a.txt").unwrap_err().to_string();
        assert!(err.contains("a.txt"), "{err}");
    }

    #[test]
    fn a_compressed_manifest_is_still_parsed() {
        let manifest = manifest::Manifest::base("demo");
        let mut writer = PackWriter::new();
        writer
            .add(manifest::ENTRY, manifest.render().into_bytes())
            .unwrap();
        writer.add("room/1000.rdt", vec![5u8; 32 * 1024]).unwrap();
        // The manifest is tiny, so it may or may not shrink; either way the
        // reader must parse it.
        writer.compress_entries(2, |_| false).unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.manifest(), Some(&manifest));
    }

    #[test]
    fn decompression_refuses_a_declared_size_over_the_cap() {
        // A hand-built frame header that declares 4096 bytes with no blocks.
        let frame = [0x28, 0xB5, 0x2F, 0xFD, 0xA0, 0x00, 0x10, 0x00, 0x00];
        assert_eq!(
            zstd::zstd_safe::get_frame_content_size(&frame).unwrap(),
            Some(4096)
        );
        let err = decompress_zstd_with_cap(&frame, "a.bin", 1024)
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn decompression_refuses_output_over_the_cap() {
        // A streaming encoder writes no content size, so the cap is enforced
        // by the read loop rather than the declared-size check.
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        encoder.write_all(&[7u8; 4096]).unwrap();
        let frame = encoder.finish().unwrap();
        assert_eq!(
            zstd::zstd_safe::get_frame_content_size(&frame).unwrap(),
            None
        );
        assert_eq!(
            decompress_zstd_with_cap(&frame, "a.bin", 4096)
                .unwrap()
                .len(),
            4096
        );
        let err = decompress_zstd_with_cap(&frame, "a.bin", 1024)
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn over_cap_entries_are_kept_raw() {
        let mut writer = PackWriter::new();
        writer.add("a.bin", vec![3u8; 4096]).unwrap();
        assert_eq!(
            writer.compress_entries_capped(1, |_| false, 1024).unwrap(),
            0
        );
        let bytes = writer.to_bytes().unwrap();
        assert_eq!(bytes[HEADER_LEN + 24], 0);
        // Raised to the entry's own size, the same entry compresses.
        assert_eq!(
            writer.compress_entries_capped(1, |_| false, 4096).unwrap(),
            1
        );
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
    fn rejects_an_entry_count_over_the_cap_before_allocating() {
        let mut counted = empty_pack();
        let huge = u32::try_from(budget::MAX_PACK_ENTRIES + 1).unwrap();
        counted[6..10].copy_from_slice(&huge.to_le_bytes());
        let message = budget::assert_cap_error(Pack::from_bytes(counted));
        assert!(message.contains("entry count"), "{message}");
    }

    #[test]
    fn rejects_an_entry_length_over_the_cap() {
        let mut bytes = single_entry_pack("a.txt", b"xyz");
        write_u64(&mut bytes, HEADER_LEN + 16, budget::MAX_ENTRY_BYTES + 1);
        let message = budget::assert_cap_error(Pack::from_bytes(bytes));
        assert!(message.contains("length"), "{message}");
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
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let mut writer = PackWriter::new();
        writer.add("a.bin", vec![7, 8, 9]).unwrap();
        writer.write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();
        assert_eq!(&*pack.read("A.BIN").unwrap(), &[7, 8, 9]);
        std::fs::remove_file(&path).unwrap();
    }

    /// Self-deleting temporary directory unique to this process and label.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("arklay-pack-{}-{label}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Write a pack of raw entries to `path`.
    fn write_pack(path: &Path, entries: &[(&str, &[u8])]) {
        let mut writer = PackWriter::new();
        for (entry, data) in entries {
            writer.add(entry, data.to_vec()).unwrap();
        }
        writer.write(path).unwrap();
    }

    /// The rendered bytes of a mod manifest.
    fn mod_manifest(id: &str, base: &str, load_order: i32) -> Vec<u8> {
        manifest::Manifest {
            format: manifest::FORMAT,
            id: id.to_string(),
            name: None,
            version: None,
            kind: manifest::PackKind::Mod,
            base: Some(base.to_string()),
            load_order,
            rdt_version: manifest::DEFAULT_RDT_VERSION.to_string(),
            scd_version: manifest::DEFAULT_SCD_VERSION.to_string(),
            engine: None,
            lua: Vec::new(),
        }
        .render()
        .into_bytes()
    }

    #[test]
    fn layers_shadow_entries_case_insensitively() {
        let dir = TempDir::new("layers");
        let base = dir.join("base.akpak");
        let mod_a = dir.join("mod-a.akpak");
        let mod_b = dir.join("mod-b.akpak");
        let base_manifest = manifest::Manifest::base("base").render().into_bytes();
        write_pack(
            &base,
            &[
                (manifest::ENTRY, &base_manifest),
                ("room/1000.rdt", b"base-room"),
                ("bgm/013.wav", b"music"),
            ],
        );
        write_pack(
            &mod_a,
            &[
                (manifest::ENTRY, &mod_manifest("a", "base", 0)),
                ("ROOM/1000.RDT", b"mod-a"),
            ],
        );
        write_pack(
            &mod_b,
            &[
                (manifest::ENTRY, &mod_manifest("b", "base", 10)),
                ("room/1000.rdt", b"mod-b"),
                ("new/file.bin", b"new"),
            ],
        );

        let layered = Pack::open_layered(&base, &[mod_a, mod_b.clone()]).unwrap();
        assert!(layered.is_layered());
        assert!(layered.warnings().is_empty());
        assert_eq!(layered.manifest().unwrap().id, "base");
        assert_eq!(&*layered.read("room/1000.rdt").unwrap(), b"mod-b");
        assert_eq!(&*layered.read("ROOM/1000.RDT").unwrap(), b"mod-b");
        assert_eq!(&*layered.read("BgM/013.WaV").unwrap(), b"music");
        assert!(layered.contains("new/file.bin"));
        assert!(!layered.contains("missing.bin"));
        assert!(layered.read("missing.bin").is_err());
        assert_eq!(
            layered.paths().collect::<Vec<_>>(),
            [
                "bgm/013.wav",
                "manifest.toml",
                "new/file.bin",
                "room/1000.rdt"
            ]
        );
        assert_eq!(layered.len(), 4);
        assert_eq!(
            layered
                .entries()
                .map(|entry| entry.size())
                .collect::<Vec<_>>(),
            [5, mod_manifest("b", "base", 10).len(), 3, 5]
        );
        assert_eq!(layered.layer_of("room/1000.rdt"), Some(mod_b.as_path()));
        assert_eq!(layered.layer_of("bgm/013.wav"), Some(base.as_path()));
        assert_eq!(layered.layer_of("missing.bin"), None);
        assert_eq!(layered.overrides().count(), 2);
    }

    #[test]
    fn a_mod_layer_shadows_a_compressed_entry() {
        let dir = TempDir::new("compressed-layer");
        let base = dir.join("base.akpak");
        let mod_a = dir.join("mod.akpak");

        let mut writer = PackWriter::new();
        writer.add("bgm/013.wav", vec![2u8; 16 * 1024]).unwrap();
        writer.add("room/1000.rdt", vec![1u8; 32 * 1024]).unwrap();
        writer.compress_entries(2, |_| false).unwrap();
        writer.write(&base).unwrap();

        let layer_bytes = vec![9u8; 8 * 1024];
        let mut writer = PackWriter::new();
        writer
            .add(manifest::ENTRY, mod_manifest("mod", "base", 0))
            .unwrap();
        writer.add("room/1000.rdt", layer_bytes.clone()).unwrap();
        writer
            .compress_entries(1, |path| path == manifest::ENTRY)
            .unwrap();
        writer.write(&mod_a).unwrap();

        let layered = Pack::open_layered(&base, &[mod_a]).unwrap();
        assert_eq!(
            &*layered.read("room/1000.rdt").unwrap(),
            layer_bytes.as_slice()
        );
        assert_eq!(&*layered.read("bgm/013.wav").unwrap(), &[2u8; 16 * 1024]);
    }

    #[test]
    fn later_layers_win_and_sort_by_load_order_then_id() {
        let dir = TempDir::new("order");
        let base = dir.join("base.akpak");
        let first = dir.join("first.akpak");
        let second = dir.join("second.akpak");
        let third = dir.join("third.akpak");
        write_pack(&base, &[("x.txt", b"base")]);
        write_pack(
            &first,
            &[
                (manifest::ENTRY, &mod_manifest("first", "base", 10)),
                ("x.txt", b"first"),
            ],
        );
        write_pack(
            &second,
            &[
                (manifest::ENTRY, &mod_manifest("second", "base", 10)),
                ("X.TXT", b"second"),
            ],
        );
        write_pack(
            &third,
            &[
                (manifest::ENTRY, &mod_manifest("third", "base", 0)),
                ("x.txt", b"third"),
            ],
        );

        // Passed out of order: load_order first, then id, decides application.
        let layered = Pack::open_layered(&base, &[first, second, third]).unwrap();
        let order: Vec<&str> = layered
            .overrides()
            .map(|layer| layer.manifest().unwrap().id.as_str())
            .collect();
        assert_eq!(order, ["third", "first", "second"]);
        assert_eq!(&*layered.read("x.txt").unwrap(), b"second");
        assert_eq!(&*layered.read("X.TXT").unwrap(), b"second");
    }

    #[test]
    fn layered_validation_reports_named_failures() {
        let dir = TempDir::new("validation");
        let base = dir.join("base.akpak");
        let base_manifest = manifest::Manifest::base("base").render().into_bytes();
        write_pack(
            &base,
            &[(manifest::ENTRY, &base_manifest), ("x.txt", b"base")],
        );

        let bare = dir.join("bare.akpak");
        write_pack(&bare, &[("x.txt", b"bare")]);
        let err = Pack::open_layered(&base, std::slice::from_ref(&bare))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("bare.akpak") && err.contains("manifest.toml"),
            "{err}"
        );

        let wrong = dir.join("wrong.akpak");
        write_pack(
            &wrong,
            &[(manifest::ENTRY, &mod_manifest("wrong", "other", 0))],
        );
        let err = Pack::open_layered(&base, &[wrong]).unwrap_err().to_string();
        assert!(
            err.contains("\"other\"") && err.contains("\"base\""),
            "{err}"
        );

        let duplicate_a = dir.join("duplicate-a.akpak");
        let duplicate_b = dir.join("duplicate-b.akpak");
        write_pack(
            &duplicate_a,
            &[(manifest::ENTRY, &mod_manifest("same", "base", 0))],
        );
        write_pack(
            &duplicate_b,
            &[(manifest::ENTRY, &mod_manifest("same", "base", 1))],
        );
        let err = Pack::open_layered(&base, &[duplicate_a, duplicate_b.clone()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate mod id"), "{err}");

        let err = Pack::open_layered(&base, &[duplicate_b.clone(), duplicate_b])
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate mod layer path"), "{err}");

        let other_base = dir.join("other-base.akpak");
        write_pack(
            &other_base,
            &[(
                manifest::ENTRY,
                manifest::Manifest::base("x")
                    .render()
                    .into_bytes()
                    .as_slice(),
            )],
        );
        let err = Pack::open_layered(&base, &[other_base])
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected mod"), "{err}");

        // A base without a manifest still checks a mod's declared base
        // against the stem id it adopts.
        let bare_mod = dir.join("bare-mod.akpak");
        write_pack(
            &bare_mod,
            &[(manifest::ENTRY, &mod_manifest("bare-mod", "other", 0))],
        );
        let err = Pack::open_layered(&bare, &[bare_mod])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("\"other\"") && err.contains("\"bare\""),
            "{err}"
        );
    }

    #[test]
    fn base_without_manifest_defaults_to_its_stem() {
        let dir = TempDir::new("default-base");
        let base = dir.join("re1.akpak");
        write_pack(&base, &[("x.txt", b"base")]);

        let layered = Pack::open_layered(&base, &[]).unwrap();
        let manifest = layered.manifest().unwrap();
        assert_eq!(manifest.id, "re1");
        assert_eq!(manifest.kind, manifest::PackKind::Base);
        assert_eq!(manifest.rdt_version, manifest::DEFAULT_RDT_VERSION);
        assert_eq!(manifest.scd_version, manifest::DEFAULT_SCD_VERSION);
        assert!(!layered.is_layered());
        assert_eq!(layered.warnings().len(), 1);
        assert!(
            layered.warnings()[0].contains("manifest.toml"),
            "{:?}",
            layered.warnings()
        );
    }

    #[test]
    fn single_pack_view_is_unchanged_by_layering() {
        let dir = TempDir::new("single");
        let base = dir.join("base.akpak");
        write_pack(
            &base,
            &[("b.txt", b"bb"), ("A.txt", b"aa"), ("c/C.bin", b"cc")],
        );

        let plain = Pack::open(&base).unwrap();
        let layered = Pack::open_layered(&base, &[]).unwrap();
        assert_eq!(
            plain.paths().collect::<Vec<_>>(),
            layered.paths().collect::<Vec<_>>()
        );
        assert_eq!(
            plain
                .entries()
                .map(|e| (e.path(), e.size()))
                .collect::<Vec<_>>(),
            layered
                .entries()
                .map(|e| (e.path(), e.size()))
                .collect::<Vec<_>>()
        );
        assert_eq!(plain.len(), layered.len());
        for path in plain.paths() {
            assert_eq!(
                plain.read(path).unwrap(),
                layered.read(path).unwrap(),
                "{path}"
            );
        }

        let memory = Pack::from_bytes(std::fs::read(&base).unwrap()).unwrap();
        assert_eq!(
            plain.paths().collect::<Vec<_>>(),
            memory.paths().collect::<Vec<_>>()
        );
        assert_eq!(plain.manifest(), None);
        assert!(memory.manifest().is_none());
    }

    #[test]
    fn pack_surfaces_and_validates_the_manifest() {
        let manifest = manifest::Manifest {
            kind: manifest::PackKind::Mod,
            base: Some("re1".to_string()),
            ..manifest::Manifest::base("demo")
        };
        let mut writer = PackWriter::new();
        writer
            .add(manifest::ENTRY, manifest.render().into_bytes())
            .unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert_eq!(pack.manifest(), Some(&manifest));
        assert!(pack.manifest().unwrap().is_mod());
        assert_eq!(pack.len(), 1);

        let mut writer = PackWriter::new();
        writer
            .add(manifest::ENTRY, b"this is not toml".to_vec())
            .unwrap();
        let err = Pack::from_bytes(writer.to_bytes().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid manifest.toml"), "{err}");
    }
}
