//! The per-tick effect behaviour engine.
//!
//! [`update`] walks the 64 pool slots from 63 down to 0 while the message
//! freeze gate is open. Each slot runs its animation header's two behaviour
//! ids through one 64-entry dispatch table, integrates the velocity header
//! through the slot's yaw and attach transform, projects the world position
//! and steps the sprite animation.
//!
//! The table is ported in tiers. The ids the shipped scripts and every shipped
//! sprite's animation data can reach have real implementations; anything else
//! is an inert, counted placeholder ([`GameState::effect_placeholder_hits`],
//! [`implemented`]). The corpus audit in `tests/effects_real.rs` locks the
//! reachable set: no shipped script or sprite may dispatch a placeholder.

use std::rc::Rc;

use crate::anim;
use crate::effects::pool::{Attach, EFFECT_POOL_SIZE, Effect, create_attached};
use crate::effects::room::{EffectSprite, FrameEntry};
use crate::game::{BANK_SYSTEM, GameState, MESSAGE_FLAG_EFFECTS};
use crate::render::Camera;
use crate::state::RoomState;

/// The 4.12 identity rotation.
const IDENTITY_MATRIX: [i16; 9] = [4096, 0, 0, 0, 4096, 0, 0, 0, 4096];

/// Whether behaviour `id` has an implementation (and never counts as a
/// placeholder).
///
/// The set covers the script-spawned ids (`1,2,3,12,13,18,26,30,33,41,47,51,
/// 57,58`), their update partners (`0,12,15,19,26,38`), and the ids every
/// shipped sprite's animation data adds. The combat-only projectile, flame and
/// bullet/rocket entries (`10,37,43,45`) stay counted placeholders by design
/// until combat exists.
pub const fn implemented(id: u8) -> bool {
    matches!(
        id,
        0..=9 | 11..=20 | 21..=36 | 38..=42 | 44 | 46..=63
    )
}

/// One tick of every live effect slot.
pub fn update(game: &mut GameState, room: &RoomState) {
    let Some(cut) = room.cuts.get(room.current_cut) else {
        return;
    };
    let camera = Camera::from_cut(cut);
    let running = game.message_flags & MESSAGE_FLAG_EFFECTS != 0;
    for index in (0..EFFECT_POOL_SIZE).rev() {
        if game.effects.slot(index).is_none_or(|e| e.anim_id == 0) {
            continue;
        }
        step(game, room, &camera, cut.fov, index, running);
    }
}

/// One slot's full update-and-render step.
fn step(
    game: &mut GameState,
    room: &RoomState,
    camera: &Camera,
    fov: i32,
    index: usize,
    running: bool,
) {
    if running {
        let id = game.effects.slot(index).map_or(0, |e| e.anim_id);
        run_behavior(game, room, index, id);
    }

    integrate(game, camera, fov, index, running);

    if running {
        let id = game.effects.slot(index).map_or(0, |e| e.update_id);
        run_behavior(game, room, index, id);
    }

    if running {
        let animate = game.effects.slot(index).is_some_and(|e| e.flags() & 1 != 0);
        if animate {
            animate_sprite(game, index);
        }
    }
}

// ---------------------------------------------------------------------------
// Shared behaviour helpers
// ---------------------------------------------------------------------------

/// A header word at `offset` (relative to the animation header).
fn h16(effect: &Effect, offset: usize) -> i16 {
    i16::from_le_bytes([effect.header[offset], effect.header[offset + 1]])
}

/// Overwrite a header word at `offset`.
fn set_h16(effect: &mut Effect, offset: usize, value: i16) {
    effect.header[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

/// Copy one header word over another.
fn copy_h16(effect: &mut Effect, dst: usize, src: usize) {
    let value = h16(effect, src);
    set_h16(effect, dst, value);
}

/// Look the slot's sprite up in the room's declared table first, then in the
/// global weapon-FX metadata.
fn find_sprite<'a>(
    room: &'a crate::effects::RoomEffects,
    weapon: &'a crate::effects::WeaponEffects,
    effect: &Effect,
) -> Option<&'a EffectSprite> {
    let index = effect.sprite?;
    room.sprite(index).or_else(|| weapon.sprite(index))
}

/// The frames of the effect's selected animation row.
fn row_frames(sprite: &EffectSprite, depth: u8) -> Option<&Vec<crate::effects::room::AnimFrame>> {
    sprite.anim.depth(usize::from(depth))
}

/// Flattened index of the slot's current block within its animation row.
fn flat_index(frames: &[crate::effects::room::AnimFrame], effect: &Effect) -> Option<usize> {
    let mut index = 0usize;
    for frame in frames.iter().take(usize::from(effect.anim_frame)) {
        index += frame.blocks.len();
    }
    let block = usize::from(effect.anim_block);
    if block >= frames.get(usize::from(effect.anim_frame))?.blocks.len() {
        return None;
    }
    Some(index + block)
}

/// Locate a flattened block, returning the frame/block cursor too.
fn block_at(
    frames: &[crate::effects::room::AnimFrame],
    index: usize,
) -> Option<(u8, u8, crate::effects::pool::EffectBlock)> {
    let mut remaining = index;
    for (frame_index, frame) in frames.iter().enumerate() {
        if remaining < frame.blocks.len() {
            return Some((frame_index as u8, remaining as u8, frame.blocks[remaining]));
        }
        remaining -= frame.blocks.len();
    }
    None
}

/// Point the slot's current frame entry at `entry_index`.
fn apply_entry(effect: &mut Effect, sprite: &EffectSprite, entry_index: usize, entry: FrameEntry) {
    effect.frame_entry = entry_index as u8;
    effect.frame_index = entry.uv_index;
    effect.frame_delay = entry.delay;
    effect.size = [entry.width, entry.height];
    if let Some(uv) = sprite.info.uvs.get(usize::from(entry.uv_index)) {
        effect.uv = [uv.u, uv.v, uv.pivot_x, uv.pivot_y];
    }
}

/// Point the slot at frame-table entry `entry_index` of its current sprite.
fn set_frame(game: &mut GameState, index: usize, entry_index: usize) {
    let Some(effect) = game.effects.slot(index) else {
        return;
    };
    let effect = *effect;
    let Some(sprite) = find_sprite(&game.room_effects, &game.weapon_effects, &effect) else {
        return;
    };
    let Some(entry) = sprite.info.frames.get(entry_index).copied() else {
        return;
    };
    let mut updated = effect;
    apply_entry(&mut updated, sprite, entry_index, entry);
    if let Some(slot) = game.effects.slot_mut(index) {
        *slot = updated;
    }
}

/// Advance to the next 24-byte animation block, copying it over the header.
///
/// The yaw and light factor survive the copy; the type byte does not (the new
/// block's `initial_type` wins).
fn next_phase(game: &mut GameState, index: usize) {
    let Some(effect) = game.effects.slot(index) else {
        return;
    };
    let effect = *effect;
    let Some(sprite) = find_sprite(&game.room_effects, &game.weapon_effects, &effect) else {
        return;
    };
    let Some(frames) = row_frames(sprite, effect.anim_depth) else {
        return;
    };
    let Some(flat) = flat_index(frames, &effect) else {
        return;
    };
    let Some((frame, block, next)) = block_at(frames, flat + 1) else {
        return;
    };
    let mut updated = effect;
    updated.anim_id = next.anim_id();
    updated.update_id = next.update_id();
    updated.active = next.initial_type();
    updated.header.copy_from_slice(&next.0[4..22]);
    updated.anim_frame = frame;
    updated.anim_block = block;
    if let Some(slot) = game.effects.slot_mut(index) {
        *slot = updated;
    }
}

/// Land on the ground: jump to the splash frame, zero the accumulated
/// velocity, switch to the inert behaviour and, when the phase asks for it,
/// spawn a type-9 dust ring.
fn ground_splat(game: &mut GameState, index: usize) {
    let Some(effect) = game.effects.slot(index) else {
        return;
    };
    let effect = *effect;
    let dust_light = effect.header[3];
    let dust = effect.header[1] != 0;
    let position = effect.pos;
    set_frame(game, index, 5);
    let Some(slot) = game.effects.slot_mut(index) else {
        return;
    };
    let mut e = *slot;
    e.anim_id = 1;
    e.update_id = 1;
    e.header[6] = 0;
    e.header[7] = 0;
    copy_h16(&mut e, 4, 6);
    copy_h16(&mut e, 0x0e, 4);
    copy_h16(&mut e, 0x0c, 0x0e);
    *slot = e;

    if dust {
        let pos = [
            i32::from(position[0]),
            i32::from(position[1]),
            i32::from(position[2]),
        ];
        spawn(game, 9, 1, Attach::Identity, pos, 0, dust_light);
    }
}

/// Spawn a billboard and remember the returned slot for later behaviours.
fn spawn_from_header(game: &mut GameState, index: usize) {
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    let pos = [
        i32::from(effect.pos[0]),
        i32::from(effect.pos[1]),
        i32::from(effect.pos[2]),
    ];
    let spawned = spawn(
        game,
        effect.header[0],
        effect.header[1],
        Attach::Identity,
        pos,
        effect.yaw,
        0,
    );
    game.last_effect_spawn = spawned;
}

/// Spawn a billboard with an explicit attach target.
fn spawn(
    game: &mut GameState,
    effect_type: u8,
    depth: u8,
    attach: Attach,
    pos: [i32; 3],
    yaw: i16,
    light: u8,
) -> Option<u8> {
    let room = Rc::clone(&game.room_effects);
    create_attached(game, &room, effect_type, depth, attach, pos, yaw, light)
}

/// Free a slot immediately.
fn kill(game: &mut GameState, index: usize) {
    game.effects.release(index);
}

