//! Presentation tweening for render rates above the 30 Hz simulation.
//!
//! The simulation always steps at the original's fixed tick. When the
//! `[graphics] frame_rate` is higher, the loop presents several frames per
//! tick and each one draws the scene between the last two ticks: `alpha` is
//! how far the wall clock has moved from the previous tick toward the current
//! one, `0.0..=1.0`. Nothing here is read back by the simulation; a disabled
//! [`Tween`] (traces, captures, plain headless runs and any rate at or below
//! the tick rate) records nothing and leaves every value exact. Only a
//! headless `--stats --frame-rate` run above 30 enables one, to time the
//! frames it would present.
//!
//! The cache works on what the renderer is about to draw, so one mechanism
//! covers every moving thing:
//!
//! - **Tweened:** the posed joint matrices of the player, NPCs, held weapons,
//!   web clones, Plant 42 companions, Tyrant parts and the room's object and
//!   item models (position, facing and animation frame all live in them);
//!   ground shadows; effect billboards (item sparkles included); the
//!   room-entry fade.
//! - **Snapped:** a camera cut or room change drops the whole cache; a model
//!   that jumped more than [`TELEPORT_DISTANCE`] or rotated a joint by more
//!   than 60 degrees in one tick (a teleport or a scripted pose snap) draws
//!   its current pose; an effect that jumped more than
//!   [`BILLBOARD_JUMP`] pixels, changed its sprite frame's pivot or whose
//!   pool slot now holds a different spawn; the screen shake; the UI,
//!   menus, messages, door animations, the DIED screen and films.
//!
//! - **Camera:** every RE1 camera is a fixed pre-rendered cut, so the camera
//!   only ever changes by cutting, and a cut always snaps.
//!
//! The previous tick's values are only those drawn on that tick, so a frame
//! that follows two or more ticks without a draw (a hitch) snaps.

use std::collections::HashMap;

use crate::anim::Mat4x3;
use crate::state::RoomId;
use crate::transition::Overlay;

/// A root translation jump (world units) treated as a teleport.
pub const TELEPORT_DISTANCE: i64 = 1500;
/// A billboard jump (screen pixels) treated as a respawn.
pub const BILLBOARD_JUMP: i32 = 48;
/// The smallest joint-axis cosine still tweened (cos 60 degrees).
const MIN_AXIS_COS: f32 = 0.5;

/// What a cached set of joint matrices belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// The player model.
    Player,
    /// The NPC in entity slot `n` with entity id `id`, so a slot that a
    /// different enemy took over snaps.
    Npc(usize, u8),
    /// Room object record `n`.
    Object(usize),
    /// Item record `n`.
    Item(usize),
    /// Web-thread clone `n`.
    Web(usize),
    /// Plant 42 companion `n`.
    Companion(usize),
    /// The Tyrant scratch draw `n` of entity slot `slot`.
    Tyrant(usize, usize),
}

/// A shadow's tweened placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShadowPose {
    pos: [i32; 3],
    angle: u16,
}

/// An effect billboard's tweened rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Billboard {
    spawn: [i32; 4],
    uv: [u8; 4],
    pos: [i32; 2],
    size: [i32; 2],
}

/// Everything drawn on one tick.
#[derive(Debug, Default)]
struct Frame {
    /// The room and camera cut the values were drawn in.
    view: Option<(RoomId, usize)>,
    joints: HashMap<Key, Vec<Mat4x3>>,
    shadows: HashMap<Key, ShadowPose>,
    billboards: HashMap<usize, Billboard>,
    fade: Option<Overlay>,
}

impl Frame {
    fn clear(&mut self) {
        self.view = None;
        self.joints.clear();
        self.shadows.clear();
        self.billboards.clear();
        self.fade = None;
    }
}

/// The two-tick presentation cache.
#[derive(Debug, Default)]
pub struct Tween {
    /// `None` disables tweening: nothing is recorded or changed.
    alpha: Option<f32>,
    /// Whether the previous frame is usable for this draw's view.
    blend: bool,
    prev: Frame,
    cur: Frame,
}

impl Tween {
    /// Set the draw's position between the previous and the current tick
    /// (`None` turns tweening off and drops the cache).
    pub fn set_alpha(&mut self, alpha: Option<f32>) {
        if alpha.is_none() && self.alpha.is_some() {
            self.reset();
        }
        self.alpha = alpha.map(|alpha| alpha.clamp(0.0, 1.0));
    }

    /// Whether tweening is on.
    pub fn enabled(&self) -> bool {
        self.alpha.is_some()
    }

