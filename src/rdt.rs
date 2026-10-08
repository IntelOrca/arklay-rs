//! RDT room parser.
//!
//! Reads the header pointers of an RDT file and everything gameplay needs:
//! camera cuts, ambient and point lights, collision boundaries, camera switch
//! zones, walkable zones and the room's declared effect sprites (pointer slots
//! 13/14/15).

use anyhow::{Context, Result, bail};

use crate::budget;
use crate::mask;
pub use crate::state::{Collision, CollisionRect, FootstepZone, Light, WalkZone, Zone};
use crate::state::{Cut, RoomId, RoomState};

/// Offset of the ambient light color within an RDT file.
const AMBIENT_OFFSET: usize = 0x06;
/// Offset of the three room light records within an RDT file.
const LIGHTS_OFFSET: usize = 0x0C;
/// Number of room light records.
const LIGHT_COUNT: usize = 3;
/// Size in bytes of one room light record.
const LIGHT_SIZE: usize = 0x14;
/// Offset of the data pointer table within an RDT file.
const POINTERS_OFFSET: usize = 0x48;
/// Number of data pointers in the header.
const POINTER_COUNT: usize = 19;
/// Pointer slot of the camera switch zone table.
const CAMERA_ZONES_SLOT: usize = 0;
/// Pointer slot of the collision boundary table.
const COLLISION_SLOT: usize = 1;
/// Pointer slot of the omodel `{TMD*, TIM*}` pair table.
const OBJECT_MODELS_SLOT: usize = 2;
/// Pointer slot of the item-model `{TMD*, TIM*}` pair table (`0x58`).
const ITEM_MODELS_SLOT: usize = 3;
/// Pointer slot of the walkable zone table.
const WALK_ZONES_SLOT: usize = 4;
/// Pointer slot of the footstep sound zone table.
const FOOTSTEP_SLOT: usize = 5;
/// Pointer slot of the room player-animation header (EMR, `RDT+0x6C`).
const PLAYER_ANIM_HEADER_SLOT: usize = 9;
/// Pointer slot of the room player-animation base (EDD, `RDT+0x70`).
const PLAYER_ANIM_BASE_SLOT: usize = 10;
/// Pointer slot of the room message block (`RDT+0x74`).
const MESSAGE_SLOT: usize = 11;
/// Pointer slot of the effect sprite index table (`RDT+0x7C`).
pub const EFFECT_INDEX_SLOT: usize = 13;
/// Pointer slot of the effect sprite-info offsets (`RDT+0x80`).
pub const EFFECT_INFO_SLOT: usize = 14;
/// Pointer slot of the effect sprite TIM offsets (`RDT+0x84`).
pub const EFFECT_TIM_SLOT: usize = 15;
/// Offset of the camera records within an RDT file.
const CAMERAS_OFFSET: usize = 0x94;
/// Number of little-endian `i32` fields in one camera record.
const CAMERA_FIELDS: usize = 11;
/// Size in bytes of one camera record.
const CAMERA_SIZE: usize = CAMERA_FIELDS * 4;
/// Size in bytes of one camera switch zone record.
const ZONE_SIZE: usize = 0x14;
/// Size in bytes of the collision boundary header.
const COLLISION_HEADER_SIZE: usize = 0x18;
/// Size in bytes of one collision boundary record.
const COLLISION_RECORD_SIZE: usize = 0xC;
/// Size in bytes of one walkable zone record.
const WALK_ZONE_SIZE: usize = 0xC;
/// Size in bytes of the footstep zone table header.
const FOOTSTEP_HEADER_SIZE: usize = 2;
/// Size in bytes of one footstep sound zone record.
const FOOTSTEP_ZONE_SIZE: usize = 10;

/// Parse an RDT file into internal room state.
pub fn parse(data: &[u8], id: RoomId) -> Result<RoomState> {
    let Some(&cameras_count) = data.get(0x01) else {
        bail!(
            "RDT is too short for a camera count: need at least 2 bytes, got {}",
            data.len()
        );
    };
    let omodel_slot_count = data.get(0x02).copied().unwrap_or(0);
    let item_count = data.get(0x03).copied().unwrap_or(0);

    let cuts = parse_cameras(data, cameras_count)?;
    let ambient = parse_ambient(data)?;
    let lights = parse_lights(data)?;
    let pointers = read_pointers(data);
    let collision = parse_collision(data, pointers[COLLISION_SLOT])?;
    let zones = parse_zones(data, pointers[CAMERA_ZONES_SLOT])?;
    let walk_zones = parse_walk_zones(data, pointers[WALK_ZONES_SLOT])?;
    let footstep_zones =
        parse_footstep_zones(data, pointers[FOOTSTEP_SLOT], pointers[FOOTSTEP_SLOT + 1])?;
    let messages = parse_messages(data, &pointers)?;
    let effects = crate::effects::RoomEffects::parse(
        data,
        pointers[EFFECT_INDEX_SLOT],
        pointers[EFFECT_INFO_SLOT],
        pointers[EFFECT_TIM_SLOT],
    );
    // The embedded model pairs are tolerant like the effect tables: a
    // malformed pair is skipped with a warning, a null half is simply
    // unbuilt, and a damaged model never fails the room.
    let (object_models, mut model_warnings) =
        crate::objects::parse_assets(data, pointers[OBJECT_MODELS_SLOT], omodel_slot_count);
    let (item_models, item_warnings) =
        crate::objects::parse_assets(data, pointers[ITEM_MODELS_SLOT], item_count);
    model_warnings.extend(item_warnings);
    // The room's player-animation pair is tolerant too: a room without one, or
    // with a damaged one, plays the no-op stance instead of failing the load.
    let room_anim = parse_room_anim(data, &pointers);

    Ok(RoomState {
        stage: id.stage,
        room: id.room,
        player_flag: id.player_flag,
        omodel_slot_count,
        item_count,
        cuts,
        current_cut: 0,
        // A freshly parsed room boots with the wooden-door pair; a door
        // transition reloads it from the record's sfx byte.
        room_sfx: 0,
        ambient,
        lights,
        collision,
        zones,
        walk_zones,
        footstep_zones,
        messages,
        effects,
        object_models,
        item_models,
        model_warnings,
        room_anim,
    })
}

