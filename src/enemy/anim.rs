//! The per-entity animation clock.
//!
//! [`EntityAnim`] wraps [`crate::anim::AnimPlayer`] the way the original's
//! `Joint_move` wraps its frame table, but the entity's visible words stay
//! authoritative: the clock syncs `animation_id`/`animation_frame_id`/
//! `timing_control` from the entity before every advance, and publishes the
//! pending frame and remaining hold back. Behaviours and scripts therefore
//! compare exactly the words the original's handlers compare; the clock only
//! remembers the frame applied last so the renderer can pose it.
//!
//! Reverse playback comes from `scd_entity_flags & 1`: the logical frame
//! counter still walks forward and wraps at the same index, while the keyframe
//! and timing are read from the end of the clip.
//!
//! `blend_counter` is the blend step counter the walk layer scales its
//! interpolation by ([`blend_step`]); it decrements once per frame-consuming
//! tick, exactly like the original clock's blending branch.

use std::sync::Arc;

use crate::anim::AnimPlayer;
use crate::game::Entity;
use crate::model::{Clip, Keyframe};

/// The animation clock state of one entity slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityAnim {
    /// The wrapped clip clock. `display_frame` is the frame applied last;
    /// `frame` is the frame the next consuming tick applies.
    pub player: AnimPlayer,
    /// Tracking joint yaw (`rotDeltaX`), slewed toward the look-at target.
    pub look_at_yaw: i16,
    /// Tracking joint pitch (`rotDeltaY`), slewed toward the look-at target.
    pub look_at_pitch: i16,
    /// The entity model's keyframe table, shared with the model cache. The
    /// clock needs it to materialize the blended pose on every consuming tick
    /// (the original writes the interpolated rotations straight into the
    /// joints), so a clip switch eases from the pose on screen.
    pub keyframes: Option<Arc<Vec<Keyframe>>>,
    /// The entity model's joint hierarchy, shared with the model cache. The
    /// monster drivers need it to compose the per-frame joint world matrices
    /// (the original's `EntityComputeJointWorldMatrices`).
    pub skeleton: Option<Arc<crate::model::Skeleton>>,
    /// The Yawn custom-skeleton pose: the fixed-point chain the head's own
    /// animator maintains. When present it replaces the shared clock's joint
    /// worlds entirely (the original's Yawn joints are marked so the render
    /// pass never recomposes them); `reset_joints` clears it with the rest of
    /// the clock.
    pub yawn: Option<Box<crate::enemy::custom_anim::YawnPose>>,
}

impl Default for EntityAnim {
    fn default() -> Self {
        Self {
            player: AnimPlayer::new(0),
            look_at_yaw: 0,
            look_at_pitch: 0,
            keyframes: None,
            skeleton: None,
            yawn: None,
        }
    }
}

impl EntityAnim {
    /// The frame the entity's pose currently shows (the frame applied last).
    pub fn display_frame(&self) -> usize {
        self.player.display_frame
    }

    /// The frame the next consuming tick applies.
    pub fn pending_frame(&self) -> usize {
        self.player.frame
    }

    /// Copy the entity's visible words into the clock.
    pub fn sync(&mut self, entity: &Entity) {
        self.player.clip = usize::from(entity.animation_id);
        self.player.frame = usize::from(entity.animation_frame_id);
        self.player.timing = u16::from(entity.timing_control);
    }

    /// Advance the clock one tick and publish the new words, exactly like the
    /// original `Joint_move`.
    ///
    /// If `timing_control > 1` the frame is held: only the hold counter is
    /// published back. Otherwise the current frame's data is applied (the
    /// displayed frame), its timing is reloaded, the pending frame advances,
    /// and passing the last frame wraps and reports completion. `reverse`
    /// reads the frame data backwards; `blend_step` is the caller's step and
    /// scales the pose interpolation [`EntityAnim::pose_keyframe`] applies.
    /// The entity's `blend_counter` is the blend state: a consuming tick
    /// applies it and decrements, a held tick leaves it alone. Returns whether
    /// the clip just completed.
    pub fn advance(
        &mut self,
        entity: &mut Entity,
        clips: &[Clip],
        reverse: bool,
        blend_step: u16,
    ) -> bool {
        self.sync(entity);
        self.player.reverse = reverse;
        self.player.blend_step = blend_step;
        self.player.blend_counter = u16::from(entity.blend_counter);
        let completed = self.player.update(clips);
        entity.animation_frame_id = self.player.frame as u8;
        entity.timing_control = self.player.timing.min(u16::from(u8::MAX)) as u8;
        entity.blend_counter = self.player.blend_counter.min(u16::from(u8::MAX)) as u8;
        // Materialize the pose this tick blended toward, exactly like the
        // original's joint write-back: the next tick (and a clip switch later)
        // eases from the pose currently on screen.
        if let Some(keyframes) = self.keyframes.as_deref() {
            let _ = self.player.pose_keyframe(clips, keyframes);
        }
        completed
    }

