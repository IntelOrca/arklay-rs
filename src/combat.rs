//! Shared combat tables and the weapon-damage pipeline.
//!
//! This module owns everything the shared damage layer needs that is not
//! per-monster behaviour:
//!
//! - [`CombatTables`] carries the per-weapon hit ranges, both playthroughs'
//!   hit records, the per-enemy hit-joint lists and hit reactions, the
//!   per-type init data (the reference type's health/stagger/animation-id
//!   tables and collision records), the shared player collision records and
//!   the entity-model path table. The converter decodes it from the player's
//!   own executable into the pack entry [`COMBAT_ENTRY`]; a pack without the
//!   entry (a mod, or an install the decoder cannot read) falls back to the
//!   built-in [`CombatTables::default`].
//! - [`GameState::apply_weapon_damage`] runs the original's per-shot pipeline:
//!   collect the active enemy list in the original's slot order, pick the
//!   closest candidate through the per-class hit detector, apply the
//!   line-of-sight rule, look the hit record up for the active playthrough,
//!   subtract the damage with a 16-bit wrap, compose `hit_state`, run the
//!   per-weapon post-hit callback (which dispatches the per-type reaction FX),
//!   then write the `state = 3` / surviving `state = 2` contract that the
//!   monster scripts consume.
//! - [`GameState::snapshot_enemies`] / [`GameState::clear_room_entities`]
//!   implement the saved-enemy snapshot the original builds when a door leads
//!   into another room: the outgoing room's live enemies are copied into the
//!   16-slot TTL table keyed by room and spawn-type byte, and the live list is
//!   torn down. [`GameState::spawn_enemy`] restores a matching snapshot when
//!   the record does not force a fresh spawn.
//! - [`GameState::apply_player_hurt`] and its siblings are the shared
//!   player-side damage helpers the attacking monsters call: the 16-bit
//!   health write with the clamp/lethal rules, the poison status/timer
//!   writes, and the grab pose the wasp scripts need.
//!
//! # Documented deviations
//!
//! - **The knife and the joint-attached gore.** The original measures the
//!   knife reach from the player's knife-hand joint world matrix and anchors
//!   several hit reactions on specific joints. The port has no game-layer
//!   joint matrices yet, so the knife origin is the player position plus the
//!   per-character reach offset rotated by the yaw, and the joint-attached
//!   gore spurts/tints are skipped. The hit/miss rules, ranges and damage
//!   stay exact.
//! - **Per-joint fudges in the head reaction.** The original clamps the
//!   lifted head blood against joint 1's world height; the port compares
//!   against the entity origin instead and documents the difference.
//! - **Death-flag timing.** The killing hit raises the room event bit in the
//!   shared pipeline instead of waiting for the monster's state-3 handler one
//!   frame later. Scripts that also raise it (the spider web does) are
//!   idempotent; the difference is invisible except to a script polling the
//!   bank on the exact frame after the shot.

use std::rc::Rc;

use anyhow::{Result, bail};

use crate::effects::Attach;
use crate::game::{
    BANK_ENEMIES, BANK_SCENARIO, ENTITY_COUNT, Entity, GameState, SCENARIO_FLAG_SECOND_PLAYTHROUGH,
};
use crate::state::RoomState;

/// The pack entry carrying the decoded combat tables.
pub const COMBAT_ENTRY: &str = "data/combat.bin";

/// Magic at the head of [`COMBAT_ENTRY`].
const COMBAT_MAGIC: &[u8; 4] = b"ARCB";
/// Format version of [`COMBAT_ENTRY`].
const COMBAT_VERSION: u16 = 1;

/// Weapon slots in the per-weapon tables (equipped item ids 1..=10).
pub const WEAPON_SLOTS: usize = 10;
/// Enemy type rows in the hit records and reaction tables.
pub const ENEMY_TYPE_COUNT: usize = 20;
/// Hit records per table (20 enemy types x 10 weapon slots).
pub const HIT_RECORD_ROWS: usize = ENEMY_TYPE_COUNT * WEAPON_SLOTS;
/// Entity-model path entries in the shared model table.
pub const MODEL_ENTRIES: usize = 106;
/// Bytes per entity-model path entry.
pub const MODEL_ENTRY_LEN: usize = 17;
/// Player/default collision records in the shared table.
pub const PLAYER_SCA_RECORDS: usize = 3;
/// Reference-type collision records in the init profile.
pub const ZOMBIE_SCA_RECORDS: usize = 2;
/// Saved-enemy snapshot slots the door transition walks.
pub const SAVED_ENEMY_SLOTS: usize = 16;
/// Snapshot TTL written for a live enemy.
pub const SAVED_ENEMY_TTL: u8 = 5;
/// Enemy count cap the original's list walk uses.
pub const ENEMY_LIST_LEN: usize = 30;

/// Bytes one encoded [`CombatTables`] occupies.
pub const COMBAT_BYTES: usize = 6
    + MODEL_ENTRIES * MODEL_ENTRY_LEN
    + WEAPON_SLOTS * 2 * 4
    + ENEMY_TYPE_COUNT
    + ENEMY_TYPE_COUNT * 6
    + HIT_RECORD_ROWS * 12
    + HIT_RECORD_ROWS * 12
    + ZOMBIE_SCA_RECORDS * 8 * 2
    + 16
    + 16
    + 32;

/// The player-hurt helper's clamp rule: a blow that would drop the health
/// below zero leaves the player at 1.
pub const PLAYER_HURT_CLAMP_TO_ONE: u16 = 0x0001;
/// With [`PLAYER_HURT_CLAMP_TO_ONE`], a flagged lethal blow resolves to the
/// scripted `health = -1` death instead of 1 (the big wasp's sting).
pub const PLAYER_HURT_LETHAL: u16 = 0x0002;
/// Poison duration written by the shared poison helper (`pad_174`).
pub const POISON_TIMER: u16 = 0x96;

/// The weapon class that picks the hit detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeaponClass {
    /// Radial reach measured from the knife-hand reach point (`weapon_id` 1).
    Knife,
    /// Two-tier aim cone in front of the player (`weapon_id` 2..=5).
    Gun,
    /// Radial reach measured from the projectile origin (`weapon_id` 6..=10).
    Projectile,
}

/// The post-hit callback that runs after the damage lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostHit {
    /// Knife: stab cue, reaction, and a per-character blood billboard.
    Knife,
    /// Handgun: the per-enemy reaction only.
    Reaction,
    /// Shotgun/pythons: long-range chip then the reaction.
    Shotgun,
    /// Flamethrower and the GL flame rounds: heavy blood FX.
    Blood,
    /// GL explosive rounds: joint-spurt blood FX.
    Blood2,
    /// GL acid rounds: sparks plus tint.
    Sparks,
    /// Rocket launcher: heavy blood FX then the joint spurts.
    Blood3,
}

/// The per-enemy-type hit reaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitReaction {
    /// No reaction at all (the web, the roots, the bosses).
    None,
    /// One blood billboard with the record's type/data bytes.
    Basic,
    /// Head blood plus the body billboard (the hunter).
    Head,
    /// Long-range blood with the random health restore (Plant 42).
    Blood,
    /// The zombie reaction: aim-gated head-pop plus body blood.
    Zombie,
}

/// One first-playthrough hit record; also the knockback and post-hit input
/// source for both playthroughs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HitRecordFirst {
    /// Knockback vector (local X/Y/Z offset for the hit billboard).
    pub knockback: [i16; 3],
    /// First-playthrough damage.
    pub damage: i16,
    /// Post-hit billboard type.
    pub effect_type: u8,
    /// Post-hit billboard depth group.
    pub effect_data: u8,
    /// First-playthrough hit-state byte.
    pub hit: u8,
}

/// One second-playthrough hit record (only damage and hit state are read).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HitRecordSecond {
    /// Second-playthrough damage.
    pub damage: i16,
    /// Second-playthrough hit-state byte.
    pub hit: u8,
}

/// The shared combat data tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombatTables {
    /// Hit ranges per weapon slot, first Chris' block then Jill's.
    pub weapon_ranges: [[u32; WEAPON_SLOTS]; 2],
    /// First-playthrough hit records, indexed `weapon + type * 10`.
    pub first_run: [HitRecordFirst; HIT_RECORD_ROWS],
    /// Second-playthrough hit records, same indexing.
    pub second_run: [HitRecordSecond; HIT_RECORD_ROWS],
    /// Per-enemy-type blood-spurt joint lists.
    pub hit_joints: [[u8; 6]; ENEMY_TYPE_COUNT],
    /// Per-enemy-type hit reactions.
    pub reactions: [HitReaction; ENEMY_TYPE_COUNT],
    /// Entity-model path entries (ASCII, NUL padded).
    pub model_entries: [[u8; MODEL_ENTRY_LEN]; MODEL_ENTRIES],
    /// The shared player/default collision records.
    pub player_sca: [[i16; 8]; PLAYER_SCA_RECORDS],
    /// The reference type's collision records (standard and variant).
    pub type_sca: [[i16; 8]; ZOMBIE_SCA_RECORDS],
    /// The reference type's health roll table.
    pub type_health: [u8; 16],
    /// The reference type's initial-animation table.
    pub type_anim: [u8; 16],
    /// The reference type's stagger budget table.
    pub type_stagger: [u8; 32],
}

impl Default for CombatTables {
    fn default() -> Self {
        let first_run = std::array::from_fn(|row| {
            let [kx, ky, kz, damage, effect_type, effect_data, hit] = DEFAULT_FIRST_RUN[row];
            HitRecordFirst {
                knockback: [kx, ky, kz],
                damage,
                effect_type: effect_type as u8,
                effect_data: effect_data as u8,
                hit: hit as u8,
            }
        });
        let second_run = std::array::from_fn(|row| {
            let [damage, hit] = DEFAULT_SECOND_RUN[row];
            HitRecordSecond {
                damage,
                hit: hit as u8,
            }
        });
        Self {
            weapon_ranges: DEFAULT_RANGES,
            first_run,
            second_run,
            hit_joints: DEFAULT_HIT_JOINTS,
            reactions: DEFAULT_REACTIONS,
            model_entries: default_model_entries(),
            player_sca: DEFAULT_PLAYER_SCA,
            type_sca: DEFAULT_ZOMBIE_SCA,
            type_health: DEFAULT_ZOMBIE_HEALTH,
            type_anim: DEFAULT_ZOMBIE_ANIM,
            type_stagger: DEFAULT_ZOMBIE_STAGGER,
        }
    }
}

