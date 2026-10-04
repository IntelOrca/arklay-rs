//! Skeleton animation: fixed-point joint matrices and clip playback.
//!
//! The engine poses the player model by composing one 4.12 fixed-point matrix
//! per joint and then transforming each mesh object by its joint's world
//! matrix. [`entity_matrix`] places the whole entity in the room,
//! [`joint_matrices`] composes the skeleton hierarchy from one keyframe, and
//! [`AnimPlayer`] is the 30 Hz clip clock driven by the player state machine.

use std::sync::OnceLock;

use crate::model;

/// 4.12 fixed-point 3x3 rotation plus integer translation.
///
/// Row-major, column-vector convention: applying the matrix to a vector is
/// `out[i] = r[i][0]*v[0] + r[i][1]*v[1] + r[i][2]*v[2]`. 1.0 is 4096; the
/// game's trig tables saturate at 0x3FFF, so a rotation built from them never
/// reads back as exactly 4096.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mat4x3 {
    pub r: [[i32; 3]; 3],
    pub t: [i32; 3],
}

/// Entity matrix: rotation from a 12-bit direction angle with translation `pos`.
///
/// The game feeds its rotation builder `(0x1000 - x, y, 0x1000 - z)`; a
/// yaw-only entity has `(x, z) = 0`, so this builds `rotation_matrix(0, angle, 0)`.
/// At yaw `a` the model's +X axis maps to world `(cos a, 0, -sin a)`, the same
/// direction [`crate::player`] uses for movement: angle 0 walks along +X and
/// increasing yaw turns toward -Z.
pub fn entity_matrix(pos: [i32; 3], angle: u16) -> Mat4x3 {
    Mat4x3 {
        r: rotation_matrix(0, i32::from(angle), 0),
        t: pos,
    }
}

/// Compose world matrices for every joint:
/// `world[0] = entity * transform[0]`; `world[i] = world[parent] * transform[i]`.
///
/// `transform[i]` rotates by `keyframe.rotations[i]` (or the zero rotation when
/// the keyframe has fewer entries) and translates by `skeleton.relative[i]`.
/// The parent of each joint is derived from `skeleton.children`; out-of-range
/// child indices and joints with no parent are treated as roots, and a parent
/// cycle falls back to treating the remaining joints as roots. Joints beyond
/// the relative list are not produced, so the result length equals
/// `skeleton.relative.len()`.
pub fn joint_matrices(
    skeleton: &model::Skeleton,
    keyframe: &model::Keyframe,
    entity: &Mat4x3,
) -> Vec<Mat4x3> {
    let local: Vec<Mat4x3> = skeleton
        .relative
        .iter()
        .enumerate()
        .map(|(index, relative)| {
            let rotation = keyframe.rotations.get(index).copied().unwrap_or([0, 0, 0]);
            // The root joint's translation comes from the keyframe (root motion);
            // every other joint keeps its relative bind-pose translation.
            let translation = if index == 0 {
                keyframe.offset
            } else {
                *relative
            };
            Mat4x3 {
                r: rotation_matrix(
                    i32::from(rotation[0]),
                    i32::from(rotation[1]),
                    i32::from(rotation[2]),
                ),
                t: translation.map(i32::from),
            }
        })
        .collect();

    let count = local.len();
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
        t: [0, 0, 0],
    };
    let mut world = vec![identity; count];
    let mut resolved = vec![false; count];
    let mut unresolved: Vec<usize> = (0..count).collect();
    while !unresolved.is_empty() {
        let mut remaining = Vec::new();
        for &index in &unresolved {
            match parent[index] {
                Some(parent_index) if resolved[parent_index] => {
                    world[index] = compose(&world[parent_index], &local[index]);
                    resolved[index] = true;
                }
                Some(_) => remaining.push(index),
                None => {
                    world[index] = compose(entity, &local[index]);
                    resolved[index] = true;
                }
            }
        }
        if remaining.len() == unresolved.len() {
            for &index in &remaining {
                world[index] = compose(entity, &local[index]);
                resolved[index] = true;
            }
            break;
        }
        unresolved = remaining;
    }
    world
}

