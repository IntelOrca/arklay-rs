//! Room object models: the embedded RDT TMD/TIM pair table, the runtime
//! object records the SCD `obj`/`eml_*` opcodes drive, and the transform the
//! renderer and the effect attach seam read.
//!
//! The RDT header holds two flat arrays of 8-byte `{TMD*, TIM*}` pairs:
//! pointer slot 2 `object_models` with its count in header byte `0x02`, and
//! slot 3 `item_models` with its count in byte `0x03`. Both TMD and TIM
//! pointers are relative to the RDT start and either half may be null. The
//! parse is tolerant the way [`crate::effects::RoomEffects::parse`] is: a
//! malformed pair is skipped with a warning and never fails the room.
//!
//! Every declared omodel slot owns one [`ObjectRecord`]. The `obj` opcode
//! builds a record (its 28-byte operand block is mapped field for field),
//! `objtbl_b_set` writes its flag byte, `ck_anim` compares its push counter,
//! `eml_rot`/`eml_pos` write its rotation and position, `model_op` accumulates
//! its colour tint, and the renderer composes its world matrix. Item-model
//! records are not built this milestone; the item-table selectors are typed
//! no-ops.
//!
//! This module also owns the three entity/object box tests the room-object
//! pass is built on: [`chk_entity_slide`] (resolve an actor against a record),
//! [`chk_obj_slide`] (shove a record out of another) and
//! [`chk_pl_reach_entity`] (the 470-unit reach box), plus the
//! [`check_climb_object`] scan and its mid-climb verification.

use anyhow::{Context, Result, bail};

use crate::anim::{self, Mat4x3};
use crate::model::{Texture8, Tmd};
use crate::render::Lighting;
use crate::scd::ir::Operand;
use crate::state::{RoomId, RoomState};
use crate::tim;
use crate::tmd;

/// Object flag bit `0x01`: active and drawn.
pub const OBJECT_FLAG_ACTIVE: u8 = 0x01;
/// Object flag bit `0x02`: intangible (never collides).
pub const OBJECT_FLAG_INTANGIBLE: u8 = 0x02;
/// Object flag bit `0x04`: the object's own floor probe is skipped.
pub const OBJECT_FLAG_SKIP_FLOOR_PROBE: u8 = 0x04;
/// Object flag bit `0x08`: no collision.
pub const OBJECT_FLAG_NO_COLLISION: u8 = 0x08;
/// Object flag bit `0x20`: not pushable.
pub const OBJECT_FLAG_NOT_PUSHABLE: u8 = 0x20;
/// Object flag bit `0x40`: climbable.
pub const OBJECT_FLAG_CLIMBABLE: u8 = 0x40;

/// One embedded `{TMD, TIM}` pair, decoded at room load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectAsset {
    /// The pair's index in the declared table; the SCD `obj` slot selects it.
    pub pair_index: usize,
    /// The parsed single-object TMD mesh.
    pub model: Tmd,
    /// The decoded 8bpp texture.
    pub texture: Texture8,
}

/// Parse a flat `{TMD*, TIM*}` pair table.
///
/// `pointer` is the RDT header slot, `count` its declared pair count. Returns
/// the successfully decoded assets (each keeping its source pair index) and
/// one warning per skipped pair. A zero pointer or count declares no assets.
pub fn parse_assets(data: &[u8], pointer: u32, count: u8) -> (Vec<ObjectAsset>, Vec<String>) {
    let mut assets = Vec::new();
    let mut warnings = Vec::new();
    if pointer == 0 || count == 0 {
        return (assets, warnings);
    }
    let base = pointer as usize;
    for pair in 0..usize::from(count) {
        let at = base + pair * 8;
        let read = |field: usize| -> Result<u32> {
            let bytes = data
                .get(at + field * 4..at + field * 4 + 4)
                .with_context(|| {
                    format!(
                        "model pair {pair} at 0x{at:x} is out of bounds for the {}-byte RDT",
                        data.len()
                    )
                })?;
            Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
        };
        let (tmd_pointer, tim_pointer) = match (read(0), read(1)) {
            (Ok(tmd), Ok(tim)) => (tmd, tim),
            (Err(error), _) | (_, Err(error)) => {
                warnings.push(format!("model pair {pair}: {error:#}"));
                continue;
            }
        };
        // Either half may be null; a pair with no mesh is declared but
        // unbuilt, not malformed.
        if tmd_pointer == 0 || tim_pointer == 0 {
            continue;
        }
        let parsed = (|| -> Result<ObjectAsset> {
            let model_bytes = data
                .get(tmd_pointer as usize..)
                .with_context(|| format!("TMD pointer 0x{tmd_pointer:x} is out of bounds"))?;
            let magic = u32::from_le_bytes(
                model_bytes
                    .get(..4)
                    .context("TMD is truncated")?
                    .try_into()
                    .unwrap(),
            );
            if magic != 0x41 {
                bail!("bad TMD magic 0x{magic:08X}");
            }
            let model = tmd::parse(model_bytes)?;
            let texture_bytes = data
                .get(tim_pointer as usize..)
                .with_context(|| format!("TIM pointer 0x{tim_pointer:x} is out of bounds"))?;
            let texture = tim::decode_8bpp(texture_bytes)?;
            Ok(ObjectAsset {
                pair_index: pair,
                model,
                texture,
            })
        })();
        match parsed {
            Ok(asset) => assets.push(asset),
            Err(error) => warnings.push(format!("model pair {pair}: {error:#}")),
        }
    }
    (assets, warnings)
}

/// One runtime object record. The original reuses a 0xA4-byte `Entity` head
/// for these; the port keeps a named struct with only the fields the opcodes
/// and the collision helpers address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ObjectRecord {
    /// Flag byte (record +0): the `OBJECT_FLAG_*` bits.
    pub flag: u8,
    /// Model byte (record +1): the RDT pair slot plus the push-grunt `0x40`
    /// bit; the texture-queue `0x80` bit is dropped.
    pub model: u8,
    /// Entry flags word (record +0x7E, same value as the initial yaw).
    pub entry_flags: u16,
    /// SCA parent selector (record +0x64): `0xFE` player, `0xFF` none,
    /// `< 0x80` another object, else enemy `& 0x7F`.
    pub parent: u8,
    /// Live 32-bit position (record +0x34).
    pub pos: [i32; 3],
    /// Last committed 16-bit position (record +0x6C), written by the push
    /// helpers.
    pub committed: [i16; 3],
    /// Rotation SVECTOR (record +0x72).
    pub rotation: [i16; 3],
    /// The rotation copy the push rollback reads (record +0x78).
    pub rotation_rollback: [i16; 3],
    /// Collision half-extents X/Y/Z (record +0x8A/+0x8C/+0x8E).
    pub half_extents: [u16; 3],
    /// The entity-side extent word (record +0x90, the second Y copy).
    pub extent_word: u16,
    /// The entity-side radius word (record +0x92).
    pub radius: u16,
    /// The two floor-probe X/Z endpoints (record +0x94..+0xA0).
    pub probe: [[u16; 2]; 2],
    /// The push-hold counter (record +0x86), compared by `ck_anim`.
    pub push_counter: u16,
    /// Index of the bound asset in [`crate::state::RoomState::object_models`];
    /// `None` when the pair had no mesh.
    pub asset: Option<u8>,
    /// Per-channel tint multipliers accumulated by `model_op` (slice 4).
    pub tint: [i8; 3],
    /// The luminance light scale stored by `model_op` (slice 4).
    pub light_scale: i16,
}

