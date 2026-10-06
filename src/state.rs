//! Game and room state.
//!
//! These are the engine's internal state after an RDT has been loaded. Source
//! file formats are not kept around during gameplay.

use crate::mask::MaskSprite;

/// A stage/room/player identity, e.g. stage 1, room 0x00, player 1 -> RDT 1001.
///
/// The identity is packed hex: the first digit is the stage, the middle two are
/// the room in hex, and the last is the player/flag digit. This matches the
/// shipped `ROOM####.RDT` names and the `RC` background names.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RoomId {
    /// 1-based stage number (1-7).
    pub stage: u8,
    /// Room number within the stage (0x00-0x1F).
    pub room: u8,
    /// Player/flag digit (0 = Chris, 1 = Jill).
    pub player_flag: u8,
}

impl RoomId {
    /// Highest valid stage digit.
    pub const MAX_STAGE: u8 = 7;
    /// Highest valid room number.
    pub const MAX_ROOM: u8 = 0x1F;

    /// Parse a three- or four-digit RDT identity, e.g. `100`, `1001`, `11C1`.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let digits: Vec<u8> = text
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<_>>()
            .ok_or_else(|| anyhow::anyhow!("`{text}` is not a hex RDT identity"))?;
        match digits.as_slice() {
            [stage, hi, lo] => Self::from_digits(*stage, (*hi << 4) | *lo, 0, text),
            [stage, hi, lo, player] => Self::from_digits(*stage, (*hi << 4) | *lo, *player, text),
            _ => anyhow::bail!("`{text}` must be three or four hex digits, e.g. 100 or 1001"),
        }
    }

    /// Parse a three-digit room id (`100`, `11C`) plus a player digit.
    pub fn from_room_and_player(room: &str, player: u8) -> anyhow::Result<Self> {
        let digits: Vec<u8> = room
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<_>>()
            .ok_or_else(|| anyhow::anyhow!("`{room}` is not a hex room id"))?;
        match digits.as_slice() {
            [stage, hi, lo] => Self::from_digits(*stage, (*hi << 4) | *lo, player, room),
            _ => anyhow::bail!("`{room}` must be exactly three hex digits, e.g. 100 or 11C"),
        }
    }

    fn from_digits(stage: u8, room: u8, player_flag: u8, text: &str) -> anyhow::Result<Self> {
        if !(1..=Self::MAX_STAGE).contains(&stage) {
            anyhow::bail!("stage digit in `{text}` must be 1-7");
        }
        if room > Self::MAX_ROOM {
            anyhow::bail!("room in `{text}` must be 00-1F");
        }
        if player_flag > 9 {
            anyhow::bail!("player digit in `{text}` must be 0-9");
        }
        Ok(Self {
            stage,
            room,
            player_flag,
        })
    }

    /// Four-digit packed hex RDT number, e.g. 0x1001.
    pub fn rdt_number(self) -> u16 {
        ((self.stage as u16) << 12) | ((self.room as u16) << 4) | self.player_flag as u16
    }

    /// Pack path of the raw RDT, e.g. `room/1001.rdt`.
    pub fn rdt_entry(self) -> String {
        format!("room/{:04x}.rdt", self.rdt_number())
    }

    /// Pack path of a standalone SCD override, e.g. `scd/1001.scd`.
    pub fn scd_entry(self) -> String {
        format!("scd/{:04x}.scd", self.rdt_number())
    }

    /// Three-digit room id used by cut names, e.g. `100`, `11C`.
    pub fn room3(self) -> String {
        format!("{}{:02X}", self.stage, self.room)
    }

    /// Pack path of a camera background, e.g. `roomcut/100_000.bmp`.
    pub fn cut_entry(self, camera: usize) -> String {
        format!("roomcut/{}_{camera:03}.bmp", self.room3())
    }

    /// Pack path of a camera's mask page, e.g. `roommask/100_000.bmp`.
    pub fn roommask_entry(self, camera: usize) -> String {
        format!("roommask/{}_{camera:03}.bmp", self.room3())
    }

    /// Zero-based stage index for stage-indexed tables.
    pub fn stage_index(self) -> u8 {
        self.stage - 1
    }

    /// Stage digit used by the background files. Stages 6 and 7 reuse the
    /// STAGE1/STAGE2 backgrounds with the digit reduced by 5.
    pub fn fold_stage_digit(self) -> u8 {
        if self.stage >= 6 {
            self.stage - 5
        } else {
            self.stage
        }
    }
}