/// The documented entity-model path block, rebuilt rather than embedded
/// because every entry follows from its slot index.
///
/// Each block is four player models, the 22 enemy models (`em1{block}xx`),
/// ten `em100a`/`em110a` fillers, then the 17 character models shared by both
/// blocks, ending in the two per-scenario outfit variants.
fn default_model_entries() -> [[u8; MODEL_ENTRY_LEN]; MODEL_ENTRIES] {
    let mut entries = [[0u8; MODEL_ENTRY_LEN]; MODEL_ENTRIES];
    fn put(entries: &mut [[u8; MODEL_ENTRY_LEN]; MODEL_ENTRIES], index: usize, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(MODEL_ENTRY_LEN);
        entries[index][..len].copy_from_slice(&bytes[..len]);
    }
    for block in 0..2usize {
        let base = block * 53;
        for (index, name) in ["char10", "char11", "char12", "char13"]
            .into_iter()
            .enumerate()
        {
            put(&mut entries, base + index, &format!("enemy/{name}.emd"));
        }
        for index in 0..=0x15usize {
            put(
                &mut entries,
                base + 4 + index,
                &format!("enemy/em1{block}{index:02x}.emd"),
            );
        }
        let filler = if block == 0 {
            "enemy/em100a.emd"
        } else {
            "enemy/em110a.emd"
        };
        for index in 0..10 {
            put(&mut entries, base + 26 + index, filler);
        }
        for index in 0x20..=0x2Eusize {
            put(
                &mut entries,
                base + 36 + (index - 0x20),
                &format!("enemy/em10{index:02x}.emd"),
            );
        }
        if block == 0 {
            put(&mut entries, base + 51, "enemy/em1030.emd");
            put(&mut entries, base + 52, "enemy/em1032.emd");
        } else {
            put(&mut entries, base + 51, "enemy/em1031.emd");
            put(&mut entries, base + 52, "enemy/em1033.emd");
        }
    }
    entries
}

impl CombatTables {
    /// Parse the packed table blob.
    pub fn parse(data: &[u8]) -> Result<Self> {
        crate::budget::check_len(
            data.len(),
            crate::budget::MAX_COMBAT_TABLE_BYTES,
            "combat table",
        )?;
        if data.len() != COMBAT_BYTES {
            bail!(
                "combat table is {} bytes, expected {COMBAT_BYTES}",
                data.len()
            );
        }
        if &data[..4] != COMBAT_MAGIC {
            bail!("combat table has no {:?} magic", COMBAT_MAGIC);
        }
        let version = u16::from_le_bytes([data[4], data[5]]);
        if version != COMBAT_VERSION {
            bail!("unsupported combat table version {version}");
        }
        let mut offset = 6;
        let models: [[u8; MODEL_ENTRY_LEN]; MODEL_ENTRIES] = std::array::from_fn(|_| {
            let entry = data[offset..offset + MODEL_ENTRY_LEN].try_into().unwrap();
            offset += MODEL_ENTRY_LEN;
            entry
        });
        let read_i16 = |slice: &mut usize| {
            let value = i16::from_le_bytes([data[*slice], data[*slice + 1]]);
            *slice += 2;
            value
        };
        let read_u32 = |slice: &mut usize| {
            let value = u32::from_le_bytes(data[*slice..*slice + 4].try_into().unwrap());
            *slice += 4;
            value
        };
        let weapon_ranges = std::array::from_fn(|_| std::array::from_fn(|_| read_u32(&mut offset)));
        let reactions = std::array::from_fn(|_| {
            let value = data[offset];
            offset += 1;
            parse_reaction(value)
        });
        let hit_joints = std::array::from_fn(|_| {
            let row: [u8; 6] = data[offset..offset + 6].try_into().unwrap();
            offset += 6;
            row
        });
        let first_run = std::array::from_fn(|_| {
            let record = HitRecordFirst {
                knockback: [
                    read_i16(&mut offset),
                    read_i16(&mut offset),
                    read_i16(&mut offset),
                ],
                damage: read_i16(&mut offset),
                effect_type: {
                    let value = data[offset];
                    offset += 1;
                    value
                },
                effect_data: {
                    let value = data[offset];
                    offset += 1;
                    value
                },
                hit: {
                    let value = data[offset];
                    offset += 1;
                    value
                },
            };
            // Skip the record's pad byte so the next record starts on its
            // 12-byte boundary.
            offset += 1;
            record
        });
        let second_run = std::array::from_fn(|_| {
            let damage = read_i16(&mut offset);
            let _unk = read_i16(&mut offset);
            let hit = {
                let value = data[offset];
                offset += 1;
                value
            };
            offset += 1;
            let _kx = read_i16(&mut offset);
            let _ky = read_i16(&mut offset);
            let _kz = read_i16(&mut offset);
            HitRecordSecond { damage, hit }
        });
        let type_sca: [[i16; 8]; ZOMBIE_SCA_RECORDS] =
            std::array::from_fn(|_| std::array::from_fn(|_| read_i16(&mut offset)));
        let type_health: [u8; 16] = std::array::from_fn(|_| {
            let value = data[offset];
            offset += 1;
            value
        });
        let type_anim: [u8; 16] = std::array::from_fn(|_| {
            let value = data[offset];
            offset += 1;
            value
        });
        let type_stagger: [u8; 32] = std::array::from_fn(|_| {
            let value = data[offset];
            offset += 1;
            value
        });
        // The player/default collision records are not part of the encoded
        // payload: they are fixed shared data, kept synthetic in the fallback
        // and in the decoder's output alike.
        Ok(Self {
            weapon_ranges,
            first_run,
            second_run,
            hit_joints,
            reactions,
            model_entries: models,
            player_sca: DEFAULT_PLAYER_SCA,
            type_sca,
            type_health,
            type_anim,
            type_stagger,
        })
    }