/// Angular step of the game's trig tables: `2*pi / 4096`.
const ANGLE_STEP: f64 = 0.0015339807880859375;

/// Build one 14-bit trig table, truncating the `f64` product toward zero and
/// saturating an exact `+/-1.0` so it never wraps to the signed minimum.
fn trig_table(function: fn(f64) -> f64) -> [i32; 4096] {
    let mut table = [0i32; 4096];
    for (index, entry) in table.iter_mut().enumerate() {
        let angle = (index as f64) * ANGLE_STEP;
        *entry = ((function(angle) * 16384.0) as i32).clamp(-0x3FFF, 0x3FFF);
    }
    table
}

/// The game's sine table: 4096 entries at 14-bit amplitude.
///
/// The game keeps sine and cosine as two independently built tables, so this
/// does too: `sin(angle + 0x400)` and `cos(angle)` can differ by one entry
/// after truncation, and the joint chain would amplify that into a visible
/// mismatch.
fn sin_table() -> &'static [i32; 4096] {
    static TABLE: OnceLock<[i32; 4096]> = OnceLock::new();
    TABLE.get_or_init(|| trig_table(f64::sin))
}

/// The game's cosine table, built exactly like the sine table.
fn cos_table() -> &'static [i32; 4096] {
    static TABLE: OnceLock<[i32; 4096]> = OnceLock::new();
    TABLE.get_or_init(|| trig_table(f64::cos))
}

/// 14-bit sine of a 12-bit angle (angles wrap modulo 0x1000).
fn sin14(angle: i32) -> i32 {
    sin_table()[(angle & 0x0FFF) as usize]
}

/// 14-bit cosine of a 12-bit angle.
fn cos14(angle: i32) -> i32 {
    cos_table()[(angle & 0x0FFF) as usize]
}

/// `(a * b) >> 14` for two 14-bit values, truncated toward zero.
fn mul14(a: i32, b: i32) -> i32 {
    let product = a * b;
    (product + ((product >> 31) & 0x3FFF)) >> 14
}

/// `(a * b) >> 12` for a 4.12 element, truncated toward zero. The product is
/// formed at 64 bits so large translations cannot wrap.
fn mul12(a: i32, b: i32) -> i64 {
    let product = i64::from(a) * i64::from(b);
    (product + ((product >> 63) & 0xFFF)) >> 12
}

/// The nine 14-bit rotation components from Euler angles `(sx, sy, sz)`, in
/// row-major order. Each table product is floored back to 14 bits before the
/// next is formed; the negative products are corrected toward zero first.
fn rotation_components(sx: i32, sy: i32, sz: i32) -> [i32; 9] {
    let (sin_x, cos_x) = (sin14(sx), cos14(sx));
    let (sin_y, cos_y) = (sin14(sy), cos14(sy));
    let (sin_z, cos_z) = (sin14(sz), cos14(sz));

    [
        mul14(cos_z, cos_y),
        -mul14(cos_y, sin_z),
        sin_y,
        mul14(mul14(sin_x, sin_y), cos_z) + mul14(cos_x, sin_z),
        mul14(cos_x, cos_z) - mul14(mul14(sin_x, sin_z), sin_y),
        -mul14(sin_x, cos_y),
        mul14(sin_x, sin_z) - mul14(mul14(cos_x, sin_y), cos_z),
        mul14(mul14(sin_z, sin_y), cos_x) + mul14(sin_x, cos_z),
        mul14(cos_x, cos_y),
    ]
}

/// 4.12 rotation matrix from an `(x, y, z)` 12-bit Euler triple.
///
/// Reproduces the game's rotation builder used for entity yaw and every
/// keyframe joint: the angles handed to the component math are
/// `(0x1000 - x, y, 0x1000 - z)` and the nine 14-bit components are scaled to
/// 4.12 with a truncating `>> 2`. The result is row-major; at `(0, 0, 0)` it is
/// the near-identity `diag(4095, 4095, 4095)` because the cosine table
/// saturates.
fn rotation_matrix(x: i32, y: i32, z: i32) -> [[i32; 3]; 3] {
    let components = rotation_components(0x1000 - x, y, 0x1000 - z);
    let mut matrix = [[0i32; 3]; 3];
    for (slot, component) in components.iter().enumerate() {
        matrix[slot / 3][slot % 3] = (*component + ((*component >> 31) & 3)) >> 2;
    }
    matrix
}

