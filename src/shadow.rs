//! The player's ground shadow: the texture bake, the per-camera placement
//! record and the conditions under which the shadow is queued.
//!
//! The original builds the player's shadow as a textured parallelogram on the
//! floor: a quad spanning half-extents 500 (local X) by 700 (local Z) around
//! the entity, rotated by the entity's yaw and placed at the entity's floor
//! height. Its texture is the shared `KAGE.TIM` mask page, whose palette's red
//! channel is a radial coverage ramp: every non-transparent entry collapses to
//! a single flat shade and the destination is multiplied down by at most
//! `16/31` of the coverage. The shadow is queued every frame through the same
//! fade-sprite path the enemies use, so it interleaves with the room masks and
//! the model by view-space depth.
//!
//! [`decode`] bakes that coverage into an RGBA page: white texels carrying the
//! darkening alpha. The renderer multiplies the framebuffer towards
//! [`billboard_tint`] by that alpha.
//!
//! The shadow is skipped when the entity is outside the current camera's
//! switch zone and not on a stair/ladder, and while a door transition owns the
//! player; the water tank entry room queues it unconditionally.

use anyhow::{Context, Result};

use crate::model::Texture8;
use crate::player::PlayerState;
use crate::state::{Image, RoomId, RoomState};

/// Pack entry of the shadow mask page.
pub const KAGE_ENTRY: &str = "shadow/kage.tim";

/// Half-extent of the player shadow along the entity's local X, in room units.
pub const PLAYER_HALF_X: i32 = 500;
/// Half-extent of the player shadow along the entity's local Z, in room units.
pub const PLAYER_HALF_Z: i32 = 700;
/// The living player's billboard tint (`0x808080` in the original header).
pub const PLAYER_COLOR: u32 = 0x0080_8080;

/// 1-based stage digit of the guardhouse (the original's 0-indexed stage 3).
const STAGE_GUARDHOUSE: u8 = 4;
/// Room id of the water tank entry, where the shadow is never skipped.
const ROOM_WATER_TANK_ENTRY: u8 = 0x0D;

/// Decode a shadow mask TIM and bake its coverage into the texel alpha.
///
/// The palette's red channel is the ramp: `r5` is the raw 5-bit red of the
/// entry, `coverage = 255 - ((255 - r5 * 8) * 2 & 0xFF)` stretches it across
/// the page, and the alpha is `coverage * 16 / 31` - the fraction of the
/// destination the flat shadow shade can take away. Transparent entries
/// (`r5 == 0`) bake to zero alpha.
pub fn decode(data: &[u8]) -> Result<Image> {
    let texture = crate::tim::decode_8bpp(data).context("invalid shadow mask TIM")?;
    Ok(bake(&texture))
}

/// Bake a decoded 8bpp shadow texture into a white RGBA page with the
/// coverage-derived alpha.
pub fn bake(texture: &Texture8) -> Image {
    let mut rgba = Vec::with_capacity(texture.indices.len() * 4);
    for &index in &texture.indices {
        let red = texture.palette(0, index)[0];
        rgba.extend_from_slice(&[255, 255, 255, coverage_alpha(red)]);
    }
    Image {
        width: texture.width,
        height: texture.height,
        rgba,
    }
}

/// The baked alpha of one palette entry, from its expanded 8-bit red.
fn coverage_alpha(red: u8) -> u8 {
    let r5 = (u32::from(red) * 31 + 127) / 255;
    let stretched = ((255 - r5 * 8) * 2) & 0xFF;
    let coverage = 255 - stretched;
    (coverage * 16 / 31) as u8
}

/// The tint the fade-sprite primitive blends towards.
///
/// The original converts each tint byte with `b / 256` below `0x80` and
/// `(256 - b) / 256` above it, with an all-equal special case that collapses
/// the shadow grey `0x808080` to near-black.
pub fn billboard_tint(rgb: u32) -> [u8; 3] {
    // The packed tint sits in a little-endian dword, so the low byte is red.
    let bytes = [rgb as u8, (rgb >> 8) as u8, (rgb >> 16) as u8];
    if bytes[0] == bytes[1] && bytes[1] == bytes[2] {
        [0, 0, 1]
    } else {
        // `256 - byte` as an 8-bit wrap: 0x80 stays 128, 0xFF folds to 1.
        bytes.map(|byte| {
            if byte < 0x80 {
                byte
            } else {
                byte.wrapping_neg()
            }
        })
    }
}