    /// A simulation tick ran: what was drawn becomes the previous tick.
    pub fn advance(&mut self) {
        if !self.enabled() {
            return;
        }
        std::mem::swap(&mut self.prev, &mut self.cur);
        self.cur.clear();
    }

    /// Drop both ticks, so the next draw snaps.
    pub fn reset(&mut self) {
        self.prev.clear();
        self.cur.clear();
    }

    /// Start a scene draw in `room` through camera cut `cut`. A cut or room
    /// change since the previous tick snaps every value of this draw.
    pub fn begin(&mut self, room: RoomId, cut: usize) {
        if !self.enabled() {
            return;
        }
        let view = Some((room, cut));
        if self.cur.view != view {
            self.cur.clear();
            self.cur.view = view;
        }
        self.blend = self.prev.view == view;
    }

    /// The blend weight for this draw, when it blends at all.
    fn weight(&self) -> Option<f32> {
        self.alpha.filter(|alpha| self.blend && *alpha < 1.0)
    }

    /// Record `joints` (exact) for `key` and replace them with the pose
    /// between the previous tick's and these.
    pub fn joints(&mut self, key: Key, joints: &mut [Mat4x3]) {
        if !self.enabled() {
            return;
        }
        self.cur.joints.insert(key, joints.to_vec());
        let Some(alpha) = self.weight() else {
            return;
        };
        let Some(prev) = self.prev.joints.get(&key) else {
            return;
        };
        if prev.len() != joints.len()
            || prev
                .iter()
                .zip(joints.iter())
                .any(|(a, b)| discontinuous(a, b))
        {
            return;
        }
        for (joint, prev) in joints.iter_mut().zip(prev) {
            *joint = lerp_matrix(prev, joint, alpha);
        }
    }

    /// Record a shadow's exact placement for `key` and tween it.
    pub fn shadow(&mut self, key: Key, pos: &mut [i32; 3], angle: &mut u16) {
        if !self.enabled() {
            return;
        }
        let exact = ShadowPose {
            pos: *pos,
            angle: *angle,
        };
        self.cur.shadows.insert(key, exact);
        let Some(alpha) = self.weight() else {
            return;
        };
        let Some(prev) = self.prev.shadows.get(&key) else {
            return;
        };
        if distance_sq(prev.pos, exact.pos) > TELEPORT_DISTANCE * TELEPORT_DISTANCE {
            return;
        }
        *pos = std::array::from_fn(|axis| lerp_i32(prev.pos[axis], exact.pos[axis], alpha));
        *angle = lerp_angle(prev.angle, exact.angle, alpha);
    }

    /// Record effect pool slot `slot`'s billboard rectangle and tween it.
    /// `spawn` identifies the effect occupying the slot (its type and spawn
    /// position), so a slot freed and reused within a tick snaps. A billboard
    /// tweens only while it keeps its spawn and its sprite frame's pivot and
    /// moves less than [`BILLBOARD_JUMP`] pixels.
    pub fn billboard(
        &mut self,
        slot: usize,
        spawn: [i32; 4],
        uv: [u8; 4],
        pos: &mut [i32; 2],
        size: &mut [i32; 2],
    ) {
        if !self.enabled() {
            return;
        }
        let exact = Billboard {
            spawn,
            uv,
            pos: *pos,
            size: *size,
        };
        self.cur.billboards.insert(slot, exact);
        let Some(alpha) = self.weight() else {
            return;
        };
        let Some(prev) = self.prev.billboards.get(&slot) else {
            return;
        };
        if prev.spawn != exact.spawn
            || prev.uv[2..] != exact.uv[2..]
            || (0..2).any(|axis| (prev.pos[axis] - exact.pos[axis]).abs() > BILLBOARD_JUMP)
        {
            return;
        }
        for axis in 0..2 {
            pos[axis] = lerp_i32(prev.pos[axis], exact.pos[axis], alpha);
            size[axis] = lerp_i32(prev.size[axis], exact.size[axis], alpha);
        }
    }

    /// Record the room-entry fade's exact overlay and return the one to
    /// draw: its alpha tweens while both ticks show the same colour.
    pub fn fade(&mut self, overlay: Option<Overlay>) -> Option<Overlay> {
        if !self.enabled() {
            return overlay;
        }
        self.cur.fade = overlay;
        let (Some(alpha), Some(prev), Some(mut exact)) = (self.weight(), self.prev.fade, overlay)
        else {
            return overlay;
        };
        if prev.color != exact.color {
            return overlay;
        }
        exact.alpha = lerp_i32(i32::from(prev.alpha), i32::from(exact.alpha), alpha) as u8;
        Some(exact)
    }
}

