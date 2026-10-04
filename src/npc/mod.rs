//! Scripted-character (NPC) driver and model metadata.
//!
//! Entity ids `0x20..=0x2E` are the game's human characters. [`data`] holds
//! their model paths, collision radii and behaviour dispatch tables, [`idle`]
//! their state-1 behaviours plus the state-0 spawn init, and [`anim`] the
//! per-entity animation clock that publishes the pending frame into the entity
//! words the scripts and behaviours read. [`update_all`] runs the driver for
//! every active entity slot and is called by the room tick after the event VM.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::emd;
use crate::game::{ENTITY_COUNT, ENTITY_STATUS_ACTIVE, GameState};
use crate::model::{Clip, Emd};
use crate::pack::Pack;
use crate::state::RoomState;

pub mod anim;
pub mod data;
pub mod idle;
pub mod scd;
pub mod walk;

pub use anim::EntityAnim;
pub use data::{
    FIRST_ID, LAST_ID, RADIUS_LARGE, RADIUS_MEDIUM, RADIUS_SPRAWLED, character_name,
    collision_radius, model_path, shadow_tint,
};

/// First entity id backed by an `npc/*.emd` model.
pub const CHARACTER_ID_MIN: u8 = data::FIRST_ID;
/// Last entity id backed by an `npc/*.emd` model.
pub const CHARACTER_ID_MAX: u8 = data::LAST_ID;

/// Y offset the look-at target is aimed at on an entity target, below its
/// origin: the original shifts the target's Y by `-0xA28` (head height).
const LOOK_AT_HEAD_HEIGHT: i32 = 0xA28;

/// Clip blend step the state-0 init and the state-1 idle behaviours pass to
/// the animation clock (the `0x400` the original's handlers use).
const IDLE_BLEND_STEP: u16 = 0x400;

/// Parsed NPC (scripted character) models, keyed by entity id.
///
/// The cache loads `npc/{id:02x}.emd` from the pack on first use and keeps the
/// parsed model for the rest of the session, so every entity with the same id
/// shares one copy. A missing or invalid entry is logged once and remembered
/// as absent: the character still exists and its scripts still run, it just has
/// no clips to animate with and no mesh to draw.
#[derive(Default)]
pub struct EntityModelCache {
    pub(crate) models: HashMap<u8, Arc<Emd>>,
    pub(crate) missing: HashSet<u8>,
}

impl EntityModelCache {
    /// The parsed model for character `id`, loading it from the pack on first
    /// use. Ids outside the character range yield `None` without a warning.
    pub fn get(&mut self, pack: &Pack, id: u8) -> Option<Arc<Emd>> {
        if let Some(model) = self.models.get(&id) {
            return Some(Arc::clone(model));
        }
        if self.missing.contains(&id) {
            return None;
        }
        let Some(path) = data::model_path(id) else {
            self.missing.insert(id);
            return None;
        };
        let bytes = match pack.read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("warning: missing NPC model {path}: {err}");
                self.missing.insert(id);
                return None;
            }
        };
        match emd::parse(bytes) {
            Ok(model) => {
                let model = Arc::new(model);
                self.models.insert(id, Arc::clone(&model));
                Some(model)
            }
            Err(err) => {
                eprintln!("warning: invalid NPC model {path}: {err:#}");
                self.missing.insert(id);
                None
            }
        }
    }
}

/// One native update per active entity slot, called after the event VM and
/// before the player's physics mirror. A displayed message that masks the
/// entity-think bit pauses the characters with the room. Returns the number of
/// slots visited.
pub fn update_all(
    game: &mut GameState,
    room: &RoomState,
    models: &mut EntityModelCache,
    pack: &Pack,
) -> usize {
    let mut updated = 0;
    for slot in 1..ENTITY_COUNT {
        if !game.entities[slot].active() {
            continue;
        }
        let target = live_look_at_target(game, slot);
        let model = models.get(pack, game.entities[slot].id);
        let clips: &[Clip] = model.as_ref().map_or(&[], |model| &model.clips);
        update_entity(game, slot, room, clips, target);
        updated += 1;
    }
    updated
}