    /// The keyframe to pose `entity`'s mesh at.
    ///
    /// The clock's applied frame wins once it has advanced this clip; before
    /// that (a freshly spawned entity or a script that just changed the
    /// animation) the entity's own frame word is used, so a frame rendered
    /// before the first driver tick still shows the spawned clip.
    pub fn keyframe_index(&self, entity: &Entity, clips: &[Clip]) -> usize {
        let clip_index = usize::from(entity.animation_id);
        let Some(clip) = clips.get(clip_index) else {
            return 0;
        };
        if clip.frames.is_empty() {
            return 0;
        }
        let display = if self.player.clip == clip_index {
            self.player.display_frame
        } else {
            usize::from(entity.animation_frame_id)
        }
        .min(clip.frames.len() - 1);
        let index = if self.player.reverse {
            clip.frames.len().saturating_sub(1 + display)
        } else {
            display
        };
        clip.frames
            .get(index)
            .map_or(0, |frame| usize::from(frame.keyframe))
    }

    /// The pose to display for the entity's current frame.
    ///
    /// The clock materializes its blended pose on every consuming tick (see
    /// [`EntityAnim::advance`]), so this returns the pose on screen. A script
    /// that changed the entity's animation without advancing the clock keeps
    /// its own frame until the next tick; a clock without a stored keyframe
    /// table folds its pending steps against the table passed here.
    pub fn pose_keyframe(
        &self,
        entity: &Entity,
        clips: &[Clip],
        keyframes: &[Keyframe],
    ) -> Option<Keyframe> {
        if self.player.clip != usize::from(entity.animation_id) {
            return keyframes.get(self.keyframe_index(entity, clips)).cloned();
        }
        self.player.pose_keyframe(clips, keyframes)
    }

    /// The per-frame joint world matrices of the entity's posed skeleton, the
    /// computation the original runs in its render pass
    /// (`EntityComputeJointWorldMatrices`): rebuild the entity local matrix
    /// from the `pitch`/`angle`/`roll` rotation triple, apply the
    /// `joint_scale` column scale, then compose the skeleton hierarchy from
    /// the pose on screen. Returns an empty list when the model or keyframes
    /// are not available.
    ///
    /// The driver calls this at the end of every update, so the next tick's
    /// script - and the same tick's effect pass, which runs after the entity
    /// updates - reads the matrices of the pose left by the previous frame,
    /// exactly the original's order.
    pub fn joint_worlds(&self, entity: &Entity, clips: &[Clip]) -> Vec<crate::anim::Mat4x3> {
        // A Yawn's worlds are the chain its own animator maintains; the
        // shared clock never poses it.
        if let Some(pose) = self.yawn.as_deref() {
            return pose.world.to_vec();
        }
        let Some(skeleton) = self.skeleton.as_deref() else {
            return Vec::new();
        };
        let Some(keyframes) = self.keyframes.as_deref() else {
            return Vec::new();
        };
        let Some(pose) = self.pose_keyframe(entity, clips, keyframes) else {
            return Vec::new();
        };
        let matrix = crate::anim::entity_matrix_rotated_scaled(
            entity.pos,
            entity.pitch,
            entity.angle,
            entity.roll,
            entity.joint_scale,
        );
        crate::anim::joint_matrices(skeleton, &pose, &matrix)
    }

    /// The render-time joint worlds of a Yawn: the chain worlds the animator
    /// maintains, with joints 0-2 recomposed from their transforms the way the
    /// original's render pass does (the chain joints 3-14 are marked so the
    /// pass leaves them alone). `None` for every entity without a Yawn pose.
    pub fn yawn_render_worlds(&self, entity: &Entity) -> Option<Vec<crate::anim::Mat4x3>> {
        let pose = self.yawn.as_deref()?;
        let skeleton = self.skeleton.as_deref()?;
        Some(pose.render_worlds(skeleton, entity))
    }

