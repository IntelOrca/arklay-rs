//! The custom skeleton animators.
//!
//! Most monsters advance their pose with the shared `Joint_move` clock
//! ([`crate::enemy::EntityAnim::advance`]). Plant 42 does not: its own
//! animator always poses the skeleton, sub-frame interpolates once the blend
//! counter has run out but the frame hold has not, consumes the blend counter
//! *before* it is used, and only advances the clip frame on the tick the hold
//! reaches zero. [`plant42_advance`] is that clock, expressed against the same
//! entity words and keyframe table so the script and renderer read exactly the
//! words the original's handlers compare.
//!
//! The other two custom animators (Yawn's fixed-point chain and the Tyrant's
//! root-motion extractor) land with their own groups; this module is where
//! they will live.

use crate::anim::{AnimPlayer, Mat4x3, blend_keyframes};
use crate::enemy::EntityAnim;
use crate::game::Entity;
use crate::model::{Clip, Keyframe, Skeleton};
use crate::state::RoomState;

/// The Plant 42 animator's clock step. See the module comment for how it
/// differs from the shared `Joint_move` clock; the blend arithmetic is the
/// original's:
///
/// - `timing_control` decrements first, every tick;
/// - the blend counter is read and then decremented, every tick;
/// - when the counter was already zero and the hold has not expired, the
///   blend weight becomes `(0x1000 / blend_step) / (timing_control + 1)`;
/// - the pose is always written: a zero blend snaps to the target frame's
///   keyframe, otherwise the pose on screen is interpolated with the same
///   `blend_keyframes` weights the shared clock uses;
/// - the frame only advances when `timing_control` reaches zero, reloading
///   the target frame's own hold.
pub fn plant42_advance(
    clock: &mut EntityAnim,
    entity: &mut Entity,
    clips: &[Clip],
    reverse: bool,
    blend_step: u16,
) -> bool {
    clock.sync(entity);
    let keyframes = clock.keyframes.clone();
    let keyframes: &[Keyframe] = keyframes.as_deref().map_or(&[], Vec::as_slice);
    let player = &mut clock.player;
    player.reverse = reverse;
    player.blend_step = blend_step;
    player.blend_counter = u16::from(entity.blend_counter);
    let completed = plant42_step(player, clips, keyframes, blend_step);
    entity.animation_frame_id = player.frame as u8;
    entity.timing_control = player.timing.min(u16::from(u8::MAX)) as u8;
    entity.blend_counter = player.blend_counter.min(u16::from(u8::MAX)) as u8;
    completed
}

/// One tick of [`plant42_advance`] against a bare clock (the entity words are
/// the caller's sync/publish).
fn plant42_step(
    player: &mut AnimPlayer,
    clips: &[Clip],
    keyframes: &[Keyframe],
    blend_step: u16,
) -> bool {
    // The hold counts down before anything else, every tick.
    if player.timing != 0 {
        player.timing -= 1;
    }

    // The blend counter is consumed before it is used, every tick.
    let mut blend = player.blend_counter;
    if blend != 0 {
        player.blend_counter -= 1;
    }

    let Some(clip) = clips.get(player.clip) else {
        player.frame = 0;
        player.display_frame = 0;
        player.timing = 0;
        return false;
    };
    if clip.frames.is_empty() {
        player.frame = 0;
        player.display_frame = 0;
        player.timing = 0;
        return false;
    }

    let index = if player.frame < clip.frames.len() {
        player.frame
    } else {
        0
    };
    let data = if player.reverse {
        clip.frames[clip.frames.len() - 1 - index]
    } else {
        clip.frames[index]
    };

    // The sub-frame blend: only once the counter has run out and the hold is
    // still running. `blend_step` is never zero at a shipped call site; a
    // zero would fault in the original and snaps here.
    if blend == 0 && player.timing != 0 && blend_step != 0 {
        blend = ((0x1000u32 / u32::from(blend_step)) / (u32::from(player.timing) + 1))
            .min(u32::from(u8::MAX)) as u16;
    }

    if let Some(target) = keyframes.get(usize::from(data.keyframe)) {
        let pose = if blend == 0 {
            target.clone()
        } else if let Some(source) = player.applied_pose() {
            blend_keyframes(&source, target, blend, blend_step)
        } else {
            target.clone()
        };
        player.set_applied_pose(Some(pose));
    }
    player.display_frame = index;

    // The frame advances only when the hold has expired.
    if player.timing == 0 {
        player.timing = data.timing;
        player.frame = index + 1;
        if player.frame >= clip.frames.len() {
            player.frame = 0;
            return true;
        }
    }
    false
}

/// `ScaleMatrixCols`: scale the rotation columns of `matrix` by `[x, y, z]`
/// in 4.12 fixed point, each component `(value * scale) >> 12` truncated
/// toward zero and stored back at the original's 16-bit width. The
/// translation is untouched.
pub fn scale_columns_xyz(matrix: &mut Mat4x3, scale: [i32; 3]) {
    for (column, factor) in scale.iter().enumerate() {
        for row in matrix.r.iter_mut() {
            let product = row[column] * factor;
            row[column] = i32::from(((product + ((product >> 31) & 0xFFF)) >> 12) as i16);
        }
    }
}

/// The original's `plant42_orient_from_matrix`: derive a rotation matrix from
/// a fixed 3x3 orientation (the Chris hold matrix) using the game's own
/// integer atan2/sqrt helpers. `m` is the 3x3 short block in the original's
/// storage order (`m[row + col * 3]`).
pub fn orient_from_matrix(m: [i16; 9]) -> Mat4x3 {
    let c0 = i32::from(m[2]);
    let c1 = i32::from(m[5]);
    let c2 = i32::from(m[8]);
    let len = c2 * c2 + c0 * c0 + c1 * c1;
    let len = (c0 << 12) / ((len + (len >> 31 & 0xFFF)) >> 12);

    let mut rot = [0i16; 3];
    rot[0] = gte_atan2_int(-c1, c2) as i16;
    let sq = len * len;
    rot[1] = gte_atan2_int(
        len,
        gte_fsqrt_int(0x1000 - ((sq + (sq >> 31 & 0xFFF)) >> 12)),
    ) as i16;

    let mut rm = crate::anim::rotation_matrix(i32::from(rot[0]), i32::from(rot[1]), 0);
    // The original transposes the rotation, then transforms the matrix's
    // first column by it and derives the Z rotation from the result.
    let transposed = [
        [rm[0][0], rm[1][0], rm[2][0]],
        [rm[0][1], rm[1][1], rm[2][1]],
        [rm[0][2], rm[1][2], rm[2][2]],
    ];
    let axis = [i32::from(m[0]), i32::from(m[3]), i32::from(m[6])];
    let mut out_axis = [0i32; 3];
    for (row, value) in out_axis.iter_mut().enumerate() {
        let mut sum = 0i64;
        for (k, element) in transposed[row].iter().enumerate() {
            sum += i64::from(*element) * i64::from(axis[k]);
        }
        *value = ((sum + ((sum >> 63) & 0xFFF)) >> 12) as i32;
    }
    rot[2] = gte_atan2_int(out_axis[1], out_axis[0]) as i16;
    rm = crate::anim::rotation_matrix(i32::from(rot[0]), i32::from(rot[1]), i32::from(rot[2]));
    Mat4x3 { r: rm, t: [0; 3] }
}

