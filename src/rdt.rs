//! RDT room parser.
//!
//! Reads the camera cut table from an RDT file's header.

use anyhow::{Result, bail};

use crate::state::{Cut, RoomId, RoomState};

/// Offset of the camera records within an RDT file.
const CAMERAS_OFFSET: usize = 0x94;

/// Number of little-endian `i32` fields in one camera record.
const CAMERA_FIELDS: usize = 11;

/// Size in bytes of one camera record.
const CAMERA_SIZE: usize = CAMERA_FIELDS * 4;

/// Parse an RDT file into internal room state.
pub fn parse(data: &[u8], id: RoomId) -> Result<RoomState> {
    let Some(&cameras_count) = data.get(0x01) else {
        bail!(
            "RDT is too short for a camera count: need at least 2 bytes, got {}",
            data.len()
        );
    };

    if cameras_count == 0 {
        return Ok(empty_room(id));
    }

    let required = CAMERAS_OFFSET + cameras_count as usize * CAMERA_SIZE;
    if data.len() < required {
        bail!(
            "RDT is truncated for {cameras_count} camera(s): need {required} bytes, got {}",
            data.len()
        );
    }

    let cuts = (0..cameras_count as usize)
        .map(|index| {
            let start = CAMERAS_OFFSET + index * CAMERA_SIZE;
            parse_cut(&data[start..start + CAMERA_SIZE], index)
        })
        .collect();

    Ok(RoomState {
        stage: id.stage,
        room: id.room,
        player_flag: id.player_flag,
        cuts,
        current_cut: 0,
    })
}

/// Build the state of a room without any camera cuts.
fn empty_room(id: RoomId) -> RoomState {
    RoomState {
        stage: id.stage,
        room: id.room,
        player_flag: id.player_flag,
        cuts: Vec::new(),
        current_cut: 0,
    }
}

/// Parse one 44-byte camera record.
fn parse_cut(record: &[u8], index: usize) -> Cut {
    let mut fields = [0i32; CAMERA_FIELDS];
    for (field, bytes) in fields.iter_mut().zip(record.as_chunks::<4>().0) {
        *field = i32::from_le_bytes(*bytes);
    }

    Cut {
        index,
        pos: [fields[2], fields[3], fields[4]],
        look_at: [fields[5], fields[6], fields[7]],
        roll: fields[8],
        fov: fields[10],
        background: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOM_ID: RoomId = RoomId {
        stage: 2,
        room: 5,
        player_flag: 3,
    };

    fn encode_record(fields: [i32; CAMERA_FIELDS]) -> Vec<u8> {
        fields
            .iter()
            .flat_map(|field| field.to_le_bytes())
            .collect()
    }

    fn build_rdt(cameras_count: u8, records: &[[i32; CAMERA_FIELDS]]) -> Vec<u8> {
        let mut data = vec![0u8; CAMERAS_OFFSET];
        data[0x01] = cameras_count;
        for fields in records {
            data.extend_from_slice(&encode_record(*fields));
        }
        data
    }

    #[test]
    fn parses_two_camera_records() {
        let first = [11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21];
        let second = [-11, -12, -13, -14, -15, -16, -17, -18, -19, -20, -21];
        let data = build_rdt(2, &[first, second]);

        let state = parse(&data, ROOM_ID).unwrap();

        assert_eq!(state.stage, 2);
        assert_eq!(state.room, 5);
        assert_eq!(state.player_flag, 3);
        assert_eq!(state.current_cut, 0);
        assert_eq!(state.cuts.len(), 2);

        let cut = &state.cuts[0];
        assert_eq!(cut.index, 0);
        assert_eq!(cut.pos, [13, 14, 15]);
        assert_eq!(cut.look_at, [16, 17, 18]);
        assert_eq!(cut.roll, 19);
        assert_eq!(cut.fov, 21);
        assert!(cut.background.is_none());

        let cut = &state.cuts[1];
        assert_eq!(cut.index, 1);
        assert_eq!(cut.pos, [-13, -14, -15]);
        assert_eq!(cut.look_at, [-16, -17, -18]);
        assert_eq!(cut.roll, -19);
        assert_eq!(cut.fov, -21);
        assert!(cut.background.is_none());
    }

    #[test]
    fn parses_zero_cameras() {
        let data = build_rdt(0, &[]);

        let state = parse(&data, ROOM_ID).unwrap();

        assert!(state.cuts.is_empty());
        assert_eq!(state.current_cut, 0);
        assert_eq!(state.stage, 2);
        assert_eq!(state.room, 5);
        assert_eq!(state.player_flag, 3);
    }

    #[test]
    fn minimal_input_with_zero_cameras_works() {
        let state = parse(&[0x00, 0x00], ROOM_ID).unwrap();

        assert!(state.cuts.is_empty());
        assert_eq!(state.current_cut, 0);
    }

    #[test]
    fn errors_when_header_is_too_short() {
        assert!(parse(&[], ROOM_ID).is_err());
        assert!(parse(&[0x02], ROOM_ID).is_err());
    }

    #[test]
    fn errors_when_records_are_truncated() {
        let mut data = build_rdt(1, &[[0; CAMERA_FIELDS]]);
        data.truncate(CAMERAS_OFFSET + CAMERA_SIZE - 1);

        let message = parse(&data, ROOM_ID).unwrap_err().to_string();

        assert!(message.contains(&(CAMERAS_OFFSET + CAMERA_SIZE).to_string()));
        assert!(message.contains(&(CAMERAS_OFFSET + CAMERA_SIZE - 1).to_string()));
    }

    #[test]
    fn errors_when_camera_table_is_missing() {
        let data = build_rdt(0, &[]);
        let mut data = data;
        data[0x01] = 1;

        assert!(parse(&data, ROOM_ID).is_err());
    }
}
