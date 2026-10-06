//! M14 real-asset tests: the AVI demuxer, the Cinepak decoder, the movie pack
//! and the playback session.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=... ARKLAY_RE1_PACK=... \
//!  ARKLAY_MOVIE_GOLDEN=... cargo test --test m14_real -- --ignored --nocapture`
//!
//! The playback tests additionally need the film pack: `ARKLAY_RE1_MOVIE`, or
//! a sibling `<pack stem>.movie.akpak` next to `ARKLAY_RE1_PACK`; a configured
//! environment without one is a partial setup and fails. The integer-pixel
//! comparison needs `ARKLAY_MOVIE_GOLDEN`, a directory of sampled BMPs and a
//! `hashes_crc32.txt` (never committed). The golden sets used so far were
//! produced by a same-author Python reimplementation of the codec, not a
//! third-party decoder, so the comparison is a cross-implementation check
//! rather than fully independent verification; see `docs/m14-deviations.md`
//! item 8. A set variable with missing goldens fails; only an unset variable
//! skips.

mod common;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use arklay::avi::Avi;
use arklay::cinepak::Decoder;
use arklay::convert::{MoviePackOptions, VoicePackOptions, convert_game_with_packs};
use arklay::engine::{simulate_room_with_input, simulate_room_with_movie};
use arklay::movie::{self, MovieSession, MovieTick};
use arklay::pack::Pack;
use arklay::player;
use arklay::scd::ir::Decoded;
use arklay::scd::reader;
use arklay::state::RoomId;

/// `(basename, video frames, audio chunks)` for all 27 shipped films.
const FILMS: &[(&str, usize, usize)] = &[
    ("capcom", 76, 76),
    ("dm1", 74, 74),
    ("dm2", 74, 74),
    ("dm3", 154, 154),
    ("dm4", 147, 147),
    ("dm6", 150, 150),
    ("dm7", 67, 60),
    ("dm8", 112, 112),
    ("dmb", 180, 180),
    ("dmc", 180, 180),
    ("dmd", 116, 116),
    ("dme", 134, 134),
    ("dmf", 66, 66),
    ("oj", 128, 128),
    ("pj", 2261, 2257),
    ("ed1", 586, 586),
    ("ed2", 432, 432),
    ("ed3", 390, 390),
    ("ed4", 480, 480),
    ("ed5", 539, 539),
    ("ed6", 458, 458),
    ("ed7", 458, 458),
    ("ed8", 94, 94),
    ("staf_b", 1800, 1800),
    ("stfc_b", 1200, 1200),
    ("stfj_b", 1200, 1200),
    ("stfz_b", 1204, 1200),
];

/// The total shipped video frames.
const TOTAL_FRAMES: usize = 12_760;
/// The total shipped movie bytes.
const TOTAL_BYTES: u64 = 251_624_740;

/// The `JPN/movie` directory of the real install.
fn movie_dir(root: &Path) -> PathBuf {
    root.join("JPN").join("movie")
}

/// Read and parse one installed film.
fn parse_film(root: &Path, name: &str) -> Avi {
    let path = movie_dir(root).join(format!("{name}.avi"));
    let bytes = fs::read(&path).unwrap_or_else(|error| panic!("failed to read {path:?}: {error}"));
    Avi::parse(bytes).unwrap_or_else(|error| panic!("failed to parse {path:?}: {error}"))
}

/// The golden-decoder directory, or `None` when not configured. A set variable
/// whose directory or hash file is missing fails rather than skipping.
fn golden_dir() -> Option<PathBuf> {
    let dir = std::env::var("ARKLAY_MOVIE_GOLDEN").ok()?;
    let dir = PathBuf::from(dir);
    assert!(
        dir.is_dir(),
        "ARKLAY_MOVIE_GOLDEN is set but {} is not a directory",
        dir.display()
    );
    let hashes = dir.join("hashes_crc32.txt");
    assert!(
        hashes.is_file(),
        "ARKLAY_MOVIE_GOLDEN is set but {} is missing",
        hashes.display()
    );
    Some(dir)
}