    /// Serialize the tables into the packed layout.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(COMBAT_BYTES);
        out.extend_from_slice(COMBAT_MAGIC);
        out.extend_from_slice(&COMBAT_VERSION.to_le_bytes());
        for entry in &self.model_entries {
            out.extend_from_slice(entry);
        }
        for block in &self.weapon_ranges {
            for range in block {
                out.extend_from_slice(&range.to_le_bytes());
            }
        }
        for reaction in &self.reactions {
            out.push(encode_reaction(*reaction));
        }
        for row in &self.hit_joints {
            out.extend_from_slice(row);
        }
        for record in &self.first_run {
            out.extend_from_slice(&record.knockback[0].to_le_bytes());
            out.extend_from_slice(&record.knockback[1].to_le_bytes());
            out.extend_from_slice(&record.knockback[2].to_le_bytes());
            out.extend_from_slice(&record.damage.to_le_bytes());
            out.push(record.effect_type);
            out.push(record.effect_data);
            out.push(record.hit);
            out.push(0);
        }
        for record in &self.second_run {
            out.extend_from_slice(&record.damage.to_le_bytes());
            out.extend_from_slice(&0i16.to_le_bytes());
            out.push(record.hit);
            out.push(0);
            out.extend_from_slice(&0i16.to_le_bytes());
            out.extend_from_slice(&0i16.to_le_bytes());
            out.extend_from_slice(&0i16.to_le_bytes());
        }
        for record in &self.type_sca {
            for value in record {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        out.extend_from_slice(&self.type_health);
        out.extend_from_slice(&self.type_anim);
        out.extend_from_slice(&self.type_stagger);
        debug_assert_eq!(out.len(), COMBAT_BYTES);
        out
    }

    /// Load the tables from `pack`, falling back to the built-in table when
    /// the entry is absent or unreadable (a mod or a foreign install).
    pub fn load(pack: &crate::pack::Pack) -> Self {
        match pack.read(COMBAT_ENTRY) {
            Ok(data) => match Self::parse(data) {
                Ok(tables) => tables,
                Err(err) => {
                    eprintln!(
                        "warning: invalid {COMBAT_ENTRY}: {err:#}; using the built-in combat tables"
                    );
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// The hit range for a weapon slot (`weapon_adj = weapon_id - 1`) and
    /// player character (`0` Chris, `1` Jill; the mask makes Rebecca read
    /// Jill's block).
    pub fn weapon_range(&self, weapon_adj: u8, player: u8) -> Option<u32> {
        if usize::from(weapon_adj) >= WEAPON_SLOTS {
            return None;
        }
        let block = usize::from(player & 1);
        self.weapon_ranges[block]
            .get(usize::from(weapon_adj))
            .copied()
    }

    /// The first-playthrough record for a weapon slot and enemy type.
    pub fn hit_record(&self, weapon_adj: u8, enemy_type: u8) -> Option<HitRecordFirst> {
        self.record_index(weapon_adj, enemy_type)
            .and_then(|index| self.first_run.get(index).copied())
    }

    /// The second-playthrough record for a weapon slot and enemy type.
    pub fn hit_record_second(&self, weapon_adj: u8, enemy_type: u8) -> Option<HitRecordSecond> {
        self.record_index(weapon_adj, enemy_type)
            .and_then(|index| self.second_run.get(index).copied())
    }

    fn record_index(&self, weapon_adj: u8, enemy_type: u8) -> Option<usize> {
        if usize::from(weapon_adj) >= WEAPON_SLOTS || usize::from(enemy_type) >= ENEMY_TYPE_COUNT {
            return None;
        }
        Some(usize::from(weapon_adj) + usize::from(enemy_type) * WEAPON_SLOTS)
    }

    /// The per-enemy-type hit reaction.
    pub fn reaction(&self, enemy_type: u8) -> HitReaction {
        self.reactions
            .get(usize::from(enemy_type))
            .copied()
            .unwrap_or(HitReaction::None)
    }

    /// The per-enemy-type blood-spurt joint list.
    pub fn hit_joints(&self, enemy_type: u8) -> [u8; 6] {
        self.hit_joints
            .get(usize::from(enemy_type))
            .copied()
            .unwrap_or([0; 6])
    }

    /// The entity-model file name for entity `id` in the given scenario
    /// (`0` Chris, `1` Jill), exactly as the original's `(id + 4)` model-table
    /// index resolves it.
    pub fn model_entry(&self, id: u8, player: u8) -> Option<&str> {
        if id >= 0x30 {
            return None;
        }
        let index = usize::from(player & 1) * 53 + usize::from(id) + 4;
        let entry = self.model_entries.get(index)?;
        let end = entry
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(MODEL_ENTRY_LEN);
        std::str::from_utf8(&entry[..end]).ok()
    }
}

fn parse_reaction(value: u8) -> HitReaction {
    match value {
        1 => HitReaction::Basic,
        2 => HitReaction::Head,
        3 => HitReaction::Blood,
        4 => HitReaction::Zombie,
        _ => HitReaction::None,
    }
}

fn encode_reaction(reaction: HitReaction) -> u8 {
    match reaction {
        HitReaction::None => 0,
        HitReaction::Basic => 1,
        HitReaction::Head => 2,
        HitReaction::Blood => 3,
        HitReaction::Zombie => 4,
    }
}

/// The hit detector class for an equipped item id (`1`..=`10`).
pub fn weapon_class(weapon_id: u8) -> Option<WeaponClass> {
    match weapon_id {
        1 => Some(WeaponClass::Knife),
        2..=5 => Some(WeaponClass::Gun),
        6..=10 => Some(WeaponClass::Projectile),
        _ => None,
    }
}

/// The post-hit callback class for a weapon slot (`weapon_id - 1`).
pub fn post_hit_class(weapon_adj: u8) -> Option<PostHit> {
    match weapon_adj {
        0 => Some(PostHit::Knife),
        1 => Some(PostHit::Reaction),
        2..=4 => Some(PostHit::Shotgun),
        5 | 8 => Some(PostHit::Blood),
        6 => Some(PostHit::Blood2),
        7 => Some(PostHit::Sparks),
        9 => Some(PostHit::Blood3),
        _ => None,
    }
}

/// One saved enemy state: the original's 28-byte door snapshot record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SavedEnemyState {
    /// `status_flags & 0x0F`, or zero when the enemy was dead without a
    /// death event.
    pub status_flags: u8,
    /// The enemy's behaviour byte.
    pub behavior_flags: u8,
    /// Room the enemy belonged to.
    pub room: u8,
    /// The entity's spawn type byte (`variant`), the restore key's high half.
    pub enemy_type: u8,
    /// The enemy's state byte.
    pub state: u8,
    /// Position, truncated to the stored words.
    pub pos: [i16; 3],
    /// Yaw.
    pub angle: u16,
    /// TTL: non-zero means occupied.
    pub valid: u8,
}

impl SavedEnemyState {
    /// Whether the slot holds a live snapshot.
    pub fn occupied(&self) -> bool {
        self.valid != 0
    }
}

/// Integer square root of the original's `SquareRoot0` for a non-negative
/// value.
fn square_root0(value: u32) -> u32 {
    value.isqrt()
}

/// `MovePlayerXZ`: rotate the local offset `(x, 0, z)` by the yaw and narrow
/// each component to the original's `short`.
fn move_player_xz(angle: u16, offset: [i16; 3]) -> [i16; 3] {
    let (x, z) = crate::player::rotate_xz(angle, i32::from(offset[0]), i32::from(offset[2]));
    [x as i16, 0, z as i16]
}

/// `checkEntityInRangeCone`: is `dist` inside the triangle bounded by
/// `left`/`right` (local cone edges) with the aim direction `aim`? Keeps the
/// closest candidate in `displacement`.
fn check_entity_in_range_cone(
    aim: [i16; 3],
    dist: [i32; 2],
    left: [i16; 3],
    right: [i16; 3],
    displacement: &mut u32,
) -> bool {
    let cross = |x1: i32, z1: i32, x2: i32, z2: i32| z2 * x1 - x2 * z1;
    let (ax, az) = (i32::from(aim[0]), i32::from(aim[2]));
    let (lx, lz) = (i32::from(left[0]), i32::from(left[2]));
    let (rx, rz) = (i32::from(right[0]), i32::from(right[2]));

    if cross(ax, az, dist[0] - lx, dist[1] - lz) > 0 {
        return false;
    }
    if cross(ax, az, dist[0] - rx, dist[1] - rz) < 0 {
        return false;
    }
    if cross(lx - rx, lz - rz, dist[0] - ax, dist[1] - az) > 0 {
        return false;
    }
    let dx = dist[0].unsigned_abs();
    let dz = dist[1].unsigned_abs();
    let distance = square_root0(dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz)));
    if distance < *displacement {
        *displacement = distance;
        return true;
    }
    false
}

/// `weapon_hit_detect_gun`: the two-tier aim cone.
fn detect_gun(
    aim_flags: u8,
    weapon_adj: u8,
    candidate: &Entity,
    range: i16,
    player_angle: u16,
    player_pos: [i32; 3],
    displacement: &mut u32,
) -> bool {
    let enemy_radius = i32::from(candidate.sca_radius);
    let enemy_id = candidate.id;
    if matches!(enemy_id, 0x00 | 0x01 | 0x11) && aim_flags & 0x80 != 0 && weapon_adj != 2 {
        return false;
    }
    let mut range = i32::from(range);
    if enemy_id == 0x04 {
        range -= 1000;
    }
    if enemy_id == 0x08 {
        range += 2000;
    }
    let height = if aim_flags & 0x20 != 0 { 0 } else { 50 };
    let near_left = move_player_xz(
        player_angle,
        [height as i16, 0, (enemy_radius + 200) as i16],
    );
    let near_right = move_player_xz(
        player_angle,
        [height as i16, 0, (-200 - enemy_radius) as i16],
    );
    let far_left = move_player_xz(player_angle, [0x28A, 0, (enemy_radius + range) as i16]);
    let far_right = move_player_xz(player_angle, [0x28A, 0, (-(enemy_radius + range)) as i16]);
    let aim = move_player_xz(player_angle, [0x28A, 0, 0]);
    let dist = [
        candidate.pos[0] - player_pos[0],
        candidate.pos[2] - player_pos[2],
    ];
    if check_entity_in_range_cone(aim, dist, near_left, near_right, displacement) {
        return true;
    }
    check_entity_in_range_cone(aim, dist, far_left, far_right, displacement)
}

/// `weapon_hit_detect_knife`: radial reach from the player's knife-hand
/// reach point (the port anchors it at the player origin plus the
/// per-character reach offset rotated by the yaw).
fn detect_knife(
    aim_flags: u8,
    candidate: &Entity,
    range: i16,
    player_character: u8,
    player_angle: u16,
    player_pos: [i32; 3],
    displacement: &mut u32,
) -> bool {
    let offset: i16 = if player_character & 1 == 0 {
        0x99
    } else {
        -0x17C
    };
    let (reach_x, reach_z) = crate::player::rotate_speed(player_angle, 0, i32::from(offset));
    let knife = [player_pos[0] + reach_x, player_pos[2] + reach_z];
    let dist = [candidate.pos[0] - knife[0], candidate.pos[2] - knife[1]];

    let enemy_id = candidate.id;
    if matches!(enemy_id, 0x00 | 0x01 | 0x11) && aim_flags & 0x80 != 0 {
        return false;
    }
    if ((enemy_id == 0x02 && candidate.pos[1] > -400) || enemy_id == 0x03) && aim_flags & 0x40 != 0
    {
        return false;
    }
    if matches!(enemy_id, 0x05 | 0x07 | 0x09) && candidate.pos[1] < -4800 {
        return false;
    }

    let radius = i32::from(candidate.sca_radius);
    let mut effective = (radius as u32).wrapping_add(i32::from(range) as u32);
    if enemy_id == 0x04 {
        effective = effective.wrapping_sub(800);
    }
    if enemy_id == 0x08 {
        effective = effective.wrapping_add(2000);
    }
    if matches!(enemy_id, 0x07 | 0x0A) {
        effective = effective.wrapping_add(100);
    }

    let dx = dist[0].unsigned_abs();
    let dz = dist[1].unsigned_abs();
    let distance = square_root0(dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz)));
    if distance < effective && distance < *displacement {
        *displacement = distance;
        return true;
    }
    false
}

/// `weapon_hit_detect_projectile`: radial reach measured from the parked
/// projectile origin.
fn detect_projectile(
    origin: [i32; 3],
    candidate: &Entity,
    range: i16,
    displacement: &mut u32,
) -> bool {
    let dist = [candidate.pos[0] - origin[0], candidate.pos[2] - origin[2]];
    let radius = u32::from(candidate.sca_radius as u16);
    let mut effective = radius.wrapping_add(i32::from(range) as u32);
    if candidate.id == 0x04 {
        effective = effective.wrapping_sub(1000);
    }
    let dx = dist[0].unsigned_abs();
    let dz = dist[1].unsigned_abs();
    let distance = square_root0(dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz)));
    if distance < effective && distance < *displacement {
        *displacement = distance;
        return true;
    }
    false
}

/// `check_weapon_line_of_sight`: walk the room's four sight-blocking layers
/// from the player to the hit point.
fn check_weapon_line_of_sight(game: &GameState, room: &RoomState, target: [i32; 3]) -> u8 {
    let player = &game.entities[0];
    let dir = [
        target[0] - player.pos[0],
        target[1] - player.pos[1],
        target[2] - player.pos[2],
    ];
    let mut blocked = 0u8;
    for layer in (0..=3u8).rev() {
        blocked |=
            crate::enemy::walk::room_check_sight_blocked(room, player, player.pos, dir, layer);
    }
    blocked
}

/// The hit-billboard yaw: player facing minus enemy facing plus the 0x800
/// angle convention offset.
fn hit_billboard_rot(game: &GameState, enemy: &Entity) -> i16 {
    (game.entities[0].angle)
        .wrapping_sub(enemy.angle)
        .wrapping_add(0x800) as i16
}

/// Spawn one hit billboard anchored to `slot`'s entity matrix with `offset`
/// as its local position.
fn spawn_billboard(
    game: &mut GameState,
    slot: usize,
    effect_type: u8,
    depth: u8,
    offset: [i32; 3],
    yaw: i16,
) {
    let room_effects = Rc::clone(&game.room_effects);
    let _ = crate::effects::create_attached(
        game,
        &room_effects,
        effect_type,
        depth,
        Attach::Entity(slot as u8),
        offset,
        yaw,
        0,
    );
}

/// Spawn one hit billboard anchored to the player's matrix (the knife-hand
/// blood the port cannot place in the hand joint's space).
fn spawn_player_billboard(
    game: &mut GameState,
    effect_type: u8,
    depth: u8,
    offset: [i32; 3],
    yaw: i16,
) {
    let room_effects = Rc::clone(&game.room_effects);
    let _ = crate::effects::create_attached(
        game,
        &room_effects,
        effect_type,
        depth,
        Attach::Player,
        offset,
        yaw,
        0,
    );
}

/// Queue the enemy-bank cue `id` at `slot`'s position (the monster `Snd_em`).
fn queue_enemy_cue(game: &mut GameState, room: &RoomState, slot: usize, id: u8) {
    let group = (game.entities[slot].variant >> 4) & 0x7;
    if let Some((name, column)) = crate::sfx::enemy_sound(room, id, group) {
        let pos = game.entities[slot].pos;
        game.entity_sounds.push(crate::game::EntitySound {
            name,
            bank: 2,
            column,
            pos,
        });
    }
}

impl GameState {
    /// Set the shared combat tables, replacing the built-in fallback. The
    /// engine calls this once per session after reading the pack.
    pub fn set_combat_tables(&mut self, tables: CombatTables) {
        self.combat = tables;
    }

    /// Fire `weapon_id` through the shared damage pipeline from the player's
    /// position with the live aim flags.
    ///
    /// This is the Rust entry the weapon runtime and the tests call; the CLI
    /// binds no key to it. Returns `1` when a monster was hit, `0` on a miss,
    /// or the target's id when the closest candidate is an NPC id
    /// (`>= 0x14`), which takes no damage.
    pub fn apply_weapon_damage(&mut self, room: &RoomState, weapon_id: u8) -> u8 {
        let origin = self.entities[0].pos;
        let flags = self.player_flags;
        self.apply_weapon_damage_with(room, weapon_id, origin, flags)
    }

