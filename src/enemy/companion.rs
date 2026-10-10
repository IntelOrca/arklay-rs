//! The Plant 42 companion arena.
//!
//! Plant 42 owns two *copied* entities carved out of the room-data buffer in
//! the original: the flower body and the root ball. They are not enemy slots,
//! so the entity list never ticks or draws them; the plant's own update does,
//! through its copy pass. This module is the port's equivalent: a fixed
//! arena of [`Companion`] records outside the 30-slot enemy list, allocated
//! by the plant's init, ticked by the plant's script and drawn by the
//! companion render pass.
//!
//! # Machines
//!
//! The two companions run the original's small state machines here rather
//! than in Lua, because their scratch lives at raw byte offsets the port's
//! [`Entity`] does not model (`+0x6C`/`+0x6E` sway words, `+0x70` vine pool,
//! `+0x7E`, the three 32-bit scale factors). The body has idle/shrivel/fall;
//! the roots idle/pulse/rise. The machines keep the original's exact draw
//! order, because the platform random stream is shared with the plant.
//!
//! # Rendering
//!
//! The plant model carries two mesh objects beyond its joint skeleton: object
//! 16 (the flower body, a broad leaf cluster) and object 17 (the root ball).
//! The clones are the original's per-object animation slots for those two
//! objects, so the render pass draws exactly one object per companion at the
//! companion's transform with the body's three scale factors (or the root
//! ball's single pulse scale) applied to the rotation columns.

use crate::effects::Attach;
use crate::game::{Entity, EntitySound, GameState};
use crate::state::RoomState;

/// The companion arena capacity. The original's room-data buffer fits far
/// more, but Plant 42 only ever needs two and the later heart rider one.
pub const COMPANION_CAP: usize = 4;

/// The 90-entry breathing ramp the flower body and the root ball read
/// backwards as `table[0x59 - counter]`.
const PULSE: [u8; 90] = [
    0, 1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 66, 78, 91, 105, 120, 136, 153, 172, 190, 205, 217,
    226, 232, 237, 241, 249, 252, 254, 255, 254, 253, 252, 251, 249, 247, 245, 243, 240, 237, 234,
    231, 227, 223, 219, 215, 210, 205, 200, 195, 189, 183, 177, 171, 164, 157, 150, 143, 135, 127,
    119, 111, 113, 106, 99, 92, 86, 80, 74, 68, 63, 58, 53, 48, 44, 40, 36, 32, 29, 26, 23, 20, 18,
    16, 14, 12, 10, 8, 6, 4,
];

/// Which Plant 42 companion a slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionKind {
    /// The flower body: mesh object 16, the pulsing/shrivelling/falling core.
    Body,
    /// The root ball: mesh object 17, the pulsing/rising root.
    Roots,
    /// The Tyrant's exposed heart: mesh object 15, riding joint 1 with its own
    /// beat and wobble scale.
    Heart,
}

/// One companion clone: a full entity copy plus the raw scratch words the
/// original's machine keeps at byte offsets the port's [`Entity`] does not
/// map. The clone's rotation, translation, scale, tint and shadow live on the
/// copied entity; the scratch below is the machine's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Companion {
    /// The copied entity record. Its `pos`/`angle`/`pitch`/`roll`/
    /// `joint_scale`/`model_tint`/shadow fields drive the render.
    pub entity: Entity,
    /// Which companion this is.
    pub kind: CompanionKind,
    /// Entity slot of the plant that spawned it.
    pub owner: u8,
    /// Whether the machine ticked it this frame; the render pass draws only
    /// live companions.
    pub live: bool,
    /// The companion's state byte (offset 0x84).
    pub state: u8,
    /// The companion's action-state byte (offset 0x87).
    pub action_state: u8,
    /// The companion's health word (offset 0x88). The body goes negative
    /// when the plant dies; the roots never read theirs.
    pub health: i16,
    /// The companion's behaviour byte (offset 0x02).
    pub behavior_flags: u8,
    /// The companion's hit latch (offset 0x8A). The body's latch routes the
    /// plant's damage reaction.
    pub hit_state: u8,
    /// The companion's blend counter (offset 0x8C).
    pub blend_counter: u8,
    /// The companion's 16-bit timer (offset 0xC4).
    pub timer: i16,
    /// The companion's signed 16-bit scale/fall step (offset 0x78).
    pub step: i16,
    /// The flower body's vine pool (offset 0x70); the plant's real health
    /// pool.
    pub vine_pool: i16,
    /// The flower body's animation scratch word (offset 0x7E).
    pub flag_7e: i16,
    /// The sway oscillators: `+0x6C` (position X), `+0x6E` (position Y),
    /// `+0x7A` (speed Y) and `+0x7C` (speed Z), all signed words.
    pub osc: [i16; 4],
    /// The flower body's three 32-bit scale factors (offsets 0x16C/0x170/
    /// 0x174), applied to the local matrix's columns.
    pub scale: [i32; 3],
    /// The root ball's 16-bit scale at offset 0xCA.
    pub joint_scale: u16,
    /// The rider's explicit world matrix (the Tyrant heart's composed
    /// `anchor * local`): the renderer draws it in place of the entity
    /// transform. `None` for the plant companions.
    pub matrix: Option<crate::anim::Mat4x3>,
    /// The heart's wobble scale word at offset 0x6C (`0x1000` at spawn,
    /// stepped by the beat table).
    pub wobble: i16,
    /// The heart's beat counter at offset 0xC4, running 21 -> 0 and reloading.
    pub beat: i16,
    /// The heart drop's velocity at offset 0x70.
    pub vel: i16,
    /// The heart drop's tumble, accumulating into the rotation Z at offset
    /// 0x76.
    pub tumble_z: i16,
    /// The heart's own local translation (`+0x34`), the position words the
    /// drop integrates and the render composes under the joint anchor.
    pub local_t: [i32; 3],
}

impl Companion {
    /// The mesh object index this companion draws (the plant model's two
    /// extra objects beyond its joint skeleton, or the Tyrant's heart object).
    pub fn mesh_object(&self) -> usize {
        match self.kind {
            CompanionKind::Body => 16,
            CompanionKind::Roots => 17,
            CompanionKind::Heart => crate::enemy::tyrant::HEART_OBJECT,
        }
    }

