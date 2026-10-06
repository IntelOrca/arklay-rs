//! Stable parser torture matrix.
//!
//! Builds a minimal valid synthetic seed for every parser the engine reads
//! (LZW, TIM in all three modes, TMD, IVM, EMD/EMW, RDT, SCD reader and
//! assembler, pack, manifest, save, indexed/direct BMP, mask table, WAV and a
//! minimal 320x240 `cvid` AVI), then applies a deterministic mutation matrix:
//! truncation at sampled lengths, single-byte flips at sampled offsets, zero
//! runs, LCG random overwrites, 512 fully random inputs and text-boundary
//! cases. Every case runs the same public entry point the engine uses inside
//! `catch_unwind`; a panic fails the test naming the format, the case index
//! and the input as hex.
//!
//! The matrix is always-run, dependency-free beyond the engine itself and
//! enforces a floor of 10,000 cases. The seed builders double as the committed
//! `cargo-fuzz` corpus: `cargo test --test torture -- --ignored
//! write_fuzz_seeds` regenerates `fuzz/seeds/<target>/seed`, and a local fuzz
//! run reads that file while writing new units to the gitignored
//! `fuzz/corpus/<target>` scratch directory. Every seed is synthetic; no game
//! bytes are read or written.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use arklay::avi::Avi;
use arklay::budget;
use arklay::cinepak::Decoder;
use arklay::manifest::{self, Manifest};
use arklay::pack::{Pack, PackWriter};
use arklay::save::SaveFile;
use arklay::state::{Image, RoomId};
use arklay::{audio, bmp, emd, ivm, lzw, mask, rdt, scd, tim, tmd};

/// One parser target: a synthetic seed and the engine entry point it feeds.
struct Target {
    name: &'static str,
    seed: fn() -> Vec<u8>,
    run: fn(&[u8]),
}

/// Fully random cases per target.
const RANDOM_CASES: usize = 512;
/// Random-overwrite cases per target.
const LCG_CASES: usize = 64;

// --- runners: the same entry points the engine uses -----------------------

fn run_pack(data: &[u8]) {
    let _ = Pack::from_bytes(data.to_vec());
}

fn run_manifest(data: &[u8]) {
    let _ = Manifest::parse(&String::from_utf8_lossy(data));
}

fn run_rdt(data: &[u8]) {
    let _ = rdt::parse(data, RoomId::parse("1000").unwrap());
}

fn run_scd(data: &[u8]) {
    let _ = scd::reader::parse(data);
}

fn run_scd_asm(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    if let Ok(assembled) = scd::asm::assemble(&text) {
        let _ = assembled.to_container();
    }
}

fn run_emd(data: &[u8]) {
    let _ = emd::parse(data);
    let _ = emd::parse_emw(data);
}

fn run_tim(data: &[u8]) {
    let _ = tim::decode(data);
    let _ = tim::decode_8bpp(data);
    let _ = tim::decode_4bpp(data);
}

fn run_tmd(data: &[u8]) {
    let _ = tmd::parse(data);
}

fn run_ivm(data: &[u8]) {
    let _ = ivm::parse(data);
}

fn run_avi(data: &[u8]) {
    let _ = Avi::parse(data.to_vec());
}

fn run_cinepak(data: &[u8]) {
    if let Ok(mut decoder) = Decoder::new(320, 240) {
        let _ = decoder.decode(data);
        let _ = decoder.decode(data);
    }
}

fn run_lzw(data: &[u8]) {
    let _ = lzw::decode(data);
}

fn run_bmp(data: &[u8]) {
    let _ = bmp::decode(data);
    let _ = bmp::decode_mask(data);
}

fn run_save(data: &[u8]) {
    let _ = SaveFile::from_bytes(data);
}

fn run_mask(data: &[u8]) {
    let _ = mask::MaskTable::parse(data, 4);
}

fn run_wav(data: &[u8]) {
    let _ = audio::parse_wav(data);
}

// --- synthetic seeds -------------------------------------------------------