    /// [`GameState::apply_weapon_damage`] with an explicit projectile origin
    /// and aim-flag snapshot, mirroring the effect behaviours that park the
    /// flame/rocket position and stage the player's aim byte before firing.
    pub fn apply_weapon_damage_with(
        &mut self,
        room: &RoomState,
        weapon_id: u8,
        projectile_origin: [i32; 3],
        aim_flags: u8,
    ) -> u8 {
        let Some(class) = weapon_class(weapon_id) else {
            return 0;
        };
        let count = usize::from(self.enemy_count).min(ENEMY_LIST_LEN);
        if count == 0 {
            return 0;
        }

        // First pass: collect the active list indices in the original's
        // fill order (it writes from the end backwards, and the candidate
        // walk then starts at slot [0]).
        let mut found = Vec::with_capacity(count);
        for index in 0..count {
            let slot = 1 + index;
            if slot >= ENTITY_COUNT {
                break;
            }
            if self.entities[slot].status_flags != 0 {
                found.push(index as u8);
            }
        }
        let n = found.len();
        if n == 0 {
            return 0;
        }
        let mut active = vec![0u8; n];
        for (position, &index) in found.iter().enumerate() {
            active[n - 1 - position] = index;
        }
        let mut order = Vec::with_capacity(n);
        order.push(active[0]);
        for position in (1..n).rev() {
            order.push(active[position]);
        }

        let weapon_adj = weapon_id - 1;
        let player_character = self.id.player_flag & 1;
        let range = self
            .combat
            .weapon_range(weapon_adj, player_character)
            .unwrap_or(0) as i16;
        let player_angle = self.entities[0].angle;
        let player_pos = self.entities[0].pos;
        let mut displacement = 0x7FFF_FFFFu32;
        let mut enemy_slot: Option<usize> = None;

        for &index in &order {
            let slot = 1 + usize::from(index);
            let candidate = self.entities[slot];
            if (candidate.status_flags & aim_flags & 0xE0) == 0 || candidate.hit_state != 0 {
                continue;
            }
            let hit = match class {
                WeaponClass::Knife => detect_knife(
                    aim_flags,
                    &candidate,
                    range,
                    player_character,
                    player_angle,
                    player_pos,
                    &mut displacement,
                ),
                WeaponClass::Gun => detect_gun(
                    aim_flags,
                    weapon_adj,
                    &candidate,
                    range,
                    player_angle,
                    player_pos,
                    &mut displacement,
                ),
                WeaponClass::Projectile => {
                    detect_projectile(projectile_origin, &candidate, range, &mut displacement)
                }
            };
            if hit {
                enemy_slot = Some(slot);
            }
        }

        let Some(slot) = enemy_slot else {
            return 0;
        };
        let enemy_pos = self.entities[slot].pos;
        if weapon_adj < 5 && check_weapon_line_of_sight(self, room, enemy_pos) != 0 {
            return 0;
        }
        let enemy_type = self.entities[slot].id;
        if enemy_type >= 0x14 {
            return enemy_type;
        }
        let Some(record) = self.combat.hit_record(weapon_adj, enemy_type) else {
            return 0;
        };
        let (damage, hit_byte) =
            if self.flags[usize::from(BANK_SCENARIO)].bit(SCENARIO_FLAG_SECOND_PLAYTHROUGH) {
                match self.combat.hit_record_second(weapon_adj, enemy_type) {
                    Some(second) => (second.damage, second.hit),
                    None => (record.damage, record.hit),
                }
            } else {
                (record.damage, record.hit)
            };

        let pre_health = self.entities[slot].health;
        self.entities[slot].health = pre_health.wrapping_sub(damage);
        let mut hit_state = hit_byte;
        if aim_flags & 0xE0 != 0x20 {
            hit_state = hit_state.wrapping_add(aim_flags >> 5);
        }
        hit_state |= weapon_id.wrapping_mul(8);
        self.entities[slot].hit_state = hit_state;

        self.apply_post_hit(
            room,
            slot,
            weapon_adj,
            weapon_id,
            record,
            displacement,
            aim_flags,
            pre_health,
        );

        self.entities[slot].set_state(3);
        self.entities[slot].set_ignore(0);
        self.entities[slot].action_behavior = 0;
        self.entities[slot].action_state = 0;
        if self.entities[slot].health >= 0 {
            self.entities[slot].set_state(2);
        } else if self.entities[slot].death_event_id != 0xFF {
            let bit = self.entities[slot].death_event_id;
            self.flags[usize::from(BANK_ENEMIES)].apply(bit, 0);
        }
        1
    }

    /// The per-weapon post-hit callback, with the per-enemy reaction
    /// dispatch it drives.
    #[allow(clippy::too_many_arguments)]
    fn apply_post_hit(
        &mut self,
        room: &RoomState,
        slot: usize,
        weapon_adj: u8,
        weapon_id: u8,
        record: HitRecordFirst,
        displacement: u32,
        aim_flags: u8,
        pre_health: i16,
    ) {
        let yaw = hit_billboard_rot(self, &self.entities[slot]);
        let offset = [
            i32::from(record.knockback[0]),
            i32::from(record.knockback[1]),
            i32::from(record.knockback[2]),
        ];
        match post_hit_class(weapon_adj) {
            Some(PostHit::Knife) => {
                // The original's bank-1 stab cue and the knife-hand blood
                // billboard use the weapon-joint space the port does not
                // model; the billboard anchors to the player instead.
                if record.effect_type != 1 {
                    spawn_player_billboard(self, record.effect_type, record.effect_data, offset, 0);
                }
                self.dispatch_reaction(
                    room,
                    slot,
                    record,
                    displacement,
                    weapon_adj,
                    aim_flags,
                    pre_health,
                    yaw,
                );
            }
            Some(PostHit::Reaction) => {
                self.dispatch_reaction(
                    room,
                    slot,
                    record,
                    displacement,
                    weapon_adj,
                    aim_flags,
                    pre_health,
                    yaw,
                );
            }
            Some(PostHit::Shotgun) => {
                if weapon_adj == 2 && displacement > 9000 {
                    self.entities[slot].health = self.entities[slot].health.wrapping_add(10);
                    self.entities[slot].hit_state = self.entities[slot].hit_state.wrapping_sub(1);
                }
                self.dispatch_reaction(
                    room,
                    slot,
                    record,
                    displacement,
                    weapon_adj,
                    aim_flags,
                    pre_health,
                    yaw,
                );
            }
            Some(PostHit::Blood) => {
                self.blood_fx(slot, record, aim_flags, yaw);
                self.dispatch_reaction(
                    room,
                    slot,
                    record,
                    displacement,
                    weapon_adj,
                    aim_flags,
                    pre_health,
                    yaw,
                );
            }
            Some(PostHit::Blood2) => {
                self.blood2_fx(slot, record, aim_flags, yaw);
                self.dispatch_reaction(
                    room,
                    slot,
                    record,
                    displacement,
                    weapon_adj,
                    aim_flags,
                    pre_health,
                    yaw,
                );
            }
            Some(PostHit::Sparks) => {
                self.sparks_fx(slot, record, aim_flags, yaw);
                self.dispatch_reaction(
                    room,
                    slot,
                    record,
                    displacement,
                    weapon_adj,
                    aim_flags,
                    pre_health,
                    yaw,
                );
            }
            Some(PostHit::Blood3) => {
                self.blood3_fx(slot, record, aim_flags, yaw);
                self.blood2_fx(slot, record, aim_flags, yaw);
                self.dispatch_reaction(
                    room,
                    slot,
                    record,
                    displacement,
                    weapon_adj,
                    aim_flags,
                    pre_health,
                    yaw,
                );
            }
            None => {}
        }
        let _ = weapon_id;
    }