impl ObjectRecord {
    /// Whether the active/drawn flag is set.
    pub fn active(&self) -> bool {
        self.flag & OBJECT_FLAG_ACTIVE != 0
    }

    /// The per-channel RGB multiplier the renderer applies to this object's
    /// shaded triangles.
    ///
    /// The record stores signed deltas accumulated by `model_op` variant 0 and
    /// the luminance light scale; the original keeps these as float multipliers
    /// on the model object, this port folds both into one 0..=255 channel
    /// multiplier (8/unit). The default (no tint, no scale) is white.
    pub fn shade(&self) -> [u8; 3] {
        self.tint.map(|delta| {
            let value = 255 + 8 * (i32::from(self.light_scale) + i32::from(delta));
            value.clamp(0, 255) as u8
        })
    }
}

/// One record per declared omodel slot plus the `obj` build counter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectTable {
    /// The declared omodel records; `obj` slots past the end are ignored.
    pub records: Vec<ObjectRecord>,
    /// `g_omodelCount`: how many `obj` calls have built a record.
    pub built: u8,
}

impl ObjectTable {
    /// An empty table for `slot_count` declared omodel slots.
    pub fn new(slot_count: usize) -> Self {
        Self {
            records: vec![ObjectRecord::default(); slot_count],
            built: 0,
        }
    }

    /// Room entry: drop every record and the build counter, then size the
    /// table to the new room's declared slot count.
    pub fn reset(&mut self, slot_count: usize) {
        self.records.clear();
        self.records.resize(slot_count, ObjectRecord::default());
        self.built = 0;
    }

    /// One record by slot.
    pub fn record(&self, index: usize) -> Option<&ObjectRecord> {
        self.records.get(index)
    }

    /// One record by slot, mutably.
    pub fn record_mut(&mut self, index: usize) -> Option<&mut ObjectRecord> {
        self.records.get_mut(index)
    }

    /// `obj` (0x1F): build the slot named by the model byte's low six bits.
    ///
    /// The 28-byte operand record maps field for field: `+1` the model byte
    /// (bit `0x40` is kept for the push grunt, bit `0x80` queued the original's
    /// texture processing), `+2` the flag byte, `+3` the SCA parent, `+4..+9`
    /// the s16 X/Y/Z position (written to both the 32-bit live position and the
    /// 16-bit committed copy), `+10` the entry-flags word (also the record's
    /// initial yaw), `+12..+19` the two floor-probe X/Z endpoints, `+20` the
    /// entity-side radius word and `+22..+27` the half extents and their second
    /// Y copy.
    ///
    /// # Documented no-ops
    ///
    /// The original's stage-5 texture-bank overrides, greenhouse/front-lesson
    /// palette fix-ups, the `0x40000000`/`0x40000040` SCA scale word and the
    /// `0x80` texture-queue request have no analogue in the per-model texture
    /// design and are not ported (the model byte keeps both flag bits so the
    /// push grunt still reads `0x40`). The room/slot positional overrides are
    /// transcribed by [`apply_position_overrides`].
    ///
    /// Returns whether a record was written; a slot past the declared table
    /// is ignored (the original would run off the loaded record block).
    pub fn build(&mut self, operands: &[Operand], id: RoomId) -> bool {
        let slot = usize::from(operand_u8(operands, 0) & 0x3F);
        let Some(_) = self.records.get(slot) else {
            return false;
        };

        let entry = operand_u16(operands, 6);
        let mut pos = [
            i32::from(operand_i16(operands, 3)),
            i32::from(operand_i16(operands, 4)),
            i32::from(operand_i16(operands, 5)),
        ];
        // Room/slot positional overrides. The original transcribes these as
        // stage/room special cases around the generic load.
        apply_position_overrides(&mut pos, slot, id);

        let record = &mut self.records[slot];
        *record = ObjectRecord {
            flag: operand_u8(operands, 1),
            model: operand_u8(operands, 0) & 0x7F,
            entry_flags: entry,
            parent: operand_u8(operands, 2),
            pos,
            committed: [pos[0] as i16, pos[1] as i16, pos[2] as i16],
            rotation: [0, entry as i16, 0],
            rotation_rollback: [0, entry as i16, 0],
            half_extents: [
                operand_word(operands, 19),
                operand_word(operands, 17),
                operand_word(operands, 21),
            ],
            extent_word: operand_word(operands, 17),
            radius: operand_word(operands, 15),
            probe: [
                [operand_word(operands, 7), operand_word(operands, 9)],
                [operand_word(operands, 11), operand_word(operands, 13)],
            ],
            push_counter: 0,
            asset: Some(slot as u8),
            tint: [0; 3],
            light_scale: 0,
        };
        self.built = self.built.wrapping_add(1);
        true
    }

    /// `objtbl_b_set` (0x35): write byte 0 of an omodel or item model.
    ///
    /// The water-tank-entry special case forces object 5 to zero regardless
    /// of the table selector. Item-model writes are a typed no-op this
    /// milestone.
    pub fn set_flag(&mut self, operands: &[Operand], id: RoomId) -> bool {
        let table = operand_u8(operands, 0);
        let index = operand_u8(operands, 1);
        let value = operand_u8(operands, 2);
        if id.stage == 4 && id.room == 0x0D && index & 0x3F == 5 {
            if let Some(record) = self.records.get_mut(usize::from(index)) {
                record.flag = 0;
            }
            return true;
        }
        if table == 0
            && let Some(record) = self.records.get_mut(usize::from(index))
        {
            record.flag = value;
            return true;
        }
        false
    }

