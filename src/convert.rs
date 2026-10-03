//! `convert-game`: migrate a game installation into an `.akpak` pack.
//!
//! M0 converts only room 1000 of RE1: the RDT plus every camera background
//! (`RC100<cam>.pak` -> `/roomcut/100_<cam:03>.bmp`).

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::pack::PackWriter;
use crate::state::RoomId;
use crate::{bmp, lzw, rdt, tim};

/// How many directory levels below the root are searched for `stage1`.
const MAX_STAGE_DEPTH: usize = 2;

/// Three-digit room number converted by M0.
const ROOM: u32 = 100;

/// Player/flag digit converted by M0.
const PLAYER: u8 = 0;

/// RDT file name within the stage directory.
const RDT_FILE: &str = "room1000.rdt";

/// Pack path of the raw RDT.
const RDT_ENTRY: &str = "room/1000.rdt";

/// Pack path prefix of the camera backgrounds.
const ROOMCUT_PREFIX: &str = "roomcut/";

/// Expected camera background dimensions.
const CUT_WIDTH: u32 = 320;
/// Expected camera background dimensions.
const CUT_HEIGHT: u32 = 240;

/// Byte offset of the bit depth field in a BMP header.
const BMP_BPP_OFFSET: usize = 28;

/// One printed summary row.
struct Row {
    pack_path: String,
    source: String,
    dimensions: String,
    bpp: String,
}

/// Convert `root`'s room 1000 into the `.akpak` pack `out`.
pub fn convert_game(root: &Path, out: &Path) -> Result<()> {
    let stage = find_stage1(root)?;
    let rdt_path = find_file(&stage, RDT_FILE)?;
    let rdt_bytes =
        fs::read(&rdt_path).with_context(|| format!("failed to read {}", rdt_path.display()))?;
    let room = rdt::parse(&rdt_bytes, RoomId::from_room_and_player(ROOM, PLAYER))
        .with_context(|| format!("failed to parse {}", rdt_path.display()))?;

    let paks = find_camera_paks(&stage, room.cuts.len())?;
    let mut writer = PackWriter::new();
    let mut rows = Vec::with_capacity(room.cuts.len() + 1);

    rows.push(Row {
        pack_path: RDT_ENTRY.to_owned(),
        source: file_name(&rdt_path),
        dimensions: "-".to_owned(),
        bpp: "-".to_owned(),
    });
    writer.add(RDT_ENTRY, rdt_bytes)?;

    for (camera, pak) in paks.iter().enumerate() {
        let source = file_name(pak);
        let compressed = fs::read(pak).with_context(|| format!("failed to read {source}"))?;
        let decoded =
            lzw::decode(&compressed).with_context(|| format!("failed to LZW-decode {source}"))?;
        let image =
            tim::decode(&decoded).with_context(|| format!("failed to decode {source} as TIM"))?;
        if image.width != CUT_WIDTH || image.height != CUT_HEIGHT {
            bail!(
                "{source}: expected {CUT_WIDTH}x{CUT_HEIGHT}, got {}x{}",
                image.width,
                image.height
            );
        }

        let bmp_bytes = bmp::encode_to_vec(&image)
            .with_context(|| format!("failed to encode {source} as BMP"))?;
        let bpp = bmp_bpp(&bmp_bytes)?;
        let entry = format!("{ROOMCUT_PREFIX}{ROOM}_{camera:03}.bmp");
        writer.add(&entry, bmp_bytes)?;
        rows.push(Row {
            pack_path: entry,
            source,
            dimensions: format!("{}x{}", image.width, image.height),
            bpp: bpp.to_string(),
        });
    }

    writer.write(out)?;
    print_summary(&rows, out);
    Ok(())
}

/// Breadth-first, case-insensitive search for a `stage1` directory.
fn find_stage1(root: &Path) -> Result<PathBuf> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if dir.is_dir()
            && dir
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("stage1"))
        {
            return Ok(dir);
        }
        if depth == MAX_STAGE_DEPTH {
            continue;
        }
        for entry in read_dir_sorted(&dir)? {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }
    bail!(
        "no `stage1` directory within {MAX_STAGE_DEPTH} levels of {}",
        root.display()
    )
}