/// Whether two ticks of one joint are too far apart to tween.
fn discontinuous(a: &Mat4x3, b: &Mat4x3) -> bool {
    if distance_sq(a.t, b.t) > TELEPORT_DISTANCE * TELEPORT_DISTANCE {
        return true;
    }
    (0..3).any(|column| {
        let (ca, la) = column_of(a, column);
        let (cb, lb) = column_of(b, column);
        if la == 0.0 || lb == 0.0 {
            return la != lb;
        }
        let dot = (0..3).map(|row| ca[row] * cb[row]).sum::<f32>() / (la * lb);
        dot < MIN_AXIS_COS
    })
}

/// A rotation column and its length.
fn column_of(m: &Mat4x3, column: usize) -> ([f32; 3], f32) {
    let c = [
        m.r[0][column] as f32,
        m.r[1][column] as f32,
        m.r[2][column] as f32,
    ];
    let len = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt();
    (c, len)
}

/// Blend two joint matrices: the translation linearly and each rotation
/// column linearly, then rescaled to the blended column length so a turning
/// joint keeps its size (and any joint scale) through the blend.
fn lerp_matrix(a: &Mat4x3, b: &Mat4x3, alpha: f32) -> Mat4x3 {
    let mut out = *b;
    for axis in 0..3 {
        out.t[axis] = lerp_i32(a.t[axis], b.t[axis], alpha);
    }
    for column in 0..3 {
        let (ca, la) = column_of(a, column);
        let (cb, lb) = column_of(b, column);
        let mixed: [f32; 3] = std::array::from_fn(|row| ca[row] + (cb[row] - ca[row]) * alpha);
        let len = (mixed[0] * mixed[0] + mixed[1] * mixed[1] + mixed[2] * mixed[2]).sqrt();
        let target = la + (lb - la) * alpha;
        let scale = if len > 0.0 { target / len } else { 1.0 };
        for (row, value) in mixed.iter().enumerate() {
            out.r[row][column] = (value * scale).round() as i32;
        }
    }
    out
}

fn lerp_i32(a: i32, b: i32, alpha: f32) -> i32 {
    (a as f32 + (b as f32 - a as f32) * alpha).round() as i32
}

/// Blend two 12-bit angles the short way round.
fn lerp_angle(a: u16, b: u16, alpha: f32) -> u16 {
    let delta = ((i32::from(b) - i32::from(a) + 0x800).rem_euclid(0x1000)) - 0x800;
    (i32::from(a) + (delta as f32 * alpha).round() as i32).rem_euclid(0x1000) as u16
}