/// `a * b` for two [`Mat4x3`] values: rotation matrix product and the game's
/// conjugated translation, every 4.12 product truncated toward zero.
///
/// The game composes the translation with the PS1 Y-sign convention:
/// `t = D * (a.r * (D * b.t)) + a.t` where `D = diag(1, -1, 1)`. Only the
/// Y-sign conjugation around the rotation differs from a plain product, and it
/// matters for every pitched or rolled joint: with a plain product the child
/// translations come out mirrored, which visibly throws limbs to the wrong
/// side.
fn compose(a: &Mat4x3, b: &Mat4x3) -> Mat4x3 {
    let mut r = [[0i32; 3]; 3];
    for (i, row) in r.iter_mut().enumerate() {
        for (j, entry) in row.iter_mut().enumerate() {
            let mut sum = 0i64;
            for k in 0..3 {
                sum += mul12(a.r[i][k], b.r[k][j]);
            }
            *entry = sum as i32;
        }
    }

    let source = [b.t[0], b.t[1].wrapping_neg(), b.t[2]];
    let mut t = [0i32; 3];
    for (row, value) in a.r.iter().zip(t.iter_mut()) {
        let mut sum = 0i64;
        for (element, &input) in row.iter().zip(&source) {
            sum += mul12(*element, input);
        }
        *value = sum as i32;
    }
    t[1] = t[1].wrapping_neg();
    for (value, parent) in t.iter_mut().zip(a.t) {
        *value = value.wrapping_add(parent);
    }

    Mat4x3 { r, t }
}

/// Clip playback state. One `update` is one 30 Hz tick.
///
/// `frame` is the index of the next frame the game applies and
/// `display_frame` is the frame currently shown. They agree on a tick that
/// consumes a frame; while a frame is held by its timing the display stays on
/// the frame applied last and `frame` has already moved past it, matching the
/// game's order (compute speed from the pending frame, apply it, advance).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnimPlayer {
    pub clip: usize,
    pub frame: usize,
    pub display_frame: usize,
    pub timing: u16,
}

impl AnimPlayer {
    /// Start clip `clip` on its first frame with no hold left.
    pub fn new(clip: usize) -> Self {
        Self {
            clip,
            frame: 0,
            display_frame: 0,
            timing: 0,
        }
    }

    /// Switch clips and restart playback from frame 0.
    pub fn set_clip(&mut self, clip: usize) {
        self.clip = clip;
        self.frame = 0;
        self.display_frame = 0;
        self.timing = 0;
    }

    /// Keyframe of the displayed clip frame, or 0 when the clip or frame is
    /// out of range.
    pub fn keyframe_index(&self, clips: &[model::Clip]) -> usize {
        clips
            .get(self.clip)
            .and_then(|clip| clip.frames.get(self.display_frame))
            .map_or(0, |frame| usize::from(frame.keyframe))
    }

