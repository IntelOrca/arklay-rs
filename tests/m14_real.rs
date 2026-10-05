//! M14 real-asset tests: the AVI demuxer and the Cinepak decoder.
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=... ARKLAY_RE1_PACK=... \
//!  ARKLAY_MOVIE_GOLDEN=... cargo test --test m14_real -- --ignored --nocapture`
//!
//! The integer-pixel comparison needs `ARKLAY_MOVIE_GOLDEN`, a directory of
//! sampled BMPs and a `hashes_crc32.txt` produced by an independent decoder
//! (never committed); without it the tests still decode the whole corpus and
//! check determinism. Only an unset environment skips; a partial
//! configuration fails loudly.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use arklay::avi::Avi;
use arklay::cinepak::Decoder;

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

/// The independent-decoder directory, or `None` when not configured.
fn golden_dir() -> Option<PathBuf> {
    let dir = std::env::var("ARKLAY_MOVIE_GOLDEN").ok()?;
    let dir = PathBuf::from(dir);
    assert!(
        dir.is_dir(),
        "ARKLAY_MOVIE_GOLDEN is set but {} is not a directory",
        dir.display()
    );
    Some(dir)
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
    let mut decoder = Decoder::new(320, 240);
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
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let Some(golden) = golden_dir() else {
        return;
    };
    let hashes_path = golden.join("hashes_crc32.txt");
    let golden_hashes = fs::read_to_string(&hashes_path)
        .unwrap_or_else(|error| panic!("failed to read {hashes_path:?}: {error}"));
    let expected: std::collections::HashSet<&str> = golden_hashes.lines().collect();
    let mut matched = 0usize;
    for &(name, frames, _) in FILMS {
        let avi = parse_film(&root, name);
        let picks = [0usize, frames / 2, frames - 1];
        let mut decoder = Decoder::new(320, 240);
        let mut rgba = vec![0u8; 320 * 240 * 4];
        for index in 0..frames {
            let frame = avi.video(index).unwrap();
            decoder
                .decode(frame)
                .unwrap_or_else(|error| panic!("{name} frame {index} failed: {error}"));
            let line = format!("{name} {index} {:08x}", crc32(decoder.rgb()));
            assert!(
                expected.contains(line.as_str()),
                "{name} frame {index} does not match the independent decoder ({line})"
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