/// The converted film pack: `ARKLAY_RE1_MOVIE`, or the sibling
/// `<pack stem>.movie.akpak` beside `ARKLAY_RE1_PACK`.
///
/// A configured asset environment without a movie pack is a partial setup and
/// fails loudly; callers only reach this after [`common::asset_env`] succeeded.
fn real_movie_pack(pack_path: &Path) -> PathBuf {
    if let Ok(path) = std::env::var("ARKLAY_RE1_MOVIE") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "ARKLAY_RE1_MOVIE is set but {} is not a file",
            path.display()
        );
        return path;
    }
    let stem = pack_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_else(|| panic!("pack path {} has no file stem", pack_path.display()));
    let sibling = pack_path.with_file_name(format!("{stem}.movie.akpak"));
    assert!(
        sibling.is_file(),
        "movie pack {} is missing; set ARKLAY_RE1_MOVIE or convert the sibling pack",
        sibling.display()
    );
    sibling
}

/// A self-deleting temporary directory unique to this process and label.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("arklay-m14-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// CRC-32 (IEEE) over the packed RGB canvas, shared with the golden
/// generator.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Decode one whole film and return the per-frame CRC-32 hashes.
fn decode_film(avi: &Avi) -> Vec<u32> {
    let mut decoder = Decoder::new(320, 240).expect("320x240 is within the canvas cap");
    let mut hashes = Vec::with_capacity(avi.frame_count());
    for index in 0..avi.frame_count() {
        let frame = avi.video(index).expect("indexed frame exists");
        decoder
            .decode(frame)
            .unwrap_or_else(|error| panic!("frame {index} failed to decode: {error}"));
        assert_eq!(decoder.rgb().len(), 320 * 240 * 3);
        hashes.push(crc32(decoder.rgb()));
    }
    hashes
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn all_shipped_films_parse_with_the_expected_format() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let mut total_frames = 0usize;
    let mut total_bytes = 0u64;
    for &(name, frames, audio_chunks) in FILMS {
        let path = movie_dir(&root).join(format!("{name}.avi"));
        total_bytes += fs::metadata(&path).expect("film exists").len();
        let avi = parse_film(&root, name);
        let format = avi.format();
        assert_eq!((format.width, format.height), (320, 240), "{name}");
        assert_eq!(format.video_codec, *b"cvid", "{name}");
        assert_eq!(format.total_frames as usize, frames, "{name}");
        assert_eq!(avi.frame_count(), frames, "{name}");
        let audio_frames = (0..avi.frame_count())
            .filter(|&index| avi.audio(index).is_some())
            .count();
        assert_eq!(audio_frames, audio_chunks, "{name}");
        for index in 0..avi.frame_count() {
            assert!(
                avi.video(index).is_some_and(|frame| !frame.is_empty()),
                "{name} frame {index} is missing"
            );
        }
        let expected_rate = if name == "staf_b" { (15, 1) } else { (10, 1) };
        assert_eq!(format.frame_rate, expected_rate, "{name}");
        assert_eq!(format.audio.format_tag, 1, "{name} is PCM");
        assert_eq!(format.audio.channels, 2, "{name}");
        assert_eq!(format.audio.avg_bytes_per_sec, 88_200, "{name}");
        // Every film is 22050 Hz s16 stereo except the Capcom logo, whose
        // shipped header is the byte-equivalent 44100 Hz 8-bit stereo.
        if name == "capcom" {
            assert_eq!(format.audio.sample_rate, 44_100, "{name}");
            assert_eq!(format.audio.bits_per_sample, 8, "{name}");
            assert_eq!(format.audio.block_align, 2, "{name}");
            assert_eq!(avi.samples_per_frame(), 4410, "{name}");
        } else {
            assert_eq!(format.audio.sample_rate, 22_050, "{name}");
            assert_eq!(format.audio.bits_per_sample, 16, "{name}");
            assert_eq!(format.audio.block_align, 4, "{name}");
            let expected = if name == "staf_b" { 1470 } else { 2205 };
            assert_eq!(avi.samples_per_frame(), expected, "{name}");
        }
        assert_eq!(
            avi.samples_before(3),
            avi.samples_per_frame() as u64 * 3,
            "{name}"
        );
        total_frames += avi.frame_count();
    }
    assert_eq!(total_frames, TOTAL_FRAMES);
    assert_eq!(total_bytes, TOTAL_BYTES);
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn every_frame_decodes_deterministically() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let mut total = 0usize;
    for &(name, frames, _) in FILMS {
        let avi = parse_film(&root, name);
        assert_eq!(avi.frame_count(), frames, "{name}");
        let first = decode_film(&avi);
        let second = decode_film(&avi);
        assert_eq!(first, second, "{name} decoded differently on a second pass");
        total += first.len();
    }
    assert_eq!(total, TOTAL_FRAMES);
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn decoded_frames_match_the_independent_goldens() {
    // A configured golden set without assets is a partial setup: fail before
    // the asset skip could hide it.
    let Some(golden) = golden_dir() else {
        eprintln!(
            "note: ARKLAY_MOVIE_GOLDEN is unset; skipping the golden comparison. \
             The goldens used by this project come from a same-author \
             reimplementation, not a third-party decoder \
             (docs/m14-deviations.md item 8)."
        );
        return;
    };
    let Some((root, _pack)) = common::asset_env() else {
        panic!("ARKLAY_MOVIE_GOLDEN is set but ARKLAY_RE1_ROOT/ARKLAY_RE1_PACK are not");
    };
    let hashes_path = golden.join("hashes_crc32.txt");
    let golden_hashes = fs::read_to_string(&hashes_path)
        .unwrap_or_else(|error| panic!("failed to read {hashes_path:?}: {error}"));
    let expected: std::collections::HashSet<&str> = golden_hashes.lines().collect();
    assert_eq!(
        expected.len(),
        TOTAL_FRAMES,
        "the golden hash file is incomplete"
    );
    let mut matched = 0usize;
    for &(name, frames, _) in FILMS {
        let avi = parse_film(&root, name);
        let picks = [0usize, frames / 2, frames - 1];
        let mut decoder = Decoder::new(320, 240).expect("320x240 is within the canvas cap");
        let mut rgba = vec![0u8; 320 * 240 * 4];
        for index in 0..frames {
            let frame = avi.video(index).unwrap();
            decoder
                .decode(frame)
                .unwrap_or_else(|error| panic!("{name} frame {index} failed: {error}"));
            let line = format!("{name} {index} {:08x}", crc32(decoder.rgb()));
            assert!(
                expected.contains(line.as_str()),
                "{name} frame {index} does not match the golden decoder ({line})"
            );
            matched += 1;
            if picks.contains(&index) {
                decoder.rgba_into(&mut rgba);
                let bmp_path = golden.join(format!("{name}_{index:04}.bmp"));
                let bytes = fs::read(&bmp_path)
                    .unwrap_or_else(|error| panic!("failed to read {bmp_path:?}: {error}"));
                let image = arklay::bmp::decode(&bytes)
                    .unwrap_or_else(|error| panic!("failed to decode {bmp_path:?}: {error}"));
                assert_eq!((image.width, image.height), (320, 240), "{bmp_path:?}");
                assert_eq!(image.rgba, rgba, "{name} frame {index} pixels differ");
            }
        }
    }
    assert_eq!(matched, TOTAL_FRAMES);
}

/// The film ids the shipped room scripts request through `movie_on` (0x29):
/// ids 3-9 and 11-13 across 19 sites.
const MOVIE_SITE_IDS: &[u8] = &[3, 4, 5, 6, 7, 8, 9, 11, 12, 13];

#[test]
#[ignore = "requires a converted movie pack"]
fn pj_prologue_cut_resumes_at_the_second_cut_point() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let movie_path = real_movie_pack(&pack_path);
    let pack = Pack::open(&movie_path).unwrap();

    // Chris plays the whole 2261-frame intro.
    let chris = MovieSession::open(&pack, 1, 0).unwrap();
    assert_eq!(chris.frame_count(), 2261);
    assert_eq!(chris.avi_frame_index(), 0);

    // Jill presents up to the frame before the cut (AVI 1777), then the next
    // advance resumes at the second cut point (AVI 1885).
    let mut jill = MovieSession::open(&pack, 1, 1).unwrap();
    assert_eq!(jill.frame_count(), 2261 - (1885 - 1778));
    let mut ticks = 0;
    while jill.frame_index() < 1777 {
        match jill.tick(0, None) {
            MovieTick::Waiting | MovieTick::Advanced => {}
            MovieTick::Finished | MovieTick::Skipped => panic!("the cut film ended early"),
        }
        ticks += 1;
        assert!(ticks < 6000, "the film never reached the cut");
    }
    assert_eq!(jill.avi_frame_index(), 1777);
    loop {
        match jill.tick(0, None) {
            MovieTick::Advanced => break,
            MovieTick::Waiting => {}
            MovieTick::Finished | MovieTick::Skipped => panic!("the cut film ended early"),
        }
    }
    assert_eq!(jill.frame_index(), 1778);
    assert_eq!(jill.avi_frame_index(), 1885);
    // The audio clock counts kept frames only: kept 1778 is 107 frames short
    // of its raw AVI clock.
    assert_eq!(jill.samples_before(1778), 1778 * 2205);
    println!(
        "pj cut: {} kept frames, resumed at AVI 1885 after {ticks} ticks",
        jill.frame_count()
    );
}

