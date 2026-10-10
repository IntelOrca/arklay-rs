//! Decode budgets and checked allocation shared by every parser.
//!
//! Every byte a parser reads is untrusted: a pack, a mod or a save file may be
//! authored by anyone, and M15 opened the pack format to third parties. Each
//! reader therefore bounds its header-derived counts and lengths against the
//! fixed caps below *before* it allocates, and every allocation whose length
//! comes from a header field goes through [`alloc`], which reserves with
//! `try_reserve_exact` and refuses to exceed [`MAX_DECODE_ALLOC`]. An over-cap
//! input returns an error naming the format, the cap and the observed value;
//! no parser panics, aborts or grows without bound on malformed data.
//!
//! The caps are policy, chosen per format from the shipped corpus with
//! generous headroom (the converter's pack holds 2,541 entries and 408 MiB):
//!
//! | cap | limit | corpus peak | formats |
//! |---|---|---|---|
//! | [`MAX_PACK_BYTES`] | 2 GiB | 408 MiB | `.akpak` image |
//! | [`MAX_PACK_ENTRIES`] | 1,048,576 | 2,541 | pack table of contents |
//! | [`MAX_ENTRY_BYTES`] | 1 GiB | ~40 MiB | one pack entry |
//! | [`MAX_DECODE_ALLOC`] | 512 MiB | ~64 MiB | one transient decode buffer |
//! | [`MAX_PIXELS`] | 16,777,216 | 320x240 | TIM/BMP/IVM/cinepak canvas |
//! | [`MAX_LZW_OUTPUT`] | 64 MiB | ~1 MiB | one LZW stream |
//! | [`MAX_RECORDS`] | 1,048,576 | 348 RDTs total | per-format record lists |
//! | [`MAX_TEXT_BYTES`] | 16 MiB | a few KiB | manifest, `.s` source |
//! | [`MAX_CLUT_ENTRIES`] | 65,536 | 1,536 | TIM CLUT |
//! | [`MAX_CLUT_ROWS`] | 256 | 3 | TIM 4bpp CLUT rows |
//! | [`MAX_TMD_OBJECTS`] | 65,536 | 16 (player EMD) | TMD/IVM objects |
//! | [`MAX_TMD_VERTICES`] | 1,048,576 | a few thousand | TMD/IVM vertex pools |
//! | [`MAX_TMD_PRIMS`] | 1,048,576 | ~2,000 | TMD/IVM primitive lists |
//! | [`MAX_EMD_JOINTS`] | 4,096 | 16 | EMD/EMW skeleton joints |
//! | [`MAX_EMD_KEYFRAMES`] | 65,536 | 123 | EMD/EMW keyframes |
//! | [`MAX_EMD_CLIPS`] | 4,096 | 35 | EMD/EMW animation clips |
//! | [`MAX_AVI_FRAMES`] | 1,048,576 | ~5,000 | AVI frame index |
//! | [`MAX_MASK_GROUPS`] | 32 | 20 | camera mask groups |
//! | [`MAX_MASK_SPRITES`] | 65,536 | 53 | camera mask sprites |
//! | [`MAX_WAV_DATA`] | 64 MiB | ~8 MiB | WAV `data` chunk |
//! | [`MAX_WAV_SAMPLES`] | 134,217,728 | ~3.6M per SE | converted mono samples |
//! | [`MAX_SCD_BLOCKS`] | 65,536 | a handful | SCD container blocks |
//! | [`MAX_SCD_EVENTS`] | 256 | 59 | SCD event table |
//! | [`MAX_SCD_INSTRUCTIONS`] | 1,048,576 | a few thousand | one SCD container/table |
//! | [`MAX_ASM_LINES`] | 1,048,576 | n/a (mod source) | `.s` source lines |
//! | [`MAX_ASM_OUTPUT`] | 64 MiB | n/a (mod source) | standalone `.scd` |
//! | [`MAX_MANIFEST_ITEMS`] | 4,096 | 11 | manifest statements |
//! | [`MAX_MANIFEST_LINE`] | 65,536 | < 100 | one manifest line |
//! | [`MAX_MANIFEST_LUA`] | 1,024 | a handful | manifest Lua list |
//! | [`MAX_LUA_CHUNKS`] | 256 | a handful | loaded Lua chunks |
//! | [`MAX_LUA_CHUNK`] | 1 MiB | a few KiB | one Lua chunk |
//! | [`MAX_SAVE_BYTES`] | 1 MiB | 2 KiB | save slot file |
//! | [`MAX_COMBAT_TABLE_BYTES`] | 64 KiB | ~7 KiB | `data/combat.bin` |
//!
//! Raising a cap is a policy change: measure the new corpus peak, update the
//! constant and this table, and rerun the budget tests.