/// The inverse of [`crate::anim::compose`]: `compose(a, compose_inverse(a))`
/// is the identity in the game's composition algebra (rotation transpose plus
/// the Y-sign conjugation the compose applies to translations).
pub fn compose_inverse(a: &Mat4x3) -> Mat4x3 {
    let r = [
        [a.r[0][0], a.r[1][0], a.r[2][0]],
        [a.r[0][1], a.r[1][1], a.r[2][1]],
        [a.r[0][2], a.r[1][2], a.r[2][2]],
    ];
    // Solve `D * (a.r * (D * t)) + a.t = 0`: `t = D * (a.r^T * (-D * a.t))`.
    let source = [a.t[0].wrapping_neg(), a.t[1], a.t[2].wrapping_neg()];
    let mut t = [0i32; 3];
    for (row, value) in t.iter_mut().enumerate() {
        let mut sum = 0i64;
        for (element, &input) in r[row].iter().zip(&source) {
            sum += i64::from(*element) * i64::from(input);
        }
        *value = ((sum + ((sum >> 63) & 0xFFF)) >> 12) as i32;
    }
    t[1] = t[1].wrapping_neg();
    Mat4x3 { r, t }
}

/// `plant42_capture_setup`: build the capture matrix from a grabbing joint's
/// world matrix and the player's local matrix, so
/// [`hold_player`] can re-apply the player's relative transform to the
/// (moved) joint.
pub fn capture_setup(joint: &Mat4x3, player: &Mat4x3) -> Mat4x3 {
    crate::anim::compose(&compose_inverse(joint), player)
}

/// `plant42_hold_player`: the player's local matrix under the capture
/// transform, `joint * capture`.
pub fn hold_player(joint: &Mat4x3, capture: &Mat4x3) -> Mat4x3 {
    crate::anim::compose(joint, capture)
}

// ============================================================================
// The Tyrant's root-motion extractor.
//
// Unlike the other custom animators this one does not pose anything: it reads
// the pose the shared clock just applied and converts the animation's own
// translation into the entity's move speed, optionally subtracting it from the
// entity position. The walk, thrust and impale slides all depend on it, and
// the two clip sets (walk and run) are selected by the caller.
// ============================================================================

/// `tyrant_root_motion`: compose the entity's yaw with the claw chain's local
/// transforms, subtract the chain end's previous-frame world translation, and
/// derive the frame's planar speed from what is left. `set` selects the walk
/// (0) or run (1) clip set: transform 0 first, then the three chain
/// transforms 9/10/11 (walk) or 12/13/14 (run). With `apply` the leftover XZ
/// delta is subtracted from the entity position, which is what slides the
/// body along the lunge animations.
///
/// Returns the speed word stored in `move_speed_current` (already written),
/// or the untouched speed when the entity has no pose.
pub fn tyrant_root_motion(
    game: &mut crate::game::GameState,
    slot: usize,
    clips: &[Clip],
    set: u8,
    apply: bool,
) -> u16 {
    let entity = game.entities[slot];
    let transforms = game.entity_anims[slot].local_transforms(&entity, clips);
    if transforms.is_empty() {
        return entity.move_speed_current;
    }

    let mut scratch = crate::anim::compose(
        &crate::anim::entity_matrix_rotated(entity.pos, entity.pitch, entity.angle, entity.roll),
        &transforms[0],
    );
    if entity.joint_scale != 0 {
        let scale = i32::from(entity.joint_scale);
        scale_columns_xyz(&mut scratch, [scale, scale, scale]);
    }
    let base = 11 + usize::from(set) * 3;
    for index in [base - 2, base - 1, base] {
        if let Some(transform) = transforms.get(index) {
            scratch = crate::anim::compose(&scratch, transform);
        }
    }

    if let Some(world) = game.joint_worlds[slot].get(base) {
        scratch.t[0] = scratch.t[0].wrapping_sub(world.t[0]);
        scratch.t[2] = scratch.t[2].wrapping_sub(world.t[2]);
    }
    scratch.t[1] = 0;

    let speed = sqrt0(
        scratch.t[2]
            .wrapping_mul(scratch.t[2])
            .wrapping_add(scratch.t[0].wrapping_mul(scratch.t[0])),
    ) as u16;
    let entity = &mut game.entities[slot];
    entity.move_speed_current = speed;
    if apply {
        entity.pos[0] = entity.pos[0].wrapping_sub(scratch.t[0]);
        entity.pos[2] = entity.pos[2].wrapping_sub(scratch.t[2]);
    }
    speed
}

/// `0x0040a990`: the game's integer atan2 in 12-bit angle units.
fn gte_atan2_int(y: i32, x: i32) -> i32 {
    ((y as f64).atan2(x as f64) * 57.295_777_918_682_04 * 11.377_777_777_777_778) as i32
}

/// `0x0040a530`: the game's integer square root, `sqrt(value / 4096) * 4096`.
fn gte_fsqrt_int(value: i32) -> i32 {
    if value < 0 {
        return 0;
    }
    ((value as f64 * 0.000_244_140_625).sqrt() * 4096.0) as i32
}

// ============================================================================
// Yawn's fixed-point chain animator.
//
// Yawn's fifteen joints are not posed by the shared clock: joints 0-2 take the
// animation directly, joints 3-14 stack yaw and roll onto the previous joint,
// the whole chain is accumulated in `<<9` fixed point, and the body is
// anchored so joint 13 lands back on its own previous world position, with the
// leftover XZ delta moving the entity. The pose below is the port's model of
// the original's per-joint `JointStruct` state (rotation vector, local
// transform matrix, world matrix and the previous-frame yaw scratch the
// animator keeps in the joint's velocity word).
// ============================================================================

/// The number of joints Yawn's model carries.
pub const YAWN_JOINTS: usize = 15;

/// One Yawn's persistent skeleton state: the original's fifteen joint structs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YawnPose {
    /// Each joint's rotation triple (`JointStruct.rotation`).
    pub rotation: [[i16; 3]; YAWN_JOINTS],
    /// Each joint's local transform matrix (`JointStruct.transform`).
    pub transform: [Mat4x3; YAWN_JOINTS],
    /// Each joint's world matrix (`JointStruct.world`).
    pub world: [Mat4x3; YAWN_JOINTS],
    /// The previous frame's animation yaw per joint (the animator's `velY`
    /// scratch).
    pub prev_yaw: [i16; YAWN_JOINTS],
}

impl Default for YawnPose {
    fn default() -> Self {
        YawnPose {
            rotation: [[0; 3]; YAWN_JOINTS],
            transform: [Mat4x3::default(); YAWN_JOINTS],
            world: [Mat4x3::default(); YAWN_JOINTS],
            prev_yaw: [0; YAWN_JOINTS],
        }
    }
}

impl YawnPose {
    /// The world matrices the renderer draws (the original's render pass
    /// recomposes joints 0-2 from their transforms every frame but leaves the
    /// chain joints 3-14 alone, because Yawn's init marks them so).
    pub fn render_worlds(&self, skeleton: &Skeleton, entity: &Entity) -> Vec<Mat4x3> {
        let mut worlds = self.world.to_vec();
        let matrix =
            crate::anim::entity_matrix_rotated(entity.pos, entity.pitch, entity.angle, entity.roll);
        let standard = compose_from_transforms(skeleton, &self.transform, &matrix);
        for (index, joint) in standard.into_iter().enumerate().take(3) {
            if let Some(slot) = worlds.get_mut(index) {
                *slot = joint;
            }
        }
        worlds
    }
}