    /// The scale applied to the local matrix's X/Y/Z columns each render: the
    /// body's three authored factors, the root ball's single pulse scale
    /// (`table[0x59 - timer] + eu16(0xCA)`), or the identity for the heart,
    /// whose scale is already folded into its composed matrix.
    pub fn render_scale(&self) -> [i32; 3] {
        match self.kind {
            CompanionKind::Body => self.scale,
            CompanionKind::Roots => {
                let scale = i32::from(pulse(self.timer)) + i32::from(self.joint_scale);
                [scale, scale, scale]
            }
            CompanionKind::Heart => [0x1000; 3],
        }
    }
}

/// The breathing ramp read at `counter`, clamped to the table's ends.
fn pulse(counter: i16) -> u8 {
    let index = (0x59 - i32::from(counter)).clamp(0, 89) as usize;
    PULSE[index]
}

/// Queue the companion's enemy-bank cue at its own position (the clone kept
/// the plant's spawn-record variant, which carries the sound group).
fn play_companion_sound(game: &mut GameState, room: &RoomState, companion: &Companion, id: u8) {
    let group = (companion.entity.variant >> 4) & 0x7;
    if let Some((name, column)) = crate::sfx::enemy_sound(room, id, group) {
        game.entity_sounds.push(EntitySound {
            name,
            bank: 2,
            column,
            pos: companion.entity.pos,
        });
    }
}

/// Queue a 3D cue at the companion's position.
fn play_companion_3d(game: &mut GameState, room: &RoomState, companion: &Companion, id: u8) {
    let character = game.id.player_flag;
    if let Some((name, bank, column)) = crate::sfx::play_3d_cue(room, character, 2, id) {
        game.entity_sounds.push(EntitySound {
            name,
            bank,
            column,
            pos: companion.entity.pos,
        });
    }
}

/// Spawn the two Plant 42 companions for `slot`: the flower body and the root
/// ball, initialised exactly like the original's two `clone_entity` calls plus
/// the field setup that follows each. Consumes the two clone draws from the
/// platform stream (body first, then roots). Returns their handles.
///
/// `flags` is the plant's `behavior_flags`: `0x0F == 8` tints the poison body,
/// `== 4` parks the room-40C0 fallen core, `== 5` is the poison arena (the
/// plant's own scale, handled by the script).
pub fn plant42_spawn(game: &mut GameState, slot: usize, flags: u8) -> Option<(u16, u16)> {
    let free: Vec<usize> = game
        .companions
        .iter()
        .enumerate()
        .filter_map(|(index, companion)| companion.is_none().then_some(index))
        .collect();
    if free.len() < 2 {
        eprintln!("[companion] arena full: Plant 42 body/roots not spawned");
        return None;
    }
    let parent = game.entities[slot];
    let (body_index, roots_index) = (free[0], free[1]);

    // ---- flower body ----
    let mut body = Companion {
        entity: parent,
        kind: CompanionKind::Body,
        owner: slot as u8,
        live: false,
        state: 0,
        action_state: 0,
        health: parent.health,
        behavior_flags: 0,
        hit_state: 0,
        blend_counter: 7,
        timer: 0x59,
        step: 0,
        vine_pool: 4,
        flag_7e: 0,
        osc: [0, 1, 0, 1],
        scale: [0x1000; 3],
        joint_scale: 0x1000,
        matrix: None,
        wobble: 0,
        beat: 0,
        vel: 0,
        tumble_z: 0,
        local_t: [0; 3],
    };
    // The clone fans by `remaining * 0x100` with one copy, so +0x100.
    body.entity.angle = parent.angle.wrapping_add(0x100);
    body.entity.shadow_offset = [0, 0, 0];
    body.entity.shadow_half_x = 3000;
    body.entity.shadow_half_z = 3000;
    body.entity.shadow_tint = 0x0060_6060;
    body.entity.joint_scale = 0x1000;
    body.entity.pitch = 0;
    body.entity.roll = 0;
    body.entity.model_tint = [0; 3];
    // The clone's `action_state = (rand & 3) == 0` draw happens even though
    // the body's state word is cleared right after; keep the draw's position
    // in the stream.
    let _body_draw = crate::game::platform_rand(&mut game.rand_state) & 3;
    body.action_state = 0;
    if flags & 0x0F == 8 {
        tint_companion(&mut body, -1, -2, 0, 0, 0x200);
        body.vine_pool = 0;
    }
    if flags == 4 {
        // Room 40C0's fallen-core pose: the body is dead and parked in
        // state 2 / action_state 3, which is a no-op branch of the fall.
        body.health = -1;
        body.state = 2;
        body.action_state = 3;
        body.scale = [0x19DC, 0x0818, 0x19DC];
        body.blend_counter = 1;
    }

    // ---- root ball ----
    let mut roots = Companion {
        entity: parent,
        kind: CompanionKind::Roots,
        owner: slot as u8,
        live: false,
        state: 0,
        action_state: 0,
        health: parent.health,
        behavior_flags: 0,
        hit_state: 0,
        blend_counter: 0,
        timer: 0x59,
        step: 0,
        vine_pool: 0,
        flag_7e: 0,
        osc: [0, 1, 0, 1],
        scale: [0x1000; 3],
        joint_scale: 0x1000,
        matrix: None,
        wobble: 0,
        beat: 0,
        vel: 0,
        tumble_z: 0,
        local_t: [0; 3],
    };
    roots.entity.angle = 0;
    roots.entity.pitch = 0;
    roots.entity.roll = 0;
    roots.entity.joint_scale = 0x1000;
    roots.entity.shadow_half_x = 0;
    roots.entity.shadow_half_z = 0;
    roots.entity.model_tint = [0; 3];
    // The roots' clone draw is kept: a set bit starts them pulsing.
    roots.action_state = u8::from(crate::game::platform_rand(&mut game.rand_state) & 3 == 0);
    if flags == 4 {
        roots.joint_scale = 1000;
        roots.entity.pos[1] = -1300;
        roots.state = 2;
        roots.action_state = 2;
    }

    game.companions[body_index] = Some(body);
    game.companions[roots_index] = Some(roots);
    game.entities[slot].plant42_body = Some(body_index as u16);
    game.entities[slot].plant42_roots = Some(roots_index as u16);
    game.plant42_shared_body = Some(body_index as u16);
    Some((body_index as u16, roots_index as u16))
}