    /// The displayed pose's per-joint local transforms (the original's
    /// `JointStruct.transform`), the chain the zombie's body-part physics
    /// multiplies. Empty when the model or keyframes are unavailable.
    pub fn local_transforms(&self, entity: &Entity, clips: &[Clip]) -> Vec<crate::anim::Mat4x3> {
        let Some(skeleton) = self.skeleton.as_deref() else {
            return Vec::new();
        };
        let Some(keyframes) = self.keyframes.as_deref() else {
            return Vec::new();
        };
        let Some(pose) = self.pose_keyframe(entity, clips, keyframes) else {
            return Vec::new();
        };
        crate::anim::joint_local_transforms(skeleton, &pose)
    }

    /// `entity_apply_anim_vertex`: set the entity's X/Z translation from the
    /// current animation frame's root vertex plus the `unk_c6`/`unk_c8`
    /// offsets the grab pose latched.
    ///
    /// The original extracts the vertex of the frame the clip is showing:
    /// while a hold is running (`timing_control > 1`) that is the frame just
    /// applied, one behind the entity's pending word, wrapping to the clip's
    /// last frame at zero; otherwise it is the pending frame itself, because
    /// the same tick's `Joint_move` is about to apply it. The vertex is
    /// rotated by the entity's yaw through the shared 4.12 pipeline and only
    /// X/Z are written; Y stays put. The base offsets zero-extend through
    /// their 16-bit fields, exactly like the original's `(unsigned int)` cast.
    pub fn apply_anim_vertex(&self, entity: &mut Entity, clips: &[Clip]) {
        let Some((x, z)) = self.root_vertex(entity, clips) else {
            return;
        };
        entity.pos[0] = i32::from(entity.unk_c6).wrapping_add(x);
        entity.pos[2] = i32::from(entity.unk_c8).wrapping_add(z);
    }

    /// The root vertex of the clip frame the entity is showing, rotated by the
    /// entity's yaw (`entity_extract_anim_vertex` plus the `ApplyMatrixSV`
    /// yaw rotation). `None` when the model carries no frame table.
    pub fn root_vertex(&self, entity: &Entity, clips: &[Clip]) -> Option<(i32, i32)> {
        let keyframes = self.keyframes.as_deref()?;
        let clip = clips.get(usize::from(entity.animation_id))?;
        if clip.frames.is_empty() {
            return None;
        }
        let pending = usize::from(entity.animation_frame_id);
        let index = if entity.timing_control > 1 {
            if pending == 0 {
                clip.frames.len() - 1
            } else {
                pending - 1
            }
        } else {
            pending
        };
        let frame = clip.frames.get(index)?;
        let keyframe = keyframes.get(usize::from(frame.keyframe))?;
        Some(crate::player::rotate_xz(
            entity.angle,
            i32::from(keyframe.offset[0]),
            i32::from(keyframe.offset[2]),
        ))
    }

    /// Slew the tracking joint's yaw and pitch toward the entity's stored
    /// look-at target.
    ///
    /// A `0x10` look-at flag enables the slew; bit 0 enables the yaw and bit 1
    /// the pitch. The yaw target is the angle to the target minus the entity's
    /// own facing; the pitch is the target's elevation over the horizontal
    /// distance. The yaw slew is the original's exact routine: it snaps only
    /// when the remaining turn is under one step, keeps the previous angle
    /// when the snap would land outside the +/-0x2C8 cone, decays a zero
    /// target toward zero, and clamps the period branches to the cone.
    ///
    /// The pitch is a bounded approximation: the port derives its target from
    /// the entity position rather than the tracking joint's world translation
    /// and steps it with the same under-two-steps snap rule.
    pub fn slew_look_at(&mut self, entity: &Entity) {
        if entity.look_at_flags & 0x10 == 0 {
            return;
        }
        let yaw_target = if entity.look_at_flags & 0x01 != 0 {
            crate::sfx::angle_between_xz(
                entity.pos[0],
                entity.pos[2],
                entity.target[0],
                entity.target[2],
            )
            .wrapping_sub(entity.angle)
                & 0x0FFF
        } else {
            0
        };
        let dx = i64::from(entity.target[0]) - i64::from(entity.pos[0]);
        let dz = i64::from(entity.target[2]) - i64::from(entity.pos[2]);
        let horizontal =
            crate::sfx::integer_sqrt((dx * dx + dz * dz).min(i64::from(i32::MAX)) as i32);
        let pitch_target = if entity.look_at_flags & 0x02 != 0 && horizontal > 0 {
            let slope = (i32::from(
                (entity.target[1] - entity.pos[1]).clamp(i32::from(i16::MIN), i32::from(i16::MAX))
                    as i16,
            ) << 12)
                / horizontal;
            let radians = (f64::from(slope) / 4096.0).atan();
            (radians * (2048.0 / std::f64::consts::PI)) as i32
        } else {
            0
        };
        self.look_at_yaw = slew_yaw(self.look_at_yaw, yaw_target, entity.look_at_yaw_step);
        self.look_at_pitch = step_angle(
            self.look_at_pitch,
            pitch_target as u16 & 0x0FFF,
            u16::from(entity.look_at_pitch_step),
            PITCH_CONE,
        );
    }
}

