//! Game and room state.
//!
//! These are the engine's internal state after an RDT has been loaded. Source
//! file formats are not kept around during gameplay.

/// A stage/room/player identity, e.g. room 100 player 0 -> RDT 1000.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RoomId {
    /// 1-based stage number.
    pub stage: u8,
    /// Room number within the stage (0-99).
    pub room: u8,
    /// Player/flag digit (0-9).
    pub player_flag: u8,
}

impl RoomId {
    /// `room` is the three-digit room id, e.g. 100.
    pub fn from_room_and_player(room: u32, player: u8) -> Self {
        Self {
            stage: (room / 100) as u8,
            room: (room % 100) as u8,
            player_flag: player,
        }
    }

    /// Four-digit RDT number, e.g. 1000.
    pub fn rdt_number(self) -> u32 {
        (self.stage as u32 * 100 + self.room as u32) * 10 + self.player_flag as u32
    }

    /// Three-digit room id, e.g. 100.
    pub fn room_number(self) -> u32 {
        self.stage as u32 * 100 + self.room as u32
    }
}

/// Raw RGBA8 image.
#[derive(Debug, Clone, Default)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
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
}

/// The loaded room: its cuts and the currently displayed one.
#[derive(Debug, Default, Clone)]
pub struct RoomState {
    pub stage: u8,
    pub room: u8,
    pub player_flag: u8,
    pub cuts: Vec<Cut>,
    pub current_cut: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_id_roundtrip() {
        let id = RoomId::from_room_and_player(100, 0);
        assert_eq!(id.stage, 1);
        assert_eq!(id.room, 0);
        assert_eq!(id.rdt_number(), 1000);
        assert_eq!(id.room_number(), 100);

        let id = RoomId::from_room_and_player(205, 3);
        assert_eq!(id.stage, 2);
        assert_eq!(id.room, 5);
        assert_eq!(id.rdt_number(), 2053);
        assert_eq!(id.room_number(), 205);
    }
}
