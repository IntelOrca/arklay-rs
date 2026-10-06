//! Room mask (foreground overlay) sprites and their depth ordering.
//!
//! A camera can carry a table of opaque sprites drawn over the background:
//! pillars, doorframes, staircases and other foreground pieces that the player
//! model must pass behind. The table itself is parsed by [`MaskTable`]; the
//! per-(room, camera) bias records and the room-specific overrides that turn a
//! sprite's `pos_data` into a far-to-near draw key live here as well.

use anyhow::{Context, Result, bail};

use crate::budget;
use crate::state::RoomId;

/// Highest group count a camera table may declare. Group ids are one-based and
/// [`Cut`](crate::state::Cut) tracks group visibility in a `u32`, so a camera
/// can have at most 32 groups. The shipped data peaks at 20.
const MAX_GROUPS: usize = budget::MAX_MASK_GROUPS;

/// Number of entries in the room table (room within stage, times stage).
const ROOM_TABLE_LEN: usize = 160;

/// One sprite in a camera's mask table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MaskSprite {
    /// Texture coordinates into the camera's mask page: `(u, v)`.
    pub uv: (u8, u8),
    /// Final screen position: the group origin plus the record's screen delta.
    pub pos: (i32, i32),
    /// Pixel size; both axes are equal on the packed size form.
    pub size: (u16, u16),
    /// Depth/brightness data word.
    pub pos_data: u16,
    /// Raw flags: texture page in the low bits, mirror/size bits above.
    pub flags: u16,
    /// One-based group id, matching the toggle operand used by scripts.
    pub group: u8,
}

/// One group header of a camera's mask table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MaskGroup {
    /// Number of sprites that follow the group headers for this group.
    pub sprite_count: u16,
    /// Texture page/CLUT bits shared by the group.
    pub tex_bits: u16,
    /// Group origin added to every sprite's screen delta.
    pub origin: (i16, i16),
}

/// The mask table of one camera: its group headers and flattened sprites in
/// file order (group by group).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MaskTable {
    pub groups: Vec<MaskGroup>,
    pub sprites: Vec<MaskSprite>,
}

impl MaskTable {
    /// Parse the table at direct file offset `pointer`.
    ///
    /// A zero pointer means the camera has no mask table and yields an empty
    /// table. A non-zero pointer must lie inside `data`; malformed or truncated
    /// tables are reported as errors, never panics.
    pub fn parse(data: &[u8], pointer: u32) -> Result<Self> {
        if pointer == 0 {
            return Ok(Self::default());
        }

        let base = usize::try_from(pointer).context("mask table pointer does not fit usize")?;
        let group_count = read_i32(data, base).with_context(|| {
            format!(
                "mask table pointer 0x{pointer:x} is out of bounds for the {}-byte RDT",
                data.len()
            )
        })?;
        if group_count < 0 {
            bail!("mask table at 0x{pointer:x} has negative group count {group_count}");
        }
        let group_count = group_count as usize;
        if group_count == 0 {
            return Ok(Self::default());
        }
        if group_count > MAX_GROUPS {
            bail!(
                "mask table at 0x{pointer:x} has {group_count} groups, more than the supported \
                 {MAX_GROUPS}"
            );
        }

        let mut groups = Vec::with_capacity(group_count);
        let mut offset = base + 4;
        for _ in 0..group_count {
            let sprite_count = read_u16(data, offset)?;
            let tex_bits = read_u16(data, offset + 2)?;
            let origin_x = read_u16(data, offset + 4)? as i16;
            let origin_y = read_u16(data, offset + 6)? as i16;
            groups.push(MaskGroup {
                sprite_count,
                tex_bits,
                origin: (origin_x, origin_y),
            });
            offset += 8;
        }

        let mut sprites = Vec::new();
        for (index, group) in groups.iter().enumerate() {
            let group_id = (index + 1) as u8;
            for _ in 0..group.sprite_count {
                budget::check_len(
                    sprites.len().saturating_add(1),
                    budget::MAX_MASK_SPRITES,
                    "mask sprite count",
                )?;
                let uv = read_u16(data, offset)?;
                let delta = read_u16(data, offset + 2)?;
                let pos_data = read_u16(data, offset + 4)?;
                let flags = read_u16(data, offset + 6)?;
                let (size, next) = if flags & 0xF000 == 0 {
                    let width = read_u16(data, offset + 8)?;
                    let height = read_u16(data, offset + 10)?;
                    ((width, height), offset + 12)
                } else {
                    let size = (flags & 0xF1FF) >> 9;
                    ((size, size), offset + 8)
                };
                sprites.push(MaskSprite {
                    uv: (uv as u8, (uv >> 8) as u8),
                    pos: (
                        i32::from(group.origin.0) + i32::from(delta as u8),
                        i32::from(group.origin.1) + i32::from((delta >> 8) as u8),
                    ),
                    size,
                    pos_data,
                    flags,
                    group: group_id,
                });
                offset = next;
            }
        }

        Ok(Self { groups, sprites })
    }