/// Parse the RDT's player-animation pair against its next-section bound.
///
/// Returns `None` when either pointer is null or the pair fails to parse; the
/// bound is the first declared section pointer after the animation base, or the
/// end of the file when none follows.
fn parse_room_anim(data: &[u8], pointers: &[u32; POINTER_COUNT]) -> Option<crate::model::RoomAnim> {
    let header = pointers[PLAYER_ANIM_HEADER_SLOT];
    let base = pointers[PLAYER_ANIM_BASE_SLOT];
    if header == 0 || base == 0 {
        return None;
    }
    let bound = pointers
        .iter()
        .skip(PLAYER_ANIM_BASE_SLOT + 1)
        .copied()
        .filter(|pointer| *pointer > base)
        .min()
        .unwrap_or(data.len() as u32);
    match crate::emd::parse_room_anim(data, header, base, bound) {
        Ok(anim) => Some(anim),
        Err(error) => {
            eprintln!("warning: invalid room player animation pair: {error:#}");
            None
        }
    }
}

/// Parse the camera records, one per cut.
fn parse_cameras(data: &[u8], cameras_count: u8) -> Result<Vec<Cut>> {
    if cameras_count == 0 {
        return Ok(Vec::new());
    }

    let required = CAMERAS_OFFSET + usize::from(cameras_count) * CAMERA_SIZE;
    if data.len() < required {
        bail!(
            "RDT is truncated for {cameras_count} camera(s): need {required} bytes, got {}",
            data.len()
        );
    }

    (0..usize::from(cameras_count))
        .map(|index| parse_cut(data, CAMERAS_OFFSET + index * CAMERA_SIZE, index))
        .collect()
}

/// Parse the ambient light color, defaulting to black when absent.
fn parse_ambient(data: &[u8]) -> Result<[i16; 3]> {
    if data.len() < AMBIENT_OFFSET + 6 {
        return Ok([0; 3]);
    }
    Ok([
        i16_at(data, AMBIENT_OFFSET)?,
        i16_at(data, AMBIENT_OFFSET + 2)?,
        i16_at(data, AMBIENT_OFFSET + 4)?,
    ])
}

/// Parse the three room light records, defaulting to dark when absent.
fn parse_lights(data: &[u8]) -> Result<[Light; LIGHT_COUNT]> {
    if data.len() < LIGHTS_OFFSET + LIGHT_COUNT * LIGHT_SIZE {
        return Ok([Light::default(); LIGHT_COUNT]);
    }

    let mut lights = [Light::default(); LIGHT_COUNT];
    for (index, light) in lights.iter_mut().enumerate() {
        *light = parse_light(data, LIGHTS_OFFSET + index * LIGHT_SIZE)?;
    }
    Ok(lights)
}

/// Parse one room light record.
fn parse_light(data: &[u8], offset: usize) -> Result<Light> {
    let color = bytes_at(data, offset + 0x0C, 3)?;
    Ok(Light {
        pos: [
            i32_at(data, offset)?,
            i32_at(data, offset + 4)?,
            i32_at(data, offset + 8)?,
        ],
        color: [color[0], color[1], color[2]],
        kind: u16_at(data, offset + 0x10)?,
        radius: u16_at(data, offset + 0x12)?,
    })
}

/// Read the data pointer table. A truncated table reads as all-null pointers.
fn read_pointers(data: &[u8]) -> [u32; POINTER_COUNT] {
    if data.len() < POINTERS_OFFSET + POINTER_COUNT * 4 {
        return [0; POINTER_COUNT];
    }

    let mut pointers = [0u32; POINTER_COUNT];
    for (index, pointer) in pointers.iter_mut().enumerate() {
        let Ok(value) = u32_at(data, POINTERS_OFFSET + index * 4) else {
            return [0; POINTER_COUNT];
        };
        *pointer = value;
    }
    pointers
}

/// Parse the collision boundary table at `pointer`, if it has one.
fn parse_collision(data: &[u8], pointer: u32) -> Result<Collision> {
    if pointer == 0 {
        return Ok(Collision::default());
    }

    let base = pointer as usize;
    base.checked_add(COLLISION_HEADER_SIZE)
        .filter(|&end| end <= data.len())
        .with_context(|| {
            format!(
                "collision pointer 0x{pointer:x} is out of bounds for the {}-byte RDT",
                data.len()
            )
        })?;

    let cell_x = i16_at(data, base)?;
    let cell_z = i16_at(data, base + 2)?;
    let mut counts = [0i32; 5];
    for (index, count) in counts.iter_mut().enumerate() {
        *count = i32_at(data, base + 4 + index * 4)?;
    }

    let mut total = 0usize;
    for (quadrant, &count) in counts[..4].iter().enumerate() {
        if count < 0 {
            bail!("collision quadrant {quadrant} has negative record count {count}");
        }
        total = total
            .checked_add(count as usize)
            .context("collision record count overflows")?;
    }
    budget::check_len(total, budget::MAX_RECORDS, "collision record count")?;

    let records_start = base + COLLISION_HEADER_SIZE;
    let records_end = total
        .checked_mul(COLLISION_RECORD_SIZE)
        .and_then(|bytes| records_start.checked_add(bytes))
        .context("collision record table overflows")?;
    if records_end > data.len() {
        bail!(
            "collision table at 0x{base:x} with {total} record(s) overruns the {}-byte RDT",
            data.len()
        );
    }

    let mut quadrants: [Vec<CollisionRect>; 4] = std::array::from_fn(|_| Vec::new());
    let mut offset = records_start;
    for (quadrant, &count) in quadrants.iter_mut().zip(counts[..4].iter()) {
        for _ in 0..count {
            quadrant.push(parse_collision_rect(data, offset)?);
            offset += COLLISION_RECORD_SIZE;
        }
    }

    Ok(Collision {
        cell_x,
        cell_z,
        quadrants,
    })
}

/// Parse one collision boundary record.
fn parse_collision_rect(data: &[u8], offset: usize) -> Result<CollisionRect> {
    Ok(CollisionRect {
        x_max: u16_at(data, offset)?,
        z_max: u16_at(data, offset + 2)?,
        x_min: u16_at(data, offset + 4)?,
        z_min: u16_at(data, offset + 6)?,
        kind: u16_at(data, offset + 8)?,
        flags: u16_at(data, offset + 10)?,
    })
}