/// The tracking joint's yaw cone half-width, `+/-0x2C8`.
const YAW_CONE: u16 = 0x2C8;
/// The tracking joint's pitch cone half-width, `+/-0x138`.
const PITCH_CONE: u16 = 0x138;

/// One yaw step of the original's look-at slew.
///
/// `yaw_diff` folds the step into the turn, so the routine snaps under one
/// step; if the snap lands outside the `+/-0x2C8` cone the previous angle is
/// kept rather than clamped. A zero target decays toward zero from whichever
/// side it is on. The two period branches step toward the target and clamp to
/// the cone, exactly the original's comparisons.
fn slew_yaw(current: i16, target: u16, step: u8) -> i16 {
    let step = i32::from(step);
    let target = i32::from(target) & 0x0FFF;
    let current = i32::from(current) & 0x0FFF;
    let yaw_diff = (step - current + target) & 0x0FFF;
    if yaw_diff < step * 2 {
        // Snap, but keep the previous angle when the target is out of cone.
        if ((target + i32::from(YAW_CONE)) & 0x0FFF) > 0x590 {
            signed12(current)
        } else {
            signed12(target)
        }
    } else if target == 0 {
        let next = if current < 0x801 {
            (current - step) & 0x0FFF
        } else {
            (current + step) & 0x0FFF
        };
        signed12(next)
    } else if ((target - current) & 0x0FFF) < 0x800 {
        let mut next = (current + step) & 0x0FFF;
        if ((next as i16 - 0x2C8) & 0x0FFF) < 0xA70 {
            next = 0x2C8;
        }
        signed12(next)
    } else {
        let mut next = (current - step) & 0x0FFF;
        if ((next as i16 + 0x2C8) & 0x0FFF) > 0x590 {
            next = 0xD38;
        }
        signed12(next)
    }
}

/// Reinterpret a 12-bit angle as a signed value, the original's `(short)`
/// store of the wrapped rotation component.
fn signed12(value: i32) -> i16 {
    let wrapped = value & 0x0FFF;
    if wrapped > 0x800 {
        (wrapped - 0x1000) as i16
    } else {
        wrapped as i16
    }
}

/// Step one 12-bit angle toward `target` by at most `step`, snapping when the
/// remaining turn is under two steps, then clamp into `+/-cone`.
fn step_angle(current: i16, target: u16, step: u16, cone: u16) -> i16 {
    let step = i32::from(step.min(0x0FFF));
    let current_u = current as u16 & 0x0FFF;
    let difference = (i32::from(target) - i32::from(current_u)).rem_euclid(0x1000);
    let next = if difference < step * 2 || difference > 0x1000 - step * 2 {
        i32::from(target)
    } else if difference < 0x800 {
        i32::from(current_u) + step
    } else {
        i32::from(current_u) - step
    };
    let wrapped = next.rem_euclid(0x1000);
    let signed = if wrapped > 0x800 {
        wrapped - 0x1000
    } else {
        wrapped
    };
    signed.clamp(-i32::from(cone), i32::from(cone)) as i16
}

