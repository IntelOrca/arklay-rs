//! Character lookup tables: models, names, collision radii and the idle
//! behaviour dispatch.
//!
//! Entity ids `0x20..=0x2E` are the game's human characters. Their EMD models
//! are packed raw under `npc/{id:02x}.emd`, each carries the collision radius
//! the room collision pass reads while the character walks, and each falls into
//! one of three shadow-tint classes. The idle behaviour byte selects one of the
//! state-1 handlers from [`IdleBehavior`].

use crate::game::{FLAG_BANK_COUNT, FlagBank};

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

/// One character's SCA collision volume: the entity-local point the cylinder
/// is centred on, its half-height and its XZ radius.
///
/// The original's per-character `Sca_info` records (`0x004c2bb0`, 16 bytes
/// each) pack a single volume into the same words as the list terminator: the
/// local point at `+2/+4/+6` (`x`, `y`, `z`), the half-height at `+8` and the
/// radius at `+10`. `ResolveEntityScaCollision` reads the point (rotated into
/// world space by `SetEntityScaHitData`) and the two extents; the flat
/// `collision_radius` is the same record's `+10`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaVolume {
    /// Entity-local centre of the cylinder; Y is the height offset.
    pub offset: [i16; 3],
    /// Half-height of the cylinder.
    pub half_height: i16,
    /// Cylinder radius, the same value as [`collision_radius`].
    pub radius: i16,
}

/// The standing characters' volume: centred 0x5FA below the origin, the
/// radius depending on the model class.
const SCA_STANDING_LARGE: ScaVolume = ScaVolume {
    offset: [0, -0x5FA, 0],
    half_height: 0x5FA,
    radius: RADIUS_LARGE as i16,
};
/// The standing female models (Jill and Rebecca).
const SCA_STANDING_MEDIUM: ScaVolume = ScaVolume {
    offset: [0, -0x5FA, 0],
    half_height: 0x5FA,
    radius: RADIUS_MEDIUM as i16,
};
/// Kenneth/Forest's corpse: a short volume over the sprawled body.
const SCA_SPRAWLED: ScaVolume = ScaVolume {
    offset: [0x258, -0xB4, -0xC8],
    half_height: 0xB4,
    radius: RADIUS_SPRAWLED as i16,
};
/// Enrico's corpse: the same short volume without the Z offset.
const SCA_SPRAWLED_ENRICO: ScaVolume = ScaVolume {
    offset: [0x258, -0xB4, 0],
    half_height: 0xB4,
    radius: RADIUS_SPRAWLED as i16,
};

/// The per-character SCA volumes, indexed by `id - FIRST_ID`. The Barry,
/// Rebecca and Wesker cutscene aliases reuse their base character's record,
/// exactly like the original's init handlers.
const SCA_VOLUMES: [ScaVolume; 15] = [
    SCA_STANDING_LARGE,  // Chris
    SCA_STANDING_MEDIUM, // Jill
    SCA_STANDING_LARGE,  // Barry
    SCA_STANDING_MEDIUM, // Rebecca
    SCA_STANDING_LARGE,  // Wesker
    SCA_STANDING_LARGE,  // Kenneth corpse
    SCA_SPRAWLED,        // Forest corpse
    SCA_STANDING_LARGE,  // Richard
    SCA_SPRAWLED_ENRICO, // Enrico
    SCA_STANDING_LARGE,  // Kenneth (devoured)
    SCA_STANDING_LARGE,  // Barry 2
    SCA_STANDING_LARGE,  // Barry 2 (Stars)
    SCA_STANDING_MEDIUM, // Rebecca 2 (Stars)
    SCA_STANDING_LARGE,  // Barry 3
    SCA_STANDING_LARGE,  // Wesker 2 (Stars)
];

/// The SCA volume of character `id`, or `None` for non-characters.
pub fn sca_volume(id: u8) -> Option<ScaVolume> {
    SCA_VOLUMES.get(index(id)?).copied()
}