/// Case-insensitive lookup of one file directly inside `dir`.
fn find_file(dir: &Path, name: &str) -> Result<PathBuf> {
    for entry in read_dir_sorted(dir)? {
        if entry.file_name().eq_ignore_ascii_case(name) {
            return Ok(entry.path());
        }
    }
    bail!("no file named {name} in {}", dir.display())
}

/// Expected camera background file name, e.g. `RC100A.pak`.
fn camera_pak_name(camera: usize) -> String {
    format!("RC{ROOM}{camera:X}.pak")
}

/// Resolve every camera background, aggregating all missing names.
fn find_camera_paks(dir: &Path, count: usize) -> Result<Vec<PathBuf>> {
    let entries = read_dir_sorted(dir)?;
    let actual: Vec<String> = entries
        .iter()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let expected: Vec<String> = (0..count).map(camera_pak_name).collect();

    let missing = missing_names(&expected, &actual);
    if !missing.is_empty() {
        bail!(
            "missing camera background file(s) in {}: {}",
            dir.display(),
            missing.join(", ")
        );
    }

    expected
        .iter()
        .map(|name| {
            entries
                .iter()
                .find(|entry| entry.file_name().eq_ignore_ascii_case(name))
                .map(|entry| entry.path())
                .with_context(|| format!("missing camera background {name} in {}", dir.display()))
        })
        .collect()
}

/// Names in `expected` without a case-insensitive match in `actual`.
fn missing_names(expected: &[String], actual: &[String]) -> Vec<String> {
    expected
        .iter()
        .filter(|name| {
            !actual
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(name))
        })
        .cloned()
        .collect()
}