/// Raw RGBA8 image.
#[derive(Debug, Clone, Default)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// One room light. The RDT stores three of them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Light {
    /// World-space position.
    pub pos: [i32; 3],
    /// Red, green and blue components.
    pub color: [u8; 3],
    /// Light type: 0 is a point light with radial falloff, anything else is
    /// directional.
    pub kind: u16,
    /// Falloff radius, used by point lights.
    pub radius: i16,
}

/// One collision boundary rectangle, corners stored max-first.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CollisionRect {
    pub x_max: u16,
    pub z_max: u16,
    pub x_min: u16,
    pub z_min: u16,
    pub kind: u16,
    pub flags: u16,
}

/// Room collision boundaries, split into quadrants around `(cell_x, cell_z)`.
#[derive(Debug, Clone)]
pub struct Collision {
    pub cell_x: i16,
    pub cell_z: i16,
    pub quadrants: [Vec<CollisionRect>; 4],
}

impl Default for Collision {
    fn default() -> Self {
        Self {
            cell_x: 0,
            cell_z: 0,
            quadrants: std::array::from_fn(|_| Vec::new()),
        }
    }
}

impl Collision {
    /// The records of the quadrant containing `(x, z)`.
    pub fn records(&self, x: i32, z: i32) -> &[CollisionRect] {
        let quadrant = (usize::from(z < i32::from(self.cell_z)) << 1)
            | usize::from(x < i32::from(self.cell_x));
        &self.quadrants[quadrant]
    }
}

/// One camera switch zone record. Every record is a zone; the first record of
/// each `cam_from` group is that group's header.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    pub cam_to: i16,
    pub cam_from: i16,
    pub corners: [[i16; 2]; 4],
}

impl Zone {
    /// Whether `(x, z)` lies inside the quad.
    ///
    /// Faithful to the original: every corner is zero-extended to unsigned
    /// before the four cross-product edge tests, pivoting on corners 0 and 2.
    pub fn contains(&self, x: i32, z: i32) -> bool {
        let corner = |index: usize| {
            [
                i64::from(self.corners[index][0] as u16),
                i64::from(self.corners[index][1] as u16),
            ]
        };
        let [x0, z0] = corner(0);
        let [x1, z1] = corner(1);
        let [x2, z2] = corner(2);
        let [x3, z3] = corner(3);
        let x = i64::from(x);
        let z = i64::from(z);

        let dx = x - x0;
        let dz = z - z0;
        if (x1 - x0) * dz > (z1 - z0) * dx {
            return false;
        }
        if (x3 - x0) * dz < (z3 - z0) * dx {
            return false;
        }
        let dx2 = x - x2;
        let dz2 = z - z2;
        if (x1 - x2) * dz2 < (z1 - z2) * dx2 {
            return false;
        }
        if (x3 - x2) * dz2 > (z3 - z2) * dx2 {
            return false;
        }
        true
    }
}

/// One footstep sound zone from the RDT's `.flr` table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FootstepZone {
    pub base_x: u16,
    pub base_z: u16,
    pub width: u16,
    pub height: u16,
    pub sound_data: u16,
}

/// One walkable zone used by NPC navigation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WalkZone {
    pub x1: i16,
    pub z1: i16,
    pub x2: i16,
    pub z2: i16,
    pub field_08: u16,
    pub flags: u16,
}

impl WalkZone {
    /// Half-open containment: `x in [x1, x2)` and `z in [z1, z2)`, compared
    /// with the original's wrapping unsigned-short arithmetic.
    pub fn contains(&self, x: i32, z: i32) -> bool {
        let wrap = |value: i64| value as u16;
        wrap(i64::from(x) - i64::from(self.x1)) < wrap(i64::from(self.x2) - i64::from(self.x1))
            && wrap(i64::from(z) - i64::from(self.z1))
                < wrap(i64::from(self.z2) - i64::from(self.z1))
    }
}

/// One camera background plus its camera transform.
#[derive(Debug, Default, Clone)]
pub struct Cut {
    pub index: usize,
    pub pos: [i32; 3],
    pub look_at: [i32; 3],
    pub roll: i32,
    pub fov: i32,
    pub background: Option<Image>,
    /// Direct file offset of the camera's mask sprite table; zero when absent.
    pub mask_pointer: u32,
    /// Direct file offset of the camera's embedded mask TIM; zero when absent.
    pub tim_mask_pointer: u32,
    /// Number of groups in the camera's mask table.
    pub mask_group_count: u8,
    /// One bit per one-based group id: set while the group is visible.
    pub mask_active: u32,
    /// The camera's parsed mask sprites in group order.
    pub masks: Vec<MaskSprite>,
}