    /// Advance one tick and report whether the clip just completed.
    ///
    /// If `timing > 1` it is decremented and the frame is held. Otherwise the
    /// frame is consumed: it becomes the displayed frame, its timing is
    /// reloaded and the frame index advances; passing the last frame wraps to
    /// 0 and returns true. A frame whose timing is 1 therefore advances every
    /// tick. An out-of-range clip or an empty frame list resets the player and
    /// returns false.
    pub fn update(&mut self, clips: &[model::Clip]) -> bool {
        if self.timing > 1 {
            self.timing -= 1;
            return false;
        }

        let Some(clip) = clips.get(self.clip) else {
            self.frame = 0;
            self.display_frame = 0;
            self.timing = 0;
            return false;
        };
        if clip.frames.is_empty() {
            self.frame = 0;
            self.display_frame = 0;
            self.timing = 0;
            return false;
        }

        let index = if self.frame < clip.frames.len() {
            self.frame
        } else {
            0
        };
        self.display_frame = index;
        self.timing = clip.frames[index].timing;
        self.frame = index + 1;
        if self.frame >= clip.frames.len() {
            self.frame = 0;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Clip, ClipFrame, Keyframe, Skeleton};

    fn axis_x(matrix: &Mat4x3) -> [i32; 3] {
        [matrix.r[0][0], matrix.r[1][0], matrix.r[2][0]]
    }

    fn axis_z(matrix: &Mat4x3) -> [i32; 3] {
        [matrix.r[0][2], matrix.r[1][2], matrix.r[2][2]]
    }

    fn timing_clip(timings: &[u16]) -> Clip {
        Clip {
            frames: timings
                .iter()
                .enumerate()
                .map(|(index, &timing)| ClipFrame {
                    keyframe: index as u16,
                    timing,
                })
                .collect(),
        }
    }

    #[test]
    fn entity_matrix_quadrant_angles_match_the_rendering_convention() {
        let cases = [
            (0x000u16, [[4095, 0, 0], [0, 4095, 0], [0, 0, 4095]]),
            (0x400, [[0, 0, 4095], [0, 4095, 0], [-4095, 0, 0]]),
            (0x800, [[-4095, 0, 0], [0, 4095, 0], [0, 0, -4095]]),
            (0xC00, [[0, 0, -4095], [0, 4095, 0], [4095, 0, 0]]),
        ];

        for (angle, expected) in cases {
            let matrix = entity_matrix([0; 3], angle);
            assert_eq!(matrix.r, expected, "angle 0x{angle:03X}");
        }
    }

    #[test]
    fn entity_matrix_maps_the_model_axes_as_documented() {
        let yaw_0 = entity_matrix([0; 3], 0x000);
        assert_eq!(axis_x(&yaw_0), [4095, 0, 0]);
        assert_eq!(axis_z(&yaw_0), [0, 0, 4095]);

        let yaw_90 = entity_matrix([0; 3], 0x400);
        assert_eq!(axis_x(&yaw_90), [0, 0, -4095]);
        assert_eq!(axis_z(&yaw_90), [4095, 0, 0]);

        let yaw_180 = entity_matrix([0; 3], 0x800);
        assert_eq!(axis_x(&yaw_180), [-4095, 0, 0]);
        assert_eq!(axis_z(&yaw_180), [0, 0, -4095]);

        let yaw_270 = entity_matrix([0; 3], 0xC00);
        assert_eq!(axis_x(&yaw_270), [0, 0, 4095]);
        assert_eq!(axis_z(&yaw_270), [-4095, 0, 0]);
    }

    #[test]
    fn entity_x_axis_agrees_with_the_player_movement_convention() {
        // Mirrors the table and shifts of src/player.rs `rotate_speed`: `speed`
        // is applied along `(cos, -sin)` in XZ.
        fn movement(angle: u16) -> (i32, i32) {
            let radians = f64::from(angle & 0x0FFF) * ANGLE_STEP;
            let cos = ((radians.cos() * 16384.0) as i32).clamp(-0x3FFF, 0x3FFF);
            let sin = ((radians.sin() * 16384.0) as i32).clamp(-0x3FFF, 0x3FFF);
            (cos >> 2, (-sin) >> 2)
        }

        for angle in [0x000u16, 0x200, 0x400, 0x600, 0x800, 0xA00, 0xC00, 0xE00] {
            let matrix = entity_matrix([0; 3], angle);
            let x = matrix.r[0][0];
            let z = matrix.r[2][0];
            let (movement_x, movement_z) = movement(angle);

            assert_eq!(x.signum(), movement_x.signum(), "x sign at 0x{angle:03X}");
            assert_eq!(z.signum(), movement_z.signum(), "z sign at 0x{angle:03X}");
            assert!((x - movement_x).abs() <= 2, "x magnitude at 0x{angle:03X}");
            assert!((z - movement_z).abs() <= 2, "z magnitude at 0x{angle:03X}");
        }
    }

    #[test]
    fn entity_matrix_copies_translation_and_wraps_angles() {
        let matrix = entity_matrix([123, -456, 789], 0x321);
        assert_eq!(matrix.t, [123, -456, 789]);
        assert_eq!(
            entity_matrix([123, -456, 789], 0x400),
            entity_matrix([123, -456, 789], 0x1400),
            "angles wrap modulo 0x1000"
        );
    }

    #[test]
    fn joint_chain_composes_parent_and_child_translations() {
        let skeleton = Skeleton {
            relative: vec![[10, 0, 0], [20, 0, 0]],
            children: vec![vec![1], vec![]],
        };
        let keyframe = Keyframe {
            offset: [10, 0, 0],
            rotations: vec![[0, 0, 0]; 2],
        };
        let entity = entity_matrix([100, 0, 0], 0);

        let world = joint_matrices(&skeleton, &keyframe, &entity);

        assert_eq!(world.len(), 2);
        assert_eq!(world[0].t, [109, 0, 0]);
        assert_eq!(world[1].t, [128, 0, 0]);
        // Every composition multiplies the saturated 4095 diagonal down one
        // more sample; the entity's own 4095 diagonal does the same to joint 0.
        assert_eq!(world[0].r, [[4094, 0, 0], [0, 4094, 0], [0, 0, 4094]]);
        assert_eq!(world[1].r, [[4093, 0, 0], [0, 4093, 0], [0, 0, 4093]]);
    }

    #[test]
    fn joint_matrices_tolerate_bad_children_and_missing_parents() {
        let skeleton = Skeleton {
            relative: vec![[100, 0, 0], [200, 0, 0], [300, 0, 0]],
            // 9 and 255 are out of range; 1 is a real child of 0, while joint 2
            // appears in no child list and becomes a second root.
            children: vec![vec![9, 1], vec![255], vec![]],
        };
        let keyframe = Keyframe {
            offset: [100, 0, 0],
            rotations: Vec::new(),
        };
        let entity = entity_matrix([0; 3], 0);

        let world = joint_matrices(&skeleton, &keyframe, &entity);

        assert_eq!(world.len(), 3);
        assert_eq!(world[0].t, [99, 0, 0]);
        assert_eq!(world[1].t, [298, 0, 0]);
        assert_eq!(world[2].t, [299, 0, 0]);
    }

    #[test]
    fn joint_matrices_survive_a_parent_cycle() {
        let skeleton = Skeleton {
            relative: vec![[1, 0, 0], [2, 0, 0]],
            children: vec![vec![1], vec![0]],
        };
        let keyframe = Keyframe::default();
        let entity = entity_matrix([0; 3], 0);

        let world = joint_matrices(&skeleton, &keyframe, &entity);

        assert_eq!(world.len(), 2);
    }

    #[test]
    fn empty_skeletons_produce_no_matrices() {
        let world = joint_matrices(
            &Skeleton::default(),
            &Keyframe::default(),
            &Mat4x3 {
                r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                t: [0, 0, 0],
            },
        );
        assert!(world.is_empty());
    }

    #[test]
    fn anim_timing_above_one_holds_the_frame() {
        let clips = vec![timing_clip(&[3, 1])];
        let mut player = AnimPlayer::new(0);

        // First tick consumes frame 0 and reloads its timing.
        assert!(!player.update(&clips));
        assert_eq!((player.frame, player.timing), (1, 3));

        // timing > 1 holds the frame and counts down.
        assert!(!player.update(&clips));
        assert_eq!((player.frame, player.timing), (1, 2));
        assert!(!player.update(&clips));
        assert_eq!((player.frame, player.timing), (1, 1));

        // timing == 1 releases frame 1, which is the last frame: wrap.
        assert!(player.update(&clips));
        assert_eq!((player.frame, player.timing), (0, 1));
    }

    #[test]
    fn displayed_frame_holds_for_the_frame_timing() {
        // Frame 0 has timing 3, frame 1 timing 1: the game shows frame 0 on
        // the first three ticks and frame 1 on the fourth, then wraps.
        let clips = vec![timing_clip(&[3, 1])];
        let mut player = AnimPlayer::new(0);

        assert!(!player.update(&clips));
        assert_eq!(
            (player.display_frame, player.frame, player.timing),
            (0, 1, 3)
        );
        assert!(!player.update(&clips));
        assert_eq!((player.display_frame, player.timing), (0, 2));
        assert!(!player.update(&clips));
        assert_eq!((player.display_frame, player.timing), (0, 1));
        assert!(player.update(&clips));
        assert_eq!((player.display_frame, player.frame), (1, 0));
    }

    #[test]
    fn anim_timing_one_advances_every_tick_and_wraps() {
        let clips = vec![timing_clip(&[1, 1, 1])];
        let mut player = AnimPlayer::new(0);

        assert!(!player.update(&clips));
        assert_eq!(player.frame, 1);
        assert!(!player.update(&clips));
        assert_eq!(player.frame, 2);
        assert!(player.update(&clips));
        assert_eq!(player.frame, 0);
        assert!(!player.update(&clips));
        assert_eq!(player.frame, 1);
    }

    #[test]
    fn set_clip_resets_frame_and_timing() {
        let clips = vec![timing_clip(&[1, 1]), timing_clip(&[1, 1, 1])];
        let mut player = AnimPlayer::new(0);

        player.update(&clips);
        assert_ne!(player.frame, 0);

        player.set_clip(1);

        assert_eq!((player.clip, player.frame, player.timing), (1, 0, 0));
    }

    #[test]
    fn keyframe_index_follows_the_frame_and_is_zero_out_of_range() {
        let clips = vec![Clip {
            frames: vec![
                ClipFrame {
                    keyframe: 7,
                    timing: 1,
                },
                ClipFrame {
                    keyframe: 9,
                    timing: 1,
                },
            ],
        }];
        let mut player = AnimPlayer::new(0);

        assert_eq!(player.keyframe_index(&clips), 7);
        player.display_frame = 1;
        assert_eq!(player.keyframe_index(&clips), 9);
        player.display_frame = 5;
        assert_eq!(player.keyframe_index(&clips), 0);
        player.display_frame = 0;
        player.clip = 3;
        assert_eq!(player.keyframe_index(&clips), 0);
    }

    #[test]
    fn update_out_of_range_clip_or_empty_frames_is_safe() {
        let mut player = AnimPlayer::new(9);
        assert!(!player.update(&[]));
        assert_eq!((player.clip, player.frame, player.timing), (9, 0, 0));

        let clips = vec![Clip::default(), timing_clip(&[])];
        player.set_clip(1);
        assert!(!player.update(&clips));
        assert_eq!((player.frame, player.timing), (0, 0));

        player.frame = 99;
        player.timing = 0;
        assert!(!player.update(&clips));
        assert_eq!(player.frame, 0);
    }

    #[test]
    fn compose_translation_uses_the_y_sign_conjugation() {
        // A pitched parent: RotMatrix(0x200, 0, 0) is
        // [[4095, 0, 0], [0, 2896, 2896], [0, -2896, 2896]].
        let parent = Mat4x3 {
            r: [[4095, 0, 0], [0, 2896, 2896], [0, -2896, 2896]],
            t: [0, 0, 0],
        };
        let child = Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [100, 200, 300],
        };

        let world = compose(&parent, &child);

        // The child translation is Y-negated, rotated by the parent, and the
        // result's Y negated again before the parent translation is added.
        assert_eq!(world.t, [99, -71, 353]);
        assert_eq!(world.r, parent.r);
    }