#[test]
#[ignore = "requires a real RE1 installation and a converted movie pack"]
fn every_shipped_movie_on_site_resolves_to_a_packed_film() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    let movie_path = real_movie_pack(&pack_path);
    let pack = Pack::open(&movie_path).unwrap();

    let mut sites = 0usize;
    let mut ids = HashSet::new();
    let mut missing = Vec::new();
    for stage in 1..=7u8 {
        let dir = root.join("JPN").join(format!("STAGE{stage}"));
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(digits) = name
                .to_ascii_lowercase()
                .strip_prefix("room")
                .and_then(|name| name.strip_suffix(".rdt"))
                .map(str::to_owned)
            else {
                continue;
            };
            let bytes = fs::read(entry.path()).unwrap();
            let Ok(scripts) = reader::parse(&bytes) else {
                continue;
            };
            for insn in scripts
                .init
                .iter()
                .flat_map(|block| block.insns.iter())
                .chain(scripts.main.iter().flat_map(|block| block.insns.iter()))
                .chain(scripts.events.iter().flat_map(|stream| stream.insns.iter()))
            {
                let Decoded::Command(op) = insn.decoded else {
                    continue;
                };
                if op.op != 0x29 {
                    continue;
                }
                // The reader decodes `movie_on`'s one-byte `u` operand as the
                // high byte of the original's id word, which is the id.
                let id = insn.operands.first().map_or(0, |operand| operand.value) as u8;
                match movie::name(id) {
                    Some(name) if pack.contains(&movie::pack_path(name)) => {
                        sites += 1;
                        ids.insert(id);
                    }
                    Some(name) => missing.push(format!(
                        "{} (room {digits} movie_on {id})",
                        movie::pack_path(name)
                    )),
                    None => missing.push(format!("null id {id} (room {digits})")),
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "unresolved movie_on site(s): {missing:?}"
    );
    assert_eq!(sites, 19, "the shipped corpus has 19 movie_on sites");
    let expected: HashSet<u8> = MOVIE_SITE_IDS.iter().copied().collect();
    assert_eq!(ids, expected, "the shipped movie_on ids changed");
    println!("resolved {sites} movie_on sites: {ids:?}");
}

/// The film table ids the shipped install packs (every named film but the
/// absent Virgin logo).
#[test]
#[ignore = "requires a converted movie pack"]
fn the_movie_pack_carries_the_shipped_films() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let movie_path = real_movie_pack(&pack_path);
    let pack = Pack::open(&movie_path).unwrap();
    let mut expected = 0usize;
    for id in 0..=28u8 {
        let Some(name) = movie::name(id) else {
            continue;
        };
        let entry = movie::pack_path(name);
        if name == "vlogo" {
            assert!(!pack.contains(&entry), "vlogo is absent from this install");
            continue;
        }
        expected += 1;
        assert!(pack.contains(&entry), "missing {entry}");
    }
    assert_eq!(expected, 27, "27 named films ship in this install");
    assert_eq!(
        pack.paths()
            .filter(|path| path.starts_with("movie/"))
            .count(),
        27
    );
    let bytes: u64 = pack
        .paths()
        .filter(|path| path.starts_with("movie/"))
        .map(|path| pack.read(path).unwrap().len() as u64)
        .sum();
    assert_eq!(bytes, TOTAL_BYTES);
    println!("movie pack: {} entries, {bytes} bytes", pack.len());
}

