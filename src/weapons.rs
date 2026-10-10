//! The player weapon runtime: aim, fire, reload and the held-weapon FX.
//!
//! This module drives the original's aim/fire state machine for the player:
//!
//! - [`WeaponState`] on the player: the aim behavior byte (0x12 aim, 0x13
//!   raise/hold, 0x14 auto-aim fire, 0x15/0x16 quick-fire and recoil recover,
//!   0x17 holster, 0x18 reload, 0x19 empty click, 0x1a lock-on fire) plus the
//!   knife's own 0x14 swing and 0x16 turn, with the aim-direction byte, the
//!   reticle lock and the swing cooldown.
//! - The fire data tables: each weapon's fire frame, sound ids and
//!   muzzle/secondary billboard offsets, the fire end frames, the special
//!   weapons' per-character frame windows and the aim height table. The
//!   converter packs the weapon animation files (`player/w*.emw`) and the
//!   character held-weapon meshes (`player/ws*.tmd`); these tables select the
//!   file for the equipped item and time the FX against the packed clips.
//! - The ammo helpers: the clip (`weapon_autoaim_check`), the reload transfer
//!   from the matching ammunition stack (`fire_consume_ammo_stack`) and the
//!   once-per-trigger volley latch. The equipped weapon's own quantity byte is
//!   the clip; the weapon's ammunition item (`weapon + 9`) is the reserve.
//! - The FX spawns: muzzle billboards at the weapon hand joint, the big flash
//!   on the player matrix and the ejected shell, with the exact per-weapon
//!   offsets, spawn frames and effect-header weapon tags.
//!
//! # Entry and interplay with the locomotion machine
//!
//! The engine calls [`update`] for every unfrozen state-1 tick. When the aim
//! input is held with a weapon equipped, the machine takes the tick; otherwise
//! the call falls through to [`crate::player::update_with_room`], so the
//! locomotion behavior is untouched. While the machine owns the tick the
//! player stands, aims and turns: the original has no aim-walk.
//!
//! # Documented deviations
//!
//! - **The hand joint.** The original composes the weapon hand (joint 14)
//!   through `EntityUpdateWeaponJoint` and anchors the body against the
//!   previous frame's hand world; the port composes the current pose's joints
//!   every tick and attaches the FX to joint 14 directly, which subsumes the
//!   anchor correction.
//! - **The detached magazine.** The beretta reload arms the accessory joint 15
//!   (the ejected magazine physics); the port models the ammo transfer, the
//!   volley latch and the reload cue but not the falling clip mesh.
//! - **The projectile damage.** Only the weapons the original damages at a
//!   fire frame run through [`GameState::apply_weapon_damage_with`] here (the
//!   GL explosive round among them); the flamethrower/acid/flame/rocket
//!   damage rides their projectile effect behaviours, which stay in the
//!   effect pool's placeholder set.
//! - **The player character sounds.** The original's `PlayEntitySnd` aim/cloth
//!   cues are a no-op, like the engine's other player-side cue sites.

use std::rc::Rc;

use crate::effects::{self, Attach};
use crate::game::{BANK_SCENARIO, EntitySound, GameState};
use crate::model::{Clip, Keyframe, Skeleton};
use crate::player::{ClipSource, Input, LockedAction, PlayerState};
use crate::state::RoomState;

/// The infinite rocket launcher's scenario flag.
pub const SCENARIO_FLAG_INF_R_LAUNCHER: u8 = 0x7E;

/// The knife item id.
pub const ITEM_KNIFE: u8 = 1;
/// The flamethrower item id.
pub const ITEM_FLAMETHROWER: u8 = 6;
/// The Ingram's item id (a special weapon).
pub const ITEM_INGRAM: u8 = 0x6F;
/// The Minimi's item id (a special weapon).
pub const ITEM_MINIMI: u8 = 0x70;
/// Weapon id at and above which the row is a "special" (machine gun).
pub const SPECIAL_WEAPON_MIN: u8 = 0x6F;

/// The joint the weapon mesh and the muzzle FX ride (the original's `0xE`).
pub const WEAPON_JOINT: usize = 14;

/// Number of weapon data rows: the ten normal weapons fill 0..=9 (weapon ids
/// 2..=11) and the two specials 12/13 (ids 0x6F/0x70).
pub const WEAPON_ROWS: usize = 14;

/// The player's weapon animation files by character block and weapon id. The
/// entry is the shipped file number (`Wxx.EMW`); 0x00/0x10 are the no-weapon
/// locomotion pair. Block 2 mirrors Chris and block 3 is Rebecca; only 0/1
/// are used by the player.
#[rustfmt::skip]
pub const WEAPON_EMW: [[u8; 14]; 4] = [
    [0x00, 0x01, 0x02, 0x03, 0x04, 0x04, 0x05, 0x06, 0x06, 0x06, 0x07, 0x0B, 0x18, 0x08],
    [0x10, 0x11, 0x12, 0x13, 0x14, 0x14, 0x15, 0x16, 0x16, 0x16, 0x17, 0x1B, 0x18, 0x08],
    [0x00, 0x01, 0x02, 0x03, 0x04, 0x04, 0x05, 0x06, 0x06, 0x06, 0x07, 0x0B, 0x18, 0x08],
    [0x30, 0x11, 0x32, 0x13, 0x14, 0x14, 0x15, 0x16, 0x16, 0x16, 0x17, 0x1B, 0x18, 0x08],
];

/// The five-block character held-weapon mesh table (`WS*.TMD` file numbers)
/// indexed by the entity block and `behavior_flags`. Block 0 Chris, 1 Jill,
/// 2 the fallback, 3 Rebecca, 4 Wesker; a character with behavior 0 holds
/// nothing.
#[rustfmt::skip]
pub const CHARACTER_WEAPON_TMD: [[u16; 7]; 5] = [
    [0x202, 0x202, 0x202, 0x202, 0x202, 0x202, 0x202],
    [0x212, 0x212, 0x212, 0x212, 0x212, 0x212, 0x212],
    [0x224, 0x224, 0x224, 0x224, 0x224, 0x225, 0x225],
    [0x232, 0x232, 0x232, 0x232, 0x232, 0x232, 0x236],
    [0x242, 0x242, 0x242, 0x242, 0x242, 0x242, 0x242],
];

/// One weapon's auto-aim fire data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeaponFireData {
    /// Weapon id passed to the damage pipeline (0 = the swing never damages).
    pub weapon_id: u8,
    /// Animation frame that triggers the damage and the fire sounds.
    pub fire_frame: u8,
    /// First fire sound id.
    pub sfx1: u8,
    /// Second fire sound id.
    pub sfx2: u8,
}

/// The per-weapon fire data, indexed by [`fire_index`].
#[rustfmt::skip]
pub const FIRE_DATA: [WeaponFireData; WEAPON_ROWS] = [
    WeaponFireData { weapon_id:  2, fire_frame:  5, sfx1: 7, sfx2:  8 }, // weapon 2 handgun
    WeaponFireData { weapon_id:  3, fire_frame:  5, sfx1: 7, sfx2:  8 }, // weapon 3 shotgun
    WeaponFireData { weapon_id:  4, fire_frame:  5, sfx1: 7, sfx2:  8 }, // weapon 4 python
    WeaponFireData { weapon_id:  5, fire_frame:  5, sfx1: 7, sfx2:  8 }, // weapon 5 magnum
    WeaponFireData { weapon_id:  6, fire_frame:  0, sfx1: 0, sfx2:  0 }, // weapon 6 flamethrower
    WeaponFireData { weapon_id:  7, fire_frame:  2, sfx1: 7, sfx2:  8 }, // weapon 7 GL explosive
    WeaponFireData { weapon_id:  8, fire_frame:  2, sfx1: 7, sfx2:  8 }, // weapon 8 GL acid
    WeaponFireData { weapon_id:  9, fire_frame:  2, sfx1: 7, sfx2:  8 }, // weapon 9 GL flame
    WeaponFireData { weapon_id: 10, fire_frame:  5, sfx1: 7, sfx2:  8 }, // weapon 10 rocket
    WeaponFireData { weapon_id:  0, fire_frame:  0, sfx1: 0, sfx2:  0 }, // weapon 11 unused
    WeaponFireData { weapon_id:  0, fire_frame:  0, sfx1: 0, sfx2:  0 },
    WeaponFireData { weapon_id:  0, fire_frame:  0, sfx1: 0, sfx2:  0 },
    WeaponFireData { weapon_id:  2, fire_frame:  5, sfx1: 7, sfx2:  8 }, // special 0x6f
    WeaponFireData { weapon_id:  2, fire_frame:  5, sfx1: 7, sfx2:  8 }, // special 0x70
];

/// One billboard spawn record: the animation frame, the effect type/depth and
/// the local offset in the weapon hand's space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeaponFx {
    /// Spawn frame (`0x63` never fires).
    pub frame: u8,
    /// Billboard effect type.
    pub effect_type: u8,
    /// Billboard depth group.
    pub depth: u8,
    /// Local offset X.
    pub x: i16,
    /// Local offset Y.
    pub y: i16,
    /// Local offset Z.
    pub z: i16,
}

impl WeaponFx {
    /// The local offset triple.
    pub fn offset(&self) -> [i32; 3] {
        [i32::from(self.x), i32::from(self.y), i32::from(self.z)]
    }
}

/// The frame value that never fires.
pub const FX_DISABLED: u8 = 0x63;

/// The muzzle billboard table: also carries the ammo-decrement frame.
#[rustfmt::skip]
pub const FIRE_BILLBOARD: [WeaponFx; WEAPON_ROWS] = [
    WeaponFx { frame:  1, effect_type: 17, depth:  0, x:  110, y:   540, z:   0 }, // handgun
    WeaponFx { frame:  1, effect_type: 17, depth:  1, x:  640, y:  1110, z:   0 }, // shotgun
    WeaponFx { frame:  1, effect_type: 17, depth:  2, x:  160, y:   610, z:   0 }, // python
    WeaponFx { frame:  1, effect_type: 17, depth: 10, x:  160, y:   610, z:   0 }, // magnum
    WeaponFx { frame:  0, effect_type:  0, depth:  0, x:    0, y:     0, z:   0 }, // flamethrower
    WeaponFx { frame:  2, effect_type:  8, depth:  7, x:  400, y:   660, z:   0 }, // GL explosive
    WeaponFx { frame:  2, effect_type:  8, depth:  7, x:  400, y:   660, z:   0 }, // GL acid
    WeaponFx { frame:  2, effect_type:  8, depth:  7, x:  400, y:   660, z:   0 }, // GL flame
    WeaponFx { frame:  1, effect_type: 11, depth:  9, x: -190, y:  1020, z:  90 }, // rocket
    WeaponFx { frame:  1, effect_type: 11, depth:  9, x: -190, y:  1020, z: -60 },
    WeaponFx { frame:  1, effect_type: 11, depth:  9, x:  -60, y:  1040, z:  90 },
    WeaponFx { frame:  1, effect_type: 11, depth:  9, x:  -60, y:  1040, z: -60 },
    WeaponFx { frame:  1, effect_type: 17, depth:  0, x:  110, y:   540, z:   0 }, // special 0x6f
    WeaponFx { frame:  1, effect_type: 17, depth:  0, x:  600, y:  1370, z:   0 }, // special 0x70
];

/// The big muzzle-flash table, spawned on the player matrix.
#[rustfmt::skip]
pub const MUZZLE_FLASH: [WeaponFx; WEAPON_ROWS] = [
    WeaponFx { frame:  3, effect_type:  5, depth:  0, x:   370, y: -2870, z: -220 }, // handgun
    WeaponFx { frame: 25, effect_type:  5, depth:  9, x:   360, y: -2050, z: -440 }, // shotgun
    WeaponFx { frame: FX_DISABLED, effect_type: 0, depth: 0, x: 0, y: 0, z: 0 }, // python
    WeaponFx { frame: FX_DISABLED, effect_type: 0, depth: 0, x: 0, y: 0, z: 0 }, // magnum
    WeaponFx { frame:  0, effect_type:  0, depth:  0, x:     0, y:     0, z:    0 }, // flamethrower
    WeaponFx { frame: FX_DISABLED, effect_type: 0, depth: 0, x: 0, y: 0, z: 0 }, // GL explosive
    WeaponFx { frame: FX_DISABLED, effect_type: 0, depth: 0, x: 0, y: 0, z: 0 }, // GL acid
    WeaponFx { frame: FX_DISABLED, effect_type: 0, depth: 0, x: 0, y: 0, z: 0 }, // GL flame
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  1400, y: -2800, z: -300 }, // rocket
    WeaponFx { frame:  0, effect_type:  0, depth:  0, x: 0, y: 0, z: 0 },
    WeaponFx { frame:  0, effect_type:  0, depth:  0, x: 0, y: 0, z: 0 },
    WeaponFx { frame:  0, effect_type:  0, depth:  0, x: 0, y: 0, z: 0 },
    WeaponFx { frame:  3, effect_type:  5, depth:  0, x:   250, y: -1900, z: -250 }, // special 0x6f
    WeaponFx { frame:  3, effect_type:  5, depth:  0, x:   250, y: -1900, z: -250 }, // special 0x70
];

