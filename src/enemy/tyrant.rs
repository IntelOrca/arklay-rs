//! The Tyrant's render-only state machines.
//!
//! Three pieces of the Tyrant live outside its entity record: the slash
//! ribbon's nine-slot history, the two claw-ghost joint copies with their
//! oscillating scale pair, and the five severed limbs the rocket death
//! launches as free bodies. The original keeps them in file-scope scratch
//! (`0x004ba250..0x004ba27f`) and mutates them from the entity's update; this
//! module is the port's typed home for that state, with the arithmetic spelled
//! out at the original's widths.
//!
//! - [`Trail`] is the ribbon history: nine slots of a matrix pair, their
//!   element-wise midpoint and a near/far blade pair, plus the timer word
//!   whose `0x8000` bit arms the sweep and whose low bits count the tail down.
//!   The final rasterisation has no consumer in the port (the original's D3D
//!   strip path; see the ribbon notes in `docs/modding.md`); every value the
//!   state machine produces is exact and observable.
//! - [`Ghost`] is the two claw copies: one unscaled, one scaled by a pair of
//!   counters that oscillate between 3000 and 6000 by a signed-byte step.
//! - [`Limb`] is one severed joint: a free body with X/Y/Z velocity, gravity,
//!   a bounce budget and a tumble rotation.

use crate::anim::{Mat4x3, compose, rotation_matrix};

/// Ribbon history slots (the original's 9 x 0x100-byte pool).
pub const TRAIL_SLOTS: usize = 9;

/// Severed limbs the rocket death launches.
pub const LIMB_COUNT: usize = 5;

/// The joint each launched limb starts from, in launch order.
pub const LIMB_JOINTS: [usize; LIMB_COUNT] = [2, 4, 7, 10, 12];

/// The mesh object index of the Tyrant model's exposed heart (the extra
/// object beyond its fifteen joints).
pub const HEART_OBJECT: usize = 15;

/// The mesh object index of the claw (joint 8), drawn for the two ghosts.
pub const CLAW_OBJECT: usize = 8;

/// The claw-ghost oscillation bounds and start values.
pub const GHOST_SCALE_MIN: i16 = 3000;
pub const GHOST_SCALE_MAX: i16 = 6000;

/// The 22-entry heart wobble ramp, read by the exposed heart's own counter as
/// it runs 21 -> 0 and reloads.
pub const HEART_BEAT: [i8; 22] = [
    -122, -114, -98, -74, 0, 82, 72, 52, 0, -52, -72, -82, 0, 74, 98, 114, 122, 102, 62, 0, -62,
    -102,
];

/// One ribbon history slot: the matrix pair, their element-wise midpoint and
/// the near/far blade endpoints (the original's `+0x90`/`+0xb0`/`+0xd0`/
/// `+0xf0`/`+0xf8`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrailSlot {
    /// Blade A (the previous frame's claw world matrix at store time).
    pub mat_a: Mat4x3,
    /// Blade B (the freshly computed matrix).
    pub mat_b: Mat4x3,
    /// `mid`: per-element `(b - a) / 2 + a`, truncating toward zero.
    pub mid: Mat4x3,
    /// Near endpoint in blade B's local frame.
    pub near: [i16; 3],
    /// Far endpoint in blade B's local frame.
    pub far: [i16; 3],
}

/// The ribbon's persistent state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trail {
    /// The nine history slots.
    pub slots: [TrailSlot; TRAIL_SLOTS],
    /// `0x004ba264`: bit `0x8000` arms the sweep, the low bits count the tail
    /// down.
    pub timer: u16,
    /// `0x004ba268`: the number of live history segments.
    pub segments: i32,
    /// `0x004ba270`: the near blade endpoint (0, -300, 0).
    pub near: [i16; 3],
    /// `0x004ba278`: the far blade endpoint (0, 1500, 0), swept by the arm.
    pub far: [i16; 3],
    /// `DAT_008fc41c`: the segment tint byte (0x70 red).
    pub tint: u8,
    /// The alloc ran (the arm and update paths are gated on this in the port;
    /// the original gates on the pool pointer).
    pub allocated: bool,
}