/// Parse the camera switch zone table at `pointer`, if it has one.
///
/// The table has no count: records are read in file order until one whose
/// `cam_from` is not a camera id (the trailing `0xFFFFFFFF` sentinel).
fn parse_zones(data: &[u8], pointer: u32) -> Result<Vec<Zone>> {
    if pointer == 0 {
        return Ok(Vec::new());
    }

    let base = pointer as usize;
    if base >= data.len() {
        bail!(
            "camera switch zone pointer 0x{pointer:x} is out of bounds for the {}-byte RDT",
            data.len()
        );
    }

    let mut zones = Vec::new();
    let mut offset = base;
    while offset + ZONE_SIZE <= data.len() {
        let cam_to = i16_at(data, offset)?;
        let cam_from = i16_at(data, offset + 2)?;
        if !(0..=7).contains(&cam_from) {
            break;
        }

        let mut corners = [[0i16; 2]; 4];
        for (index, corner) in corners.iter_mut().enumerate() {
            corner[0] = i16_at(data, offset + 4 + index * 4)?;
            corner[1] = i16_at(data, offset + 6 + index * 4)?;
        }
        budget::check_len(
            zones.len() + 1,
            budget::MAX_RECORDS,
            "camera switch zone count",
        )?;
        zones.push(Zone {
            cam_to,
            cam_from,
            corners,
        });
        offset += ZONE_SIZE;
    }
    Ok(zones)
}

/// Parse the walkable zone table at `pointer`, if it has one.
fn parse_walk_zones(data: &[u8], pointer: u32) -> Result<Vec<WalkZone>> {
    if pointer == 0 {
        return Ok(Vec::new());
    }

    let base = pointer as usize;
    let start = base
        .checked_add(2)
        .filter(|&end| end <= data.len())
        .with_context(|| {
            format!(
                "walk zone pointer 0x{pointer:x} is out of bounds for the {}-byte RDT",
                data.len()
            )
        })?;
    let count = usize::from(data[base]);
    let required = start
        .checked_add(count * WALK_ZONE_SIZE)
        .context("walk zone table size overflows")?;
    if required > data.len() {
        bail!(
            "walk zone table at 0x{base:x} with {count} zone(s) overruns the {}-byte RDT",
            data.len()
        );
    }

    let mut walk_zones = Vec::with_capacity(count);
    for index in 0..count {
        let offset = start + index * WALK_ZONE_SIZE;
        walk_zones.push(WalkZone {
            x1: i16_at(data, offset)?,
            z1: i16_at(data, offset + 2)?,
            x2: i16_at(data, offset + 4)?,
            z2: i16_at(data, offset + 6)?,
            field_08: u16_at(data, offset + 8)?,
            flags: u16_at(data, offset + 10)?,
        });
    }
    Ok(walk_zones)
}

/// Parse the footstep sound zone table at `pointer`, if it has one.
///
/// The table starts with a `u16` header and has no terminator, so its length
/// comes from `next`, the pointer of the following section. When that pointer
/// is missing or not after the table, the remainder of the file is used.
fn parse_footstep_zones(data: &[u8], pointer: u32, next: u32) -> Result<Vec<FootstepZone>> {
    if pointer == 0 {
        return Ok(Vec::new());
    }

    let base = pointer as usize;
    let start = base
        .checked_add(FOOTSTEP_HEADER_SIZE)
        .filter(|&end| end <= data.len())
        .with_context(|| {
            format!(
                "footstep zone pointer 0x{pointer:x} is out of bounds for the {}-byte RDT",
                data.len()
            )
        })?;

    let next = next as usize;
    let end = if next > start && next <= data.len() {
        next
    } else {
        data.len()
    };

    let count = (end - start) / FOOTSTEP_ZONE_SIZE;
    budget::check_len(count, budget::MAX_RECORDS, "footstep zone count")?;
    let mut zones = Vec::with_capacity(count);
    for index in 0..count {
        let offset = start + index * FOOTSTEP_ZONE_SIZE;
        zones.push(FootstepZone {
            base_x: u16_at(data, offset)?,
            base_z: u16_at(data, offset + 2)?,
            width: u16_at(data, offset + 4)?,
            height: u16_at(data, offset + 6)?,
            sound_data: u16_at(data, offset + 8)?,
        });
    }
    Ok(zones)
}

/// Parse the room message block at the `RDT+0x74` pointer, if the RDT has one.
///
/// The block is a `u16` offset table followed by the encoded message streams;
/// it has no explicit length, so it ends at the next section pointer that
/// follows it (or at the end of the file). Message ids are looked up in
/// [`RoomState::message`], which the original indexes unchecked; this parser
/// keeps the whole block and the lookup bounds-checks it.
fn parse_messages(data: &[u8], pointers: &[u32; POINTER_COUNT]) -> Result<Option<Vec<u8>>> {
    let pointer = pointers[MESSAGE_SLOT];
    if pointer == 0 {
        return Ok(None);
    }

    let base = pointer as usize;
    if base >= data.len() {
        bail!(
            "message pointer 0x{pointer:x} is out of bounds for the {}-byte RDT",
            data.len()
        );
    }

    let end = pointers
        .iter()
        .map(|&value| value as usize)
        .filter(|&value| value > base && value <= data.len())
        .min()
        .unwrap_or(data.len());
    let len = end - base;
    budget::check_len(len, budget::MAX_DECODE_ALLOC, "room message block size")?;
    let mut block = budget::alloc::<u8>(len, "room message block")?;
    block.extend_from_slice(&data[base..end]);

    Ok(Some(block))
}

impl RoomState {
    /// The encoded bytes of room message `id`, masking the id to the low six
    /// bits exactly like the original's `msg_id & 0x3F`.
    ///
    /// The slice runs to the end of the message block: the message state
    /// machine stops at the `0x01` terminator and reads the action byte after
    /// it. Bit `0x40` selects the global table instead of the room table; call
    /// [`crate::text::Text::message`] to resolve both cases.
    pub fn message(&self, id: u16) -> Option<&[u8]> {
        let block = self.messages.as_deref()?;
        let index = usize::from(id & 0x3F) * 2;
        let raw = block.get(index..index + 2)?;
        let offset = usize::from(u16::from_le_bytes([raw[0], raw[1]]));
        if offset == 0 || offset > block.len() {
            return None;
        }
        block.get(offset..)
    }
}