/// The second-flash table, spawned at the weapon hand.
#[rustfmt::skip]
pub const FLASH2: [WeaponFx; WEAPON_ROWS] = [
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  110, y:   500, z:   0 }, // handgun
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  640, y:  1060, z:   0 }, // shotgun
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  160, y:   610, z:   0 }, // python
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  160, y:   610, z:   0 }, // magnum
    WeaponFx { frame:  0, effect_type:  0, depth:  0, x:    0, y:     0, z:   0 }, // flamethrower
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  640, y:  1060, z:   0 }, // GL explosive
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  640, y:  1060, z:   0 }, // GL acid
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  640, y:  1060, z:   0 }, // GL flame
    WeaponFx { frame:  2, effect_type:  8, depth:  2, x:  430, y:  -830, z:  90 }, // rocket
    WeaponFx { frame:  2, effect_type:  8, depth:  2, x:  430, y:  -830, z: -60 },
    WeaponFx { frame:  2, effect_type:  8, depth:  2, x:  570, y:  -810, z:  90 },
    WeaponFx { frame:  2, effect_type:  8, depth:  2, x:  570, y:  -810, z: -60 },
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  110, y:   500, z:   0 }, // special 0x6f
    WeaponFx { frame:  2, effect_type:  9, depth: 11, x:  640, y:  1500, z:   0 }, // special 0x70
];

/// The auto-aim fire end frames; the special path reads `weapon - 99`.
pub const FIRE_END_FRAME: [u8; 16] = [
    0x08, 0x18, 0x0C, 0x0C, 0x00, 0x0D, 0x0D, 0x0D, 0x12, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The special weapons' fire intervals: damage, big flash, second flash.
pub const FIRE_INTERVALS: [u32; 3] = [2, 6, 10];

/// The special weapons' per-(character, motion) frame windows, indexed
/// `(character * 3 + ((motion - 1) & 3)) * 4 + step`.
#[rustfmt::skip]
pub const SPECIAL_FRAME_WINDOWS: [u32; 24] = [
    0, 17, 25, 43,  0,  5, 13, 29,
    0,  5, 13, 29,  0, 15, 25, 43,
    0,  5, 13, 29,  0,  5, 13, 29,
];

/// The per-character aim height pairs (normal / gun / special), in Y units.
#[rustfmt::skip]
pub const AIM_HEIGHT_TABLE: [i16; 12] = [
    -2026, -1656, -2530, -2280, -2040, -1800, // Chris
    -1917, -1617, -2190, -1940, -2003, -1720, // Jill
];

/// The knife swing's per-(character, motion) fire-frame windows.
pub const KNIFE_FIRE_WINDOW: [u8; 12] = [6, 7, 4, 2, 4, 4, 3, 8, 4, 2, 4, 4];

/// The player's live weapon sub-machine. `behavior` is the original's
/// `action_behavior`; 0 means the locomotion machine owns the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WeaponState {
    /// `action_behavior` (0 when inactive).
    pub behavior: u8,
    /// `action_state`.
    pub state: u8,
    /// `weaponAimFlags` for the guns; the knife writes the entity flags byte.
    pub aim_flags: u8,
    /// `weaponAimState`: 0 none, bit 1 armed, bit 0 target locked.
    pub aim_state: u8,
    /// The reticle/lock-on target's entity slot.
    pub target: Option<u8>,
    /// `unk_bf`: the per-state action tick latch.
    pub action_tick: u8,
    /// `attackDirection` reuse: the knife swing cooldown / lock-on countdown.
    pub cooldown: u16,
    /// `g_weaponSpecialFireCountdown`.
    pub special_countdown: i16,
    /// The weapon EMW file id the machine loaded when it started.
    pub weapon_file: Option<u8>,
    /// The equipped item is the knife.
    pub knife: bool,
}

impl WeaponState {
    /// Whether the weapon machine owns the player's tick.
    pub fn active(&self) -> bool {
        self.behavior != 0
    }
}

/// The clip banks the weapon tick may need: the body, the no-weapon and room
/// locomotion banks for the fall-through, and the active weapon's bank.
pub struct WeaponClips<'a> {
    /// The body EMD clips.
    pub emd: &'a [Clip],
    /// The no-weapon locomotion EMW clips.
    pub emw: &'a [Clip],
    /// The room's player-animation clips.
    pub room: &'a [Clip],
    /// The active weapon EMW's clips.
    pub weapon: &'a [Clip],
    /// The body EMD keyframes (the raise-down blend plays a body clip).
    pub emd_keyframes: &'a [Keyframe],
    /// The body EMD skeleton (the held-joint composition).
    pub emd_skeleton: &'a Skeleton,
    /// The active weapon EMW's keyframes.
    pub weapon_keyframes: &'a [Keyframe],
    /// The active weapon EMW's skeleton.
    pub weapon_skeleton: &'a Skeleton,
}

/// The data row index for an item id: normal weapons 2..=11 index 0..=9, the
/// two specials 0x6F/0x70 index 12/13. `None` for the knife, no weapon and
/// out-of-range ids.
pub fn fire_index(weapon: u8) -> Option<usize> {
    match weapon {
        2..=11 => Some(usize::from(weapon) - 2),
        ITEM_INGRAM => Some(12),
        ITEM_MINIMI => Some(13),
        _ => None,
    }
}

/// The ammunition item id for a weapon: `weapon + 9`.
pub fn weapon_ammo_item_id(weapon: u8) -> u8 {
    weapon.wrapping_add(9)
}

/// The weapon EMW file id for a character block and weapon id.
pub fn weapon_emw_id(character: u8, weapon: u8) -> Option<u8> {
    let row = WEAPON_EMW.get(usize::from(character & 3))?;
    match weapon {
        0..=0x0D => row.get(usize::from(weapon)).copied(),
        ITEM_INGRAM => row.get(0x0C).copied(),
        ITEM_MINIMI => row.get(0x0D).copied(),
        _ => None,
    }
}

/// The pack entry of the weapon EMW a character block and weapon id select.
///
/// The no-weapon locomotion pair stays under its `player/{character}.emw`
/// entry; every other file is packed as `player/w{file:02x}.emw`.
pub fn weapon_emw_entry(character: u8, weapon: u8) -> Option<String> {
    let file = weapon_emw_id(character, weapon)?;
    if (character & 1 == 0 && file == 0x00) || (character & 1 == 1 && file == 0x10) {
        Some(format!("player/{:02}.emw", character & 1))
    } else {
        Some(format!("player/w{file:02x}.emw"))
    }
}

/// The held-weapon mesh file id for a scripted character and its behavior
/// byte. `behavior_flags` 0 means the character holds nothing; the block
/// clamps exactly like the original (above 4 folds onto the shared block 2).
pub fn character_weapon_id(id: u8, behavior_flags: u8) -> Option<u16> {
    if behavior_flags == 0 || behavior_flags > 6 {
        return None;
    }
    let mut block = id.wrapping_sub(0x20);
    if block > 4 {
        block = 2;
    }
    Some(CHARACTER_WEAPON_TMD[usize::from(block)][usize::from(behavior_flags)])
}

/// The held-weapon mesh entry for a scripted character and its behavior byte.
pub fn character_weapon_entry(id: u8, behavior_flags: u8) -> Option<String> {
    let file = character_weapon_id(id, behavior_flags)?;
    Some(format!("player/ws{file:03x}.tmd"))
}

/// Whether an item id is a weapon the aim machine accepts.
pub fn is_weapon(item: u8) -> bool {
    item > 0x6E || (item != 0 && item < 0x0B)
}

/// The equipped weapon id, or `None` when nothing/a non-weapon is equipped.
fn equipped_weapon(game: &GameState) -> Option<u8> {
    game.equipped.filter(|&item| is_weapon(item))
}

/// The inventory index of the equipped weapon's stack.
fn equipped_index(game: &GameState, weapon: u8) -> Option<usize> {
    game.inventory.iter().position(|stack| stack.id == weapon)
}

/// `weapon_autoaim_check`: the rounds left in the equipped weapon's clip,
/// masked to the low seven bits. The knife and an absent weapon return 0, the
/// flamethrower returns its full quantity, and an empty clip refills for the
/// infinite rocket launcher and the special machine guns.
pub fn weapon_autoaim_check(game: &mut GameState) -> u8 {
    let Some(weapon) = equipped_weapon(game) else {
        return 0;
    };
    let Some(index) = equipped_index(game, weapon) else {
        return 0;
    };
    let quantity = game.inventory[index].quantity;
    if weapon == ITEM_KNIFE {
        return 0;
    }
    if weapon == ITEM_FLAMETHROWER {
        return quantity;
    }
    if quantity & 0x7F != 0 {
        return quantity & 0x7F;
    }
    let infinite_launcher =
        weapon == 10 && game.flags[usize::from(BANK_SCENARIO)].bit(SCENARIO_FLAG_INF_R_LAUNCHER);
    if infinite_launcher {
        game.inventory[index].quantity = 4;
        return 4;
    }
    if weapon < SPECIAL_WEAPON_MIN {
        return 0;
    }
    game.inventory[index].quantity = 4;
    4
}

/// `fire_ammo_volley_gate`: latch the high bit of the equipped weapon's
/// quantity, true exactly once per trigger pull.
fn fire_ammo_volley_gate(game: &mut GameState) -> bool {
    let Some(weapon) = equipped_weapon(game) else {
        return false;
    };
    let Some(index) = equipped_index(game, weapon) else {
        return false;
    };
    if game.inventory[index].quantity & 0x80 != 0 {
        return false;
    }
    game.inventory[index].quantity |= 0x80;
    true
}

/// `fire_consume_ammo_stack`: move rounds from the largest matching
/// ammunition stack into the equipped weapon's clip, up to the stack maximum.
/// An emptied stack is dropped, the way `rearrange_item_slots` compacts the
/// original's slot list.
pub fn fire_consume_ammo_stack(game: &mut GameState) {
    let Some(weapon) = equipped_weapon(game) else {
        return;
    };
    let ammo = weapon_ammo_item_id(weapon);
    let max_quantity = crate::items::max_quantity(ammo);
    let Some(equipped) = equipped_index(game, weapon) else {
        return;
    };
    let capacity = game.inventory_capacity().min(game.inventory.len());
    let mut best = None;
    let mut best_quantity = 0u8;
    for index in 0..capacity {
        let stack = game.inventory[index];
        if stack.id == ammo && stack.quantity > best_quantity {
            best = Some(index);
            best_quantity = stack.quantity;
        }
    }
    let Some(source) = best else {
        return;
    };
    if max_quantity < best_quantity {
        game.inventory[equipped].quantity = max_quantity;
        game.inventory[source].quantity = best_quantity - max_quantity;
    } else {
        game.inventory[equipped].quantity = best_quantity;
        game.inventory[source].id = 0;
        game.rebuild_slots();
    }
}

/// Queue one weapon sample through the weapon bank: the original restarts the
/// equipped weapon's own bank (`SetSndSlot(handle, 0)`), so the bank key is
/// `(1, weapon)` and the sample restarts on every shot.
fn play_weapon_sound(game: &mut GameState, weapon: u8, id: u8) {
    let Some(name) = crate::sfx::weapon_sound(weapon, id) else {
        return;
    };
    game.entity_sounds.push(EntitySound {
        name,
        bank: 1,
        column: weapon,
        pos: game.entities[0].pos,
    });
}

/// Spawn one billboard attached to the weapon hand.
fn spawn_hand_fx(
    game: &mut GameState,
    effect_type: u8,
    depth: u8,
    offset: [i32; 3],
    yaw: i16,
) -> Option<u8> {
    let room_effects = Rc::clone(&game.room_effects);
    effects::create_attached(
        game,
        &room_effects,
        effect_type,
        depth,
        Attach::Joint(0, WEAPON_JOINT as u8),
        offset,
        yaw,
        0,
    )
}

/// Spawn one billboard on the player's matrix.
fn spawn_player_fx(
    game: &mut GameState,
    effect_type: u8,
    depth: u8,
    offset: [i32; 3],
    yaw: i16,
) -> Option<u8> {
    let room_effects = Rc::clone(&game.room_effects);
    effects::create_attached(
        game,
        &room_effects,
        effect_type,
        depth,
        Attach::Player,
        offset,
        yaw,
        0,
    )
}

/// Tag one spawned effect's animation header the way `fire_fx_tag` does.
fn tag_effect(game: &mut GameState, slot: u8, field: usize, weapon: u8) {
    if let Some(effect) = game.effects.slot_mut(usize::from(slot))
        && let Some(byte) = effect.header.get_mut(field)
    {
        *byte = weapon;
    }
}

/// One tick of the weapon machine, or the locomotion fall-through.
///
/// Called by the engine for every unfrozen state-1 tick in place of
/// [`crate::player::update_with_room`].
pub fn update(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    player.input = input;
    if player.weapon.behavior == 0 && !try_enter(game, player, clips) {
        crate::player::update_with_room(player, room, clips.emd, clips.emw, clips.room, input);
        return;
    }
    if player.weapon.behavior == 0 {
        return;
    }
    tick(game, player, room, clips, input);
}