fn distance_sq(a: [i32; 3], b: [i32; 3]) -> i64 {
    (0..3)
        .map(|axis| {
            let d = i64::from(a[axis]) - i64::from(b[axis]);
            d * d
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anim::entity_matrix;
    use crate::transition::FadeColor;

    fn room() -> RoomId {
        RoomId::from_room_and_player("106", 0).unwrap()
    }

    /// Draw `joints` for `key` on one tick and return what was drawn.
    fn draw(tween: &mut Tween, cut: usize, joints: &[Mat4x3]) -> Vec<Mat4x3> {
        let mut drawn = joints.to_vec();
        tween.begin(room(), cut);
        tween.joints(Key::Player, &mut drawn);
        drawn
    }

    #[test]
    fn disabled_records_nothing_and_changes_nothing() {
        let mut tween = Tween::default();
        let a = [entity_matrix([0, 0, 0], 0)];
        let b = [entity_matrix([100, 0, 0], 0)];
        assert_eq!(draw(&mut tween, 0, &a), a);
        tween.advance();
        assert_eq!(draw(&mut tween, 0, &b), b);
        assert!(tween.cur.joints.is_empty());
    }

    #[test]
    fn a_draw_between_two_ticks_blends_and_alpha_one_is_exact() {
        let mut tween = Tween::default();
        tween.set_alpha(Some(0.5));
        let a = [entity_matrix([0, 0, 0], 0)];
        let b = [entity_matrix([100, 0, -40], 0x100)];
        assert_eq!(draw(&mut tween, 0, &a), a, "no previous tick snaps");
        tween.advance();
        let half = draw(&mut tween, 0, &b);
        assert_eq!(half[0].t, [50, 0, -20]);
        let mid = entity_matrix([0, 0, 0], 0x80);
        for row in 0..3 {
            for column in 0..3 {
                assert!((half[0].r[row][column] - mid.r[row][column]).abs() <= 4);
            }
        }
        tween.set_alpha(Some(1.0));
        assert_eq!(draw(&mut tween, 0, &b), b);
    }

    #[test]
    fn cuts_teleports_pose_snaps_and_missed_ticks_snap() {
        let mut tween = Tween::default();
        tween.set_alpha(Some(0.5));
        let a = [entity_matrix([0, 0, 0], 0)];
        draw(&mut tween, 0, &a);
        tween.advance();
        let moved = [entity_matrix([10, 0, 0], 0)];
        assert_eq!(draw(&mut tween, 1, &moved), moved, "camera cut");

        tween.advance();
        let far = [entity_matrix([5000, 0, 0], 0)];
        assert_eq!(draw(&mut tween, 1, &far), far, "teleport");

        tween.advance();
        let turned = [entity_matrix([5000, 0, 0], 0x800)];
        assert_eq!(draw(&mut tween, 1, &turned), turned, "pose snap");

        tween.advance();
        tween.advance();
        let next = [entity_matrix([5010, 0, 0], 0x800)];
        assert_eq!(draw(&mut tween, 1, &next), next, "a tick without a draw");
    }

    #[test]
    fn shadows_billboards_and_fades_tween() {
        let mut tween = Tween::default();
        tween.set_alpha(Some(0.25));
        tween.begin(room(), 0);
        let (mut pos, mut angle) = ([0, 0, 0], 0xFF0);
        tween.shadow(Key::Player, &mut pos, &mut angle);
        let (mut quad, mut size) = ([10, 10], [8, 8]);
        tween.billboard(3, [1; 4], [0, 0, 0x80, 0x80], &mut quad, &mut size);
        let fade = |alpha| {
            Some(Overlay {
                alpha,
                color: FadeColor::Black,
            })
        };
        assert_eq!(tween.fade(fade(200)), fade(200));
        tween.advance();
        tween.begin(room(), 0);
        let (mut pos, mut angle) = ([40, 0, 0], 0x010);
        tween.shadow(Key::Player, &mut pos, &mut angle);
        assert_eq!((pos, angle), ([10, 0, 0], 0xFF8));
        let (mut quad, mut size) = ([30, 10], [8, 16]);
        tween.billboard(3, [1; 4], [4, 0, 0x80, 0x80], &mut quad, &mut size);
        assert_eq!((quad, size), ([15, 10], [8, 10]));
        assert_eq!(tween.fade(fade(100)), fade(175));
    }

    #[test]
    fn billboards_snap_on_a_jump_a_new_pivot_or_a_reused_slot() {
        let mut tween = Tween::default();
        tween.set_alpha(Some(0.5));
        let mut billboard = |spawn, uv, x| {
            tween.begin(room(), 0);
            let (mut pos, mut size) = ([x, 0], [8, 8]);
            tween.billboard(0, spawn, uv, &mut pos, &mut size);
            tween.advance();
            pos[0]
        };
        let uv = [0, 0, 0x80, 0x80];
        assert_eq!(billboard([1; 4], uv, 0), 0);
        assert_eq!(billboard([1; 4], uv, 10), 5, "tweened");
        assert_eq!(
            billboard([1; 4], uv, 10 + BILLBOARD_JUMP + 1),
            10 + BILLBOARD_JUMP + 1,
            "jump"
        );
        assert_eq!(billboard([1; 4], [0, 0, 0x70, 0x80], 80), 80, "pivot");
        assert_eq!(billboard([2; 4], [0, 0, 0x70, 0x80], 90), 90, "reused slot");
        assert_eq!(billboard([2; 4], [8, 0, 0x70, 0x80], 100), 95, "next frame");
    }

    #[test]
    fn a_fade_colour_change_snaps() {
        let mut tween = Tween::default();
        tween.set_alpha(Some(0.5));
        let overlay = |alpha, color| Some(Overlay { alpha, color });
        tween.begin(room(), 0);
        tween.fade(overlay(200, FadeColor::Black));
        tween.advance();
        tween.begin(room(), 0);
        let white = overlay(100, FadeColor::White);
        assert_eq!(tween.fade(white), white);
    }

    #[test]
    fn turning_tweening_off_drops_the_cache() {
        let mut tween = Tween::default();
        tween.set_alpha(Some(0.5));
        let a = [entity_matrix([0, 0, 0], 0)];
        draw(&mut tween, 0, &a);
        tween.advance();
        tween.set_alpha(None);
        let b = [entity_matrix([100, 0, 0], 0)];
        assert_eq!(draw(&mut tween, 0, &b), b, "disabled");
        // Back on, the stale tick from before is gone: the first draw snaps.
        tween.set_alpha(Some(0.5));
        tween.advance();
        assert_eq!(draw(&mut tween, 0, &b), b, "re-enabled");
    }
}