/// Read a directory and sort its entries by file name.
fn read_dir_sorted(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("failed to open directory {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to list directory {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

/// The file name of `path` as a display string.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Read the bit depth from a BMP header.
fn bmp_bpp(data: &[u8]) -> Result<u16> {
    let raw = data
        .get(BMP_BPP_OFFSET..BMP_BPP_OFFSET + 2)
        .context("encoded BMP is missing its bit depth field")?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

/// Print the conversion table and the output path.
fn print_summary(rows: &[Row], out: &Path) {
    let pack_width = rows
        .iter()
        .map(|row| row.pack_path.len())
        .chain([4])
        .max()
        .unwrap();
    let source_width = rows
        .iter()
        .map(|row| row.source.len())
        .chain([6])
        .max()
        .unwrap();
    let dim_width = rows
        .iter()
        .map(|row| row.dimensions.len())
        .chain([10])
        .max()
        .unwrap();

    println!(
        "{:<pack_width$}  {:<source_width$}  {:<dim_width$}  bpp",
        "pack", "source", "dimensions"
    );
    for row in rows {
        println!(
            "{:<pack_width$}  {:<source_width$}  {:<dim_width$}  {}",
            row.pack_path, row.source, row.dimensions, row.bpp
        );
    }
    println!("wrote {}", out.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-deleting temporary directory unique to this process and label.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("arklay-convert-{}-{label}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn discovers_stage1_at_root_and_depths() {
        let root = TempDir::new("discover-root");
        let stage = root.path.join("sTaGe1");
        fs::create_dir_all(&stage).unwrap();
        assert_eq!(find_stage1(&stage).unwrap(), stage);

        let depth1 = TempDir::new("discover-depth1");
        fs::create_dir_all(depth1.path.join("STAGE1")).unwrap();
        assert_eq!(
            find_stage1(&depth1.path).unwrap(),
            depth1.path.join("STAGE1")
        );

        let depth2 = TempDir::new("discover-depth2");
        fs::create_dir_all(depth2.path.join("a/StAgE1")).unwrap();
        assert_eq!(
            find_stage1(&depth2.path).unwrap(),
            depth2.path.join("a/StAgE1")
        );

        let shallow = TempDir::new("discover-shallow");
        fs::create_dir_all(shallow.path.join("stage1")).unwrap();
        fs::create_dir_all(shallow.path.join("aaa/STAGE1")).unwrap();
        assert_eq!(
            find_stage1(&shallow.path).unwrap(),
            shallow.path.join("stage1")
        );
    }

    #[test]
    fn errors_when_stage1_is_missing() {
        let temp = TempDir::new("missing-stage1");
        fs::create_dir_all(temp.path.join("a/b/stage1")).unwrap();

        let message = find_stage1(&temp.path).unwrap_err().to_string();

        assert!(message.contains("stage1"), "{message}");
        assert!(
            message.contains(&temp.path.display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn finds_rdt_case_insensitively() {
        let temp = TempDir::new("rdt-case");
        let rdt = temp.path.join("Room1000.RDT");
        fs::write(&rdt, b"rdt").unwrap();

        assert_eq!(find_file(&temp.path, RDT_FILE).unwrap(), rdt);
    }

    #[test]
    fn errors_when_rdt_is_missing() {
        let temp = TempDir::new("missing-rdt");

        let message = find_file(&temp.path, RDT_FILE).unwrap_err().to_string();

        assert!(message.contains(RDT_FILE), "{message}");
        assert!(
            message.contains(&temp.path.display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn missing_names_reports_every_expected_file() {
        let expected = [
            "RC1000.pak".to_owned(),
            "RC1001.pak".to_owned(),
            "RC1002.pak".to_owned(),
        ];
        let actual = ["rc1002.PAK".to_owned(), "other.bin".to_owned()];

        let missing = missing_names(&expected, &actual);

        assert_eq!(missing, ["RC1000.pak", "RC1001.pak"]);
    }

    #[test]
    fn missing_camera_paks_aggregate_in_one_error() {
        let temp = TempDir::new("missing-paks");
        fs::write(temp.path.join("rc1000.PAK"), b"x").unwrap();
        fs::write(temp.path.join("RC1002.pak"), b"x").unwrap();

        let message = find_camera_paks(&temp.path, 4).unwrap_err().to_string();

        assert!(message.contains("RC1001.pak"), "{message}");
        assert!(message.contains("RC1003.pak"), "{message}");
        assert!(!message.contains("RC1000.pak"), "{message}");
        assert!(!message.contains("RC1002.pak"), "{message}");
        assert!(
            message.contains(&temp.path.display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn finds_camera_paks_in_camera_order() {
        let temp = TempDir::new("pak-order");
        fs::write(temp.path.join("rc1001.PAK"), b"second").unwrap();
        fs::write(temp.path.join("RC1000.pak"), b"first").unwrap();

        let paks = find_camera_paks(&temp.path, 2).unwrap();

        assert_eq!(paks.len(), 2);
        assert_eq!(fs::read(&paks[0]).unwrap(), b"first");
        assert_eq!(fs::read(&paks[1]).unwrap(), b"second");
    }

    #[test]
    fn reads_bmp_bit_depth_from_header() {
        let mut data = vec![0u8; 30];
        data[28..30].copy_from_slice(&8u16.to_le_bytes());
        assert_eq!(bmp_bpp(&data).unwrap(), 8);
        data[28..30].copy_from_slice(&24u16.to_le_bytes());
        assert_eq!(bmp_bpp(&data).unwrap(), 24);
        assert!(bmp_bpp(&data[..28]).is_err());
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn converts_real_install() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let stage = find_stage1(&root).unwrap();
        let rdt_path = find_file(&stage, RDT_FILE).unwrap();
        let rdt_bytes = fs::read(&rdt_path).unwrap();
        let room = rdt::parse(&rdt_bytes, RoomId::from_room_and_player(ROOM, PLAYER)).unwrap();

        let temp = TempDir::new("real-install");
        let out = temp.path.join("re1.akpak");
        convert_game(&root, &out).unwrap();

        let pack = crate::pack::Pack::open(&out).unwrap();
        assert_eq!(pack.len(), 1 + room.cuts.len());

        let mut cuts = 0;
        for path in pack.paths() {
            if path.starts_with(ROOMCUT_PREFIX) {
                let image = bmp::decode(pack.read(path).unwrap()).unwrap();
                assert_eq!(image.width, CUT_WIDTH);
                assert_eq!(image.height, CUT_HEIGHT);
                cuts += 1;
            }
        }
        assert_eq!(cuts, room.cuts.len());
    }
}