    /// The per-enemy reaction the post-hit callbacks dispatch.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_reaction(
        &mut self,
        room: &RoomState,
        slot: usize,
        record: HitRecordFirst,
        displacement: u32,
        weapon_adj: u8,
        aim_flags: u8,
        pre_health: i16,
        yaw: i16,
    ) {
        let enemy_type = self.entities[slot].id;
        let offset = [
            i32::from(record.knockback[0]),
            i32::from(record.knockback[1]),
            i32::from(record.knockback[2]),
        ];
        match self.combat.reaction(enemy_type) {
            HitReaction::None => {}
            HitReaction::Basic => {
                spawn_billboard(
                    self,
                    slot,
                    record.effect_type,
                    record.effect_data,
                    offset,
                    yaw,
                );
            }
            HitReaction::Head => {
                let mut offset = offset;
                if aim_flags & 0x20 != 0 {
                    offset[1] += 1000;
                }
                spawn_billboard(self, slot, 3, 8, offset, yaw);
                spawn_billboard(
                    self,
                    slot,
                    record.effect_type,
                    record.effect_data,
                    offset,
                    yaw,
                );
            }
            HitReaction::Blood => {
                if weapon_adj != 0 {
                    if displacement > 7000
                        && crate::game::platform_rand(&mut self.rand_state) & 1 != 0
                    {
                        self.entities[slot].health = pre_health;
                    }
                    spawn_billboard(self, slot, 0, 0x18, offset, yaw);
                }
            }
            HitReaction::Zombie => {
                self.zombie_reaction(room, slot, record, displacement, weapon_adj, aim_flags, yaw);
            }
        }
    }

    /// The zombie reaction: the aim-gated instant head-pop (shotgun point
    /// blank, python on the level), the head/body billboards and the Jill
    /// handgun chip. The joint-attached head gore and the joint tint are
    /// deferred with the rest of the joint plumbing.
    #[allow(clippy::too_many_arguments)]
    fn zombie_reaction(
        &mut self,
        room: &RoomState,
        slot: usize,
        record: HitRecordFirst,
        displacement: u32,
        weapon_adj: u8,
        aim_flags: u8,
        yaw: i16,
    ) {
        let mut offset = [
            i32::from(record.knockback[0]),
            i32::from(record.knockback[1]),
            i32::from(record.knockback[2]),
        ];
        let enemy_type = self.entities[slot].id;
        if enemy_type != 0x02 {
            let head_pop = ((weapon_adj == 2 && displacement < 3000) && aim_flags & 0xC0 != 0)
                || (matches!(weapon_adj, 3 | 4) && aim_flags & 0x40 != 0);
            if head_pop {
                self.entities[slot].health = -300;
                if self.entities[slot].death_event_id != 0xFF {
                    let bit = self.entities[slot].death_event_id;
                    self.flags[usize::from(BANK_ENEMIES)].apply(bit, 0);
                }
                queue_enemy_cue(self, room, slot, 6);
                // `joint_setup_attack_effect(head, 30, 2, 3)`: clear the head
                // joint's active bit and set `0x28`, the gore state the
                // death/get-up paths read (`& 0xCC`, `& 0x40`).
                let head = 2usize;
                if self.entities[slot].joint_flag(head) & 1 != 0 {
                    let value = (self.entities[slot].joint_flag(head) & !1) | 0x28;
                    self.entities[slot].set_joint_flag(head, value);
                }
                let room_effects = Rc::clone(&self.room_effects);
                let _ = crate::effects::create_attached(
                    self,
                    &room_effects,
                    0,
                    3,
                    Attach::Joint(slot as u8, head as u8),
                    [100, -600, 0],
                    0,
                    0,
                );
                offset[1] = record.knockback[1].wrapping_sub(0x78) as i32;
                spawn_billboard(self, slot, 3, 0, offset, yaw);
                offset[1] = offset[1].wrapping_add(0x78);
            }
            if aim_flags & 0x20 != 0 {
                if self.entities[slot].behavior_flags & 2 != 0 {
                    offset[1] = -500;
                } else {
                    offset[1] = -600;
                }
            }
        }
        if self.id.player_flag & 1 != 0 && weapon_adj == 1 {
            let health = self.entities[slot].health;
            self.entities[slot].health = if enemy_type == 0x02 {
                health.wrapping_sub(7)
            } else {
                health.wrapping_sub(3)
            };
        }
        let mut effect_type = record.effect_type;
        if effect_type == 4 {
            if displacement < 9000 {
                for depth in (1..=4u8).rev() {
                    let rot = (i16::from(depth).wrapping_add(5))
                        .wrapping_mul(0x100)
                        .wrapping_sub(self.entities[slot].angle as i16)
                        .wrapping_add(self.entities[0].angle as i16);
                    spawn_billboard(self, slot, 4, depth, offset, rot);
                }
            }
            // The original zeroes the shared effect-type scratch, so the
            // final body billboard spawns as type 0.
            effect_type = 0;
            self.entities[slot].hit_state &= !0xE0;
        }
        spawn_billboard(self, slot, effect_type, record.effect_data, offset, yaw);
    }

    /// `weapon_post_hit_blood`: the flamethrower/flame-round FX pair.
    fn blood_fx(&mut self, slot: usize, record: HitRecordFirst, aim_flags: u8, yaw: i16) {
        if record.effect_type == 1 {
            return;
        }
        let mut offset = [
            i32::from(record.knockback[0]),
            i32::from(record.knockback[1]),
            i32::from(record.knockback[2]),
        ];
        if record.knockback[0] == 0x96 && aim_flags & 0x20 != 0 {
            offset[1] += 1000;
        }
        spawn_billboard(self, slot, 0x0e, record.effect_data, offset, yaw);
        spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
        if !matches!(self.entities[slot].id, 0x07 | 0x0A | 0x08) {
            offset[1] -= 500;
            spawn_billboard(self, slot, 0x0e, 0x03, offset, yaw);
            spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
        }
        if self.entities[slot].health < 0
            && record.effect_type == 0x0e
            && !matches!(self.entities[slot].id, 0x05 | 0x07 | 0x0A)
        {
            {
                offset = [0, 0, 0];
                offset[0] = -100;
                offset[2] = -300;
                spawn_billboard(self, slot, 0x0e, 0x06, offset, yaw);
                spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
                offset[0] = 100;
                offset[2] = 300;
                spawn_billboard(self, slot, 0x0e, 0x06, offset, yaw);
                spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
            }
        }
    }

    /// `weapon_post_hit_blood2`: the explosive-round FX; the joint spurts
    /// are deferred with the joint plumbing.
    fn blood2_fx(&mut self, slot: usize, record: HitRecordFirst, aim_flags: u8, yaw: i16) {
        if record.effect_type == 1 {
            return;
        }
        let mut offset = [
            i32::from(record.knockback[0]),
            i32::from(record.knockback[1]),
            i32::from(record.knockback[2]),
        ];
        if record.knockback[0] == 0x96 && aim_flags & 0x20 != 0 {
            offset[1] += 1000;
        }
        spawn_billboard(self, slot, 0x0e, 7, offset, yaw);
        spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
        spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
        if self.entities[slot].id != 0x08 {
            offset[1] -= 200;
            spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
            spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
        }
    }

    /// `weapon_post_hit_sparks`: the acid-round FX.
    fn sparks_fx(&mut self, slot: usize, record: HitRecordFirst, aim_flags: u8, yaw: i16) {
        if record.effect_type == 1 {
            return;
        }
        let mut offset = [
            i32::from(record.knockback[0]),
            i32::from(record.knockback[1]),
            i32::from(record.knockback[2]),
        ];
        if record.knockback[0] == 0x96 && aim_flags & 0x20 != 0 {
            offset[1] += 1000;
        }
        spawn_billboard(self, slot, 0x09, 0, offset, yaw);
        spawn_billboard(self, slot, 0x09, 0, offset, yaw);
        if self.entities[slot].health < 0
            && record.effect_type == 9
            && !matches!(self.entities[slot].id, 0x05 | 0x07 | 0x0A | 0x08)
        {
            {
                offset = [0, 0, 0];
                offset[0] = -100;
                offset[2] = -300;
                spawn_billboard(self, slot, 0x09, 0, offset, yaw);
                spawn_billboard(self, slot, 0x09, 1, offset, yaw);
                offset[0] = 100;
                offset[2] = 300;
                spawn_billboard(self, slot, 0x09, 0, offset, yaw);
                spawn_billboard(self, slot, 0x09, 1, offset, yaw);
            }
        }
    }

    /// `weapon_post_hit_blood3`: the rocket FX; the joint spurts/tint are
    /// deferred with the joint plumbing.
    fn blood3_fx(&mut self, slot: usize, record: HitRecordFirst, aim_flags: u8, yaw: i16) {
        if record.effect_type == 1 {
            return;
        }
        let mut offset = [
            i32::from(record.knockback[0]),
            i32::from(record.knockback[1]),
            i32::from(record.knockback[2]),
        ];
        if record.knockback[0] == 0x96 && aim_flags & 0x20 != 0 {
            offset[1] += 1000;
        }
        spawn_billboard(self, slot, 0x0e, record.effect_data, offset, yaw);
        spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
        if !matches!(self.entities[slot].id, 0x07 | 0x0A | 0x08) {
            offset[1] -= 500;
            spawn_billboard(self, slot, 0x0e, 0x03, offset, yaw);
            spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
        }
        if self.entities[slot].health < 0
            && record.effect_type != 2
            && !matches!(self.entities[slot].id, 0x05 | 0x07 | 0x0A)
        {
            {
                offset = [0, 0, 0];
                offset[0] = -100;
                offset[2] = -300;
                spawn_billboard(self, slot, 0x0e, 0x06, offset, yaw);
                spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
                offset[0] = 100;
                offset[2] = 300;
                spawn_billboard(self, slot, 0x0e, 0x06, offset, yaw);
                spawn_billboard(self, slot, 0x09, 0x0d, offset, yaw);
            }
        }
    }

    /// The shared player-hurt helper: subtract `damage` from the player's
    /// health with a 16-bit wrap and mirror the result into the BioCard copy.
    ///
    /// `flags` is a bitmask of [`PLAYER_HURT_CLAMP_TO_ONE`] (a killing blow
    /// leaves the player at 1) and [`PLAYER_HURT_LETHAL`] (with the clamp, a
    /// flagged lethal blow resolves to the scripted `-1` death). Returns the
    /// written health.
    pub fn apply_player_hurt(&mut self, damage: i16, flags: u16) -> i16 {
        let mut health = self.entities[0].health.wrapping_sub(damage);
        if flags & PLAYER_HURT_CLAMP_TO_ONE != 0 && health < 0 {
            health = 1;
            if flags & PLAYER_HURT_LETHAL != 0 {
                health = -1;
            }
        }
        self.entities[0].health = health;
        self.set_health_copy(health);
        health
    }

    /// The shared poison write: raise the poison status bit and reset the
    /// 150-frame poison timer.
    pub fn poison_player(&mut self) {
        let status = self.health_status | 0x02;
        self.set_health_status(status);
        self.poison_timer = POISON_TIMER;
    }

    /// The shared grab pose the wasp scripts use: pin the player's animation
    /// offsets and the grabbing entity's offsets to the player's position,
    /// set the attacked flag and the grabbed animation. `slot` is the
    /// grabbing enemy's entity slot.
    pub fn grab_player(&mut self, slot: usize) {
        let player_pos = self.entities[0].pos;
        let x = player_pos[0] as i16 as u16;
        let z = player_pos[2] as i16 as u16;
        if let Some(enemy) = self.entities.get_mut(slot) {
            enemy.unk_c6 = x;
            enemy.unk_c8 = z;
        }
        let player = &mut self.entities[0];
        player.unk_c6 = x;
        player.unk_c8 = z;
        player.is_being_attacked = 1;
        player.animation_id = 5;
        player.animation_frame_id = 7;
        player.action_behavior = 0;
        player.action_state = 0;
    }

    /// The shared cross-enemy special-weapon check the adder and several
    /// other monsters call from their damaged/flinch sub-states: when the
    /// player's equipped item id is in the high special range (`> 0x6E`) and
    /// the entity's current animation frame lands on a five-frame boundary,
    /// the hit latch clears so a heavy weapon cancels the reaction.
    pub fn zombie_check_special_weapon(&mut self, slot: usize) {
        if self.equipped.is_some_and(|item| item > 0x6E)
            && self.entities[slot].animation_frame_id.is_multiple_of(5)
        {
            self.entities[slot].hit_state = 0;
        }
    }

    /// Snapshot the outgoing room's live enemies into the 16-slot TTL table
    /// and tear the live list down, exactly like the original's door-load
    /// pass. `destination_changed` is the original's room-change gate: the
    /// slots only age when the destination room differs from the source.
    pub fn snapshot_enemies(&mut self, destination_changed: bool) {
        if destination_changed {
            for slot in &mut self.saved_enemies {
                if slot.valid != 0 {
                    slot.valid -= 1;
                }
            }
        }
        let count = usize::from(self.enemy_count).min(ENEMY_LIST_LEN);
        for index in 0..count {
            let live = self.entities[1 + index];
            let (status, store) = if live.health < 0 {
                (0, live.death_event_id == 0xFF)
            } else {
                (live.status_flags & 0x0F, true)
            };
            // A force-spawned record (variant bit 7) is never snapshotted.
            if !store || live.variant & 0x80 != 0 {
                continue;
            }
            let Some(free) = self.saved_enemies.iter_mut().find(|slot| slot.valid == 0) else {
                break;
            };
            free.status_flags = status;
            free.behavior_flags = live.behavior_flags;
            free.room = self.id.room;
            free.enemy_type = live.variant;
            free.state = live.state();
            free.pos = [live.pos[0] as i16, live.pos[1] as i16, live.pos[2] as i16];
            free.angle = live.angle;
            free.valid = SAVED_ENEMY_TTL;
        }
    }

    /// Clear every enemy slot's status and the live count, the original's
    /// post-snapshot teardown.
    pub fn clear_room_entities(&mut self) {
        self.enemy_count = 0;
        for entity in self.entities.iter_mut().skip(1) {
            entity.status_flags = 0;
        }
    }

    /// The original's saved-state restore: find the first occupied snapshot
    /// for the current room whose type byte matches, copy it into the entity
    /// at `slot`, consume the slot and return `true`. The Y position is only
    /// restored when the saved behaviour byte has any of `0x70` set.
    pub(crate) fn restore_saved_enemy(&mut self, slot: usize, enemy_type: u8) -> bool {
        let Some(index) = self.saved_enemies.iter().position(|saved| {
            saved.valid != 0 && saved.room == self.id.room && saved.enemy_type == enemy_type
        }) else {
            return false;
        };
        let saved = self.saved_enemies[index];
        let entity = &mut self.entities[slot];
        entity.status_flags = saved.status_flags;
        entity.behavior_flags = saved.behavior_flags;
        entity.pos[0] = i32::from(saved.pos[0]);
        if saved.behavior_flags & 0x70 != 0 {
            entity.pos[1] = i32::from(saved.pos[1]);
        }
        entity.pos[2] = i32::from(saved.pos[2]);
        entity.angle = saved.angle;
        self.saved_enemies[index].valid = 0;
        true
    }
}