    /// Visibility bits with every group active: bit `g - 1` for group `g`.
    pub fn active_bits(&self) -> u32 {
        match self.groups.len() {
            0 => 0,
            groups if groups >= MAX_GROUPS => u32::MAX,
            groups => (1u32 << groups) - 1,
        }
    }
}

/// Whether group `group` (one-based) is visible in `bits`.
pub fn group_active(bits: u32, group: u8) -> bool {
    group != 0 && group <= MAX_GROUPS as u8 && bits & (1 << (group - 1)) != 0
}

/// Turn group `group` (one-based) on or off in `bits`.
///
/// Ids outside `1..=32` do nothing: no sprite can carry them.
pub fn set_group_active(bits: &mut u32, group: u8, active: bool) {
    if group == 0 || group > MAX_GROUPS as u8 {
        return;
    }
    if active {
        *bits |= 1 << (group - 1);
    } else {
        *bits &= !(1 << (group - 1));
    }
}

/// The folded room index used by the mask ordering tables.
///
/// Stages 6 and 7 reuse the STAGE1/STAGE2 rows, exactly like the backgrounds.
pub fn room_table_index(id: RoomId) -> usize {
    let stage = usize::from(id.fold_stage_digit()) - 1;
    usize::from(id.room) + stage * 0x20
}

/// A per-(room, camera) sort and brightness bias.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DepthBias {
    /// Subtracted from the sprite's brightness key.
    pub depth_bias: i16,
    /// Subtracted from the sprite's sort key.
    pub fade_bias: i32,
    /// When set, every sprite in the camera shares the record's brightness.
    pub pinned: bool,
}

/// The 32 shared bias records. Room/camera pairs index this table through
/// [`RECORD_INDEX`]; unlisted pairs use record 0.
static DEPTH_RECORDS: [DepthBias; 32] = [
    DepthBias {
        depth_bias: 0,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: -250,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: 30,
        fade_bias: -5,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: -40,
        pinned: false,
    },
    DepthBias {
        depth_bias: -30,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: -70,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: 60,
        fade_bias: -40,
        pinned: false,
    },
    DepthBias {
        depth_bias: -100,
        fade_bias: -20,
        pinned: false,
    },
    DepthBias {
        depth_bias: 100,
        fade_bias: -10,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: -10,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: 19,
        pinned: false,
    },
    DepthBias {
        depth_bias: 5,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: 1,
        fade_bias: 8,
        pinned: false,
    },
    DepthBias {
        depth_bias: -100,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: -50,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: -2,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: 1,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: -4,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: -20,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: -5,
        pinned: false,
    },
    DepthBias {
        depth_bias: 1000,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: 0,
        pinned: true,
    },
    DepthBias {
        depth_bias: 90,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: -30,
        fade_bias: -65,
        pinned: false,
    },
    DepthBias {
        depth_bias: -70,
        fade_bias: -30,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: 10,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: -10,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: -100,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: 84,
        fade_bias: 0,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: 100,
        pinned: false,
    },
    DepthBias {
        depth_bias: 0,
        fade_bias: -11,
        pinned: false,
    },
];