use std::collections::HashMap;
use std::mem::size_of;

use anyhow::{Context, Result, anyhow, bail};

/// Largest pack image accepted from disk or memory.
pub const MAX_PACK_BYTES: u64 = 2 << 30;
/// Largest entry count a pack table of contents may declare.
pub const MAX_PACK_ENTRIES: usize = 1 << 20;
/// Largest single pack entry accepted.
pub const MAX_ENTRY_BYTES: u64 = 1 << 30;
/// Largest single transient decode allocation, in bytes.
pub const MAX_DECODE_ALLOC: usize = 512 << 20;
/// Largest decoded image, in pixels.
pub const MAX_PIXELS: usize = 4096 * 4096;
/// Largest decoded LZW stream, in bytes.
pub const MAX_LZW_OUTPUT: usize = 64 << 20;
/// Default per-format record-list cap.
pub const MAX_RECORDS: usize = 1 << 20;
/// Largest manifest or assembler source text, in bytes.
pub const MAX_TEXT_BYTES: usize = 16 << 20;
/// Largest TIM CLUT, in entries.
pub const MAX_CLUT_ENTRIES: usize = 1 << 16;
/// Largest TIM 4bpp CLUT row count.
pub const MAX_CLUT_ROWS: usize = 1 << 8;
/// Largest TMD/IVM object count.
pub const MAX_TMD_OBJECTS: usize = 1 << 16;
/// Largest TMD/IVM vertex or normal pool.
pub const MAX_TMD_VERTICES: usize = 1 << 20;
/// Largest TMD/IVM primitive list.
pub const MAX_TMD_PRIMS: usize = 1 << 20;
/// Largest EMD/EMW skeleton.
pub const MAX_EMD_JOINTS: usize = 1 << 12;
/// Largest EMD/EMW keyframe list.
pub const MAX_EMD_KEYFRAMES: usize = 1 << 16;
/// Largest EMD/EMW clip list.
pub const MAX_EMD_CLIPS: usize = 1 << 12;
/// Largest AVI frame index.
pub const MAX_AVI_FRAMES: usize = 1 << 20;
/// Largest camera mask group count (group bits live in a `u32`).
pub const MAX_MASK_GROUPS: usize = 32;
/// Largest camera mask sprite list.
pub const MAX_MASK_SPRITES: usize = 1 << 16;
/// Largest WAV `data` chunk.
pub const MAX_WAV_DATA: usize = 64 << 20;
/// Largest converted mono sample buffer.
pub const MAX_WAV_SAMPLES: usize = 1 << 27;
/// Largest SCD container block list.
pub const MAX_SCD_BLOCKS: usize = 1 << 16;
/// Largest SCD event table (event ids are one byte).
pub const MAX_SCD_EVENTS: usize = u8::MAX as usize + 1;
/// Largest instruction count one SCD container or event table may decode.
pub const MAX_SCD_INSTRUCTIONS: usize = 1 << 20;
/// Largest `.s` source line count.
pub const MAX_ASM_LINES: usize = 1 << 20;
/// Largest assembled standalone `.scd` container.
pub const MAX_ASM_OUTPUT: usize = 64 << 20;
/// Largest manifest statement count.
pub const MAX_MANIFEST_ITEMS: usize = 1 << 12;
/// Largest single manifest line.
pub const MAX_MANIFEST_LINE: usize = 1 << 16;
/// Largest manifest Lua hook list.
pub const MAX_MANIFEST_LUA: usize = 1 << 10;
/// Largest number of Lua chunks one pack may load.
pub const MAX_LUA_CHUNKS: usize = 256;
/// Largest single Lua chunk.
pub const MAX_LUA_CHUNK: usize = 1 << 20;
/// Largest save slot file accepted.
pub const MAX_SAVE_BYTES: usize = 1 << 20;
/// Largest combat table blob accepted (`data/combat.bin`).
pub const MAX_COMBAT_TABLE_BYTES: usize = 1 << 16;
/// Largest cinepak canvas, in pixels.
pub const MAX_CINEPAK_PIXELS: usize = MAX_PIXELS;