// The built-in fallback tables carry the documented values so a pack without
// `data/combat.bin` behaves like the shipped data.
pub const DEFAULT_RANGES: [[u32; 10]; 2] = [
    [400, 1200, 2600, 800, 800, 400, 900, 900, 900, 1100],
    [500, 1300, 2600, 800, 800, 400, 900, 900, 900, 1100],
];

pub const DEFAULT_FIRST_RUN: [[i16; 7]; 200] = [
    [100, -1800, 0, 8, 0, 0, 1],
    [100, -2620, 0, 9, 0, 1, 1],
    [100, -2500, 0, 53, 4, 0, 2],
    [100, -2620, 0, 50, 4, 0, 2],
    [100, -2620, 0, 130, 4, 0, 2],
    [150, -1500, 0, 20, 14, 6, 1],
    [150, -1620, 0, 201, 0, 0, 2],
    [150, -1520, 0, 95, 9, 0, 1],
    [150, -1500, 0, 95, 14, 6, 1],
    [150, -1620, 0, 900, 0, 6, 2],
    [100, -1800, 0, 8, 0, 0, 1],
    [100, -2620, 0, 9, 0, 1, 1],
    [100, -2500, 0, 53, 4, 0, 2],
    [100, -2620, 0, 50, 4, 0, 2],
    [100, -2620, 0, 130, 4, 0, 2],
    [150, -1500, 0, 20, 14, 6, 1],
    [150, -1620, 0, 201, 0, 0, 2],
    [150, -1520, 0, 95, 9, 0, 1],
    [150, -1500, 0, 95, 14, 6, 1],
    [150, -1620, 0, 900, 0, 6, 2],
    [100, -1200, 0, 30, 0, 0, 1],
    [100, -1200, 0, 20, 0, 1, 1],
    [100, -1200, 0, 40, 4, 0, 2],
    [100, -1200, 0, 60, 4, 0, 2],
    [100, -1200, 0, 130, 4, 0, 2],
    [100, 0, 0, 20, 14, 6, 1],
    [100, -100, 0, 200, 0, 0, 2],
    [100, 0, 0, 100, 9, 0, 1],
    [100, 0, 0, 100, 14, 6, 1],
    [100, -100, 0, 900, 0, 6, 2],
    [0, -1200, 0, 10, 0, 8, 1],
    [0, -1220, 0, 20, 0, 9, 1],
    [0, -1100, 0, 40, 0, 8, 2],
    [0, -1220, 0, 40, 0, 8, 2],
    [0, -1220, 0, 130, 0, 8, 2],
    [0, -1100, 0, 20, 14, 7, 1],
    [0, -1120, 0, 100, 0, 0, 2],
    [0, -1120, 0, 100, 9, 0, 1],
    [0, -1100, 0, 200, 14, 7, 1],
    [0, -1120, 0, 900, 0, 7, 2],
    [0, -900, 0, 10, 0, 8, 1],
    [0, -920, 0, 14, 0, 9, 1],
    [0, -800, 0, 40, 0, 8, 2],
    [0, -920, 0, 50, 0, 8, 2],
    [0, -920, 0, 70, 0, 8, 2],
    [0, -800, 0, 20, 14, 7, 1],
    [0, -820, 0, 60, 0, 0, 2],
    [0, -820, 0, 60, 9, 0, 1],
    [0, -800, 0, 205, 14, 7, 1],
    [0, -820, 0, 900, 0, 7, 2],
    [0, 0, 0, 50, 0, 0, 1],
    [0, 0, 0, 26, 0, 0, 1],
    [0, 0, 0, 50, 0, 0, 2],
    [0, 0, 0, 50, 0, 0, 2],
    [0, 0, 0, 130, 0, 0, 2],
    [0, 0, 0, 20, 14, 5, 1],
    [0, 0, 0, 200, 0, 0, 2],
    [0, 0, 0, 60, 9, 0, 1],
    [0, 0, 0, 60, 14, 5, 1],
    [0, 0, 0, 900, 0, 5, 2],
    [0, -1500, 0, 16, 9, 6, 1],
    [0, -1500, 0, 14, 9, 6, 1],
    [0, -1500, 0, 32, 9, 6, 2],
    [0, -1500, 0, 40, 9, 6, 2],
    [0, -1500, 0, 130, 9, 6, 2],
    [150, -1500, 0, 20, 14, 6, 1],
    [150, -1500, 0, 100, 0, 0, 2],
    [150, -1500, 0, 200, 9, 0, 1],
    [150, -1500, 0, 100, 14, 6, 1],
    [150, -1500, 0, 900, 0, 6, 2],
    [0, 0, 0, 20, 0, 16, 1],
    [0, 0, 0, 30, 0, 17, 1],
    [0, 0, 0, 60, 0, 16, 2],
    [0, 0, 0, 70, 0, 16, 2],
    [0, 0, 0, 130, 0, 16, 2],
    [0, 0, 0, 20, 14, 4, 1],
    [0, 0, 0, 200, 0, 0, 2],
    [0, 0, 0, 80, 9, 0, 1],
    [0, 0, 0, 80, 14, 4, 1],
    [0, 0, 0, 900, 0, 4, 2],
    [0, 0, 0, 15, 1, 0, 1],
    [0, 1000, 0, 15, 0, 0, 1],
    [0, 1500, 0, 20, 0, 0, 2],
    [0, 1000, 0, 38, 0, 0, 2],
    [0, 1000, 0, 74, 0, 0, 2],
    [0, 1500, 0, 20, 14, 6, 1],
    [0, 1500, 0, 50, 2, 0, 2],
    [0, 1500, 0, 40, 9, 0, 1],
    [0, 1500, 0, 150, 14, 6, 1],
    [0, 1500, 0, 900, 2, 6, 2],
    [0, 0, 0, 17, 0, 0, 1],
    [0, 0, 0, 20, 1, 0, 1],
    [0, 0, 0, 30, 1, 0, 2],
    [0, 0, 0, 40, 1, 0, 2],
    [0, 0, 0, 130, 1, 0, 2],
    [150, -1500, 0, 20, 14, 6, 1],
    [150, -1500, 0, 200, 0, 0, 2],
    [150, -1500, 0, 60, 9, 0, 1],
    [150, -1500, 0, 60, 14, 6, 1],
    [150, -1500, 0, 900, 0, 6, 2],
    [0, 0, 0, 20, 0, 0, 1],
    [0, 0, 0, 20, 0, 0, 1],
    [0, 0, 0, 40, 0, 0, 2],
    [0, 0, 0, 50, 0, 0, 2],
    [0, 0, 0, 130, 0, 0, 2],
    [0, 0, 0, 30, 14, 3, 1],
    [0, 0, 0, 200, 0, 0, 2],
    [0, 0, 0, 60, 9, 0, 1],
    [0, 0, 0, 60, 14, 3, 1],
    [0, 0, 0, 900, 0, 3, 2],
    [0, 0, 0, 0, 1, 0, 1],
    [0, 0, 0, 0, 1, 0, 1],
    [0, 0, 0, 0, 1, 0, 1],
    [0, 0, 0, 0, 1, 0, 1],
    [0, 0, 0, 0, 1, 0, 1],
    [0, -1500, 0, 0, 14, 7, 1],
    [0, -1500, 0, 0, 0, 0, 2],
    [0, -1500, 0, 0, 9, 0, 1],
    [0, -1500, 0, 0, 14, 7, 1],
    [0, -1500, 0, 0, 0, 7, 2],
    [0, 0, 0, 10, 0, 0, 1],
    [0, 0, 0, 20, 1, 0, 1],
    [0, 0, 0, 30, 1, 0, 2],
    [0, 0, 0, 50, 1, 0, 2],
    [0, 0, 0, 80, 1, 0, 2],
    [150, -2000, 0, 20, 2, 6, 1],
    [150, -2000, 0, 100, 2, 0, 2],
    [150, -2000, 0, 100, 2, 0, 1],
    [150, -2000, 0, 100, 2, 6, 1],
    [150, -2000, 0, 900, 2, 6, 2],
    [0, 0, 0, 20, 0, 8, 1],
    [0, 0, 0, 30, 1, 0, 1],
    [0, 0, 0, 40, 1, 0, 2],
    [0, 0, 0, 40, 1, 0, 2],
    [0, 0, 0, 80, 1, 0, 2],
    [0, 0, 0, 20, 2, 7, 1],
    [0, 0, 0, 80, 2, 0, 2],
    [0, 0, 0, 130, 2, 0, 1],
    [0, 0, 0, 50, 2, 7, 1],
    [0, 0, 0, 900, 2, 7, 2],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 0, 1, 0, 0],
    [0, 0, 0, 10, 0, 0, 1],
    [0, 0, 0, 20, 1, 0, 1],
    [0, 0, 0, 30, 1, 0, 2],
    [0, 0, 0, 50, 1, 0, 2],
    [0, 0, 0, 80, 1, 0, 2],
    [150, -2000, 0, 20, 2, 6, 1],
    [150, -2000, 0, 100, 2, 0, 2],
    [150, -2000, 0, 100, 2, 0, 1],
    [150, -2000, 0, 100, 2, 6, 1],
    [150, -2000, 0, 900, 2, 6, 2],
    [100, -1800, 0, 8, 0, 0, 1],
    [100, -2620, 0, 9, 0, 1, 1],
    [100, -1500, 0, 53, 4, 0, 2],
    [100, -2620, 0, 50, 4, 0, 2],
    [100, -2620, 0, 130, 4, 0, 2],
    [150, -2500, 0, 20, 14, 6, 1],
    [150, -1620, 0, 201, 0, 0, 2],
    [150, -1520, 0, 95, 9, 0, 1],
    [150, -1500, 0, 95, 14, 6, 1],
    [150, -1620, 0, 900, 0, 6, 2],
    [0, 0, 0, 20, 0, 8, 1],
    [0, 0, 0, 30, 1, 0, 1],
    [0, 0, 0, 40, 1, 0, 2],
    [0, 0, 0, 40, 1, 0, 2],
    [0, 0, 0, 80, 1, 0, 2],
    [0, 0, 0, 20, 2, 7, 1],
    [0, 0, 0, 80, 2, 0, 2],
    [0, 0, 0, 130, 2, 0, 1],
    [0, 0, 0, 50, 2, 7, 1],
    [0, 0, 0, 900, 2, 7, 2],
    [0, 0, 0, 10, 1, 0, 1],
    [0, 0, 0, 0, 1, 0, 1],
    [0, 0, 0, 0, 1, 0, 2],
    [0, 0, 0, 0, 1, 0, 2],
    [0, 0, 0, 0, 1, 0, 2],
    [0, 0, 0, 2, 2, 7, 1],
    [0, 0, 0, 30, 2, 0, 2],
    [0, 0, 0, 30, 2, 0, 1],
    [0, 0, 0, 30, 2, 7, 1],
    [0, 0, 0, 900, 2, 7, 2],
];