/// The packed `0x00RRGGBB` tint the character's ground shadow blends towards
/// plus the shadow quad's half-extents and the local offset it is built
/// around, indexed by `id - FIRST_ID`.
///
/// The grey `0x808080` of the living characters special-cases to near-black in
/// [`crate::shadow::billboard_tint`]; the corpse props carry warm tints and
/// Richard and Enrico are dimmer. The state-0 init builds each character's
/// shadow from this record, and [`character_shadow`] applies the story-flag
/// overrides on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharacterInit {
    /// Packed `0x00RRGGBB` shadow tint.
    pub tint: u32,
    /// Shadow quad half-extent along the entity's local X.
    pub shadow_half_x: i16,
    /// Shadow quad half-extent along the entity's local Z.
    pub shadow_half_z: i16,
    /// Local offset the shadow quad is built around.
    pub shadow_offset: [i16; 3],
}

/// The living characters' shadow geometry: the 0x200 x 0x280 quad with the
/// small -0x50 Z offset the per-character init handlers pass.
const SHADOW_LIVING: CharacterInit = CharacterInit {
    tint: 0x0080_8080,
    shadow_half_x: 0x200,
    shadow_half_z: 0x280,
    shadow_offset: [0, 0, -0x50],
};

/// Per-character state-0 init data, indexed by `id - FIRST_ID`.
///
/// The Barry, Rebecca and Wesker cutscene aliases reuse their base character's
/// record. The three corpse props and Richard/Enrico restart on animation 0 and
/// carry their own tints and quad sizes.
const CHARACTER_INITS: [CharacterInit; 15] = [
    SHADOW_LIVING, // Chris
    SHADOW_LIVING, // Jill
    SHADOW_LIVING, // Barry
    SHADOW_LIVING, // Rebecca
    SHADOW_LIVING, // Wesker
    CharacterInit {
        // Kenneth corpse
        tint: 0x00FF_FF50,
        shadow_half_x: 500,
        shadow_half_z: 700,
        shadow_offset: [0, 0, 0],
    },
    CharacterInit {
        // Forest corpse
        tint: 0x00FF_FF50,
        shadow_half_x: 700,
        shadow_half_z: 700,
        shadow_offset: [-600, 0, 200],
    },
    CharacterInit {
        // Richard
        tint: 0x0060_6060,
        shadow_half_x: 500,
        shadow_half_z: 700,
        shadow_offset: [0, 0, 0],
    },
    CharacterInit {
        // Enrico
        tint: 0x0040_4040,
        shadow_half_x: 700,
        shadow_half_z: 700,
        shadow_offset: [-600, 0, 200],
    },
    CharacterInit {
        // Kenneth (devoured)
        tint: 0x0060_6060,
        shadow_half_x: 500,
        shadow_half_z: 700,
        shadow_offset: [0, 0, 0],
    },
    SHADOW_LIVING, // Barry 2
    SHADOW_LIVING, // Barry 2 (Stars)
    SHADOW_LIVING, // Rebecca 2 (Stars)
    SHADOW_LIVING, // Barry 3
    SHADOW_LIVING, // Wesker 2 (Stars)
];

/// The state-0 spawn-init data of character `id`, or `None` for non-characters.
pub fn character_init(id: u8) -> Option<CharacterInit> {
    CHARACTER_INITS.get(index(id)?).copied()
}

/// The packed shadow tint of character `id`, or `None` for non-characters.
pub fn shadow_tint(id: u8) -> Option<u32> {
    Some(character_init(id)?.tint)
}

/// The tint the wounded Rebecca and variant Wesker shadows blend towards.
pub const VARIANT_SHADOW_TINT: u32 = 0x00FF_FF70;
/// Half-extent of the wounded Rebecca's resized shadow quad.
pub const REBECCA_WOUNDED_SHADOW_HALF: i16 = 0x1E0;
/// Half-extent of the variant Wesker's enlarged shadow quad.
pub const WESKER_VARIANT_SHADOW_HALF: i16 = 0x5DC;