/// The rotation matrix of the slot's attach target.
fn attach_matrix(game: &GameState, attach: Attach) -> [i16; 9] {
    let entity = match attach {
        Attach::Identity => return IDENTITY_MATRIX,
        Attach::Omodel(index) => {
            // A missing or inactive object keeps the identity fallback. The
            // object's transform is its composed SCA world matrix, so an
            // effect attached to a child rides the parent chain.
            if !game
                .objects
                .record(usize::from(index))
                .is_some_and(|object| object.active())
            {
                return IDENTITY_MATRIX;
            }
            let world = crate::objects::world_matrix(
                &game.objects,
                usize::from(index),
                game.entities[0].pos,
                game.entities[0].angle,
            );
            return std::array::from_fn(|slot| world.r[slot / 3][slot % 3] as i16);
        }
        Attach::Item(index) => {
            // The item is not an entity; its composed world transform is the
            // attach frame. A missing record falls back to identity.
            let world = crate::objects::item_world_transform(
                &game.items,
                &game.objects,
                usize::from(index),
                game.entities[0].pos,
                game.entities[0].angle,
            );
            return std::array::from_fn(|slot| world.r[slot / 3][slot % 3] as i16);
        }
        Attach::Joint(entity, joint) => {
            let Some(world) = game
                .joint_worlds
                .get(usize::from(entity))
                .and_then(|joints| joints.get(usize::from(joint)))
            else {
                return IDENTITY_MATRIX;
            };
            return std::array::from_fn(|slot| world.r[slot / 3][slot % 3] as i16);
        }
        Attach::Player => &game.entities[0],
        Attach::Entity(slot) => match game.entities.get(usize::from(slot)) {
            Some(entity) => entity,
            None => return IDENTITY_MATRIX,
        },
        Attach::WebClone(slot) => match game
            .web_clones
            .get(usize::from(slot))
            .and_then(Option::as_ref)
        {
            Some(clone) => &clone.entity,
            None => return IDENTITY_MATRIX,
        },
        Attach::Companion(slot) => match game
            .companions
            .get(usize::from(slot))
            .and_then(Option::as_ref)
        {
            Some(companion) => &companion.entity,
            None => return IDENTITY_MATRIX,
        },
    };
    let matrix = anim::entity_matrix(entity.pos, entity.angle);
    std::array::from_fn(|index| matrix.r[index / 3][index % 3] as i16)
}

/// The translation of the slot's attach target.
///
/// The original copies the parent's full `MATRIX` over the slot while `type`
/// is 1; its translation lands in the sprite-offset fields and is added back
/// after the rotation, so an effect attached to a character is positioned
/// relative to that character, not the world origin. An object model resolves
/// to its composed world position through the SCA parent chain, exactly like a
/// character.
fn attach_translation(game: &GameState, attach: Attach) -> [i32; 3] {
    match attach {
        Attach::Identity => [0, 0, 0],
        Attach::Omodel(index) => {
            if !game
                .objects
                .record(usize::from(index))
                .is_some_and(|object| object.active())
            {
                return [0, 0, 0];
            }
            crate::objects::world_matrix(
                &game.objects,
                usize::from(index),
                game.entities[0].pos,
                game.entities[0].angle,
            )
            .t
        }
        Attach::Item(index) => {
            crate::objects::item_world_transform(
                &game.items,
                &game.objects,
                usize::from(index),
                game.entities[0].pos,
                game.entities[0].angle,
            )
            .t
        }
        Attach::Joint(entity, joint) => game
            .joint_worlds
            .get(usize::from(entity))
            .and_then(|joints| joints.get(usize::from(joint)))
            .map_or([0, 0, 0], |world| world.t),
        Attach::Player => game.entities[0].pos,
        Attach::Entity(slot) => game
            .entities
            .get(usize::from(slot))
            .map_or([0, 0, 0], |entity| entity.pos),
        Attach::WebClone(slot) => game
            .web_clones
            .get(usize::from(slot))
            .and_then(Option::as_ref)
            .map_or([0, 0, 0], |clone| clone.entity.pos),
        Attach::Companion(slot) => game
            .companions
            .get(usize::from(slot))
            .and_then(Option::as_ref)
            .map_or([0, 0, 0], |companion| companion.entity.pos),
    }
}

/// Re-copy the attach transform into the slot (`type == 1` refresh),
/// translation included.
fn refresh_transform(game: &mut GameState, index: usize) {
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    let matrix = attach_matrix(game, effect.attach);
    let translation = attach_translation(game, effect.attach);
    if let Some(slot) = game.effects.slot_mut(index) {
        slot.transform = matrix;
        slot.sprite_offset = translation;
    }
}

/// The shared tail of the wobble behaviours: switch to the fire behaviour in
/// its inert pose.
fn switch_to_wobble_phase(game: &mut GameState, index: usize) {
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    let transform = attach_matrix(game, effect.attach);
    let translation = attach_translation(game, effect.attach);
    if let Some(slot) = game.effects.slot_mut(index) {
        slot.anim_id = 0x11;
        slot.header[3] = 0;
        slot.active = 2;
        slot.transform = transform;
        slot.sprite_offset = translation;
    }
}

/// The bouncing-debris body (behaviours 17 and 36): reflect the vertical speed
/// on floor contact and re-randomize the spread while the fall is above the
/// phase count. `spread` is the horizontal base and `weight` the gravity base.
fn bounce_debris(game: &mut GameState, room: &RoomState, index: usize, spread: i16, weight: i16) {
    let seed = game.rand_seed;
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    let pos = [
        i32::from(effect.pos[0]),
        i32::from(effect.pos[1]),
        i32::from(effect.pos[2]),
    ];
    if h16(&effect, 0x0e) > 0
        && probe_ground(room, pos, [0, 0, 0], 0x3c) != 0
        && let Some(slot) = game.effects.slot_mut(index)
    {
        let flipped = h16(slot, 4).wrapping_neg();
        set_h16(slot, 4, flipped);
    }
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    // The phase byte is signed in the comparison.
    let phase = effect.header[3] as i8 as i16;
    if h16(&effect, 0x0e) >= phase {
        return;
    }
    let factor = effect.header[2] as i8 as i16;
    let low = (seed & 3) as i16;
    let horizontal = (spread - low).wrapping_mul(factor);
    let vertical = (0x23 - low).wrapping_mul(2);
    let new_phase = (vertical / -3) as u8;
    let Some(slot) = game.effects.slot_mut(index) else {
        return;
    };
    set_h16(slot, 0x0c, horizontal);
    set_h16(slot, 0x0e, vertical);
    slot.header[3] = new_phase;
    let drift = h16(slot, 0x10).wrapping_add(((seed % 5) as i16).wrapping_mul(factor));
    set_h16(slot, 0x10, drift);
    set_h16(slot, 4, ((seed % 3) as i16).wrapping_mul(factor));
    set_h16(slot, 6, (-weight).wrapping_sub((seed & 1) as i16));
    slot.header[2] = effect.header[2].wrapping_neg();
}

/// The ground probe: walk the collision records of the quadrant the point
/// falls in and OR their ground-contact flag bits. Returns 1 on a full ground
/// contact (both flag bits set), otherwise the OR of the seen flag bits.
fn probe_ground(room: &RoomState, pos: [i32; 3], offset: [i16; 3], radius: u32) -> u16 {
    let x = pos[0].wrapping_add(i32::from(offset[0]));
    let z = pos[2].wrapping_add(i32::from(offset[2]));
    let r = (radius & 0xFFFF) as i32;
    let mut flags = 0u16;
    for rect in room.collision.records(x, z) {
        let x_hi = i32::from(rect.x_max) + r;
        let z_hi = i32::from(rect.z_max) + r;
        let x_lo = i32::from(rect.x_min) - r;
        let z_lo = i32::from(rect.z_min) - r;
        if crate::player::point_outside(x, z, x_hi, z_hi, x_lo, z_lo) {
            continue;
        }
        let bits = rect.flags & 0xff00;
        if (bits >> 8) == 3 {
            return 1;
        }
        flags |= bits & 0x0300;
    }
    flags
}

// ---------------------------------------------------------------------------
// Velocity integration and projection
// ---------------------------------------------------------------------------

/// `(a*b + correction) >> 12`, truncating toward zero.
fn dot12(row: [i32; 3], vector: [i32; 3]) -> i32 {
    let product = row[0] * vector[0] + row[1] * vector[1] + row[2] * vector[2];
    (product + ((product >> 31) & 0xFFF)) >> 12
}

/// `M * v` with the PS1 Y-negation convention, returning a short vector.
fn apply_matrix_sv(matrix: &[i16; 9], v: [i16; 3]) -> [i16; 3] {
    let vector = [i32::from(v[0]), -i32::from(v[1]), i32::from(v[2])];
    let out = [
        dot12(
            [
                i32::from(matrix[0]),
                i32::from(matrix[1]),
                i32::from(matrix[2]),
            ],
            vector,
        ),
        dot12(
            [
                i32::from(matrix[3]),
                i32::from(matrix[4]),
                i32::from(matrix[5]),
            ],
            vector,
        ),
        dot12(
            [
                i32::from(matrix[6]),
                i32::from(matrix[7]),
                i32::from(matrix[8]),
            ],
            vector,
        ),
    ];
    [out[0] as i16, (-out[1]) as i16, out[2] as i16]
}

/// `M * v` with the PS1 Y-negation convention, returning full-width ints.
fn apply_matrix(matrix: &[i16; 9], v: [i16; 3]) -> [i32; 3] {
    let vector = [i32::from(v[0]), -i32::from(v[1]), i32::from(v[2])];
    let out = [
        dot12(
            [
                i32::from(matrix[0]),
                i32::from(matrix[1]),
                i32::from(matrix[2]),
            ],
            vector,
        ),
        dot12(
            [
                i32::from(matrix[3]),
                i32::from(matrix[4]),
                i32::from(matrix[5]),
            ],
            vector,
        ),
        dot12(
            [
                i32::from(matrix[6]),
                i32::from(matrix[7]),
                i32::from(matrix[8]),
            ],
            vector,
        ),
    ];
    [out[0], -out[1], out[2]]
}