impl Default for Trail {
    fn default() -> Self {
        Trail {
            slots: [TrailSlot::default(); TRAIL_SLOTS],
            timer: 0,
            segments: 8,
            near: [0, -300, 0],
            far: [0, 1500, 0],
            tint: 0x70,
            allocated: false,
        }
    }
}

/// The two claw-ghost copies and their oscillation counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ghost {
    /// `0x004ba254`: the X/Z scale counter, a signed 16-bit word.
    pub scale_a: i16,
    /// `0x004ba258`: the Y scale counter, a signed 16-bit word.
    pub scale_b: i16,
    /// `0x004ba25c`: the per-frame step, a signed byte (0xC8 = -56).
    pub step: i8,
    /// The copy matrices: `[0]` scaled, `[1]` unscaled (the original's
    /// `block + i * 0x7c + 0x44`, copy index 0 first).
    pub matrices: [Mat4x3; 2],
    /// Both copies were refreshed at least once (the draw gate).
    pub live: bool,
}

impl Default for Ghost {
    fn default() -> Self {
        Ghost {
            scale_a: 3000,
            scale_b: 300,
            step: -56,
            matrices: [Mat4x3::default(); 2],
            live: false,
        }
    }
}

/// One severed limb: a free joint body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Limb {
    /// The joint's world matrix, mutated directly by the physics.
    pub world: Mat4x3,
    /// The tumble rotation (`joint + 4`), a signed rotation triple.
    pub rotation: [i16; 3],
    /// The X velocity (`joint + 0x72`).
    pub vel_x: i16,
    /// The Y velocity (`joint + 0x70`), the one gravity accumulates into.
    pub vel_y: i16,
    /// The Z velocity (`joint + 0x74`).
    pub vel_z: i16,
    /// The per-frame gravity step (`joint + 0x76`).
    pub gravity: i16,
    /// The remaining bounces (`joint + 0x78`); zero stops the physics.
    pub bounces: u16,
    /// The limb was launched (the port's live gate for free-body rendering).
    pub live: bool,
}

/// The Tyrant scratch block.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TyrantState {
    /// The slash ribbon.
    pub trail: Trail,
    /// The two claw ghosts.
    pub ghost: Ghost,
    /// The five free limbs, in launch order (joints 2, 4, 7, 10 and 12).
    pub limbs: [Limb; LIMB_COUNT],
}

/// `0x0048aec0`: reserve the ribbon pool and store the segment tint. The
/// original carves `count * 0x100` bytes out of the room-data buffer; the
/// port's pool is the fixed nine slots, so only the tint and the armed state
/// are recorded.
pub fn trail_alloc(state: &mut TyrantState, tint: u8) {
    state.trail.tint = tint;
    state.trail.allocated = true;
}

/// The element-wise midpoint matrix: every rotation short and translation
/// long is `(b - a) / 2 + a`, truncating toward zero.
pub fn trail_midpoint(a: &Mat4x3, b: &Mat4x3) -> Mat4x3 {
    let mut out = Mat4x3::default();
    for (row, (a_row, b_row)) in out.r.iter_mut().zip(a.r.iter().zip(&b.r)) {
        for (column, (a, b)) in row.iter_mut().zip(a_row.iter().zip(b_row)) {
            *column = (b - a) / 2 + a;
        }
    }
    for (value, (a, b)) in out.t.iter_mut().zip(a.t.iter().zip(&b.t)) {
        *value = (b - a) / 2 + a;
    }
    out
}

/// `ApplyMatrixSV` plus the translation, kept at full 32-bit width: the
/// original's 16-bit corner truncation wrapped a world point across the
/// screen, so the port keeps the int sum (the same substitution the shadow
/// projection uses).
fn trail_xform(matrix: &Mat4x3, v: [i16; 3]) -> [i32; 3] {
    let vx = i32::from(v[0]);
    let vy = -i32::from(v[1]);
    let vz = i32::from(v[2]);
    let mut out = [0i32; 3];
    for (row, value) in matrix.r.iter().zip(out.iter_mut()) {
        let sum = row[0] * vx + row[1] * vy + row[2] * vz;
        *value = (sum + ((sum >> 31) & 0xFFF)) >> 12;
    }
    out[1] = -out[1];
    for (value, translation) in out.iter_mut().zip(matrix.t) {
        *value = value.wrapping_add(translation);
    }
    out
}