/// Decode one whole film through the session clock and return the per-frame
/// RGBA hashes in presentation order.
fn session_hashes(pack: &Pack, id: u8, character: u8) -> Vec<u32> {
    let mut session = MovieSession::open(pack, id, character).unwrap();
    let mut hashes = vec![crc32(session.frame_rgba())];
    loop {
        match session.tick(0, None) {
            MovieTick::Advanced => hashes.push(crc32(session.frame_rgba())),
            MovieTick::Waiting => {}
            MovieTick::Finished | MovieTick::Skipped => break,
        }
    }
    hashes
}

#[test]
#[ignore = "requires a converted movie pack"]
fn a_full_film_decodes_to_its_frame_count_with_a_stable_hash() {
    let Some((root, pack_path)) = common::asset_env() else {
        return;
    };
    let movie_path = real_movie_pack(&pack_path);
    let pack = Pack::open(&movie_path).unwrap();
    for (id, frames) in [(23u8, 76usize), (4, 147)] {
        let first = session_hashes(&pack, id, 0);
        assert_eq!(first.len(), frames, "film id {id}");
        let second = session_hashes(&pack, id, 0);
        assert_eq!(
            first, second,
            "film id {id} decoded differently on a second pass"
        );
        println!(
            "film id {id}: {frames} frames, last hash {:08x}",
            first[frames - 1]
        );
    }
    // The 44100 Hz 8-bit logo is converted to the mixer's rate, so its audio
    // clock stays 2205 samples per 10 fps frame.
    let avi = parse_film(&root, "capcom");
    assert_eq!(avi.samples_per_frame(), 4410);
    let session = MovieSession::open(&pack, 23, 0).unwrap();
    assert_eq!(session.samples_per_frame(), 2205);
}

