//! In-room stairs: the `stairs_height_update` and `set_stairs_zone` room
//! actions.
//!
//! Room scripts register stair zones with `aot_set`: handler `0x11`
//! (`stairs_height_update`) ramps the player's height across a rectangular
//! zone, and handler `0x0C` (`set_stairs_zone`) marks a stair/ladder entry and
//! latches the ladder base. This module owns the zone records and the pure
//! height math; [`crate::game`] wires them to the SCD host and
//! [`crate::player`] applies the height and the collision suspension.

use crate::game::{ROOM_ACTION_SLOTS, RoomAction, RoomActionKind};

/// One stair zone collected from the live room action table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StairZone {
    /// `stairs_height_update` (handler `0x11`): the player's height ramps
    /// across the zone from the edge selected by `edge`.
    Height {
        /// Room action slot that registered the zone.
        slot: u8,
        /// Interaction box: `x`, `z`, `width`, `depth`.
        zone: [i16; 4],
        /// Probe flags byte.
        flags: u8,
        /// Edge the height ramps from: `0` low X, `1` high X, `2` low Z,
        /// `3` high Z.
        edge: u16,
        /// Zone length in room units; divides the distance from the edge.
        length: u16,
        /// Height added per step; signed, so a zone can descend.
        step: i16,
    },
    /// `set_stairs_zone` (handler `0x0C`): a stair/ladder entry.
    Entry {
        /// Room action slot that registered the zone.
        slot: u8,
        /// Interaction box: `x`, `z`, `width`, `depth`.
        zone: [i16; 4],
        /// Probe flags byte.
        flags: u8,
        /// Ladder variant word; non-zero raises the ladder zone flag.
        variant: u16,
        /// Latched ladder base X.
        base_x: u16,
        /// Latched ladder base Z.
        base_z: u16,
    },
}

impl StairZone {
    /// Build a zone from a room action, or `None` for a non-stair action.
    pub fn from_action(action: &RoomAction) -> Option<Self> {
        let slot = action.slot;
        let zone = action.zone;
        let flags = action.flags;
        match action.kind {
            RoomActionKind::StairsHeight => Some(Self::Height {
                slot,
                zone,
                flags,
                edge: action.param_word(0),
                length: action.param_word(1),
                step: action.param_word(2) as i16,
            }),
            RoomActionKind::StairsZone => Some(Self::Entry {
                slot,
                zone,
                flags,
                variant: action.param_word(0),
                base_x: action.param_word(1),
                base_z: action.param_word(2),
            }),
            _ => None,
        }
    }

    /// The room action slot the zone came from.
    pub fn slot(&self) -> u8 {
        match *self {
            Self::Height { slot, .. } | Self::Entry { slot, .. } => slot,
        }
    }

    /// The interaction box.
    pub fn zone(&self) -> [i16; 4] {
        match *self {
            Self::Height { zone, .. } | Self::Entry { zone, .. } => zone,
        }
    }

    /// The original's unsigned box test: coordinates left of or above the box
    /// wrap to a huge value and fail, so the box only extends towards +X/+Z.
    pub fn contains(&self, x: i32, z: i32) -> bool {
        let [origin_x, origin_z, width, depth] = self.zone();
        (x as u32).wrapping_sub(u32::from(origin_x as u16)) <= u32::from(width as u16)
            && (z as u32).wrapping_sub(u32::from(origin_z as u16)) <= u32::from(depth as u16)
    }
}

/// The height a `stairs_height_update` zone holds at `(x, z)`.
///
/// The original picks one of the four edges, measures the distance into the
/// zone, divides it by the zone length and adds one step, then scales by the
/// signed step. A zero length would divide by zero in the original; it is
/// treated as one here so a malformed record cannot panic.
pub fn ramp_height(zone: &StairZone, x: i32, z: i32) -> i32 {
    let StairZone::Height {
        zone: [zx, zz, zw, zd],
        edge,
        length,
        step,
        ..
    } = *zone
    else {
        return 0;
    };
    let local = match edge {
        0 => x - i32::from(zx),
        1 => i32::from(zx) + i32::from(zw) - x,
        2 => z - i32::from(zz),
        _ => i32::from(zz) + i32::from(zd) - z,
    };
    let length = i32::from(length).max(1);
    // The original truncates the quotient to a signed 16-bit word before the
    // +1 and the step multiply.
    let steps = i32::from((local / length) as i16);
    (steps + 1) * i32::from(step)
}

/// What the last `set_stairs_zone` (or stair-door) probe latched onto the
/// player entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StairEntryState {
    /// Room action slot that fired.
    pub slot: u8,
    /// The entry's variant word was non-zero (the ladder zone flag `0x10`).
    pub ladder: bool,
    /// Latched ladder base.
    pub base_x: u16,
    /// Latched ladder base Z.
    pub base_z: u16,
    /// Point the climb behaviour turns towards: a ladder entry's base, or a
    /// stair door's zone centre.
    pub target_x: i32,
    /// Climb target Z.
    pub target_z: i32,
}

/// The stair zones registered by the current room action table, in slot order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StairZones {
    zones: Vec<StairZone>,
}

impl StairZones {
    /// Collect the stair zones from a room action table.
    pub fn collect(actions: &[Option<RoomAction>; ROOM_ACTION_SLOTS]) -> Self {
        let zones = actions
            .iter()
            .flatten()
            .filter_map(StairZone::from_action)
            .collect();
        Self { zones }
    }

    /// The zones in slot order.
    pub fn zones(&self) -> &[StairZone] {
        &self.zones
    }

    /// Whether no stair zone is registered.
    pub fn is_empty(&self) -> bool {
        self.zones.is_empty()
    }