/// The store branch of the ribbon push: one history entry, the matrix pair,
/// their midpoint and the blade endpoints. `slot` is not masked - slot 8 is
/// the real degenerate terminator.
pub fn trail_store(
    state: &mut TyrantState,
    a: &Mat4x3,
    b: &Mat4x3,
    near: [i16; 3],
    far: [i16; 3],
    slot: u8,
) {
    if !state.trail.allocated || usize::from(slot) >= TRAIL_SLOTS {
        return;
    }
    let entry = &mut state.trail.slots[usize::from(slot)];
    entry.mat_a = *a;
    entry.mat_b = *b;
    entry.mid = trail_midpoint(a, b);
    entry.near = near;
    entry.far = far;
}

/// The draw branch of the ribbon push: scroll the three matrix blocks up one
/// slot (near/far deliberately do not scroll), write the fresh pair into slot
/// zero, and walk the history newest-first computing each segment's band and
/// flare blade corners. The corners are computed for determinism; the port
/// has no rasterisation consumer (see the module comment).
pub fn trail_scroll(state: &mut TyrantState, a: &Mat4x3, b: &Mat4x3) -> Vec<[[i32; 3]; 4]> {
    let mut corners = Vec::new();
    if !state.trail.allocated {
        return corners;
    }
    let mut count = state.trail.segments;
    if count - 1 > 7 {
        count = 8;
    }
    if count <= 0 {
        return corners;
    }

    for disp in (0..count).rev() {
        let source = state.trail.slots[disp as usize];
        let destination = &mut state.trail.slots[disp as usize + 1];
        destination.mat_a = source.mat_a;
        destination.mat_b = source.mat_b;
        destination.mid = source.mid;
    }
    {
        let entry = &mut state.trail.slots[0];
        entry.mat_a = *a;
        entry.mat_b = *b;
        entry.mid = trail_midpoint(a, b);
        // near/far are deliberately not rewritten here, matching the original.
    }

    let sh = state.trail.slots[count as usize];
    let mut hi_near = trail_xform(&sh.mat_b, sh.near);
    let mut hi_far = trail_xform(&sh.mat_b, sh.far);

    for disp in (0..count).rev() {
        let sd = state.trail.slots[disp as usize];
        let mut jn = sd.near;
        jn[1] = jn[1].wrapping_add(0x32);
        let mut jf = sd.far;
        jf[1] = jf[1].wrapping_add(100);
        let lo_near = trail_xform(&sd.mid, jn);
        let lo_far = trail_xform(&sd.mid, jf);
        corners.push([lo_near, lo_far, hi_far, hi_near]);

        let cur_near = trail_xform(&sd.mat_b, sd.near);
        let cur_far = trail_xform(&sd.mat_b, sd.far);
        corners.push([lo_near, lo_far, cur_far, cur_near]);

        hi_near = cur_near;
        hi_far = cur_far;
    }
    corners
}

/// The plain push entry point: `count == 0` stores one entry, otherwise the
/// history scrolls and the segment corners are computed.
pub fn trail_push(
    state: &mut TyrantState,
    a: &Mat4x3,
    b: &Mat4x3,
    near: [i16; 3],
    far: [i16; 3],
    slot: u8,
    count: i32,
) -> Vec<[[i32; 3]; 4]> {
    if count == 0 {
        trail_store(state, a, b, near, far, slot);
        Vec::new()
    } else {
        trail_scroll(state, a, b)
    }
}

/// The arm sweep: seed the whole history from the current claw pose, sweeping
/// the far endpoint through its `+100` steps (with the `+1000` swell when the
/// claw is fully grown) and ending on the degenerate slot-8 terminator where
/// near equals far. Clears the `0x8000` arm bit.
pub fn trail_arm(state: &mut TyrantState, claw: &Mat4x3) {
    if !state.trail.allocated {
        return;
    }
    state.trail.segments = 8;
    let mut slot = 7u8;
    if state.ghost.scale_b > 8000 {
        state.trail.far[1] = state.trail.far[1].wrapping_add(1000);
    }
    state.trail.far[1] = state.trail.far[1].wrapping_sub(800);
    loop {
        state.trail.far[1] = state.trail.far[1].wrapping_add(100);
        let near = state.trail.near;
        let far = state.trail.far;
        trail_store(state, claw, claw, near, far, slot);
        if slot == 0 {
            break;
        }
        slot -= 1;
    }
    let near = state.trail.near;
    trail_store(state, claw, claw, near, near, 8);
    state.trail.timer &= 0x7fff;
    if state.ghost.scale_b > 8000 {
        state.trail.far[1] = state.trail.far[1].wrapping_sub(1000);
    }
}

