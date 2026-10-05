//! Synthetic effect fixtures shared by the unit tests.
//!
//! Only compiled for tests. The builders construct the parsed types directly,
//! so the pool and page-packing tests stay independent of the RDT byte layout
//! (which has its own fixtures in `room`).

use crate::effects::pool::EffectBlock;
use crate::effects::room::{
    AnimFrame, EffectSprite, FrameEntry, SpriteAnimation, SpriteInfo, TimGeometry, UvRecord,
};

/// One animation frame from its raw 24-byte blocks.
pub fn frame(blocks: &[[u8; 24]]) -> AnimFrame {
    AnimFrame {
        blocks: blocks.iter().copied().map(EffectBlock).collect(),
    }
}

/// A 24-byte block with the given anim/update ids and yaw.
pub fn block(anim_id: u8, update_id: u8, yaw: i16) -> [u8; 24] {
    let mut bytes = [0u8; 24];
    bytes[0] = anim_id;
    bytes[1] = update_id;
    bytes[18..20].copy_from_slice(&yaw.to_le_bytes());
    bytes
}

/// An `EffectSprite` with a valid frame table and raw block depth rows.
pub fn sprite(index: u8, rows: [Vec<Vec<[u8; 24]>>; 8]) -> EffectSprite {
    let depth_frames = rows.map(|frames| {
        frames
            .into_iter()
            .map(|blocks| frame(&blocks))
            .collect::<Vec<_>>()
    });
    sprite_from_rows(index, depth_frames)
}

/// An `EffectSprite` from already-built animation frames.
pub fn sprite_from_rows(index: u8, depth_frames: [Vec<AnimFrame>; 8]) -> EffectSprite {
    EffectSprite {
        index,
        info: SpriteInfo {
            uv_count: 1,
            frames: vec![FrameEntry {
                uv_index: 0,
                delay: 4,
            }],
            uvs: vec![UvRecord {
                u: 16,
                v: 32,
                pivot_x: 64,
                pivot_y: 64,
            }],
            page_clut_word: 0x7840,
            page_id: 0,
            page_v: 0,
        },
        anim: SpriteAnimation {
            depth_table: [2, 3, 4, 5, 6, 7, 8, 9],
            depth_frames,
        },
        tim: None,
        geometry: TimGeometry::default(),
    }
}

/// Raw bytes of a 4bpp TIM image `width` x `height` with `rows` CLUT rows.
pub fn tim_bytes(width: u16, height: u16, rows: u16) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&0x10u32.to_le_bytes());
    data.extend_from_slice(&8u32.to_le_bytes());
    data.extend_from_slice(&(12 + 16 * 2 * u32::from(rows)).to_le_bytes());
    data.extend_from_slice(&0i16.to_le_bytes());
    data.extend_from_slice(&0i16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(&rows.to_le_bytes());
    for entry in 0..usize::from(rows) * 16 {
        data.extend_from_slice(&((entry as u16) & 0x7FFF).to_le_bytes());
    }
    let pixel_bytes = usize::from(width) / 4 * usize::from(height) * 2;
    data.extend_from_slice(&(12 + pixel_bytes as u32).to_le_bytes());
    data.extend_from_slice(&0i16.to_le_bytes());
    data.extend_from_slice(&0i16.to_le_bytes());
    data.extend_from_slice(&(width / 4).to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data.extend(std::iter::repeat_n(0x11u8, pixel_bytes));
    data
}