/// Start the weapon machine when the aim input is held with a weapon ready.
fn try_enter(game: &mut GameState, player: &mut PlayerState, clips: &WeaponClips<'_>) -> bool {
    if !player.input.aim || player.locked != LockedAction::None {
        return false;
    }
    let Some(weapon) = equipped_weapon(game) else {
        return false;
    };
    if clips.weapon.is_empty() {
        return false;
    }
    let character = game.id.player_flag & 3;
    player.weapon = WeaponState {
        behavior: 0x12,
        knife: weapon == ITEM_KNIFE,
        weapon_file: weapon_emw_id(character, weapon),
        ..WeaponState::default()
    };
    player.clip_source = ClipSource::Weapon;
    true
}

/// End the machine and hand control back to the locomotion behavior.
fn holster_done(player: &mut PlayerState) {
    player.weapon = WeaponState::default();
    player.return_from_weapon();
}

/// One tick of the active machine.
fn tick(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    let knife = player.weapon.knife;
    match (knife, player.weapon.behavior) {
        (false, 0x12) => gun_aim(game, player, room, clips, input),
        (false, 0x13) => {
            gun_raise(game, player, clips);
            if player.weapon.active() {
                gun_hold_input(game, player, room, clips, input);
            }
        }
        (false, 0x14) => gun_autoaim(game, player, room, clips, input),
        (false, 0x15) => gun_quick_fire(game, player, clips, false),
        (false, 0x16) => gun_quick_fire(game, player, clips, true),
        (false, 0x17) => gun_holster(game, player, clips, input),
        (false, 0x18) => gun_reload(game, player, clips),
        (false, 0x19) => gun_click(game, player),
        (false, 0x1A) => gun_lockon_fire(game, player, room, clips, input),
        (true, 0x12) => knife_aim(game, player, room, clips, input),
        (true, 0x13) => knife_hold(game, player, room, clips, input),
        (true, 0x14) => knife_swing(game, player, room, clips),
        (true, 0x15) => knife_holster(game, player, clips, input),
        (true, 0x16) => knife_turn(game, player, room, clips, input),
        // A stale behavior after an item swap: release control.
        _ => holster_done(player),
    }
    // The shared frame-3 tail: refresh the weapon hand's world matrix for the
    // effects pass, then the flamethrower's per-frame pitch update.
    if player.weapon.active() {
        refresh_weapon_joint(game, player, clips);
        if !knife && equipped_weapon(game) == Some(ITEM_FLAMETHROWER) {
            auto_aim_pitch(game, player);
        }
    }
}

/// The body/weapon clip bank a `Joint_move` step plays from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bank {
    Weapon,
    Body,
}

/// Compose the player's pose into `joint_worlds[0]` so `Attach::Joint(0, 14)`
/// resolves the weapon hand for this tick's and the effects pass's FX.
fn refresh_weapon_joint(game: &mut GameState, player: &PlayerState, clips: &WeaponClips<'_>) {
    let (bank, keyframes, skeleton) = match player.clip_source {
        ClipSource::Weapon => (clips.weapon, clips.weapon_keyframes, clips.weapon_skeleton),
        _ => (clips.emd, clips.emd_keyframes, clips.emd_skeleton),
    };
    let Some(keyframe) = player.anim.pose_keyframe(bank, keyframes) else {
        return;
    };
    let entity = crate::anim::entity_matrix(player.pos, player.angle);
    let worlds = crate::anim::joint_matrices(skeleton, &keyframe, &entity);
    if !worlds.is_empty() {
        game.joint_worlds[0] = worlds;
    }
}

/// The world Y of the composed pose's joint 11, the auto-aim pitch origin
/// (the original composes joints 0/9/10/11).
fn aim_pitch_y(game: &GameState, player: &PlayerState) -> i32 {
    game.joint_worlds[0]
        .get(11)
        .map_or(player.pos[1], |world| world.t[1])
}

/// `auto_aim_pitch_update`: write the aim direction (0x20 up, 0x40 neutral,
/// 0x80 down) from the hand height against the per-character height table.
fn auto_aim_pitch(game: &mut GameState, player: &PlayerState) {
    let Some(weapon) = equipped_weapon(game) else {
        return;
    };
    let base = game.player_flags & 0x1F;
    game.player_flags = base | 0x40;
    if weapon == 10 {
        return;
    }
    let character = usize::from(game.id.player_flag & 1);
    let t = character * 6;
    let entity_y = player.pos[1] as i16;
    let aim_y = aim_pitch_y(game, player);
    let mut low = i32::from(AIM_HEIGHT_TABLE[t]) + i32::from(entity_y);
    let mut high = i32::from(AIM_HEIGHT_TABLE[t + 1]) + i32::from(entity_y);
    if weapon < 6 && weapon != 3 {
        low = i32::from(AIM_HEIGHT_TABLE[t + 2]) + i32::from(entity_y);
        high = i32::from(AIM_HEIGHT_TABLE[t + 3]) + i32::from(entity_y);
    }
    if weapon > 0x6E {
        low = i32::from(AIM_HEIGHT_TABLE[t + 4]) + i32::from(entity_y);
        high = i32::from(AIM_HEIGHT_TABLE[t + 5]) + i32::from(entity_y);
    }
    if aim_y < low {
        game.player_flags = base | 0x80;
    }
    if high < aim_y {
        game.player_flags = (game.player_flags & 0x1F) | 0x20;
    }
}

/// `weapon_special_frame_update`: clamp the pending frame into the special
/// weapon's per-(character, motion) window. Mode 0 holds at the window end
/// minus one, mode 1 rewinds to the window start. True when the frame was
/// outside the window.
fn special_frame_update(game: &GameState, player: &mut PlayerState, step: usize, mode: u8) -> bool {
    let u3 = (step + 1) & 3;
    let character = usize::from(game.id.player_flag & 1);
    let i = (character * 3 + ((player.anim.clip + 0xFF) & 3)) * 4;
    let frame = player.anim.frame as u32;
    if frame < SPECIAL_FRAME_WINDOWS[step + i] || SPECIAL_FRAME_WINDOWS[i + u3] <= frame {
        if mode == 0 {
            player.anim.frame = (SPECIAL_FRAME_WINDOWS[i + u3] - 1) as usize;
        } else if mode == 1 {
            player.anim.frame = SPECIAL_FRAME_WINDOWS[step + i] as usize;
        }
        return true;
    }
    false
}

/// One `Joint_move` step: advance the selected bank with `step`'s blend and
/// the given direction. Returns the original's completion flag.
fn joint_move(
    player: &mut PlayerState,
    clips: &WeaponClips<'_>,
    bank: Bank,
    step: u16,
    reverse: bool,
) -> bool {
    player.clip_source = match bank {
        Bank::Weapon => ClipSource::Weapon,
        Bank::Body => ClipSource::Emd,
    };
    player.anim.blend_step = step;
    player.anim.reverse = reverse;
    let clip_list = match bank {
        Bank::Weapon => clips.weapon,
        Bank::Body => clips.emd,
    };
    player.anim.update(clip_list)
}

/// Arm a clip switch with the original's blend counter for `step`.
fn set_blend(player: &mut PlayerState, step: u16) {
    player.anim.blend_step = step;
    player.anim.blend_counter = player.anim.full_blend_counter();
}

/// Reset the pending/displayed frame and timing (the handlers' explicit
/// `animation_frame_id = 0`).
fn reset_frame(player: &mut PlayerState) {
    player.anim.frame = 0;
    player.anim.display_frame = 0;
    player.anim.timing = 0;
}

/// Select a weapon motion (the original writing `attackAnim`) without
/// resetting the frame.
fn set_motion(player: &mut PlayerState, motion: usize) {
    player.anim.clip = motion;
}

/// The per-character hold/aim steering step: right is the original's
/// `& 2` branch `(id & 1) * -0x10 + 0x48`, left its `& 8` branch
/// `(id & 1) * 0x10 - 0x48` (the second value is already negative).
fn steer_step(character: u8, right: bool) -> u16 {
    if right {
        (0x48u16).wrapping_sub(u16::from(character) * 0x10)
    } else {
        (u16::from(character) * 0x10).wrapping_sub(0x48)
    }
}

/// The direction term the guns add into the motion id:
/// `(up_bit >> 4) + down_bit`.
fn aim_dir_term(flags: u8) -> u8 {
    ((flags & 0x20) >> 4) + (flags >> 7)
}

/// Emit the equipped weapon's fire sounds.
fn play_fire_sounds(game: &mut GameState, weapon: u8, index: usize) {
    play_weapon_sound(game, weapon, FIRE_DATA[index].sfx1);
    play_weapon_sound(game, weapon, FIRE_DATA[index].sfx2);
}

// ---------------------------------------------------------------------------
// The gun behaviors
// ---------------------------------------------------------------------------

/// `player_behavior_12_gun_aim`: the initial raise pose, the reticle and the
/// quick-fire entry.
fn gun_aim(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    match player.weapon.state {
        0 => {
            player.weapon.state = 1;
            reset_frame(player);
            player.move_speed_current = 1;
            player.weapon.action_tick = 0;
            set_motion(player, 5);
            set_blend(player, 0x400);
            player.weapon.aim_flags = 0;
            match reticle_scan(game, player, room) {
                Some(target) => {
                    player.weapon.target = Some(target);
                    player.weapon.aim_state = 3;
                }
                None => {
                    player.weapon.target = None;
                    player.weapon.aim_state = 2;
                }
            }
        }
        1 => {}
        2 => {
            player.weapon.behavior = 0x13;
            player.weapon.state = 0;
            return;
        }
        _ => return,
    }

    // Reticle follow: a fresh fire press drops the lock; a locked target
    // turns the player toward it.
    if input.fire_pressed {
        player.weapon.aim_state = 0;
    }
    if player.weapon.aim_state == 3
        && let Some(position) = player
            .weapon
            .target
            .and_then(|slot| target_position(game, slot))
    {
        let character = u16::from(game.id.player_flag & 1);
        let mut step = character * 0x20 + 0xF0;
        if turn_toward_target(player, position, 0x200) == 0 {
            step = (character + 6) * 0x20;
        }
        if character == 0 && equipped_weapon(game) == Some(2) {
            step += 0x20;
        }
        rotate_toward_target(player, position, step);
    }

    let done = joint_move(player, clips, Bank::Weapon, 0x400, false);
    player.weapon.state = player.weapon.state.wrapping_add(u8::from(done));

    if input.right {
        player.angle = player
            .angle
            .wrapping_add(steer_step(game.id.player_flag & 1, true))
            & 0x0FFF;
    }
    if input.left {
        player.angle = player
            .angle
            .wrapping_add(steer_step(game.id.player_flag & 1, false))
            & 0x0FFF;
    }
    let weapon = equipped_weapon(game).unwrap_or(0);
    if weapon == 10 {
        return;
    }

    let character = game.id.player_flag & 1;
    let frame = player.anim.frame as u8;
    let ready = (character == 0 && frame > 4 && matches!(weapon, 2 | 4 | 5)) || frame > 9;
    if ready && input.up {
        player.weapon.aim_flags &= 0x1F;
        game.player_flags &= 0x1F;
        player.weapon.aim_flags |= 0x20;
        game.player_flags |= 0x20;
        player.weapon.behavior = 0x15;
        player.weapon.state = 0;
        set_motion(player, if weapon < SPECIAL_WEAPON_MIN { 0x0B } else { 7 });
        return;
    }
    if ready && input.down {
        player.weapon.aim_flags &= 0x1F;
        game.player_flags &= 0x1F;
        player.weapon.aim_flags |= 0x80;
        game.player_flags |= 0x80;
        player.weapon.behavior = 0x15;
        player.weapon.state = 0;
        set_motion(player, if weapon < SPECIAL_WEAPON_MIN { 8 } else { 6 });
        return;
    }
    if weapon > 0x6E && special_frame_update(game, player, 0, 0) {
        player.weapon.state = 2;
    }
}

