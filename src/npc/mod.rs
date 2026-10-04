//! Scripted-character (NPC) model metadata.
//!
//! Entity ids `0x20..=0x2E` are the game's human characters. Their EMD models
//! are packed raw under `npc/{id:02x}.emd`, and each carries the collision
//! radius the room collision pass reads while the character walks.

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
}