/// The non-zero rows of the 160-room x 8-camera index into [`DEPTH_RECORDS`].
///
/// Most rooms use record 0 for every camera and are omitted. The last two rows
/// are not reached by the shipped corpus but are part of the same table.
static RECORD_INDEX: [(u8, [u8; 8]); 24] = [
    (0, [0, 0, 0, 0, 0, 7, 0, 0]),
    (7, [0, 0, 0, 0, 7, 0, 0, 0]),
    (11, [0, 0, 31, 0, 0, 0, 0, 0]),
    (14, [0, 0, 15, 0, 0, 0, 0, 0]),
    (19, [0, 16, 0, 0, 0, 0, 0, 0]),
    (33, [0, 5, 0, 0, 0, 0, 0, 0]),
    (35, [11, 0, 0, 0, 0, 0, 0, 0]),
    (38, [19, 0, 0, 0, 0, 0, 0, 0]),
    (43, [0, 0, 6, 0, 0, 0, 0, 0]),
    (44, [0, 0, 20, 0, 0, 0, 0, 0]),
    (48, [0, 14, 0, 0, 0, 0, 0, 0]),
    (55, [0, 0, 0, 0, 31, 0, 0, 0]),
    (66, [0, 0, 0, 0, 0, 0, 21, 10]),
    (71, [0, 0, 0, 0, 27, 0, 0, 0]),
    (79, [0, 0, 0, 0, 0, 26, 0, 0]),
    (97, [0, 2, 0, 0, 0, 0, 0, 0]),
    (99, [18, 0, 0, 0, 0, 0, 0, 0]),
    (102, [0, 0, 0, 0, 9, 0, 0, 0]),
    (104, [0, 0, 0, 1, 1, 0, 0, 0]),
    (110, [22, 0, 0, 0, 0, 0, 0, 0]),
    (111, [23, 24, 25, 0, 0, 0, 0, 0]),
    (130, [0, 12, 0, 0, 0, 0, 0, 0]),
    (135, [0, 0, 0, 0, 9, 0, 0, 0]),
    (143, [0, 3, 0, 0, 13, 0, 0, 0]),
];

/// The eight rooms whose sprites are submitted in file order (bit per camera)
/// instead of the usual reverse walk. Room index 53 has no mask cameras in the
/// shipped corpus but is part of the same table.
static FORWARD_PATH_FLAGS: [(u8, u8); 8] = [
    (38, 0xFF),
    (50, 0x01),
    (53, 0xFF),
    (65, 0x08),
    (98, 0x01),
    (104, 0x10),
    (138, 0xFF),
    (144, 0xFF),
];

/// The per-(room, camera) bias record. Unlisted pairs use record 0.
pub fn depth_bias(id: RoomId, camera: usize) -> DepthBias {
    let room = room_table_index(id);
    if room >= ROOM_TABLE_LEN || camera >= 8 {
        return DepthBias::default();
    }
    let record = RECORD_INDEX
        .iter()
        .find(|&&(row, _)| usize::from(row) == room)
        .map(|&(_, cams)| usize::from(cams[camera]))
        .unwrap_or(0);
    DEPTH_RECORDS[record]
}

/// Whether this camera's sprites are submitted in file order.
///
/// The rest walk the entries backwards so that equal keys keep the original
/// paint order; [`mask_submission_order`] applies the direction.
pub fn walks_forward(id: RoomId, camera: usize) -> bool {
    let room = room_table_index(id);
    if room >= ROOM_TABLE_LEN || camera >= 8 {
        return false;
    }
    FORWARD_PATH_FLAGS
        .iter()
        .any(|&(row, flags)| usize::from(row) == room && flags & (1 << camera) != 0)
}

/// A per-entry rule on top of the shared bias record.
///
/// The original's backward walk carries a handful of hand-tuned per-overlay
/// offsets that the shared record cannot express. The depth offsets matter to
/// this engine even though sprites are drawn opaque and unshaded, because the
/// fixed 550 key depends on the depth being exactly zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskEntryOverride {
    /// The entry is never drawn on this camera.
    Hidden,
    /// The sort key uses this fixed fade value, ignoring `fade_bias`.
    FixedFade(i32),
    /// Added to the computed fade value before bucketing.
    FadeOffset(i32),
    /// The depth subtracts this fixed constant instead of the record's
    /// `depth_bias`.
    DepthBias(i16),
    /// The depth subtracts the record's `depth_bias` plus this constant.
    DepthExtra(i16),
    /// Both a depth extra and a fade offset together.
    DepthExtraAndFade { depth: i16, fade: i32 },
}

/// Room table index of stage 3 room 0B (the boulder passage).
const ROOM_BOULDER_PASSAGE: usize = 75;
/// Room table index of stage 4 room 0E (the water tank).
const ROOM_WATER_TANK: usize = 110;
/// Room table index of stage 4 room 0F (the security room).
const ROOM_SECURITY_ROOM: usize = 111;