pub const DEFAULT_SECOND_RUN: [[i16; 2]; 200] = [
    [8, 1],
    [9, 1],
    [20, 2],
    [50, 2],
    [60, 2],
    [20, 1],
    [201, 2],
    [70, 1],
    [70, 1],
    [900, 2],
    [8, 1],
    [9, 1],
    [20, 2],
    [50, 2],
    [60, 2],
    [20, 1],
    [201, 2],
    [70, 1],
    [70, 1],
    [900, 2],
    [30, 1],
    [20, 1],
    [35, 2],
    [60, 2],
    [60, 2],
    [20, 1],
    [200, 2],
    [90, 1],
    [90, 1],
    [900, 2],
    [10, 1],
    [15, 1],
    [24, 2],
    [30, 2],
    [40, 2],
    [20, 1],
    [100, 2],
    [100, 1],
    [200, 1],
    [900, 2],
    [10, 1],
    [12, 1],
    [20, 2],
    [50, 2],
    [70, 2],
    [20, 1],
    [50, 2],
    [50, 1],
    [103, 1],
    [900, 2],
    [50, 1],
    [26, 1],
    [50, 2],
    [50, 2],
    [50, 2],
    [20, 1],
    [200, 2],
    [60, 1],
    [60, 1],
    [900, 2],
    [16, 1],
    [14, 1],
    [25, 2],
    [40, 2],
    [80, 2],
    [20, 1],
    [50, 2],
    [80, 1],
    [50, 1],
    [900, 2],
    [20, 1],
    [30, 1],
    [60, 2],
    [70, 2],
    [70, 2],
    [20, 1],
    [200, 2],
    [80, 1],
    [80, 1],
    [900, 2],
    [15, 1],
    [10, 1],
    [20, 2],
    [38, 2],
    [20, 2],
    [20, 1],
    [40, 2],
    [40, 1],
    [150, 1],
    [900, 2],
    [10, 1],
    [12, 1],
    [20, 2],
    [40, 2],
    [41, 2],
    [20, 1],
    [60, 2],
    [60, 1],
    [100, 1],
    [900, 2],
    [20, 1],
    [20, 1],
    [40, 2],
    [50, 2],
    [50, 2],
    [30, 1],
    [200, 2],
    [60, 1],
    [60, 1],
    [900, 2],
    [0, 1],
    [0, 1],
    [0, 1],
    [0, 1],
    [0, 1],
    [0, 1],
    [0, 2],
    [0, 1],
    [0, 1],
    [0, 2],
    [10, 1],
    [15, 1],
    [20, 2],
    [50, 2],
    [40, 2],
    [20, 1],
    [35, 2],
    [35, 1],
    [35, 1],
    [900, 2],
    [15, 1],
    [18, 1],
    [5, 2],
    [40, 2],
    [60, 2],
    [20, 1],
    [60, 2],
    [120, 1],
    [60, 1],
    [900, 2],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [0, 0],
    [10, 1],
    [15, 1],
    [20, 2],
    [50, 2],
    [40, 2],
    [20, 1],
    [35, 2],
    [35, 1],
    [35, 1],
    [900, 2],
    [8, 1],
    [9, 1],
    [20, 2],
    [50, 2],
    [60, 2],
    [20, 1],
    [201, 2],
    [70, 1],
    [70, 1],
    [900, 2],
    [15, 1],
    [18, 1],
    [15, 2],
    [40, 2],
    [60, 2],
    [20, 1],
    [60, 2],
    [120, 1],
    [60, 1],
    [900, 2],
    [6, 1],
    [0, 1],
    [0, 2],
    [0, 2],
    [0, 2],
    [2, 1],
    [10, 2],
    [20, 1],
    [30, 1],
    [900, 2],
];

pub const DEFAULT_HIT_JOINTS: [[u8; 6]; 20] = [
    [2, 3, 4, 5, 7, 8],
    [3, 4, 5, 6, 7, 8],
    [2, 3, 4, 5, 6, 8],
    [0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0],
    [1, 2, 4, 5, 8, 9],
    [4, 5, 6, 7, 8, 9],
    [0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0],
    [8, 4, 5, 6, 9, 10],
    [0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0],
    [2, 4, 5, 6, 7, 8],
    [0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0],
];

pub const DEFAULT_REACTIONS: [HitReaction; 20] = [
    HitReaction::Zombie,
    HitReaction::Zombie,
    HitReaction::Zombie,
    HitReaction::Basic,
    HitReaction::Basic,
    HitReaction::Basic,
    HitReaction::Head,
    HitReaction::Basic,
    HitReaction::Blood,
    HitReaction::None,
    HitReaction::Basic,
    HitReaction::None,
    HitReaction::None,
    HitReaction::None,
    HitReaction::None,
    HitReaction::None,
    HitReaction::None,
    HitReaction::Zombie,
    HitReaction::None,
    HitReaction::None,
];

pub const DEFAULT_PLAYER_SCA: [[i16; 8]; 3] = [
    [-32768, 0, -1530, 0, 1530, 422, 0, 0],
    [-32768, 0, 400, 0, 0, 0, 0, 0],
    [-32768, 0, -1350, 0, 1350, 372, 0, 0],
];

pub const DEFAULT_ZOMBIE_SCA: [[i16; 8]; 2] = [
    [-32767, 0, -1530, 0, 1530, 422, 0, 0],
    [-32767, 0, -1530, 0, 1530, 322, 0, 0],
];