    /// `ck_anim` (0x36): compare an object's push counter.
    ///
    /// Mode 0 `==`, 1 `>`, 2 `>=`, 3 `<`, 4 `<=`, 5 `!=`; any other mode (and
    /// a missing record) reports false.
    pub fn compare_counter(&self, operands: &[Operand]) -> bool {
        let index = usize::from(operand_u8(operands, 0));
        let mode = operand_u8(operands, 1);
        let value = u16::from(operand_u8(operands, 2));
        let Some(record) = self.records.get(index) else {
            return false;
        };
        let field = record.push_counter;
        match mode {
            0 => field == value,
            1 => field > value,
            2 => field >= value,
            3 => field < value,
            4 => field <= value,
            5 => field != value,
            _ => false,
        }
    }

    /// `eml_rot` (0x3B): write rotation X/Z when the selected record is
    /// active. Selectors below `0x8000` name the item-model table and are a
    /// typed no-op this milestone.
    pub fn rotate(&mut self, operands: &[Operand]) -> bool {
        let selector = (u16::from(operand_u8(operands, 0)) << 8) | 0x3B;
        if selector < 0x8000 {
            return false;
        }
        let index = usize::from(operand_u8(operands, 0) & 0x7F);
        let Some(record) = self.records.get_mut(index) else {
            return false;
        };
        if record.flag == 0 {
            return false;
        }
        record.rotation[0] = operand_i16(operands, 1);
        record.rotation[2] = operand_i16(operands, 2);
        true
    }

    /// `eml_pos` (0x47): set rotation X/Y/Z and position (both the 16- and
    /// 32-bit copies). The object index is the high byte of the first operand
    /// word; the low byte is the opcode itself.
    pub fn transform(&mut self, operands: &[Operand]) -> bool {
        let index = usize::from(operand_u8(operands, 0));
        let Some(record) = self.records.get_mut(index) else {
            return false;
        };
        let rotation = [
            operand_i16(operands, 1),
            operand_i16(operands, 2),
            operand_i16(operands, 3),
        ];
        let pos = [
            i32::from(operand_i16(operands, 4)),
            i32::from(operand_i16(operands, 5)),
            i32::from(operand_i16(operands, 6)),
        ];
        record.rotation = rotation;
        record.pos = pos;
        record.committed = [pos[0] as i16, pos[1] as i16, pos[2] as i16];
        true
    }
}

/// The `obj` stage/room positional special cases.
fn apply_position_overrides(pos: &mut [i32; 3], slot: usize, id: RoomId) {
    // Front lesson room (stages 2F and their return): slot 0 shifts Z by
    // +10, slot 1 by -0x28.
    if id.stage % 5 == 2 && id.room == 0x0B {
        match slot {
            0 => pos[2] += 10,
            1 => pos[2] -= 0x28,
            _ => {}
        }
    }
    // Guardhouse save room slot 0.
    if id.stage == 4 && id.room == 0x03 && slot == 0 {
        pos[0] += 0x1E;
        pos[1] -= 10;
        pos[2] += 0x82;
    }
    // Security room slot 0.
    if id.stage == 4 && id.room == 0x0F && slot == 0 {
        pos[1] -= 0x15E;
    }
    // Dining room slot 1, Jill's RDT variant.
    if id.stage == 1 && id.room == 0x05 && id.player_flag == 1 && slot == 1 {
        pos[1] = -5;
    }
}

/// The object's 4.12 rotation matrix.
pub fn rotation(object: &ObjectRecord) -> [[i32; 3]; 3] {
    anim::rotation_matrix(
        i32::from(object.rotation[0]),
        i32::from(object.rotation[1]),
        i32::from(object.rotation[2]),
    )
}

/// Compose the object's world matrix: its SVECTOR rotation and its live
/// 32-bit position.
///
/// `lighting` is accepted for the per-object shading the object render path
/// will consume (slice 4); the matrix itself is lighting-independent.
pub fn rebuild(object: &ObjectRecord, lighting: &Lighting) -> Mat4x3 {
    let _ = lighting;
    Mat4x3 {
        r: rotation(object),
        t: object.pos,
    }
}

/// Player SCA height for the 422-unit body radius (Chris, 0x05FA).
pub const CHRIS_HEIGHT: i16 = 0x05FA;
/// Player SCA height for the 372-unit body radius (Jill, 0x0546).
pub const JILL_HEIGHT: i16 = 0x0546;
/// The object reach box of `ChkPlReachEntity`: 470 units in front of the
/// player along the current facing.
pub const OBJECT_REACH_DISTANCE: i32 = 470;

/// The player SCA height matching a collision radius. The two shipped player
/// records pair radius 422 with height 1530 and radius 372 with height 1350.
pub fn player_height(radius: i32) -> i16 {
    if radius == crate::player::CHRIS_RADIUS {
        CHRIS_HEIGHT
    } else {
        JILL_HEIGHT
    }
}

/// The entity-side collision description `ChkEntitySlide` reads.
///
/// The original reaches through the SCA info (+4) for the radius (short 5) and
/// height (short 4) and through the rotated part list (+8) for the part
/// offsets. The player's single part carries `[0, -height, 0]`, exactly the
/// shipped record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityCollision {
    /// Entity flag byte; bit `0x08` disables the entity's collision.
    pub flag: u8,
    /// SCA radius, the horizontal half-extent contribution.
    pub radius: i16,
    /// SCA height, the vertical half-extent contribution.
    pub height: i16,
    /// World-space part offsets.
    pub offsets: [i32; 3],
}

impl EntityCollision {
    /// The player's collision record at `radius`.
    pub fn player(radius: i32) -> Self {
        let height = player_height(radius);
        Self {
            flag: 0,
            radius: radius as i16,
            height,
            offsets: [0, -i32::from(height), 0],
        }
    }
}