/// `player_behavior_13_gun_raise`: the raise/hold/lower pose machine.
fn gun_raise(game: &mut GameState, player: &mut PlayerState, clips: &WeaponClips<'_>) {
    if player.weapon.state >= 4 {
        return;
    }
    let weapon = equipped_weapon(game).unwrap_or(0);
    match player.weapon.state {
        0 => {
            reset_frame(player);
            player.weapon.state = 1;
            player.weapon.action_tick = 0;
            let mut motion = usize::from(aim_dir_term(player.weapon.aim_flags)) * 3 + 7;
            if weapon > 0x6E {
                player.anim.frame = 0x0E;
                motion = usize::from(aim_dir_term(player.weapon.aim_flags)) + 5;
            }
            set_motion(player, motion);
            set_blend(player, 0x400);
            player.move_speed_current = 0;
            let _ = joint_move(player, clips, Bank::Weapon, 0x400, false);
        }
        1 => {
            let _ = joint_move(player, clips, Bank::Weapon, 0x400, false);
            if weapon > 0x6E {
                special_frame_update(game, player, 0, 0);
            }
        }
        2 => {
            if player.anim.blend_counter == 0 {
                player.weapon.state = 3;
                player.weapon.action_tick = 0;
                reset_frame(player);
                player.move_speed_current = 1;
                set_blend(player, 0x200);
                let mut motion = usize::from(weapon)
                    .wrapping_mul(3)
                    .wrapping_add(usize::from(aim_dir_term(player.weapon.aim_flags)))
                    .wrapping_add(2);
                if weapon > 0x6E {
                    motion = usize::from(aim_dir_term(player.weapon.aim_flags)) + 5;
                    player.anim.frame = 0x0F;
                    special_frame_update(game, player, 0, 0);
                }
                set_motion(player, motion);
            } else {
                let _ = joint_move(player, clips, Bank::Weapon, 0x200, false);
                if weapon > 0x6E {
                    special_frame_update(game, player, 0, 0);
                }
            }
        }
        3 => {
            if player.anim.blend_counter != 0 {
                if weapon > 0x6E {
                    player.anim.frame = 0x0F;
                    let _ = joint_move(player, clips, Bank::Weapon, 0x200, false);
                    special_frame_update(game, player, 0, 0);
                } else {
                    // The lowering blend plays the body bank at the same
                    // motion id; the frame resets so the body clip can start.
                    let _ = joint_move(player, clips, Bank::Body, 0x200, false);
                }
            } else {
                player.weapon.state = 2;
                reset_frame(player);
                player.weapon.action_tick = 0;
                player.move_speed_current = 1;
                set_blend(player, 0x200);
                let mut motion = usize::from(aim_dir_term(player.weapon.aim_flags)) * 3 + 7;
                if weapon > 0x6E {
                    motion = usize::from(aim_dir_term(player.weapon.aim_flags)) + 5;
                    player.anim.frame = 0x0F;
                    special_frame_update(game, player, 0, 0);
                }
                set_motion(player, motion);
            }
        }
        _ => {}
    }
}

/// `player_behavior_13_gun_hold_input`: the aim-hold input handler.
fn gun_hold_input(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    let old = player.weapon.aim_flags;
    player.weapon.aim_flags = (old & 0x1F) | 0x40;
    if input.down {
        player.weapon.aim_flags = (player.weapon.aim_flags & 0x1F) | 0x80;
    }
    if input.up {
        player.weapon.aim_flags = (player.weapon.aim_flags & 0x1F) | 0x20;
    }
    let weapon = equipped_weapon(game).unwrap_or(0);

    let quick_fire_motion = |flags: u8| -> usize {
        let term = aim_dir_term(flags);
        if weapon < SPECIAL_WEAPON_MIN {
            usize::from(term) * 3 + 5
        } else {
            usize::from(term) + 5
        }
    };

    // A direction change quick-fires in the new direction.
    if (player.weapon.aim_flags ^ old) & 0xA0 != 0 {
        player.weapon.behavior = 0x15;
        player.weapon.state = 0;
        set_motion(player, quick_fire_motion(player.weapon.aim_flags));
    }
    if old & 0x40 != 0 && player.weapon.aim_flags & 0xA0 != 0 {
        player.weapon.behavior = 0x15;
        player.weapon.state = 0;
        set_motion(player, quick_fire_motion(player.weapon.aim_flags));
    }
    if old & 0xA0 != 0 && player.weapon.aim_flags & 0x40 != 0 {
        player.weapon.behavior = 0x16;
        player.weapon.state = 0;
        let term = ((old >> 5) & 1) * 2 + ((old >> 7) & 1);
        let motion = if weapon < SPECIAL_WEAPON_MIN {
            usize::from(term) * 3 + 5
        } else {
            usize::from(aim_dir_term(player.weapon.aim_flags)) + 5
        };
        set_motion(player, motion);
    }

    if !input.aim {
        player.weapon.behavior = 0x17;
        player.weapon.state = 0;
        return;
    }

    if input.fire {
        if weapon_autoaim_check(game) != 0 {
            player.weapon.behavior = 0x14;
            player.weapon.state = if weapon == 6 || weapon >= SPECIAL_WEAPON_MIN {
                3
            } else {
                0
            };
            return;
        }
        if input.fire_pressed {
            play_weapon_sound(game, weapon, 9);
            if weapon < 6 && game.has_item(weapon_ammo_item_id(weapon)) {
                player.weapon.behavior = 0x18;
                player.weapon.state = 0;
                return;
            }
        }
    }
    if input.fire && player_find_aim_target(game, player, room) {
        player.weapon.behavior = 0x1A;
        player.weapon.state = 0;
        return;
    }

    if input.right {
        player.angle = player
            .angle
            .wrapping_add(steer_step(game.id.player_flag & 1, true))
            & 0x0FFF;
        if player.weapon.state < 2 {
            player.weapon.state = 2;
            player.anim.blend_counter = 0;
        }
        return;
    }
    if input.left {
        player.angle = player
            .angle
            .wrapping_add(steer_step(game.id.player_flag & 1, false))
            & 0x0FFF;
        if player.weapon.state < 2 {
            player.weapon.state = 2;
            player.anim.blend_counter = 0;
        }
        return;
    }
    if player.weapon.state > 1 {
        player.weapon.state = 0;
    }
    let _ = clips;
}

/// `player_behavior_14_autoaim`: the auto-aim fire state machine.
fn gun_autoaim(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    match player.weapon.state {
        0 => autoaim_raise(game, player, room, clips),
        1 => autoaim_fire(game, player, room, clips, input),
        2 => {
            player.weapon.behavior = 0x13;
            player.weapon.state = 0;
            let weapon = equipped_weapon(game).unwrap_or(0);
            if let Some(index) = equipped_index(game, weapon)
                && game.inventory[index].quantity & 0x7F == 0
            {
                play_weapon_sound(game, weapon, 9);
            }
        }
        3 => {
            let weapon = equipped_weapon(game).unwrap_or(0);
            let term = aim_dir_term(player.weapon.aim_flags);
            reset_frame(player);
            player.weapon.action_tick = 0;
            player.weapon.state = 4;
            let mut motion = usize::from(term.wrapping_add(2)) * 3;
            if weapon > 0x6E {
                motion = usize::from(term) + 5;
                special_frame_update(game, player, 1, 1);
            }
            set_motion(player, motion);
            set_blend(player, 0x400);
            player.move_speed_current = 1;
            autoaim_raise2(game, player, clips);
        }
        4 => autoaim_raise2(game, player, clips),
        5 => {
            let weapon = equipped_weapon(game).unwrap_or(0);
            let term = aim_dir_term(player.weapon.aim_flags);
            player.weapon.state = 6;
            player.weapon.action_tick = 0;
            if weapon < SPECIAL_WEAPON_MIN {
                reset_frame(player);
                set_motion(player, usize::from(term) + 0x0E);
            } else {
                set_motion(player, usize::from(term) + 5);
            }
            set_blend(player, 0x400);
            autoaim_holdfire(game, player, room, clips, input);
        }
        6 => autoaim_holdfire(game, player, room, clips, input),
        7 => {}
        8 => {
            let weapon = equipped_weapon(game).unwrap_or(0);
            player.weapon.state = 9;
            if weapon < SPECIAL_WEAPON_MIN {
                reset_frame(player);
            }
            player.weapon.action_tick = 0;
            set_blend(player, 0x400);
            play_weapon_sound(game, weapon, 6);
            autoaim_reverse(game, player, clips, input);
        }
        9 => autoaim_reverse(game, player, clips, input),
        10 => player.weapon.state = 5,
        _ => {}
    }
}

/// The auto-aim raise pose and its lock-on fan.
fn autoaim_raise(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
) {
    player.weapon.state = 1;
    reset_frame(player);
    player.move_speed_current = 1;
    player.weapon.action_tick = 0;
    set_blend(player, 0x400);
    let term = aim_dir_term(player.weapon.aim_flags);
    set_motion(player, usize::from(term.wrapping_add(2)) * 3);
    weapon_lockon_effect(game, player, room);
    autoaim_fire(game, player, room, clips, player.input);
}

/// The fire frame: the damage, the ammo decrement and the flash spawns.
fn autoaim_fire(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    let weapon = equipped_weapon(game).unwrap_or(0);
    let Some(index) = fire_index(weapon) else {
        return;
    };
    let done = joint_move(player, clips, Bank::Weapon, 0x400, false);
    player.weapon.state = player.weapon.state.wrapping_add(u8::from(done));
    let frame = player.anim.frame as u8;
    if frame == 2 {
        // The pitch reads the hand chain's height; compose the pose the tick
        // just applied before sampling it.
        refresh_weapon_joint(game, player, clips);
        auto_aim_pitch(game, player);
    }

    if FIRE_BILLBOARD[index].frame == frame {
        if let Some(slot) = equipped_index(game, weapon) {
            game.inventory[slot].quantity = game.inventory[slot].quantity.wrapping_sub(1);
        }
        if index == 8 {
            let ammo = weapon_autoaim_check(game);
            let entry = usize::from(ammo & 3) + index;
            let row = FIRE_BILLBOARD[entry];
            spawn_hand_fx(game, row.effect_type, row.depth, row.offset(), 0);
        } else {
            let row = FIRE_BILLBOARD[index];
            spawn_hand_fx(game, row.effect_type, row.depth, row.offset(), 0);
            if index == 2 {
                spawn_hand_fx(game, 0x11, 0x03, [0x96, 0x17C, 0], 0);
            }
            if index == 3 {
                spawn_hand_fx(game, 0x11, 0x0B, [0x96, 0x17C, 0], 0);
            }
        }
    }

    if FIRE_DATA[index].fire_frame == frame {
        if index < 6 {
            let weapon_id = FIRE_DATA[index].weapon_id;
            let flags = game.player_flags;
            let origin = player.pos;
            game.apply_weapon_damage_with(room, weapon_id, origin, flags);
        }
        play_fire_sounds(game, weapon, index);
    }

    if MUZZLE_FLASH[index].frame == frame {
        let row = MUZZLE_FLASH[index];
        let character = game.id.player_flag & 1;
        let mut y_off = (1i32 - index as i32) * i32::from(character) * 300;
        if index == 8 {
            y_off = i32::from(character) * 500;
        }
        let mut yaw = index as i32 - 8;
        if (yaw as u16) < 1 {
            yaw += 1;
        }
        yaw -= 1;
        let offset = [i32::from(row.x), y_off + i32::from(row.y), i32::from(row.z)];
        if let Some(slot) = spawn_player_fx(
            game,
            row.effect_type,
            row.depth,
            offset,
            (yaw as i16) & 0x555,
        ) {
            tag_effect(game, slot, if index == 8 { 0 } else { 3 }, index as u8);
        }
    }

    if FLASH2[index].frame == frame {
        let slot = if index == 8 {
            let ammo = weapon_autoaim_check(game);
            let entry = usize::from(ammo & 3) + index;
            let row = FLASH2[entry];
            spawn_hand_fx(game, row.effect_type, row.depth, row.offset(), 0)
        } else {
            let row = FLASH2[index];
            spawn_hand_fx(game, row.effect_type, row.depth, row.offset(), 0)
        };
        if let Some(slot) = slot {
            tag_effect(game, slot, 0, index as u8);
        }
    }

    // The shotgun fires twice.
    if index == 1 && (frame == 7 || frame == 9) {
        let weapon_id = FIRE_DATA[index].weapon_id;
        let flags = game.player_flags;
        let origin = player.pos;
        game.apply_weapon_damage_with(room, weapon_id, origin, flags);
    }

    if FIRE_END_FRAME[index] < frame && !input.aim {
        player.weapon.behavior = 0x13;
        player.weapon.state = 0;
    }
    if FIRE_END_FRAME[index].wrapping_sub(frame) == 2 && index == 1 {
        play_weapon_sound(game, weapon, 0x0B);
    }
    if FIRE_END_FRAME[index] < frame && index == 1 {
        let aim = player.weapon.aim_flags;
        if (aim & 0xA0 != 0 && !(input.up || input.down))
            || (aim & 0x80 != 0 && !input.down)
            || (aim & 0x20 != 0 && !input.up)
        {
            player.weapon.aim_flags = (aim & 0x1F) | 0x40;
            player.weapon.behavior = 0x13;
            player.weapon.state = 0;
        }
    }
}

/// The auto-aim re-raise loop.
fn autoaim_raise2(game: &mut GameState, player: &mut PlayerState, clips: &WeaponClips<'_>) {
    let done = joint_move(player, clips, Bank::Weapon, 0x400, false);
    player.weapon.state = player.weapon.state.wrapping_add(u8::from(done));
    if equipped_weapon(game).unwrap_or(0) > 0x6E {
        player.weapon.state = player.weapon.state.wrapping_add(1);
    }
}