/// Rotate a short vector about Y by `yaw` (a yaw-only 4.12 rotation).
fn yaw_rotate(yaw: i16, v: [i16; 3]) -> [i16; 3] {
    let sin = anim::sin14(i32::from(yaw)) / 4;
    let cos = anim::cos14(i32::from(yaw)) / 4;
    let matrix = [
        cos as i16,
        0,
        sin as i16,
        0,
        4096,
        0,
        -sin as i16,
        0,
        cos as i16,
    ];
    apply_matrix_sv(&matrix, v)
}

/// Project a world point to screen pixels and a quarter-depth.
fn project(camera: &Camera, fov: i32, world: [i32; 3]) -> (i16, i16, i32) {
    let view = camera.view_position(world);
    let depth = if view[2] == 0 { 1 } else { view[2] };
    let focal = i64::from(fov);
    let z = i64::from(depth);
    let screen_x = i64::from(view[0]) * focal / z + 160;
    let screen_y = 120 - i64::from(view[1]) * focal / z;
    (screen_x as i16, screen_y as i16, depth >> 2)
}

/// Compute the slot's world position and projection under `camera`, returning
/// the updated record. `None` when the slot's header does not carry the
/// transform bit, in which case it is not projected at all.
///
/// The type-1 rows re-copy the attach transform first, matching the original's
/// per-tick refresh; the returned record carries the refreshed transform so
/// callers can store it.
fn projected(game: &GameState, camera: &Camera, fov: i32, effect: &Effect) -> Option<Effect> {
    if effect.flags() & 0x0002 == 0 {
        return None;
    }

    let rotated = yaw_rotate(effect.yaw, effect.rot_speed);
    let local = [
        rotated[0].wrapping_add(effect.local_offset[0]),
        rotated[1].wrapping_add(effect.local_offset[1]),
        rotated[2].wrapping_add(effect.local_offset[2]),
    ];

    let mut e = *effect;
    if e.active == 1 {
        e.transform = attach_matrix(game, e.attach);
        e.sprite_offset = attach_translation(game, e.attach);
    }
    let transformed = apply_matrix(&e.transform, local);
    let mut world = [
        (e.sprite_offset[0] as i16).wrapping_add(transformed[0] as i16) as i32,
        (e.sprite_offset[1] as i16).wrapping_add(transformed[1] as i16) as i32,
        (e.sprite_offset[2] as i16).wrapping_add(transformed[2] as i16) as i32,
    ];
    if e.flags() & 0x0004 != 0 {
        world[1] = 0;
    }
    if e.flags() & 0x0008 != 0 {
        world[1] = game.entities[0].pos[1];
    }
    e.pos = [world[0] as i16, world[1] as i16, world[2] as i16];

    let (screen_x, screen_y, depth) = project(camera, fov, world);
    e.screen = [screen_x, screen_y];
    e.proj_depth = depth;
    e.depth_scaled = ((depth as u16) << 2) as i16;
    Some(e)
}

/// Integrate the velocity header through the yaw and attach transform, project
/// the result, and accumulate this tick's velocity deltas.
fn integrate(game: &mut GameState, camera: &Camera, fov: i32, index: usize, running: bool) {
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    let Some(mut e) = projected(game, camera, fov, &effect) else {
        return;
    };

    if running {
        let velocity = [h16(&e, 4), h16(&e, 6), h16(&e, 8)];
        let accumulator = [h16(&e, 12), h16(&e, 14), h16(&e, 16)];
        for axis in 0..3 {
            let next = accumulator[axis].wrapping_add(velocity[axis]);
            set_h16(&mut e, 12 + axis * 2, next);
            e.rot_speed[axis] = e.rot_speed[axis].wrapping_add(next);
        }
    }

    if let Some(slot) = game.effects.slot_mut(index) {
        *slot = e;
    }
}

/// Re-project every live slot under the room's current camera without
/// advancing behaviours or velocity. The camera-zone scan runs after
/// [`update`] in the room tick, so this restores a consistent frame when the
/// cut moved.
pub fn reproject(game: &mut GameState, room: &RoomState) {
    let Some(cut) = room.cuts.get(room.current_cut) else {
        return;
    };
    let camera = Camera::from_cut(cut);
    for index in (0..EFFECT_POOL_SIZE).rev() {
        let Some(effect) = game.effects.slot(index).copied() else {
            continue;
        };
        if effect.anim_id == 0 {
            continue;
        }
        if let Some(updated) = projected(game, &camera, cut.fov, &effect)
            && let Some(slot) = game.effects.slot_mut(index)
        {
            *slot = updated;
        }
    }
}

/// Step the sprite animation when the current frame delay expires.
fn animate_sprite(game: &mut GameState, index: usize) {
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    if effect.frame_delay != 0 {
        if let Some(slot) = game.effects.slot_mut(index) {
            slot.frame_delay -= 1;
        }
        return;
    }
    let Some(sprite) = find_sprite(&game.room_effects, &game.weapon_effects, &effect) else {
        return;
    };
    let frames = &sprite.info.frames;
    let next = usize::from(effect.frame_entry) + 1;
    let Some(entry) = frames.get(next).copied() else {
        return;
    };
    if entry.uv_index == 0 && entry.delay == 0 {
        if effect.anim_id == 0 && effect.update_id == 0 {
            return;
        }
        game.effects.release(index);
        return;
    }
    let (entry_index, entry) = if entry.delay == 0xFF {
        let jump = usize::from(entry.uv_index);
        match frames.get(jump).copied() {
            Some(jump_entry) => (jump, jump_entry),
            None => (next, entry),
        }
    } else {
        (next, entry)
    };
    let mut updated = effect;
    apply_entry(&mut updated, sprite, entry_index, entry);
    updated.frame_delay = entry.delay.wrapping_sub(1);
    if let Some(slot) = game.effects.slot_mut(index) {
        *slot = updated;
    }
}

// ---------------------------------------------------------------------------
// The dispatch table
// ---------------------------------------------------------------------------