/// Queue the companion's model tint (`scd_model_tint_apply`'s enemy path) on
/// the companion's own entity, without touching the plant's other slots.
fn tint_companion(companion: &mut Companion, r: i16, g: i16, b: i16, word_a: u16, word_b: u16) {
    companion.entity.queue_model_tint(r, g, b, word_a, word_b);
    companion.entity.add_model_tint(r, g, b);
}

/// `BillboardAdjSize` on the companion's ground quad.
fn adjust_shadow(companion: &mut Companion, half_x: i16, half_z: i16) {
    companion.entity.shadow_half_x = companion.entity.shadow_half_x.wrapping_add(half_x);
    companion.entity.shadow_half_z = companion.entity.shadow_half_z.wrapping_add(half_z);
}

/// The dead-move anchor's X (identity in the port; see the effect helpers).
const DEAD_X: i32 = 0;

/// The flower body's idle: pulse the scale, cue the breathing sound on the
/// `0x41` frame, integrate the two sway oscillators and count the timer down.
fn body_idle(game: &mut GameState, room: &RoomState, companion: &mut Companion) {
    let scale = i32::from(pulse(companion.timer)) * 3 + 0x1000;
    companion.scale = [scale, scale, scale];
    if companion.timer == 0x41 {
        play_companion_sound(game, room, companion, 0);
    }
    let t = companion.timer;
    companion.timer = t.wrapping_sub(1);
    if t == 0 {
        companion.timer = 0x59;
    }
    companion.entity.angle = companion.entity.angle.wrapping_add(companion.osc[2] as u16);
    companion.entity.roll = companion.entity.roll.wrapping_add(companion.osc[0] as u16);
    companion.osc[2] = companion.osc[2].wrapping_sub(companion.osc[3]);
    companion.osc[0] = companion.osc[0].wrapping_sub(companion.osc[1]);
    if 0x10u8 < ((companion.osc[2] as i8).wrapping_add(8)) as u8 {
        companion.osc[3] = companion.osc[3].wrapping_neg();
    }
    if 0x0Eu8 < ((companion.osc[0] as i8).wrapping_add(7)) as u8 {
        companion.osc[1] = companion.osc[1].wrapping_neg();
    }
}

/// The flower body's shrivel (state 1) and regrow (state 3/4): drive the
/// scale by the oscillating step, run the tint queue down and spray the
/// shrivel billboards.
fn body_shrivel(game: &mut GameState, companion: &mut Companion, handle: u16) {
    let mut st = companion.action_state;
    if st == 0 {
        companion.step = 0x80;
        tint_companion(companion, -1, -1, 0, 0, 0x200);
        companion.blend_counter = 0x0F;
        companion.action_state = 1;
        st = 1;
    }

    if st == 1 {
        let d = i32::from(companion.step) + 0x18;
        for scale in &mut companion.scale {
            *scale -= d;
        }
        companion.step = companion.step.wrapping_neg();
        let before = companion.blend_counter;
        companion.blend_counter = (before as i8).wrapping_sub(1) as u8;
        if before == 0 {
            tint_companion(companion, 0, -1, 0, 0, 0x200);
        }
        adjust_shadow(companion, -16, -16);
        if companion.behavior_flags & 0x40 == 0 && companion.blend_counter.is_multiple_of(0x1E) {
            spawn_companion_effect(game, handle, 0, 0x1B, [DEAD_X, 3000, 0], 0, 0);
        }
    } else if st == 3 || st == 4 {
        if st == 3 {
            companion.step = -0x80;
            companion.action_state = 4;
        }
        let d = i32::from(companion.step) + 0x18;
        for scale in &mut companion.scale {
            *scale += d;
        }
        companion.step = companion.step.wrapping_neg();
        companion.blend_counter = companion.blend_counter.wrapping_sub(1);
        adjust_shadow(companion, 0x10, 0x10);
        if companion.behavior_flags & 0x40 == 0 && companion.blend_counter.is_multiple_of(0x1E) {
            spawn_companion_effect(game, handle, 0, 0x1B, [DEAD_X, 3000, 0], 0, 0);
        }
    }

    if crate::game::platform_rand(&mut game.rand_state) & 0x1F == 0 {
        let x = i32::from(crate::game::platform_rand(&mut game.rand_state) & 0x1FF);
        let z = i32::from(crate::game::platform_rand(&mut game.rand_state) & 0x1FF);
        spawn_companion_effect(game, handle, 0, 0x1C, [x, 2000, z], 0, 0);
    }
}

/// The flower body's fall (state 2): drop and bounce the core, then settle on
/// the `0x3C` step with the impact billboards and the four-cue sound burst.
fn body_fall(game: &mut GameState, room: &RoomState, companion: &mut Companion, handle: u16) {
    let st = companion.action_state as i8;
    if st == 0 {
        companion.step = 0;
        companion.action_state = 1;
        companion.blend_counter = 0x78;
    } else if st != 1 {
        if st != 2 {
            return;
        }
        if companion.blend_counter == 0 {
            return;
        }
        // The three draws flip the scale factors' signs in the original's
        // order: 0x170, then 0x16C, then 0x174.
        let flip_1 = (crate::game::platform_rand(&mut game.rand_state) & 1) as i32 * -2 + 1;
        companion.scale[1] += flip_1 * i32::from(companion.step);
        let flip_0 = (crate::game::platform_rand(&mut game.rand_state) & 1) as i32 * -2 + 1;
        companion.scale[0] += flip_0 * i32::from(companion.step);
        let flip_2 = (crate::game::platform_rand(&mut game.rand_state) & 1) as i32 * -2 + 1;
        companion.scale[2] += flip_2 * i32::from(companion.step);
        companion.step = companion.step.wrapping_neg();
        if companion.step < 0 {
            companion.step += 1;
        }
        companion.blend_counter = companion.blend_counter.wrapping_sub(1);
        return;
    }

    companion.entity.pos[1] += i32::from(companion.step) * 5;
    companion.step += 1;
    if companion.step < 6 && companion.step & 1 != 0 {
        tint_companion(companion, -1, -1, 0, 0, 0x200);
    }

    if 199 < companion.entity.pos[1] {
        companion.step -= 1;
        companion.entity.pos[1] += i32::from(companion.step) * -5;
        companion.action_state = 2;
        companion.blend_counter = companion.blend_counter.wrapping_sub(1);
        companion.scale[1] -= 0x914;
        companion.scale[0] += 0x9DC;
        companion.scale[2] += 0x9DC;
        if companion.behavior_flags & 0x40 == 0 {
            spawn_companion_effect(game, handle, 9, 0x11, [600, -200, 600], 0, 0x28);
            spawn_companion_effect(game, handle, 9, 0x11, [-600, -200, 600], 0x400, 0x28);
            spawn_companion_effect(game, handle, 9, 0x11, [-600, -200, -600], 0x200, 0x28);
            spawn_companion_effect(game, handle, 9, 0x11, [600, -200, -600], 0x800, 0x28);
            spawn_companion_effect(game, handle, 0, 0x1B, [0, -600, 0], 0, 0x3C);
        }
        play_companion_3d(game, room, companion, 8);
        play_companion_3d(game, room, companion, 0);
        play_companion_3d(game, room, companion, 0);
        play_companion_3d(game, room, companion, 0);
        companion.step = 0x3C;
    }
}