/// Parse one 44-byte camera record and its mask table.
///
/// Fields 0 and 1 are direct file offsets to the camera's mask sprite table
/// and its embedded mask TIM; the table itself is parsed into the cut.
fn parse_cut(data: &[u8], start: usize, index: usize) -> Result<Cut> {
    let record = &data[start..start + CAMERA_SIZE];
    let mut fields = [0i32; CAMERA_FIELDS];
    for (field, bytes) in fields.iter_mut().zip(record.as_chunks::<4>().0) {
        *field = i32::from_le_bytes(*bytes);
    }

    let mask_pointer = u32::try_from(fields[0])
        .with_context(|| format!("camera {index} has negative mask pointer {}", fields[0]))?;
    let tim_mask_pointer = u32::try_from(fields[1])
        .with_context(|| format!("camera {index} has negative mask TIM pointer {}", fields[1]))?;
    let table = mask::MaskTable::parse(data, mask_pointer)
        .with_context(|| format!("failed to parse the mask table of camera {index}"))?;

    Ok(Cut {
        index,
        pos: [fields[2], fields[3], fields[4]],
        look_at: [fields[5], fields[6], fields[7]],
        roll: fields[8],
        fov: fields[10],
        background: None,
        mask_pointer,
        tim_mask_pointer,
        mask_group_count: table.groups.len() as u8,
        mask_active: table.active_bits(),
        masks: table.sprites,
    })
}

/// Read a bounds-checked byte range.
fn bytes_at(data: &[u8], offset: usize, length: usize) -> Result<&[u8]> {
    let end = offset
        .checked_add(length)
        .with_context(|| format!("RDT read at 0x{offset:x} overflows"))?;
    data.get(offset..end).with_context(|| {
        format!(
            "RDT read of {length} byte(s) at 0x{offset:x} is out of bounds ({} bytes)",
            data.len()
        )
    })
}

/// Read a little-endian `u16`.
fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    let raw = bytes_at(data, offset, 2)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

/// Read a little-endian `i16`.
fn i16_at(data: &[u8], offset: usize) -> Result<i16> {
    Ok(u16_at(data, offset)? as i16)
}