/// The hand-tuned per-entry overrides of the rooms that need them.
///
/// `index` is the sprite's position in the flattened table, matching the
/// submission entry order. The falls room's forward-branch entry-18 offset is
/// deliberately absent: it only applies on the forward walk, and that room's
/// path flags never select the forward walk, so it is unreachable in the
/// shipped data.
pub fn mask_override(id: RoomId, camera: usize, index: usize) -> Option<MaskEntryOverride> {
    match (room_table_index(id), camera, index) {
        (ROOM_BOULDER_PASSAGE, 5, 25 | 26) => Some(MaskEntryOverride::Hidden),
        (ROOM_WATER_TANK, 1, 11) => Some(MaskEntryOverride::FixedFade(0x4B0)),
        (ROOM_WATER_TANK, 1, 9) => Some(MaskEntryOverride::DepthBias(0x5A)),
        (ROOM_WATER_TANK, 3, 7) => Some(MaskEntryOverride::DepthBias(0x4B)),
        (ROOM_WATER_TANK, 3, 9) => Some(MaskEntryOverride::DepthBias(0x3D)),
        (ROOM_WATER_TANK, 3, 31 | 32) => Some(MaskEntryOverride::DepthBias(0x3C)),
        (ROOM_WATER_TANK, 3, 33) => Some(MaskEntryOverride::Hidden),
        (ROOM_WATER_TANK, 2, 28) => Some(MaskEntryOverride::Hidden),
        (ROOM_SECURITY_ROOM, _, 34..=52) => Some(MaskEntryOverride::DepthExtraAndFade {
            depth: 0x352,
            fade: if index == 49 { 0x23 } else { 0x46 },
        }),
        (ROOM_SECURITY_ROOM, 0, 54) => Some(MaskEntryOverride::DepthExtra(0x12C)),
        _ => None,
    }
}

/// The sprite's brightness key: `pos_data >> 2`, clamped once the word leaves
/// the ordering table's 12-bit range.
///
/// The bound test is on `pos_data & 0xfffc`, so the bottom two bits never push
/// an otherwise in-range word over the clamp.
fn brightness_key(pos_data: u16) -> i16 {
    if pos_data & 0xfffc < 0x1000 {
        (pos_data >> 2) as i16
    } else {
        0x3ff
    }
}

/// The far-to-near draw key for one mask sprite, given the camera's record.
///
/// Larger keys are farther away. The original's `AddSprite` picks its fixed
/// 550 slot from the sprite's *brightness*, not its fade:
///
/// ```text
/// depth = pinned ? record.depth_bias : brightness(pos_data) - record.depth_bias
/// key   = (depth == 0) ? 550 : (clamp(fade, 0) & ~3) * 16
/// ```
///
/// so a sprite whose brightness equals the record's bias exactly is pinned to
/// 550 whatever fade it carries, while a non-zero depth with a negative fade
/// lands on key 0, the nearest slot.
pub fn mask_depth_key(pos_data: u16, record: DepthBias) -> u32 {
    let brightness = brightness_key(pos_data);
    let depth = if record.pinned {
        i32::from(record.depth_bias)
    } else {
        i32::from(brightness) - i32::from(record.depth_bias)
    };
    key_from_depth_and_fade(depth as i16, i32::from(pos_data) - record.fade_bias)
}

/// The key for one entry of a camera's table: the shared bias, the room's
/// per-entry override and the zero-depth rule. `None` means the entry is
/// hidden.
pub fn mask_sprite_key(
    id: RoomId,
    camera: usize,
    index: usize,
    sprite: &MaskSprite,
) -> Option<u32> {
    let record = depth_bias(id, camera);
    let brightness = i32::from(brightness_key(sprite.pos_data));
    let base_depth = if record.pinned {
        i32::from(record.depth_bias)
    } else {
        brightness - i32::from(record.depth_bias)
    };
    let base_fade = i32::from(sprite.pos_data) - record.fade_bias;

    let (depth, fade) = match mask_override(id, camera, index) {
        Some(MaskEntryOverride::Hidden) => return None,
        Some(MaskEntryOverride::FixedFade(value)) => (base_depth, value),
        Some(MaskEntryOverride::FadeOffset(offset)) => (base_depth, base_fade + offset),
        Some(MaskEntryOverride::DepthBias(value)) => (brightness - i32::from(value), base_fade),
        Some(MaskEntryOverride::DepthExtra(extra)) => (base_depth - i32::from(extra), base_fade),
        Some(MaskEntryOverride::DepthExtraAndFade { depth, fade }) => {
            (base_depth - i32::from(depth), base_fade + fade)
        }
        None => (base_depth, base_fade),
    };
    Some(key_from_depth_and_fade(depth as i16, fade))
}