/// `ChkEntitySlide` (0x00474330): resolve `ent_pos` against one object along
/// the shallower penetration axis.
///
/// `move_object` selects the mode: `false` pushes the entity out of the object
/// (the response that makes furniture solid), `true` moves the object out of
/// the entity and reports how many axes were moved. The object records have a
/// single collision part, so the original's part walk is one iteration.
pub fn chk_entity_slide(
    ent_pos: &mut [i32; 3],
    ent: EntityCollision,
    obj: &mut ObjectRecord,
    move_object: bool,
) -> u8 {
    if ent.flag & 0x08 != 0 {
        return 0;
    }
    if obj.flag & OBJECT_FLAG_INTANGIBLE != 0 {
        return 0;
    }

    let mut moved = 0u8;
    let obj_x = obj.pos[0];
    let obj_y = obj.pos[1];
    let obj_z = obj.pos[2];

    let dx = (obj_x - ent.offsets[0]) - ent_pos[0];
    let dy = (obj_y - ent.offsets[1]) - ent_pos[1];
    let dz = (obj_z - ent.offsets[2]) - ent_pos[2];

    let ext_x = i32::from(obj.half_extents[0]) + i32::from(ent.radius);
    let ext_y = i32::from(obj.half_extents[1]) + i32::from(ent.height);
    let ext_z = i32::from(obj.half_extents[2]) + i32::from(ent.radius);

    // The unsigned-wrap containment test accepts both sides of the box.
    if (dx.wrapping_add(ext_x) as u32) <= (ext_x.wrapping_mul(2)) as u32
        && (dy.wrapping_add(ext_y) as u32) <= (ext_y.wrapping_mul(2)) as u32
        && (dz.wrapping_add(ext_z) as u32) <= (ext_z.wrapping_mul(2)) as u32
    {
        // Escape along whichever axis has the shallower penetration.
        let cross_x = ext_x.wrapping_mul(dz);
        let cross_z = ext_z.wrapping_mul(dx);
        if cross_x.unsigned_abs() < cross_z.unsigned_abs() {
            let push = if move_object {
                if dx < 0 { -ext_x } else { ext_x }
            } else if dx >= 0 {
                -ext_x
            } else {
                ext_x
            };
            if move_object {
                moved += 1;
                // The entity's own X half-extent is zero for the player.
                obj.pos[0] = ent_pos[0] + push;
            } else {
                ent_pos[0] = obj_x + push;
            }
        } else {
            let push = if move_object {
                if dz < 0 { -ext_z } else { ext_z }
            } else if dz >= 0 {
                -ext_z
            } else {
                ext_z
            };
            if move_object {
                moved += 1;
                obj.pos[2] = ent_pos[2] + push;
            } else {
                ent_pos[2] = obj_z + push;
            }
        }
    }

    moved
}

/// `ChkObjSlide` (0x00474500): shove `other` out of `mover` along the shallower
/// axis, X/Z only. Both records' `0x08` no-collision bits veto. The pushed
/// object parks one unit clear of the touching distance.
pub fn chk_obj_slide(mover: &ObjectRecord, other: &mut ObjectRecord) -> bool {
    if (other.flag | mover.flag) & OBJECT_FLAG_NO_COLLISION != 0 {
        return false;
    }

    let dx = other.pos[0] - mover.pos[0];
    let dz = other.pos[2] - mover.pos[2];
    let ext_x = i32::from(other.half_extents[0]) + i32::from(mover.half_extents[0]);
    let ext_z = i32::from(other.half_extents[2]) + i32::from(mover.half_extents[2]);

    if (dx.wrapping_add(ext_x) as u32) > (ext_x.wrapping_mul(2)) as u32 {
        return false;
    }
    if (dz.wrapping_add(ext_z) as u32) > (ext_z.wrapping_mul(2)) as u32 {
        return false;
    }

    if ext_x.wrapping_mul(dz).unsigned_abs() < ext_z.wrapping_mul(dx).unsigned_abs() {
        let place = if dx < 0 { -1 - ext_x } else { ext_x + 1 };
        other.pos[0] = mover.pos[0] + place;
    } else {
        let place = if dz < 0 { -1 - ext_z } else { ext_z + 1 };
        other.pos[2] = mover.pos[2] + place;
    }
    true
}

/// 16-bit truncating absolute value, the original's `(ushort)` arithmetic.
fn abs16(value: i32) -> u16 {
    let sign = value >> 31;
    ((value ^ sign) - sign) as u16
}

/// `ChkPlReachEntity` (0x00474A20): is `obj` inside the 470-unit reach box in
/// front of the player?
///
/// Returns the (possibly side-zeroed) probe point when it is, `None` otherwise.
/// The two 16-bit comparisons are transcribed exactly: they are unsigned tests
/// of `extent*2` against a signed probe offset, which wrap for probes far to
/// the left. The returned point has the axis nearer the object zeroed, the
/// original's side scratch.
pub fn chk_pl_reach_entity(
    player_pos: [i32; 3],
    angle: u16,
    obj: &ObjectRecord,
) -> Option<[i32; 2]> {
    let (dx, dz) = crate::player::rotate_speed(angle, 0, OBJECT_REACH_DISTANCE);
    let mut probe = [
        dx + i32::from(player_pos[0] as i16),
        dz + i32::from(player_pos[2] as i16),
    ];
    let ext_x = i32::from(obj.half_extents[0] as i16);
    let ext_z = i32::from(obj.half_extents[2] as i16);

    if (ext_x.wrapping_mul(2) as u32) < (ext_x - obj.pos[0] + probe[0]) as u32 {
        return None;
    }
    if (ext_z.wrapping_mul(2) as u32) < (probe[1] - obj.pos[2] + ext_z) as u32 {
        return None;
    }
    if probe[1].abs() < probe[0].abs() {
        probe[1] = 0;
    } else {
        probe[0] = 0;
    }
    Some(probe)
}

/// The `check_climb_object` scan result: the slot of the first climbable record
/// accepted from the last built one down, and the side `attackDirection` the
/// facing picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClimbCandidate {
    /// Object slot the candidate occupies.
    pub slot: usize,
    /// `-1` or `1`, from the facing's half-turn bit.
    pub attack_direction: i8,
}

/// `check_climb_object` (0x00474930) when msf bit 7 is clear: walk the built
/// records from the last down to the first, keeping the first with flag `0x40`
/// that the reach box accepts and whose yaw is within 299/4096 of the player's
/// facing on either wrap side.
pub fn check_climb_object(
    objects: &ObjectTable,
    player_pos: [i32; 3],
    player_angle: u16,
) -> Option<ClimbCandidate> {
    let built = usize::from(objects.built).min(objects.records.len());
    for slot in (0..built).rev() {
        let record = &objects.records[slot];
        if record.flag & OBJECT_FLAG_CLIMBABLE == 0 {
            continue;
        }
        if chk_pl_reach_entity(player_pos, player_angle, record).is_none() {
            continue;
        }
        let angle_diff =
            i32::from((player_angle.wrapping_add(0x800)) & 0x0FFF) - i32::from(record.rotation[1]);
        let diff = abs16(angle_diff);
        if 299 < diff && diff < 0xED5 {
            continue;
        }
        return Some(ClimbCandidate {
            slot,
            attack_direction: if (player_angle.wrapping_add(0x200) & 0x800) == 0 {
                -1
            } else {
                1
            },
        });
    }
    None
}

/// The `check_climb_object` verification branch: with msf bit 7 raised, does
/// the player still face the latched record? `true` settles the climb onto the
/// return side (clear the bit, raise `zoneFlags` `0x10`); `false` cancels.
pub fn verify_climb_object(record: &ObjectRecord, player_angle: u16) -> bool {
    let angle_diff = i32::from(player_angle as i16) - i32::from(record.rotation[1]);
    let diff = abs16(angle_diff);
    !(299 < diff && diff < 0xED5)
}