/// The root ball's idle: count the pulse timer down.
fn roots_idle(companion: &mut Companion) {
    let t = companion.timer;
    companion.timer = t.wrapping_sub(1);
    if t == 0 {
        companion.timer = 0x59;
    }
}

/// The root ball's pulse: drive `+0xCA` down and back up by the oscillating
/// step.
fn roots_pulse(companion: &mut Companion) {
    let mut st = companion.action_state;
    if st == 0 {
        companion.step = 0x10;
        companion.action_state = 1;
        st = 1;
    }
    if st == 1 {
        companion.joint_scale = companion
            .joint_scale
            .wrapping_sub(companion.step.wrapping_add(0x30) as u16);
        companion.step = companion.step.wrapping_neg();
        return;
    }
    if st == 3 {
        companion.action_state = 4;
        companion.step = -0x10;
        st = 4;
    }
    if st == 4 {
        companion.joint_scale = companion
            .joint_scale
            .wrapping_add(companion.step.wrapping_add(0x30) as u16);
        companion.step = companion.step.wrapping_neg();
    }
}

/// The root ball's rise: climb to the `-0x513` ceiling and shrink the scale
/// toward 999.
fn roots_rise(companion: &mut Companion) {
    if companion.action_state == 0 {
        companion.step = 0;
        companion.action_state = 1;
    } else if companion.action_state != 1 {
        return;
    }

    companion.entity.pos[1] += i32::from(companion.step) * 4;
    companion.step += 1;
    if -0x513 < companion.entity.pos[1] {
        companion.step -= 1;
        companion.entity.pos[1] += i32::from(companion.step) * -4;
    }
    if 999 < companion.joint_scale {
        companion.joint_scale = companion.joint_scale.wrapping_sub(0x38);
    }
}

/// Spawn a billboard attached to the companion's own matrix with a local
/// offset, the original's `Effect_CreateBillboard(..., &ENTITY->matrix, ...)`.
fn spawn_companion_effect(
    game: &mut GameState,
    handle: u16,
    effect_type: u8,
    depth: u8,
    offset: [i32; 3],
    yaw: i16,
    light: u8,
) {
    let room_effects = std::rc::Rc::clone(&game.room_effects);
    crate::effects::create_attached(
        game,
        &room_effects,
        effect_type,
        depth,
        Attach::Companion(handle),
        offset,
        yaw,
        light,
    );
}

/// One tick of the plant's two companions, in the original's order: the body
/// machine, then the root ball machine, both inside the monster message gate.
/// Marks both live for the render pass. Does nothing when the plant has no
/// companions (a split-vine record).
pub fn plant42_tick(game: &mut GameState, room: &RoomState, slot: usize) {
    let Some(body_handle) = game.entities[slot].plant42_body else {
        return;
    };
    let Some(roots_handle) = game.entities[slot].plant42_roots else {
        return;
    };
    let paused = game.message_freezes_monsters();

    for handle in [body_handle, roots_handle] {
        let index = usize::from(handle);
        let Some(mut companion) = game.companions.get_mut(index).and_then(Option::take) else {
            continue;
        };
        companion.live = true;
        if !paused {
            match companion.kind {
                CompanionKind::Body => match companion.state {
                    0 => body_idle(game, room, &mut companion),
                    1 | 3 => body_shrivel(game, &mut companion, handle),
                    2 => body_fall(game, room, &mut companion, handle),
                    _ => {}
                },
                CompanionKind::Roots => match companion.state {
                    0 => roots_idle(&mut companion),
                    1 => roots_pulse(&mut companion),
                    2 => roots_rise(&mut companion),
                    _ => {}
                },
                // The heart rider ticks through `heart_tick`, not the plant.
                CompanionKind::Heart => {}
            }
        }
        if let Some(entry) = game.companions.get_mut(index) {
            *entry = Some(companion);
        }
    }
}

/// The body companion handle the plant in `slot` reads: its own clone, or the
/// room-shared body for a split-vine record (the original's `scd_target_ptr`
/// distribution).
pub fn resolve_body(game: &GameState, slot: usize) -> Option<u16> {
    let entity = game.entities.get(slot)?;
    if entity.behavior_flags & 1 == 0 {
        entity.plant42_body
    } else {
        game.plant42_shared_body
    }
}

/// The root-ball companion handle the plant in `slot` reads: its own clone, or
/// the boss's clone for a split-vine record (the roots pointer is never
/// distributed in the original; a split-vine record reaches them through the
/// shared body's owner).
pub fn resolve_roots(game: &GameState, slot: usize) -> Option<u16> {
    let entity = game.entities.get(slot)?;
    if entity.behavior_flags & 1 == 0 {
        return entity.plant42_roots;
    }
    let owner = game
        .companions
        .get(usize::from(game.plant42_shared_body?))
        .and_then(Option::as_ref)?
        .owner;
    game.entities.get(usize::from(owner))?.plant42_roots
}

/// The body's hit latch, or `None` when there is no live body.
pub fn body_hit_state(game: &GameState, slot: usize) -> Option<u8> {
    let handle = resolve_body(game, slot)?;
    game.companions
        .get(usize::from(handle))
        .and_then(Option::as_ref)
        .map(|companion| companion.hit_state)
}