/// One entity's driver tick: the state dispatch plus the shared tail.
///
/// State 0 is the spawn init, state 1 the idle behaviours, state 8 the
/// scripted-action handlers and state 9 the follow driver (slice 5). `clips`
/// are the entity's EMD animation clips and `look_at_target` the live position
/// an entity-target `act_motion` is aiming at, when one is set.
pub fn update_entity(
    game: &mut GameState,
    slot: usize,
    room: &RoomState,
    clips: &[Clip],
    look_at_target: Option<[i32; 3]>,
) {
    let advance = match game.entities[slot].state() {
        0 => {
            idle::init(
                &mut game.entities[slot],
                &mut game.entity_anims[slot],
                clips,
            );
            false
        }
        1 => idle::update(
            &mut game.entities[slot],
            &mut game.entity_anims[slot],
            clips,
        ),
        8 => {
            scd::update(game, slot, room, clips);
            false
        }
        // The follow driver lands in the next slice; the slots stay inert so
        // their scripts can still select the entities.
        9 => false,
        _ => false,
    };
    if advance {
        game.entity_anims[slot].advance(&mut game.entities[slot], clips, false, IDLE_BLEND_STEP);
    }

    // Common tail. The look-at slew itself belongs to the action/walk layer;
    // the target an `act_motion` latched is already refreshed from its live
    // entity here.
    if let Some(target) = look_at_target {
        game.entities[slot].target = target;
    }
    game.entities[slot].has_enter_switch_zone = u8::from(in_camera_zone(
        room,
        room.current_cut,
        game.entities[slot].pos,
    ));
    game.entities[slot].status_flags |= ENTITY_STATUS_ACTIVE;
}

/// Whether `pos` lies inside the current camera's switch zone.
///
/// This is the room's zone test the player's ground shadow already uses: the
/// first zone whose `cam_from` names `camera` is that camera's header quad.
/// An entity outside it is not drawn by the cut that owns it.
pub fn in_camera_zone(room: &RoomState, camera: usize, pos: [i32; 3]) -> bool {
    room.zones
        .iter()
        .find(|zone| zone.cam_from >= 0 && zone.cam_from as usize == camera)
        .is_some_and(|zone| zone.contains(pos[0], pos[2]))
}

/// The look-at target to refresh this tick, or `None` when the entity's
/// look-at is not tracking a live entity. Bit `0x80` of `look_at_flags` asks
/// for the reload; the target's Y is biased down to head height.
fn live_look_at_target(game: &GameState, slot: usize) -> Option<[i32; 3]> {
    let entity = &game.entities[slot];
    if entity.look_at_flags & 0x80 == 0 {
        return None;
    }
    let target = game.entities.get(usize::from(entity.target_entity?))?;
    Some([
        target.pos[0],
        target.pos[1] - LOOK_AT_HEAD_HEIGHT,
        target.pos[2],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_look_at_target_follows_the_named_entity() {
        let mut game = GameState::default();
        game.entities[1].look_at_flags = 0x80;
        game.entities[1].target_entity = Some(3);
        game.entities[3].pos = [100, 50, -20];
        assert_eq!(live_look_at_target(&game, 1), Some([100, 50 - 0xA28, -20]));

        game.entities[1].look_at_flags = 0;
        assert_eq!(live_look_at_target(&game, 1), None);

        game.entities[1].look_at_flags = 0x80;
        game.entities[1].target_entity = Some(0xFE);
        assert_eq!(live_look_at_target(&game, 1), None);
    }

    #[test]
    fn camera_zone_test_uses_the_current_camera_header() {
        use crate::state::Zone;
        let room = RoomState {
            zones: vec![Zone {
                cam_from: 1,
                cam_to: 0,
                corners: [[0, 0], [0, 100], [100, 100], [100, 0]],
            }],
            ..RoomState::default()
        };
        assert!(in_camera_zone(&room, 1, [50, 0, 50]));
        assert!(!in_camera_zone(&room, 1, [150, 0, 50]));
        assert!(!in_camera_zone(&room, 0, [50, 0, 50]));
    }
}