/// The blend step the walk layer derives from the entity's `blend_counter`:
/// `0x1000 / (blend_counter + 1)`, so the usual counter of 7 yields 0x200.
pub fn blend_step(entity: &Entity) -> u16 {
    0x1000 / (u16::from(entity.blend_counter) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Clip, ClipFrame};

    fn clip(timings: &[(u16, u16)]) -> Clip {
        Clip {
            frames: timings
                .iter()
                .map(|&(keyframe, timing)| ClipFrame { keyframe, timing })
                .collect(),
        }
    }

    fn entity(animation_id: u8, frame: u8, timing: u8) -> Entity {
        Entity {
            animation_id,
            animation_frame_id: frame,
            timing_control: timing,
            ..Entity::default()
        }
    }

    #[test]
    fn advance_publishes_the_pending_frame_and_reloads_the_hold() {
        let clips = vec![clip(&[(7, 3), (9, 1)])];
        let mut clock = EntityAnim::default();
        let mut entity = entity(0, 0, 0);

        assert!(!clock.advance(&mut entity, &clips, false, 0x400));
        // Frame 0 (keyframe 7) is applied and held for three ticks; the word
        // now names the pending frame 1.
        assert_eq!(clock.display_frame(), 0);
        assert_eq!(entity.animation_frame_id, 1);
        assert_eq!(entity.timing_control, 3);
        assert_eq!(clock.keyframe_index(&entity, &clips), 7);

        assert!(!clock.advance(&mut entity, &clips, false, 0x400));
        assert_eq!(clock.display_frame(), 0);
        assert_eq!(entity.animation_frame_id, 1);
        assert_eq!(entity.timing_control, 2);

        assert!(!clock.advance(&mut entity, &clips, false, 0x400));
        assert_eq!((clock.display_frame(), entity.timing_control), (0, 1));

        // The hold is spent: frame 1 (keyframe 9) applies, the clock wraps to
        // frame 0 and reports completion.
        assert!(clock.advance(&mut entity, &clips, false, 0x400));
        assert_eq!(clock.display_frame(), 1);
        assert_eq!(entity.animation_frame_id, 0);
        assert_eq!(entity.timing_control, 1);
        assert_eq!(clock.keyframe_index(&entity, &clips), 9);
    }

    #[test]
    fn advance_reads_a_scripted_seek_from_the_entity_words() {
        let clips = vec![clip(&[(7, 1), (9, 1), (11, 1), (13, 1)])];
        let mut clock = EntityAnim::default();
        let mut entity = entity(0, 0, 0);
        clock.advance(&mut entity, &clips, false, 0x400);
        clock.advance(&mut entity, &clips, false, 0x400);
        assert_eq!(clock.display_frame(), 1);

        // A script rewinds the frame and zeroes the hold; the next advance
        // applies the scripted frame instead of the clock's pending one.
        entity.animation_frame_id = 3;
        entity.timing_control = 0;
        assert!(clock.advance(&mut entity, &clips, false, 0x400));
        assert_eq!(clock.display_frame(), 3);
        assert_eq!(clock.keyframe_index(&entity, &clips), 13);
        assert_eq!(entity.animation_frame_id, 0);
    }

    #[test]
    fn reverse_reads_the_clip_backwards() {
        let clips = vec![clip(&[(0, 1), (1, 2), (2, 3)])];
        let mut clock = EntityAnim::default();
        let mut entity = entity(0, 0, 0);

        assert!(!clock.advance(&mut entity, &clips, true, 0x400));
        assert_eq!(clock.display_frame(), 0);
        assert_eq!(entity.timing_control, 3);
        assert_eq!(clock.keyframe_index(&entity, &clips), 2);

        // Two held ticks, then the reversed frame 1 (keyframe 1) applies.
        clock.advance(&mut entity, &clips, true, 0x400);
        clock.advance(&mut entity, &clips, true, 0x400);
        assert!(!clock.advance(&mut entity, &clips, true, 0x400));
        assert_eq!(clock.display_frame(), 1);
        assert_eq!(entity.timing_control, 2);
        assert_eq!(clock.keyframe_index(&entity, &clips), 1);

        // The counter still wraps forward and reports completion.
        clock.advance(&mut entity, &clips, true, 0x400);
        assert!(clock.advance(&mut entity, &clips, true, 0x400));
        assert_eq!(clock.display_frame(), 2);
        assert_eq!(clock.keyframe_index(&entity, &clips), 0);
    }

    #[test]
    fn out_of_range_clip_resets_the_words() {
        let clips = vec![clip(&[(0, 1)])];
        let mut clock = EntityAnim::default();
        let mut entity = entity(9, 4, 0);

        assert!(!clock.advance(&mut entity, &clips, false, 0x400));
        assert_eq!(entity.animation_frame_id, 0);
        assert_eq!(entity.timing_control, 0);
        assert_eq!(clock.display_frame(), 0);

        let mut clock = EntityAnim::default();
        clock.advance(&mut entity, &[], false, 0x400);
        assert_eq!(entity.animation_frame_id, 0);
    }

    #[test]
    fn blend_counter_decrements_on_consuming_ticks_only() {
        let clips = vec![clip(&[(0, 3), (1, 1)])];
        let mut clock = EntityAnim::default();
        let mut entity = entity(0, 0, 0);
        entity.blend_counter = 7;

        clock.advance(&mut entity, &clips, false, 0x400);
        assert_eq!(entity.blend_counter, 6);
        clock.advance(&mut entity, &clips, false, 0x400);
        assert_eq!(entity.blend_counter, 6, "a held tick does not blend");
        clock.advance(&mut entity, &clips, false, 0x400);
        assert_eq!(entity.blend_counter, 6);
        clock.advance(&mut entity, &clips, false, 0x400);
        assert_eq!(entity.blend_counter, 5);

        // The step derives from the counter.
        entity.blend_counter = 7;
        assert_eq!(blend_step(&entity), 0x200);
        entity.blend_counter = 3;
        assert_eq!(blend_step(&entity), 0x400);
        entity.blend_counter = 0;
        assert_eq!(blend_step(&entity), 0x1000);
    }

    #[test]
    fn keyframe_index_falls_back_before_the_first_advance() {
        let clips = vec![clip(&[(5, 1), (6, 1)]), clip(&[(9, 1), (10, 1)])];
        let clock = EntityAnim::default();
        let first = entity(1, 1, 0);
        assert_eq!(clock.keyframe_index(&first, &clips), 10);
        let wrapped = entity(1, 9, 0);
        assert_eq!(
            clock.keyframe_index(&wrapped, &clips),
            10,
            "clamped to the end"
        );
        let absent = entity(7, 0, 0);
        assert_eq!(clock.keyframe_index(&absent, &clips), 0);
    }

    #[test]
    fn joint_worlds_compose_the_entity_matrix_and_the_pose() {
        use crate::model::{Keyframe, Skeleton};
        use std::sync::Arc;

        let clips = vec![clip(&[(0, 1)])];
        let keyframes = vec![Keyframe {
            offset: [10, 0, 0],
            rotations: vec![[0, 0, 0], [0, 0, 0]],
        }];
        let skeleton = Skeleton {
            relative: vec![[0, 0, 0], [20, 0, 0]],
            children: vec![vec![1], vec![]],
        };
        let mut clock = EntityAnim {
            keyframes: Some(Arc::new(keyframes)),
            skeleton: Some(Arc::new(skeleton)),
            ..EntityAnim::default()
        };
        let mut entity = entity(0, 0, 0);
        entity.pos = [100, 0, 0];

        // The same composition the render path uses: the root keyframe
        // offset and each joint's relative translation fold through the
        // saturated entity diagonal.
        let worlds = clock.joint_worlds(&entity, &clips);
        assert_eq!(worlds.len(), 2);
        assert_eq!(worlds[0].t, [109, 0, 0]);
        assert_eq!(worlds[1].t, [128, 0, 0]);

        // The entity joint scale multiplies the rotation columns, tripling
        // the composed root translation.
        entity.joint_scale = 0x3000;
        let scaled = clock.joint_worlds(&entity, &clips);
        assert_eq!(scaled[0].t, [129, 0, 0]);
        assert_eq!(scaled[1].t, [188, 0, 0]);

        // Without a skeleton or keyframes the list is empty.
        let bare = EntityAnim {
            keyframes: clock.keyframes.take(),
            ..EntityAnim::default()
        };
        assert!(bare.joint_worlds(&entity, &clips).is_empty());
    }

    #[test]
    fn npc_keyframe_pose_interpolates_by_the_published_step() {
        use crate::model::Keyframe;
        let clips = vec![clip(&[(0, 1), (1, 1), (2, 1), (3, 1)])];
        let keyframes = vec![
            Keyframe {
                offset: [0, 100, 0],
                rotations: vec![[0, 0, 0]],
            },
            Keyframe {
                offset: [0, 300, 0],
                rotations: vec![[0x400, 0, 0]],
            },
            Keyframe {
                offset: [0, 500, 0],
                rotations: vec![[0x800, 0, 0]],
            },
            Keyframe {
                offset: [0, 700, 0],
                rotations: vec![[0xC00, 0, 0]],
            },
        ];
        let mut clock = EntityAnim::default();
        let mut entity = entity(0, 0, 0);
        entity.blend_counter = 7;
        let step = blend_step(&entity);
        assert_eq!(step, 0x200);

        // The first applied frame blends from itself.
        clock.advance(&mut entity, &clips, false, step);
        let pose = clock.pose_keyframe(&entity, &clips, &keyframes).unwrap();
        assert_eq!(pose.rotations[0], [0, 0, 0]);

        // Counter 6 weights the incoming keyframe 2/8: 0x400 * 2/8 = 0x100.
        clock.advance(&mut entity, &clips, false, step);
        let pose = clock.pose_keyframe(&entity, &clips, &keyframes).unwrap();
        assert_eq!(pose.rotations[0], [0x100, 0, 0]);
        assert_eq!(pose.offset[1], 150, "the root Y interpolates too");
    }

    #[test]
    fn a_clip_switch_blends_from_the_pose_on_screen() {
        use crate::model::Keyframe;
        use std::sync::Arc;

        let clips = vec![clip(&[(0, 1)]), clip(&[(1, 1), (1, 1)])];
        let keyframes = vec![
            Keyframe {
                offset: [0, 0, 0],
                rotations: vec![[0, 0, 0]],
            },
            Keyframe {
                offset: [0, 800, 0],
                rotations: vec![[0x400, 0, 0]],
            },
        ];
        let mut clock = EntityAnim {
            keyframes: Some(Arc::new(keyframes)),
            ..EntityAnim::default()
        };
        let mut entity = entity(0, 0, 0);
        entity.blend_counter = 7;

        // Play clip 0 so its pose is the pose on screen.
        clock.advance(&mut entity, &clips, false, 0x200);
        let first = clock
            .pose_keyframe(&entity, &clips, clock.keyframes.as_deref().unwrap())
            .unwrap();
        assert_eq!(first.rotations[0], [0, 0, 0]);

        // A script switches the animation id and re-arms the blend. The first
        // tick of clip 1 mixes the old pose with the new frame 0 at counter 7:
        // 7/8 old, 1/8 new.
        entity.animation_id = 1;
        entity.animation_frame_id = 0;
        entity.timing_control = 0;
        entity.blend_counter = 7;
        clock.advance(&mut entity, &clips, false, 0x200);
        let pose = clock
            .pose_keyframe(&entity, &clips, clock.keyframes.as_deref().unwrap())
            .unwrap();
        assert_eq!(pose.rotations[0], [0x80, 0, 0]);
        assert_eq!(pose.offset[1], 100);
    }

    #[test]
    fn apply_anim_vertex_reads_the_frame_and_offsets() {
        use crate::model::{Keyframe, Skeleton};
        use std::sync::Arc;

        let clips = vec![clip(&[(0, 1), (1, 3)])];
        let keyframes = vec![
            Keyframe {
                offset: [10, 0, 20],
                rotations: vec![[0, 0, 0]],
            },
            Keyframe {
                offset: [30, 0, 40],
                rotations: vec![[0, 0, 0]],
            },
        ];
        let clock = EntityAnim {
            keyframes: Some(Arc::new(keyframes)),
            skeleton: Some(Arc::new(Skeleton::default())),
            ..EntityAnim::default()
        };
        let mut wasp = entity(0, 0, 0);
        wasp.unk_c6 = 100;
        wasp.unk_c8 = 0x0032;
        wasp.pos = [500, 7, 500];
        wasp.angle = 0x400;

        // A spent hold reads the pending frame, the one the same tick's
        // advance is about to apply.
        clock.apply_anim_vertex(&mut wasp, &clips);
        let (x, z) = crate::player::rotate_xz(0x400, 10, 20);
        assert_eq!(wasp.pos, [100 + x, 7, 0x32 + z], "X/Z only");

        // A running hold reads the frame just shown.
        wasp.timing_control = 2;
        wasp.pos = [0, 7, 0];
        clock.apply_anim_vertex(&mut wasp, &clips);
        let (x, z) = crate::player::rotate_xz(0x400, 30, 40);
        assert_eq!(wasp.pos, [100 + x, 7, 0x32 + z]);

        // The step-back wraps from pending frame 0 to the clip's last frame.
        wasp.animation_frame_id = 0;
        wasp.pos = [0, 7, 0];
        clock.apply_anim_vertex(&mut wasp, &clips);
        assert_eq!(wasp.pos, [100 + x, 7, 0x32 + z]);

        // A spent hold at frame 1 reads frame 1 itself.
        wasp.animation_frame_id = 1;
        wasp.timing_control = 0;
        wasp.pos = [0, 7, 0];
        clock.apply_anim_vertex(&mut wasp, &clips);
        let (x, z) = crate::player::rotate_xz(0x400, 30, 40);
        assert_eq!(wasp.pos, [100 + x, 7, 0x32 + z]);

        // The base offsets zero-extend through their 16-bit fields, like the
        // original's `(unsigned int)` cast.
        let mut wrapped = entity(0, 0, 0);
        wrapped.unk_c6 = 0xFFCE;
        wrapped.unk_c8 = 0;
        wrapped.angle = 0;
        clock.apply_anim_vertex(&mut wrapped, &clips);
        let (x, _) = crate::player::rotate_xz(0, 10, 20);
        assert_eq!(wrapped.pos[0], 0xFFCE + x, "the u16 base zero-extends");

        // No keyframe table or an out-of-range frame changes nothing.
        let bare = EntityAnim::default();
        let mut untouched = entity(0, 0, 0);
        untouched.pos = [1, 2, 3];
        bare.apply_anim_vertex(&mut untouched, &clips);
        assert_eq!(untouched.pos, [1, 2, 3]);
        let mut untouched = entity(0, 0, 0);
        untouched.animation_frame_id = 9;
        untouched.pos = [1, 2, 3];
        clock.apply_anim_vertex(&mut untouched, &clips);
        assert_eq!(untouched.pos, [1, 2, 3]);
    }

    #[test]
    fn look_at_slew_steps_the_yaw_and_clamps_to_the_cone() {
        let mut clock = EntityAnim::default();
        let mut entity = entity(0, 0, 0);
        entity.look_at_flags = 0x11; // slew enable + yaw
        entity.look_at_yaw_step = 0x40;
        entity.look_at_pitch_step = 0x40;
        // 45 degrees to the right of the entity's facing.
        entity.target = [1000, 0, 1000];
        clock.slew_look_at(&entity);
        assert_eq!(clock.look_at_yaw, -0x40);
        for _ in 0..7 {
            clock.slew_look_at(&entity);
        }
        assert_eq!(clock.look_at_yaw, -0x200);
        clock.slew_look_at(&entity);
        assert_eq!(clock.look_at_yaw, -0x200, "snapped and held");

        // Straight behind the entity the target lies outside the +/-0x2C8
        // cone, so the slew stops on the cone's edge.
        entity.target = [0, 0, 1000];
        for _ in 0..16 {
            clock.slew_look_at(&entity);
        }
        assert_eq!(clock.look_at_yaw, -0x2C8);

        // A cleared slew-enable bit freezes the angles in place.
        let frozen = clock.look_at_yaw;
        entity.look_at_flags = 0;
        entity.target = [1000, 0, 0];
        clock.slew_look_at(&entity);
        assert_eq!(clock.look_at_yaw, frozen);
    }

    #[test]
    fn look_at_yaw_snaps_only_under_one_step_and_keeps_out_of_cone_snaps() {
        // yaw_diff folds the step into the turn, so the snap window is
        // [-step, step): 0x3F snaps, 0x40 steps.
        assert_eq!(slew_yaw(0, 0x3F, 0x40), 0x3F);
        assert_eq!(slew_yaw(0, 0x40, 0x40), 0x40);

        // A snap whose target is outside the +/-0x2C8 cone keeps the previous
        // angle instead of jumping to the target.
        assert_eq!(slew_yaw(-0x2C8, 0xD00, 0x40), -0x2C8);
        assert_eq!(slew_yaw(0, 0, 0x40) as u16 & 0xFFF, 0);
    }

    #[test]
    fn look_at_slew_steps_the_pitch_at_its_own_rate() {
        let mut clock = EntityAnim::default();
        let mut entity = entity(0, 0, 0);
        entity.look_at_flags = 0x12; // slew enable + pitch
        entity.look_at_pitch_step = 0x40;
        entity.look_at_yaw_step = 0x40;
        // 250 below over 1000 horizontal: atan(0.25) in 12-bit units.
        let target_pitch = -(((0.25f64).atan() * (2048.0 / std::f64::consts::PI)) as i16);
        entity.target = [0, -250, 1000];
        clock.slew_look_at(&entity);
        assert_eq!(clock.look_at_pitch, -0x40);
        // The remaining turn (95 units) is under two steps, so the second
        // call snaps to the target.
        clock.slew_look_at(&entity);
        assert_eq!(clock.look_at_pitch, target_pitch, "snapped");
        assert_eq!(clock.look_at_yaw, 0, "the yaw bit is clear");
    }
}