/// `tyrant_trail_update`: push this frame's segment through the draw branch,
/// then - inside the message gate - retire the oldest segment once the timer
/// has counted below the live count and tick the timer down.
pub fn trail_update(
    state: &mut TyrantState,
    claw: &Mat4x3,
    scratch: &Mat4x3,
    gated: bool,
) -> Vec<[[i32; 3]; 4]> {
    let near = state.trail.near;
    let far = state.trail.far;
    let segments = state.trail.segments;
    let corners = trail_push(state, claw, scratch, near, far, 0, segments);
    if gated {
        if i32::from(state.trail.timer) < state.trail.segments {
            state.trail.segments -= 1;
        }
        state.trail.timer = state.trail.timer.wrapping_sub(1);
    }
    corners
}

/// `tyrant_draw_claw_ghosts`' state half: refresh both copy matrices and the
/// oscillating scale pair. The unscaled copy is this frame's matrix; the
/// scaled copy applies `[scale_a, scale_b, scale_a]` after both counters have
/// stepped. The step flips once `scale_a` leaves the 3000..=6000 band.
pub fn ghost_tick(ghost: &mut Ghost, scratch: &Mat4x3, gated: bool) {
    if !gated {
        return;
    }
    let mut scaled = *scratch;
    ghost.scale_a = ghost.scale_a.wrapping_add(i16::from(ghost.step));
    ghost.scale_b = ghost.scale_b.wrapping_add(i16::from(ghost.step));
    crate::enemy::custom_anim::scale_columns_xyz(
        &mut scaled,
        [
            i32::from(ghost.scale_a),
            i32::from(ghost.scale_b),
            i32::from(ghost.scale_a),
        ],
    );
    ghost.matrices[0] = scaled;
    ghost.matrices[1] = *scratch;
    if ghost.scale_a > GHOST_SCALE_MAX || ghost.scale_a < GHOST_SCALE_MIN {
        ghost.step = ghost.step.wrapping_neg();
    }
    ghost.live = true;
}

/// `tyrant_limb_launch`: flag the limb, store its velocity (the `.y` word
/// first, then `.x` and `.z`), the gravity step, the bounce budget and the
/// tumble rotation.
pub fn limb_launch(
    limb: &mut Limb,
    world: Mat4x3,
    vel: [i16; 3],
    tumble: [i16; 3],
    gravity: i16,
    bounces: u16,
) {
    limb.world = world;
    limb.vel_y = vel[1];
    limb.vel_x = vel[0];
    limb.vel_z = vel[2];
    limb.gravity = gravity;
    limb.bounces = bounces;
    limb.rotation = tumble;
    limb.live = true;
}