/// Compose the standard joint hierarchy from per-joint transform matrices:
/// the original's `EntityComputeJointWorldMatrices` walk. The parent of each
/// joint comes from the skeleton's child lists, exactly like
/// [`crate::anim::joint_matrices`].
pub fn compose_from_transforms(
    skeleton: &Skeleton,
    transforms: &[Mat4x3],
    entity: &Mat4x3,
) -> Vec<Mat4x3> {
    let count = skeleton.relative.len();
    let mut parent: Vec<Option<usize>> = vec![None; count];
    for (index, children) in skeleton.children.iter().enumerate().take(count) {
        for &child in children {
            let child = usize::from(child);
            if child < count && child != index && parent[child].is_none() {
                parent[child] = Some(index);
            }
        }
    }
    let identity = Mat4x3 {
        r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
        t: [0; 3],
    };
    let mut world = vec![identity; count];
    let mut resolved = vec![false; count];
    let mut unresolved: Vec<usize> = (0..count).collect();
    while !unresolved.is_empty() {
        let mut remaining = Vec::new();
        for &index in &unresolved {
            let local = transforms.get(index).copied().unwrap_or(identity);
            match parent[index] {
                Some(parent_index) if resolved[parent_index] => {
                    world[index] = crate::anim::compose(&world[parent_index], &local);
                    resolved[index] = true;
                }
                Some(_) => remaining.push(index),
                None => {
                    world[index] = crate::anim::compose(entity, &local);
                    resolved[index] = true;
                }
            }
        }
        if remaining.len() == unresolved.len() {
            for &index in &remaining {
                let local = transforms.get(index).copied().unwrap_or(identity);
                world[index] = crate::anim::compose(entity, &local);
                resolved[index] = true;
            }
            break;
        }
        unresolved = remaining;
    }
    world
}

/// `yawn_pose_init`: the one-shot pose the head's init lays down before the
/// twelve segment clones read their joints. Joints 0-2 are posed straight
/// from the keyframe, joints 3-14 stack yaw/roll onto the previous joint,
/// and the worlds compose in normal (not `<<9`) scale.
pub fn yawn_pose_init(
    pose: &mut YawnPose,
    entity: &Entity,
    skeleton: &Skeleton,
    keyframe: &Keyframe,
) {
    let relative = |index: usize| -> [i32; 3] {
        skeleton
            .relative
            .get(index)
            .map_or([0; 3], |value| value.map(i32::from))
    };
    for index in 0..YAWN_JOINTS {
        let rotation = keyframe.rotations.get(index).copied().unwrap_or([0; 3]);
        if index < 3 {
            pose.rotation[index] = rotation;
            pose.transform[index] = Mat4x3 {
                r: crate::anim::rotation_matrix(
                    i32::from(rotation[0]),
                    i32::from(rotation[1]),
                    i32::from(rotation[2]),
                ),
                t: relative(index),
            };
        } else {
            pose.prev_yaw[index] = rotation[1];
            pose.rotation[index][0] = 0;
            if index == 3 {
                pose.rotation[index][1] = entity.angle.wrapping_add(rotation[1] as u16) as i16;
                let roll = pose.rotation[0][2].wrapping_add(pose.rotation[2][2]);
                pose.rotation[index][2] = roll.wrapping_add(rotation[2]);
            } else {
                pose.rotation[index][1] = pose.rotation[index - 1][1].wrapping_add(rotation[1]);
                pose.rotation[index][2] = pose.rotation[index - 1][2].wrapping_add(rotation[2]);
            }
            if index == 7 {
                pose.rotation[index][2] = 0;
            }
            pose.transform[index] = Mat4x3 {
                r: crate::anim::rotation_matrix(
                    i32::from(pose.rotation[index][0]),
                    i32::from(pose.rotation[index][1]),
                    i32::from(pose.rotation[index][2]),
                ),
                t: relative(index),
            };
        }
    }
    // The root joint's Y is the keyframe's root translation, not the armature
    // relative; the init has already cleared the root's local X/Z, so only Y
    // survives.
    pose.transform[0].t = [0, i32::from(keyframe.offset[1]), 0];

    let matrix =
        crate::anim::entity_matrix_rotated(entity.pos, entity.pitch, entity.angle, entity.roll);
    pose.world[0] = crate::anim::compose(&matrix, &pose.transform[0]);
    pose.world[1] = crate::anim::compose(&pose.world[0], &pose.transform[1]);
    pose.world[2] = crate::anim::compose(&pose.world[0], &pose.transform[2]);
    for index in 3..YAWN_JOINTS {
        let scratch = apply_matrix_lv(
            &pose.world[index - 1],
            pose.transform[index].t.map(i32::from),
        );
        pose.world[index].r = crate::anim::rotation_matrix(
            i32::from(pose.rotation[index][0]),
            i32::from(pose.rotation[index][1]),
            i32::from(pose.rotation[index][2]),
        );
        for (axis, offset) in scratch.iter().enumerate() {
            pose.world[index].t[axis] = pose.world[index - 1].t[axis].wrapping_add(*offset);
        }
    }
}