/// Dispatch behaviour `id` on the slot. Unimplemented ids only bump the
/// placeholder counter.
fn run_behavior(game: &mut GameState, room: &RoomState, index: usize, id: u8) {
    if !implemented(id) {
        if let Some(count) = game.effect_placeholder_hits.get_mut(usize::from(id)) {
            *count += 1;
        }
        return;
    }
    match id {
        0 | 1 => {}
        // Countdown, then advance a phase and refresh the attach transform.
        2 => {
            let phase = game.effects.slot(index).map_or(0, |e| e.header[3]);
            if phase == 0 {
                next_phase(game, index);
                refresh_transform(game, index);
            } else if let Some(effect) = game.effects.slot_mut(index) {
                effect.header[3] -= 1;
            }
        }
        // Freeze the current sprite frame.
        3 => {
            if let Some(effect) = game.effects.slot_mut(index) {
                effect.frame_delay = 0;
            }
        }
        // Burning ember: count down, then jump sideways with a hot colour.
        4 => {
            let phase = game.effects.slot(index).map_or(0, |e| e.header[3]);
            if phase == 0 {
                next_phase(game, index);
                if let Some(effect) = game.effects.slot_mut(index) {
                    effect.light_factor = 0x1e;
                    effect.local_offset[0] = effect.local_offset[0].wrapping_sub(0xfa);
                }
            } else if let Some(effect) = game.effects.slot_mut(index) {
                effect.header[3] -= 1;
            }
        }
        // Gravity settle: zero the accumulators on ground contact.
        5 => {
            let effect = game.effects.slot(index).copied();
            let Some(effect) = effect else { return };
            let pos = [
                i32::from(effect.pos[0]),
                i32::from(effect.pos[1]),
                i32::from(effect.pos[2]),
            ];
            if probe_ground(room, pos, [0, 0, 0], 4) != 0
                && let Some(effect) = game.effects.slot_mut(index)
            {
                zero_accumulators(effect);
                effect.anim_id = 1;
            }
        }
        // Plain phase timer.
        6 => {
            let phase = game.effects.slot(index).map_or(1, |e| e.header[3]);
            if phase == 0 {
                next_phase(game, index);
            } else if let Some(effect) = game.effects.slot_mut(index) {
                effect.header[3] -= 1;
            }
        }
        // Floor bounce.
        7 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            if effect.pos[1] as i32 + i32::from(h16(&effect, 0x0e)) >= 0
                && let Some(slot) = game.effects.slot_mut(index)
            {
                slot.anim_id = 1;
                slot.update_id = 1;
                let flags = slot.flags() | 0x4007;
                slot.set_flags(flags);
                settle_velocity(slot);
                slot.rot_speed[1] = (slot.sprite_offset[1] as i16)
                    .wrapping_add(slot.local_offset[1])
                    .wrapping_neg();
            }
        }
        // Bouncing debris: reflect the vertical speed on the floor and
        // re-randomize the spread while the fall is above the phase count.
        17 => bounce_debris(game, room, index, 0x1E, 2),
        // Gravity impact: on contact go inert and mirror the horizontal speed.
        8 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let pos = [
                i32::from(effect.pos[0]),
                i32::from(effect.pos[1]),
                i32::from(effect.pos[2]),
            ];
            if probe_ground(room, pos, [0, 0, 0], 4) != 0
                && let Some(slot) = game.effects.slot_mut(index)
            {
                slot.anim_id = 1;
                slot.header[0] = 2;
                slot.header[2] = 1;
                let x = h16(slot, 0x0c).wrapping_neg();
                set_h16(slot, 0x0c, x);
            }
        }
        // Gravity rise: pull toward the player's height.
        9 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let player_y = game.entities[0].pos[1];
            // The pose flags are applied every tick, before the height test.
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[10] = 3;
                slot.header[11] = 0x40;
            }
            if player_y < i32::from(effect.pos[1]) + i32::from(h16(&effect, 0x0e))
                && let Some(slot) = game.effects.slot_mut(index)
            {
                let flags = slot.flags() | 0x400b;
                slot.set_flags(flags);
                slot.anim_id = 0x2f;
                slot.update_id = 0;
                slot.rot_speed[1] =
                    (player_y - slot.sprite_offset[1] - i32::from(slot.local_offset[1])) as i16;
            }
        }
        // Random next phase.
        11 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            set_frame(game, index, usize::from(effect.header[0]));
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let Some(sprite) = find_sprite(&game.room_effects, &game.weapon_effects, &effect)
            else {
                return;
            };
            let Some(frames) = row_frames(sprite, effect.anim_depth) else {
                return;
            };
            let Some(flat) = flat_index(frames, &effect) else {
                return;
            };
            let pick = flat + 1 + usize::from(game.rand_seed & 1);
            let Some((_, _, chosen)) = block_at(frames, pick) else {
                return;
            };
            // Both branches leave the animation cursor two blocks ahead of
            // where it started.
            let cursor = block_at(frames, flat + 2);
            let phase = effect.header[3];
            let light = effect.light_factor;
            let yaw = effect.yaw;
            let transform = attach_matrix(game, effect.attach);
            let translation = attach_translation(game, effect.attach);
            let Some(slot) = game.effects.slot_mut(index) else {
                return;
            };
            slot.anim_id = chosen.anim_id();
            slot.update_id = chosen.update_id();
            slot.active = chosen.initial_type();
            slot.header.copy_from_slice(&chosen.0[4..22]);
            if let Some((frame, block, _)) = cursor {
                slot.anim_frame = frame;
                slot.anim_block = block;
            }
            slot.yaw = chosen.yaw().wrapping_add(yaw);
            slot.header[3] = phase;
            slot.light_factor = light;
            slot.transform = transform;
            slot.sprite_offset = translation;
        }
        // Spawner: clone itself at its local offset, then advance.
        12 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            if effect.header[2] == 0 {
                let pos = [
                    i32::from(effect.local_offset[0]),
                    i32::from(effect.local_offset[1]),
                    i32::from(effect.local_offset[2]),
                ];
                spawn(
                    game,
                    effect.effect_type,
                    effect.depth_group,
                    effect.attach,
                    pos,
                    effect.yaw,
                    0,
                );
                next_phase(game, index);
            } else if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[2] -= 1;
            }
        }
        // Countdown, then next phase.
        13 => {
            let count = game.effects.slot(index).map_or(1, |e| e.header[2]);
            if count == 0 {
                next_phase(game, index);
            } else if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[2] -= 1;
            }
        }
        // Blink: toggle the hidden bit every tick.
        14 => {
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[11] ^= 0x80;
            }
        }
        // Floor splash: freeze, spawn the header billboard, merge the light.
        15 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            if effect.pos[1] as i32 + i32::from(h16(&effect, 0x0e)) >= 0 {
                let parent_light = effect.light_factor;
                if let Some(slot) = game.effects.slot_mut(index) {
                    slot.anim_id = 0x2f;
                    slot.update_id = 0;
                    slot.rot_speed[1] = (slot.sprite_offset[1] as i16)
                        .wrapping_add(slot.local_offset[1])
                        .wrapping_neg();
                }
                spawn_from_header(game, index);
                if let Some(child) = game.last_effect_spawn
                    && let Some(slot) = game.effects.slot_mut(usize::from(child))
                {
                    slot.light_factor =
                        slot.light_factor.wrapping_add(parent_light.wrapping_sub(3));
                }
            }
        }
        // Projectile wobble: random spin and colour while the count lasts.
        16 => {
            let count = game.effects.slot(index).map_or(0, |e| e.header[3]);
            if count != 0 {
                wobble(game, index, 0x32, 0x78, 8);
                return;
            }
            switch_to_wobble_phase(game, index);
        }
        // Clone the slot into a fresh one and retire.
        18 => clone(game, index),
        // Fall and stop at the floor.
        19 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            if effect.pos[1] as i32 + i32::from(h16(&effect, 0x0e)) >= 0
                && let Some(slot) = game.effects.slot_mut(index)
            {
                slot.anim_id = 0x2f;
                slot.header[10] = 7;
                slot.header[11] = 0x40;
                settle_velocity(slot);
                slot.rot_speed[1] = (slot.sprite_offset[1] as i16)
                    .wrapping_add(slot.local_offset[1])
                    .wrapping_neg();
            }
        }
        // Next phase with a downward lurch.
        20 => {
            next_phase(game, index);
            if let Some(slot) = game.effects.slot_mut(index) {
                let fall = h16(slot, 0x0e).wrapping_sub(0x18);
                set_h16(slot, 0x0e, fall);
            }
        }
        // Faded fall: hand over to the drop-splat behaviour.
        21 => {
            let start = game.effects.slot(index).map_or(1, |e| e.header[0]);
            if start == 0 {
                let drop = game.effects.slot(index).map_or(0, |e| e.header[2]);
                if let Some(slot) = game.effects.slot_mut(index) {
                    slot.anim_id = 0x16;
                    slot.header[0x0e] = 0;
                    slot.header[0x0f] = 0;
                    set_h16(slot, 6, i16::from(drop));
                }
            }
            drop_splat(game, room, index);
        }
        // Drop and splat.
        22 => drop_splat(game, room, index),
        // Ground contact splat.
        23 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            if effect.pos[1] as i32 + i32::from(h16(&effect, 0x0e)) >= 0 {
                ground_splat(game, index);
                if let Some(slot) = game.effects.slot_mut(index) {
                    let flags = slot.flags() | 0x4007;
                    slot.set_flags(flags);
                }
            }
        }
        // Jitter: each set bit adds a 0..4 random delta to one velocity.
        24 => {
            let bits = game.effects.slot(index).map_or(0, |e| e.header[3]);
            let seed = game.rand_seed;
            if let Some(slot) = game.effects.slot_mut(index) {
                for (bit, offset) in [
                    (0x01u8, 0x0cu8),
                    (0x02, 0x0e),
                    (0x04, 0x10),
                    (0x08, 0x04),
                    (0x10, 0x06),
                    (0x20, 0x08),
                ] {
                    if bits & bit != 0 {
                        let delta = (seed % 5) as i16;
                        let value = h16(slot, usize::from(offset)).wrapping_add(delta);
                        set_h16(slot, usize::from(offset), value);
                    }
                }
            }
        }
        // Drift: move the local offset along the header's direction bytes.
        25 => {
            if let Some(slot) = game.effects.slot_mut(index) {
                for axis in 0..3 {
                    let delta = (slot.header[axis] as i8 as i16).wrapping_mul(2);
                    slot.local_offset[axis] = slot.local_offset[axis].wrapping_add(delta);
                }
            }
            next_phase(game, index);
        }
        // Spawn the header billboard at the slot's world position.
        26 => spawn_from_header(game, index),
        // Advance one frame-table entry with no delay.
        27 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let Some(sprite) = find_sprite(&game.room_effects, &game.weapon_effects, &effect)
            else {
                return;
            };
            if sprite
                .info
                .frames
                .get(usize::from(effect.frame_entry) + 1)
                .is_some()
                && let Some(slot) = game.effects.slot_mut(index)
            {
                slot.frame_entry += 1;
                slot.frame_delay = 0;
            }
        }
        // Ready-to-use next phase.
        28 => next_phase(game, index),
        // Jump 0..2 frame entries forward at random.
        29 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let step = usize::from((game.rand_seed % 3) as u8);
            set_frame(game, index, usize::from(effect.frame_entry) + step);
        }
        // Spawn the header billboard, then advance.
        30 => {
            spawn_from_header(game, index);
            next_phase(game, index);
        }
        // Snap to the frame entry named by the header.
        31 => {
            let entry = game.effects.slot(index).map_or(0, |e| e.header[0]);
            set_frame(game, index, usize::from(entry));
        }
        // Floor contact: freeze, step the frame and advance.
        32 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            if effect.pos[1] as i32 + i32::from(h16(&effect, 0x0e)) >= 0 {
                if let Some(slot) = game.effects.slot_mut(index) {
                    settle_velocity(slot);
                    slot.rot_speed[1] = (slot.sprite_offset[1] as i16)
                        .wrapping_add(slot.local_offset[1])
                        .wrapping_neg();
                }
                let entry = game.effects.slot(index).map_or(0, |e| e.header[0]);
                set_frame(game, index, usize::from(entry));
                next_phase(game, index);
            }
        }
        // Step the sprite frame, then next phase.
        33 => {
            let entry = game.effects.slot(index).map_or(0, |e| e.header[0]);
            set_frame(game, index, usize::from(entry));
            next_phase(game, index);
        }
        // Burning ground fire: ground contact spawns and kills.
        34 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let pos = [
                i32::from(effect.pos[0]),
                i32::from(effect.pos[1]),
                i32::from(effect.pos[2]),
            ];
            let contact = probe_ground(room, pos, [0, 0, 0], 4);
            if contact != 0 && effect.header[2] == 0x21 {
                spawn_from_header(game, index);
                kill(game, index);
                return;
            }
            let contact = probe_ground(room, pos, [0, 0, 0], 4);
            if contact == 1 {
                spawn_from_header(game, index);
                kill(game, index);
            }
        }
        // Weapon tint selector.
        38 => {
            const TINT: [u8; 10] = [7, 0x0d, 8, 8, 0x0f, 0x0f, 0x0f, 0x11, 0x11, 0x11];
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let light = TINT
                .get(usize::from(effect.header[0]))
                .copied()
                .unwrap_or(0);
            let transform = attach_matrix(game, effect.attach);
            let translation = attach_translation(game, effect.attach);
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.update_id = 1;
                slot.active = 2;
                slot.light_factor = light;
                slot.transform = transform;
                slot.sprite_offset = translation;
            }
        }
        // Paired timers: kill on one count, re-spawn on the other.
        39 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let count = effect.header[2].wrapping_sub(1);
            let life = effect.header[3].wrapping_sub(1);
            if life == 0 {
                kill(game, index);
                return;
            }
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[2] = count;
                slot.header[3] = life;
            }
            if count == 0 {
                let (effect_type, depth, yaw, attach, pos, light) = {
                    let e = game.effects.slot(index).copied().unwrap_or_default();
                    (
                        e.header[0],
                        e.header[1],
                        e.yaw,
                        e.attach,
                        [e.spawn_pos[0], e.spawn_pos[1], e.spawn_pos[2]],
                        e.light_factor,
                    )
                };
                if let Some(slot) = game.effects.slot_mut(index) {
                    slot.header[2] = 4;
                }
                spawn(game, effect_type, depth, attach, pos, yaw, light);
            }
        }
        // Latch the player's state into the header after the phase timer.
        40 => {
            refresh_phase(game, index);
            let flags = game.player_flags;
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[2] = flags;
            }
        }
        // Random flip bits, then jump to the behaviour named by the header.
        41 => {
            let pick = game.rand_seed % 3;
            let target = game.effects.slot(index).map_or(1, |e| e.header[2]);
            if let Some(slot) = game.effects.slot_mut(index) {
                let flags = slot.flags() | (pick << 6);
                slot.set_flags(flags);
                slot.anim_id = target;
            }
        }
        // Weapon charge: capture the player state, pose, then spawn the ring.
        42 => {
            let state = game.effects.slot(index).map_or(3, |e| e.header[3]);
            match state {
                0 => {
                    let flags = game.player_flags;
                    let weapon = game.equipped.unwrap_or(0);
                    if let Some(slot) = game.effects.slot_mut(index) {
                        slot.header[0] = flags;
                        slot.header[1] = weapon;
                        slot.header[3] += 1;
                    }
                }
                1 => {
                    let attach = game
                        .effects
                        .slot(index)
                        .map(|e| e.attach)
                        .unwrap_or_default();
                    let transform = attach_matrix(game, attach);
                    let translation = attach_translation(game, attach);
                    if let Some(slot) = game.effects.slot_mut(index) {
                        slot.active = 2;
                        slot.header[3] += 1;
                        slot.transform = transform;
                        slot.sprite_offset = translation;
                    }
                }
                2 => {
                    let Some(effect) = game.effects.slot(index).copied() else {
                        return;
                    };
                    let pos = [
                        i32::from(effect.pos[0]),
                        i32::from(effect.pos[1]),
                        i32::from(effect.pos[2]),
                    ];
                    spawn(game, 8, 1, Attach::Identity, pos, effect.yaw, 0);
                }
                _ => {}
            }
        }
        // Fire burst: spawn a ladder of type-1 billboards, then kill.
        44 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let pos = [
                i32::from(effect.local_offset[0]),
                i32::from(effect.local_offset[1]),
                i32::from(effect.local_offset[2]),
            ];
            let depth = effect.depth_group;
            match effect.header[0] {
                0 => {
                    spawn(
                        game,
                        1,
                        depth.wrapping_add(1),
                        effect.attach,
                        pos,
                        effect.yaw,
                        0,
                    );
                    if let Some(slot) = game.effects.slot_mut(index) {
                        slot.header[0] = 1;
                    }
                }
                1 => {
                    spawn(
                        game,
                        1,
                        depth.wrapping_add(2),
                        effect.attach,
                        pos,
                        effect.yaw,
                        0,
                    );
                    spawn(
                        game,
                        1,
                        depth.wrapping_add(3),
                        effect.attach,
                        pos,
                        effect.yaw,
                        0,
                    );
                    if let Some(slot) = game.effects.slot_mut(index) {
                        slot.header[0] = 2;
                    }
                }
                2 => {
                    spawn(
                        game,
                        1,
                        depth.wrapping_add(4),
                        effect.attach,
                        pos,
                        effect.yaw,
                        0,
                    );
                    kill(game, index);
                }
                _ => {}
            }
        }
        // Countdown, then kill.
        46 => {
            let count = game.effects.slot(index).map_or(1, |e| e.header[2]);
            if count == 0 {
                kill(game, index);
            } else if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[2] -= 1;
            }
        }
        47 => kill(game, index),
        // Fade: step the sprite every frame, decay the light factor.
        48 => {
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.frame_delay = 0;
                if slot.light_factor != 0 {
                    slot.light_factor = slot.light_factor.wrapping_sub(2);
                }
            }
        }
        // Flash marker: pin the header flags, count down to the kill.
        49 => {
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[10] = 6;
                slot.header[11] = 0;
            }
            let count = game.effects.slot(index).map_or(1, |e| e.header[1]);
            if count == 0 {
                kill(game, index);
            } else if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[1] -= 1;
            }
        }
        // Floor stop: freeze at the floor and retire.
        50 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            if effect.pos[1] as i32 + i32::from(h16(&effect, 0x0e)) >= 0
                && let Some(slot) = game.effects.slot_mut(index)
            {
                slot.anim_id = 0x2f;
                slot.update_id = 0;
                slot.header[10] = 6;
                slot.header[11] = 0;
                settle_velocity(slot);
                slot.rot_speed[1] = (slot.sprite_offset[1] as i16)
                    .wrapping_add(slot.local_offset[1])
                    .wrapping_neg();
            }
        }
        // System-flag gate: when system bit 0 rises, go inert on the named
        // frame.
        51 => {
            if game.flag_test(BANK_SYSTEM, 0, false) {
                if let Some(slot) = game.effects.slot_mut(index) {
                    slot.anim_id = 1;
                }
                let entry = game.effects.slot(index).map_or(0, |e| e.header[0]);
                set_frame(game, index, usize::from(entry));
            }
        }
        // Adopt the inert pose and settle immediately.
        52 => {
            let attach = game
                .effects
                .slot(index)
                .map(|e| e.attach)
                .unwrap_or_default();
            let transform = attach_matrix(game, attach);
            let translation = attach_translation(game, attach);
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.transform = transform;
                slot.sprite_offset = translation;
                slot.active = 2;
                slot.anim_id = 5;
            }
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let pos = [
                i32::from(effect.pos[0]),
                i32::from(effect.pos[1]),
                i32::from(effect.pos[2]),
            ];
            if probe_ground(room, pos, [0, 0, 0], 4) != 0
                && let Some(slot) = game.effects.slot_mut(index)
            {
                zero_accumulators(slot);
                slot.anim_id = 1;
            }
        }
        // Floor stop, flags untouched.
        53 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            if effect.pos[1] as i32 + i32::from(h16(&effect, 0x0e)) >= 0
                && let Some(slot) = game.effects.slot_mut(index)
            {
                slot.anim_id = 1;
                slot.update_id = 0;
                settle_velocity(slot);
                slot.rot_speed[1] = (slot.sprite_offset[1] as i16)
                    .wrapping_add(slot.local_offset[1])
                    .wrapping_neg();
            }
        }
        // Random spin on all three axes (one frame seed for every draw).
        54 => {
            let odd = game.rand_seed & 1 == 1;
            let speed = if odd { 5 } else { -15 };
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.rot_speed = [speed, speed, speed];
            }
        }
        // Splash timer: step to the splash frame every four ticks.
        55 => {
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let count = effect.header[0].wrapping_sub(1);
            if count == 0 {
                set_frame(game, index, 7);
                if let Some(slot) = game.effects.slot_mut(index) {
                    slot.header[0] = 4;
                }
            } else if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[0] = count;
            }
            let Some(effect) = game.effects.slot(index).copied() else {
                return;
            };
            let y = i32::from(h16(&effect, 0x0e)) + i32::from(effect.pos[1]);
            // The original also runs the player-damage splash probe inside the
            // `-0x961..-1` band, which needs combat; only the floor crossing
            // (`-1 < y`, the tighter of the two) remains.
            if -1 < y {
                ground_splat(game, index);
                if let Some(slot) = game.effects.slot_mut(index) {
                    let flags = slot.flags() | 0x4007;
                    slot.set_flags(flags);
                }
            }
        }
        // Fire wobble: the smaller-spread sibling of the projectile wobble.
        35 => {
            let count = game.effects.slot(index).map_or(0, |e| e.header[3]);
            if count != 0 {
                wobble(game, index, 0x19, 0x3c, 10);
                return;
            }
            switch_to_wobble_phase(game, index);
        }
        // Fire bounce: the tighter-spread sibling of the debris bounce.
        36 => bounce_debris(game, room, index, 10, 10),
        // Fire/ember lifecycle: grow, steady, re-ignite, decay.
        56 => fire_phases(game, index),
        // Muzzle-flash lifecycle: arm, flash pose, flicker, done.
        57 => muzzle_phases(game, index),
        // Auto-aim flash: the phase timer with the aim state latched.
        58 => {
            refresh_phase(game, index);
            let autoaim = autoaim_flags(game);
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[2] = autoaim;
            }
        }
        // Dual shot: fire one or two type-5 billboards.
        59 => dual_shot(game, index),
        // 60..=63 are the original's inert entries.
        60..=63 => {}
        _ => {}
    }
}