/// The reversed recover after a quick-fire.
fn autoaim_reverse(
    game: &mut GameState,
    player: &mut PlayerState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    let done = joint_move(player, clips, Bank::Weapon, 0x400, true);
    player.weapon.state = player.weapon.state.wrapping_add(u8::from(done));
    if equipped_weapon(game).unwrap_or(0) > 0x6E && special_frame_update(game, player, 0, 2) {
        player.weapon.state = player.weapon.state.wrapping_add(1);
    }
    if input.up || input.down {
        player.weapon.state = 5;
    }
}

/// The hold-fire loop: consume a round, run the click/flash cadence, and
/// quick-fire on a direction change.
fn autoaim_holdfire(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    let old = player.weapon.aim_flags;
    player.weapon.aim_flags = (old & 0x1F) | 0x40;
    if input.down {
        player.weapon.aim_flags = (player.weapon.aim_flags & 0x1F) | 0x80;
    }
    if input.up {
        player.weapon.aim_flags = (player.weapon.aim_flags & 0x1F) | 0x20;
    }
    let weapon = equipped_weapon(game).unwrap_or(0);

    if weapon_autoaim_check(game) == 0 {
        play_weapon_sound(game, weapon, 3);
        player.weapon.behavior = 0x13;
        player.weapon.state = 0;
        return;
    }

    let frame = player.anim.frame as u8;
    if weapon < SPECIAL_WEAPON_MIN {
        if frame.is_multiple_of(0x0F) {
            play_weapon_sound(game, weapon, 3);
            play_weapon_sound(game, weapon, 4);
        }
        if frame & 3 == 0 {
            spawn_hand_fx(game, 0x0C, 0, [0x21C, 0x4EC, 0], 0);
        }
        if let Some(slot) = equipped_index(game, weapon) {
            game.inventory[slot].quantity = game.inventory[slot].quantity.wrapping_sub(1);
        }
    } else {
        let index = usize::from(weapon) - 99;
        if u32::from(frame) % FIRE_INTERVALS[0] == 0 {
            auto_aim_pitch(game, player);
            let row = FIRE_BILLBOARD[index];
            spawn_hand_fx(game, row.effect_type, row.depth, row.offset(), 0);
            let weapon_id = FIRE_DATA[index].weapon_id;
            let flags = game.player_flags;
            let origin = player.pos;
            game.apply_weapon_damage_with(room, weapon_id, origin, flags);
            play_fire_sounds(game, weapon, index);
        }
        if u32::from(frame) % FIRE_INTERVALS[1] == 0 {
            let row = MUZZLE_FLASH[index];
            if let Some(slot) =
                spawn_player_fx(game, row.effect_type, row.depth, row.offset(), 0x555)
            {
                tag_effect(game, slot, if index == 8 { 0 } else { 3 }, 0);
            }
        }
        if u32::from(frame) % FIRE_INTERVALS[2] == 0 {
            let row = FLASH2[index];
            if let Some(slot) = spawn_hand_fx(game, row.effect_type, row.depth, row.offset(), 0) {
                tag_effect(game, slot, 0, 0);
            }
        }
        if FIRE_END_FRAME[index] < frame && !input.aim {
            player.weapon.behavior = 0x13;
            player.weapon.state = 0;
        }
    }

    if old & 0x40 != 0 && player.weapon.aim_flags & 0xA0 != 0 {
        player.weapon.state = 8;
        let term = aim_dir_term(player.weapon.aim_flags);
        set_motion(player, usize::from(term) * 3 + 5);
        if weapon > 0x6E {
            set_motion(player, usize::from(term) + 5);
            special_frame_update(game, player, 1, 1);
        }
        player.weapon.aim_flags &= 0xFE;
        return;
    }
    if old & 0xA0 != 0 && player.weapon.aim_flags & 0x40 != 0 {
        let old_dir = ((old >> 5) & 1) * 2 + ((old >> 7) & 1);
        player.weapon.state = 8;
        player.weapon.aim_flags |= 1;
        set_motion(player, usize::from(old_dir) * 3 + 5);
        if weapon > 0x6E {
            set_motion(player, usize::from(((old >> 4) & 2) + ((old >> 7) & 1)) + 5);
            special_frame_update(game, player, 1, 1);
        }
        return;
    }
    if (player.weapon.aim_flags ^ old) & 0xA0 == 0 {
        let _ = joint_move(player, clips, Bank::Weapon, 0x400, false);
        if weapon > 0x6E {
            special_frame_update(game, player, 1, 1);
        }
        if !input.fire {
            player.weapon.behavior = 0x13;
            player.weapon.state = 0;
            return;
        }
        if input.right {
            player.angle = player.angle.wrapping_add(0x20) & 0x0FFF;
        } else if input.left {
            player.angle = player.angle.wrapping_sub(0x20) & 0x0FFF;
        }
    } else {
        let term = aim_dir_term(player.weapon.aim_flags);
        player.weapon.state = 8;
        player.weapon.aim_flags &= 0xFE;
        set_motion(player, usize::from(term) * 3 + 5);
        if weapon > 0x6E {
            set_motion(player, usize::from(term) + 5);
        }
    }
}

/// `player_behavior_15_gun_fire`: the anim-only quick-fire and its reverse.
fn gun_quick_fire(
    game: &mut GameState,
    player: &mut PlayerState,
    clips: &WeaponClips<'_>,
    reverse: bool,
) {
    match player.weapon.state {
        0 => {
            player.move_speed_current = 1;
            player.weapon.state = 1;
            reset_frame(player);
            player.weapon.action_tick = 0;
            set_blend(player, 0x400);
            if equipped_weapon(game).unwrap_or(0) > 0x6E && reverse {
                set_motion(player, 5);
                player.anim.frame = 0x0F;
                player.weapon.special_countdown = 0x0F;
            }
        }
        1 => {}
        2 => {
            player.weapon.behavior = 0x13;
            player.weapon.state = 0;
            return;
        }
        _ => return,
    }

    let weapon = equipped_weapon(game).unwrap_or(0);
    if weapon < SPECIAL_WEAPON_MIN || !reverse {
        let done = joint_move(player, clips, Bank::Weapon, 0x400, reverse);
        player.weapon.state = player.weapon.state.wrapping_add(u8::from(done));
    } else {
        player.anim.frame = 0x0F;
        let _ = joint_move(player, clips, Bank::Weapon, 0x400, false);
        let countdown = player.weapon.special_countdown;
        player.weapon.special_countdown = countdown.wrapping_sub(1);
        if countdown < 0 {
            player.weapon.state = player.weapon.state.wrapping_add(1);
        }
    }

    if player.input.fire || player.input.up || player.input.down {
        player.weapon.behavior = 0x13;
        player.weapon.state = 0;
    }
}

/// `player_behavior_17_holster`: lower the weapon and release control.
fn gun_holster(
    game: &mut GameState,
    player: &mut PlayerState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    if input.right {
        player.angle = player.angle.wrapping_add(0x50) & 0x0FFF;
    }
    if input.left {
        player.angle = player.angle.wrapping_sub(0x50) & 0x0FFF;
    }
    if player.weapon.state == 0 {
        set_motion(player, 5);
        set_blend(player, 0x400);
        player.move_speed_current = 1;
        player.weapon.state = 1;
        if equipped_weapon(game).unwrap_or(0) > 0x6E {
            player.anim.frame = 0x1C;
        }
    }
    let done = joint_move(player, clips, Bank::Weapon, 0x400, true);
    if done {
        holster_done(player);
    }
}

/// `player_behavior_18_hold_fire`: the reload motion and its per-weapon FX.
fn gun_reload(game: &mut GameState, player: &mut PlayerState, clips: &WeaponClips<'_>) {
    match player.weapon.state {
        0 => {
            player.weapon.state = 1;
            reset_frame(player);
            game.set_health_status(game.health_status | 0x80);
            game.message_flags &= !0x40;
            player.move_speed_current = 1;
            // The action tick gates the per-weapon FX frames (`unk_bf == 1`);
            // the reload handler arms it for the FX gate.
            player.weapon.action_tick = 1;
            set_motion(player, 0x0E);
            set_blend(player, 0x400);
        }
        1 => {
            let weapon = equipped_weapon(game).unwrap_or(0);
            fire_weapon_fx(game, player, weapon);
            let done = joint_move(player, clips, Bank::Weapon, 0x400, false);
            if done {
                game.set_health_status(game.health_status & 0x7F);
                game.message_flags |= 0x40;
                if weapon > 6 {
                    player.weapon.behavior = 0x18;
                    player.weapon.state = 2;
                    reset_frame(player);
                    player.weapon.action_tick = 0;
                    game.set_health_status(game.health_status | 0x80);
                    game.message_flags &= !0x40;
                    set_blend(player, 0x200);
                    set_motion(player, 7);
                } else {
                    player.weapon.behavior = 0x13;
                    player.weapon.state = 0;
                }
            }
        }
        2 => {
            player.anim.blend_step = 0x200;
            let _ = player.anim.update(clips.weapon);
            if player.anim.blend_counter != 0 {
                return;
            }
            game.set_health_status(game.health_status & 0x7F);
            game.message_flags |= 0x40;
            player.weapon.behavior = 0x13;
            player.weapon.state = 0;
        }
        _ => {}
    }
}

/// The per-weapon reload FX: the volley latch, the ejected shell and the ammo
/// transfer on their motion frames.
fn fire_weapon_fx(game: &mut GameState, player: &mut PlayerState, weapon: u8) {
    let frame = player.anim.frame as u8;
    let active = player.weapon.action_tick == 1;
    match weapon {
        2 => {
            if frame == 0x0A && active {
                // The volley latch arms the beretta's ejected magazine; the
                // port keeps the latch and the transfer, not the clip mesh.
                let _ = fire_ammo_volley_gate(game);
            }
            if frame == 0x11 && active {
                fire_consume_ammo_stack(game);
                play_weapon_sound(game, weapon, 5);
            }
        }
        3 => {
            if (frame == 0x0F || frame == 0x19 || frame == 0x23) && active {
                play_weapon_sound(game, weapon, 9);
                if frame == 0x0F {
                    fire_consume_ammo_stack(game);
                }
            }
        }
        4 | 5 => {
            if frame == 0x0C && active && fire_ammo_volley_gate(game) {
                // The ejected case spawns at the weapon hand's world position
                // with the identity rotation the original's null sprite matrix
                // selects.
                let hand = game.joint_worlds[0]
                    .get(WEAPON_JOINT)
                    .map_or(game.entities[0].pos, |world| world.t);
                let room_effects = Rc::clone(&game.room_effects);
                let _ = effects::create_attached(
                    game,
                    &room_effects,
                    5,
                    4,
                    Attach::Identity,
                    hand,
                    0,
                    5,
                );
            }
            if frame == 0x1C && active {
                fire_consume_ammo_stack(game);
                play_weapon_sound(game, weapon, 0x0A);
            }
        }
        _ => {
            if frame == 0x12 && active {
                fire_consume_ammo_stack(game);
                play_weapon_sound(game, weapon, 5);
            }
        }
    }
}

/// `player_behavior_19_fire_click`: the dry-fire click.
fn gun_click(game: &mut GameState, player: &mut PlayerState) {
    if player.weapon.state == 0 {
        player.weapon.state = 1;
        player.weapon.cooldown = 0x0F;
        let weapon = equipped_weapon(game).unwrap_or(0);
        play_weapon_sound(game, weapon, 9);
    }
    let previous = player.weapon.cooldown;
    player.weapon.cooldown = previous.wrapping_sub(1);
    if previous == 0 {
        player.weapon.behavior = 0x13;
        player.weapon.state = 0;
    }
}

/// `player_behavior_1a_lockon_fire`: the locked-target fire while steering.
fn gun_lockon_fire(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    if player.weapon.state != 0 && input.fire {
        player_find_aim_target(game, player, room);
    }
    let up = (player.weapon.aim_flags & 0x20) >> 4;
    let dir = player.weapon.aim_flags >> 7;

    if player.weapon.state == 0 {
        player.anim.blend_counter = 0;
        player.move_speed_current = 1;
        player.weapon.state = 1;
        lockon_state1(game, player, clips, up, dir);
        return;
    }
    if player.weapon.state == 1 {
        lockon_state1(game, player, clips, up, dir);
        return;
    }
    if player.weapon.state == 2 {
        if player.anim.blend_counter == 0 {
            reset_frame(player);
            set_motion(player, usize::from((up + dir) * 3 + 7));
            player.weapon.action_tick = 0;
            player.weapon.state = 1;
            set_blend(player, 0x200);
            return;
        }
        turn_tail(game, player);
        return;
    }
    turn_tail(game, player);
}

/// The lock-on state-1 body: arm the hold motion or play it.
fn lockon_state1(
    game: &mut GameState,
    player: &mut PlayerState,
    clips: &WeaponClips<'_>,
    up: u8,
    dir: u8,
) {
    if player.anim.blend_counter == 0 {
        player.weapon.state = 2;
        player.weapon.action_tick = 0;
        reset_frame(player);
        set_blend(player, 0x200);
        let weapon = equipped_weapon(game).unwrap_or(0);
        let motion = usize::from(up + dir) + usize::from(weapon) * 3 + 2;
        set_motion(player, motion);
        return;
    }
    player.anim.blend_step = 0x200;
    let _ = player.anim.update(clips.weapon);
}