/// The loaded room: its contents and the currently displayed cut.
#[derive(Debug, Default, Clone)]
pub struct RoomState {
    pub stage: u8,
    pub room: u8,
    pub player_flag: u8,
    /// Declared omodel pair count from header byte `0x02`; also the number of
    /// runtime object records the room's init script may build.
    pub omodel_slot_count: u8,
    /// Declared item-model pair count from header byte `0x03`.
    pub item_count: u8,
    pub cuts: Vec<Cut>,
    pub current_cut: usize,
    /// The live room-SFX pair index (`g_nextRoomSfxId`): 0 at boot, reloaded
    /// from every door record's sfx byte. Bank-0 `se_play_3d` resolves through
    /// this pair of [`crate::sfx::room_sfx`] entries.
    pub room_sfx: u8,
    /// Ambient light color, 12-bit per channel.
    pub ambient: [i16; 3],
    /// The room's three lights.
    pub lights: [Light; 3],
    /// Collision boundary records.
    pub collision: Collision,
    /// Camera switch zones in file order, group headers included.
    pub zones: Vec<Zone>,
    /// Walkable zones for NPC navigation.
    pub walk_zones: Vec<WalkZone>,
    /// Footstep sound zones, in file order.
    pub footstep_zones: Vec<FootstepZone>,
    /// Raw RDT message block: a `u16` offset table followed by the encoded
    /// message streams. Absent when the RDT has no message pointer.
    pub messages: Option<Vec<u8>>,
    /// The room's declared effect sprites, parsed from RDT header pointer
    /// slots 13/14/15. Empty when the room declares none.
    pub effects: crate::effects::RoomEffects,
    /// Declared omodel `{TMD, TIM}` pairs (header byte `0x02`), parsed from
    /// pointer slot 2. Malformed pairs are skipped; the source pair index is
    /// kept on each asset.
    pub object_models: Vec<crate::objects::ObjectAsset>,
    /// Declared item-model `{TMD, TIM}` pairs (header byte `0x03`), parsed
    /// from pointer slot 3. Item models stay unbuilt this milestone.
    pub item_models: Vec<crate::objects::ObjectAsset>,
    /// Non-fatal problems from parsing the embedded model pairs: one entry
    /// per malformed (never per null) pair.
    pub model_warnings: Vec<String>,
    /// The RDT-embedded room player-animation pair (pointer slots 9/10): the
    /// push/vault/ladder clips the action behaviours play. `None` when the RDT
    /// declares no pair or it fails to parse; behaviours then fall back to the
    /// no-op stance.
    pub room_anim: Option<crate::model::RoomAnim>,
}