/// Zero the three position accumulators (the accumulator at 0x10 is cleared
/// last so the two spreads inherit the zero).
fn zero_accumulators(effect: &mut Effect) {
    effect.header[0x10] = 0;
    effect.header[0x11] = 0;
    copy_h16(effect, 0x0e, 0x10);
    copy_h16(effect, 0x0c, 0x0e);
}

/// The floor-contact settle: zero the vertical velocity, collapse every
/// velocity component onto it, then zero the accumulators.
fn settle_velocity(effect: &mut Effect) {
    effect.header[8] = 0;
    effect.header[9] = 0;
    copy_h16(effect, 6, 8);
    copy_h16(effect, 4, 6);
    zero_accumulators(effect);
}

/// The shared phase-timer body: count the phase byte down; at zero advance a
/// phase and refresh the attach transform.
fn refresh_phase(game: &mut GameState, index: usize) {
    let phase = game.effects.slot(index).map_or(0, |e| e.header[3]);
    if phase == 0 {
        next_phase(game, index);
        refresh_transform(game, index);
    } else if let Some(effect) = game.effects.slot_mut(index) {
        effect.header[3] -= 1;
    }
}

/// The original's `weapon_autoaim_check() & 3` latch value: the low two bits
/// of the equipped weapon's ammo count.
///
/// `weapon_autoaim_check` tops empty special weapons back up to counts that
/// are all multiples of four (the flamethrower returns its raw count), so the
/// low two bits equal the carried quantity's. Combat does not exist yet, so
/// the refill side effects are not applied here.
fn autoaim_flags(game: &GameState) -> u8 {
    game.equipped.map_or(0, |item| game.item_count(item) as u8) & 3
}