/// The `AddSprite` ordering key: the fixed 550 slot when the depth is exactly
/// zero, otherwise the fade clamped at zero, rounded down to a bucket of four
/// and scaled by sixteen.
fn key_from_depth_and_fade(depth: i16, fade: i32) -> u32 {
    if depth == 0 {
        550
    } else {
        let bucket = if fade < 0 { 0 } else { fade & !3 };
        (bucket as u32).wrapping_mul(16)
    }
}

/// The flattened indices of a camera's sprites in submission order.
///
/// Most cameras walk the table backwards, which is what makes equal-key
/// sprites paint in the right order once the list is stably sorted far to
/// near; hidden entries are skipped.
pub fn mask_submission_order(id: RoomId, camera: usize, count: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..count).collect();
    if !walks_forward(id, camera) {
        order.reverse();
    }
    order.retain(|&index| mask_override(id, camera, index) != Some(MaskEntryOverride::Hidden));
    order
}

/// Stable far-to-near order for a mixed draw list.
///
/// Items with larger keys are farther and are drawn first. Equal keys keep
/// their submission order, so mask entries that share a key stay in the order
/// [`mask_submission_order`] produced relative to each other and to any other
/// scene item.
pub fn order_far_to_near<T>(items: &mut [T], mut key: impl FnMut(&T) -> u32) {
    items.sort_by_key(|item| std::cmp::Reverse(key(item)));
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    let end = offset
        .checked_add(2)
        .context("mask table offset overflow")?;
    let raw = data.get(offset..end).with_context(|| {
        format!(
            "mask table read at 0x{offset:x} is out of bounds for the {}-byte RDT",
            data.len()
        )
    })?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_i32(data: &[u8], offset: usize) -> Result<i32> {
    let end = offset
        .checked_add(4)
        .context("mask table offset overflow")?;
    let raw = data.get(offset..end).with_context(|| {
        format!(
            "mask table read at 0x{offset:x} is out of bounds for the {}-byte RDT",
            data.len()
        )
    })?;
    Ok(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(stage: u8, room: u8) -> RoomId {
        RoomId {
            stage,
            room,
            player_flag: 0,
        }
    }

    fn sprite(pos_data: u16) -> MaskSprite {
        MaskSprite {
            pos_data,
            ..MaskSprite::default()
        }
    }

    /// Where [`table`] places the table, past a leading word so that the
    /// direct offset is never the reserved zero pointer.
    const TABLE_OFFSET: u32 = 4;

    /// One i32 group count plus group headers and sprite words, at
    /// [`TABLE_OFFSET`].
    fn table(group_count: i32, groups: &[[u16; 4]], sprites: &[u16]) -> Vec<u8> {
        let mut data = vec![0u8; TABLE_OFFSET as usize];
        data.extend_from_slice(&group_count.to_le_bytes());
        for group in groups {
            for word in group {
                data.extend_from_slice(&word.to_le_bytes());
            }
        }
        for word in sprites {
            data.extend_from_slice(&word.to_le_bytes());
        }
        data
    }

    #[test]
    fn parses_both_size_forms() {
        let groups = [
            [1, 0x1234, 10, 20],
            [1, 0x4321, (-5i16) as u16, (-6i16) as u16],
        ];
        let sprites = [
            // uv (1,2), delta (3,4), posData 100, flags 0x0800, w 8, h 16.
            0x0201, 0x0403, 100, 0x0800, 8, 16, // uv (5,6), delta (0,0),
            // posData 200, flags 0x2401 -> packed size 16x16.
            0x0605, 0, 200, 0x2401,
        ];
        let data = table(2, &groups, &sprites);

        let parsed = MaskTable::parse(&data, TABLE_OFFSET).unwrap();

        assert_eq!(parsed.groups.len(), 2);
        assert_eq!(parsed.groups[0].sprite_count, 1);
        assert_eq!(parsed.groups[0].tex_bits, 0x1234);
        assert_eq!(parsed.groups[0].origin, (10, 20));
        assert_eq!(parsed.groups[1].origin, (-5, -6));
        assert_eq!(parsed.groups[1].tex_bits, 0x4321);
        assert_eq!(parsed.sprites.len(), 2);

        let first = parsed.sprites[0];
        assert_eq!(first.uv, (1, 2));
        assert_eq!(first.pos, (13, 24));
        assert_eq!(first.size, (8, 16));
        assert_eq!(first.pos_data, 100);
        assert_eq!(first.flags, 0x0800);
        assert_eq!(first.group, 1);

        let second = parsed.sprites[1];
        assert_eq!(second.uv, (5, 6));
        assert_eq!(second.pos, (-5, -6));
        assert_eq!(second.size, (16, 16));
        assert_eq!(second.pos_data, 200);
        assert_eq!(second.flags, 0x2401);
        assert_eq!(second.group, 2);

        assert_eq!(parsed.active_bits(), 0b11);
    }

    #[test]
    fn zero_pointer_and_zero_groups_are_empty() {
        assert_eq!(MaskTable::parse(&[], 0).unwrap(), MaskTable::default());
        let data = table(0, &[], &[]);
        assert_eq!(
            MaskTable::parse(&data, TABLE_OFFSET).unwrap(),
            MaskTable::default()
        );
    }

    #[test]
    fn rejects_out_of_bounds_pointer() {
        let data = table(1, &[[1, 0, 0, 0]], &[0, 0, 0, 0, 1, 1]);
        let message = MaskTable::parse(&data, data.len() as u32 + 8)
            .unwrap_err()
            .to_string();
        assert!(message.contains("out of bounds"), "{message}");
    }

    #[test]
    fn rejects_negative_group_count() {
        let data = table(-1, &[], &[]);
        let message = MaskTable::parse(&data, TABLE_OFFSET)
            .unwrap_err()
            .to_string();
        assert!(message.contains("negative"), "{message}");
    }

    #[test]
    fn rejects_too_many_groups() {
        let data = table(33, &[], &[]);
        let message = MaskTable::parse(&data, TABLE_OFFSET)
            .unwrap_err()
            .to_string();
        assert!(message.contains("more than"), "{message}");
    }

    #[test]
    fn rejects_sprite_counts_over_the_cap() {
        // Two groups of 65,535 packed-size sprites each: parsing the second
        // group's second sprite crosses the 65,536-sprite cap before the input
        // runs out.
        let groups = [[0xFFFFu16, 0, 0, 0], [2, 0, 0, 0]];
        let mut data = table(2, &groups, &[]);
        for _ in 0..=budget::MAX_MASK_SPRITES {
            // A nonzero high nibble selects the packed eight-byte size form.
            for word in [0u16, 0, 100, 0x2000] {
                data.extend_from_slice(&word.to_le_bytes());
            }
        }
        let message = budget::assert_cap_error(MaskTable::parse(&data, TABLE_OFFSET));
        assert!(message.contains("mask sprite count"), "{message}");
    }

    #[test]
    fn rejects_truncated_group_headers() {
        let mut data = table(2, &[[1, 0, 0, 0]], &[]);
        data.truncate(data.len() - 2);
        assert!(MaskTable::parse(&data, TABLE_OFFSET).is_err());
    }

    #[test]
    fn rejects_truncated_sprites() {
        // One group claiming two sprites but carrying one and a half.
        let mut data = table(1, &[[2, 0, 0, 0]], &[]);
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&0x0800u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        assert!(MaskTable::parse(&data, TABLE_OFFSET).is_err());
    }

    #[test]
    fn room_index_folds_return_stages() {
        assert_eq!(room_table_index(room(1, 0)), 0);
        assert_eq!(room_table_index(room(2, 6)), 38);
        assert_eq!(room_table_index(room(3, 0x0B)), 75);
        assert_eq!(room_table_index(room(4, 0x0E)), 110);
        assert_eq!(room_table_index(room(5, 0x10)), 144);
        assert_eq!(room_table_index(room(6, 0)), 0);
        assert_eq!(room_table_index(room(7, 0x10)), 48);
    }

    #[test]
    fn depth_records_follow_the_room_camera_index() {
        let default = depth_bias(room(1, 0), 0);
        assert_eq!(default, DepthBias::default());
        assert!(!default.pinned);

        assert_eq!(depth_bias(room(1, 0), 5).depth_bias, -100);
        assert_eq!(depth_bias(room(1, 0), 5).fade_bias, -20);
        assert_eq!(depth_bias(room(2, 6), 0).depth_bias, 0);
        assert_eq!(depth_bias(room(2, 6), 0).fade_bias, -5);
        assert_eq!(depth_bias(room(4, 8), 3).depth_bias, -250);
        assert_eq!(depth_bias(room(4, 8), 4).depth_bias, -250);
        assert_eq!(depth_bias(room(3, 2), 7).fade_bias, 19);
        assert!(depth_bias(room(3, 2), 6).pinned);
        assert_eq!(depth_bias(room(4, 0x0E), 0).depth_bias, 90);

        // Out-of-range cameras fall back to the default record.
        assert_eq!(depth_bias(room(1, 0), 8), DepthBias::default());
    }

    #[test]
    fn path_flags_select_the_forward_walk() {
        assert!(walks_forward(room(2, 6), 0));
        assert!(walks_forward(room(2, 6), 7));
        assert!(walks_forward(room(2, 0x12), 0));
        assert!(!walks_forward(room(2, 0x12), 1));
        assert!(!walks_forward(room(3, 1), 2));
        assert!(walks_forward(room(3, 1), 3));
        assert!(walks_forward(room(4, 2), 0));
        assert!(!walks_forward(room(4, 2), 1));
        assert!(walks_forward(room(4, 8), 4));
        assert!(walks_forward(room(5, 0xA), 3));
        assert!(walks_forward(room(5, 0xA), 4));
        assert!(!walks_forward(room(1, 0), 0));
    }

    #[test]
    fn depth_key_buckets_and_clamps() {
        let default = DepthBias::default();
        assert_eq!(mask_depth_key(0, default), 550);
        assert_eq!(mask_depth_key(3, default), 550);
        assert_eq!(mask_depth_key(4, default), 64);
        assert_eq!(mask_depth_key(7, default), 64);
        assert_eq!(mask_depth_key(8, default), 128);
        assert_eq!(mask_depth_key(380, default), 6080);
        // A non-zero depth with a fade that goes negative lands on the nearest
        // slot 0; the 550 rule keys on the depth, not the fade.
        assert_eq!(
            mask_depth_key(
                5,
                DepthBias {
                    fade_bias: 100,
                    ..default
                }
            ),
            0
        );
        assert_eq!(
            mask_depth_key(
                100,
                DepthBias {
                    fade_bias: 19,
                    ..default
                }
            ),
            1280
        );
        // A zero depth takes 550 even when the fade bucket is non-zero.
        assert_eq!(
            mask_depth_key(
                0,
                DepthBias {
                    fade_bias: -100,
                    ..default
                }
            ),
            550
        );
    }

    #[test]
    fn pinned_records_share_the_fixed_key() {
        // ROOM3020/3021 camera 6 uses the one pinned record: every sprite's
        // depth is the record's own bias (zero), so all take the 550 slot.
        let pinned = depth_bias(room(3, 2), 6);
        assert!(pinned.pinned);
        for pos_data in [0, 4, 400, 0x4000] {
            assert_eq!(
                mask_sprite_key(room(3, 2), 6, 0, &sprite(pos_data)),
                Some(550),
                "pos_data {pos_data}"
            );
        }
    }

    #[test]
    fn sprite_keys_apply_the_room_overrides() {
        // The water tank camera 1 entry 11 has a fixed fade and the record's
        // depth.
        assert_eq!(
            mask_sprite_key(room(4, 0x0E), 1, 11, &sprite(100)),
            Some(key_from_depth_and_fade(25, 0x4B0))
        );
        // The water tank's hand-tuned depth entries replace the record bias.
        assert_eq!(
            mask_sprite_key(room(4, 0x0E), 1, 9, &sprite(100)),
            Some(key_from_depth_and_fade(25 - 0x5A, 100))
        );
        // The security room pulls its run of overlays back by 0x352 and
        // offsets the fade.
        let record = depth_bias(room(4, 0x0F), 1);
        assert_eq!(
            mask_sprite_key(room(4, 0x0F), 1, 40, &sprite(100)),
            Some(key_from_depth_and_fade(
                (25 - i32::from(record.depth_bias) - 0x352) as i16,
                100 - record.fade_bias + 0x46,
            ))
        );
        let record = depth_bias(room(4, 0x0F), 2);
        assert_eq!(
            mask_sprite_key(room(4, 0x0F), 2, 49, &sprite(100)),
            Some(key_from_depth_and_fade(
                (25 - i32::from(record.depth_bias) - 0x352) as i16,
                100 - record.fade_bias + 0x23,
            ))
        );
    }

    #[test]
    fn the_falls_entry_18_override_is_unreachable() {
        // The falls room never walks its entries forward, so the original's
        // forward-only +200 offset on entry 18 is dead code.
        assert!(!walks_forward(room(3, 2), 0));
        assert_eq!(mask_override(room(3, 2), 0, 18), None);
        assert_eq!(
            mask_sprite_key(room(3, 2), 0, 18, &sprite(100)),
            Some(key_from_depth_and_fade(25, 100))
        );
    }

    #[test]
    fn overrides_hide_entries() {
        assert_eq!(
            mask_override(room(3, 0x0B), 5, 25),
            Some(MaskEntryOverride::Hidden)
        );
        assert_eq!(
            mask_override(room(3, 0x0B), 5, 26),
            Some(MaskEntryOverride::Hidden)
        );
        assert_eq!(
            mask_override(room(4, 0x0E), 3, 33),
            Some(MaskEntryOverride::Hidden)
        );
        assert_eq!(
            mask_override(room(4, 0x0E), 2, 28),
            Some(MaskEntryOverride::Hidden)
        );
        assert_eq!(mask_sprite_key(room(3, 0x0B), 5, 25, &sprite(10)), None);
        assert_eq!(mask_override(room(3, 0x0B), 5, 24), None);
    }

    #[test]
    fn submission_order_follows_the_path_flags() {
        // A backward room starts at the last entry.
        assert_eq!(mask_submission_order(room(1, 0), 0, 4), vec![3, 2, 1, 0]);
        // A forward room keeps file order.
        assert_eq!(mask_submission_order(room(2, 6), 0, 4), vec![0, 1, 2, 3]);
        // Hidden entries are skipped where they appear in the walk.
        let order = mask_submission_order(room(4, 0x0E), 2, 30);
        assert_eq!(order.len(), 29);
        assert!(!order.contains(&28));
    }

    #[test]
    fn order_far_to_near_is_stable() {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        struct Item {
            key: u32,
            order: u8,
        }
        let mut items = [
            Item { key: 100, order: 0 },
            Item { key: 50, order: 1 },
            Item { key: 100, order: 2 },
            Item { key: 550, order: 3 },
            Item { key: 50, order: 4 },
        ];

        order_far_to_near(&mut items, |item| item.key);

        assert_eq!(
            items,
            [
                Item { key: 550, order: 3 },
                Item { key: 100, order: 0 },
                Item { key: 100, order: 2 },
                Item { key: 50, order: 1 },
                Item { key: 50, order: 4 },
            ]
        );
    }

    #[test]
    fn order_far_to_near_mixes_masks_and_other_items() {
        enum SceneItem {
            Mask(u16),
            Player { view_z: u32 },
        }
        let mut items = vec![
            SceneItem::Mask(380),
            SceneItem::Player { view_z: 6000 },
            SceneItem::Player { view_z: 5000 },
            SceneItem::Mask(100),
        ];

        order_far_to_near(&mut items, |item| match item {
            SceneItem::Mask(pos_data) => mask_depth_key(*pos_data, DepthBias::default()),
            SceneItem::Player { view_z } => *view_z,
        });

        assert!(matches!(items[0], SceneItem::Mask(380)));
        assert!(matches!(items[1], SceneItem::Player { view_z: 6000 }));
        assert!(matches!(items[2], SceneItem::Player { view_z: 5000 }));
        assert!(matches!(items[3], SceneItem::Mask(100)));
    }

    #[test]
    fn group_bits_round_trip() {
        let mut bits = 0u32;
        assert!(!group_active(bits, 1));
        set_group_active(&mut bits, 1, true);
        set_group_active(&mut bits, 3, true);
        assert!(group_active(bits, 1));
        assert!(!group_active(bits, 2));
        assert!(group_active(bits, 3));
        set_group_active(&mut bits, 1, false);
        assert_eq!(bits, 1 << 2);
        assert!(!group_active(bits, 0));
        assert!(!group_active(bits, 33));
        set_group_active(&mut bits, 0, true);
        set_group_active(&mut bits, 33, true);
        assert_eq!(bits, 1 << 2);
    }
}