impl RoomState {
    /// The packed footstep zone value for `(x, z)`: the surface type in the
    /// high byte and the sound offset for that surface in the low byte.
    ///
    /// The bounds test is the original's 32-bit unsigned subtraction: the
    /// sign-extended position minus the zero-extended base, compared against
    /// the width. It therefore does not wrap at 16 bits - a position below a
    /// base produces a huge value and falls through to the next zone. The scan
    /// stops after 256 entries; a room whose last zone is not a catch-all
    /// would otherwise run past the table, and returning `None` keeps that
    /// contained.
    pub fn footstep_zone(&self, x: i32, z: i32) -> Option<u16> {
        let x = x as u32;
        let z = z as u32;
        self.footstep_zones
            .iter()
            .take(256)
            .find(|zone| {
                x.wrapping_sub(u32::from(zone.base_x)) < u32::from(zone.width)
                    && z.wrapping_sub(u32::from(zone.base_z)) < u32::from(zone.height)
            })
            .map(|zone| ((zone.height >> 8) << 8) | (zone.sound_data & 0xFF))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_id_roundtrip() {
        let id = RoomId::from_room_and_player("100", 0).unwrap();
        assert_eq!(id.stage, 1);
        assert_eq!(id.room, 0x00);
        assert_eq!(id.rdt_number(), 0x1000);
        assert_eq!(id.rdt_entry(), "room/1000.rdt");
        assert_eq!(id.scd_entry(), "scd/1000.scd");
        assert_eq!(id.room3(), "100");
        assert_eq!(id.cut_entry(3), "roomcut/100_003.bmp");
        assert_eq!(id.stage_index(), 0);
        assert_eq!(id.fold_stage_digit(), 1);

        let id = RoomId::from_room_and_player("205", 3).unwrap();
        assert_eq!(id.stage, 2);
        assert_eq!(id.room, 0x05);
        assert_eq!(id.rdt_number(), 0x2053);
        assert_eq!(id.room3(), "205");
    }

    #[test]
    fn parses_four_digit_identities() {
        let id = RoomId::parse("1001").unwrap();
        assert_eq!(
            id,
            RoomId {
                stage: 1,
                room: 0x00,
                player_flag: 1
            }
        );
        assert_eq!(id.rdt_entry(), "room/1001.rdt");
        assert_eq!(id.room3(), "100");

        let id = RoomId::parse("11C0").unwrap();
        assert_eq!(
            id,
            RoomId {
                stage: 1,
                room: 0x1C,
                player_flag: 0
            }
        );
        assert_eq!(id.room3(), "11C");

        let id = RoomId::parse("71A1").unwrap();
        assert_eq!(
            id,
            RoomId {
                stage: 7,
                room: 0x1A,
                player_flag: 1
            }
        );
        assert_eq!(id.rdt_entry(), "room/71a1.rdt");
        assert_eq!(id.room3(), "71A");
    }

    #[test]
    fn every_shipped_room_id_roundtrips() {
        for stage in 1..=RoomId::MAX_STAGE {
            for room in 0..=RoomId::MAX_ROOM {
                for player in 0..=1u8 {
                    let id = RoomId {
                        stage,
                        room,
                        player_flag: player,
                    };
                    let text = format!("{:04X}", id.rdt_number());
                    assert_eq!(RoomId::parse(&text).unwrap(), id);
                    assert_eq!(
                        RoomId::from_room_and_player(&id.room3(), player).unwrap(),
                        id
                    );
                }
            }
        }
    }

    #[test]
    fn folds_return_stages() {
        let cases = [(1, 1), (5, 5), (6, 1), (7, 2)];
        for (stage, folded) in cases {
            let id = RoomId {
                stage,
                room: 0,
                player_flag: 0,
            };
            assert_eq!(id.fold_stage_digit(), folded);
        }
    }

    #[test]
    fn rejects_bad_identities() {
        assert!(RoomId::parse("").is_err());
        assert!(RoomId::parse("10012").is_err());
        assert!(RoomId::parse("0A0").is_err());
        assert!(RoomId::parse("800").is_err());
        assert!(RoomId::parse("1FF").is_err());
        assert!(RoomId::parse("1X0").is_err());
        assert!(RoomId::from_room_and_player("1001", 0).is_err());
    }

    #[test]
    fn footstep_zone_does_not_wrap_the_coordinate_space() {
        let room = RoomState {
            footstep_zones: vec![
                FootstepZone {
                    base_x: 0xFF00,
                    base_z: 0xFFF0,
                    width: 0x0200,
                    height: 0x0020,
                    sound_data: 0x2D45,
                },
                FootstepZone {
                    base_x: 0,
                    base_z: 0,
                    width: 0x7FE4,
                    height: 0x7FEC,
                    sound_data: 0,
                },
            ],
            ..RoomState::default()
        };

        // A position inside the first zone matches it; the packed result keeps
        // the height's high byte and the low sound byte.
        assert_eq!(room.footstep_zone(0xFF00, 0xFFF0), Some(0x45));
        assert_eq!(room.footstep_zone(0xFF10, 0xFFF5), Some(0x45));
        // 0x0010 - 0xFF00 is a huge unsigned 32-bit value, not a 16-bit wrap
        // back to 0x0110, so the catch-all zone wins instead.
        assert_eq!(room.footstep_zone(0x10, 0x08), Some(0x7F00));
        assert_eq!(room.footstep_zone(0, 0), Some(0x7F00));
        assert_eq!(room.footstep_zone(0x1000, 0x1000), Some(0x7F00));
        assert_eq!(room.footstep_zone(0x7FE3, 0x7FEB), Some(0x7F00));
    }

    #[test]
    fn footstep_zone_stops_after_256_entries() {
        let room = RoomState {
            footstep_zones: (0..300)
                .map(|index| FootstepZone {
                    base_x: 1000 + index,
                    base_z: 1000 + index,
                    width: 1,
                    height: 1,
                    sound_data: index,
                })
                .collect(),
            ..RoomState::default()
        };

        assert_eq!(room.footstep_zone(1000, 1000), Some(0));
        assert_eq!(room.footstep_zone(1255, 1255), Some(255));
        assert_eq!(room.footstep_zone(1299, 1299), None);
        assert_eq!(RoomState::default().footstep_zone(0, 0), None);
    }
}