/// `yawn_anim_advance`: one tick of Yawn's own skeleton clock. Returns
/// whether the clip wrapped, the value every caller adds into `action_state`.
pub fn yawn_advance(
    clock: &mut EntityAnim,
    entity: &mut Entity,
    clips: &[Clip],
    reverse: bool,
    blend_step: u16,
) -> bool {
    let Some(keyframes) = clock.keyframes.clone() else {
        return false;
    };
    if clock.yawn.is_none() {
        return false;
    }
    if entity.timing_control > 1 {
        entity.timing_control -= 1;
        return false;
    }
    let Some(clip) = clips.get(usize::from(entity.animation_id)) else {
        return false;
    };
    if clip.frames.is_empty() {
        return false;
    }
    let count = clip.frames.len();
    let frame_id = usize::from(entity.animation_frame_id).min(count - 1);
    let index = if reverse {
        count - 1 - frame_id
    } else {
        frame_id
    };
    let data = clip.frames[index];
    let Some(keyframe) = keyframes.get(usize::from(data.keyframe)) else {
        return false;
    };

    let blend = entity.blend_counter;
    let step = i32::from(blend_step);
    let inverse = if step != 0 { 0x1000 / step } else { 0 };
    let pose = clock.yawn.as_deref_mut().expect("checked above");

    // ---- joints 0..2: posed straight from the animation ----
    for joint in 0..3 {
        let target = keyframe.rotations.get(joint).copied().unwrap_or([0; 3]);
        if blend == 0 {
            pose.rotation[joint] = target;
        } else {
            let w_current = i32::from(blend) * step;
            let w_target = (inverse - i32::from(blend)) * step;
            pose.rotation[joint] = fp_lerp(pose.rotation[joint], target, w_current, w_target);
        }
        pose.transform[joint].r = crate::anim::rotation_matrix(
            i32::from(pose.rotation[joint][0]),
            i32::from(pose.rotation[joint][1]),
            i32::from(pose.rotation[joint][2]),
        );
    }
    pose.transform[0].t[1] = i32::from(keyframe.offset[1]);

    // The entity's local matrix and joints 0/2's worlds, before the chain.
    let matrix =
        crate::anim::entity_matrix_rotated(entity.pos, entity.pitch, entity.angle, entity.roll);
    pose.world[0] = crate::anim::compose(&matrix, &pose.transform[0]);
    pose.world[1] = crate::anim::compose(&pose.world[0], &pose.transform[1]);
    pose.world[2] = crate::anim::compose(&pose.world[0], &pose.transform[2]);

    // The anchor: joint 13's previous world position and the init ground
    // height, read before the chain is rebuilt.
    let anchor_x = pose.world[13].t[0];
    let anchor_y = i32::from(entity.yawn_ground_y());
    let anchor_z = pose.world[13].t[2];

    // Joint 2's world translation moves into `<<9` space; the rest of the
    // chain accumulates there and converts back at the very end.
    for axis in 0..3 {
        pose.world[2].t[axis] = pose.world[2].t[axis].wrapping_shl(9);
    }

    // ---- joints 3..14 ----
    for joint in 3..YAWN_JOINTS {
        let rotation = keyframe.rotations.get(joint).copied().unwrap_or([0; 3]);
        let z_target = if joint == 3 {
            pose.rotation[0][2]
                .wrapping_add(pose.rotation[2][2])
                .wrapping_add(rotation[2])
        } else {
            pose.rotation[joint - 1][2].wrapping_add(rotation[2])
        };
        if blend == 0 {
            let prev = if frame_id == 0 {
                rotation[1]
            } else {
                pose.prev_yaw[joint]
            };
            pose.rotation[joint][1] =
                pose.rotation[joint][1].wrapping_add(rotation[1].wrapping_sub(prev));
            pose.rotation[joint][2] = z_target;
        } else {
            let prev = if frame_id == 0 {
                rotation[1]
            } else {
                pose.prev_yaw[joint]
            };
            let target = [
                0,
                pose.rotation[joint][1]
                    .wrapping_sub(prev)
                    .wrapping_add(rotation[1]),
                z_target,
            ];
            let w_current = i32::from(blend) * step;
            let w_target = (inverse - i32::from(blend)) * step;
            pose.rotation[joint] = fp_lerp(pose.rotation[joint], target, w_current, w_target);
        }
        pose.prev_yaw[joint] = rotation[1];
        if joint == 7 {
            pose.rotation[joint][2] = 0;
        }
        let source = [
            pose.transform[joint].t[0].wrapping_shl(9),
            pose.transform[joint].t[1].wrapping_shl(9),
            pose.transform[joint].t[2].wrapping_shl(9),
        ];
        let scratch = apply_matrix_lv(&pose.world[joint - 1], source);
        pose.world[joint].r = crate::anim::rotation_matrix(
            i32::from(pose.rotation[joint][0]),
            i32::from(pose.rotation[joint][1]),
            i32::from(pose.rotation[joint][2]),
        );
        for (axis, offset) in scratch.iter().enumerate() {
            pose.world[joint].t[axis] = pose.world[joint - 1].t[axis].wrapping_add(*offset);
        }
    }
    if blend != 0 {
        entity.blend_counter = entity.blend_counter.wrapping_sub(1);
    }

    // ---- snap the body onto its anchor and convert back out of `<<9` ----
    // The anchor is joint 13's world position from before the rebuild: the
    // chain is shifted so joint 13 lands back on it, and the leftover is what
    // moves the entity.
    let off_x = anchor_x
        .wrapping_mul(0x200)
        .wrapping_sub(pose.world[13].t[0]);
    let off_y = anchor_y
        .wrapping_mul(0x200)
        .wrapping_sub(pose.world[13].t[1]);
    let off_z = anchor_z
        .wrapping_mul(0x200)
        .wrapping_sub(pose.world[13].t[2]);
    for joint in (3..YAWN_JOINTS).rev() {
        for (axis, offset) in [off_x, off_y, off_z].iter().enumerate() {
            pose.world[joint].t[axis] = pose.world[joint].t[axis].wrapping_add(*offset) >> 9;
        }
    }
    entity.pos[0] = entity.pos[0].wrapping_add(off_x >> 9);
    entity.pos[2] = entity.pos[2].wrapping_add(off_z >> 9);
    pose.transform[0].t[1] = pose.transform[0].t[1].wrapping_add(off_y >> 9);

    entity.timing_control = data.timing as u8;
    entity.animation_frame_id = entity.animation_frame_id.wrapping_add(1);
    if usize::from(entity.animation_frame_id) > count - 1 {
        entity.animation_frame_id = 0;
        return true;
    }
    false
}

/// `yawn_chain_follow`: drag `seg` along behind `lead`, 34/35 of the lead's
/// local offset, re-aiming the segment's yaw at the lead when it is more than
/// 50 units from its rest point. The last write uses the full-precision
/// rotated offset (the 34/35 shrink is only for the distance test).
pub fn yawn_chain_follow(
    pose: &mut YawnPose,
    lead: usize,
    seg: usize,
    angle_step: i16,
    head_speed: u16,
    head_angle: i16,
) {
    let old_yaw = pose.rotation[seg][1];
    let source = [
        pose.transform[seg].t[0].wrapping_shl(12),
        pose.transform[seg].t[1].wrapping_shl(12),
        pose.transform[seg].t[2].wrapping_shl(12),
    ];
    let rotated = apply_matrix_lv(&pose.world[lead], source);
    let (rot_x, rot_y, rot_z) = (rotated[0], rotated[1], rotated[2]);

    let rest = |axis: usize| -> i32 {
        let shrunk = (rotated[axis] / 0x23).wrapping_mul(0x22);
        shrunk.wrapping_add(pose.world[lead].t[axis].wrapping_mul(0x1000))
    };
    let dx = rest(0).wrapping_sub(pose.world[seg].t[0].wrapping_mul(0x1000));
    let dz = rest(2).wrapping_sub(pose.world[seg].t[2].wrapping_mul(0x1000));
    let hx = dx >> 12;
    let hz = dz >> 12;
    let dist = sqrt0(hz.wrapping_mul(hz).wrapping_add(hx.wrapping_mul(hx))) as u16;

    if dist >= 0x32 {
        let dx = dx >> 4;
        let dz = dz >> 4;
        if dx == 0 {
            pose.rotation[seg][1] = (if dz > 0 { 0x800 } else { 0 }) + 0x400;
        } else {
            let q = angle_quadrant(dz.wrapping_shl(12) / dx);
            pose.rotation[seg][1] = ((2 - i32::from(dx < 0)) * 0x800 - q) as i16;
        }
        let prev_angle = old_yaw;
        let distance = (pose.rotation[seg][1]
            .wrapping_sub(old_yaw)
            .wrapping_add(angle_step))
            & 0xFFF;
        let step = angle_step as u16;
        if step.wrapping_mul(2) < distance as u16 {
            pose.rotation[seg][1] = prev_angle.wrapping_sub(angle_step);
            if step.wrapping_add(0x800) >= distance as u16 {
                pose.rotation[seg][1] = prev_angle.wrapping_add(angle_step);
            }
        } else if head_speed < 100 {
            // The head is nearly stopped: the segment snaps to the lead.
            pose.rotation[seg][1] = prev_angle;
        }
        if seg < 5 {
            let mut reference = pose.rotation[lead][1];
            if seg == 3 {
                reference = head_angle;
            }
            let rel = (pose.rotation[seg][1]
                .wrapping_sub(reference)
                .wrapping_add(0x400))
                & 0xFFF;
            if rel as u16 > 0x800 {
                pose.rotation[seg][1] = reference.wrapping_sub(0x400);
                if (rel as u16) < 0xC00 {
                    pose.rotation[seg][1] = reference.wrapping_add(0x400);
                }
            }
        }
    }

    pose.world[seg].r = crate::anim::rotation_matrix(
        i32::from(pose.rotation[seg][0]),
        i32::from(pose.rotation[seg][1]),
        i32::from(pose.rotation[seg][2]),
    );
    pose.world[seg].t[0] = (rot_x.wrapping_add(pose.world[lead].t[0].wrapping_mul(0x1000))) >> 12;
    pose.world[seg].t[1] = (rot_y.wrapping_add(pose.world[lead].t[1].wrapping_mul(0x1000))) >> 12;
    pose.world[seg].t[2] = (rot_z.wrapping_add(pose.world[lead].t[2].wrapping_mul(0x1000))) >> 12;
}