/// The lock-on steer: rotate toward the target until aligned, then release.
fn turn_tail(game: &mut GameState, player: &mut PlayerState) {
    let character = u16::from(game.id.player_flag & 1);
    if let Some(position) = player
        .weapon
        .target
        .and_then(|slot| target_position(game, slot))
    {
        let mut step = character * 0x20 + 0xF0;
        if turn_toward_target(player, position, 0x200) == 0 {
            step = (character + 6) * 0x20;
        }
        rotate_toward_target(player, position, step);
        if turn_toward_target(player, position, 0x20) == 0 {
            player.weapon.behavior = 0x13;
            player.weapon.state = 0;
        }
    } else {
        player.weapon.behavior = 0x13;
        player.weapon.state = 0;
    }
}

// ---------------------------------------------------------------------------
// The knife behaviors
// ---------------------------------------------------------------------------

/// `player_behavior_12_knife_aim`: the knife raise.
fn knife_aim(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    if player.weapon.state == 0 {
        reset_frame(player);
        player.move_speed_current = 1;
        player.weapon.state = 1;
        player.weapon.cooldown = 0;
        player.weapon.action_tick = 0;
        set_motion(player, 5);
        set_blend(player, 0x400);
        match reticle_scan(game, player, room) {
            Some(target) => {
                player.weapon.target = Some(target);
                player.weapon.aim_state = 3;
            }
            None => player.weapon.aim_state = 0,
        }
    }

    player.weapon.aim_state |= 2;
    if input.fire_pressed {
        player.weapon.aim_state = 0;
    }
    if player.weapon.aim_state == 3
        && let Some(position) = player
            .weapon
            .target
            .and_then(|slot| target_position(game, slot))
    {
        let character = u16::from(game.id.player_flag & 1);
        let mut step = character * 0x20 + 0xF0;
        if turn_toward_target(player, position, 0x200) == 0 {
            step = (character + 6) * 0x20;
        }
        rotate_toward_target(player, position, step);
    }

    let done = joint_move(player, clips, Bank::Weapon, 0x400, false);
    if done {
        player.anim.blend_counter = 0;
        player.weapon.behavior = 0x13;
        player.weapon.state = 0;
    }

    if input.right {
        player.angle = player.angle.wrapping_add(0x20) & 0x0FFF;
        return;
    }
    if input.left {
        player.angle = player.angle.wrapping_sub(0x20) & 0x0FFF;
    }
}

/// The knife hold's motion id base: `((flags & 0xbf) >> 6) + ((flags & 0x20) >> 3)`.
fn knife_motion(game: &GameState) -> u8 {
    let flags = game.player_flags;
    ((flags & 0xBF) >> 6) + ((flags & 0x20) >> 3)
}

/// `player_behavior_13_knife_hold`.
fn knife_hold(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    let old = game.player_flags;
    game.player_flags = (old & 0x1F) | 0x40;
    if input.down {
        game.player_flags = (game.player_flags & 0x1F) | 0x80;
    }
    if input.up {
        game.player_flags = (game.player_flags & 0x1F) | 0x20;
    }
    player.weapon.aim_flags &= 0xFD;

    if player.weapon.state == 0 {
        if player.anim.blend_counter == 0 {
            player.weapon.state = 1;
            player.move_speed_current = 1;
            set_motion(player, usize::from(knife_motion(game)) + 10);
            reset_frame(player);
            player.weapon.action_tick = 0;
            set_blend(player, 0x100);
        }
        let _ = joint_move(player, clips, Bank::Weapon, 0x100, false);
    }
    if player.anim.blend_counter == 0 {
        reset_frame(player);
        player.weapon.action_tick = 0;
        set_blend(player, 0x100);
        player.move_speed_current = 1;
        set_motion(player, usize::from(knife_motion(game)) + 9);
    }
    let _ = joint_move(player, clips, Bank::Weapon, 0x100, false);

    if player.weapon.cooldown != 0 {
        player.weapon.cooldown -= 1;
    }
    if input.fire && player.weapon.cooldown == 0 {
        player.weapon.behavior = 0x14;
        player.weapon.state = 0;
        player.weapon.cooldown = 10;
        return;
    }
    if !input.aim {
        player.weapon.behavior = 0x15;
        player.weapon.state = 0;
        return;
    }
    if input.fire && player_find_aim_target(game, player, room) {
        player.weapon.behavior = 0x16;
        player.weapon.state = 0;
        return;
    }
    if input.right {
        player.angle = player
            .angle
            .wrapping_add(steer_step(game.id.player_flag & 1, true))
            & 0x0FFF;
        return;
    }
    if input.left {
        player.angle = player
            .angle
            .wrapping_add(steer_step(game.id.player_flag & 1, false))
            & 0x0FFF;
    }
}

/// `player_behavior_14_knife_swing`.
fn knife_swing(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
) {
    if player.weapon.state == 0 {
        reset_frame(player);
        player.weapon.state = 1;
        player.weapon.action_tick = 0;
        set_blend(player, 0x400);
        player.move_speed_current = 1;
        let flags = game.player_flags;
        let motion = ((flags & 0x20) >> 4)
            .wrapping_sub(((flags as i8) >> 7) as u8)
            .wrapping_add(6);
        set_motion(player, usize::from(motion));
    }

    let frame = player.anim.frame as u8;
    if frame == 7 && player.weapon.action_tick & 1 != 0 {
        play_weapon_sound(game, ITEM_KNIFE, 0);
    }

    game.entities[0].is_being_attacked = 0;

    let character = usize::from(game.id.player_flag & 1);
    let motion = player.anim.clip;
    let e = (character * 3 + motion) * 2;
    if (12..=13).contains(&e) {
        let window = KNIFE_FIRE_WINDOW[e - 12];
        let width = KNIFE_FIRE_WINDOW[e - 11];
        if frame.wrapping_sub(window) < width && player.weapon.aim_flags & 2 == 0 {
            let flags = game.player_flags;
            let origin = player.pos;
            if game.apply_weapon_damage_with(room, ITEM_KNIFE, origin, flags) != 0 {
                let head = game.entities.get(1).map_or(0, |entity| entity.id);
                let second = game.entities.get(2).map_or(0, |entity| entity.id);
                if motion != 6 || head == 8 || head == 0x0D || second == 0x13 {
                    player.weapon.aim_flags |= 2;
                }
            }
        }
    }

    let done = joint_move(player, clips, Bank::Weapon, 0x400, false);
    if done {
        player.weapon.behavior = 0x13;
        player.weapon.state = 0;
    }
}

/// `player_behavior_15_knife_holster`.
fn knife_holster(
    game: &mut GameState,
    player: &mut PlayerState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    if input.right {
        player.angle = player.angle.wrapping_add(0x50) & 0x0FFF;
    }
    if input.left {
        player.angle = player.angle.wrapping_sub(0x50) & 0x0FFF;
    }
    if player.weapon.state == 0 {
        reset_frame(player);
        player.move_speed_current = 1;
        player.weapon.state = 1;
        set_motion(player, 5);
        set_blend(player, 0x400);
        player.weapon.action_tick = 0;
    }
    let done = joint_move(player, clips, Bank::Weapon, 0x400, true);
    if done {
        holster_done(player);
    }
    let _ = game;
}