pub const DEFAULT_ZOMBIE_HEALTH: [u8; 16] = [
    59, 59, 79, 59, 59, 39, 59, 79, 79, 99, 59, 79, 59, 59, 79, 59,
];
pub const DEFAULT_ZOMBIE_ANIM: [u8; 16] = [0, 0, 9, 9, 0, 12, 9, 29, 0, 0, 9, 0, 0, 0, 0, 0];
pub const DEFAULT_ZOMBIE_STAGGER: [u8; 32] = [
    4, 3, 5, 3, 4, 4, 3, 4, 3, 5, 4, 4, 5, 3, 4, 5, 4, 3, 3, 4, 4, 3, 4, 4, 5, 3, 5, 3, 4, 3, 3, 4,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::ENTITY_STATUS_ACTIVE;

    fn armed_game() -> GameState {
        let mut game = GameState::default();
        // Neutral aim so the candidate filter's `0xE0` mask matches the
        // enemy's aligned bit.
        game.player_flags = 0x40;
        game
    }

    fn place_enemy(game: &mut GameState, slot: usize, id: u8, pos: [i32; 3]) {
        let entity = &mut game.entities[slot];
        entity.id = id;
        entity.pos = pos;
        entity.set_active(true);
        entity.status_flags |= 0x40;
        entity.health = 55;
        game.enemy_count = game.enemy_count.max(slot as u8);
    }

    #[test]
    fn defaults_round_trip_through_the_packed_layout() {
        let tables = CombatTables::default();
        let bytes = tables.encode();
        assert_eq!(bytes.len(), COMBAT_BYTES);
        let parsed = CombatTables::parse(&bytes).unwrap();
        for (index, (a, b)) in tables.first_run.iter().zip(&parsed.first_run).enumerate() {
            assert_eq!(a, b, "first_run {index}");
        }
        for (index, (a, b)) in tables.second_run.iter().zip(&parsed.second_run).enumerate() {
            assert_eq!(a, b, "second_run {index}");
        }
        for (index, (a, b)) in tables
            .model_entries
            .iter()
            .zip(&parsed.model_entries)
            .enumerate()
        {
            assert_eq!(a, b, "model {index}");
        }
        assert_eq!(parsed, tables);
    }

    #[test]
    fn parse_rejects_truncated_and_mismatched_blobs() {
        let bytes = CombatTables::default().encode();
        assert!(CombatTables::parse(&bytes[..bytes.len() - 1]).is_err());
        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert!(CombatTables::parse(&bad_magic).is_err());
        let mut bad_version = bytes.clone();
        bad_version[5] = 9;
        assert!(CombatTables::parse(&bad_version).is_err());
    }

    #[test]
    fn defaults_index_ranges_records_and_models() {
        let tables = CombatTables::default();
        assert_eq!(tables.weapon_range(0, 0), Some(400));
        assert_eq!(tables.weapon_range(0, 1), Some(500));
        assert_eq!(tables.weapon_range(9, 1), Some(1100));
        assert_eq!(tables.weapon_range(10, 0), None);

        let record = tables.hit_record(0, 0x13).unwrap();
        assert_eq!(record.damage, 10, "the web knife record");
        assert_eq!(record.hit, 1);
        assert_eq!(tables.hit_record_second(0, 0x13).unwrap().damage, 6);
        assert_eq!(tables.hit_record(0, 0x14), None);

        assert_eq!(
            tables.model_entry(0x00, 0),
            Some("enemy/em1000.emd"),
            "zombie model"
        );
        assert_eq!(tables.model_entry(0x13, 0), Some("enemy/em1013.emd"));
        assert_eq!(tables.model_entry(0x00, 1), Some("enemy/em1100.emd"));
        assert_eq!(tables.model_entry(0x20, 1), Some("enemy/em1020.emd"));
        assert_eq!(tables.model_entry(0x30, 0), None);

        assert_eq!(tables.reaction(0x13), HitReaction::None);
        assert_eq!(tables.reaction(0x07), HitReaction::Basic);
        assert_eq!(tables.hit_joints(0x00), [2, 3, 4, 5, 7, 8]);
    }

    #[test]
    fn default_invariants_are_non_degenerate() {
        let tables = CombatTables::default();
        assert!(tables.weapon_ranges.iter().flatten().all(|&r| r > 0));
        assert!(tables.first_run.iter().any(|record| record.damage > 0));
        assert!(tables.second_run.iter().any(|record| record.damage > 0));
        assert!(
            tables
                .model_entries
                .iter()
                .all(|entry| entry[0] != 0 && entry.contains(&0))
        );
        assert!(tables.type_health.iter().all(|&health| health > 0));
        assert!(tables.type_anim.iter().all(|&anim| anim < 32));
        assert!(
            tables
                .type_stagger
                .iter()
                .all(|&value| (3..=5).contains(&value))
        );
    }

    #[test]
    fn weapon_class_and_post_hit_slots() {
        assert_eq!(weapon_class(0), None);
        assert_eq!(weapon_class(1), Some(WeaponClass::Knife));
        assert_eq!(weapon_class(4), Some(WeaponClass::Gun));
        assert_eq!(weapon_class(6), Some(WeaponClass::Projectile));
        assert_eq!(weapon_class(11), None);
        assert_eq!(post_hit_class(0), Some(PostHit::Knife));
        assert_eq!(post_hit_class(1), Some(PostHit::Reaction));
        assert_eq!(post_hit_class(3), Some(PostHit::Shotgun));
        assert_eq!(post_hit_class(5), Some(PostHit::Blood));
        assert_eq!(post_hit_class(8), Some(PostHit::Blood));
        assert_eq!(post_hit_class(9), Some(PostHit::Blood3));
        assert_eq!(post_hit_class(10), None);
    }

    #[test]
    fn knife_hit_damages_and_sets_the_damaged_state() {
        let room = RoomState::default();
        let mut game = armed_game();
        place_enemy(&mut game, 1, 0x13, [200, 0, 0]);
        assert_eq!(game.apply_weapon_damage(&room, 1), 1);
        let web = &game.entities[1];
        assert_eq!(web.health, 55 - 10);
        assert_eq!(web.state(), 2, "surviving hit enters state 2");
        assert_eq!(web.ignore(), 0);
        assert_eq!(web.action_behavior, 0);
        assert_eq!(web.action_state, 0);
        assert_eq!(
            web.hit_state,
            0x08 | 1 | 2,
            "knife id shifted plus the record hit plus the neutral-aim step"
        );
        assert!(!game.flags[usize::from(BANK_ENEMIES)].bit(0));
    }

    #[test]
    fn a_killing_blow_enters_state_three_and_raises_the_death_flag() {
        let room = RoomState::default();
        let mut game = armed_game();
        place_enemy(&mut game, 1, 0x13, [200, 0, 0]);
        game.entities[1].death_event_id = 7;
        game.entities[1].health = 3;
        assert_eq!(game.apply_weapon_damage(&room, 1), 1);
        assert_eq!(game.entities[1].state(), 3);
        assert!(game.flags[usize::from(BANK_ENEMIES)].bit(7));
    }

    #[test]
    fn the_hit_state_gate_rejects_a_second_shot_until_it_is_cleared() {
        let room = RoomState::default();
        let mut game = armed_game();
        place_enemy(&mut game, 1, 0x13, [200, 0, 0]);
        assert_eq!(game.apply_weapon_damage(&room, 1), 1);
        assert_eq!(game.apply_weapon_damage(&room, 1), 0, "hit_state gate");
        game.entities[1].hit_state = 0;
        assert_eq!(game.apply_weapon_damage(&room, 1), 1);
    }

    #[test]
    fn the_aim_mask_gate_and_npc_id_path() {
        let room = RoomState::default();
        let mut game = armed_game();
        place_enemy(&mut game, 1, 0x13, [200, 0, 0]);
        game.player_flags = 0;
        assert_eq!(game.apply_weapon_damage(&room, 1), 0, "no aim bits");
        game.player_flags = 0x40;
        game.entities[1].id = 0x20;
        assert_eq!(game.apply_weapon_damage(&room, 1), 0x20, "NPC id path");
        assert_eq!(game.entities[1].health, 55, "NPCs take no damage");
    }

    #[test]
    fn second_playthrough_reads_the_second_table() {
        let room = RoomState::default();
        let mut game = armed_game();
        place_enemy(&mut game, 1, 0x13, [200, 0, 0]);
        game.apply_flag(BANK_SCENARIO, SCENARIO_FLAG_SECOND_PLAYTHROUGH, 0);
        assert_eq!(game.apply_weapon_damage(&room, 1), 1);
        assert_eq!(game.entities[1].health, 55 - 6, "knife second-run damage");
    }

    #[test]
    fn closest_candidate_wins_over_a_farther_in_range_one() {
        let room = RoomState::default();
        let mut game = armed_game();
        place_enemy(&mut game, 1, 0x13, [500, 0, 0]);
        place_enemy(&mut game, 2, 0x13, [200, 0, 0]);
        assert_eq!(game.apply_weapon_damage(&room, 1), 1);
        assert_eq!(game.entities[2].health, 45, "the closer web was hit");
        assert_eq!(game.entities[1].health, 55, "the farther web was not");
    }

    #[test]
    fn gun_cone_hits_the_target_ahead_and_misses_the_one_behind() {
        let room = RoomState::default();
        let mut game = armed_game();
        game.entities[0].angle = 0;
        place_enemy(&mut game, 1, 0x00, [800, 0, 0]);
        assert_eq!(game.apply_weapon_damage(&room, 2), 1, "handgun in front");
        game.entities[1].hit_state = 0;
        game.entities[1].health = 55;
        game.entities[1].pos = [-800, 0, 0];
        assert_eq!(game.apply_weapon_damage(&room, 2), 0, "behind the cone");
    }

    #[test]
    fn line_of_sight_blocks_a_gun_shot_through_a_wall() {
        use crate::state::CollisionRect;

        let room = RoomState::default();
        let mut game = armed_game();
        game.entities[0].angle = 0;
        place_enemy(&mut game, 1, 0x00, [800, 0, 0]);
        assert_eq!(game.apply_weapon_damage(&room, 2), 1, "clear line of sight");

        // One fully-blocking boundary record sits on the shot's path.
        let mut blocked_room = RoomState::default();
        blocked_room.collision.quadrants[0].push(CollisionRect {
            x_max: 792,
            z_max: 90,
            x_min: 396,
            z_min: 0,
            kind: 1,
            flags: 0x300,
        });
        game.entities[1].hit_state = 0;
        game.entities[1].health = 55;
        assert_eq!(
            game.apply_weapon_damage(&blocked_room, 2),
            0,
            "the wall blocks the handgun"
        );
        // The projectile class skips the line-of-sight rule (weapon slot >= 5).
        assert_eq!(game.apply_weapon_damage(&blocked_room, 10), 1);
    }

    #[test]
    fn projectile_uses_the_parked_origin() {
        let room = RoomState::default();
        let mut game = armed_game();
        place_enemy(&mut game, 1, 0x0A, [2000, 0, 0]);
        assert_eq!(game.apply_weapon_damage(&room, 10), 0, "outside 1100+200");
        assert_eq!(
            game.apply_weapon_damage_with(&room, 10, [2000, 0, 0], 0x40),
            1,
            "the parked projectile origin reaches"
        );
    }

    #[test]
    fn snapshot_round_trips_through_a_cleared_room() {
        let mut game = GameState::default();
        game.id = crate::state::RoomId {
            stage: 1,
            room: 0x30,
            player_flag: 0,
        };
        game.enemy_count = 1;
        let entity = &mut game.entities[1];
        entity.id = 0x13;
        entity.set_active(true);
        entity.status_flags = 0x41;
        entity.behavior_flags = 3;
        entity.pos = [100, 0, -200];
        entity.angle = 0x400;
        entity.variant = 0x03;
        entity.health = 40;
        game.snapshot_enemies(true);
        game.clear_room_entities();
        assert_eq!(game.enemy_count, 0);
        assert!(!game.entities[1].active());
        let saved = game.saved_enemies[0];
        assert!(saved.occupied());
        assert_eq!(saved.enemy_type, 0x03);
        assert_eq!(saved.pos, [100, 0, -200]);

        // The destination spawn without force restores the snapshot.
        game.restore_saved_enemy(1, 0x03);
        assert_eq!(game.entities[1].status_flags, 0x01);
        assert_eq!(game.entities[1].pos, [100, 0, -200]);
        assert_eq!(game.entities[1].angle, 0x400);
        assert!(!game.saved_enemies[0].occupied(), "the slot is consumed");
    }

    #[test]
    fn a_dead_enemy_with_a_death_event_is_not_snapshotted() {
        let mut game = GameState::default();
        game.enemy_count = 1;
        let entity = &mut game.entities[1];
        entity.set_active(true);
        entity.health = -1;
        entity.death_event_id = 4;
        entity.variant = 1;
        game.snapshot_enemies(true);
        assert!(!game.saved_enemies[0].occupied());
    }

    #[test]
    fn force_spawned_enemies_are_not_snapshotted() {
        let mut game = GameState::default();
        game.enemy_count = 1;
        let entity = &mut game.entities[1];
        entity.set_active(true);
        entity.health = 10;
        entity.variant = 0x81;
        game.snapshot_enemies(true);
        assert!(!game.saved_enemies[0].occupied());
    }

    #[test]
    fn player_hurt_applies_the_clamp_and_lethal_rules() {
        let mut game = GameState::default();
        game.max_health = 140;
        game.entities[0].health = 5;
        assert_eq!(game.apply_player_hurt(6, 0), -1);
        game.entities[0].health = 5;
        assert_eq!(
            game.apply_player_hurt(6, PLAYER_HURT_CLAMP_TO_ONE),
            1,
            "clamped to one"
        );
        game.entities[0].health = 5;
        assert_eq!(
            game.apply_player_hurt(15, PLAYER_HURT_CLAMP_TO_ONE | PLAYER_HURT_LETHAL),
            -1,
            "the lethal flagged blow resolves to the scripted death"
        );
    }

    #[test]
    fn poison_sets_the_status_bit_and_the_timer() {
        let mut game = GameState::default();
        game.poison_player();
        assert_eq!(game.health_status & 0x02, 0x02);
        assert_eq!(game.poison_timer, POISON_TIMER);
    }

    #[test]
    fn special_weapon_check_clears_the_hit_latch_on_five_frame_boundaries() {
        let mut game = GameState::default();
        game.entities[1].hit_state = 0x20;
        game.entities[1].animation_frame_id = 5;

        // An ordinary weapon id leaves the latch alone.
        game.equipped = Some(0x0A);
        game.zombie_check_special_weapon(1);
        assert_eq!(game.entities[1].hit_state, 0x20);

        // A high special id clears it on a five-frame boundary...
        game.equipped = Some(0x6F);
        game.zombie_check_special_weapon(1);
        assert_eq!(game.entities[1].hit_state, 0);

        // ...but not on the frames between.
        game.entities[1].hit_state = 0x20;
        game.entities[1].animation_frame_id = 4;
        game.zombie_check_special_weapon(1);
        assert_eq!(game.entities[1].hit_state, 0x20);
    }

    #[test]
    fn set_state_keeps_the_ignore_byte_but_the_pipeline_zeroes_it() {
        let mut entity = Entity {
            status_flags: ENTITY_STATUS_ACTIVE,
            ..Entity::default()
        };
        entity.set_state(4);
        entity.set_ignore(9);
        entity.set_state(2);
        assert_eq!(entity.ignore(), 9);
        entity.set_ignore(0);
        assert_eq!(entity.ignore(), 0);
    }
}