/// `tyrant_limb_update`: one frame of the free body. The world X/Z advance by
/// their velocities, gravity accumulates into the Y velocity, the world Y
/// integrates that velocity, and crossing the `-200` floor bounces (budget
/// down, half the velocity back inverted). The tumble rotation premultiplies
/// the joint's world rotation.
pub fn limb_update(limb: &mut Limb) {
    if limb.bounces == 0 {
        return;
    }
    limb.world.t[0] = limb.world.t[0].wrapping_add(i32::from(limb.vel_x));
    let vy = limb.gravity.wrapping_add(limb.vel_y);
    limb.vel_y = vy;
    limb.world.t[2] = limb.world.t[2].wrapping_add(i32::from(limb.vel_z));
    let y = limb.world.t[1].wrapping_add(i32::from(vy));
    limb.world.t[1] = y;
    if y > -200 {
        limb.world.t[1] = y.wrapping_sub(i32::from(vy));
        limb.bounces = limb.bounces.wrapping_sub(1);
        limb.vel_y = vy.wrapping_div(2).wrapping_neg();
    }
    let rotated = compose(
        &Mat4x3 {
            r: rotation_matrix(
                i32::from(limb.rotation[0]),
                i32::from(limb.rotation[1]),
                i32::from(limb.rotation[2]),
            ),
            t: [0; 3],
        },
        &limb.world,
    );
    limb.world.r = rotated.r;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Mat4x3 {
        Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [0, 0, 0],
        }
    }

    #[test]
    fn alloc_records_the_tint_and_arms_the_pool() {
        let mut state = TyrantState::default();
        assert!(!state.trail.allocated);
        trail_alloc(&mut state, 0x70);
        assert!(state.trail.allocated);
        assert_eq!(state.trail.tint, 0x70);
    }

    #[test]
    fn the_midpoint_halves_the_span_toward_zero() {
        let a = Mat4x3 {
            r: [[0, 0, 0], [0, 0, 0], [0, 0, 0]],
            t: [0, 0, 0],
        };
        let b = Mat4x3 {
            r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
            t: [100, -101, 7],
        };
        let mid = trail_midpoint(&a, &b);
        assert_eq!(mid.r[0][0], 2048);
        assert_eq!(mid.r[2][2], 2048);
        // -101 / 2 = -50 (toward zero), + 0.
        assert_eq!(mid.t, [50, -50, 3]);
    }

    #[test]
    fn store_then_scroll_keeps_the_history_order() {
        let mut state = TyrantState::default();
        trail_alloc(&mut state, 0x70);
        for slot in (0..=8u8).rev() {
            let mut a = identity();
            a.t[0] = i32::from(slot) * 100;
            trail_store(&mut state, &a, &a, [0, -300, 0], [0, 1500, 0], slot);
        }
        // Slot 0 held the last store (slot 8 was written first); scroll a
        // fresh pair in and every slot must shift one up.
        let mut a = identity();
        a.t[0] = 999;
        trail_scroll(&mut state, &a, &a);
        assert_eq!(state.trail.slots[0].mat_a.t[0], 999);
        assert_eq!(state.trail.slots[1].mat_a.t[0], 0, "slot 0 moved to 1");
        assert_eq!(state.trail.slots[8].mat_a.t[0], 700, "slot 7 moved to 8");
        // near/far do not scroll: slot 1 kept its own.
        assert_eq!(state.trail.slots[1].near, [0, -300, 0]);
    }

    #[test]
    fn the_arm_sweep_seeds_eight_slots_and_the_terminator() {
        let mut state = TyrantState::default();
        trail_alloc(&mut state, 0x70);
        let claw = identity();
        trail_arm(&mut state, &claw);
        // far.y starts 1500, drops 800, then +100 per push: 800, 900, ...,
        // and the terminator stores near as both endpoints.
        assert_eq!(state.trail.slots[0].far[1], 1500 - 800 + 800);
        assert_eq!(state.trail.slots[7].far[1], 1500 - 800 + 100);
        assert_eq!(state.trail.slots[7].near[1], -300);
        assert_eq!(state.trail.slots[8].far, state.trail.slots[8].near);
        assert_eq!(state.trail.slots[8].near, [0, -300, 0]);
        assert_eq!(state.trail.timer & 0x8000, 0);
        assert_eq!(state.trail.segments, 8);
    }

    #[test]
    fn the_arm_sweep_swells_the_far_blade_when_the_claw_is_grown() {
        let mut state = TyrantState::default();
        trail_alloc(&mut state, 0x70);
        state.ghost.scale_b = 9000;
        trail_arm(&mut state, &identity());
        // +1000, -800, then +100 x8: 1800 at the first push and 2500 at the
        // last; the +1000 is removed after the sweep.
        assert_eq!(state.trail.slots[7].far[1], 1800);
        assert_eq!(state.trail.slots[0].far[1], 2500);
        assert_eq!(state.trail.far[1], 1500);
    }

    #[test]
    fn the_timer_counts_down_only_inside_the_gate() {
        let mut state = TyrantState::default();
        trail_alloc(&mut state, 0x70);
        state.trail.timer = 0x8010;
        state.trail.segments = 8;
        let corners = trail_update(&mut state, &identity(), &identity(), true);
        assert_eq!(corners.len(), 16, "eight segments, two quads each");
        assert_eq!(state.trail.timer, 0x800F);
        assert_eq!(state.trail.segments, 8, "timer >= segments: no retire");
        state.trail.timer = 0x0003;
        trail_update(&mut state, &identity(), &identity(), true);
        assert_eq!(state.trail.segments, 7);
        assert_eq!(state.trail.timer, 2);
        trail_update(&mut state, &identity(), &identity(), false);
        assert_eq!(state.trail.timer, 2, "paused frames do not count down");
    }

    #[test]
    fn ghost_scales_step_down_first_and_flip_at_the_bounds() {
        // Start at the top bound: the first step takes it back inside.
        let mut ghost = Ghost {
            scale_a: 6000,
            scale_b: 3000,
            ..Ghost::default()
        };
        ghost_tick(&mut ghost, &identity(), true);
        assert_eq!(ghost.scale_a, 6000 - 56);
        assert_eq!(ghost.scale_b, 3000 - 56);
        assert_eq!(ghost.step, -56);
        assert_eq!(ghost.matrices[1], identity());
        assert_ne!(ghost.matrices[0], identity());

        // Run to the bottom bound and back out: the step flips exactly once.
        ghost.scale_a = 3004;
        ghost.step = -56;
        ghost_tick(&mut ghost, &identity(), true);
        assert_eq!(ghost.scale_a, 2948);
        assert_eq!(ghost.step, 56);
    }

    #[test]
    fn ghost_ticks_are_gated() {
        let mut ghost = Ghost::default();
        ghost_tick(&mut ghost, &identity(), false);
        assert_eq!(ghost.scale_a, 3000);
        assert!(!ghost.live);
    }

    #[test]
    fn limb_launch_stores_the_rotated_velocity_words() {
        let mut limb = Limb::default();
        limb_launch(
            &mut limb,
            identity(),
            [10, -400, 0x19],
            [0x40, 0x60, 0],
            0x0F,
            3,
        );
        assert_eq!(limb.vel_x, 10);
        assert_eq!(limb.vel_y, -400);
        assert_eq!(limb.vel_z, 0x19);
        assert_eq!(limb.gravity, 0x0F);
        assert_eq!(limb.bounces, 3);
        assert_eq!(limb.rotation, [0x40, 0x60, 0]);
        assert!(limb.live);
    }

    #[test]
    fn limb_update_integrates_and_bounces_on_the_floor() {
        let mut limb = Limb::default();
        let mut world = identity();
        world.t = [0, -100, 0];
        limb_launch(&mut limb, world, [0, -50, 0], [0, 0, 0], 15, 3);
        // vy = 15 - 50 = -35; y = -135, above the -200 floor, so it bounces:
        // y returns to -100, the budget drops and vy becomes 17 (trunc of
        // -35/2 negated).
        limb_update(&mut limb);
        assert_eq!(limb.world.t[1], -100);
        assert_eq!(limb.bounces, 2);
        assert_eq!(limb.vel_y, 17);
        assert!(limb.live);
        // No rotation tumble: the world rotation is the saturated identity.
        assert_eq!(limb.world.r[0][0], 4095);
        assert_eq!(limb.world.r[1][1], 4095);
    }

    #[test]
    fn limb_update_falls_through_the_floor_gate() {
        let mut limb = Limb::default();
        let mut world = identity();
        world.t = [0, -195, 0];
        limb_launch(&mut limb, world, [0, -100, 0], [0, 0, 0], 15, 3);
        // vy = -85; y = -280, below -200: no bounce, the body keeps falling.
        limb_update(&mut limb);
        assert_eq!(limb.world.t[1], -280);
        assert_eq!(limb.bounces, 3);
        assert_eq!(limb.vel_y, -85);
    }

    #[test]
    fn limb_update_stops_when_the_bounce_budget_is_spent() {
        let mut limb = Limb::default();
        limb_launch(&mut limb, identity(), [5, 0, 0], [0, 0, 0], 0, 0);
        limb.world.t = [0; 3];
        limb_update(&mut limb);
        assert_eq!(limb.world.t, [0; 3]);
    }

    #[test]
    fn limb_update_tumbles_the_rotation() {
        let mut limb = Limb::default();
        limb_launch(&mut limb, identity(), [0, 0, 0], [0, 0x400, 0], 0, 3);
        limb_update(&mut limb);
        assert_ne!(limb.world.r, identity().r);
        assert_eq!(limb.world.t, [0; 3], "no velocity, no translation");
    }
}
