//! Character lookup tables: models, names, collision radii and the idle
//! behaviour dispatch.
//!
//! Entity ids `0x20..=0x2E` are the game's human characters. Their EMD models
//! are packed raw under `npc/{id:02x}.emd`, each carries the collision radius
//! the room collision pass reads while the character walks, and each falls into
//! one of three shadow-tint classes. The idle behaviour byte selects one of the
//! state-1 handlers from [`IdleBehavior`].

/// First entity id backed by an `npc/*.emd` model.
pub const FIRST_ID: u8 = 0x20;
/// Last entity id backed by an `npc/*.emd` model.
pub const LAST_ID: u8 = 0x2E;

/// Character-model pack paths, indexed by `id - FIRST_ID`.
const MODEL_PATHS: [&str; 15] = [
    "npc/20.emd",
    "npc/21.emd",
    "npc/22.emd",
    "npc/23.emd",
    "npc/24.emd",
    "npc/25.emd",
    "npc/26.emd",
    "npc/27.emd",
    "npc/28.emd",
    "npc/29.emd",
    "npc/2a.emd",
    "npc/2b.emd",
    "npc/2c.emd",
    "npc/2d.emd",
    "npc/2e.emd",
];

/// Character names, indexed by `id - FIRST_ID`; diagnostics only.
const CHARACTER_NAMES: [&str; 15] = [
    "Chris (Stars)",
    "Jill (Stars)",
    "Barry (Stars)",
    "Rebecca (Stars)",
    "Wesker (Stars)",
    "Kenneth corpse",
    "Forest corpse",
    "Richard",
    "Enrico",
    "Kenneth (devoured)",
    "Barry 2",
    "Barry 2 (Stars)",
    "Rebecca 2 (Stars)",
    "Barry 3",
    "Wesker 2 (Stars)",
];

/// Collision radius of the adult male models and Kenneth's corpse.
pub const RADIUS_LARGE: i32 = 422;
/// Collision radius of the female models (Jill and Rebecca).
pub const RADIUS_MEDIUM: i32 = 372;
/// Collision radius of the two sprawled corpses (Forest and Enrico).
pub const RADIUS_SPRAWLED: i32 = 500;

/// The character index of `id`, or `None` outside `FIRST_ID..=LAST_ID`.
fn index(id: u8) -> Option<usize> {
    let index = id.checked_sub(FIRST_ID)?;
    (usize::from(index) < MODEL_PATHS.len()).then_some(usize::from(index))
}

/// The pack path of character `id`'s EMD model, or `None` for non-characters.
pub fn model_path(id: u8) -> Option<&'static str> {
    MODEL_PATHS.get(index(id)?).copied()
}

/// The character name of `id`, or `None` for non-characters.
pub fn character_name(id: u8) -> Option<&'static str> {
    CHARACTER_NAMES.get(index(id)?).copied()
}

/// The collision radius of character `id`, or `None` for non-characters.
///
/// The shipped models fall into three classes: the adult male models (422),
/// Jill and Rebecca including their cutscene aliases (372), and the two
/// sprawled corpses, Forest and Enrico (500).
pub fn collision_radius(id: u8) -> Option<i32> {
    Some(match id {
        0x20 | 0x22 | 0x24 | 0x25 | 0x27 | 0x29 | 0x2A | 0x2B | 0x2D | 0x2E => RADIUS_LARGE,
        0x21 | 0x23 | 0x2C => RADIUS_MEDIUM,
        0x26 | 0x28 => RADIUS_SPRAWLED,
        _ => return None,
    })
}

/// The packed `0x00RRGGBB` tint the character's ground shadow blends towards,
/// indexed by `id - FIRST_ID`.
///
/// The grey `0x808080` of the living characters special-cases to near-black in
/// [`crate::shadow::billboard_tint`]; the corpse props carry warm tints and
/// Richard and Enrico are dimmer. NPC shadows are not drawn yet; the table is
/// the state-0 init data the shadow path will read.
const SHADOW_TINTS: [u32; 15] = [
    0x0080_8080, // Chris
    0x0080_8080, // Jill
    0x0080_8080, // Barry
    0x0080_8080, // Rebecca
    0x0080_8080, // Wesker
    0x00FF_FF50, // Kenneth corpse
    0x00FF_FF50, // Forest corpse
    0x0060_6060, // Richard
    0x0040_4040, // Enrico
    0x0060_6060, // Kenneth (devoured)
    0x0080_8080, // Barry 2
    0x0080_8080, // Barry 2 (Stars)
    0x0080_8080, // Rebecca 2 (Stars)
    0x0080_8080, // Barry 3
    0x0080_8080, // Wesker 2 (Stars)
];

/// The packed shadow tint of character `id`, or `None` for non-characters.
pub fn shadow_tint(id: u8) -> Option<u32> {
    SHADOW_TINTS.get(index(id)?).copied()
}

/// The state-1 idle handler an `action_behavior` byte selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleBehavior {
    /// Behaviour 0: re-dispatch on the entity id; the corpse props play their
    /// ambient animation, everyone else does nothing.
    ById,
    /// Behaviour 1: walk forward until blocked, then knock.
    Walk01,
    /// Behaviour 2: the scripted death with the blood spray (effects absent).
    Walk02,
    /// Behaviour 3: the bleeding-out death facing enemy 1 (effects absent).
    Walk03,
    /// Behaviours 9, 10 and 13: rewind to frame 0 on entry, then play.
    PlayAnim,
    /// Every other behaviour is a no-op.
    Nop,
}