#[test]
#[ignore = "requires a converted movie pack"]
fn a_short_audio_film_pads_silence_without_stalling() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let movie_path = real_movie_pack(&pack_path);
    let pack = Pack::open(&movie_path).unwrap();
    // `dm7` ships 60 audio chunks for 67 video frames.
    let mut session = MovieSession::open(&pack, 7, 0).unwrap();
    assert_eq!(session.frame_count(), 67);
    let mut samples = session.take_audio().len();
    let mut guard = 0;
    loop {
        match session.tick(0, None) {
            MovieTick::Waiting => {}
            MovieTick::Advanced => {}
            MovieTick::Finished | MovieTick::Skipped => break,
        }
        samples += session.take_audio().len();
        guard += 1;
        assert!(guard < 1000, "the short-audio film stalled");
    }
    assert_eq!(
        samples,
        67 * 2205 * 2,
        "every frame queues one frame of audio"
    );
}

#[test]
#[ignore = "requires a converted movie pack and SDL's offscreen driver"]
fn standalone_fmv_capture_is_deterministic_and_not_blank() {
    use std::process::Command;

    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let movie_path = real_movie_pack(&pack_path);
    let dir = TempDir::new("fmv");
    let first = dir.0.join("first.bmp");
    let second = dir.0.join("second.bmp");
    // The current test binary carries no `arklay` CLI, so shell out to the
    // built binary: `--fmv 23 --ticks 6 --capture` is frame 2 of the Capcom
    // logo, the same bytes every run.
    let binary = env!("CARGO_BIN_EXE_arklay");
    for path in [&first, &second] {
        let status = Command::new(binary)
            .args([
                pack_path.to_str().unwrap(),
                "--movie",
                movie_path.to_str().unwrap(),
                "--fmv",
                "23",
                "--ticks",
                "6",
                "--capture",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("failed to run the arklay binary");
        assert!(status.success(), "standalone --fmv capture failed");
    }
    assert_eq!(
        fs::read(&first).unwrap(),
        fs::read(&second).unwrap(),
        "--fmv captures differ between runs"
    );
    let image = arklay::bmp::decode(&fs::read(&first).unwrap()).unwrap();
    assert_eq!((image.width, image.height), (320, 240));
    assert!(
        image
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0),
        "the Capcom logo frame is blank"
    );
    println!(
        "--fmv 23 capture: {} bytes",
        fs::metadata(&first).unwrap().len()
    );
}

/// Inventory the pack's room entries.
fn room_ids(pack: &Pack) -> Vec<RoomId> {
    let mut ids: Vec<RoomId> = pack
        .paths()
        .filter(|path| path.starts_with("room/") && path.ends_with(".rdt"))
        .filter_map(|path| RoomId::parse(&path[5..9]).ok())
        .collect();
    ids.sort_by_key(|id| (id.stage, id.room, id.player_flag));
    ids.dedup_by_key(|id| (id.stage, id.room, id.player_flag));
    ids
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_m14_corpus_audit_drains_every_film_request() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let ids = room_ids(&pack);
    assert!(ids.len() > 300, "expected the shipped room corpus");

    let mut simulated = 0usize;
    let mut requested = 0u64;
    let mut misses = 0u64;
    for id in &ids {
        let Ok(sim) = simulate_room_with_input(&pack, *id, 300, |tick| player::Input {
            up: true,
            action_pressed: tick % 15 == 0,
            ..player::Input::default()
        }) else {
            // Stub rooms without camera cuts cannot load.
            continue;
        };
        simulated += 1;
        assert!(
            !sim.game.placeholders.contains_key(&0x29),
            "ROOM{id:?} dispatched a movie_on placeholder"
        );
        requested += sim.game.fmv.taken;
        misses += sim.game.fmv.misses;
    }
    assert!(simulated > 300, "only {simulated} rooms simulated");
    assert_eq!(misses, 0, "no shipped movie_on id is filtered");
    assert!(
        requested > 0,
        "no room drained a film request; the audit is vacuous"
    );
    println!(
        "corpus: {simulated} rooms, {requested} film requests drained, {misses} misses, \
         zero movie_on placeholders"
    );
}

#[test]
#[ignore = "requires a converted movie pack"]
fn movie_on_sites_start_and_resume_the_room_with_its_bgm() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let movie_path = real_movie_pack(&pack_path);
    let pack = Pack::open(&pack_path).unwrap();
    let movie = Pack::open(&movie_path).unwrap();

    // Room 5130's init requests `movie_on 11`, 5131's `movie_on 12` and 60A0's
    // event `movie_on 6` on a fresh boot; 71C0's main script needs the room
    // flag the original sets on the first visit (SCD bank 4 bit 24).
    type Case<'a> = (&'a str, &'a [(u8, u8)], u8);
    let cases: [Case<'_>; 4] = [
        ("5130", &[], 11),
        ("5131", &[], 12),
        ("71c0", &[(4, 24)], 4),
        ("60a0", &[], 6),
    ];
    for (room, flags, film) in cases {
        let id = RoomId::parse(room).unwrap();
        let sim =
            simulate_room_with_movie(&pack, &movie, id, flags, 900, |_| player::Input::default())
                .unwrap();
        assert_eq!(
            sim.handoff.requested,
            vec![film],
            "ROOM{room} must request film {film}"
        );
        assert_eq!(
            sim.handoff.played,
            vec![film],
            "ROOM{room} film {film} must run to completion"
        );
        assert_eq!(
            sim.handoff.bgm_at_start, sim.handoff.bgm_at_end,
            "ROOM{room} BGM state must survive the film"
        );
        assert_eq!(sim.room.game.fmv.misses, 0);
        assert!(sim.room.game.fmv.request.is_none());
        assert!(!sim.room.game.fmv_requested());
        assert!(
            sim.room.game.frame > 0 && sim.room.game.frame < 900,
            "ROOM{room} room frame {} did not freeze for the film",
            sim.room.game.frame
        );
        println!(
            "ROOM{room}: film {film} played, room resumed at frame {}",
            sim.room.game.frame
        );
    }
}

