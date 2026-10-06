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

use crate::anim::AnimPlayer;
use crate::game::Entity;
use crate::model::Clip;

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
}

impl Default for EntityAnim {
    fn default() -> Self {
        Self {
            player: AnimPlayer::new(0),
            look_at_yaw: 0,
            look_at_pitch: 0,
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
    /// When the entity is mid-ease (`blend_counter` armed on its consuming
    /// ticks) the previous and current keyframes are interpolated with the
    /// published blend step; otherwise the applied keyframe is used directly.
    pub fn pose_keyframe(
        &self,
        entity: &Entity,
        clips: &[Clip],
        keyframes: &[crate::model::Keyframe],
    ) -> Option<crate::model::Keyframe> {
        let current = keyframes.get(self.keyframe_index(entity, clips))?;
        if self.player.blend_used == 0
            || self.player.blend_step == 0
            || self.player.clip != usize::from(entity.animation_id)
        {
            return Some(current.clone());
        }
        let Some(previous) = keyframes.get(self.player.previous_keyframe) else {
            return Some(current.clone());
        };
        Some(crate::anim::blend_keyframes(
            previous,
            current,
            self.player.blend_used,
            self.player.blend_step,
        ))
    }

    /// Slew the tracking joint's yaw and pitch toward the entity's stored
    /// look-at target.
    ///
    /// A `0x10` look-at flag enables the slew; bit 0 enables the yaw and bit 1
    /// the pitch. The yaw target is the angle to the target minus the entity's
    /// own facing; the pitch is the target's elevation over the horizontal
    /// distance. Both step by at most `look_at_yaw_step`/`look_at_pitch_step`
    /// per call and clamp to the original's +/-0x2C8 yaw and +/-0x138 pitch
    /// cone. Cleared flags leave the last angles frozen, so the joint holds
    /// its aim instead of snapping back.
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
        self.look_at_yaw = step_angle(
            self.look_at_yaw,
            yaw_target,
            u16::from(entity.look_at_yaw_step),
            YAW_CONE,
        );
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