/// Read a little-endian `u32`.
fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    let raw = bytes_at(data, offset, 4)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// Read a little-endian `i32`.
fn i32_at(data: &[u8], offset: usize) -> Result<i32> {
    Ok(u32_at(data, offset)? as i32)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const ROOM_ID: RoomId = RoomId {
        stage: 2,
        room: 5,
        player_flag: 3,
    };
    const RDT_HEADER_LEN: usize = 0x94;

    fn encode_record(fields: [i32; CAMERA_FIELDS]) -> Vec<u8> {
        fields
            .iter()
            .flat_map(|field| field.to_le_bytes())
            .collect()
    }

    fn build_rdt(cameras_count: u8, records: &[[i32; CAMERA_FIELDS]]) -> Vec<u8> {
        let mut data = vec![0u8; RDT_HEADER_LEN];
        data[0x01] = cameras_count;
        for fields in records {
            data.extend_from_slice(&encode_record(*fields));
        }
        data
    }

    fn set_ptr(data: &mut [u8], slot: usize, offset: usize) {
        let start = POINTERS_OFFSET + slot * 4;
        data[start..start + 4].copy_from_slice(&(offset as u32).to_le_bytes());
    }

    fn set_light(
        data: &mut [u8],
        index: usize,
        pos: [i32; 3],
        color: [u8; 3],
        kind: u16,
        radius: i16,
    ) {
        let offset = LIGHTS_OFFSET + index * LIGHT_SIZE;
        for (axis, value) in pos.iter().enumerate() {
            data[offset + axis * 4..offset + axis * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        data[offset + 0x0C..offset + 0x0F].copy_from_slice(&color);
        data[offset + 0x0F] = 0;
        data[offset + 0x10..offset + 0x12].copy_from_slice(&kind.to_le_bytes());
        data[offset + 0x12..offset + 0x14].copy_from_slice(&radius.to_le_bytes());
    }

    fn push_collision_rect(data: &mut Vec<u8>, record: [u16; 6]) {
        for value in record {
            data.extend_from_slice(&value.to_le_bytes());
        }
    }

    fn push_zone(data: &mut Vec<u8>, cam_to: i16, cam_from: i16, corners: [[i16; 2]; 4]) {
        data.extend_from_slice(&cam_to.to_le_bytes());
        data.extend_from_slice(&cam_from.to_le_bytes());
        for corner in corners {
            data.extend_from_slice(&corner[0].to_le_bytes());
            data.extend_from_slice(&corner[1].to_le_bytes());
        }
    }

    fn push_walk_zone(data: &mut Vec<u8>, record: [i16; 4], field_08: u16, flags: u16) {
        for value in record {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&field_08.to_le_bytes());
        data.extend_from_slice(&flags.to_le_bytes());
    }

    fn push_footstep_zone(data: &mut Vec<u8>, record: [u16; 5]) {
        for value in record {
            data.extend_from_slice(&value.to_le_bytes());
        }
    }

    #[test]
    fn parses_footstep_zones_until_the_next_section() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, FOOTSTEP_SLOT, offset);
        data.extend_from_slice(&1u16.to_le_bytes());
        push_footstep_zone(&mut data, [900, 1100, 8700, 9800, 45]);
        push_footstep_zone(&mut data, [0x7FE4, 0x7FEC, 0x7FE4, 0x7FEC, 0]);
        let next = data.len();
        set_ptr(&mut data, FOOTSTEP_SLOT + 1, next);
        data.extend_from_slice(&[0xAB; 7]);

        let state = parse(&data, ROOM_ID).unwrap();

        assert_eq!(state.footstep_zones.len(), 2);
        assert_eq!(state.footstep_zones[0].base_x, 900);
        assert_eq!(state.footstep_zones[0].sound_data, 45);
        assert_eq!(state.footstep_zones[1].width, 0x7FE4);
        assert_eq!(state.footstep_zone(4850, 4950), Some((38 << 8) | 45));
        assert_eq!(state.footstep_zone(40000, 40000), Some(0x7F00));
    }

    #[test]
    fn footstep_zones_fall_back_to_the_file_end() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, FOOTSTEP_SLOT, offset);
        data.extend_from_slice(&0u16.to_le_bytes());
        push_footstep_zone(&mut data, [1, 2, 3, 4, 5]);

        let state = parse(&data, ROOM_ID).unwrap();

        assert_eq!(state.footstep_zones.len(), 1);
        assert_eq!(state.footstep_zones[0].base_z, 2);
        assert_eq!(state.footstep_zone(1, 2), Some(5));
    }

    #[test]
    fn no_footstep_pointer_means_no_zones() {
        let data = build_rdt(0, &[]);
        let state = parse(&data, ROOM_ID).unwrap();
        assert!(state.footstep_zones.is_empty());
        assert_eq!(state.footstep_zone(0, 0), None);
    }

    #[test]
    fn rejects_out_of_bounds_footstep_pointer() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len() + 4;
        set_ptr(&mut data, FOOTSTEP_SLOT, offset);

        assert!(parse(&data, ROOM_ID).is_err());
    }

    #[test]
    fn parses_two_camera_records() {
        let first = [0, 0, 13, 14, 15, 16, 17, 18, 19, 20, 21];
        let second = [0, 0, -13, -14, -15, -16, -17, -18, -19, -20, -21];
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
        assert_eq!(cut.mask_pointer, 0);
        assert_eq!(cut.tim_mask_pointer, 0);
        assert_eq!(cut.mask_group_count, 0);
        assert_eq!(cut.mask_active, 0);
        assert!(cut.masks.is_empty());

        let cut = &state.cuts[1];
        assert_eq!(cut.index, 1);
        assert_eq!(cut.pos, [-13, -14, -15]);
        assert_eq!(cut.look_at, [-16, -17, -18]);
        assert_eq!(cut.roll, -19);
        assert_eq!(cut.fov, -21);
        assert!(cut.background.is_none());
        assert!(cut.masks.is_empty());
    }

    /// Append a mask table: an `i32` group count, the group headers, then the
    /// flattened sprite words.
    fn push_mask_table(data: &mut Vec<u8>, groups: &[[u16; 4]], sprites: &[u16]) {
        data.extend_from_slice(&(groups.len() as i32).to_le_bytes());
        for group in groups {
            for word in group {
                data.extend_from_slice(&word.to_le_bytes());
            }
        }
        for word in sprites {
            data.extend_from_slice(&word.to_le_bytes());
        }
    }

    #[test]
    fn parses_camera_masks_into_the_cut() {
        let mut data = build_rdt(1, &[[0; CAMERA_FIELDS]]);
        let offset = data.len();
        data[CAMERAS_OFFSET..CAMERAS_OFFSET + 4].copy_from_slice(&(offset as i32).to_le_bytes());
        data[CAMERAS_OFFSET + 4..CAMERAS_OFFSET + 8].copy_from_slice(&1234i32.to_le_bytes());
        push_mask_table(
            &mut data,
            &[[1, 0, 10, 20]],
            &[0x0201, 0x0403, 380, 0x0800, 8, 40],
        );

        let state = parse(&data, ROOM_ID).unwrap();

        let cut = &state.cuts[0];
        assert_eq!(cut.mask_pointer as usize, offset);
        assert_eq!(cut.tim_mask_pointer, 1234);
        assert_eq!(cut.mask_group_count, 1);
        assert_eq!(cut.mask_active, 1);
        assert_eq!(cut.masks.len(), 1);
        let sprite = cut.masks[0];
        assert_eq!(sprite.uv, (1, 2));
        assert_eq!(sprite.pos, (13, 24));
        assert_eq!(sprite.size, (8, 40));
        assert_eq!(sprite.pos_data, 380);
        assert_eq!(sprite.flags, 0x0800);
        assert_eq!(sprite.group, 1);
    }

    #[test]
    fn errors_on_malformed_camera_masks() {
        let mut data = build_rdt(1, &[[0; CAMERA_FIELDS]]);
        let offset = data.len();
        data[CAMERAS_OFFSET..CAMERAS_OFFSET + 4].copy_from_slice(&(offset as i32).to_le_bytes());
        data.extend_from_slice(&1i32.to_le_bytes());

        let message = parse(&data, ROOM_ID).unwrap_err().to_string();
        assert!(message.contains("mask table of camera 0"), "{message}");
    }

    #[test]
    fn errors_on_negative_mask_pointer() {
        let mut data = build_rdt(1, &[[0; CAMERA_FIELDS]]);
        data[CAMERAS_OFFSET..CAMERAS_OFFSET + 4].copy_from_slice(&(-1i32).to_le_bytes());

        let message = parse(&data, ROOM_ID).unwrap_err().to_string();
        assert!(message.contains("negative mask pointer"), "{message}");
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
    fn stub_rdt_parses_into_an_empty_room() {
        let state = parse(&[0x00, 0x00, 0x00, 0x00], ROOM_ID).unwrap();

        assert!(state.cuts.is_empty());
        assert_eq!(state.ambient, [0; 3]);
        assert_eq!(state.lights, [Light::default(); LIGHT_COUNT]);
        assert!(state.collision.quadrants.iter().all(Vec::is_empty));
        assert!(state.zones.is_empty());
        assert!(state.walk_zones.is_empty());
        assert!(state.footstep_zones.is_empty());
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
        let mut data = build_rdt(0, &[]);
        data[0x01] = 1;

        assert!(parse(&data, ROOM_ID).is_err());
    }

    #[test]
    fn parses_ambient_and_lights() {
        let mut data = build_rdt(0, &[]);
        data[AMBIENT_OFFSET..AMBIENT_OFFSET + 2].copy_from_slice(&896i16.to_le_bytes());
        data[AMBIENT_OFFSET + 2..AMBIENT_OFFSET + 4].copy_from_slice(&976i16.to_le_bytes());
        data[AMBIENT_OFFSET + 4..AMBIENT_OFFSET + 6].copy_from_slice(&656i16.to_le_bytes());
        set_light(&mut data, 0, [1, 2, 3], [10, 20, 30], 0, 7000);
        set_light(&mut data, 1, [-4, -5, -6], [40, 50, 60], 1, -1);
        set_light(&mut data, 2, [7, 8, 9], [70, 80, 90], 2, 100);

        let state = parse(&data, ROOM_ID).unwrap();

        assert_eq!(state.ambient, [896, 976, 656]);
        assert_eq!(state.lights[0].pos, [1, 2, 3]);
        assert_eq!(state.lights[0].color, [10, 20, 30]);
        assert_eq!(state.lights[0].kind, 0);
        assert_eq!(state.lights[0].radius, 7000);
        assert_eq!(state.lights[1].pos, [-4, -5, -6]);
        assert_eq!(state.lights[1].kind, 1);
        assert_eq!(state.lights[1].radius, 0xFFFF);
        assert_eq!(state.lights[2].color, [70, 80, 90]);
        assert_eq!(state.lights[2].radius, 100);
    }

    #[test]
    fn parses_collision_quadrants() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, COLLISION_SLOT, offset);
        data.extend_from_slice(&10i16.to_le_bytes());
        data.extend_from_slice(&20i16.to_le_bytes());
        for count in [2i32, 1, 0, 0, 0] {
            data.extend_from_slice(&count.to_le_bytes());
        }
        push_collision_rect(&mut data, [100, 110, 10, 20, 1, 0x300]);
        push_collision_rect(&mut data, [200, 210, 110, 120, 5, 0x200]);
        push_collision_rect(&mut data, [300, 310, 210, 220, 3, 0x100]);

        let state = parse(&data, ROOM_ID).unwrap();
        let collision = &state.collision;

        assert_eq!(collision.cell_x, 10);
        assert_eq!(collision.cell_z, 20);
        assert_eq!(collision.quadrants[0].len(), 2);
        assert_eq!(collision.quadrants[1].len(), 1);
        assert!(collision.quadrants[2].is_empty());
        assert!(collision.quadrants[3].is_empty());
        assert_eq!(collision.quadrants[0][0].kind, 1);
        assert_eq!(collision.quadrants[0][0].flags, 0x300);
        assert_eq!(collision.quadrants[1][0].x_max, 300);

        assert_eq!(collision.records(50, 50)[0].x_max, 100);
        assert_eq!(collision.records(5, 50)[0].x_max, 300);
        assert!(collision.records(50, 5).is_empty());
        assert!(collision.records(5, 5).is_empty());
    }

    #[test]
    fn parses_switch_zones_in_file_order() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, CAMERA_ZONES_SLOT, offset);
        push_zone(&mut data, 9, 0, [[0, 0], [0, 100], [100, 100], [100, 0]]);
        push_zone(&mut data, 1, 0, [[10, 10], [10, 90], [90, 90], [90, 10]]);
        push_zone(
            &mut data,
            5,
            1,
            [[200, 200], [200, 300], [300, 300], [300, 200]],
        );
        data.extend_from_slice(&[0xFF; ZONE_SIZE]);

        let state = parse(&data, ROOM_ID).unwrap();

        assert_eq!(state.zones.len(), 3);
        assert_eq!(state.zones[0].cam_to, 9);
        assert_eq!(state.zones[0].cam_from, 0);
        assert_eq!(state.zones[1].cam_from, 0);
        assert_eq!(state.zones[2].cam_to, 5);
        assert_eq!(state.zones[2].cam_from, 1);
    }

    #[test]
    fn zone_contains_matches_the_edge_tests() {
        let zone = Zone {
            cam_to: 1,
            cam_from: 0,
            corners: [[0, 0], [0, 100], [100, 100], [100, 0]],
        };

        assert!(zone.contains(50, 50));
        assert!(zone.contains(1, 99));
        assert!(!zone.contains(-1, 50));
        assert!(!zone.contains(50, -1));
        assert!(!zone.contains(101, 50));
        assert!(!zone.contains(50, 101));
        assert!(zone.contains(0, 0));
        assert!(zone.contains(100, 100));
    }

    #[test]
    fn zone_contains_zero_extends_corners() {
        let zone = Zone {
            cam_to: 1,
            cam_from: 0,
            corners: [[-1, -1], [-1, 1], [1, 1], [1, -1]],
        };

        assert!(zone.contains(65535, 65535));
        assert!(!zone.contains(0, 0));
    }

    #[test]
    fn parses_walk_zones_with_half_open_edges() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, WALK_ZONES_SLOT, offset);
        data.push(2);
        data.push(0);
        push_walk_zone(&mut data, [100, 300, 200, 400], 0x3FF, 0);
        push_walk_zone(&mut data, [100, 0, -100, 100], 0x100, 0x10);

        let state = parse(&data, ROOM_ID).unwrap();

        assert_eq!(state.walk_zones.len(), 2);
        let zone = &state.walk_zones[0];
        assert_eq!(zone.field_08, 0x3FF);
        assert!(zone.contains(100, 300));
        assert!(zone.contains(199, 399));
        assert!(!zone.contains(200, 300));
        assert!(!zone.contains(100, 400));
        assert!(!zone.contains(99, 300));

        // x wraps from 100 to -100 (unsigned 65436), so -101 is the last
        // inside value before the half-open end.
        let wrapped = &state.walk_zones[1];
        assert!(wrapped.contains(100, 50));
        assert!(wrapped.contains(-101, 50));
        assert!(!wrapped.contains(-100, 50));
        assert!(!wrapped.contains(0, 50));
        assert_eq!(wrapped.flags, 0x10);
    }

    #[test]
    fn rejects_out_of_bounds_collision_pointer() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len() + 4;
        set_ptr(&mut data, COLLISION_SLOT, offset);

        assert!(parse(&data, ROOM_ID).is_err());
    }

    #[test]
    fn rejects_collision_record_overrun() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, COLLISION_SLOT, offset);
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        for count in [3i32, 0, 0, 0, 0] {
            data.extend_from_slice(&count.to_le_bytes());
        }

        assert!(parse(&data, ROOM_ID).is_err());
    }

    #[test]
    fn rejects_collision_counts_over_the_cap() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, COLLISION_SLOT, offset);
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        for count in [i32::MAX, 0, 0, 0, 0] {
            data.extend_from_slice(&count.to_le_bytes());
        }

        let message = budget::assert_cap_error(parse(&data, ROOM_ID));
        assert!(message.contains("collision record count"), "{message}");
    }

    #[test]
    fn rejects_negative_collision_count() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, COLLISION_SLOT, offset);
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        for count in [-1i32, 0, 0, 0, 0] {
            data.extend_from_slice(&count.to_le_bytes());
        }

        assert!(parse(&data, ROOM_ID).is_err());
    }

    #[test]
    fn rejects_out_of_bounds_zone_pointer() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, CAMERA_ZONES_SLOT, offset);

        assert!(parse(&data, ROOM_ID).is_err());
    }

    #[test]
    fn rejects_walk_zone_overrun() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len();
        set_ptr(&mut data, WALK_ZONES_SLOT, offset);
        data.push(2);
        data.push(0);
        push_walk_zone(&mut data, [0, 0, 10, 10], 0, 0);

        assert!(parse(&data, ROOM_ID).is_err());
    }

    /// Minimal one-object TMD: 12-byte header plus one zeroed descriptor.
    fn empty_tmd() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0x41u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&[0u8; 28]);
        data
    }

    /// Minimal 8bpp single-CLUT-row TIM: 2x1 pixels, one 256-entry row.
    fn tiny_tim() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0x10u32.to_le_bytes());
        data.extend_from_slice(&9u32.to_le_bytes());
        data.extend_from_slice(&(12u32 + 256 * 2).to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&256u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        for entry in 0..256u16 {
            data.extend_from_slice(&(entry & 0x7FFF).to_le_bytes());
        }
        data.extend_from_slice(&14u32.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&[0, 0]);
        data
    }

    #[test]
    fn parses_embedded_omodel_and_item_pairs() {
        let mut data = build_rdt(0, &[]);
        // Omodel slot 0 and item slot 0 both point at appended art, with the
        // source pair index preserved.
        data[0x02] = 1;
        data[0x03] = 1;
        let omodel_table = data.len();
        set_ptr(&mut data, OBJECT_MODELS_SLOT, omodel_table);
        let tmd_offset = data.len() as u32 + 8;
        data.extend_from_slice(&tmd_offset.to_le_bytes());
        let tim_offset = tmd_offset + empty_tmd().len() as u32;
        data.extend_from_slice(&tim_offset.to_le_bytes());
        data.extend_from_slice(&empty_tmd());
        data.extend_from_slice(&tiny_tim());

        let item_table = data.len();
        set_ptr(&mut data, ITEM_MODELS_SLOT, item_table);
        let tmd_offset = data.len() as u32 + 8;
        data.extend_from_slice(&tmd_offset.to_le_bytes());
        let tim_offset = tmd_offset + empty_tmd().len() as u32;
        data.extend_from_slice(&tim_offset.to_le_bytes());
        data.extend_from_slice(&empty_tmd());
        data.extend_from_slice(&tiny_tim());

        let state = parse(&data, ROOM_ID).unwrap();

        assert_eq!(state.omodel_slot_count, 1);
        assert_eq!(state.item_count, 1);
        assert!(
            state.model_warnings.is_empty(),
            "{:?}",
            state.model_warnings
        );
        assert_eq!(state.object_models.len(), 1);
        assert_eq!(state.object_models[0].pair_index, 0);
        assert_eq!(state.object_models[0].model.objects.len(), 1);
        assert_eq!(state.object_models[0].texture.width, 2);
        assert_eq!(state.object_models[0].texture.height, 1);
        assert_eq!(state.item_models.len(), 1);
        assert_eq!(state.item_models[0].pair_index, 0);
    }

    #[test]
    fn skips_a_malformed_pair_and_counts_it() {
        let mut data = build_rdt(0, &[]);
        data[0x02] = 2;
        let table = data.len();
        set_ptr(&mut data, OBJECT_MODELS_SLOT, table);
        // Two pointer slots, then the first pair's art and the second pair's
        // (malformed) art.
        data.extend_from_slice(&[0u8; 16]);
        let first_tmd = data.len() as u32;
        data.extend_from_slice(&empty_tmd());
        let first_tim = data.len() as u32;
        data.extend_from_slice(&tiny_tim());
        let second_tmd = data.len() as u32;
        data.extend_from_slice(&empty_tmd());
        let second_tim = data.len() as u32;
        data.extend_from_slice(&tiny_tim());
        let entries = [(first_tmd, first_tim), (second_tmd, second_tim)];
        for (index, entry) in entries.iter().enumerate() {
            let at = table + index * 8;
            data[at..at + 4].copy_from_slice(&entry.0.to_le_bytes());
            data[at + 4..at + 8].copy_from_slice(&entry.1.to_le_bytes());
        }
        // The second TMD has bad magic.
        let second = second_tmd as usize;
        data[second..second + 4].copy_from_slice(&0u32.to_le_bytes());

        let state = parse(&data, ROOM_ID).unwrap();

        assert_eq!(state.object_models.len(), 1);
        assert_eq!(state.object_models[0].pair_index, 0);
        assert_eq!(state.model_warnings.len(), 1);
        assert!(state.model_warnings[0].contains("pair 1"));
    }

    #[test]
    fn null_pair_halves_are_declared_but_unbuilt_and_never_warn() {
        let mut data = build_rdt(0, &[]);
        data[0x02] = 3;
        let table = data.len();
        set_ptr(&mut data, OBJECT_MODELS_SLOT, table);
        // Pair 0: null TMD; pair 1: null TIM; pair 2: out-of-bounds pointer.
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0xFFFF_0000u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());

        let state = parse(&data, ROOM_ID).unwrap();

        assert!(state.object_models.is_empty());
        assert_eq!(state.model_warnings.len(), 1);
        assert!(
            state.model_warnings[0].contains("pair 2"),
            "{:?}",
            state.model_warnings
        );
    }

    #[test]
    fn parses_room_message_block_and_looks_up_ids() {
        let mut data = build_rdt(0, &[]);
        let base = data.len();
        set_ptr(&mut data, MESSAGE_SLOT, base);
        let messages: [&[u8]; 3] = [
            &[0x0C, 0x0D, 0x01, 0x00],
            &[0x05, 0x01, 0x0C, 0x03, 0x02, 0x08, 0x01, 0x30],
            &[0x0C, 0x01, 0x00],
        ];
        let mut block = Vec::new();
        let mut offset = (messages.len() * 2) as u16;
        for message in messages {
            block.extend_from_slice(&offset.to_le_bytes());
            offset += message.len() as u16;
        }
        for message in messages {
            block.extend_from_slice(message);
        }
        data.extend_from_slice(&block);

        let state = parse(&data, ROOM_ID).unwrap();

        assert_eq!(state.messages.as_deref(), Some(block.as_slice()));
        assert!(state.message(0).unwrap().starts_with(messages[0]));
        assert!(state.message(1).unwrap().starts_with(messages[1]));
        // The last message's slice runs to the end of the block.
        assert_eq!(state.message(2).unwrap(), messages[2]);
        // Only the low six bits are the table index.
        assert_eq!(state.message(0x40).unwrap(), state.message(0).unwrap());
        // An offset that lands outside the block is refused.
        assert!(state.message(3).is_none());
        assert!(state.message(0x3F).is_none());
    }

    #[test]
    fn no_message_pointer_means_no_messages() {
        let data = build_rdt(0, &[]);

        let state = parse(&data, ROOM_ID).unwrap();

        assert!(state.messages.is_none());
        assert!(state.message(0).is_none());
    }

    #[test]
    fn rejects_out_of_bounds_message_pointer() {
        let mut data = build_rdt(0, &[]);
        let offset = data.len() + 4;
        set_ptr(&mut data, MESSAGE_SLOT, offset);

        assert!(parse(&data, ROOM_ID).is_err());
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn parses_real_room_messages() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        for name in ["ROOM1000.RDT", "ROOM1001.RDT"] {
            let path = Path::new(&root).join("JPN/STAGE1").join(name);
            let data = std::fs::read(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            let room = name.trim_end_matches(".RDT").trim_start_matches("ROOM");
            let state = parse(&data, RoomId::parse(room).unwrap()).unwrap();

            let block = state.messages.as_deref().expect("no message block");
            assert_eq!(block.len(), 1928, "{name}");
            let message = state.message(0).expect("message 0 missing");
            assert_eq!(&message[..4], &[0x04, 0x00, 0x02, 0x00], "{name}");
            assert!(
                message.iter().take(256).any(|&byte| byte == 0x01),
                "{name}: message 0 has no terminator"
            );
        }
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn parses_real_room_1001() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let path = Path::new(&root).join("JPN/STAGE1/ROOM1001.RDT");
        let data = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));

        let state = parse(&data, RoomId::parse("1001").unwrap()).unwrap();

        assert_eq!(state.ambient, [896, 976, 656]);
        assert_eq!(state.lights[0].pos, [4680, -2222, 6360]);
        assert_eq!(state.lights[0].color, [100, 100, 100]);
        assert_eq!(state.lights[0].kind, 0);
        assert_eq!(state.lights[0].radius, 7000);

        assert_eq!(state.cuts.len(), 6);
        assert_eq!(state.cuts[0].fov, 221);

        assert_eq!(state.collision.cell_x, 5188);
        assert_eq!(state.collision.cell_z, 6038);
        assert_eq!(state.collision.quadrants[0].len(), 4);
        assert_eq!(state.collision.quadrants[1].len(), 4);
        assert_eq!(state.collision.quadrants[2].len(), 4);
        assert_eq!(state.collision.quadrants[3].len(), 2);
        let record = state.collision.quadrants[0][0];
        assert_eq!(record.x_max, 10864);
        assert_eq!(record.z_max, 12330);
        assert_eq!(record.x_min, 8490);
        assert_eq!(record.z_min, 4);
        assert_eq!(record.kind, 1);
        assert_eq!(record.flags, 0x300);

        assert_eq!(state.zones.len(), 14);
        assert_eq!(state.zones[0].cam_to, 9);
        assert_eq!(state.zones[0].cam_from, 0);
        assert_eq!(
            state.zones[0].corners,
            [[1100, 893], [600, 10702], [9807, 10600], [9685, 900]]
        );
        assert!(state.zones[0].contains(2000, 5000));
        assert!(!state.zones[0].contains(11000, 5000));
        assert_eq!(state.zones[3].cam_from, 1);
        assert_eq!(
            state.zones[3].corners,
            [[700, 900], [700, 11014], [9871, 11014], [9971, 900]]
        );

        assert_eq!(state.walk_zones.len(), 1);
        let zone = state.walk_zones[0];
        assert_eq!(
            (zone.x1, zone.z1, zone.x2, zone.z2),
            (1700, 1800, 8000, 8100)
        );
        assert_eq!(zone.field_08, 0x3FF);
        assert_eq!(zone.flags, 0);
        assert!(zone.contains(1700, 1800));
        assert!(!zone.contains(8000, 8100));

        assert_eq!(state.footstep_zones.len(), 1);
        let zone = state.footstep_zones[0];
        assert_eq!(
            (zone.base_x, zone.base_z, zone.width, zone.height),
            (900, 1100, 8700, 9800)
        );
        assert_eq!(zone.sound_data, 45);
        assert_eq!(state.footstep_zone(4850, 4950), Some((38 << 8) | 45));
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn parses_real_room_1000_masks() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let path = Path::new(&root).join("JPN/STAGE1/ROOM1000.RDT");
        let data = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));

        let state = parse(&data, RoomId::parse("1000").unwrap()).unwrap();

        let cut = &state.cuts[0];
        assert_eq!(cut.mask_group_count, 7);
        assert_eq!(cut.masks.len(), 53);
        assert_eq!(cut.mask_active, 0b111_1111);
        assert_ne!(cut.mask_pointer, 0);
        assert_ne!(cut.tim_mask_pointer, 0);

        let first = cut.masks[0];
        assert_eq!(first.uv, (0, 0));
        assert_eq!(first.pos, (9, 73));
        assert_eq!(first.size, (8, 40));
        assert_eq!(first.pos_data, 380);
        assert_eq!(first.flags, 0x80);
        assert_eq!(first.group, 1);
        assert_eq!(cut.masks.last().unwrap().group, 7);
    }
}