#[test]
#[ignore = "requires a converted movie pack and SDL's offscreen driver"]
fn room_capture_is_unchanged_with_and_without_the_movie_pack() {
    use std::process::Command;

    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let movie_path = real_movie_pack(&pack_path);
    let dir = TempDir::new("movie-capture");
    let with = dir.0.join("with.bmp");
    let without = dir.0.join("without.bmp");
    // Room 5130 requests `movie_on 11` from its init script; the capture loop
    // drains the request, so the frame must not depend on the movie pack.
    let binary = env!("CARGO_BIN_EXE_arklay");
    for (path, movie) in [(&with, Some(movie_path.as_path())), (&without, None)] {
        let mut command = Command::new(binary);
        command.args([
            pack_path.to_str().unwrap(),
            "--room",
            "513",
            "--player",
            "0",
            "--ticks",
            "120",
            "--capture",
            path.to_str().unwrap(),
        ]);
        if let Some(movie) = movie {
            command.args(["--movie", movie.to_str().unwrap()]);
        }
        let status = command.status().expect("failed to run the arklay binary");
        assert!(status.success(), "the room capture failed");
    }
    assert_eq!(
        fs::read(&with).unwrap(),
        fs::read(&without).unwrap(),
        "the movie pack changed a drained room capture"
    );
    println!(
        "room 5130 capture: {} bytes, identical with and without the movie pack",
        fs::metadata(&with).unwrap().len()
    );

    // The title capture skips the boot/title films, so it is stable too, and
    // the bare root capture is the same frame.
    let title_with = dir.0.join("title_with.bmp");
    let title_without = dir.0.join("title_without.bmp");
    let root = dir.0.join("root.bmp");
    for (path, movie) in [
        (&title_with, Some(movie_path.as_path())),
        (&title_without, None),
    ] {
        let mut command = Command::new(binary);
        command.args([
            pack_path.to_str().unwrap(),
            "--ui",
            "title",
            "--capture",
            path.to_str().unwrap(),
        ]);
        if let Some(movie) = movie {
            command.args(["--movie", movie.to_str().unwrap()]);
        }
        let status = command.status().expect("failed to run the arklay binary");
        assert!(status.success(), "the title capture failed");
    }
    let status = Command::new(binary)
        .args([
            pack_path.to_str().unwrap(),
            "--capture",
            root.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run the arklay binary");
    assert!(status.success(), "the root capture failed");
    assert_eq!(
        fs::read(&title_with).unwrap(),
        fs::read(&title_without).unwrap(),
        "the movie pack changed the title capture"
    );
    assert_eq!(
        fs::read(&title_with).unwrap(),
        fs::read(&root).unwrap(),
        "the root capture is the title capture"
    );
}

#[test]
#[ignore = "requires a converted movie pack and SDL's offscreen driver"]
fn ending_chain_captures_are_deterministic() {
    use std::process::Command;

    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let movie_path = real_movie_pack(&pack_path);
    let dir = TempDir::new("ending");
    let first = dir.0.join("first.bmp");
    let second = dir.0.join("second.bmp");
    let binary = env!("CARGO_BIN_EXE_arklay");
    // Ending 1's chain is 14, 15, 27, 22; 500 ticks cross the pre-ending film
    // (402 ticks) and present a frame of the ending film.
    for path in [&first, &second] {
        let status = Command::new(binary)
            .args([
                pack_path.to_str().unwrap(),
                "--movie",
                movie_path.to_str().unwrap(),
                "--ending",
                "1",
                "--ticks",
                "500",
                "--capture",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("failed to run the arklay binary");
        assert!(status.success(), "the ending capture failed");
    }
    assert_eq!(
        fs::read(&first).unwrap(),
        fs::read(&second).unwrap(),
        "--ending captures differ between runs"
    );
    let image = arklay::bmp::decode(&fs::read(&first).unwrap()).unwrap();
    assert_eq!((image.width, image.height), (320, 240));
    assert!(
        image
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0),
        "the ending capture is blank"
    );
    println!(
        "--ending 1 capture: {} bytes",
        fs::metadata(&first).unwrap().len()
    );
}

#[test]
#[ignore = "requires a real RE1 installation; writes ~660 MiB"]
fn conversion_writes_the_shipped_movie_pack() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let dir = TempDir::new("convert");
    let main_out = dir.0.join("re1.akpak");
    let movie_out = dir.0.join("re1.movie.akpak");
    convert_game_with_packs(
        &root,
        &main_out,
        None,
        0,
        &VoicePackOptions::Skip,
        &MoviePackOptions::Sibling(Some(movie_out.clone())),
    )
    .unwrap();

    let pack = Pack::open(&movie_out).unwrap();
    assert_eq!(pack.len(), 27, "one entry per shipped film");
    let bytes: u64 = pack.entries().map(|entry| entry.size() as u64).sum();
    assert_eq!(bytes, TOTAL_BYTES);
    for &(name, _, _) in FILMS {
        assert!(pack.contains(&movie::pack_path(name)), "missing {name}");
    }
    assert!(!pack.contains("movie/vlogo.avi"));
    let main = Pack::open(&main_out).unwrap();
    assert!(!main.paths().any(|path| path.starts_with("movie/")));
    println!(
        "movie pack: {} entries, {bytes} bytes; main pack {} entries",
        pack.len(),
        main.len()
    );
}
