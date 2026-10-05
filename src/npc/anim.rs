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
}

impl Default for EntityAnim {
    fn default() -> Self {
        Self {
            player: AnimPlayer::new(0),
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
    /// reads the frame data backwards; `blend_step` is the caller's step (kept
    /// for the interpolation the renderer does not do yet). Returns whether
    /// the clip just completed.
    ///
    /// TODO(parity): (gameplay) the original interpolates between the current
    /// and pending keyframes by `blend_step` (the published `blend_counter`
    /// scaled step) and runs `entity_apply_anim_vertex` for the vertex-anim
    /// joints; the port poses whole keyframes only, so NPC motion is a step
    /// animation instead of a blend.
    pub fn advance(
        &mut self,
        entity: &mut Entity,
        clips: &[Clip],
        reverse: bool,
        _blend_step: u16,
    ) -> bool {
        self.sync(entity);
        self.player.reverse = reverse;
        let held = self.player.timing > 1;
        let completed = self.player.update(clips);
        entity.animation_frame_id = self.player.frame as u8;
        entity.timing_control = self.player.timing.min(u16::from(u8::MAX)) as u8;
        if !held && entity.blend_counter > 0 {
            entity.blend_counter -= 1;
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
}