/// One queued `inst_cfg` (0x30) collision-boundary rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollisionEdit {
    /// Quadrant list (`0..4`).
    pub list: u8,
    /// Record index within the list.
    pub index: u8,
    /// The flag nibble patch; zero leaves the record's flags alone.
    pub flags: u8,
    /// The four box words in opcode order: `z`, `w`, `x`, `y`.
    pub zone: [u16; 4],
}

impl CollisionEdit {
    /// Apply the rewrite to the room's collision table.
    pub fn apply(&self, room: &mut RoomState) -> bool {
        let Some(quadrant) = room.collision.quadrants.get_mut(usize::from(self.list)) else {
            return false;
        };
        let Some(record) = quadrant.get_mut(usize::from(self.index)) else {
            return false;
        };
        record.x_min = self.zone[0];
        record.z_min = self.zone[1];
        record.x_max = self.zone[2];
        record.z_max = self.zone[3];
        if self.flags != 0 {
            record.flags = (record.flags & 0xF0FF) | ((u16::from(self.flags) & 0x000F) << 8);
        }
        true
    }
}

/// Build a collision edit from the `inst_cfg` operand bytes.
///
/// Operands: `+1` list, `+2` record index, `+3` flag nibble, then four `u16`
/// box words `z`, `w`, `x`, `y`.
pub fn collision_edit(operands: &[Operand]) -> CollisionEdit {
    CollisionEdit {
        list: operand_u8(operands, 0),
        index: operand_u8(operands, 1),
        flags: operand_u8(operands, 2),
        zone: [
            operand_u16(operands, 3),
            operand_u16(operands, 4),
            operand_u16(operands, 5),
            operand_u16(operands, 6),
        ],
    }
}

/// One queued `obj_xfm` (0x40) light rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LightEdit {
    /// Index into the room's three lights.
    pub index: u8,
    /// The seven `s16` fields.
    pub fields: [i16; 7],
}

impl LightEdit {
    /// Apply the rewrite to the room's light table.
    ///
    /// The original writes seven 32-bit fields into a wider runtime light
    /// record; the port's `Light` is the 0x14-byte RDT record, so the three
    /// position components take the first three fields and the remaining
    /// four map to the colour bytes and the type word (the record has no
    /// separate field for the last value).
    pub fn apply(&self, room: &mut RoomState) -> bool {
        let Some(light) = room.lights.get_mut(usize::from(self.index)) else {
            return false;
        };
        light.pos = [
            i32::from(self.fields[0]),
            i32::from(self.fields[1]),
            i32::from(self.fields[2]),
        ];
        light.color = [
            self.fields[3] as u8,
            self.fields[4] as u8,
            self.fields[5] as u8,
        ];
        light.kind = self.fields[6] as u16;
        true
    }
}

/// Build a light edit from the `obj_xfm` operand bytes.
///
/// Operands: `+1` light index, then seven `s16` fields.
pub fn light_edit(operands: &[Operand]) -> LightEdit {
    LightEdit {
        index: operand_u8(operands, 0),
        fields: [
            operand_i16(operands, 1),
            operand_i16(operands, 2),
            operand_i16(operands, 3),
            operand_i16(operands, 4),
            operand_i16(operands, 5),
            operand_i16(operands, 6),
            operand_i16(operands, 7),
        ],
    }
}

/// The XZ distance test behind `ck_counter` (0x3C): `sqrt(dx^2 + dz^2)` is
/// within `max_dist`.
pub fn within_distance(player: [i32; 3], target: [i32; 3], max_dist: u16) -> bool {
    let dx = i64::from(player[0]) - i64::from(target[0]);
    let dz = i64::from(player[2]) - i64::from(target[2]);
    let distance = ((dx * dx + dz * dz) as f64).sqrt() as u32;
    distance <= u32::from(max_dist)
}

fn operand_u8(operands: &[Operand], index: usize) -> u8 {
    operands.get(index).map_or(0, |operand| operand.value as u8)
}

fn operand_i16(operands: &[Operand], index: usize) -> i16 {
    operands
        .get(index)
        .map_or(0, |operand| operand.value as i16)
}

fn operand_u16(operands: &[Operand], index: usize) -> u16 {
    operands
        .get(index)
        .map_or(0, |operand| operand.value as u16)
}