/// The body's vine pool, or `None` when there is no live body.
pub fn body_vines(game: &GameState, slot: usize) -> Option<i16> {
    let handle = resolve_body(game, slot)?;
    game.companions
        .get(usize::from(handle))
        .and_then(Option::as_ref)
        .map(|companion| companion.vine_pool)
}

/// The body's health, or `None` when there is no live body.
pub fn body_health(game: &GameState, slot: usize) -> Option<i16> {
    let handle = resolve_body(game, slot)?;
    game.companions
        .get(usize::from(handle))
        .and_then(Option::as_ref)
        .map(|companion| companion.health)
}

/// The body's world height, or `None` when there is no live body.
pub fn body_y(game: &GameState, slot: usize) -> Option<i32> {
    let handle = resolve_body(game, slot)?;
    game.companions
        .get(usize::from(handle))
        .and_then(Option::as_ref)
        .map(|companion| companion.entity.pos[1])
}

/// Store the body's vine pool.
pub fn set_body_vines(game: &mut GameState, slot: usize, value: i16) {
    if let Some(handle) = resolve_body(game, slot)
        && let Some(companion) = game
            .companions
            .get_mut(usize::from(handle))
            .and_then(Option::as_mut)
    {
        companion.vine_pool = value;
    }
}

/// A companion's world position by handle.
pub fn companion_pos(game: &GameState, handle: u16) -> Option<[i32; 3]> {
    game.companions
        .get(usize::from(handle))
        .and_then(Option::as_ref)
        .map(|companion| companion.entity.pos)
}

/// Latch the body's damage reaction: `hit_state = 1`, `behavior_flags = 1`.
pub fn body_react(game: &mut GameState, slot: usize) {
    if let Some(handle) = resolve_body(game, slot)
        && let Some(companion) = game
            .companions
            .get_mut(usize::from(handle))
            .and_then(Option::as_mut)
    {
        companion.hit_state = 1;
        companion.behavior_flags = 1;
    }
}

/// Clear the body's damage reaction (`hit_state = 0`, `behavior_flags = 0`).
pub fn body_react_clear(game: &mut GameState, slot: usize) {
    if let Some(handle) = resolve_body(game, slot)
        && let Some(companion) = game
            .companions
            .get_mut(usize::from(handle))
            .and_then(Option::as_mut)
    {
        companion.hit_state = 0;
        companion.behavior_flags = 0;
    }
}

/// The withering sequence's companion writes, one call per plant sub-state:
/// 0 arms the shrivel, 2 parks the fallen core, 4 regrows and 6 restores the
/// companions to idle.
pub fn body_wither(game: &mut GameState, slot: usize, sub: u8) {
    let Some(body_handle) = resolve_body(game, slot) else {
        return;
    };
    let Some(roots_handle) = resolve_roots(game, slot) else {
        return;
    };
    if let Some(body) = game
        .companions
        .get_mut(usize::from(body_handle))
        .and_then(Option::as_mut)
    {
        match sub {
            0 => {
                body.flag_7e = 7;
                body.state = 1;
                body.action_state = 0;
            }
            2 => {
                body.flag_7e = 4;
                body.action_state = 2;
            }
            4 => {
                body.flag_7e = 0x0E;
                body.action_state = 3;
            }
            6 => {
                body.flag_7e = 0;
                body.state = 0;
                body.action_state = 0;
            }
            _ => {}
        }
    }
    if let Some(roots) = game
        .companions
        .get_mut(usize::from(roots_handle))
        .and_then(Option::as_mut)
    {
        match sub {
            0 | 6 => {
                roots.state = u8::from(sub == 0);
                roots.action_state = 0;
            }
            2 => roots.action_state = 2,
            4 => roots.action_state = 3,
            _ => {}
        }
    }
}

/// The plant's first death frame: the body dies and parks on its fall state,
/// the roots park with it.
pub fn body_kill(game: &mut GameState, slot: usize) {
    let Some(body_handle) = resolve_body(game, slot) else {
        return;
    };
    let roots_handle = resolve_roots(game, slot);
    if let Some(body) = game
        .companions
        .get_mut(usize::from(body_handle))
        .and_then(Option::as_mut)
    {
        body.health = -1;
        body.state = 2;
        body.action_state = 0;
    }
    if let Some(handle) = roots_handle
        && let Some(roots) = game
            .companions
            .get_mut(usize::from(handle))
            .and_then(Option::as_mut)
    {
        roots.state = 2;
        roots.action_state = 0;
    }
}

// ===========================================================================
// The Tyrant's exposed heart rider.
//
// A full entity copy parked at entity `+0x170` in the original, riding joint
// 1's world matrix at a fixed local offset (0x138, -760, -287) with its own
// wobble scale and beat. The clone's own counter walks the 22-entry beat ramp
// and drives a pulsing scale; the rocket death drops it as a free body.
// ===========================================================================

/// Spawn the heart rider for the Tyrant in `slot` (the original's one-clone
/// call in its init). Consumes the clone's `action_state = (rand & 3) == 0`
/// draw from the platform stream. Returns the companion handle.
pub fn spawn_heart(game: &mut GameState, slot: usize) -> Option<u16> {
    let index = game.companions.iter().position(Option::is_none)?;
    let parent = game.entities[slot];
    let mut heart = Companion {
        entity: parent,
        kind: CompanionKind::Heart,
        owner: slot as u8,
        live: false,
        state: 0,
        action_state: u8::from(crate::game::platform_rand(&mut game.rand_state) & 3 == 0),
        health: parent.health,
        behavior_flags: 0,
        hit_state: 0,
        blend_counter: parent.blend_counter,
        timer: 0,
        step: 0,
        vine_pool: 0,
        flag_7e: 0,
        osc: [0; 4],
        scale: [0; 3],
        joint_scale: 0,
        matrix: None,
        wobble: 0x1000,
        beat: 0x15,
        vel: 0,
        tumble_z: 0,
        local_t: [0x138, -760, -287],
    };
    heart.entity.pitch = 0;
    heart.entity.angle = 0;
    heart.entity.roll = 0;
    heart.entity.shadow_half_x = 0;
    heart.entity.shadow_half_z = 0;
    heart.entity.pos = heart.local_t;
    game.companions[index] = Some(heart);
    game.entities[slot].tyrant_heart = Some(index as u16);
    Some(index as u16)
}