/// A tiny valid LZW stream decoding two bytes.
fn seed_lzw() -> Vec<u8> {
    vec![0x08, 0x00, 0x20, 0x00]
}

/// A 2x2 16bpp TIM.
fn seed_tim16() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0x10u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&20u32.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    for pixel in [0x7FFFu16, 0, 0xF800, 0x001F] {
        out.extend_from_slice(&pixel.to_le_bytes());
    }
    out
}

/// A 2x2 8bpp TIM with a 256-entry CLUT row.
fn seed_tim8() -> Vec<u8> {
    let mut out = Vec::new();
    let palette: Vec<u16> = (0..256u16)
        .map(|index| index.wrapping_mul(0x0841))
        .collect();
    let pixels = [0u8, 1, 2, 3];
    out.extend_from_slice(&0x10u32.to_le_bytes());
    out.extend_from_slice(&9u32.to_le_bytes());
    out.extend_from_slice(&(12u32 + palette.len() as u32 * 2).to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&480i16.to_le_bytes());
    out.extend_from_slice(&256u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    for entry in &palette {
        out.extend_from_slice(&entry.to_le_bytes());
    }
    out.extend_from_slice(&(12u32 + pixels.len() as u32).to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&pixels);
    out
}

/// A 2x2 4bpp TIM with a 16-entry CLUT.
fn seed_tim4() -> Vec<u8> {
    let mut out = Vec::new();
    let palette = [0x001Fu16, 0x03E0, 0x7C00, 0x7FFF];
    let pixels = [0x10u8, 0x32];
    out.extend_from_slice(&0x10u32.to_le_bytes());
    out.extend_from_slice(&9u32.to_le_bytes());
    out.extend_from_slice(&(12u32 + 16 * 2).to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&480i16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    for index in 0..16u16 {
        let entry = palette[usize::from(index) % palette.len()];
        out.extend_from_slice(&entry.to_le_bytes());
    }
    out.extend_from_slice(&(12u32 + pixels.len() as u32).to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&pixels);
    out
}

/// A one-object TMD with a single gouraud textured triangle.
fn tmd_bytes() -> Vec<u8> {
    let command = 0x3400_0609u32;
    let vertices = [[1i16, 2, 3], [4, 5, 6], [7, 8, 9]];
    let normals = [[0i16, 0, 4096], [0, 4096, 0], [4096, 0, 0]];
    let prim = [
        command,
        (0x7800u32 << 16) | (10 << 8) | 1,
        (0x80u32 << 16) | (11 << 8) | 2,
        (12 << 8) | 3,
        0,
        (1 << 16) | 1,
        (2 << 16) | 2,
    ];

    let mut data = Vec::new();
    data.extend_from_slice(&0x41u32.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&1u32.to_le_bytes());
    data.extend_from_slice(&[0u8; 28]);

    let vertex_offset = data.len() - 12;
    for vertex in vertices {
        for component in vertex {
            data.extend_from_slice(&component.to_le_bytes());
        }
        data.extend_from_slice(&0i16.to_le_bytes());
    }
    let normal_offset = data.len() - 12;
    for normal in normals {
        for component in normal {
            data.extend_from_slice(&component.to_le_bytes());
        }
        data.extend_from_slice(&0i16.to_le_bytes());
    }
    let prim_offset = data.len() - 12;
    for word in prim {
        data.extend_from_slice(&word.to_le_bytes());
    }

    let descriptor = [
        vertex_offset as i32,
        3,
        normal_offset as i32,
        3,
        prim_offset as i32,
        1,
        0,
    ];
    for (slot, value) in descriptor.iter().enumerate() {
        let at = 12 + slot * 4;
        data[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    data
}

fn seed_tmd() -> Vec<u8> {
    tmd_bytes()
}

fn seed_ivm() -> Vec<u8> {
    let mut data = seed_tim8();
    data.extend_from_slice(&tmd_bytes());
    data
}

fn minimal_emr() -> Vec<u8> {
    let mut data = vec![0u8; 0x60];
    data[0..2].copy_from_slice(&0x30u16.to_le_bytes());
    data[2..4].copy_from_slice(&0x40u16.to_le_bytes());
    data[4..6].copy_from_slice(&2u16.to_le_bytes());
    data[6..8].copy_from_slice(&24u16.to_le_bytes());
    for (slot, joint) in [[1i16, 2, 3], [4, 5, 6]].iter().enumerate() {
        let at = 8 + slot * 6;
        for (axis, value) in joint.iter().enumerate() {
            data[at + axis * 2..at + axis * 2 + 2].copy_from_slice(&value.to_le_bytes());
        }
    }
    data[0x30..0x32].copy_from_slice(&1i16.to_le_bytes());
    data[0x32..0x34].copy_from_slice(&8i16.to_le_bytes());
    data[0x38] = 1;
    for (slot, value) in [7i16, 8, 9, 0, 0, 0, 10, 11, 12, 13, 14, 15]
        .iter()
        .enumerate()
    {
        let at = 0x40 + slot * 2;
        data[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    data
}

fn minimal_edd() -> Vec<u8> {
    let mut data = vec![0u8; 0x20];
    data[0..2].copy_from_slice(&1u16.to_le_bytes());
    data[2..4].copy_from_slice(&4u16.to_le_bytes());
    data[4..6].copy_from_slice(&0u16.to_le_bytes());
    data[6..8].copy_from_slice(&7u16.to_le_bytes());
    data
}

fn minimal_tim_texture() -> Vec<u8> {
    let mut data = Vec::new();
    let palette = [0x001Fu16, 0x03E0];
    data.extend_from_slice(&0x10u32.to_le_bytes());
    data.extend_from_slice(&0x09u32.to_le_bytes());
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&0i16.to_le_bytes());
    data.extend_from_slice(&480i16.to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    for entry in palette {
        data.extend_from_slice(&entry.to_le_bytes());
    }
    data.extend_from_slice(&14u32.to_le_bytes());
    data.extend_from_slice(&0i16.to_le_bytes());
    data.extend_from_slice(&0i16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&[0u8, 1]);
    data
}

fn seed_emd() -> Vec<u8> {
    let mut data = minimal_emr();
    let edd_offset = data.len();
    data.extend_from_slice(&minimal_edd());
    let tmd_offset = data.len();
    data.extend_from_slice(&tmd_bytes());
    let tim_offset = data.len();
    data.extend_from_slice(&minimal_tim_texture());
    for value in [
        0u32,
        0,
        edd_offset as u32,
        tmd_offset as u32,
        tim_offset as u32,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    data
}

fn seed_emw() -> Vec<u8> {
    let mut data = minimal_emr();
    let edd_offset = data.len();
    data.extend_from_slice(&minimal_edd());
    let mesh_offset = data.len();
    data.extend_from_slice(&tmd_bytes());
    for value in [edd_offset as u32, mesh_offset as u32] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    data
}

/// An RDT header with no cameras and a collision and message section.
fn seed_rdt() -> Vec<u8> {
    let mut data = vec![0u8; 0x94];
    let collision = data.len();
    data[0x48 + 4..0x48 + 8].copy_from_slice(&(collision as u32).to_le_bytes());
    data.extend_from_slice(&10i16.to_le_bytes());
    data.extend_from_slice(&20i16.to_le_bytes());
    for count in [1i32, 0, 0, 0, 0] {
        data.extend_from_slice(&count.to_le_bytes());
    }
    for word in [100u16, 110, 10, 20, 1, 0x300] {
        data.extend_from_slice(&word.to_le_bytes());
    }
    let messages = data.len();
    data[0x48 + 11 * 4..0x48 + 12 * 4].copy_from_slice(&(messages as u32).to_le_bytes());
    let message: [u8; 8] = [0x04, 0x00, 0x01, 0x00, 0x04, 0x01, 0x02, 0x00];
    data.extend_from_slice(&message);
    data
}

/// An RDT with init, main and one event script.
fn seed_scd() -> Vec<u8> {
    let mut data = vec![0u8; 0x94];
    let init = data.len();
    let body: [u8; 2] = [0x0E, 0x00];
    data.extend_from_slice(&((body.len() + 2) as u16).to_le_bytes());
    data.extend_from_slice(&body);
    data.extend_from_slice(&0u16.to_le_bytes());
    data[0x48 + 6 * 4..0x48 + 7 * 4].copy_from_slice(&(init as u32).to_le_bytes());
    let main = data.len();
    data.extend_from_slice(&4u16.to_le_bytes());
    data.extend_from_slice(&body);
    data.extend_from_slice(&0u16.to_le_bytes());
    data[0x48 + 7 * 4..0x48 + 8 * 4].copy_from_slice(&(main as u32).to_le_bytes());
    let events = data.len();
    let event = [
        0x01u8, 0x84, 0x30, 0x3F, 0x00, 0x80, 0xFF, 0x00, 0x00, 0x00, 0x00,
    ];
    data.extend_from_slice(&8u32.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&event);
    data[0x48 + 8 * 4..0x48 + 9 * 4].copy_from_slice(&(events as u32).to_le_bytes());
    data
}

/// The assembler demo source: one block per section and one event.
fn seed_scd_asm() -> Vec<u8> {
    b"\n.version 1\n\n.init\n.block\n    nop                     0\n\n.main\n.block\n    nop                     0\n\n.event event_00\n    evt_finish\n"
        .to_vec()
}

fn seed_pack() -> Vec<u8> {
    let mut writer = PackWriter::new();
    writer
        .add(
            manifest::ENTRY,
            b"format = 1\nid = \"re1\"\nkind = \"base\"\n".to_vec(),
        )
        .unwrap();
    writer.add("room/1000.rdt", seed_rdt()).unwrap();
    writer
        .add("text/readme.txt", b"hello pack".to_vec())
        .unwrap();
    writer.to_bytes().unwrap()
}

fn seed_manifest() -> Vec<u8> {
    b"format = 1\nid = \"re1\"\nname = \"Arklay\"\nkind = \"base\"\nlua = [\"lua/a.lua\"]\n"
        .to_vec()
}

fn seed_save() -> Vec<u8> {
    SaveFile::default().to_bytes().to_vec()
}

fn seed_bmp() -> Vec<u8> {
    let width = 4u32;
    let height = 4u32;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for index in 0..(width * height) {
        rgba.extend_from_slice(&[(index * 17) as u8, (index * 31) as u8, 40, 255]);
    }
    bmp::encode_to_vec(&Image {
        width,
        height,
        rgba,
    })
    .unwrap()
}

fn seed_mask() -> Vec<u8> {
    let mut data = vec![0u8; 4];
    data.extend_from_slice(&1i32.to_le_bytes());
    for word in [1u16, 0x1234, 10, 20] {
        data.extend_from_slice(&word.to_le_bytes());
    }
    for word in [0x0201u16, 0x0403, 100, 0x0800, 8, 16] {
        data.extend_from_slice(&word.to_le_bytes());
    }
    data
}

fn seed_wav() -> Vec<u8> {
    let data = [0u8, 64, 128, 192, 255, 128, 64, 0];
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&22050u32.to_le_bytes());
    fmt.extend_from_slice(&22050u32.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&8u16.to_le_bytes());
    let mut body = Vec::new();
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&fmt);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&data);
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&body);
    out
}

/// One RIFF chunk: fourcc, little-endian size, body and the pad byte.
fn avi_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// One LIST chunk whose body starts with its list type.
fn avi_list(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut inner = typ.to_vec();
    inner.extend_from_slice(body);
    avi_chunk(b"LIST", &inner)
}

fn avi_video_strh() -> Vec<u8> {
    let mut body = vec![0u8; 56];
    body[0..4].copy_from_slice(b"vids");
    body[20..24].copy_from_slice(&100_000u32.to_le_bytes());
    body[24..28].copy_from_slice(&1_000_000u32.to_le_bytes());
    avi_chunk(b"strh", &body)
}

fn avi_audio_strh() -> Vec<u8> {
    let mut body = vec![0u8; 56];
    body[0..4].copy_from_slice(b"auds");
    body[20..24].copy_from_slice(&4u32.to_le_bytes());
    body[24..28].copy_from_slice(&88_200u32.to_le_bytes());
    avi_chunk(b"strh", &body)
}

fn avi_video_strf() -> Vec<u8> {
    let mut body = vec![0u8; 40];
    body[0..4].copy_from_slice(&40u32.to_le_bytes());
    body[4..8].copy_from_slice(&320i32.to_le_bytes());
    body[8..12].copy_from_slice(&240i32.to_le_bytes());
    body[12..14].copy_from_slice(&1u16.to_le_bytes());
    body[14..16].copy_from_slice(&24u16.to_le_bytes());
    body[16..20].copy_from_slice(b"cvid");
    avi_chunk(b"strf", &body)
}

fn avi_audio_strf() -> Vec<u8> {
    let mut body = vec![0u8; 16];
    body[0..2].copy_from_slice(&1u16.to_le_bytes());
    body[2..4].copy_from_slice(&2u16.to_le_bytes());
    body[4..8].copy_from_slice(&22_050u32.to_le_bytes());
    body[8..12].copy_from_slice(&88_200u32.to_le_bytes());
    body[12..14].copy_from_slice(&4u16.to_le_bytes());
    body[14..16].copy_from_slice(&16u16.to_le_bytes());
    avi_chunk(b"strf", &body)
}

/// One 320x240 intra Cinepak frame: a full-height strip with a one-entry V1
/// codebook and V1 vectors for every 4x4 block.
fn seed_cinepak() -> Vec<u8> {
    let mut chunks = Vec::new();
    chunks.push(0x26u8);
    chunks.extend_from_slice(&[0, 0, 10]);
    chunks.extend_from_slice(&[90, 90, 90, 90, 0, 0]);
    chunks.push(0x32);
    let vector_bytes = (320usize / 4) * (240usize / 4);
    let chunk_size = 4 + vector_bytes;
    chunks.extend_from_slice(&[
        (chunk_size >> 16) as u8,
        (chunk_size >> 8) as u8,
        chunk_size as u8,
    ]);
    chunks.resize(chunks.len() + vector_bytes, 0);

    let strip_size = 12 + chunks.len();
    let mut strip = vec![0x10u8];
    strip.extend_from_slice(&[
        (strip_size >> 16) as u8,
        (strip_size >> 8) as u8,
        strip_size as u8,
    ]);
    strip.extend_from_slice(&0u16.to_be_bytes()); // top
    strip.extend_from_slice(&0u16.to_be_bytes()); // left
    strip.extend_from_slice(&240u16.to_be_bytes()); // bottom
    strip.extend_from_slice(&320u16.to_be_bytes()); // right
    strip.extend_from_slice(&chunks);

    let frame_size = 10 + strip.len();
    let mut out = vec![0u8];
    out.extend_from_slice(&[
        (frame_size >> 16) as u8,
        (frame_size >> 8) as u8,
        frame_size as u8,
    ]);
    out.extend_from_slice(&320u16.to_be_bytes());
    out.extend_from_slice(&240u16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&strip);
    out
}

fn seed_avi() -> Vec<u8> {
    let mut avih = vec![0u8; 56];
    avih[0..4].copy_from_slice(&100_000u32.to_le_bytes());
    avih[16..20].copy_from_slice(&1u32.to_le_bytes());
    avih[32..36].copy_from_slice(&320i32.to_le_bytes());
    avih[36..40].copy_from_slice(&240i32.to_le_bytes());
    let mut hdrl = avi_chunk(b"avih", &avih);
    hdrl.extend_from_slice(&avi_list(
        b"strl",
        &[avi_video_strh(), avi_video_strf()].concat(),
    ));
    hdrl.extend_from_slice(&avi_list(
        b"strl",
        &[avi_audio_strh(), avi_audio_strf()].concat(),
    ));
    let mut top = avi_list(b"hdrl", &hdrl);
    top.extend_from_slice(&avi_list(b"movi", &avi_chunk(b"00dc", &[0x10])));
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(top.len() as u32 + 4).to_le_bytes());
    out.extend_from_slice(b"AVI ");
    out.extend_from_slice(&top);
    out
}

/// Every target, in a stable order.
fn targets() -> Vec<Target> {
    vec![
        Target {
            name: "pack",
            seed: seed_pack,
            run: run_pack,
        },
        Target {
            name: "manifest",
            seed: seed_manifest,
            run: run_manifest,
        },
        Target {
            name: "rdt",
            seed: seed_rdt,
            run: run_rdt,
        },
        Target {
            name: "scd",
            seed: seed_scd,
            run: run_scd,
        },
        Target {
            name: "scd_asm",
            seed: seed_scd_asm,
            run: run_scd_asm,
        },
        Target {
            name: "emd",
            seed: seed_emd,
            run: run_emd,
        },
        Target {
            name: "tim",
            seed: seed_tim16,
            run: run_tim,
        },
        Target {
            name: "tmd",
            seed: seed_tmd,
            run: run_tmd,
        },
        Target {
            name: "ivm",
            seed: seed_ivm,
            run: run_ivm,
        },
        Target {
            name: "avi",
            seed: seed_avi,
            run: run_avi,
        },
        Target {
            name: "cinepak",
            seed: seed_cinepak,
            run: run_cinepak,
        },
        Target {
            name: "lzw",
            seed: seed_lzw,
            run: run_lzw,
        },
        Target {
            name: "bmp",
            seed: seed_bmp,
            run: run_bmp,
        },
        Target {
            name: "save",
            seed: seed_save,
            run: run_save,
        },
        Target {
            name: "mask",
            seed: seed_mask,
            run: run_mask,
        },
        Target {
            name: "wav",
            seed: seed_wav,
            run: run_wav,
        },
    ]
}

// --- deterministic mutation matrix -----------------------------------------

/// A tiny deterministic LCG; the matrix must not depend on the host RNG.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        // Avoid the zero state and vary per target by seeding with its name.
        Self(seed | 1)
    }

    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            self.next() as usize % bound
        }
    }
}