/// Read a `u16` from two consecutive single-byte operands.
fn operand_word(operands: &[Operand], index: usize) -> u16 {
    u16::from(operand_u8(operands, index)) | (u16::from(operand_u8(operands, index + 1)) << 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operands(values: &[i64]) -> Vec<Operand> {
        values
            .iter()
            .map(|&value| Operand {
                value,
                target: None,
            })
            .collect()
    }

    /// The 23 decoded operand bytes of one `obj` record.
    fn obj_operands(model: u8, flag: u8, parent: u8) -> Vec<Operand> {
        let mut values = vec![
            i64::from(model),
            i64::from(flag),
            i64::from(parent),
            100,    // X
            -200,   // Y
            300,    // Z
            0x1234, // entry flags
        ];
        values.extend([0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        values.extend([0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00]);
        operands(&values)
    }

    fn table(slots: usize) -> ObjectTable {
        ObjectTable::new(slots)
    }

    #[test]
    fn object_table_records_one_row_per_declared_slot_and_clears() {
        let mut objects = table(3);
        assert_eq!(objects.records.len(), 3);
        assert_eq!(objects.built, 0);

        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        assert!(objects.build(&obj_operands(1, OBJECT_FLAG_ACTIVE, 0xFF), id));
        assert_eq!(objects.built, 1);

        objects.reset(5);
        assert_eq!(objects.records.len(), 5);
        assert_eq!(objects.built, 0);
        assert!(
            objects
                .records
                .iter()
                .all(|record| *record == ObjectRecord::default())
        );
    }

    #[test]
    fn obj_maps_every_operand_field() {
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let mut objects = table(2);
        // Slot 1, push-grunt bit 0x40 set, texture bit 0x80 set.
        let model = 0x80 | 0x40 | 1;
        assert!(objects.build(&obj_operands(model, 0x05, 0xFE), id));

        let record = objects.record(1).unwrap();
        assert_eq!(record.model, 0x40 | 1);
        assert_eq!(record.flag, 0x05);
        assert_eq!(record.parent, 0xFE);
        assert_eq!(record.pos, [100, -200, 300]);
        assert_eq!(record.committed, [100, -200, 300]);
        assert_eq!(record.entry_flags, 0x1234);
        assert_eq!(record.rotation, [0, 0x1234, 0]);
        assert_eq!(record.rotation_rollback, [0, 0x1234, 0]);
        assert_eq!(record.probe, [[0x2211, 0x4433], [0x6655, 0x8877]]);
        assert_eq!(record.radius, 0xAA99);
        assert_eq!(record.half_extents, [0xEEDD, 0xCCBB, 0x00FF]);
        assert_eq!(record.extent_word, 0xCCBB);
        assert_eq!(record.asset, Some(1));
        assert_eq!(record.push_counter, 0);
        assert_eq!(record.tint, [0; 3]);
        assert_eq!(record.light_scale, 0);
        assert_eq!(objects.record(0).unwrap(), &ObjectRecord::default());
    }

    #[test]
    fn obj_ignores_out_of_range_slots_without_counting() {
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let mut objects = table(2);
        assert!(!objects.build(&obj_operands(0x3F, 1, 0), id));
        assert_eq!(objects.built, 0);
        assert!(objects.build(&obj_operands(1, 1, 0), id));
        assert_eq!(objects.built, 1);
    }

    #[test]
    fn obj_position_overrides_match_the_room_slots() {
        let build = |stage: u8, room: u8, player_flag: u8, slot: u8| {
            let id = RoomId {
                stage,
                room,
                player_flag,
            };
            let mut objects = table(2);
            let mut values = vec![i64::from(slot), 1, 0xFF, 100, -200, 300, 0];
            values.extend([0; 16]);
            assert!(objects.build(&operands(&values), id));
            objects.record(usize::from(slot)).unwrap().pos
        };

        assert_eq!(build(4, 0x03, 0, 0), [100 + 0x1E, -210, 300 + 0x82]);
        assert_eq!(build(4, 0x03, 0, 1), [100, -200, 300]);
        assert_eq!(build(4, 0x0F, 0, 0), [100, -200 - 0x15E, 300]);
        assert_eq!(build(1, 0x05, 1, 1), [100, -5, 300]);
        assert_eq!(build(1, 0x05, 0, 1), [100, -200, 300]);
        assert_eq!(build(2, 0x0B, 0, 0), [100, -200, 310]);
        assert_eq!(build(2, 0x0B, 0, 1), [100, -200, 300 - 0x28]);
        assert_eq!(build(7, 0x0B, 0, 0), [100, -200, 310]);
    }

    #[test]
    fn objtbl_b_set_writes_the_flag_byte_and_special_cases_the_water_tank() {
        let id = RoomId {
            stage: 1,
            room: 0,
            player_flag: 0,
        };
        let mut objects = table(6);
        objects.record_mut(0).unwrap().flag = 0x01;
        assert!(objects.set_flag(&operands(&[0, 0, 0x80]), id));
        assert_eq!(objects.record(0).unwrap().flag, 0x80);
        // Item table (1) is a typed no-op.
        assert!(!objects.set_flag(&operands(&[1, 0, 0x40]), id));
        assert_eq!(objects.record(0).unwrap().flag, 0x80);
        // An unknown table is a no-op too.
        assert!(!objects.set_flag(&operands(&[2, 0, 0]), id));

        let water_tank = RoomId {
            stage: 4,
            room: 0x0D,
            player_flag: 0,
        };
        assert!(objects.set_flag(&operands(&[0, 5, 0x40]), water_tank));
        assert_eq!(objects.record(5).unwrap().flag, 0);
        // The same write outside the water tank applies the value.
        assert!(objects.set_flag(&operands(&[0, 5, 0x40]), id));
        assert_eq!(objects.record(5).unwrap().flag, 0x40);
    }

    #[test]
    fn ck_anim_compares_every_mode() {
        let mut objects = table(1);
        objects.record_mut(0).unwrap().push_counter = 10;
        let check = |mode: i64, value: i64| objects.compare_counter(&operands(&[0, mode, value]));

        assert!(check(0, 10));
        assert!(!check(0, 11));
        assert!(check(1, 9));
        assert!(!check(1, 10));
        assert!(check(2, 10));
        assert!(!check(2, 11));
        assert!(check(3, 11));
        assert!(!check(3, 10));
        assert!(check(4, 10));
        assert!(!check(4, 9));
        assert!(check(5, 9));
        assert!(!check(5, 10));
        assert!(!check(6, 10));
        assert!(!objects.compare_counter(&operands(&[1, 0, 10])));
    }

    #[test]
    fn eml_rot_writes_only_an_active_record() {
        let mut objects = table(2);
        objects.record_mut(1).unwrap().flag = 1;
        objects.record_mut(1).unwrap().rotation = [1, 2, 3];

        assert!(objects.rotate(&operands(&[0x80 | 1, 0x1111, 0x2222])));
        assert_eq!(objects.record(1).unwrap().rotation, [0x1111, 2, 0x2222]);

        // An inactive record keeps its rotation.
        assert!(!objects.rotate(&operands(&[0x80, 0x3333, 0x4444])));
        assert_eq!(objects.record(0).unwrap().rotation, [0, 0, 0]);

        // A selector below 0x8000 names the item table; a typed no-op.
        assert!(!objects.rotate(&operands(&[1, 0x5555, 0x6666])));
        assert_eq!(objects.record(1).unwrap().rotation, [0x1111, 2, 0x2222]);
    }

    #[test]
    fn eml_pos_reads_the_index_from_the_first_operand_byte() {
        let mut objects = table(2);
        objects.record_mut(1).unwrap().flag = 0;
        assert!(objects.transform(&operands(&[1, 0x10, -0x20, 0x30, -100, 200, -300])));
        let record = objects.record(1).unwrap();
        assert_eq!(record.rotation, [0x10, -0x20, 0x30]);
        assert_eq!(record.pos, [-100, 200, -300]);
        assert_eq!(record.committed, [-100, 200, -300]);
        // The rotation is written even on an inactive record.
        assert_eq!(objects.record(1).unwrap().flag, 0);
    }

    #[test]
    fn rebuild_composes_rotation_and_position() {
        let mut objects = table(1);
        objects.record_mut(0).unwrap().pos = [10, 20, 30];
        objects.record_mut(0).unwrap().rotation = [0, 0, 0];
        let lighting = Lighting {
            ambient: [0; 3],
            lights: [crate::state::Light::default(); 3],
        };
        let matrix = rebuild(objects.record(0).unwrap(), &lighting);
        assert_eq!(matrix.t, [10, 20, 30]);
        // The trig tables saturate, so a zero triple is near-identity.
        assert_eq!(matrix.r[0][0], 4095);
        assert_eq!(matrix.r[1][1], 4095);
        assert_eq!(matrix.r[2][2], 4095);
    }

    #[test]
    fn collision_edit_rewrites_the_box_and_flag_nibble() {
        let mut room = RoomState::default();
        room.collision.quadrants[0].push(crate::state::CollisionRect {
            x_max: 1,
            z_max: 2,
            x_min: 3,
            z_min: 4,
            kind: 5,
            flags: 0xF123,
        });
        let edit = collision_edit(&operands(&[0, 0, 0x05, 0x11, 0x22, 0x33, 0x44]));
        assert_eq!(
            edit,
            CollisionEdit {
                list: 0,
                index: 0,
                flags: 0x05,
                zone: [0x11, 0x22, 0x33, 0x44],
            }
        );
        assert!(edit.apply(&mut room));
        let record = &room.collision.quadrants[0][0];
        assert_eq!(record.x_min, 0x11);
        assert_eq!(record.z_min, 0x22);
        assert_eq!(record.x_max, 0x33);
        assert_eq!(record.z_max, 0x44);
        assert_eq!(record.flags, 0xF523);

        // A zero flag byte leaves the flags alone.
        let edit = collision_edit(&operands(&[0, 0, 0, 1, 2, 3, 4]));
        assert!(edit.apply(&mut room));
        assert_eq!(room.collision.quadrants[0][0].flags, 0xF523);

        // Out-of-range targets are refused.
        let edit = collision_edit(&operands(&[0, 9, 0, 0, 0, 0, 0]));
        assert!(!edit.apply(&mut room));
        let edit = collision_edit(&operands(&[4, 0, 0, 0, 0, 0, 0]));
        assert!(!edit.apply(&mut room));
    }

    #[test]
    fn light_edit_rewrites_the_selected_light() {
        let mut room = RoomState::default();
        room.lights[1] = crate::state::Light {
            pos: [1, 2, 3],
            color: [4, 5, 6],
            kind: 7,
            radius: 8,
        };
        let edit = light_edit(&operands(&[1, 100, -200, 300, 0x44, 0x55, 0x66, 0x77]));
        assert_eq!(
            edit,
            LightEdit {
                index: 1,
                fields: [100, -200, 300, 0x44, 0x55, 0x66, 0x77],
            }
        );
        assert!(edit.apply(&mut room));
        let light = room.lights[1];
        assert_eq!(light.pos, [100, -200, 300]);
        assert_eq!(light.color, [0x44, 0x55, 0x66]);
        assert_eq!(light.kind, 0x77);
        assert_eq!(light.radius, 8);
        assert_eq!(room.lights[0], crate::state::Light::default());

        assert!(!light_edit(&operands(&[4, 0, 0, 0, 0, 0, 0, 0])).apply(&mut room));
    }

    fn collision_record(pos: [i32; 3], extents: [u16; 3]) -> ObjectRecord {
        ObjectRecord {
            flag: OBJECT_FLAG_ACTIVE,
            pos,
            half_extents: extents,
            ..ObjectRecord::default()
        }
    }

    #[test]
    fn chk_entity_slide_pushes_the_entity_out_on_the_shallow_axis() {
        // Object centre 100 to the right, X extents sum 200 and Z sum 300: the
        // overlap is shallower on X, so the entity exits on X to the left.
        let mut obj = collision_record([500, 0, 0], [100, 100, 150]);
        let ent = EntityCollision {
            flag: 0,
            radius: 100,
            height: 500,
            offsets: [0, -500, 0],
        };
        let mut pos = [400, 0, 0];
        assert_eq!(chk_entity_slide(&mut pos, ent, &mut obj, false), 0);
        // The entity exits to objX - extX = 300.
        assert_eq!(pos, [300, 0, 0]);
        assert_eq!(obj.pos, [500, 0, 0], "mode 0 never moves the object");

        let mut pos = [400, 0, 0];
        assert_eq!(chk_entity_slide(&mut pos, ent, &mut obj, true), 1);
        // entX + extX = 400 + 200 = 600.
        assert_eq!(obj.pos, [600, 0, 0]);
        assert_eq!(pos, [400, 0, 0], "mode 1 leaves the entity alone");
    }

    #[test]
    fn chk_entity_slide_resolves_along_z_when_z_is_shallower() {
        // dx = 300, dz = 10; extX = 200, extZ = 150. |extX*dz| = 2000 <
        // |extZ*dx| = 45000, so the shallower axis is X again. Swap to make Z
        // shallow: dx = 10, dz = 140 with extX = 300, extZ = 150.
        let mut obj = collision_record([10, 0, 140], [200, 100, 100]);
        let ent = EntityCollision {
            flag: 0,
            radius: 100,
            height: 100,
            offsets: [0, -100, 0],
        };
        let mut pos = [0, 0, 0];
        // |extX*dz| = 300*140 = 42000, |extZ*dx| = 200*10 = 2000 -> Z axis.
        assert_eq!(chk_entity_slide(&mut pos, ent, &mut obj, false), 0);
        // objZ - extZ = 140 - 200 = -60.
        assert_eq!(pos, [0, 0, -60]);
    }

    #[test]
    fn chk_entity_slide_honours_the_flag_bits_and_y_extent() {
        let mut obj = collision_record([100, 0, 0], [100, 100, 100]);
        let mut pos = [100, 0, 0];
        let mut ent = EntityCollision {
            flag: 0x08,
            radius: 50,
            height: 1234,
            offsets: [0, -1234, 0],
        };
        assert_eq!(chk_entity_slide(&mut pos, ent, &mut obj, false), 0);
        assert_eq!(pos, [100, 0, 0], "a collision-disabled entity never moves");

        ent.flag = 0;
        obj.flag |= OBJECT_FLAG_INTANGIBLE;
        assert_eq!(chk_entity_slide(&mut pos, ent, &mut obj, false), 0);
        obj.flag &= !OBJECT_FLAG_INTANGIBLE;

        // The entity's Y offset lifts the tested centre by its height: with
        // the object 2000 above the entity the boxes miss, without the offset
        // they overlap.
        obj.pos = [100, 1200, 0];
        pos = [0, 0, 0];
        assert_eq!(chk_entity_slide(&mut pos, ent, &mut obj, false), 0);
        assert_eq!(pos, [0, 0, 0], "the height offset lifts the test clear");
        ent.offsets = [0, 0, 0];
        assert_eq!(chk_entity_slide(&mut pos, ent, &mut obj, false), 0);
        assert_ne!(pos, [0, 0, 0], "without the offset the boxes overlap");
    }

    #[test]
    fn chk_obj_slide_parks_the_other_object_one_unit_clear() {
        let mover = collision_record([0, 0, 0], [100, 100, 100]);
        let mut other = collision_record([150, 0, 0], [100, 100, 100]);
        // extX = 200; dx = 150 inside; |extX*dz| = 0 < |extZ*dx| = 30000 -> X.
        assert!(chk_obj_slide(&mover, &mut other));
        assert_eq!(other.pos[0], 201, "dx >= 0 parks at extX + 1");

        let mover = collision_record([0, 0, 0], [100, 100, 100]);
        let mut other = collision_record([-150, 0, 0], [100, 100, 100]);
        assert!(chk_obj_slide(&mover, &mut other));
        assert_eq!(other.pos[0], -201, "dx < 0 parks at -1 - extX");

        // Z axis when X is not penetrating.
        let mover = collision_record([0, 0, 0], [100, 100, 100]);
        let mut other = collision_record([0, 0, 150], [100, 100, 100]);
        assert!(chk_obj_slide(&mover, &mut other));
        assert_eq!(other.pos[2], 201);

        // The 0x08 bit on either record vetoes.
        let mut mover = collision_record([0, 0, 0], [100, 100, 100]);
        mover.flag |= OBJECT_FLAG_NO_COLLISION;
        let mut other = collision_record([0, 0, 0], [100, 100, 100]);
        assert!(!chk_obj_slide(&mover, &mut other));
        assert_eq!(other.pos, [0, 0, 0]);

        // Outside the box: no shove.
        let mover = collision_record([0, 0, 0], [10, 10, 10]);
        let mut other = collision_record([1000, 0, 0], [10, 10, 10]);
        assert!(!chk_obj_slide(&mover, &mut other));
    }

    #[test]
    fn chk_pl_reach_entity_uses_the_470_unit_box_and_side_scratch() {
        let obj = collision_record([400, 0, 0], [100, 100, 100]);
        let probe = chk_pl_reach_entity([0, 0, 0], 0, &obj).unwrap();
        // The probe is (470, 0) and the larger axis zeroes: z stays 0.
        assert_eq!(probe, [469, 0]);

        // Behind the player the first test wraps out.
        let behind = collision_record([-600, 0, 0], [100, 100, 100]);
        assert!(chk_pl_reach_entity([0, 0, 0], 0, &behind).is_none());

        // Farther than the reach plus the extent.
        let far = collision_record([700, 0, 0], [100, 100, 100]);
        assert!(chk_pl_reach_entity([0, 0, 0], 0, &far).is_none());

        // Straight behind the player (0x400 faces -Z): X is zeroed.
        let north = collision_record([0, 0, -400], [50, 100, 100]);
        let probe = chk_pl_reach_entity([0, 0, 0], 0x400, &north).unwrap();
        assert_eq!(probe, [0, -470]);

        // A diagonal probe keeps only its dominant axis (X here).
        let diagonal = collision_record([400, 0, -100], [100, 100, 100]);
        let probe = chk_pl_reach_entity([0, 0, 0], 0x100, &diagonal).unwrap();
        assert_eq!(probe[1], 0);
        assert!(probe[0] > 400, "x probe {probe:?}");
    }

    #[test]
    fn check_climb_object_scans_backwards_and_checks_the_yaw_window() {
        let mut objects = ObjectTable::new(3);
        // Slot 1 is climbable; slot 2 is not active.
        objects.records[1] = collision_record([400, 0, 0], [100, 100, 100]);
        objects.records[1].flag |= OBJECT_FLAG_CLIMBABLE;
        objects.records[1].rotation[1] = 0x800;
        objects.records[0] = collision_record([400, 0, 0], [100, 100, 100]);
        objects.records[0].flag |= OBJECT_FLAG_CLIMBABLE;
        objects.records[0].rotation[1] = 0x800;
        objects.built = 2;

        let candidate = check_climb_object(&objects, [0, 0, 0], 0).unwrap();
        assert_eq!(candidate.slot, 1, "the scan keeps the last built record");
        assert_eq!(candidate.attack_direction, -1, "angle + 0x200 bit clear");

        // Within the +299 window the last record still wins.
        objects.records[1].rotation[1] = 0x800 + 299;
        assert_eq!(check_climb_object(&objects, [0, 0, 0], 0).unwrap().slot, 1);
        objects.records[1].rotation[1] = 0x800 + 300;
        assert_eq!(
            check_climb_object(&objects, [0, 0, 0], 0).unwrap().slot,
            0,
            "300/4096 is outside the window"
        );

        // The far wrap side: exactly 0xED5 is accepted.
        objects.records[1].rotation[1] = (0x800 + 0xED5) as i16;
        assert!(check_climb_object(&objects, [0, 0, 0], 0).is_some());
        objects.records[1].rotation[1] = (0x800 + 0xED5 - 1) as i16;
        assert_eq!(
            check_climb_object(&objects, [0, 0, 0], 0).unwrap().slot,
            0,
            "0xED4 is inside the window"
        );

        // A non-climbable nearer record is skipped.
        objects.records[1].flag &= !OBJECT_FLAG_CLIMBABLE;
        assert_eq!(check_climb_object(&objects, [0, 0, 0], 0).unwrap().slot, 0);

        // Attack direction flips with the facing's half-turn bit. The scan
        // compares the facing turned by 0x800 with the object's yaw, so a
        // player at 0x800 matches yaw 0.
        objects.records[0].pos = [-400, 0, 0];
        objects.records[0].rotation[1] = 0;
        let candidate = check_climb_object(&objects, [0, 0, 0], 0x800).unwrap();
        assert_eq!(candidate.attack_direction, 1);
    }

    #[test]
    fn verify_climb_object_cancels_on_drift() {
        let mut record = collision_record([400, 0, 0], [100, 100, 100]);
        record.rotation[1] = 0x100;
        assert!(verify_climb_object(&record, 0x100));
        assert!(verify_climb_object(&record, 0x100 + 0xED5));
        assert!(!verify_climb_object(&record, 0x100 + 300));
        assert!(!verify_climb_object(&record, 0x100u16.wrapping_sub(300)));
    }

    #[test]
    fn object_shade_defaults_to_white_and_applies_deltas() {
        let mut record = collision_record([0, 0, 0], [1, 1, 1]);
        assert_eq!(record.shade(), [255, 255, 255]);
        record.tint = [-1, -1, -1];
        assert_eq!(record.shade(), [247, 247, 247]);
        record.light_scale = -4;
        assert_eq!(record.shade(), [215, 215, 215]);
        record.tint = [-128, 0, 127];
        assert_eq!(record.shade(), [0, 223, 255]);
    }

    #[test]
    fn within_distance_uses_xz_only() {
        assert!(within_distance([0, 0, 0], [3, 1000, 4], 5));
        assert!(!within_distance([0, 0, 0], [3, 0, 4], 4));
        assert!(within_distance([-3, 0, -4], [0, 0, 0], 5));
    }
}