/// `yawn_chain_align`: turn the lead joint toward a pushed segment (up to
/// 0x30 per frame), used when a segment's own room collision moved it.
pub fn yawn_chain_align(pose: &mut YawnPose, lead: usize, seg: usize) {
    let source = [
        pose.transform[seg].t[0].wrapping_shl(12),
        pose.transform[seg].t[1].wrapping_shl(12),
        pose.transform[seg].t[2].wrapping_shl(12),
    ];
    let rotated = apply_matrix_lv(&pose.world[lead], source);
    let rest = |axis: usize| -> i32 {
        (rotated[axis] / 0x23)
            .wrapping_mul(0x22)
            .wrapping_add(pose.world[lead].t[axis].wrapping_mul(0x1000))
    };
    let dx = rest(0).wrapping_sub(pose.world[seg].t[0].wrapping_mul(0x1000)) >> 4;
    let dz = rest(2).wrapping_sub(pose.world[seg].t[2].wrapping_mul(0x1000)) >> 4;

    let mut target = if dx == 0 {
        (if dz > 0 { 0x800 } else { 0 }) + 0x400
    } else {
        let q = angle_quadrant(dz.wrapping_shl(12) / dx);
        (2 - i32::from(dx < 0)) * 0x800 - q
    };

    let delta = target.wrapping_sub(i32::from(pose.rotation[seg][1])) as u32;
    if ((delta.wrapping_add(0x800)) & 0xFFF) < 0x800 {
        target = i32::from(pose.rotation[seg][1]).wrapping_sub(target) & 0xFFF;
        if target > 0x30 {
            target = 0x30;
        }
        pose.rotation[lead][1] = pose.rotation[lead][1].wrapping_sub(target as i16);
    } else {
        target = (delta & 0xFFF) as i32;
        if target > 0x30 {
            target = 0x30;
        }
        pose.rotation[lead][1] = pose.rotation[lead][1].wrapping_add(target as i16);
    }

    pose.world[lead].r = crate::anim::rotation_matrix(
        i32::from(pose.rotation[lead][0]),
        i32::from(pose.rotation[lead][1]),
        i32::from(pose.rotation[lead][2]),
    );
}

/// `yawn_post_move`: the head's end-of-frame work - the player SCA pair
/// (skipped for the three scripted behaviours), the room collision with the
/// wall-stuck counter, and the follow-the-leader pass over joints 2..14. The
/// last four links only follow while the snake moves or shrinks, with half
/// the angular step.
pub fn yawn_post_move(
    game: &mut crate::game::GameState,
    room: &RoomState,
    slot: usize,
    angle_step: i16,
) {
    let behavior = game.entities[slot].action_behavior;
    if behavior != 5 && behavior != 6 && behavior != 7 {
        let player_pos = game.entities[0].pos;
        let player_angle = game.entities[0].angle;
        let player_status = game.entities[0].status_flags;
        let player_radius = crate::enemy::walk::player_radius(game.id.player_flag);
        let prev_pos = game.entities[slot]
            .saved_pos
            .unwrap_or(game.entities[slot].pos);
        crate::enemy::walk::separate_from_player(
            &mut game.entities[slot],
            prev_pos,
            player_pos,
            player_angle,
            player_radius,
            player_status,
        );
    }

    let result = crate::enemy::walk::check_room_collision(room, &mut game.entities[slot]);
    if game.entities[slot].behavior_flags & 1 == 0 {
        if result == 0 {
            if game.entities[slot].yawn_stuck_cool() != 0 {
                let value = game.entities[slot].yawn_stuck_cool() - 1;
                game.entities[slot].set_yawn_stuck_cool(value);
            }
        } else {
            let value = game.entities[slot].yawn_stuck().wrapping_add(1);
            game.entities[slot].set_yawn_stuck(value);
            game.entities[slot].set_yawn_stuck_cool(0x14);
        }
    }

    let entity = game.entities[slot];
    let head_speed = entity.move_speed_current;
    let head_angle = entity.angle as i16;
    let moving = entity.move_speed_current != 0 || entity.yawn_shrink() != 0;
    let Some(pose) = game.entity_anims[slot].yawn.as_deref_mut() else {
        return;
    };
    let matrix =
        crate::anim::entity_matrix_rotated(entity.pos, entity.pitch, entity.angle, entity.roll);
    pose.world[0] = crate::anim::compose(&matrix, &pose.transform[0]);
    pose.world[1] = crate::anim::compose(&pose.world[0], &pose.transform[1]);
    pose.world[2] = crate::anim::compose(&pose.world[0], &pose.transform[2]);

    for lead in 2..10 {
        yawn_chain_follow(pose, lead, lead + 1, angle_step, head_speed, head_angle);
    }
    if moving {
        yawn_chain_follow(pose, 10, 11, angle_step, head_speed, head_angle);
        yawn_chain_follow(pose, 11, 12, angle_step, head_speed, head_angle);
        yawn_chain_follow(pose, 12, 13, angle_step / 2, head_speed, head_angle);
        yawn_chain_follow(pose, 13, 14, angle_step / 2, head_speed, head_angle);
    }
}

/// `ApplyMatrixLV`: rotate a vector by the matrix's 4.12 rotation with the
/// game's Y-sign conjugation. The translation is not read.
fn apply_matrix_lv(matrix: &Mat4x3, v: [i32; 3]) -> [i32; 3] {
    let source = [v[0], v[1].wrapping_neg(), v[2]];
    let mut out = [0i32; 3];
    for (row, value) in matrix.r.iter().zip(out.iter_mut()) {
        let mut sum = 0i64;
        for (element, input) in row.iter().zip(&source) {
            sum += i64::from(*element) * i64::from(*input);
        }
        *value = ((sum + ((sum >> 63) & 0xFFF)) >> 12) as i32;
    }
    out[1] = out[1].wrapping_neg();
    out
}

/// `ApplyMatrixSV`: rotate an SVECTOR by a 4.12 matrix, truncating each
/// component to a signed short with the game's Y-sign negation. Unlike
/// [`apply_matrix_lv`] the products are full integer multiplies, not per-term
/// 4.12 truncations.
pub fn apply_matrix_sv(matrix: &Mat4x3, v: [i16; 3]) -> [i16; 3] {
    let vx = i32::from(v[0]);
    let vy = -i32::from(v[1]);
    let vz = i32::from(v[2]);
    let mut out = [0i16; 3];
    for (row, value) in matrix.r.iter().zip(out.iter_mut()) {
        let sum = row[0] * vx + row[1] * vy + row[2] * vz;
        *value = ((sum + ((sum >> 31) & 0xFFF)) >> 12) as i16;
    }
    out[1] = out[1].wrapping_neg();
    out
}

/// `SquareRoot0`: the game's integer square root; a non-positive input is
/// zero.
fn sqrt0(value: i32) -> i32 {
    if value <= 0 {
        return 0;
    }
    (f64::from(value)).sqrt() as i32
}

/// `GetAngleQuadrantValue`: a fixed-point slope to the game's 12-bit angle.
pub fn angle_quadrant(slope: i32) -> i32 {
    if slope == 0 {
        return 0;
    }
    ((f64::from(slope) / 4096.0).atan() * (2048.0 / std::f64::consts::PI)) as i32
}