/// Ground-contact splat shared by behaviours 21 and 22.
fn drop_splat(game: &mut GameState, room: &RoomState, index: usize) {
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    let pos = [
        i32::from(effect.pos[0]),
        i32::from(effect.pos[1]),
        i32::from(effect.pos[2]),
    ];
    if probe_ground(room, pos, [0, 0, 0], 4) != 0 {
        ground_splat(game, index);
    }
}

/// The wobble body (behaviours 16 and 35): decrement the phase, re-randomize
/// the spin, drift, colour and frame.
fn wobble(game: &mut GameState, index: usize, spread: i16, bias: i16, drop: i16) {
    let seed = game.rand_seed;
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    let phase = effect.header[2] as i8 as i16;
    let spin = (seed % 6) as i16;
    let drift = (seed % 6) as i16;
    let mut updated = effect;
    updated.header[3] -= 1;
    if seed & 1 != 0 {
        let flags = updated.flags() | 0x0080;
        updated.set_flags(flags);
    }
    set_h16(
        &mut updated,
        0x0c,
        (spin * -spread + bias).wrapping_mul(phase),
    );
    set_h16(&mut updated, 6, -drop - drift);
    if seed & 1 != 0 {
        updated.light_factor = updated.light_factor.wrapping_sub(1);
    }
    updated.yaw = (((seed % 6) as i32 * 0x3c000) / 0x168) as i16;
    updated.header[0] = (seed % 6) as u8;
    if let Some(slot) = game.effects.slot_mut(index) {
        *slot = updated;
    }
    let entry = game.effects.slot(index).map_or(0, |e| e.header[0]);
    set_frame(game, index, usize::from(entry));
}

/// Clone the slot into a free one, continue one block ahead, retire the
/// original.
fn clone(game: &mut GameState, index: usize) {
    let Some(source) = game.effects.slot(index).copied() else {
        return;
    };
    if source.header[0] != 0 {
        if let Some(slot) = game.effects.slot_mut(index) {
            slot.header[0] -= 1;
        }
        return;
    }
    if game.effects.free_slots() == 0 {
        if let Some(slot) = game.effects.slot_mut(index) {
            slot.anim_id = 1;
        }
        return;
    }
    let Some(sprite) = find_sprite(&game.room_effects, &game.weapon_effects, &source) else {
        return;
    };
    let Some(frames) = row_frames(sprite, source.anim_depth) else {
        return;
    };
    let Some(flat) = flat_index(frames, &source) else {
        return;
    };
    let Some((frame, block, next)) = block_at(frames, flat + 1) else {
        return;
    };
    let Some(slot) = game.effects.claim() else {
        return;
    };
    let first = sprite.info.frames.first().copied().unwrap_or_default();
    let mut dst = Effect {
        effect_type: source.effect_type,
        depth_group: source.depth_group,
        attach: source.attach,
        spawn_pos: source.spawn_pos,
        local_offset: [
            source.spawn_pos[0] as i16,
            source.spawn_pos[1] as i16,
            source.spawn_pos[2] as i16,
        ],
        sprite: source.sprite,
        anim_depth: source.anim_depth,
        anim_frame: frame,
        anim_block: block,
        anim_id: next.anim_id(),
        update_id: next.update_id(),
        active: next.initial_type(),
        // The original copies the whole 24-byte block over the record,
        // light factor included, so the clone keeps the next phase's scale.
        light_factor: next.light(),
        yaw: next.yaw(),
        frame_entry: 0,
        frame_index: first.uv_index,
        frame_delay: 0,
        ..Effect::default()
    };
    dst.header.copy_from_slice(&next.0[4..22]);
    apply_entry(&mut dst, sprite, 0, first);
    if let Some(slot_ref) = game.effects.slot_mut(slot) {
        *slot_ref = dst;
    }
    if let Some(slot_ref) = game.effects.slot_mut(index) {
        slot_ref.anim_id = 1;
    }
}

/// Fire/ember four-phase lifecycle.
fn fire_phases(game: &mut GameState, index: usize) {
    let phase = game.effects.slot(index).map_or(3, |e| e.header[0]);
    match phase {
        0 => {
            set_frame(game, index, 5);
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[0] = 1;
                slot.header[1] = 0x0b;
            }
            fire_phases_steady(game, index);
        }
        1 => fire_phases_steady(game, index),
        2 => {
            let countdown = ((game.rand_seed % 5) as i8)
                .wrapping_add(10)
                .wrapping_mul(10);
            set_frame(game, index, 5);
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[0] = 3;
                slot.header[1] = countdown as u8;
                slot.header[10] = 0;
                slot.header[11] = 0;
                slot.light_factor = 0;
            }
        }
        _ => {
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[10] = 0;
                slot.header[11] = 0;
                slot.light_factor = 0;
                slot.header[1] = slot.header[1].wrapping_sub(1);
                if slot.header[1] == 0 {
                    slot.header[0] = 0;
                    slot.header[1] = 0x0b;
                }
            }
        }
    }
}

/// The steady phase of the fire lifecycle.
fn fire_phases_steady(game: &mut GameState, index: usize) {
    let heat = game.effects.slot(index).map_or(0, |e| e.header[2]);
    if let Some(slot) = game.effects.slot_mut(index) {
        slot.header[10] = 3;
        slot.header[11] = 0x40;
        slot.light_factor = heat;
        slot.header[1] = slot.header[1].wrapping_sub(1);
        if slot.header[1] == 0 {
            slot.header[0] = 2;
            slot.header[10] = 0;
            slot.header[11] = 0;
            slot.light_factor = 0;
        }
    }
}

/// Muzzle-flash four-phase lifecycle.
fn muzzle_phases(game: &mut GameState, index: usize) {
    let phase = game.effects.slot(index).map_or(3, |e| e.header[0]);
    match phase {
        0 => {
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[1] = 1;
                slot.header[0] = 1;
            }
        }
        1 => {
            let attach = game
                .effects
                .slot(index)
                .map(|e| e.attach)
                .unwrap_or_default();
            let transform = attach_matrix(game, attach);
            let translation = attach_translation(game, attach);
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.transform = transform;
                slot.sprite_offset = translation;
                slot.active = 2;
                slot.header[1] = 2;
                slot.header[0] = 2;
            }
        }
        2 => {
            if let Some(slot) = game.effects.slot_mut(index) {
                slot.header[1] = slot.header[1].wrapping_add(1);
            }
            let flicker = game.effects.slot(index).map_or(0, |e| e.header[1]);
            if flicker == 9 {
                set_frame(game, index, 4);
                if let Some(slot) = game.effects.slot_mut(index) {
                    slot.header[2] = slot.header[2].wrapping_sub(1);
                    slot.header[1] = 4;
                }
            }
            let bursts = game.effects.slot(index).map_or(0, |e| e.header[2]);
            if bursts == 0
                && let Some(slot) = game.effects.slot_mut(index)
            {
                slot.header[0] = 3;
            }
        }
        _ => {}
    }
}

/// Dual shot: spawn the type-5 billboards once the count expires.
fn dual_shot(game: &mut GameState, index: usize) {
    let Some(effect) = game.effects.slot(index).copied() else {
        return;
    };
    if effect.header[0] != 0 {
        if let Some(slot) = game.effects.slot_mut(index) {
            slot.header[0] -= 1;
        }
        return;
    }
    let offset_x = effect.local_offset[0];
    let offset_y = effect.local_offset[1];
    let offset_z = effect.local_offset[2];
    let aim_x = effect.header[2] as i8 as i16;
    let aim_z = effect.header[3] as i8 as i16;
    let weapon = 0u8;
    if effect.header[1] != 0 {
        let pos = [
            i32::from(offset_x) - i32::from(aim_x),
            i32::from(offset_y) + 10,
            i32::from(aim_z) + i32::from(offset_z),
        ];
        if let Some(child) = spawn(
            game,
            5,
            3,
            effect.attach,
            pos,
            effect.yaw,
            effect.light_factor,
        ) && let Some(slot) = game.effects.slot_mut(usize::from(child))
        {
            slot.header[3] = weapon.wrapping_sub(2);
        }
    }
    let pos = [
        i32::from(aim_x) + i32::from(offset_x),
        i32::from(offset_y),
        i32::from(aim_z) + i32::from(offset_z),
    ];
    if let Some(child) = spawn(
        game,
        5,
        3,
        effect.attach,
        pos,
        effect.yaw,
        effect.light_factor,
    ) && let Some(slot) = game.effects.slot_mut(usize::from(child))
    {
        slot.header[3] = weapon.wrapping_sub(2);
    }
    next_phase(game, index);
}

/// The projected-depth cull threshold: a projection beyond this is not drawn.
pub const PROJECTED_DEPTH_CULL: i32 = 0x3FFF;

/// Whether the slot's hidden bit (`mass_mask`) suppresses this frame.
pub fn hidden(effect: &Effect) -> bool {
    effect.header[11] & 0x80 != 0
}