/// `tyrant_draw_heart`'s state half: run the gated beat/scale update, compose
/// the heart's local matrix under joint 1's world anchor and publish the
/// result for the render pass. Does nothing (and marks the heart not live)
/// while the Tyrant has not entered the current camera's switch zone, exactly
/// like the original's early return.
pub fn heart_tick(game: &mut GameState, slot: usize) {
    let Some(handle) = game.entities[slot].tyrant_heart else {
        return;
    };
    let index = usize::from(handle);
    let Some(mut heart) = game.companions.get_mut(index).and_then(Option::take) else {
        return;
    };
    let in_zone = game.entities[slot].has_enter_switch_zone & 0x7F != 0;
    heart.live = in_zone;
    if !in_zone {
        if let Some(entry) = game.companions.get_mut(index) {
            *entry = Some(heart);
        }
        return;
    }

    let alive = game.entities[slot].health >= 0;
    let mut local = crate::anim::Mat4x3 {
        r: crate::anim::rotation_matrix(0, 0, i32::from(heart.tumble_z)),
        t: heart.local_t,
    };
    if alive && !game.message_freezes_monsters() {
        let scale = i32::from(heart.wobble) + 500;
        crate::enemy::custom_anim::scale_columns_xyz(&mut local, [scale, scale, scale]);
        let beat = usize::from(heart.beat as u16);
        if let Some(&step) = crate::enemy::tyrant::HEART_BEAT.get(beat) {
            heart.wobble = heart.wobble.wrapping_add(i16::from(step).wrapping_mul(2));
        }
        let prev = heart.beat;
        heart.beat = prev.wrapping_sub(1);
        if prev == 0 {
            heart.beat = 0x15;
        }
    }

    let anchor = game.joint_worlds[slot].get(1).copied().unwrap_or_default();
    let world = crate::anim::compose(&anchor, &local);
    heart.matrix = Some(world);
    heart.entity.pos = world.t;
    if let Some(entry) = game.companions.get_mut(index) {
        *entry = Some(heart);
    }
}

/// The rocket death's heart drop: integrate the free-fall velocity while the
/// stored world height is below -400, tumble the rotation, and grow the
/// velocity. This runs even while the heart is hidden (the original's
/// hidden-body path still updates the fields).
pub fn heart_drop(game: &mut GameState, slot: usize) {
    let Some(handle) = game.entities[slot].tyrant_heart else {
        return;
    };
    let Some(heart) = game
        .companions
        .get_mut(usize::from(handle))
        .and_then(Option::as_mut)
    else {
        return;
    };
    if (heart.entity.pos[1] as i16 as i32) < -400 {
        heart.local_t[1] = heart.local_t[1].wrapping_add(i32::from(heart.vel));
        heart.tumble_z = heart.tumble_z.wrapping_add(0x18);
    }
    heart.vel = heart.vel.wrapping_add(0x0F);
}

/// The heart's stored world height (`+0x6E`, the composed matrix translation's
/// Y word), or `None` when the Tyrant has no heart.
pub fn heart_world_y(game: &GameState, slot: usize) -> Option<i32> {
    let handle = game.entities[slot].tyrant_heart?;
    game.companions
        .get(usize::from(handle))
        .and_then(Option::as_ref)
        .map(|heart| i32::from(heart.entity.pos[1] as i16))
}