/// The idle handler for `behavior` (`action_behavior`).
pub fn idle_behavior(behavior: u8) -> IdleBehavior {
    match behavior {
        0 => IdleBehavior::ById,
        1 => IdleBehavior::Walk01,
        2 => IdleBehavior::Walk02,
        3 => IdleBehavior::Walk03,
        9 | 10 | 13 => IdleBehavior::PlayAnim,
        _ => IdleBehavior::Nop,
    }
}

/// Whether idle behaviour 0 plays the ambient animation for character `id`.
///
/// The original re-dispatches behaviour 0 through the same overlapping handler
/// block by id; for the shipped characters the corpse props (Kenneth's corpse,
/// Forest's corpse and the devoured Kenneth) land on the play-animation
/// handler, while every living character lands on a no-op.
pub fn idle_0_plays_animation(id: u8) -> bool {
    matches!(id, 0x25 | 0x26 | 0x29)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_paths_cover_the_character_id_range() {
        for id in FIRST_ID..=LAST_ID {
            let path = model_path(id).unwrap_or_else(|| panic!("id {id:#04x} has no path"));
            assert_eq!(path, format!("npc/{id:02x}.emd"));
        }
        assert_eq!(model_path(FIRST_ID - 1), None);
        assert_eq!(model_path(LAST_ID + 1), None);
        assert_eq!(model_path(0x00), None);
        assert_eq!(model_path(0xFF), None);
    }

    #[test]
    fn collision_radii_match_the_three_character_classes() {
        let expected = [
            (0x20u8, RADIUS_LARGE),
            (0x21, RADIUS_MEDIUM),
            (0x22, RADIUS_LARGE),
            (0x23, RADIUS_MEDIUM),
            (0x24, RADIUS_LARGE),
            (0x25, RADIUS_LARGE),
            (0x26, RADIUS_SPRAWLED),
            (0x27, RADIUS_LARGE),
            (0x28, RADIUS_SPRAWLED),
            (0x29, RADIUS_LARGE),
            (0x2A, RADIUS_LARGE),
            (0x2B, RADIUS_LARGE),
            (0x2C, RADIUS_MEDIUM),
            (0x2D, RADIUS_LARGE),
            (0x2E, RADIUS_LARGE),
        ];
        for (id, radius) in expected {
            assert_eq!(collision_radius(id), Some(radius), "id {id:#04x}");
        }
        assert_eq!(collision_radius(FIRST_ID - 1), None);
        assert_eq!(collision_radius(LAST_ID + 1), None);
    }

    #[test]
    fn character_names_cover_the_character_id_range() {
        for id in FIRST_ID..=LAST_ID {
            assert!(character_name(id).is_some(), "id {id:#04x}");
        }
        assert_eq!(character_name(FIRST_ID - 1), None);
        assert_eq!(character_name(LAST_ID + 1), None);
    }

    #[test]
    fn shadow_tints_follow_the_three_character_classes() {
        let tint = |id| shadow_tint(id).unwrap();
        assert_eq!(tint(0x20), 0x0080_8080);
        assert_eq!(tint(0x23), 0x0080_8080);
        assert_eq!(tint(0x25), 0x00FF_FF50);
        assert_eq!(tint(0x26), 0x00FF_FF50);
        assert_eq!(tint(0x27), 0x0060_6060);
        assert_eq!(tint(0x28), 0x0040_4040);
        assert_eq!(tint(0x29), 0x0060_6060);
        for id in [0x2A, 0x2B, 0x2C, 0x2D, 0x2E] {
            assert_eq!(tint(id), 0x0080_8080, "alias {id:#04x}");
        }
        assert_eq!(shadow_tint(FIRST_ID - 1), None);
        assert_eq!(shadow_tint(LAST_ID + 1), None);
    }

    #[test]
    fn idle_dispatch_covers_the_scripted_behaviours() {
        assert_eq!(idle_behavior(0), IdleBehavior::ById);
        assert_eq!(idle_behavior(1), IdleBehavior::Walk01);
        assert_eq!(idle_behavior(2), IdleBehavior::Walk02);
        assert_eq!(idle_behavior(3), IdleBehavior::Walk03);
        assert_eq!(idle_behavior(9), IdleBehavior::PlayAnim);
        assert_eq!(idle_behavior(10), IdleBehavior::PlayAnim);
        assert_eq!(idle_behavior(13), IdleBehavior::PlayAnim);
        for behavior in [4, 5, 6, 7, 8, 11, 12, 14, 15, 16, 23] {
            assert_eq!(idle_behavior(behavior), IdleBehavior::Nop, "{behavior}");
        }
    }

    #[test]
    fn idle_0_plays_only_for_the_corpse_props() {
        for id in FIRST_ID..=LAST_ID {
            let expected = matches!(id, 0x25 | 0x26 | 0x29);
            assert_eq!(idle_0_plays_animation(id), expected, "id {id:#04x}");
        }
    }
}