/// The caps must stay mutually ordered; a wrong edit fails the build.
const _: () = {
    assert!(MAX_ENTRY_BYTES <= MAX_PACK_BYTES);
    assert!(MAX_LZW_OUTPUT <= MAX_DECODE_ALLOC);
    assert!(MAX_PIXELS * 4 <= MAX_DECODE_ALLOC);
    assert!(MAX_CLUT_ENTRIES >= 256);
    assert!(MAX_MASK_SPRITES >= 53);
    assert!(MAX_EMD_CLIPS * 4 <= MAX_DECODE_ALLOC);
};

/// Check a length or count against its cap, naming both on failure.
pub fn check_len(len: usize, limit: usize, what: &str) -> Result<usize> {
    if len > limit {
        bail!("{what} is {len}, over the {limit} limit");
    }
    Ok(len)
}

/// Check a `u64` header length against its cap.
pub fn check_len_u64(len: u64, limit: u64, what: &str) -> Result<u64> {
    if len > limit {
        bail!("{what} is {len}, over the {limit} limit");
    }
    Ok(len)
}

/// Reserve `len` elements of `T` without ever aborting on failure.
///
/// The request is capped at [`MAX_DECODE_ALLOC`] bytes and reserved with
/// `try_reserve_exact`, so an adversarial header fails with an error instead of
/// an out-of-memory abort. The returned vector has capacity for `len` elements
/// and length zero, exactly like `Vec::with_capacity`.
pub fn alloc<T>(len: usize, what: &str) -> Result<Vec<T>> {
    let bytes = len
        .checked_mul(size_of::<T>())
        .with_context(|| format!("{what} of {len} elements overflows the address space"))?;
    if bytes > MAX_DECODE_ALLOC {
        bail!("{what} needs {bytes} bytes, over the {MAX_DECODE_ALLOC}-byte allocation limit");
    }
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| anyhow!("{what}: failed to reserve {bytes} bytes"))?;
    Ok(out)
}

/// Reserve capacity in a hash map without aborting on failure.
pub fn reserve_map<K: std::hash::Hash + Eq, V>(
    map: &mut HashMap<K, V>,
    len: usize,
    what: &str,
) -> Result<()> {
    map.try_reserve(len)
        .map_err(|_| anyhow!("{what}: failed to reserve space for {len} entries"))
}

/// Test helper: assert a parser rejected an adversarial input with a
/// cap-naming error, and return the rendered message.
#[cfg(test)]
pub(crate) fn assert_cap_error<T: std::fmt::Debug>(result: anyhow::Result<T>) -> String {
    let error = result.expect_err("adversarial input must be rejected");
    let message = format!("{error:#}");
    assert!(
        message.contains("limit"),
        "rejection must name the cap: {message}"
    );
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_len_passes_at_the_cap_and_rejects_above_it() {
        assert_eq!(check_len(10, 10, "ten").unwrap(), 10);
        let error = check_len(11, 10, "eleven").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("eleven"), "{message}");
        assert!(message.contains("11"), "{message}");
        assert!(message.contains("10"), "{message}");
    }

    #[test]
    fn check_len_u64_names_the_limit() {
        assert_eq!(
            check_len_u64(1 << 30, MAX_ENTRY_BYTES, "entry").unwrap(),
            1 << 30
        );
        let error = check_len_u64((1 << 30) + 1, MAX_ENTRY_BYTES, "entry length").unwrap_err();
        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[test]
    fn alloc_reserves_up_to_the_transient_cap() {
        let mut out: Vec<u16> = alloc(1024, "test buffer").unwrap();
        assert!(out.capacity() >= 1024);
        for value in 0..1024u16 {
            out.push(value);
        }
        assert_eq!(out.len(), 1024);

        let over = MAX_DECODE_ALLOC / size_of::<u16>() + 1;
        let error = alloc::<u16>(over, "over-cap buffer").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("allocation limit"), "{message}");

        let error = alloc::<u64>(usize::MAX, "overflowing buffer").unwrap_err();
        assert!(error.to_string().contains("overflows"), "{error}");
    }

    #[test]
    fn reserve_map_is_checked() {
        let mut map: HashMap<u32, u32> = HashMap::new();
        reserve_map(&mut map, 64, "test map").unwrap();
        assert!(map.capacity() >= 64);
    }
}