/// A per-target LCG seeded by the target name and seed length.
fn lcg_for(name: &str, len: usize) -> Lcg {
    let mut state = 0xC0FF_EE00_1234_5678u64 ^ len as u64;
    for byte in name.bytes() {
        state = state.wrapping_mul(31).wrapping_add(u64::from(byte));
    }
    Lcg::new(state)
}

/// Build the deterministic mutation matrix for one seed.
fn cases(name: &str, seed: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut lcg = lcg_for(name, seed.len());

    // Truncation at sampled and random lengths.
    let mut lengths = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, 16, 24, 32];
    for divisor in [8, 4, 3, 2] {
        lengths.push(seed.len() / divisor);
    }
    lengths.push(seed.len().saturating_sub(2));
    lengths.push(seed.len().saturating_sub(1));
    lengths.push(seed.len());
    for _ in 0..8 {
        lengths.push(lcg.below(seed.len() + 1));
    }
    lengths.retain(|&length| length <= seed.len());
    lengths.sort_unstable();
    lengths.dedup();
    for length in lengths {
        out.push(seed[..length].to_vec());
    }

    // Single-byte flips at sampled and random offsets.
    let mut offsets = vec![
        0,
        seed.len().saturating_sub(1),
        seed.len() / 2,
        seed.len() / 3,
        seed.len() / 4,
    ];
    for _ in 0..27 {
        offsets.push(lcg.below(seed.len().max(1)));
    }
    offsets.retain(|&offset| offset < seed.len());
    offsets.sort_unstable();
    offsets.dedup();
    for &offset in &offsets {
        let original = seed[offset];
        for value in [0x00u8, 0xFF, original ^ 0x01, original.wrapping_add(0x80)] {
            let mut case = seed.to_vec();
            case[offset] = value;
            out.push(case);
        }
    }

    // Zero runs.
    for _ in 0..24 {
        if seed.is_empty() {
            break;
        }
        let start = lcg.below(seed.len());
        let length = 1 + lcg.below(16);
        let mut case = seed.to_vec();
        let end = (start + length).min(case.len());
        for byte in &mut case[start..end] {
            *byte = 0;
        }
        out.push(case);
    }

    // Random overwrites.
    for _ in 0..LCG_CASES {
        if seed.is_empty() {
            break;
        }
        let mut case = seed.to_vec();
        let edits = 1 + lcg.below(8);
        for _ in 0..edits {
            let at = lcg.below(case.len());
            case[at] = lcg.next() as u8;
        }
        out.push(case);
    }

    // Fully random inputs.
    for _ in 0..RANDOM_CASES {
        let length = lcg.below(seed.len() + 1);
        let mut case = vec![0u8; length];
        for byte in &mut case {
            *byte = lcg.next() as u8;
        }
        out.push(case);
    }

    // Text-boundary cases for the text parsers.
    if matches!(name, "manifest" | "scd_asm" | "pack") {
        out.push(vec![0xFF; 64]);
        out.push(vec![b'a'; budget::MAX_MANIFEST_LINE + 1]);
        out.push(b"key = \"unterminated".to_vec());
        out.push(b"\xff\xfe\xfd\xfc".to_vec());
        out.push(Vec::new());
        out.push(vec![0u8; 4096]);
    }
    out
}