/// The character's shadow record with the story-flag overrides applied.
///
/// The original's state-0 init handlers call `BillboardSetColor` and
/// `BillboardAdjSize` on the character's shadow after building it: wounded
/// Rebecca (scenario bank 1, bit 0xC0) gets the 0x00FFFF70 tint and a 0x1E0
/// square, and variant Wesker (scenario bank 0, bit 0x37) the same tint at
/// 0x5DC. The port resolves the same overrides where the shadow is queued.
pub fn character_shadow(id: u8, flags: &[FlagBank; FLAG_BANK_COUNT]) -> Option<CharacterInit> {
    let mut init = character_init(id)?;
    if is_rebecca(id) && flags[1].bit(REBECCA_WOUNDED_FLAG) {
        init.tint = VARIANT_SHADOW_TINT;
        init.shadow_half_x = REBECCA_WOUNDED_SHADOW_HALF;
        init.shadow_half_z = REBECCA_WOUNDED_SHADOW_HALF;
    } else if is_wesker(id) && flags[0].bit(WESKER_VARIANT_FLAG) {
        init.tint = VARIANT_SHADOW_TINT;
        init.shadow_half_x = WESKER_VARIANT_SHADOW_HALF;
        init.shadow_half_z = WESKER_VARIANT_SHADOW_HALF;
    }
    Some(init)
}

/// Scenario-flag bit (bank 1, `g_ScenarioFlags2`) that gives Rebecca her
/// wounded/darkened variant: a different opening pose and tinted joints.
pub const REBECCA_WOUNDED_FLAG: u8 = 0xC0;
/// Scenario-flag bit (bank 0, `g_ScenarioFlags`) that gives Wesker his later
/// animation and enlarged shadow.
pub const WESKER_VARIANT_FLAG: u8 = 0x37;
/// Opening clip of Rebecca's wounded variant.
pub const REBECCA_WOUNDED_ANIM: u8 = 0x33;
/// Opening frame of Rebecca's wounded variant.
pub const REBECCA_WOUNDED_FRAME: u8 = 0x3D;
/// Opening clip of Wesker's variant.
pub const WESKER_VARIANT_ANIM: u8 = 0x30;
/// Opening frame of Wesker's variant.
pub const WESKER_VARIANT_FRAME: u8 = 0x6D;
/// Zero-based stage index of the laboratory.
pub const STAGE_LABORATORY_INDEX: u8 = 4;
/// Room id of the lab power room, where Wesker's status bit 1 is forced.
pub const ROOM_POWER_ROOM: u8 = 0x11;

/// Whether `id` is Rebecca (including her cutscene alias).
pub fn is_rebecca(id: u8) -> bool {
    matches!(id, 0x23 | 0x2C)
}

/// Whether `id` is Wesker (including his cutscene alias).
pub fn is_wesker(id: u8) -> bool {
    matches!(id, 0x24 | 0x2E)
}