/// `player_behavior_16_knife_turn`.
fn knife_turn(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
    clips: &WeaponClips<'_>,
    input: Input,
) {
    if player.weapon.state != 0 && input.fire {
        player_find_aim_target(game, player, room);
    }
    if player.weapon.state == 0 {
        player.weapon.state = 1;
        reset_frame(player);
        player.weapon.action_tick = 0;
        set_blend(player, 0x100);
        player.move_speed_current = 1;
        set_motion(player, usize::from(knife_motion(game)) + 9);
    }
    match player.weapon.state {
        0 | 1 if player.anim.blend_counter == 0 => {
            reset_frame(player);
            player.weapon.action_tick = 0;
            player.weapon.state = 2;
            set_blend(player, 0x100);
            set_motion(player, usize::from(knife_motion(game)) + 10);
        }
        2 if player.anim.blend_counter == 0 => {
            reset_frame(player);
            player.weapon.action_tick = 0;
            player.weapon.state = 1;
            set_blend(player, 0x100);
            set_motion(player, usize::from(knife_motion(game)) + 9);
        }
        _ => {}
    }
    let _ = joint_move(player, clips, Bank::Weapon, 0x100, false);

    if let Some(position) = player
        .weapon
        .target
        .and_then(|slot| target_position(game, slot))
    {
        let character = u16::from(game.id.player_flag & 1);
        let mut step = character * 0x20 + 0xF0;
        if turn_toward_target(player, position, 0x200) == 0 {
            step = (character + 6) * 0x20;
        }
        rotate_toward_target(player, position, step);
        if turn_toward_target(player, position, 0x20) == 0 {
            player.weapon.behavior = 0x13;
            player.weapon.state = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Targeting
// ---------------------------------------------------------------------------

/// The target entity position for the aim/lock-on turns.
fn target_position(game: &GameState, slot: u8) -> Option<[i32; 2]> {
    let entity = game.entities.get(usize::from(slot))?;
    Some([entity.pos[0], entity.pos[2]])
}

/// `turn_toward_target`: the signed steer step toward `(x, z)`, 0 when inside
/// twice `step`.
fn turn_toward_target(player: &PlayerState, target: [i32; 2], step: i16) -> i32 {
    let target_angle =
        crate::sfx::angle_between_xz(player.pos[0], player.pos[2], target[0], target[1]) & 0x0FFF;
    let delta = target_angle
        .wrapping_sub(player.angle)
        .wrapping_add(step as u16)
        & 0x0FFF;
    if i32::from(delta) < i32::from((step as u16).wrapping_mul(2)) {
        return 0;
    }
    if delta < 0x801 {
        i32::from(step)
    } else {
        -i32::from(step)
    }
}

/// `entity_rotate_toward_target`: steer the facing toward `(x, z)` by `step`,
/// snapping inside twice the step.
fn rotate_toward_target(player: &mut PlayerState, target: [i32; 2], step_in: u16) {
    let mut base =
        crate::sfx::angle_between_xz(player.pos[0], player.pos[2], target[0], target[1]) & 0x0FFF;
    let mut step = step_in;
    if step & 0x8000 != 0 {
        step = step.wrapping_neg();
        base = base.wrapping_add(0x800) & 0x0FFF;
    }
    let delta = step.wrapping_sub(player.angle).wrapping_add(base) & 0x0FFF;
    if i32::from(delta) < i32::from(step as i16) * 2 {
        player.angle = base;
        return;
    }
    player.angle = player.angle.wrapping_sub(step) & 0x0FFF;
    if delta < 0x801 {
        player.angle = player.angle.wrapping_add(step.wrapping_mul(2)) & 0x0FFF;
    }
}

/// `player_aim_cone_test`: whether the ray from the player through `delta`
/// crosses a fully sight-blocking room record. Walks all four quadrant lists
/// with the original's two-diagonal straddle test.
pub fn aim_cone_blocked(room: &RoomState, player: &PlayerState, delta: [i32; 2]) -> bool {
    let player_x = player.pos[0] / 18;
    let player_z = player.pos[2] / 18;
    let dir_x = delta[0] / 18;
    let dir_z = delta[1] / 18;
    let cross_y = |a: [i32; 2], b: [i32; 2]| a[1] * b[0] - b[1] * a[0];
    for quadrant in &room.collision.quadrants {
        for rect in quadrant {
            if rect.flags & 0x300 != 0x300 {
                continue;
            }
            if rect.kind & 0xFF == 4 || rect.kind & 0xFF == 5 {
                continue;
            }
            let x_max = i32::from(rect.x_max) / 18;
            let z_max = i32::from(rect.z_max) / 18;
            let x_min = i32::from(rect.x_min) / 18;
            let z_min = i32::from(rect.z_min) / 18;

            // Diagonal 1: (xMax, zMin) -> (xMin, zMax), with the normalize.
            let edge = [x_min - x_max, z_max - z_min];
            let p1 = [player_x + dir_x - x_max, player_z + dir_z - z_min];
            let p0 = [player_x - x_max, player_z - z_min];
            if cross_y(edge, p0) ^ cross_y(edge, p1) < 0 {
                let p1 = cross_y(delta, [x_min - player_x, z_min - player_z]);
                let p0 = cross_y(delta, vector_normal([x_max - player_x, z_max - player_z]));
                if p0 ^ p1 < 0 {
                    return true;
                }
            }

            // Diagonal 2: (xMin, zMin) -> (xMax, zMax), no normalize.
            let edge = [x_max - x_min, z_max - z_min];
            let p1 = [player_x + dir_x - x_min, player_z + dir_z - z_min];
            let p0 = [player_x - x_min, player_z - z_min];
            if cross_y(edge, p0) ^ cross_y(edge, p1) < 0 {
                let p1 = cross_y(delta, [x_max - player_x, z_max - player_z]);
                let p0 = cross_y(delta, [x_min - player_x, z_min - player_z]);
                if p0 ^ p1 < 0 {
                    return true;
                }
            }
        }
    }
    false
}

/// `VectorNormal`: scale a 2D XZ vector to length 4096.
fn vector_normal(point: [i32; 2]) -> [i32; 2] {
    let x = f64::from(point[0]);
    let z = f64::from(point[1]);
    let mut len = (x * x + z * z).sqrt();
    if len == 0.0 {
        len = 1e-9;
    }
    [(x * 4096.0 / len) as i32, (z * 4096.0 / len) as i32]
}

/// `player_reticle_enemy`: the nearest valid target's entity slot.
fn reticle_scan(game: &GameState, player: &PlayerState, room: &RoomState) -> Option<u8> {
    let mut nearest: Option<(u8, u32)> = None;
    let mut low: Option<(u8, u32)> = None;
    let mut standing: Option<(u8, u32)> = None;
    for slot in 1..crate::game::ENTITY_COUNT {
        let entity = &game.entities[slot];
        if !entity.active()
            || entity.has_enter_switch_zone == 0
            || entity.health < 0
            || entity.behavior_flags & 0xC0 != 0
            || entity.id >= 0x13
        {
            continue;
        }
        let dx = entity.pos[0] - player.pos[0];
        let dz = entity.pos[2] - player.pos[2];
        if aim_cone_blocked(room, player, [dx, dz]) {
            continue;
        }
        let distance = dx
            .unsigned_abs()
            .wrapping_mul(dx.unsigned_abs())
            .wrapping_add(dz.unsigned_abs().wrapping_mul(dz.unsigned_abs()))
            .isqrt();
        if nearest.is_none_or(|(_, best)| distance < best) {
            nearest = Some((slot as u8, distance));
        }
        if entity.pos[1] < 1 {
            if low.is_none_or(|(_, best)| distance < best) {
                low = Some((slot as u8, distance));
            }
        } else if standing.is_none_or(|(_, best)| distance < best) {
            standing = Some((slot as u8, distance));
        }
    }
    let (nearest, _) = nearest?;
    if equipped_weapon(game) == Some(ITEM_KNIFE) {
        return Some(nearest);
    }
    standing.or(low).map(|(slot, _)| slot)
}

/// `player_find_aim_target`: lock the first alive, script-free enemy from the
/// current lock forward that passes the aim cone.
fn player_find_aim_target(
    game: &mut GameState,
    player: &mut PlayerState,
    room: &RoomState,
) -> bool {
    let count = usize::from(game.enemy_count).min(30);
    if count == 0 {
        return false;
    }
    let slots = crate::game::ENTITY_COUNT.saturating_sub(1);
    let start = player.weapon.target.map_or(1usize, usize::from);
    for step in 0..slots {
        let slot = 1 + (start.saturating_sub(1) + step) % slots;
        let entity = &game.entities[slot];
        if !entity.active()
            || entity.has_enter_switch_zone == 0
            || entity.health < 0
            || entity.behavior_flags & 0xC0 != 0
            || entity.id >= 0x13
        {
            continue;
        }
        let dx = entity.pos[0] - player.pos[0];
        let dz = entity.pos[2] - player.pos[2];
        if !aim_cone_blocked(room, player, [dx, dz]) {
            player.weapon.target = Some(slot as u8);
            return true;
        }
    }
    false
}

/// `weapon_lockon_effect`: the auto-aim lock-on fan of jittered billboards.
fn weapon_lockon_effect(game: &mut GameState, player: &PlayerState, room: &RoomState) {
    let weapon = equipped_weapon(game).unwrap_or(0);
    if weapon > 5 {
        return;
    }
    let is_shotgun = weapon == 3;
    let Some(cut) = room.cuts.get(room.current_cut) else {
        return;
    };
    let span_z = cut.look_at[0] - cut.pos[0];
    let span_x = cut.look_at[2] - cut.pos[2];

    // The camera yaw: `(dz < 0 ? 2 : 1) * 0x800 - GetAngleQuadrantValue(dx/dz)`.
    let camera_angle = if span_z == 0 {
        0x400 + if span_x > 0 { 0x800 } else { 0 }
    } else {
        let slope = (span_x.wrapping_mul(0x1000)) / span_z;
        let angle = (f64::from(slope) / 4096.0).atan() * (2048.0 / std::f64::consts::PI);
        let angle = angle as i32 as i16;
        let base = if span_z < 0 { 2i32 } else { 1i32 } * 0x800;
        (base - i32::from(angle)) as u16 & 0x0FFF
    };

    let offset = (u16::from(is_shotgun) + 1) * 0xA0;
    let (fan_x, fan_z) =
        crate::player::rotate_speed(camera_angle.wrapping_add(0x400), 0, i32::from(offset));
    let hand = game.joint_worlds[0]
        .get(WEAPON_JOINT)
        .map_or(player.pos, |world| world.t);

    let lift_l = (i32::from(is_shotgun) * 5 + 5) * 0x20;
    let lift_r = (i32::from(is_shotgun) * 5 - 5) * 0x20;
    let edge_l = [
        (cut.look_at[0] - hand[0] + fan_x) / 0x12,
        (cut.pos[1] - hand[1] + lift_l) / 0x12,
        (cut.look_at[2] - hand[2] + fan_z) / 0x12,
    ];
    let edge_r = [
        (cut.pos[0] - hand[0] - fan_x) / 0x12,
        (cut.pos[1] - hand[1] + lift_r) / 0x12,
        (cut.pos[2] - hand[2] - fan_z) / 0x12,
    ];

    // The aim vector is joint 14's rotation applied to (0/400, 1000, 0).
    let local = [if is_shotgun { 400 } else { 0 }, 1000, 0];
    let aim = match game.joint_worlds[0].get(WEAPON_JOINT) {
        Some(world) => [
            world.r[0][0] * local[0] + world.r[0][1] * local[1] + world.r[0][2] * local[2],
            world.r[1][0] * local[0] + world.r[1][1] * local[1] + world.r[1][2] * local[2],
            world.r[2][0] * local[0] + world.r[2][1] * local[1] + world.r[2][2] * local[2],
        ],
        None => local,
    };
    let c_l = cross3(aim, edge_l);
    let c_r = cross3(aim, edge_r);
    if c_l[1] >= 0 || c_r[1] < 0 {
        return;
    }
    if (c_r[0] ^ c_l[0]) >= 0 || (c_r[2] ^ c_l[2]) >= 0 {
        return;
    }

    // Jittered billboards around the camera position.
    let span = [span_z, 0, span_x];
    let normal = normalize3(span);
    let mut pos = [
        normal[0] / 8 + cut.pos[0] + 0x80,
        normal[1] / 8 + cut.pos[1] + 0x80,
        normal[2] / 8 + cut.pos[2] + 0x80,
    ];
    pos[0] -= i32::from(crate::game::platform_rand(&mut game.rand_state) & 0xFF);
    pos[1] -= i32::from(crate::game::platform_rand(&mut game.rand_state) & 0xFF);
    pos[2] -= i32::from(crate::game::platform_rand(&mut game.rand_state) & 0xFF);
    let room_effects = Rc::clone(&game.room_effects);
    let _ = effects::create_attached(game, &room_effects, 0x05, 0x12, Attach::Identity, pos, 0, 0);
    if is_shotgun {
        for _ in 0..3 {
            pos[0] += 0x100 - i32::from(crate::game::platform_rand(&mut game.rand_state) & 0x1FF);
            pos[1] += 0x100 - i32::from(crate::game::platform_rand(&mut game.rand_state) & 0x1FF);
            pos[2] += 0x100 - i32::from(crate::game::platform_rand(&mut game.rand_state) & 0x1FF);
            let room_effects = Rc::clone(&game.room_effects);
            let _ = effects::create_attached(
                game,
                &room_effects,
                0x05,
                0x12,
                Attach::Identity,
                pos,
                0,
                0,
            );
        }
    }
}

/// The cross product's three components (`vectorMul3`).
fn cross3(a: [i32; 3], b: [i32; 3]) -> [i32; 3] {
    [
        a[1] * b[2] - b[1] * a[2],
        a[2] * b[0] - b[2] * a[0],
        a[1] * b[0] - b[1] * a[0],
    ]
}

/// The full normalized vector (`VectorNormal`).
fn normalize3(point: [i32; 3]) -> [i32; 3] {
    let x = f64::from(point[0]);
    let y = f64::from(point[1]);
    let z = f64::from(point[2]);
    let mut len = (x * x + y * y + z * z).sqrt();
    if len == 0.0 {
        len = 1e-9;
    }
    [
        (x * 4096.0 / len) as i32,
        (y * 4096.0 / len) as i32,
        (z * 4096.0 / len) as i32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::InventoryItem;
    use crate::model::{ClipFrame, Keyframe};
    use crate::player;
    use crate::state::RoomId;

    /// A synthetic clip bank: `count` clips of `frames` frames each, one tick
    /// per frame.
    fn clip_bank(count: usize, frames: usize) -> Vec<Clip> {
        vec![
            Clip {
                frames: vec![
                    ClipFrame {
                        keyframe: 0,
                        timing: 1,
                    };
                    frames
                ],
            };
            count
        ]
    }

    /// A 15-joint chain with the root offset at `root_y`, so every composed
    /// joint (and the aim pitch) sits at that height.
    fn rig(root_y: i16) -> (Skeleton, Vec<Keyframe>) {
        let skeleton = Skeleton {
            relative: vec![[0, 0, 0]; 15],
            children: (1..15).map(|index| vec![index as u8]).collect(),
        };
        let keyframe = Keyframe {
            offset: [0, root_y, 0],
            rotations: vec![[0, 0, 0]; 15],
        };
        (skeleton, vec![keyframe])
    }

    /// A player equipped with `weapon` holding `quantity` rounds, aiming down
    /// +X from the origin.
    fn armed(weapon: u8, quantity: u8) -> (GameState, PlayerState) {
        let mut game = GameState::default();
        game.id = RoomId {
            stage: 1,
            room: 0x30,
            player_flag: 0,
        };
        game.inventory.push(InventoryItem {
            id: weapon,
            quantity,
        });
        game.set_equipped(Some(weapon));
        // The muzzle/flash sprites the fire tables name, as global weapon-FX
        // metadata so the spawns are not skipped in a room without a table.
        for index in [0x05u8, 0x08, 0x09, 0x0B, 0x0C, 0x0E, 0x11] {
            game.weapon_effects
                .sprites
                .push(crate::effects::fixtures::sprite(
                    index,
                    std::array::from_fn(|_| vec![vec![crate::effects::fixtures::block(1, 0, 0)]]),
                ));
        }
        let player = player::spawn(game.id, &RoomState::default());
        (game, player)
    }

    /// Place one active enemy of `id` at `pos` with the aligned status bit the
    /// aim filter needs.
    fn place_enemy(game: &mut GameState, slot: usize, id: u8, pos: [i32; 3]) {
        let entity = &mut game.entities[slot];
        entity.id = id;
        entity.pos = pos;
        entity.set_active(true);
        entity.status_flags |= 0x40;
        entity.health = 55;
        game.enemy_count = game.enemy_count.max(slot as u8);
    }

    fn aiming(fire: bool) -> Input {
        Input {
            aim: true,
            fire,
            ..Input::default()
        }
    }

    /// Run the weapon machine for up to `ticks` ticks with `input`.
    fn drive(
        game: &mut GameState,
        player: &mut PlayerState,
        input: Input,
        ticks: usize,
    ) -> (Vec<Clip>, Vec<Clip>, Skeleton, Vec<Keyframe>) {
        let emd = clip_bank(0x24, 8);
        let weapon_clips = clip_bank(15, 8);
        // The aim height table has a tighter pair for the handguns and a wider
        // pair everywhere else; pick a rig height that keeps the pitch neutral.
        let root_y = match game.equipped {
            Some(weapon @ 2..=5) if weapon != 3 => -2400,
            _ => -1800,
        };
        let (skeleton, keyframes) = rig(root_y);
        let room = RoomState::default();
        for _ in 0..ticks {
            let clips = WeaponClips {
                emd: &emd,
                emw: &emd,
                room: &[],
                weapon: &weapon_clips,
                emd_keyframes: &keyframes,
                emd_skeleton: &skeleton,
                weapon_keyframes: &keyframes,
                weapon_skeleton: &skeleton,
            };
            update(game, player, &room, &clips, input);
        }
        (emd, weapon_clips, skeleton, keyframes)
    }

    #[test]
    fn fire_data_indexing_and_ammo_items() {
        assert_eq!(fire_index(1), None, "the knife has no auto-aim row");
        assert_eq!(fire_index(2), Some(0));
        assert_eq!(fire_index(6), Some(4));
        assert_eq!(fire_index(10), Some(8));
        assert_eq!(fire_index(11), Some(9));
        assert_eq!(fire_index(ITEM_INGRAM), Some(12));
        assert_eq!(fire_index(ITEM_MINIMI), Some(13));
        assert_eq!(fire_index(0x71), None);
        assert_eq!(weapon_ammo_item_id(2), 0x0B, "handgun -> 9mm clip");
        assert_eq!(weapon_ammo_item_id(3), 0x0C, "shotgun -> shells");
        assert_eq!(weapon_ammo_item_id(10), 0x13, "rocket launcher -> rockets");
    }

    #[test]
    fn weapon_emw_entries_follow_the_per_character_table() {
        assert_eq!(
            weapon_emw_entry(0, 0).unwrap(),
            "player/00.emw",
            "Chris' no-weapon pair stays"
        );
        assert_eq!(weapon_emw_entry(0, 1).unwrap(), "player/w01.emw");
        assert_eq!(weapon_emw_entry(0, 2).unwrap(), "player/w02.emw");
        assert_eq!(weapon_emw_entry(0, 5).unwrap(), "player/w04.emw");
        assert_eq!(weapon_emw_entry(0, 10).unwrap(), "player/w07.emw");
        assert_eq!(weapon_emw_entry(0, ITEM_INGRAM).unwrap(), "player/w18.emw");
        assert_eq!(weapon_emw_entry(1, 0).unwrap(), "player/01.emw");
        assert_eq!(weapon_emw_entry(1, 2).unwrap(), "player/w12.emw");
        assert_eq!(weapon_emw_id(3, 1), Some(0x11), "Rebecca's knife");
    }

    #[test]
    fn character_weapon_entries_clamp_the_block() {
        assert_eq!(character_weapon_entry(0x20, 0), None, "unarmed");
        assert_eq!(character_weapon_entry(0x20, 1).unwrap(), "player/ws202.tmd");
        assert_eq!(character_weapon_entry(0x21, 1).unwrap(), "player/ws212.tmd");
        assert_eq!(character_weapon_entry(0x22, 5).unwrap(), "player/ws225.tmd");
        assert_eq!(character_weapon_entry(0x23, 6).unwrap(), "player/ws236.tmd");
        assert_eq!(character_weapon_entry(0x24, 1).unwrap(), "player/ws242.tmd");
        assert_eq!(
            character_weapon_entry(0x2E, 1).unwrap(),
            "player/ws224.tmd",
            "ids above Wesker fold onto block 2"
        );
        assert_eq!(character_weapon_entry(0x20, 7), None, "out of range");
    }

    #[test]
    fn autoaim_check_reads_the_clip_and_refills_specials() {
        let (mut game, _player) = armed(1, 0);
        assert_eq!(weapon_autoaim_check(&mut game), 0, "the knife has no ammo");

        let (mut game, _player) = armed(6, 200);
        assert_eq!(weapon_autoaim_check(&mut game), 200, "flamethrower is u8");

        let (mut game, _player) = armed(2, 0x8F);
        assert_eq!(weapon_autoaim_check(&mut game), 0x0F, "masked to 0x7f");

        let (mut game, _player) = armed(2, 0);
        assert_eq!(weapon_autoaim_check(&mut game), 0, "empty handgun");

        let (mut game, _player) = armed(10, 0);
        game.apply_flag(BANK_SCENARIO, SCENARIO_FLAG_INF_R_LAUNCHER, 0);
        assert_eq!(weapon_autoaim_check(&mut game), 4, "infinite launcher");
        assert_eq!(game.inventory[0].quantity, 4, "refilled in place");

        let (mut game, _player) = armed(ITEM_INGRAM, 0);
        assert_eq!(weapon_autoaim_check(&mut game), 4, "specials refill");
    }

    #[test]
    fn reload_transfers_from_the_largest_ammo_stack() {
        let (mut game, _player) = armed(2, 0);
        game.inventory.push(InventoryItem {
            id: 0x0B,
            quantity: 5,
        });
        game.inventory.push(InventoryItem {
            id: 0x0B,
            quantity: 20,
        });
        fire_consume_ammo_stack(&mut game);
        assert_eq!(game.inventory[0].quantity, 15, "clip max is 15");
        let remaining = game.item_count(0x0B);
        assert_eq!(remaining, 10, "20 - 15 left in the largest stack");
        assert_eq!(
            game.inventory[1].quantity, 5,
            "the largest stack kept the rest"
        );
        assert_eq!(
            game.inventory[2].quantity, 5,
            "the smaller stack is untouched"
        );

        let (mut game, _player) = armed(3, 0);
        game.inventory.push(InventoryItem {
            id: 0x0C,
            quantity: 4,
        });
        fire_consume_ammo_stack(&mut game);
        assert_eq!(game.inventory[0].quantity, 4);
        assert_eq!(game.item_count(0x0C), 0, "the stack was consumed");
    }

    /// Hold the aim with a fire burst of `fire_ticks`, then release the fire
    /// and keep aiming, so exactly one trigger cycle runs.
    fn drive_one_shot(
        game: &mut GameState,
        player: &mut PlayerState,
        fire_ticks: usize,
        ticks: usize,
    ) {
        let emd = clip_bank(0x24, 8);
        let weapon_clips = clip_bank(15, 8);
        let root_y = match game.equipped {
            Some(weapon @ 2..=5) if weapon != 3 => -2400,
            _ => -1800,
        };
        let (skeleton, keyframes) = rig(root_y);
        let room = RoomState::default();
        for tick in 0..ticks {
            let input = aiming(tick < fire_ticks);
            let clips = WeaponClips {
                emd: &emd,
                emw: &emd,
                room: &[],
                weapon: &weapon_clips,
                emd_keyframes: &keyframes,
                emd_skeleton: &skeleton,
                weapon_keyframes: &keyframes,
                weapon_skeleton: &skeleton,
            };
            update(game, player, &room, &clips, input);
        }
    }

    #[test]
    fn the_handgun_aim_fire_decrements_the_clip_and_damages_once() {
        let (mut game, mut player) = armed(2, 15);
        place_enemy(&mut game, 1, 0x00, [800, 0, 0]);
        drive_one_shot(&mut game, &mut player, 16, 40);
        assert_eq!(game.entities[1].health, 55 - 9, "the handgun zombie record");
        assert_eq!(game.inventory[0].quantity, 14, "one round left the clip");
        assert_eq!(game.entities[1].state(), 2, "the surviving hit reaction");
    }

    #[test]
    fn an_empty_clip_clicks_and_a_reload_refills_it() {
        let (mut game, mut player) = armed(2, 0);
        drive(&mut game, &mut player, aiming(true), 40);
        assert_eq!(player.weapon.behavior, 0x13, "the hold after the click");

        let (mut game, mut player) = armed(2, 0);
        game.inventory.push(InventoryItem {
            id: 0x0B,
            quantity: 15,
        });
        let emd = clip_bank(0x24, 8);
        let weapon_clips = clip_bank(15, 32);
        let (skeleton, keyframes) = rig(-2400);
        let room = RoomState::default();
        let mut reached_reload = false;
        for tick in 0..80 {
            let input = Input {
                aim: true,
                fire: true,
                fire_pressed: tick < 40,
                ..Input::default()
            };
            let clips = WeaponClips {
                emd: &emd,
                emw: &emd,
                room: &[],
                weapon: &weapon_clips,
                emd_keyframes: &keyframes,
                emd_skeleton: &skeleton,
                weapon_keyframes: &keyframes,
                weapon_skeleton: &skeleton,
            };
            update(&mut game, &mut player, &room, &clips, input);
            if player.weapon.behavior == 0x18 {
                reached_reload = true;
            }
            if game.inventory[0].quantity == 15 {
                break;
            }
        }
        assert!(reached_reload, "fire on an empty clip entered the reload");
        assert_eq!(game.inventory[0].quantity, 15, "the clip was refilled");
        assert_eq!(game.item_count(0x0B), 0, "the reserve was consumed");
    }

    #[test]
    fn the_knife_swing_damages_in_its_fire_window() {
        let (mut game, mut player) = armed(1, 0);
        place_enemy(&mut game, 1, 0x13, [200, 0, 0]);
        drive(&mut game, &mut player, aiming(true), 40);
        assert_eq!(game.entities[1].health, 45, "the web knife record");
        assert_eq!(player.weapon.behavior, 0x13, "back to the knife hold");
        assert_eq!(
            player.weapon.aim_flags & 2,
            0,
            "a neutral swing at the web never latches the once-per-swing gate"
        );
    }

    #[test]
    fn a_shot_behind_the_cone_misses_and_keeps_the_clip() {
        let (mut game, mut player) = armed(2, 15);
        place_enemy(&mut game, 1, 0x00, [-800, 0, 0]);
        drive_one_shot(&mut game, &mut player, 16, 40);
        assert_eq!(game.entities[1].health, 55, "no damage behind the player");
        assert_eq!(
            game.inventory[0].quantity, 14,
            "the round still left the clip"
        );
    }

    #[test]
    fn the_second_playthrough_reads_the_second_knife_record() {
        let (mut game, mut player) = armed(1, 0);
        game.apply_flag(
            BANK_SCENARIO,
            crate::game::SCENARIO_FLAG_SECOND_PLAYTHROUGH,
            0,
        );
        place_enemy(&mut game, 1, 0x13, [200, 0, 0]);
        drive(&mut game, &mut player, aiming(true), 40);
        assert_eq!(game.entities[1].health, 55 - 6, "the web second-run record");
    }

    #[test]
    fn the_gl_explosive_uses_the_projectile_detector() {
        let (mut game, mut player) = armed(7, 4);
        place_enemy(&mut game, 1, 0x00, [500, 0, 0]);
        // The projectile detector measures from the player origin, so the
        // target must sit inside the weapon's range (radius 422 + 400).
        drive_one_shot(&mut game, &mut player, 16, 40);
        assert_eq!(game.entities[1].health, 55 - 201, "GL explosive record");
        assert_eq!(game.entities[1].state(), 3, "the explosive round kills");
        assert_eq!(game.inventory[0].quantity, 3);
    }

    #[test]
    fn the_muzzle_flash_rides_the_weapon_hand_joint() {
        let (mut game, mut player) = armed(2, 15);
        place_enemy(&mut game, 1, 0x00, [800, 0, 0]);
        drive(&mut game, &mut player, aiming(true), 12);
        let hand_fx = game
            .effects
            .active()
            .any(|(_, effect)| effect.attach == Attach::Joint(0, WEAPON_JOINT as u8));
        assert!(hand_fx, "a hand-attached flash was spawned");
        assert!(!game.joint_worlds[0].is_empty(), "the hand pose was stored");
    }

    #[test]
    fn holstering_returns_control_to_the_locomotion_machine() {
        let (mut game, mut player) = armed(2, 15);
        let emd = clip_bank(0x24, 8);
        let weapon_clips = clip_bank(15, 8);
        let (skeleton, keyframes) = rig(-2400);
        let room = RoomState::default();
        let clips = WeaponClips {
            emd: &emd,
            emw: &emd,
            room: &[],
            weapon: &weapon_clips,
            emd_keyframes: &keyframes,
            emd_skeleton: &skeleton,
            weapon_keyframes: &keyframes,
            weapon_skeleton: &skeleton,
        };
        // Aim until the machine owns the tick.
        for _ in 0..3 {
            update(&mut game, &mut player, &room, &clips, aiming(false));
        }
        assert!(player.weapon.active(), "the aim machine started");
        // Release the aim: the holster motion runs and control returns.
        for _ in 0..80 {
            update(&mut game, &mut player, &room, &clips, Input::default());
            if !player.weapon.active() {
                break;
            }
        }
        assert!(!player.weapon.active(), "the machine released");
        assert_eq!(player.clip_source, player::ClipSource::Emd);
        assert_eq!(player.behavior, 0, "idle behavior code");
    }

    #[test]
    fn the_aim_machine_needs_a_weapon_and_a_clip_bank() {
        let (mut game, mut player) = armed(0, 0);
        game.set_equipped(None);
        let emd = clip_bank(0x24, 8);
        let empty: Vec<Clip> = Vec::new();
        let room = RoomState::default();
        let clips = WeaponClips {
            emd: &emd,
            emw: &emd,
            room: &[],
            weapon: &empty,
            emd_keyframes: &[],
            emd_skeleton: &Skeleton::default(),
            weapon_keyframes: &[],
            weapon_skeleton: &Skeleton::default(),
        };
        update(&mut game, &mut player, &room, &clips, aiming(false));
        assert!(!player.weapon.active(), "no weapon, no machine");
    }

    #[test]
    fn tables_are_consistently_shaped() {
        assert_eq!(FIRE_DATA.len(), WEAPON_ROWS);
        assert_eq!(FIRE_BILLBOARD.len(), WEAPON_ROWS);
        assert_eq!(MUZZLE_FLASH.len(), WEAPON_ROWS);
        assert_eq!(FLASH2.len(), WEAPON_ROWS);
        assert_eq!(AIM_HEIGHT_TABLE.len(), 12);
        assert_eq!(SPECIAL_FRAME_WINDOWS.len(), 24);
        assert_eq!(WEAPON_EMW.len(), 4);
    }
}