/// Re-arm the heart drop velocity for the rocket death's setup frame (the
/// original's `-0x12C` store at `+0x70`).
pub fn heart_set_drop_speed(game: &mut GameState, slot: usize, velocity: i16) {
    let Some(handle) = game.entities[slot].tyrant_heart else {
        return;
    };
    if let Some(heart) = game
        .companions
        .get_mut(usize::from(handle))
        .and_then(Option::as_mut)
    {
        heart.vel = velocity;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GameState;

    fn plant_game(flags: u8) -> GameState {
        let mut game = GameState::default();
        let entity = &mut game.entities[1];
        entity.id = 0x08;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.behavior_flags = flags;
        entity.health = 140;
        entity.angle = 0x200;
        entity.pos = [1000, 0, 1000];
        entity.variant = 0x20;
        game
    }

    fn room() -> RoomState {
        // A room whose enemy-sound table resolves the breathing cue.
        RoomState {
            stage: 4,
            room: 0x0F,
            ..RoomState::default()
        }
    }

    #[test]
    fn spawn_draws_two_values_in_clone_order_and_initialises_the_pair() {
        let mut game = plant_game(0);
        game.rand_state = 5;
        let mut expected = 5u32;
        let body_draw = crate::game::platform_rand(&mut expected) & 3;
        let roots_draw = crate::game::platform_rand(&mut expected) & 3;
        let (body, roots) = plant42_spawn(&mut game, 1, 0).expect("the arena has room");
        assert_eq!(game.rand_state, expected, "one draw per clone");
        assert_eq!(game.plant42_shared_body, Some(body));
        assert_eq!(game.entities[1].plant42_body, Some(body));
        assert_eq!(game.entities[1].plant42_roots, Some(roots));

        let body_companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!(body_companion.kind, CompanionKind::Body);
        assert_eq!(body_companion.entity.angle, 0x300, "the clone fan");
        assert_eq!(body_companion.vine_pool, 4);
        assert_eq!(body_companion.scale, [0x1000; 3]);
        assert_eq!(body_companion.timer, 0x59);
        assert_eq!(body_companion.entity.shadow_half_x, 3000);
        assert_eq!(body_companion.entity.shadow_tint, 0x0060_6060);
        assert_eq!(
            body_companion.action_state, 0,
            "the body's state word clears its clone draw"
        );
        assert_eq!(
            roots_draw & 3 == 0,
            game.companions[usize::from(roots)]
                .as_ref()
                .unwrap()
                .action_state
                == 1
        );
        let _ = body_draw;
    }

    #[test]
    fn fallen_core_spawn_parks_both_companions() {
        let mut game = plant_game(4);
        let (body, roots) = plant42_spawn(&mut game, 1, 4).unwrap();
        let body_companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!(body_companion.health, -1);
        assert_eq!(body_companion.state, 2);
        assert_eq!(body_companion.action_state, 3, "the fall's dead end");
        assert_eq!(body_companion.scale, [0x19DC, 0x0818, 0x19DC]);
        let roots_companion = game.companions[usize::from(roots)].as_ref().unwrap();
        assert_eq!(roots_companion.entity.pos[1], -1300);
        assert_eq!(roots_companion.state, 2);
        assert_eq!(roots_companion.action_state, 2, "the rise's dead end");
        assert_eq!(roots_companion.joint_scale, 1000);
    }

    #[test]
    fn poison_body_spawn_zeroes_the_vine_pool_and_tints() {
        let mut game = plant_game(8);
        let (body, _) = plant42_spawn(&mut game, 1, 8).unwrap();
        let body_companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!(body_companion.vine_pool, 0);
        assert_eq!(body_companion.entity.tint_queue, [-1, -2, 0]);
        assert!(body_companion.entity.tint_queue_armed);
    }

    #[test]
    fn body_idle_draws_the_breathing_ramp_and_cues_at_0x41() {
        let mut game = plant_game(0);
        let (body, _) = plant42_spawn(&mut game, 1, 0).unwrap();
        let room = room();
        {
            let companion = game.companions[usize::from(body)].as_mut().unwrap();
            companion.timer = 0x42;
            companion.osc = [0, 1, 0, 1];
        }
        plant42_tick(&mut game, &room, 1);
        let companion = game.companions[usize::from(body)].as_ref().unwrap();
        // pulse(0x42) = table[0x17] = 232; scale = 232*3 + 0x1000.
        assert_eq!(companion.scale, [232 * 3 + 0x1000; 3]);
        assert_eq!(companion.timer, 0x41, "counted down into the cue frame");
        assert!(companion.live);

        plant42_tick(&mut game, &room, 1);
        let companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!(companion.timer, 0x40);
        // The `0x41` frame queues the breathing cue through the room's
        // enemy-sound table; this room row names no column for the plant's
        // sound group, so the faithful result is no queued cue.
    }

    #[test]
    fn body_idle_integrates_the_two_oscillators_with_the_bounce() {
        let mut game = plant_game(0);
        let (body, _) = plant42_spawn(&mut game, 1, 0).unwrap();
        let room = room();
        {
            let companion = game.companions[usize::from(body)].as_mut().unwrap();
            companion.timer = 0x10;
            // speed Y = 0x0F, speed Z = 1: angle rises, then the bounce flips
            // the acceleration once `(char)0x0F + 8 > 0x10`.
            companion.osc = [0, 1, 0x0F, 1];
        }
        plant42_tick(&mut game, &room, 1);
        let companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!(companion.entity.angle, 0x300 + 0x0F);
        assert_eq!(companion.osc[2], 0x0E, "speed Y integrated by -Z");
        assert_eq!(companion.osc[3], -1, "the bounce flipped the acceleration");
        assert_eq!(companion.osc[0], -1, "position X integrated by -Y");
    }

    #[test]
    fn roots_pulse_drives_the_scale_word_both_ways() {
        let mut game = plant_game(0);
        let (_, roots) = plant42_spawn(&mut game, 1, 0).unwrap();
        let room = room();
        {
            let companion = game.companions[usize::from(roots)].as_mut().unwrap();
            companion.state = 1;
            companion.action_state = 0;
        }
        plant42_tick(&mut game, &room, 1);
        let companion = game.companions[usize::from(roots)].as_ref().unwrap();
        assert_eq!(companion.action_state, 1);
        assert_eq!(companion.step, -0x10);
        assert_eq!(companion.joint_scale, 0x1000u16.wrapping_sub(0x40));
        assert_eq!(
            companion.render_scale(),
            [0x1000 - 0x40, 0x1000 - 0x40, 0x1000 - 0x40]
        );
    }

    #[test]
    fn roots_rise_climbs_to_the_ceiling_and_stops() {
        let mut game = plant_game(0);
        let (_, roots) = plant42_spawn(&mut game, 1, 0).unwrap();
        let room = room();
        {
            let companion = game.companions[usize::from(roots)].as_mut().unwrap();
            companion.state = 2;
            companion.action_state = 0;
            companion.joint_scale = 0x1000;
            companion.entity.pos[1] = -0x520;
        }
        for _ in 0..64 {
            plant42_tick(&mut game, &room, 1);
        }
        let companion = game.companions[usize::from(roots)].as_ref().unwrap();
        assert_eq!(companion.entity.pos[1], -0x514, "it stalls on the ceiling");
        assert!(companion.joint_scale < 0x1000, "the rise shrinks the scale");
    }

    #[test]
    fn body_fall_bounces_and_settles_on_the_step() {
        let mut game = plant_game(0);
        let (body, _) = plant42_spawn(&mut game, 1, 0).unwrap();
        let room = room();
        {
            let companion = game.companions[usize::from(body)].as_mut().unwrap();
            companion.state = 2;
            companion.action_state = 0;
            companion.entity.pos[1] = 0;
        }
        for _ in 0..200 {
            plant42_tick(&mut game, &room, 1);
            if game.companions[usize::from(body)]
                .as_ref()
                .unwrap()
                .action_state
                == 2
            {
                break;
            }
        }
        let companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!(companion.action_state, 2, "the impact parked the fall");
        assert_eq!(companion.step, 0x3C);
        assert!(companion.entity.pos[1] > 0);
        assert_eq!(companion.scale[1], 0x1000 - 0x914);
        assert_eq!(companion.scale[0], 0x1000 + 0x9DC);
    }

    #[test]
    fn pause_freezes_the_machine_but_keeps_the_companions_live() {
        let mut game = plant_game(0);
        let (body, _) = plant42_spawn(&mut game, 1, 0).unwrap();
        game.message_flags &= !crate::game::MESSAGE_FLAG_MONSTERS;
        let room = room();
        {
            let companion = game.companions[usize::from(body)].as_mut().unwrap();
            companion.timer = 0x42;
        }
        plant42_tick(&mut game, &room, 1);
        let companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!(companion.timer, 0x42, "the machine is gated");
        assert!(companion.live, "the render still draws it");
    }

    #[test]
    fn react_and_wither_writes_land_on_the_shared_body() {
        let mut game = plant_game(0);
        let (body, roots) = plant42_spawn(&mut game, 1, 0).unwrap();
        body_react(&mut game, 1);
        assert_eq!(body_hit_state(&game, 1), Some(1));
        body_react_clear(&mut game, 1);
        assert_eq!(body_hit_state(&game, 1), Some(0));

        body_wither(&mut game, 1, 0);
        let companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!((companion.state, companion.flag_7e), (1, 7));
        let root = game.companions[usize::from(roots)].as_ref().unwrap();
        assert_eq!((root.state, root.action_state), (1, 0));

        body_wither(&mut game, 1, 2);
        let companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!((companion.action_state, companion.flag_7e), (2, 4));
        let root = game.companions[usize::from(roots)].as_ref().unwrap();
        assert_eq!(root.action_state, 2);

        body_kill(&mut game, 1);
        assert_eq!(body_health(&game, 1), Some(-1));
        let companion = game.companions[usize::from(body)].as_ref().unwrap();
        assert_eq!((companion.state, companion.action_state), (2, 0));
        let root = game.companions[usize::from(roots)].as_ref().unwrap();
        assert_eq!(root.state, 2);
    }

    #[test]
    fn a_split_vine_reads_the_shared_body() {
        let mut game = plant_game(0);
        let (body, _) = plant42_spawn(&mut game, 1, 0).unwrap();
        game.entities[2].id = 0x08;
        game.entities[2].set_active(true);
        game.entities[2].behavior_flags = 1;
        assert_eq!(resolve_body(&game, 2), Some(body));
        body_react(&mut game, 2);
        assert_eq!(body_hit_state(&game, 2), Some(1));
        assert_eq!(body_vines(&game, 2), Some(4));
    }

    #[test]
    fn enter_room_clears_the_arena_and_the_shared_body() {
        let mut game = plant_game(0);
        plant42_spawn(&mut game, 1, 0).unwrap();
        let id = crate::state::RoomId::parse("101").unwrap();
        game.enter_room(id, &room());
        assert!(game.companions.iter().all(Option::is_none));
        assert_eq!(game.plant42_shared_body, None);
    }

    // -----------------------------------------------------------------
    // The Tyrant heart rider
    // -----------------------------------------------------------------

    fn tyrant_game() -> GameState {
        let mut game = GameState::default();
        let entity = &mut game.entities[2];
        entity.id = 0x0C;
        entity.set_active(true);
        entity.status_flags = 1;
        entity.pos = [0, 0, 0];
        game
    }

    #[test]
    fn heart_spawn_consumes_the_clone_draw() {
        let mut game = tyrant_game();
        game.rand_state = 5;
        let mut expected = 5u32;
        let draw = crate::game::platform_rand(&mut expected) & 3;
        let handle = spawn_heart(&mut game, 2).expect("the arena has room");
        assert_eq!(game.rand_state, expected, "one clone draw");
        assert_eq!(game.entities[2].tyrant_heart, Some(handle));
        let heart = game.companions[usize::from(handle)].as_ref().unwrap();
        assert_eq!(heart.kind, CompanionKind::Heart);
        assert_eq!(heart.action_state, u8::from(draw == 0));
        assert_eq!(heart.wobble, 0x1000);
        assert_eq!(heart.beat, 0x15);
        assert_eq!(heart.local_t, [0x138, -760, -287]);
        assert_eq!(heart.mesh_object(), crate::enemy::tyrant::HEART_OBJECT);
        assert_eq!(heart.render_scale(), [0x1000; 3]);
    }

    #[test]
    fn heart_tick_composes_under_joint_one_and_beats() {
        let mut game = tyrant_game();
        let handle = spawn_heart(&mut game, 2).unwrap();
        game.entities[2].has_enter_switch_zone = 1;
        game.joint_worlds[2] = vec![
            crate::anim::Mat4x3 {
                r: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
                t: [0, 0, 0],
            };
            15
        ];
        game.joint_worlds[2][1].t = [100, -50, 200];
        heart_tick(&mut game, 2);
        let heart = game.companions[usize::from(handle)].as_ref().unwrap();
        // The local translation rotates through joint 1's identity and adds
        // to its translation.
        assert_eq!(heart.entity.pos, [100 + 0x138, -50 - 760, 200 - 287]);
        assert!(heart.live);
        // The beat stepped once from 0x15 and the wobble took the ramp's step.
        assert_eq!(heart.beat, 0x14);
        assert_eq!(
            heart.wobble,
            0x1000 + i16::from(crate::enemy::tyrant::HEART_BEAT[0x15]) * 2
        );
        // The first tick leaves the composed matrix at the joint anchor.
        assert_eq!(heart.matrix.unwrap().t, heart.entity.pos);
    }

    #[test]
    fn heart_tick_is_gated_by_the_owner_switch_zone() {
        let mut game = tyrant_game();
        let handle = spawn_heart(&mut game, 2).unwrap();
        heart_tick(&mut game, 2);
        let heart = game.companions[usize::from(handle)].as_ref().unwrap();
        assert!(!heart.live, "the owner has not entered the zone");
        assert_eq!(heart.beat, 0x15);
        assert_eq!(
            heart.entity.pos,
            [0x138, -760, -287],
            "the spawn translation"
        );
    }

    #[test]
    fn heart_drop_integrates_below_the_floor() {
        let mut game = tyrant_game();
        let handle = spawn_heart(&mut game, 2).unwrap();
        {
            let heart = game.companions[usize::from(handle)].as_mut().unwrap();
            heart.entity.pos[1] = -500;
            heart.vel = -300;
        }
        heart_drop(&mut game, 2);
        let heart = game.companions[usize::from(handle)].as_ref().unwrap();
        assert_eq!(heart.local_t[1], -760 - 300);
        assert_eq!(heart.tumble_z, 0x18);
        assert_eq!(heart.vel, -285);
        // The stored world height only refreshes on a heart_tick, so the
        // hidden body keeps integrating while the frozen height stays low.
        heart_drop(&mut game, 2);
        let heart = game.companions[usize::from(handle)].as_ref().unwrap();
        // The second drop adds the velocity the first one left (-285).
        assert_eq!(heart.local_t[1], -1060 - 285);
        assert_eq!(heart.tumble_z, 0x30);
        assert_eq!(heart.vel, -270);
    }
}