/// Whether `id` is one of the three corpse props that restart on animation 0.
pub fn is_corpse(id: u8) -> bool {
    matches!(id, 0x25 | 0x26 | 0x29)
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

    fn no_flags() -> [FlagBank; FLAG_BANK_COUNT] {
        [FlagBank::new(); FLAG_BANK_COUNT]
    }

    #[test]
    fn character_shadow_applies_the_story_variant_overrides() {
        // Without the flags the base record is returned unchanged.
        for id in FIRST_ID..=LAST_ID {
            assert_eq!(
                character_shadow(id, &no_flags()),
                character_init(id),
                "id {id:#04x}"
            );
        }

        // Wounded Rebecca: the enemy tint and the 0x1E0 square, on both her
        // ids; Wesker is untouched.
        let mut flags = no_flags();
        flags[1].apply(REBECCA_WOUNDED_FLAG, 0);
        for id in [0x23, 0x2C] {
            let wounded = character_shadow(id, &flags).unwrap();
            assert_eq!(wounded.tint, VARIANT_SHADOW_TINT, "id {id:#04x}");
            assert_eq!(wounded.shadow_half_x, REBECCA_WOUNDED_SHADOW_HALF);
            assert_eq!(wounded.shadow_half_z, REBECCA_WOUNDED_SHADOW_HALF);
            assert_eq!(wounded.shadow_offset, [0, 0, -0x50]);
        }
        assert_eq!(character_shadow(0x24, &flags), character_init(0x24));

        // Variant Wesker: the same tint at 0x5DC, on both his ids.
        let mut flags = no_flags();
        flags[0].apply(WESKER_VARIANT_FLAG, 0);
        for id in [0x24, 0x2E] {
            let variant = character_shadow(id, &flags).unwrap();
            assert_eq!(variant.tint, VARIANT_SHADOW_TINT, "id {id:#04x}");
            assert_eq!(variant.shadow_half_x, WESKER_VARIANT_SHADOW_HALF);
            assert_eq!(variant.shadow_half_z, WESKER_VARIANT_SHADOW_HALF);
        }
        assert_eq!(character_shadow(0x23, &flags), character_init(0x23));

        assert_eq!(character_shadow(FIRST_ID - 1, &no_flags()), None);
    }

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
    fn sca_volumes_carry_the_record_radii_and_heights() {
        for id in FIRST_ID..=LAST_ID {
            let volume = sca_volume(id).unwrap_or_else(|| panic!("id {id:#04x} has no volume"));
            assert_eq!(
                i32::from(volume.radius),
                collision_radius(id).unwrap(),
                "id {id:#04x} radius matches the record"
            );
        }
        // The standing volume sits 0x5FA below the origin with the same
        // half-height; the sprawled bodies use their own offset and 0xB4.
        let chris = sca_volume(0x20).unwrap();
        assert_eq!(chris.offset, [0, -0x5FA, 0]);
        assert_eq!(chris.half_height, 0x5FA);
        let jill = sca_volume(0x21).unwrap();
        assert_eq!(i32::from(jill.radius), RADIUS_MEDIUM);
        assert_eq!(jill.half_height, 0x5FA);
        let forest = sca_volume(0x26).unwrap();
        assert_eq!(forest.offset, [0x258, -0xB4, -0xC8]);
        assert_eq!(forest.half_height, 0xB4);
        let enrico = sca_volume(0x28).unwrap();
        assert_eq!(enrico.offset, [0x258, -0xB4, 0]);
        // Aliases reuse their base character's record.
        assert_eq!(sca_volume(0x2A), Some(sca_volume(0x22).unwrap()));
        assert_eq!(sca_volume(0x2C), Some(sca_volume(0x23).unwrap()));
        assert_eq!(sca_volume(0x2E), Some(sca_volume(0x24).unwrap()));
        assert_eq!(sca_volume(FIRST_ID - 1), None);
        assert_eq!(sca_volume(LAST_ID + 1), None);
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
    fn character_init_carries_the_per_character_shadow_geometry() {
        let living = character_init(0x20).unwrap();
        assert_eq!(living.shadow_half_x, 0x200);
        assert_eq!(living.shadow_half_z, 0x280);
        assert_eq!(living.shadow_offset, [0, 0, -0x50]);
        assert_eq!(character_init(0x2E), Some(living), "Wesker alias");

        let forest = character_init(0x26).unwrap();
        assert_eq!((forest.shadow_half_x, forest.shadow_half_z), (700, 700));
        assert_eq!(forest.shadow_offset, [-600, 0, 200]);
        let enrico = character_init(0x28).unwrap();
        assert_eq!((enrico.shadow_half_x, enrico.shadow_half_z), (700, 700));
        assert_eq!(enrico.shadow_offset, [-600, 0, 200]);

        assert!(is_rebecca(0x23) && is_rebecca(0x2C) && !is_rebecca(0x24));
        assert!(is_wesker(0x24) && is_wesker(0x2E) && !is_wesker(0x23));
        assert!(is_corpse(0x25) && is_corpse(0x26) && is_corpse(0x29));
        assert!(!is_corpse(0x23));
        assert_eq!(character_init(FIRST_ID - 1), None);
        assert_eq!(character_init(LAST_ID + 1), None);
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