/// Additional seeds for targets whose parser reads more than one layout.
fn extra_seeds(name: &str) -> Vec<(&'static str, Vec<u8>)> {
    match name {
        "tim" => vec![("tim8", seed_tim8()), ("tim4", seed_tim4())],
        "emd" => vec![("emw", seed_emw())],
        _ => Vec::new(),
    }
}

/// Render an input as hex for a panic report.
fn hex(bytes: &[u8]) -> String {
    let shown = bytes.len().min(256);
    let mut out = String::with_capacity(shown * 2 + 16);
    for byte in &bytes[..shown] {
        out.push_str(&format!("{byte:02X}"));
    }
    if shown < bytes.len() {
        out.push_str(&format!("... ({} bytes)", bytes.len()));
    }
    out
}

#[test]
fn torture_matrix_never_panics() {
    let mut total = 0usize;
    let mut per_format: BTreeMap<&str, usize> = BTreeMap::new();

    for target in targets() {
        let seed = (target.seed)();
        let mut matrix = cases(target.name, &seed);
        for (label, extra) in extra_seeds(target.name) {
            matrix.extend(cases(label, &extra));
        }
        for (index, case) in matrix.iter().enumerate() {
            let outcome = catch_unwind(AssertUnwindSafe(|| (target.run)(case)));
            if outcome.is_err() {
                panic!(
                    "parser panic: format {} case {} input {}",
                    target.name,
                    index,
                    hex(case)
                );
            }
        }
        per_format.insert(target.name, matrix.len());
        total += matrix.len();
    }

    for (name, count) in &per_format {
        println!("torture {name}: {count} cases");
    }
    println!("torture total: {total} cases");
    assert!(
        total >= 10_000,
        "torture matrix ran {total} cases, below the 10,000 floor"
    );
    assert_eq!(per_format.len(), 16, "every target must run");
}

/// Regenerate the committed cargo-fuzz seed corpus from these builders.
///
/// The `lua` target's seed is minimal sandbox-safe Lua source rather than a
/// mutated engine seed; the target wraps it in a v1 pack itself.
#[test]
#[ignore = "regenerates the committed fuzz seed corpus"]
fn write_fuzz_seeds() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/seeds");
    for target in targets() {
        let dir = root.join(target.name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("seed"), (target.seed)()).unwrap();
    }
    let lua = root.join("lua");
    std::fs::create_dir_all(&lua).unwrap();
    std::fs::write(
        lua.join("seed"),
        b"function on_room_load(api) end\nfunction on_tick(api, tick) end\n",
    )
    .unwrap();
    println!("wrote synthetic fuzz seeds under {}", root.display());
}