    #[test]
    fn compose_keeps_yaw_only_translations_unchanged() {
        // A yaw parent commutes with the Y-sign conjugation, so the plain and
        // conjugated products agree exactly.
        let parent = Mat4x3 {
            r: [[0, 0, 4095], [0, 4095, 0], [-4095, 0, 0]],
            t: [7, 8, 9],
        };
        let child = Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [100, 200, 300],
        };

        let world = compose(&parent, &child);

        assert_eq!(world.t, [306, 207, -90]);
    }

    /// FNV-1a hash over every joint matrix the real player model produces.
    fn joint_matrix_hash(skeleton: &Skeleton, keyframes: &[Keyframe]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for keyframe in keyframes {
            for angle in [0u16, 0x123, 0x800, 0xC00] {
                let entity = entity_matrix([0, 0, 0], angle);
                for matrix in joint_matrices(skeleton, keyframe, &entity) {
                    for row in &matrix.r {
                        for &value in row {
                            for byte in value.to_le_bytes() {
                                hash ^= u64::from(byte);
                                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
                            }
                        }
                    }
                    for value in matrix.t {
                        for byte in value.to_le_bytes() {
                            hash ^= u64::from(byte);
                            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
                        }
                    }
                }
            }
        }
        hash
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn real_keyframes_match_the_original_joint_matrices() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let data = std::fs::read(format!("{root}/JPN/ENEMY/CHAR11.EMD")).unwrap();
        let emd = crate::emd::parse(&data).unwrap();

        // Full matrices of keyframe 0 at yaw 0, as the original computes them.
        let expected: [([[i32; 3]; 3], [i32; 3]); 15] = [
            (
                [[4093, -118, 0], [118, 4093, 0], [0, 0, 4094]],
                [0, -1707, 0],
            ),
            (
                [[4042, 652, 0], [-652, 4042, 0], [0, 0, 4093]],
                [-36, -2324, 0],
            ),
            ([[4092, 82, 0], [-82, 4092, 0], [0, 0, 4093]], [0, -1707, 0]),
            (
                [[4088, 199, -2], [-199, 4087, -99], [-1, 99, 4091]],
                [-25, -1509, 165],
            ),
            (
                [[4091, 36, 16], [-37, 4083, 252], [-9, -251, 4083]],
                [-64, -909, 141],
            ),
            (
                [[4031, 36, -692], [-40, 4089, -24], [695, 31, 4029]],
                [-91, -104, 188],
            ),
            (
                [[4088, 199, -1], [-199, 4087, -55], [0, 55, 4092]],
                [-25, -1509, -135],
            ),
            (
                [[4091, 80, -8], [-80, 4084, -234], [5, 234, 4085]],
                [-74, -909, -144],
            ),
            (
                [[4058, 79, 517], [-87, 4089, 63], [-514, -73, 4057]],
                [-99, -104, -192],
            ),
            (
                [[4073, 396, -54], [-399, 4044, -488], [5, 492, 4063]],
                [-51, -2281, 275],
            ),
            (
                [[3914, -1177, -161], [1171, 3917, -170], [200, 119, 4084]],
                [-98, -1858, 288],
            ),
            (
                [[3895, -1164, 440], [1186, 3910, -170], [-376, 290, 4062]],
                [24, -1489, 330],
            ),
            (
                [[4088, 215, 26], [-217, 4045, 588], [3, -588, 4051]],
                [-52, -2270, -265],
            ),
            (
                [[3992, -880, 165], [879, 3995, 43], [-173, -4, 4088]],
                [-72, -1867, -274],
            ),
            (
                [[3979, -878, -343], [883, 3993, 45], [322, -114, 4076]],
                [15, -1520, -318],
            ),
        ];
        let world = joint_matrices(&emd.skeleton, &emd.keyframes[0], &entity_matrix([0; 3], 0));
        assert_eq!(world.len(), expected.len());
        for (index, (matrix, (r, t))) in world.iter().zip(expected).enumerate() {
            assert_eq!(matrix.r, r, "joint {index} rotation");
            assert_eq!(matrix.t, t, "joint {index} translation");
        }

        // And a golden hash over every keyframe at four yaws.
        assert_eq!(
            joint_matrix_hash(&emd.skeleton, &emd.keyframes),
            10_834_008_723_141_415_800
        );
    }
}