/// The view-space Y offset of the (stage, room, camera) placement record.
///
/// The original keeps 20 records selected by a `(room + stage * 0x20) * 8 +
/// camera` index table; only the records below shift the quad in view space,
/// the rest only bias its queue tie-break. Stages above the laboratory reuse
/// the first two rows.
pub fn offset_y(id: RoomId, camera: usize) -> i32 {
    let stage = usize::from(id.stage_index());
    let folded = if stage > 4 { stage - 5 } else { stage };
    match (folded, id.room, camera) {
        (0, 0x0A, _) => -200,
        (2, 0x01, 0..=6) | (2, 0x02, _) => -40,
        (2, 0x05, 0) | (2, 0x0F, 1) => 100,
        (3, 0x0D, 5) => 120,
        _ => 0,
    }
}

/// Whether the player's shadow is queued this frame.
///
/// The original tests the entity's `zoneFlags & 0x7f` (the current camera's
/// switch-zone membership bit, plus the stair/ladder and door latches) against
/// its still-being-in-a-door-transition flag, and forces the shadow on in the
/// guardhouse's water tank entry room. This engine never renders gameplay
/// while a door transition owns the player, so the door flag is implicit; the
/// `set_stairs_zone` latch survives for the whole room, while a stair ramp's
/// `stairs_height_update` height does not raise a zone flag on its own.
pub fn visible(id: RoomId, room: &RoomState, camera: usize, player: &PlayerState) -> bool {
    if id.stage == STAGE_GUARDHOUSE && id.room == ROOM_WATER_TANK_ENTRY {
        return true;
    }
    if player.stairs.in_zone {
        return true;
    }
    let header = room
        .zones
        .iter()
        .find(|zone| zone.cam_from >= 0 && zone.cam_from as usize == camera);
    header.is_some_and(|zone| zone.contains(player.pos[0], player.pos[2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::{StairState, spawn};
    use crate::state::Zone;

    fn palette_red(r5: u32) -> u8 {
        (r5 * 255 / 31) as u8
    }

    fn texture(indices: Vec<u8>, reds: &[u32]) -> Texture8 {
        Texture8 {
            width: indices.len() as u32,
            height: 1,
            indices,
            palettes: reds
                .iter()
                .map(|&r5| [palette_red(r5), 0, 0, 255])
                .collect(),
            stp: Vec::new(),
        }
    }

    #[test]
    fn coverage_alpha_follows_the_shadow_ramp() {
        // The r5 -> alpha ladder of the shipped KAGE palette.
        assert_eq!(coverage_alpha(palette_red(0)), 0);
        assert_eq!(coverage_alpha(palette_red(2)), 17);
        assert_eq!(coverage_alpha(palette_red(3)), 25);
        assert_eq!(coverage_alpha(palette_red(5)), 41);
        assert_eq!(coverage_alpha(palette_red(7)), 58);
        assert_eq!(coverage_alpha(palette_red(9)), 74);
        assert_eq!(coverage_alpha(palette_red(11)), 91);
        assert_eq!(coverage_alpha(palette_red(12)), 99);
    }

    #[test]
    fn bake_marks_transparent_entries_and_keeps_the_page_shape() {
        let texture = texture(vec![0, 1, 2], &[0, 12, 0]);
        let image = bake(&texture);
        assert_eq!((image.width, image.height), (3, 1));
        assert_eq!(&image.rgba[0..4], &[255, 255, 255, 0]);
        assert_eq!(&image.rgba[4..8], &[255, 255, 255, 99]);
        assert_eq!(&image.rgba[8..12], &[255, 255, 255, 0]);
    }

    #[test]
    fn coverage_alpha_round_trips_every_five_bit_red() {
        for r5 in 0..=31u32 {
            let stretched = ((255 - r5 * 8) * 2) & 0xFF;
            let expected = ((255 - stretched) * 16 / 31) as u8;
            assert_eq!(coverage_alpha(palette_red(r5)), expected, "r5={r5}");
        }
        // The transparent border stays a hole; the disc core caps at 99/255.
        assert_eq!(coverage_alpha(palette_red(0)), 0);
        assert_eq!(coverage_alpha(palette_red(12)), 99);
    }

    #[test]
    fn decode_rejects_a_non_shadow_tim() {
        assert!(decode(b"not a tim").is_err());
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn decodes_the_real_shadow_page() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let path = ["JPN", ""]
            .iter()
            .map(|prefix| root.join(prefix).join("DATA").join("KAGE.TIM"))
            .find(|candidate| candidate.is_file());
        let Some(path) = path else {
            panic!("KAGE.TIM not found under {}", root.display());
        };
        let image = decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((image.width, image.height), (26, 29));
        let alphas: std::collections::BTreeSet<u8> = image
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[3])
            .collect();
        assert_eq!(
            alphas.into_iter().collect::<Vec<_>>(),
            vec![0, 17, 25, 41, 58, 74, 91, 99]
        );
    }

    #[test]
    fn billboard_tint_matches_the_original_conversion() {
        // The shadow grey special case collapses to near-black.
        assert_eq!(billboard_tint(0x0080_8080), [0, 0, 1]);
        // The death blood pool's 0x00FFFF50 stores (0x50, 0xFF, 0xFF) in
        // memory and comes out dark red.
        assert_eq!(billboard_tint(0x00FF_FF50), [0x50, 1, 1]);
        // Low bytes pass through; high bytes fold back.
        assert_eq!(billboard_tint(0x0040_FF20), [0x20, 1, 0x40]);
    }

    #[test]
    fn placement_offsets_follow_the_room_table() {
        let room = |id: &str, camera: usize| offset_y(RoomId::parse(id).unwrap(), camera);
        assert_eq!(room("1000", 0), 0);
        // Stage 1 room 0A: every camera carries the -200 offset.
        assert_eq!(room("10A0", 0), -200);
        assert_eq!(room("10A0", 7), -200);
        // Stage 3 rooms 1 and 2 carry the -40 offset (room 1 camera 7 is a
        // separate record with none).
        assert_eq!(room("3010", 0), -40);
        assert_eq!(room("3010", 6), -40);
        assert_eq!(room("3010", 7), 0);
        assert_eq!(room("3020", 0), -40);
        assert_eq!(room("3020", 7), -40);
        assert_eq!(room("3050", 0), 100);
        assert_eq!(room("30F0", 1), 100);
        assert_eq!(room("40D0", 5), 120);
        // The laboratory has its own row (only room 7 carries an offset), so
        // its room 0A keeps the default placement.
        assert_eq!(room("50A0", 0), 0);
        // Return-mansion stages fold onto the mansion rows.
        assert_eq!(room("60A0", 0), -200);
        assert_eq!(room("7010", 0), 0);
    }

    fn room_with_header() -> RoomState {
        RoomState {
            zones: vec![Zone {
                cam_to: 1,
                cam_from: 0,
                corners: [[0, 0], [0, 1000], [1000, 1000], [1000, 0]],
            }],
            ..RoomState::default()
        }
    }

    fn player_at(pos: [i32; 3]) -> PlayerState {
        let mut player = spawn(
            RoomId {
                stage: 1,
                room: 0,
                player_flag: 0,
            },
            &RoomState::default(),
        );
        player.pos = pos;
        player
    }

    #[test]
    fn shadow_is_visible_inside_the_camera_switch_zone() {
        let room = room_with_header();
        let id = RoomId::parse("1000").unwrap();
        assert!(visible(id, &room, 0, &player_at([500, 0, 500])));
        // Outside the header zone the shadow is skipped.
        assert!(!visible(id, &room, 0, &player_at([2000, 0, 500])));
        // A different camera id has no zone group here.
        assert!(!visible(id, &room, 1, &player_at([500, 0, 500])));
    }

    #[test]
    fn stairs_and_the_water_tank_entry_force_the_shadow_on() {
        let room = room_with_header();
        let id = RoomId::parse("1000").unwrap();
        // A stair ramp height alone raises no zone flag in the original, so it
        // does not bring the shadow back outside the camera zone.
        let mut player = player_at([2000, 0, 500]);
        player.stairs.height = Some(300);
        assert!(!visible(id, &room, 0, &player));
        // The set_stairs_zone latch (zone flag 0x20) does.
        player.stairs.height = None;
        player.stairs = StairState {
            in_zone: true,
            ..StairState::default()
        };
        assert!(visible(id, &room, 0, &player));

        let water_tank = RoomId::parse("40D0").unwrap();
        let outside = player_at([2000, 0, 500]);
        assert!(visible(water_tank, &room, 0, &outside));
    }
}