/// Whether a world position lies inside the current camera's switch zone.
pub fn in_switch_zone(room: &RoomState, camera: usize, x: i32, z: i32) -> bool {
    let header = room
        .zones
        .iter()
        .find(|zone| zone.cam_from >= 0 && zone.cam_from as usize == camera);
    header.is_some_and(|zone| zone.contains(x, z))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::fixtures::{block, frame, sprite};
    use crate::effects::pool::create;
    use crate::effects::room::{FrameEntry, RoomEffects};
    use crate::state::{Cut, RoomState};

    fn room() -> RoomState {
        RoomState {
            cuts: vec![Cut {
                index: 0,
                pos: [0, 0, 0],
                look_at: [0, 0, 1000],
                fov: 200,
                ..Cut::default()
            }],
            ..RoomState::default()
        }
    }

    fn raw_block(anim: u8, update: u8) -> [u8; 24] {
        let mut bytes = block(anim, update, 0);
        bytes[14] = 0; // no flags by default
        bytes
    }

    /// A game with one weapon sprite (type 9) whose single depth row holds
    /// `blocks` in one frame.
    fn game_with_row(blocks: &[[u8; 24]]) -> GameState {
        let mut game = GameState::default();
        game.weapon_effects
            .sprites
            .push(sprite(9, std::array::from_fn(|_| Vec::new())));
        let sprite = &mut game.weapon_effects.sprites[0];
        sprite.anim.depth_frames[0] = vec![frame(blocks)];
        game
    }

    fn spawn(game: &mut GameState, depth: u8, pos: [i32; 3], yaw: i16, light: u8) -> u8 {
        create(game, &RoomEffects::default(), 9, depth, 0, pos, yaw, light).unwrap()
    }

    fn run(game: &mut GameState, room: &RoomState) {
        update(game, room);
    }

    #[test]
    fn omodel_attach_resolves_to_the_object_transform() {
        let mut game = GameState::default();
        game.objects.reset(1);
        {
            let record = game.objects.record_mut(0).unwrap();
            record.flag = crate::objects::OBJECT_FLAG_ACTIVE;
            record.pos = [100, 200, 300];
            record.rotation = [0, 0x400, 0];
        }
        let record = *game.objects.record(0).unwrap();
        let expected = crate::objects::rotation(&record);
        let matrix = attach_matrix(&game, Attach::Omodel(0));
        assert_eq!(
            matrix,
            std::array::from_fn(|slot| expected[slot / 3][slot % 3] as i16)
        );
        assert_eq!(
            attach_translation(&game, Attach::Omodel(0)),
            [100, 200, 300]
        );

        // An inactive or missing object keeps the identity fallback.
        game.objects.record_mut(0).unwrap().flag = 0;
        assert_eq!(attach_matrix(&game, Attach::Omodel(0)), IDENTITY_MATRIX);
        assert_eq!(attach_translation(&game, Attach::Omodel(0)), [0, 0, 0]);
        assert_eq!(attach_matrix(&game, Attach::Omodel(9)), IDENTITY_MATRIX);
        assert_eq!(attach_translation(&game, Attach::Omodel(9)), [0, 0, 0]);
    }

    #[test]
    fn joint_attach_resolves_to_the_stored_joint_world_matrix() {
        let mut game = GameState::default();
        game.joint_worlds[2] = vec![
            crate::anim::Mat4x3 {
                r: [[4095, 0, 0], [0, 4095, 0], [0, 0, 4095]],
                t: [100, -200, 300],
            },
            crate::anim::Mat4x3 {
                r: [[0, 0, 4095], [0, 4095, 0], [-4095, 0, 0]],
                t: [400, -500, 600],
            },
        ];
        assert_eq!(
            attach_matrix(&game, Attach::Joint(2, 1)),
            [0, 0, 4095, 0, 4095, 0, -4095, 0, 0]
        );
        assert_eq!(
            attach_translation(&game, Attach::Joint(2, 1)),
            [400, -500, 600]
        );

        // A missing slot or joint falls back to identity and zero.
        assert_eq!(attach_matrix(&game, Attach::Joint(3, 0)), IDENTITY_MATRIX);
        assert_eq!(attach_translation(&game, Attach::Joint(3, 0)), [0, 0, 0]);
        assert_eq!(attach_matrix(&game, Attach::Joint(2, 9)), IDENTITY_MATRIX);
        assert_eq!(attach_translation(&game, Attach::Joint(2, 9)), [0, 0, 0]);
    }

    #[test]
    fn next_phase_advances_the_block_and_keeps_yaw_and_light() {
        let blocks = [
            raw_block(2, 0),  // timer refresh, phase 0
            raw_block(13, 0), // count phase
        ];
        let mut game = game_with_row(&blocks);
        let slot = spawn(&mut game, 0, [0, 0, 0], 123, 7);
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.anim_id, 13);
        assert_eq!(effect.yaw, 123);
        assert_eq!(effect.light_factor, 7);
        assert_eq!(effect.anim_block, 1);
    }

    #[test]
    fn set_frame_snaps_to_the_header_entry() {
        let mut bytes = raw_block(31, 0);
        bytes[4] = 2; // anim_header[0] = frame-table index
        let mut game = game_with_row(&[bytes]);
        game.weapon_effects.sprites[0].info.frames = vec![
            FrameEntry {
                uv_index: 0,
                delay: 4,
                width: 8,
                height: 8,
            },
            FrameEntry {
                uv_index: 1,
                delay: 4,
                width: 8,
                height: 8,
            },
            FrameEntry {
                uv_index: 2,
                delay: 4,
                width: 24,
                height: 32,
            },
        ];
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.frame_entry, 2);
        assert_eq!(effect.frame_index, 2);
        assert_eq!(effect.size, [24, 32]);
    }

    #[test]
    fn kill_frees_the_slot_immediately() {
        let mut game = game_with_row(&[raw_block(47, 0)]);
        spawn(&mut game, 0, [0, 0, 0], 0, 0);
        assert_eq!(game.effects.active_count(), 1);
        run(&mut game, &room());
        assert_eq!(game.effects.active_count(), 0);
        assert_eq!(game.effects.free_slots(), EFFECT_POOL_SIZE as u8);
    }

    #[test]
    fn spawn_from_header_creates_a_child_at_the_slot_position() {
        let mut bytes = raw_block(26, 0);
        bytes[4] = 9; // child type
        bytes[5] = 3; // child depth
        let mut game = game_with_row(&[bytes]);
        // A child depth row so the spawn resolves.
        game.weapon_effects.sprites[0].anim.depth_frames[3] = vec![frame(&[raw_block(1, 0)])];
        let slot = spawn(&mut game, 0, [100, 200, 300], 0, 0);
        run(&mut game, &room());
        assert_eq!(game.effects.active_count(), 2);
        let child = game.last_effect_spawn.expect("child spawned");
        assert_ne!(child, slot);
        let effect = game.effects.slot(usize::from(child)).unwrap();
        assert_eq!(effect.effect_type, 9);
        assert_eq!(effect.depth_group, 3);
        // The parent has no transform bit, so its world position is still the
        // zeroed slot position when the spawn runs.
        assert_eq!(effect.spawn_pos, [0, 0, 0]);
    }

    #[test]
    fn system_flag_switches_to_the_inert_pose() {
        let mut bytes = raw_block(51, 0);
        bytes[4] = 1; // frame entry to snap to
        let mut game = game_with_row(&[bytes]);
        game.weapon_effects.sprites[0].info.frames.push(FrameEntry {
            uv_index: 0,
            delay: 0,
            width: 8,
            height: 8,
        });
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        assert_eq!(game.effects.slot(usize::from(slot)).unwrap().anim_id, 51);
        game.apply_flag(BANK_SYSTEM, 0, 0);
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.anim_id, 1);
        assert_eq!(effect.frame_entry, 1);
    }

    #[test]
    fn velocity_integration_uses_the_accumulated_header() {
        let mut bytes = raw_block(1, 0);
        bytes[14] = 0x02; // transform bit
        bytes[8..10].copy_from_slice(&10i16.to_le_bytes());
        let mut game = game_with_row(&[bytes]);
        let slot = spawn(&mut game, 0, [100, 200, 300], 0, 0);
        run(&mut game, &room());
        assert_eq!(
            game.effects.slot(usize::from(slot)).unwrap().pos,
            [100, 200, 300]
        );
        run(&mut game, &room());
        // The trig table saturates one below full scale, so the yaw matrix
        // scales by 4095/4096 before the identity apply.
        assert_eq!(
            game.effects.slot(usize::from(slot)).unwrap().pos,
            [109, 200, 300]
        );
    }

    #[test]
    fn transform_flags_force_the_height() {
        let mut bytes = raw_block(1, 0);
        bytes[14] = 0x02 | 0x04; // transform + y = 0
        let mut game = game_with_row(&[bytes]);
        let slot = spawn(&mut game, 0, [0, 250, 0], 0, 0);
        run(&mut game, &room());
        assert_eq!(game.effects.slot(usize::from(slot)).unwrap().pos[1], 0);

        let mut bytes = raw_block(1, 0);
        bytes[14] = 0x02 | 0x08; // transform + player y
        let mut game = game_with_row(&[bytes]);
        game.entities[0].pos = [0, 777, 0];
        let slot = spawn(&mut game, 0, [0, 250, 0], 0, 0);
        run(&mut game, &room());
        assert_eq!(game.effects.slot(usize::from(slot)).unwrap().pos[1], 777);
    }

    #[test]
    fn type_one_refreshes_the_attach_transform() {
        let mut bytes = raw_block(1, 0);
        bytes[14] = 0x02;
        let mut game = game_with_row(&[bytes]);
        game.entities[0].angle = 0x400;
        let slot = create(&mut game, &RoomEffects::default(), 9, 0, 1, [0, 0, 0], 0, 0).unwrap();
        run(&mut game, &room());
        let transform = game.effects.slot(usize::from(slot)).unwrap().transform;
        assert_ne!(transform, IDENTITY_MATRIX);

        if let Some(effect) = game.effects.slot_mut(usize::from(slot)) {
            effect.active = 2;
            effect.transform = IDENTITY_MATRIX;
        }
        run(&mut game, &room());
        assert_eq!(
            game.effects.slot(usize::from(slot)).unwrap().transform,
            IDENTITY_MATRIX
        );
    }

    #[test]
    fn animation_delay_counts_down_and_the_zero_entry_frees() {
        let mut bytes = raw_block(1, 0);
        bytes[14] = 0x01; // animate
        let mut game = game_with_row(&[bytes]);
        game.weapon_effects.sprites[0].info.frames = vec![
            FrameEntry {
                uv_index: 0,
                delay: 0,
                width: 8,
                height: 8,
            },
            FrameEntry {
                uv_index: 1,
                delay: 1,
                width: 8,
                height: 8,
            },
            FrameEntry {
                uv_index: 0,
                delay: 0,
                width: 0,
                height: 0,
            },
        ];
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.frame_entry, 1);
        assert_eq!(effect.frame_index, 1);
        assert_eq!(effect.frame_delay, 0);
        run(&mut game, &room());
        assert_eq!(game.effects.active_count(), 0);
    }

    #[test]
    fn the_ff_delay_jumps_through_the_entry_index() {
        let mut bytes = raw_block(1, 0);
        bytes[14] = 0x01;
        let mut game = game_with_row(&[bytes]);
        game.weapon_effects.sprites[0].info.frames = vec![
            FrameEntry {
                uv_index: 0,
                delay: 0,
                width: 8,
                height: 8,
            },
            FrameEntry {
                uv_index: 3,
                delay: 0xFF,
                width: 8,
                height: 8,
            },
            FrameEntry {
                uv_index: 2,
                delay: 2,
                width: 8,
                height: 8,
            },
            FrameEntry {
                uv_index: 4,
                delay: 5,
                width: 8,
                height: 8,
            },
        ];
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.frame_entry, 3);
        assert_eq!(effect.frame_index, 4);
        assert_eq!(effect.frame_delay, 4);
    }

    #[test]
    fn mass_mask_hides_without_stopping_the_animation() {
        let mut bytes = raw_block(1, 0);
        bytes[14] = 0x01;
        let mut game = game_with_row(&[bytes]);
        game.weapon_effects.sprites[0].info.frames = vec![
            FrameEntry {
                uv_index: 0,
                delay: 3,
                width: 8,
                height: 8,
            },
            FrameEntry {
                uv_index: 1,
                delay: 3,
                width: 8,
                height: 8,
            },
        ];
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        game.effects.modify_flags(0, 0x8000);
        assert!(hidden(game.effects.slot(usize::from(slot)).unwrap()));
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.frame_delay, 2);
        assert!(hidden(effect));
        assert_eq!(game.effects.active_count(), 1);
    }

    #[test]
    fn unimplemented_ids_are_counted_placeholders() {
        let mut game = game_with_row(&[raw_block(45, 0)]);
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        assert_eq!(game.effect_placeholder_hits[45], 1);
        assert_eq!(game.effects.slot(usize::from(slot)).unwrap().anim_id, 45);
    }

    #[test]
    fn message_freeze_pauses_the_behaviours() {
        let mut bytes = raw_block(2, 0);
        bytes[7] = 1;
        bytes[14] = 0x01;
        let mut game = game_with_row(&[bytes]);
        game.weapon_effects.sprites[0].info.frames = vec![FrameEntry {
            uv_index: 0,
            delay: 3,
            width: 8,
            height: 8,
        }];
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        game.message_flags &= !crate::game::MESSAGE_FLAG_EFFECTS;
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.frame_delay, 3);
        assert_eq!(effect.header[3], 1);
    }

    #[test]
    fn the_implemented_set_matches_the_dispatch_table() {
        // The combat-only entries stay counted placeholders by design.
        let placeholders = [10u8, 37, 43, 45];
        for id in 0..=63u8 {
            assert_eq!(
                implemented(id),
                !placeholders.contains(&id),
                "behaviour {id} tier mismatch"
            );
        }
    }
    #[test]
    fn blink_toggles_the_hidden_bit() {
        let mut game = game_with_row(&[raw_block(14, 0)]);
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        assert!(hidden(game.effects.slot(usize::from(slot)).unwrap()));
        run(&mut game, &room());
        assert!(!hidden(game.effects.slot(usize::from(slot)).unwrap()));
    }

    #[test]
    fn drift_moves_the_local_offset_then_advances() {
        let mut first = raw_block(25, 0);
        first[4] = 3; // +3 * 2
        first[5] = (-2i8) as u8; // -2 * 2
        first[6] = 1; // +1 * 2
        let second = raw_block(13, 0);
        let mut game = game_with_row(&[first, second]);
        let slot = spawn(&mut game, 0, [100, 200, 300], 0, 0);
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.local_offset, [106, 196, 302]);
        assert_eq!(effect.anim_id, 13);
    }

    #[test]
    fn jitter_adds_random_deltas_to_the_flagged_components() {
        let mut bytes = raw_block(24, 0);
        bytes[7] = 0x01; // the X accumulator bit
        let mut game = game_with_row(&[bytes]);
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        let delta = (game.rand_seed % 5) as i16;
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(h16(effect, 0x0c), delta);
        assert_eq!(h16(effect, 0x0e), 0);
    }

    #[test]
    fn bounce_debris_reseeds_the_spread_and_flips_the_factor() {
        let mut bytes = raw_block(17, 0);
        bytes[6] = 3; // anim_header[2], the spread factor
        bytes[7] = 0x7F; // phase
        let mut game = game_with_row(&[bytes]);
        let slot = spawn(&mut game, 0, [100, 200, 300], 0, 0);
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        let low = (game.rand_seed & 3) as i16;
        assert_eq!(h16(effect, 0x0e), (0x23 - low) * 2);
        // The phase byte becomes the negative vertical over -3, and the
        // spread factor flips sign.
        assert_eq!(effect.header[3], (((0x23 - low) * 2) / -3) as u8);
        assert_eq!(effect.header[2], 3u8.wrapping_neg());
    }

    #[test]
    fn clone_copies_the_next_blocks_light_factor() {
        let first = raw_block(18, 0);
        let mut second = raw_block(1, 0);
        second[3] = 0x2A; // the next phase's authored light
        let mut game = game_with_row(&[first, second]);
        let source = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        assert_eq!(
            game.effects.slot(usize::from(source)).unwrap().anim_id,
            1,
            "the source retires to the inert behaviour"
        );
        let clone = game
            .effects
            .active()
            .find(|(index, _)| *index != usize::from(source))
            .expect("the clone claimed a slot")
            .1;
        assert_eq!(clone.anim_id, 1);
        assert_eq!(
            clone.light_factor, 0x2A,
            "the clone must inherit the next block's light factor"
        );
    }

    #[test]
    fn timer_flags_latches_the_player_flags() {
        let mut game = game_with_row(&[raw_block(40, 0)]);
        game.player_flags = 0x60;
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        assert_eq!(
            game.effects.slot(usize::from(slot)).unwrap().header[2],
            0x60
        );
    }

    #[test]
    fn weapon_charge_latches_the_player_flags_and_equipped_item() {
        let mut game = game_with_row(&[raw_block(42, 0)]);
        game.player_flags = 0x60;
        game.equipped = Some(0x0B);
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.header[0], 0x60);
        assert_eq!(effect.header[1], 0x0B);
        assert_eq!(effect.header[3], 1);
    }

    #[test]
    fn autoaim_flash_latches_the_ammo_low_bits() {
        let mut game = game_with_row(&[raw_block(58, 0)]);
        game.equipped = Some(0x0B);
        game.add_item(0x0B, 7);
        let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
        run(&mut game, &room());
        assert_eq!(
            game.effects.slot(usize::from(slot)).unwrap().header[2],
            7 & 3
        );

        let mut empty = game_with_row(&[raw_block(58, 0)]);
        empty.equipped = Some(0x0B);
        let slot = spawn(&mut empty, 0, [0, 0, 0], 0, 0);
        run(&mut empty, &room());
        assert_eq!(empty.effects.slot(usize::from(slot)).unwrap().header[2], 0);
    }

    #[test]
    fn every_implemented_id_dispatches_without_a_placeholder() {
        for id in 0..=63u8 {
            let mut game = game_with_row(&[raw_block(id, 0)]);
            let slot = spawn(&mut game, 0, [0, 0, 0], 0, 0);
            run(&mut game, &room());
            let counted = game.effect_placeholder_hits[usize::from(id)] > 0;
            assert_eq!(counted, !implemented(id), "behaviour {id} tier mismatch");
            assert!(
                game.effects.slot(usize::from(slot)).is_some(),
                "behaviour {id} left no slot"
            );
        }
    }

    #[test]
    fn reproject_rebuilds_the_screen_under_the_new_camera() {
        let mut room = RoomState {
            cuts: vec![
                Cut {
                    index: 0,
                    pos: [0, 0, 0],
                    look_at: [0, 0, 1000],
                    fov: 200,
                    ..Cut::default()
                },
                Cut {
                    index: 1,
                    pos: [2000, 0, 0],
                    look_at: [2000, 0, 1000],
                    fov: 200,
                    ..Cut::default()
                },
            ],
            ..RoomState::default()
        };
        let mut bytes = raw_block(1, 0);
        bytes[14] = 0x02; // transform bit, so the slot projects
        let mut game = game_with_row(&[bytes]);
        let slot = spawn(&mut game, 0, [1000, 0, 500], 0, 0);
        run(&mut game, &room);
        let before = game.effects.slot(usize::from(slot)).unwrap().screen;

        room.current_cut = 1;
        let camera = Camera::from_cut(&room.cuts[1]);
        let expected = project(&camera, room.cuts[1].fov, [1000, 0, 500]);
        reproject(&mut game, &room);

        let effect = game.effects.slot(usize::from(slot)).unwrap();
        assert_eq!(effect.screen, [expected.0, expected.1]);
        assert_ne!(effect.screen, before, "the cut move must change the screen");
    }
}