/// `fp_lerp`: `out = (current * w_cur + target * w_tgt) >> 12`, each product
/// truncated toward zero first and the sum stored at 16-bit width.
fn fp_lerp(current: [i16; 3], target: [i16; 3], w_cur: i32, w_tgt: i32) -> [i16; 3] {
    let mut out = [0i16; 3];
    for axis in 0..3 {
        let t = i32::from(target[axis]).wrapping_mul(w_tgt);
        let c = i32::from(current[axis]).wrapping_mul(w_cur);
        out[axis] = (((t + ((t >> 31) & 0xFFF)) >> 12) + ((c + ((c >> 31) & 0xFFF)) >> 12)) as i16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClipFrame, Keyframe};

    fn clip(frames: &[(u16, u16)]) -> Clip {
        Clip {
            frames: frames
                .iter()
                .map(|&(keyframe, timing)| ClipFrame { keyframe, timing })
                .collect(),
        }
    }

    fn keyframes(rotations: &[i16]) -> Vec<Keyframe> {
        rotations
            .iter()
            .map(|&rotation| Keyframe {
                offset: [0, rotation, 0],
                rotations: vec![[rotation, 0, 0]],
            })
            .collect()
    }

    fn clock_with(keyframes: Vec<Keyframe>, _clips: Vec<Clip>) -> (EntityAnim, Entity) {
        let clock = EntityAnim {
            keyframes: Some(std::sync::Arc::new(keyframes)),
            ..EntityAnim::default()
        };
        let entity = Entity {
            animation_id: 0,
            animation_frame_id: 0,
            timing_control: 0,
            blend_counter: 0,
            ..Entity::default()
        };
        (clock, entity)
    }

    #[test]
    fn a_spent_hold_advances_the_frame_and_wraps() {
        let clips = vec![clip(&[(0, 2), (1, 1)])];
        let keyframes = keyframes(&[0, 100]);
        let (mut clock, mut entity) = clock_with(keyframes, clips.clone());
        // Entering with timing 0: the frame applies and the hold reloads.
        assert!(!plant42_advance(
            &mut clock,
            &mut entity,
            &clips,
            false,
            0x40
        ));
        assert_eq!(entity.animation_frame_id, 1);
        assert_eq!(entity.timing_control, 2);
        // One held tick decrements the hold.
        plant42_advance(&mut clock, &mut entity, &clips, false, 0x40);
        assert_eq!(entity.timing_control, 1);
        assert_eq!(entity.animation_frame_id, 1, "the frame is held");
        // The hold expired: frame 1 applies, reloads its own hold and wraps.
        assert!(plant42_advance(
            &mut clock,
            &mut entity,
            &clips,
            false,
            0x40
        ));
        assert_eq!(entity.animation_frame_id, 0);
        assert_eq!(entity.timing_control, 1);
    }

    #[test]
    fn the_blend_counter_is_consumed_every_tick() {
        let clips = vec![clip(&[(0, 5), (1, 5)])];
        let keyframes = keyframes(&[0, 100]);
        let (mut clock, mut entity) = clock_with(keyframes, clips.clone());
        entity.blend_counter = 3;
        plant42_advance(&mut clock, &mut entity, &clips, false, 0x40);
        assert_eq!(entity.blend_counter, 2, "consumed on the applying tick");
        plant42_advance(&mut clock, &mut entity, &clips, false, 0x40);
        assert_eq!(entity.blend_counter, 1, "consumed on a held tick too");
        plant42_advance(&mut clock, &mut entity, &clips, false, 0x40);
        assert_eq!(entity.blend_counter, 0);
    }

    #[test]
    fn the_sub_frame_blend_keeps_moving_the_pose_on_held_ticks() {
        let clips = vec![clip(&[(0, 4), (1, 4)])];
        let keyframes = keyframes(&[0, 0x400]);
        let (mut clock, mut entity) = clock_with(keyframes.clone(), clips.clone());
        // A fresh pose starts at the target: the first tick snaps.
        plant42_advance(&mut clock, &mut entity, &clips, false, 0x40);
        assert_eq!(entity.timing_control, 4);
        let first = clock.player.applied_pose().unwrap();
        assert_eq!(first.rotations[0][0], 0, "frame 0 is the source pose");

        // The frame advances: the blend counter is spent, so the next held
        // ticks synthesize `(0x1000/0x40) / (timing+1)`.
        entity.timing_control = 1;
        entity.blend_counter = 0;
        plant42_advance(&mut clock, &mut entity, &clips, false, 0x40);
        // Frame 1 applied at blend 0 (hold was 1 -> 0): the pose snaps to the
        // target.
        let pose = clock.player.applied_pose().unwrap();
        assert_eq!(pose.rotations[0][0], 0x400);
        assert_eq!(entity.animation_frame_id, 0);
        assert_eq!(entity.timing_control, 4);

        // Now hold: the synthesized weight lerps from the stored pose toward
        // the frame-0 target (rotation 0) each tick.
        plant42_advance(&mut clock, &mut entity, &clips, false, 0x40);
        let pose = clock.player.applied_pose().unwrap();
        // timing after decrement = 3; blend = 0x40 / 4 = 0x10; wCur =
        // 0x10 * 0x40 = 0x400; 0x400 * 0x400 >> 12 = 0x100.
        assert_eq!(pose.rotations[0][0], 0x100);
        plant42_advance(&mut clock, &mut entity, &clips, false, 0x40);
        // timing 2; blend = 0x40 / 3 = 0x15; wCur = 0x15 * 0x40 = 0x540;
        // 0x100 * 0x540 >> 12 = 0x54.
        let pose = clock.player.applied_pose().unwrap();
        assert_eq!(pose.rotations[0][0], 0x54);
    }

    #[test]
    fn reverse_reads_the_frame_table_backwards() {
        let clips = vec![clip(&[(0, 1), (1, 1)])];
        let keyframes = keyframes(&[0, 100]);
        let (mut clock, mut entity) = clock_with(keyframes, clips.clone());
        assert!(!plant42_advance(
            &mut clock,
            &mut entity,
            &clips,
            true,
            0x40
        ));
        // Frame 0 under reverse is the clip's last entry (keyframe 1).
        assert_eq!(clock.player.applied_pose().unwrap().rotations[0][0], 100);
        assert_eq!(entity.animation_frame_id, 1);
        assert!(plant42_advance(&mut clock, &mut entity, &clips, true, 0x40));
        assert_eq!(clock.player.applied_pose().unwrap().rotations[0][0], 0);
        assert_eq!(entity.animation_frame_id, 0, "wrapped");
    }

    #[test]
    fn the_last_blend_step_takes_the_short_way_round() {
        let clips = vec![clip(&[(0, 1), (1, 1)])];
        // Source 0x700, target 0x900: the direct delta is +0x200, so the
        // last step's fixup leaves the source alone.
        let keyframes = vec![
            Keyframe {
                offset: [0, 0, 0],
                rotations: vec![[0x700, 0, 0]],
            },
            Keyframe {
                offset: [0, 0, 0],
                rotations: vec![[0x900, 0, 0]],
            },
        ];
        let (mut clock, mut entity) = clock_with(keyframes.clone(), clips.clone());
        clock.player.set_applied_pose(Some(keyframes[0].clone()));
        entity.animation_frame_id = 1;
        entity.timing_control = 1;
        // Blend counter 0x3F with step 0x40: inverse = 0x40 - 0x3F = 1, the
        // last blend step.
        entity.blend_counter = 0x3F;
        plant42_advance(&mut clock, &mut entity, &clips, false, 0x40);
        let pose = clock.player.applied_pose().unwrap();
        // wTgt = 1 * 0x40 = 0x40; 0x900 * 0x40 >> 12 = 0x24; wCur = 0x3F *
        // 0x40 = 0xFC0; 0x700 * 0xFC0 >> 12 = 0x6E4; total 0x708.
        assert_eq!(pose.rotations[0][0], 0x708);
    }

    #[test]
    fn an_out_of_range_clip_resets_the_words() {
        let (mut clock, mut entity) = clock_with(keyframes(&[0]), Vec::new());
        entity.animation_id = 9;
        entity.animation_frame_id = 4;
        assert!(!plant42_advance(&mut clock, &mut entity, &[], false, 0x40));
        assert_eq!(entity.animation_frame_id, 0);
        assert_eq!(entity.timing_control, 0);
    }

    #[test]
    fn scale_columns_scales_each_column_independently() {
        let mut matrix = Mat4x3 {
            r: [[0x1000, 0, 0], [0, 0x1000, 0], [0, 0, 0x1000]],
            t: [7, 8, 9],
        };
        scale_columns_xyz(&mut matrix, [0x2000, 0x1000, 0x800]);
        assert_eq!(matrix.r[0][0], 0x2000);
        assert_eq!(matrix.r[1][1], 0x1000);
        assert_eq!(matrix.r[2][2], 0x800);
        assert_eq!(matrix.t, [7, 8, 9], "the translation is untouched");
    }

    #[test]
    fn orient_from_matrix_derives_a_rotation() {
        // The identity 3x3 (0x1000 on the diagonal) maps to the zero
        // rotation; the game's trig tables saturate at 0x3FFF, so the
        // diagonal reads back one unit low.
        let mut identity = [0i16; 9];
        identity[0] = 0x1000;
        identity[4] = 0x1000;
        identity[8] = 0x1000;
        let matrix = orient_from_matrix(identity);
        assert_eq!(matrix.r[0][0], 0x0FFF);
        assert_eq!(matrix.r[1][1], 0x0FFF);
        assert_eq!(matrix.r[2][2], 0x0FFF);
    }

    // -----------------------------------------------------------------
    // Yawn's fixed-point chain
    // -----------------------------------------------------------------

    fn yawn_skeleton() -> Skeleton {
        let mut relative = vec![[0i16; 3]; YAWN_JOINTS];
        relative[0] = [0, -1000, 0];
        for joint in relative.iter_mut().skip(1) {
            *joint = [0, -300, 0];
        }
        let mut children: Vec<Vec<u8>> = vec![Vec::new(); YAWN_JOINTS];
        children[0] = vec![1, 2];
        for (joint, child) in children
            .iter_mut()
            .enumerate()
            .take(YAWN_JOINTS - 1)
            .skip(2)
        {
            *child = vec![(joint + 1) as u8];
        }
        Skeleton { relative, children }
    }

    fn yawn_keyframe(yaw: i16, roll: i16) -> Keyframe {
        Keyframe {
            offset: [0, -1000, 0],
            rotations: vec![[0, yaw, roll]; YAWN_JOINTS],
        }
    }

    fn yawn_clip(frames: usize) -> Clip {
        Clip {
            frames: (0..frames)
                .map(|keyframe| crate::model::ClipFrame {
                    keyframe: keyframe as u16,
                    timing: 1,
                })
                .collect(),
        }
    }

    fn yawn_clock() -> EntityAnim {
        let keyframes: Vec<Keyframe> = (0..4)
            .map(|index| yawn_keyframe(0, (index as i16) * 0x40))
            .collect();
        EntityAnim {
            keyframes: Some(std::sync::Arc::new(keyframes)),
            skeleton: Some(std::sync::Arc::new(yawn_skeleton())),
            ..EntityAnim::default()
        }
    }

    #[test]
    fn yawn_pose_init_stacks_the_chain_from_the_root() {
        let skeleton = yawn_skeleton();
        let keyframe = yawn_keyframe(0x40, 0);
        let mut pose = YawnPose::default();
        let entity = Entity {
            angle: 0x200,
            ..Entity::default()
        };
        yawn_pose_init(&mut pose, &entity, &skeleton, &keyframe);

        // Joints 0-2 take the keyframe rotations; joint 3's yaw is the entity
        // angle plus the pose yaw and its roll stacks joints 0 and 2.
        assert_eq!(pose.rotation[0], [0, 0x40, 0]);
        assert_eq!(pose.rotation[3][1], (0x200u16 + 0x40) as i16);
        assert_eq!(pose.rotation[3][2], 0, "joints 0 and 2 carry no roll");
        // Joint 7 stays level.
        assert_eq!(pose.rotation[7][2], 0);
        // Each body joint adds the pose yaw to the previous joint's.
        assert_eq!(pose.rotation[8][1], pose.rotation[7][1].wrapping_add(0x40));
        // The root's transform Y is the keyframe offset.
        assert_eq!(pose.transform[0].t[1], -1000);
        // The chain hangs down the skeleton's relative offsets.
        assert!(pose.world[14].t[1] < pose.world[3].t[1]);
        assert_eq!(pose.prev_yaw[3], 0x40);
    }

    #[test]
    fn yawn_advance_wraps_and_publishes_the_frame() {
        let clips = vec![yawn_clip(3)];
        let mut clock = yawn_clock();
        let mut entity = Entity {
            animation_id: 0,
            animation_frame_id: 0,
            ..Entity::default()
        };
        let skeleton = yawn_skeleton();
        yawn_pose_init(
            clock.yawn.get_or_insert_with(Default::default),
            &entity,
            &skeleton,
            &yawn_keyframe(0, 0),
        );
        assert!(!yawn_advance(&mut clock, &mut entity, &clips, false, 0x200));
        assert_eq!(entity.animation_frame_id, 1);
        assert_eq!(entity.timing_control, 1);
        assert!(!yawn_advance(&mut clock, &mut entity, &clips, false, 0x200));
        assert!(yawn_advance(&mut clock, &mut entity, &clips, false, 0x200));
        assert_eq!(entity.animation_frame_id, 0, "wrapped");
    }

    #[test]
    fn yawn_advance_holds_the_frame_while_timing_runs() {
        let clips = vec![yawn_clip(2)];
        let mut clock = yawn_clock();
        let mut entity = Entity::default();
        let skeleton = yawn_skeleton();
        yawn_pose_init(
            clock.yawn.get_or_insert_with(Default::default),
            &entity,
            &skeleton,
            &yawn_keyframe(0, 0),
        );
        yawn_advance(&mut clock, &mut entity, &clips, false, 0x200);
        entity.timing_control = 3;
        let frame = entity.animation_frame_id;
        assert!(!yawn_advance(&mut clock, &mut entity, &clips, false, 0x200));
        assert_eq!(entity.timing_control, 2, "the hold ticks down");
        assert_eq!(entity.animation_frame_id, frame, "the frame is held");
    }

    #[test]
    fn yawn_advance_anchors_the_tail_on_the_previous_joint_thirteen() {
        let clips = vec![yawn_clip(2)];
        let mut clock = yawn_clock();
        let mut entity = Entity::default();
        entity.set_yawn_ground_y(-1000);
        let skeleton = yawn_skeleton();
        yawn_pose_init(
            clock.yawn.get_or_insert_with(Default::default),
            &entity,
            &skeleton,
            &yawn_keyframe(0, 0),
        );
        let anchor = clock.yawn.as_ref().unwrap().world[13].t;
        // Advance to a frame whose pose rolls the chain: the rebuilt chain
        // lands joint 13 back on the anchor and the entity absorbs the
        // leftover.
        let before = entity.pos;
        entity.animation_frame_id = 1;
        yawn_advance(&mut clock, &mut entity, &clips, false, 0x200);
        let pose = clock.yawn.as_deref().unwrap();
        assert_eq!(pose.world[13].t[0], anchor[0], "joint 13's anchor X");
        assert_eq!(pose.world[13].t[2], anchor[2], "joint 13's anchor Z");
        assert_eq!(
            pose.world[13].t[1], -1000,
            "and its Y snaps to the init ground height"
        );
        assert_ne!(entity.pos, before, "the leftover delta moved the entity");
    }

    #[test]
    fn yawn_chain_follow_drags_the_segment_onto_the_rest_point() {
        let mut pose = YawnPose::default();
        for joint in 0..YAWN_JOINTS {
            pose.rotation[joint] = [0, 0, 0];
            pose.transform[joint] = Mat4x3 {
                r: crate::anim::rotation_matrix(0, 0, 0),
                t: [0, -300, 0],
            };
            pose.world[joint] = Mat4x3 {
                r: crate::anim::rotation_matrix(0, 0, 0),
                t: [100 * joint as i32, 0, 0],
            };
        }
        // The segment sits far from the rest point; the follow re-aims it and
        // lands its world translation on the lead-relative rest point.
        yawn_chain_follow(&mut pose, 3, 4, 0x30, 0, 0);
        assert_eq!(
            pose.world[4].t,
            [300, -300, 0],
            "the rest point plus the lead translation"
        );
    }

    #[test]
    fn yawn_chain_align_turns_the_lead_toward_the_segment() {
        let mut pose = YawnPose::default();
        for joint in 0..YAWN_JOINTS {
            pose.rotation[joint] = [0, 0, 0];
            pose.transform[joint] = Mat4x3 {
                r: crate::anim::rotation_matrix(0, 0, 0),
                t: [0, -300, 0],
            };
            pose.world[joint] = Mat4x3 {
                r: crate::anim::rotation_matrix(0, 0, 0),
                t: [0, 0, 0],
            };
        }
        // A segment pushed to the side drags the lead's yaw at most 0x30.
        pose.world[5].t = [500, 0, 0];
        let before = pose.rotation[4][1];
        yawn_chain_align(&mut pose, 4, 5);
        let turn = pose.rotation[4][1].wrapping_sub(before).unsigned_abs();
        assert!(turn <= 0x30, "clamped to the align step");
    }

    #[test]
    fn apply_matrix_sv_rotates_with_the_y_negation() {
        let matrix = Mat4x3 {
            r: crate::anim::rotation_matrix(0, 0x400, 0),
            t: [0; 3],
        };
        let out = apply_matrix_sv(&matrix, [1000, 0, 0]);
        assert_eq!(out[0], 0);
        assert_eq!(out[2], -999, "yaw 0x400 maps +X to -Z (saturated trig)");
    }

    #[test]
    fn fp_lerp_weights_the_pair_with_truncating_twelfths() {
        let out = fp_lerp([100, -100, 0], [300, 300, 0], 0x200, 0xE00);
        // 300 * 0xE00 >> 12 = 262; 100 * 0x200 >> 12 = 12.
        assert_eq!(out[0], 274);
        // -100 * 0x200 >> 12 = -12; 300 * 0xE00 >> 12 = 262.
        assert_eq!(out[1], 250);
        assert_eq!(out[2], 0);
    }

    #[test]
    fn sqrt_and_quadrant_helpers_match_the_game() {
        assert_eq!(sqrt0(0), 0);
        assert_eq!(sqrt0(-5), 0);
        assert_eq!(sqrt0(16), 4);
        assert_eq!(sqrt0(15), 3);
        assert_eq!(angle_quadrant(0), 0);
        assert_eq!(angle_quadrant(4096), 0x200, "45 degrees in 12-bit units");
        assert_eq!(angle_quadrant(-4096), -0x200);
    }

    // -----------------------------------------------------------------
    // The Tyrant's root-motion extractor
    // -----------------------------------------------------------------

    fn tyrant_model() -> (Skeleton, Vec<Keyframe>, Vec<Clip>) {
        let relative = vec![[0i16; 3]; 15];
        let mut children: Vec<Vec<u8>> = vec![Vec::new(); 15];
        for (joint, child) in children.iter_mut().enumerate().take(14) {
            *child = vec![(joint + 1) as u8];
        }
        let skeleton = Skeleton { relative, children };
        let keyframe = Keyframe {
            offset: [0, 0, 0],
            rotations: vec![[0, 0, 0]; 15],
        };
        let clip = Clip {
            frames: vec![crate::model::ClipFrame {
                keyframe: 0,
                timing: 1,
            }],
        };
        (skeleton, vec![keyframe], vec![clip])
    }

    fn tyrant_game() -> (crate::game::GameState, Vec<Clip>) {
        let mut game = crate::game::GameState::default();
        game.entities[1].id = 0x0C;
        game.entities[1].set_active(true);
        game.entities[1].pos = [1000, 0, 2000];
        game.entities[1].animation_id = 0;
        let (skeleton, keyframes, clips) = tyrant_model();
        game.entity_anims[1].skeleton = Some(std::sync::Arc::new(skeleton));
        game.entity_anims[1].keyframes = Some(std::sync::Arc::new(keyframes));
        (game, clips)
    }

    /// The chain end the extractor reads for `set`, computed with the same
    /// composition primitives so the test pins the base selection rather than
    /// re-deriving the arithmetic.
    fn chain_end(game: &crate::game::GameState, clips: &[Clip], set: u8) -> Mat4x3 {
        let entity = game.entities[1];
        let transforms = game.entity_anims[1].local_transforms(&entity, clips);
        let mut scratch = crate::anim::compose(
            &crate::anim::entity_matrix_rotated(
                entity.pos,
                entity.pitch,
                entity.angle,
                entity.roll,
            ),
            &transforms[0],
        );
        let base = 11 + usize::from(set) * 3;
        for index in [base - 2, base - 1, base] {
            scratch = crate::anim::compose(&scratch, &transforms[index]);
        }
        scratch
    }

    #[test]
    fn root_motion_derives_the_speed_from_the_chain_delta() {
        let (mut game, clips) = tyrant_game();
        let scratch = chain_end(&game, &clips, 0);
        // The previous world end sat 300 behind on X and 400 ahead on Z: the
        // delta is (300, 0, -400), speed 500, and apply shifts the entity.
        let world = Mat4x3 {
            r: scratch.r,
            t: [
                scratch.t[0] - 300,
                scratch.t[1] + 0x1000,
                scratch.t[2] + 400,
            ],
        };
        game.joint_worlds[1] = vec![Mat4x3::default(); 15];
        game.joint_worlds[1][11] = world;
        let before = game.entities[1].pos;
        let speed = tyrant_root_motion(&mut game, 1, &clips, 0, true);
        assert!(
            (i32::from(speed) - 500).abs() <= 2,
            "the planar delta rounds to 500, got {speed}"
        );
        assert_eq!(game.entities[1].move_speed_current, speed);
        let after = game.entities[1].pos;
        assert!(
            (after[0] - before[0] + 300).abs() <= 2,
            "apply subtracts the positive X delta: {before:?} -> {after:?}"
        );
        assert_eq!(after[1], before[1], "Y is never applied");
        assert!(
            (after[2] - before[2] - 400).abs() <= 2,
            "apply subtracts the negative Z delta"
        );
    }

    #[test]
    fn root_motion_set_one_reads_the_run_chain_end() {
        let (mut game, clips) = tyrant_game();
        let world = chain_end(&game, &clips, 1);
        game.joint_worlds[1] = vec![Mat4x3::default(); 15];
        // Only joint 14 carries a delta: set 1 must find it, set 0 must not.
        game.joint_worlds[1][14] = Mat4x3 {
            r: world.r,
            t: [world.t[0] - 700, world.t[1], world.t[2]],
        };
        let speed = tyrant_root_motion(&mut game, 1, &clips, 1, false);
        assert!((i32::from(speed) - 700).abs() <= 2, "got {speed}");
        let before = game.entities[1].pos;
        tyrant_root_motion(&mut game, 1, &clips, 0, false);
        assert_eq!(game.entities[1].pos, before, "apply is off");
    }

    #[test]
    fn root_motion_without_a_pose_keeps_the_speed() {
        let (mut game, clips) = tyrant_game();
        game.entity_anims[1].keyframes = None;
        game.entities[1].move_speed_current = 1234;
        let speed = tyrant_root_motion(&mut game, 1, &clips, 0, false);
        assert_eq!(speed, 1234);
        assert_eq!(game.entities[1].move_speed_current, 1234);
    }
}