    /// The last height zone containing `(x, z)`, matching the original's probe
    /// order (every matching entry fires, so a later slot overwrites the
    /// height an earlier one set).
    pub fn height_at(&self, x: i32, z: i32) -> Option<&StairZone> {
        self.zones
            .iter()
            .rev()
            .find(|zone| matches!(zone, StairZone::Height { .. }) && zone.contains(x, z))
    }

    /// The last entry zone containing `(x, z)`.
    pub fn entry_at(&self, x: i32, z: i32) -> Option<&StairZone> {
        self.zones
            .iter()
            .rev()
            .find(|zone| matches!(zone, StairZone::Entry { .. }) && zone.contains(x, z))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(kind: RoomActionKind, zone: [i16; 4], words: [u16; 3]) -> RoomAction {
        RoomAction {
            slot: 3,
            kind,
            zone,
            sce: 0x11,
            handler: 0x11,
            flags: 0x41,
            params: [
                0,
                0,
                words[0] as u8,
                (words[0] >> 8) as u8,
                words[1] as u8,
                (words[1] >> 8) as u8,
                words[2] as u8,
                (words[2] >> 8) as u8,
            ],
            item_data: None,
            room_items_flag: 0xFF,
            reach_animation: false,
        }
    }

    fn height_zone(edge: u16, length: u16, step: i16) -> StairZone {
        StairZone::from_action(&action(
            RoomActionKind::StairsHeight,
            [100, 200, 1000, 800],
            [edge, length, step as u16],
        ))
        .unwrap()
    }

    #[test]
    fn ramp_follows_the_selected_edge_and_step() {
        // Low X edge: height grows with X.
        let zone = height_zone(0, 100, 36);
        assert_eq!(ramp_height(&zone, 100, 0), 36);
        assert_eq!(ramp_height(&zone, 200, 0), (100 / 100 + 1) * 36);
        assert_eq!(ramp_height(&zone, 1100, 0), (1000 / 100 + 1) * 36);

        // High X edge: height grows towards low X.
        let zone = height_zone(1, 100, 36);
        assert_eq!(ramp_height(&zone, 1100, 0), 36);
        assert_eq!(ramp_height(&zone, 100, 0), 11 * 36);

        // Low Z edge.
        let zone = height_zone(2, 100, 36);
        assert_eq!(ramp_height(&zone, 0, 200), 36);
        assert_eq!(ramp_height(&zone, 0, 1000), 9 * 36);

        // High Z edge, and any edge value past 3 folds onto it.
        let zone = height_zone(3, 100, 36);
        assert_eq!(ramp_height(&zone, 0, 1000), 36);
        assert_eq!(ramp_height(&zone, 0, 200), 9 * 36);
        let folded = height_zone(9, 100, 36);
        assert_eq!(ramp_height(&folded, 0, 200), ramp_height(&zone, 0, 200));
    }

    #[test]
    fn ramp_honours_a_descending_step_and_truncates_like_c() {
        let zone = height_zone(0, 175, -36);
        assert_eq!(ramp_height(&zone, 100, 0), -36);
        assert_eq!(ramp_height(&zone, 100 + 174, 0), -36);
        assert_eq!(ramp_height(&zone, 100 + 175, 0), -72);
        // Zero length would divide by zero; guarded to one.
        let zone = height_zone(0, 0, 10);
        assert_eq!(ramp_height(&zone, 150, 0), 51 * 10);
    }

    #[test]
    fn store_lookup_prefers_the_last_matching_slot() {
        let mut first = action(RoomActionKind::StairsHeight, [0, 0, 100, 100], [0, 100, 10]);
        first.slot = 1;
        let mut second = action(RoomActionKind::StairsHeight, [0, 0, 100, 100], [0, 100, 99]);
        second.slot = 5;
        let mut table: [Option<RoomAction>; ROOM_ACTION_SLOTS] = [None; ROOM_ACTION_SLOTS];
        table[1] = Some(first);
        table[5] = Some(second);
        let zones = StairZones::collect(&table);
        assert_eq!(zones.zones().len(), 2);
        let found = zones.height_at(50, 50).unwrap();
        assert_eq!(found.slot(), 5);
        assert!(zones.height_at(101, 50).is_none());
        assert!(zones.height_at(-1, 50).is_none());

        // The unsigned box test accepts a coordinate that wraps into range.
        assert!(zones.height_at(0x1_0000, 50).is_none());
    }

    #[test]
    fn store_separates_height_and_entry_zones() {
        let mut height = action(RoomActionKind::StairsHeight, [0, 0, 100, 100], [0, 100, 10]);
        height.slot = 2;
        let mut entry = action(RoomActionKind::StairsZone, [0, 0, 100, 100], [1, 500, 600]);
        entry.slot = 4;
        entry.kind = RoomActionKind::StairsZone;
        let mut table: [Option<RoomAction>; ROOM_ACTION_SLOTS] = [None; ROOM_ACTION_SLOTS];
        table[2] = Some(height);
        table[4] = Some(entry);
        let zones = StairZones::collect(&table);
        assert_eq!(zones.zones().len(), 2);
        assert_eq!(zones.height_at(50, 50).unwrap().slot(), 2);
        assert_eq!(zones.entry_at(50, 50).unwrap().slot(), 4);
        match zones.entry_at(50, 50).unwrap() {
            StairZone::Entry {
                variant,
                base_x,
                base_z,
                ..
            } => {
                assert_eq!(*variant, 1);
                assert_eq!(*base_x, 500);
                assert_eq!(*base_z, 600);
            }
            _ => panic!("expected an entry zone"),
        }
    }
}
