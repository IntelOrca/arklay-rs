//! The 64-slot effect pool and the spawn entry point.
//!
//! Every field mirrors the original's 0x84-byte effect record; the raw sprite
//! pointers (clut/vram/uv/animation) are replaced with a sprite type key plus
//! cursors into the parsed [`crate::effects::room::EffectSprite`] tables, so a
//! slot stays valid across a room change without holding packed pointers.

use crate::effects::room::RoomEffects;
use crate::game::GameState;

/// Number of effect slots in the pool.
pub const EFFECT_POOL_SIZE: usize = 64;

/// Number of entries in the effect behaviour dispatch table.
pub const EFFECT_BEHAVIOR_COUNT: usize = 64;

/// Size in bytes of one animation behaviour block.
pub const EFFECT_BLOCK_LEN: usize = 24;

/// One 24-byte animation behaviour block copied from a sprite's animation data.
///
/// The block doubles as the slot's animation header and phase timer. Bytes 0-3
/// are the two behaviour ids, the copied billboard type and the light factor;
/// bytes 4-7 are the four phase parameters (`anim_header[0..4]`); bytes 8-13
/// are the three per-frame velocity deltas; bytes 14-15 are the header flag
/// word; bytes 16-21 the three position accumulators and bytes 22-23 the yaw
/// added to the spawn yaw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EffectBlock(pub [u8; EFFECT_BLOCK_LEN]);

impl EffectBlock {
    /// The behaviour dispatched first each tick (`anim_id`).
    pub fn anim_id(&self) -> u8 {
        self.0[0]
    }

    /// The second behaviour dispatched each tick (`update_id`).
    pub fn update_id(&self) -> u8 {
        self.0[1]
    }

    /// The copied `initial_type` header byte.
    pub fn initial_type(&self) -> u8 {
        self.0[2]
    }

    /// The copied spawn light factor at block offset 3.
    pub fn light(&self) -> u8 {
        self.0[3]
    }

    /// The three signed per-frame velocity deltas at block offsets 8/10/12.
    pub fn velocity(&self) -> [i16; 3] {
        [
            read_i16(&self.0, 8),
            read_i16(&self.0, 10),
            read_i16(&self.0, 12),
        ]
    }

    /// The header flag word at block offset 14.
    pub fn flags(&self) -> u16 {
        u16::from_le_bytes([self.0[14], self.0[15]])
    }

    /// Overwrite the header flag word at block offset 14.
    pub fn set_flags(&mut self, flags: u16) {
        self.0[14..16].copy_from_slice(&flags.to_le_bytes());
    }

    /// The three signed position accumulators at block offsets 16/18/20.
    pub fn accumulators(&self) -> [i16; 3] {
        [
            read_i16(&self.0, 16),
            read_i16(&self.0, 18),
            read_i16(&self.0, 20),
        ]
    }

    /// The signed yaw added to the spawn yaw when the slot is created.
    pub fn yaw(&self) -> i16 {
        read_i16(&self.0, 22)
    }
}

/// The transform an effect is attached to, resolved from the spawn's parent
/// selector (the original's `spriteInfo` matrix pointer).
///
/// `parent` 0 is the identity transform, 1 the player, `2..=0x7F` entity slot
/// `parent - 1` in this port's entity array (the original's enemy index
/// `parent - 2`) and `>= 0x80` an object model `parent & 0x7F`.
///
/// # Documented deviation
///
/// Object models do not exist this milestone, so [`Attach::Omodel`] resolves
/// to the identity transform. The object class can fill the seam later without
/// touching the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Attach {
    /// Identity transform (parent 0).
    #[default]
    Identity,
    /// The player's transform (parent 1).
    Player,
    /// A scripted entity slot (parent `2..=0x7F`, stored as `parent - 1`).
    Entity(u8),
    /// An object model (parent `>= 0x80`, id `parent & 0x7F`; identity).
    Omodel(u8),
}

impl Attach {
    /// Resolve a `parent` operand into the attach selector.
    pub fn from_parent(parent: u8) -> Self {
        match parent {
            0 => Self::Identity,
            1 => Self::Player,
            2..=0x7F => Self::Entity(parent - 1),
            _ => Self::Omodel(parent & 0x7F),
        }
    }
}

/// One effect slot, mirroring the original's 0x84-byte record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Effect {
    /// 0x00 Behaviour id; `0` marks the slot free.
    pub anim_id: u8,
    /// 0x01 Second behaviour id.
    pub update_id: u8,
    /// 0x02 Billboard type; `1` while the effect is live.
    pub active: u8,
    /// 0x03 Light factor: the spawn argument when non-zero, otherwise the
    /// animation block's authored factor.
    pub light_factor: u8,
    /// 0x04 Animation header: bytes 4..22 of the copied 24-byte block, i.e.
    /// the original's `effect+0x04` header window.
    pub header: [u8; 18],
    /// 0x16 Yaw, the block yaw plus the spawn yaw.
    pub yaw: i16,
    /// 0x18/0x1A/0x1C Rotation speeds.
    pub rot_speed: [i16; 3],
    /// 0x1E Frame delay counter.
    pub frame_delay: u8,
    /// 0x1F Current frame's UV-record index.
    pub frame_index: u8,
    /// 0x20 World position (integrated by the per-tick update).
    pub pos: [i16; 3],
    /// 0x26 Spawn effect type (the sprite index).
    pub effect_type: u8,
    /// 0x27 Depth group: low three bits select the animation row, the rest the
    /// tint level.
    pub depth_group: u8,
    /// 0x28 Local spawn offset (the truncated spawn position).
    pub local_offset: [i16; 3],
    /// 0x2E Projected depth scaled by four.
    pub depth_scaled: i16,
    /// 0x30 3x3 4.12 rotation matrix.
    pub transform: [i16; 9],
    /// 0x42 Alignment pad.
    pub pad_42: i16,
    /// 0x44 Sprite world offsets.
    pub sprite_offset: [i32; 3],
    /// 0x50 Projected depth from the last update.
    pub proj_depth: i32,
    /// Projected screen position from the last update (the original's packed
    /// `local_1c` pair).
    pub screen: [i16; 2],
    /// Current UV record: `u`, `v`, `pivot_x`, `pivot_y`.
    pub uv: [u8; 4],
    /// Current frame entry's billboard texel size (`width`, `height`).
    pub size: [u8; 2],
    /// 0x54 Full-precision spawn position.
    pub spawn_pos: [i32; 3],
    /// 0x60 Fourth spawn-position word.
    pub spawn_pos_w: i32,
    /// Resolved sprite type (port replacement for the raw sprite pointer).
    pub sprite: Option<u8>,
    /// Resolved attach transform (port replacement for the matrix pointer).
    pub attach: Attach,
    /// Current sprite frame-table entry (port replacement for the
    /// `vramInfoBackup` pointer).
    pub frame_entry: u8,
    /// Animation row selected by `depth_group & 7`.
    pub anim_depth: u8,
    /// Current animation frame within the row.
    pub anim_frame: u8,
    /// Current 24-byte block within the frame.
    pub anim_block: u8,
}

impl Effect {
    /// The header flag word at effect+0x0E.
    pub fn flags(&self) -> u16 {
        u16::from_le_bytes([self.header[10], self.header[11]])
    }

    /// Overwrite the header flag word at effect+0x0E.
    pub fn set_flags(&mut self, flags: u16) {
        self.header[10..12].copy_from_slice(&flags.to_le_bytes());
    }
}

/// The 64 effect slots and the free-slot counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectPool {
    slots: [Effect; EFFECT_POOL_SIZE],
    free: u8,
}

impl Default for EffectPool {
    /// A pool with every slot free.
    fn default() -> Self {
        Self {
            slots: [Effect::default(); EFFECT_POOL_SIZE],
            free: EFFECT_POOL_SIZE as u8,
        }
    }
}

impl EffectPool {
    /// A pool with every slot free.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of slots the counter considers free.
    pub fn free_slots(&self) -> u8 {
        self.free
    }

    /// One slot by index.
    pub fn slot(&self, index: usize) -> Option<&Effect> {
        self.slots.get(index)
    }

    /// One slot by index, mutably.
    pub fn slot_mut(&mut self, index: usize) -> Option<&mut Effect> {
        self.slots.get_mut(index)
    }

    /// Every live slot (`anim_id != 0`) with its index, in pool order.
    pub fn active(&self) -> impl Iterator<Item = (usize, &Effect)> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, effect)| effect.anim_id != 0)
    }

    /// Number of live slots.
    pub fn active_count(&self) -> usize {
        self.active().count()
    }

    /// Claim the highest free slot, searching 63 down to 0 like the original.
    ///
    /// Returns `None` when no slot is free (the original's plain spawn
    /// failure), leaving the pool untouched.
    pub fn claim(&mut self) -> Option<usize> {
        if self.free == 0 {
            return None;
        }
        for index in (0..EFFECT_POOL_SIZE).rev() {
            if self.slots[index].anim_id == 0 {
                self.free -= 1;
                self.slots[index] = Effect::default();
                return Some(index);
            }
        }
        None
    }

    /// Return one slot to the pool. A slot that is already free is ignored, so
    /// the counter can never drift past the pool size.
    pub fn release(&mut self, index: usize) {
        if let Some(effect) = self.slots.get_mut(index)
            && (effect.anim_id != 0 || effect.update_id != 0)
        {
            effect.anim_id = 0;
            effect.update_id = 0;
            self.free = self.free.saturating_add(1).min(EFFECT_POOL_SIZE as u8);
        }
    }

    /// Free every slot (`effect_clear` 0x48).
    pub fn clear(&mut self) {
        self.slots = [Effect::default(); EFFECT_POOL_SIZE];
        self.free = EFFECT_POOL_SIZE as u8;
    }

    /// Free every live slot whose type and attach match (`effect_kill_a`
    /// 0x3E). Returns how many slots were freed.
    pub fn kill_matching_attach(&mut self, effect_type: u8, attach: Attach) -> usize {
        let mut killed = 0;
        for index in (0..EFFECT_POOL_SIZE).rev() {
            let effect = &self.slots[index];
            if (effect.anim_id != 0 || effect.update_id != 0)
                && effect.effect_type == effect_type
                && effect.attach == attach
            {
                self.release(index);
                killed += 1;
            }
        }
        killed
    }

    /// Free every live slot whose type and depth group match (`effect_kill_b`
    /// 0x42). Returns how many slots were freed.
    pub fn kill_matching_depth(&mut self, effect_type: u8, depth_group: u8) -> usize {
        let mut killed = 0;
        for index in (0..EFFECT_POOL_SIZE).rev() {
            let effect = &self.slots[index];
            if (effect.anim_id != 0 || effect.update_id != 0)
                && effect.effect_type == effect_type
                && effect.depth_group == depth_group
            {
                self.release(index);
                killed += 1;
            }
        }
        killed
    }

    /// OR/AND-NOT/XOR `mask` into the header flag word of every live slot
    /// (`mass_mask` 0x4E). Modes other than 0/1/2 are inert.
    pub fn modify_flags(&mut self, mode: u8, mask: u16) {
        for effect in &mut self.slots {
            if effect.anim_id == 0 && effect.update_id == 0 {
                continue;
            }
            let flags = effect.flags();
            let updated = match mode {
                0 => flags | mask,
                1 => flags & !mask,
                2 => flags ^ mask,
                _ => continue,
            };
            effect.set_flags(updated);
        }
    }
}

/// Spawn an effect of `effect_type` into the pool.
///
/// The type is resolved through the room's declared sprite table first, then
/// the global weapon-FX metadata. An unknown type is skipped, logged once per
/// room, and leaves the pool untouched (the original dereferences a null
/// sprite pointer here; this port hardens that with the skip).
///
/// `depth & 7` selects the animation row and one slot is allocated per frame
/// of that row, from the last frame down to the first, each initialised from
/// the frame's first 24-byte behaviour block. `parent` selects the attach
/// transform ([`Attach::from_parent`]); `yaw` is added to each block's yaw and
/// `light` overwrites the block's own light factor only when it is non-zero,
/// exactly like the original (a script spawn passes zero and keeps the
/// animation data's authored brightness).
///
/// Returns the slot allocated for the row's first (current) frame, or `None`
/// when the sprite, the row or the pool was empty.
#[allow(clippy::too_many_arguments)] // the spawn operand signature is fixed
pub fn create(
    game: &mut GameState,
    room: &RoomEffects,
    effect_type: u8,
    depth: u8,
    parent: u8,
    pos: [i32; 3],
    yaw: i16,
    light: u8,
) -> Option<u8> {
    create_attached(
        game,
        room,
        effect_type,
        depth,
        Attach::from_parent(parent),
        pos,
        yaw,
        light,
    )
}

/// Spawn an effect attached to an already-resolved transform.
///
/// This is the behaviour-spawn entry point: the original passes the parent
/// slot's sprite matrix (`spriteInfo`) to the spawn helper, not the script
/// operand, so a child inherits its parent's attach target.
#[allow(clippy::too_many_arguments)] // the spawn operand signature is fixed
pub fn create_attached(
    game: &mut GameState,
    room: &RoomEffects,
    effect_type: u8,
    depth: u8,
    attach: Attach,
    pos: [i32; 3],
    yaw: i16,
    light: u8,
) -> Option<u8> {
    let GameState {
        effects,
        weapon_effects,
        effect_missing_logged,
        ..
    } = game;

    let sprite = room
        .sprite(effect_type)
        .or_else(|| weapon_effects.sprite(effect_type));
    let Some(sprite) = sprite else {
        if effect_missing_logged.insert(effect_type) {
            eprintln!(
                "warning: effect sprite {effect_type} is not declared by this room; effect skipped"
            );
        }
        return None;
    };

    let depth_group = depth;
    let depth = usize::from(depth & 7);
    let frames = sprite.anim.depth(depth)?;
    if frames.is_empty() {
        return None;
    }

    let frame0 = sprite.info.frames.first().copied().unwrap_or_default();
    let uv = sprite
        .info
        .uvs
        .get(usize::from(frame0.uv_index))
        .copied()
        .unwrap_or_default();
    let mut current = None;
    for (index, frame) in frames.iter().enumerate().rev() {
        let Some(block) = frame.blocks.first() else {
            continue;
        };
        let Some(slot) = effects.claim() else {
            break;
        };
        let Some(effect) = effects.slot_mut(slot) else {
            break;
        };
        effect.anim_id = block.anim_id();
        effect.update_id = block.update_id();
        effect.active = 1;
        effect.light_factor = if light != 0 { light } else { block.light() };
        effect.header.copy_from_slice(&block.0[4..22]);
        effect.yaw = block.yaw().wrapping_add(yaw);
        effect.rot_speed = [0, 0, 0];
        effect.frame_delay = frame0.delay;
        effect.frame_index = frame0.uv_index;
        effect.effect_type = effect_type;
        effect.depth_group = depth_group;
        effect.local_offset = [pos[0] as i16, pos[1] as i16, pos[2] as i16];
        effect.spawn_pos = pos;
        effect.sprite = Some(effect_type);
        effect.attach = attach;
        effect.frame_entry = 0;
        effect.anim_depth = depth as u8;
        effect.anim_frame = index as u8;
        effect.anim_block = 0;
        effect.uv = [uv.u, uv.v, uv.pivot_x, uv.pivot_y];
        effect.size = [frame0.width, frame0.height];
        effect.screen = [0, 0];
        current = Some(slot as u8);
    }
    current
}

fn read_i16(bytes: &[u8; EFFECT_BLOCK_LEN], offset: usize) -> i16 {
    i16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::fixtures::{block, sprite};
    use crate::effects::room::RoomEffects;
    use crate::state::RoomId;

    fn game_with_weapon(index: u8, rows: [Vec<Vec<[u8; 24]>>; 8]) -> GameState {
        let mut game = GameState::new(RoomId::parse("1000").unwrap(), &Default::default());
        game.weapon_effects.sprites.push(sprite(index, rows));
        game
    }

    fn assert_slot_pool(game: &GameState) {
        let active = game.effects.active_count() as u8;
        assert_eq!(game.effects.free_slots(), EFFECT_POOL_SIZE as u8 - active);
    }

    #[test]
    fn seats_search_from_slot_63_down() {
        let mut game = GameState::default();
        let room = RoomEffects::default();
        game.weapon_effects.sprites.push(sprite(
            9,
            std::array::from_fn(|_| vec![vec![block(2, 0, 0)]]),
        ));
        let slot = create(&mut game, &room, 9, 0, 0, [1, 2, 3], 0, 0).unwrap();
        assert_eq!(slot, 63);
        let slot = create(&mut game, &room, 9, 0, 0, [1, 2, 3], 0, 0).unwrap();
        assert_eq!(slot, 62);
        assert_slot_pool(&game);
    }

    #[test]
    fn multi_frame_row_allocates_every_frame_in_reverse() {
        let mut game = GameState::default();
        let room = RoomEffects::default();
        let frames = vec![
            vec![block(7, 0, 0)],
            vec![block(8, 0, 0)],
            vec![block(9, 0, 0)],
        ];
        game.weapon_effects.sprites.push(sprite(
            9,
            [
                vec![],
                vec![],
                frames,
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
            ],
        ));
        let current = create(&mut game, &room, 9, 2, 0, [0, 0, 0], 0, 0).unwrap();
        assert_eq!(current, 61);
        assert_eq!(game.effects.slot(63).unwrap().anim_id, 9);
        assert_eq!(game.effects.slot(63).unwrap().anim_frame, 2);
        assert_eq!(game.effects.slot(62).unwrap().anim_id, 8);
        assert_eq!(game.effects.slot(62).unwrap().anim_frame, 1);
        assert_eq!(game.effects.slot(61).unwrap().anim_id, 7);
        assert_eq!(game.effects.slot(61).unwrap().anim_frame, 0);
        assert_slot_pool(&game);
    }

    #[test]
    fn room_declaration_overrides_the_weapon_table() {
        let mut game = GameState::default();
        let mut room = RoomEffects::default();
        room.sprites.push(sprite(
            9,
            std::array::from_fn(|_| vec![vec![block(30, 0, 0)]]),
        ));
        game.weapon_effects.sprites.push(sprite(
            9,
            std::array::from_fn(|_| vec![vec![block(1, 0, 0)]]),
        ));
        create(&mut game, &room, 9, 0, 0, [0, 0, 0], 0, 0).unwrap();
        assert_eq!(game.effects.slot(63).unwrap().anim_id, 30);
    }

    #[test]
    fn missing_type_skips_without_leaking_a_slot() {
        let mut game = GameState::default();
        let room = RoomEffects::default();
        assert_eq!(create(&mut game, &room, 9, 0, 0, [0, 0, 0], 0, 0), None);
        assert_eq!(game.effects.free_slots(), EFFECT_POOL_SIZE as u8);
        assert_eq!(game.effects.active_count(), 0);
        assert!(game.effect_missing_logged.contains(&9));
    }

    #[test]
    fn empty_pool_rejects_further_spawns() {
        let mut game = game_with_weapon(9, std::array::from_fn(|_| vec![vec![block(2, 0, 0)]]));
        let room = RoomEffects::default();
        for _ in 0..EFFECT_POOL_SIZE {
            assert!(create(&mut game, &room, 9, 0, 0, [0, 0, 0], 0, 0).is_some());
        }
        assert_eq!(game.effects.free_slots(), 0);
        assert_eq!(create(&mut game, &room, 9, 0, 0, [0, 0, 0], 0, 0), None);
        assert_eq!(game.effects.active_count(), EFFECT_POOL_SIZE);
        assert!(game.effects.slot(0).unwrap().anim_id != 0);
    }

    #[test]
    fn create_copies_the_header_and_spawn_fields() {
        let mut game = GameState::default();
        let room = RoomEffects::default();
        let mut raw = block(2, 5, 44);
        raw[4] = 25;
        raw[8..10].copy_from_slice(&7i16.to_le_bytes());
        raw[14..16].copy_from_slice(&0x0002u16.to_le_bytes());
        raw[16..18].copy_from_slice(&11i16.to_le_bytes());
        game.weapon_effects
            .sprites
            .push(sprite(9, std::array::from_fn(|_| vec![vec![raw]])));
        create(&mut game, &room, 9, 6, 0, [4420, -2500, 3800], 1536, 77).unwrap();
        let effect = game.effects.slot(63).unwrap();
        assert_eq!(effect.anim_id, 2);
        assert_eq!(effect.update_id, 5);
        assert_eq!(effect.active, 1);
        assert_eq!(effect.light_factor, 77);
        assert_eq!(effect.header[..4], [25, 0, 0, 0]);
        assert_eq!(effect.header[4..6], 7i16.to_le_bytes());
        assert_eq!(effect.header[12..14], 11i16.to_le_bytes());
        assert_eq!(effect.yaw, 1580);
        assert_eq!(effect.rot_speed, [0, 0, 0]);
        assert_eq!(effect.frame_delay, 4);
        assert_eq!(effect.frame_index, 0);
        assert_eq!(effect.uv, [16, 32, 64, 64]);
        assert_eq!(effect.size, [16, 16]);
        assert_eq!(effect.effect_type, 9);
        assert_eq!(effect.depth_group, 6);
        assert_eq!(effect.local_offset, [4420, -2500, 3800]);
        assert_eq!(effect.spawn_pos, [4420, -2500, 3800]);
        assert_eq!(effect.sprite, Some(9));
        assert_eq!(effect.attach, Attach::Identity);
        assert_eq!(effect.flags(), 0x0002);
        assert_slot_pool(&game);
    }

    #[test]
    fn create_resolves_the_first_frame_entry() {
        use crate::effects::room::FrameEntry;
        let mut game = game_with_weapon(9, std::array::from_fn(|_| vec![vec![block(2, 0, 0)]]));
        game.weapon_effects.sprites[0].info.frames = vec![
            FrameEntry {
                uv_index: 3,
                delay: 5,
                width: 24,
                height: 32,
            },
            FrameEntry {
                uv_index: 9,
                delay: 0xFF,
                width: 8,
                height: 8,
            },
        ];
        let room = RoomEffects::default();
        create(&mut game, &room, 9, 0, 0, [0, 0, 0], 0, 0).unwrap();
        let effect = game.effects.slot(63).unwrap();
        assert_eq!(effect.frame_entry, 0);
        assert_eq!(effect.frame_index, 3);
        assert_eq!(effect.frame_delay, 5);
        assert_eq!(effect.size, [24, 32]);
    }

    #[test]
    fn create_zero_light_keeps_the_block_light() {
        let mut raw = block(2, 0, 0);
        raw[3] = 25;
        let mut game = game_with_weapon(9, std::array::from_fn(|_| vec![vec![raw]]));
        let room = RoomEffects::default();
        create(&mut game, &room, 9, 0, 0, [0, 0, 0], 0, 0).unwrap();
        assert_eq!(game.effects.slot(63).unwrap().light_factor, 25);
    }

    #[test]
    fn parent_selectors_resolve_to_attach_targets() {
        assert_eq!(Attach::from_parent(0), Attach::Identity);
        assert_eq!(Attach::from_parent(1), Attach::Player);
        assert_eq!(Attach::from_parent(2), Attach::Entity(1));
        assert_eq!(Attach::from_parent(3), Attach::Entity(2));
        assert_eq!(Attach::from_parent(0x7F), Attach::Entity(126));
        assert_eq!(Attach::from_parent(0x80), Attach::Omodel(0));
        assert_eq!(Attach::from_parent(0x81), Attach::Omodel(1));
        assert_eq!(Attach::from_parent(0xFF), Attach::Omodel(0x7F));
    }

    #[test]
    fn create_stores_the_resolved_attach() {
        let mut game = game_with_weapon(9, std::array::from_fn(|_| vec![vec![block(2, 0, 0)]]));
        let room = RoomEffects::default();
        create(&mut game, &room, 9, 0, 0x81, [0, 0, 0], 0, 0).unwrap();
        assert_eq!(game.effects.slot(63).unwrap().attach, Attach::Omodel(1));
        create(&mut game, &room, 9, 0, 3, [0, 0, 0], 0, 0).unwrap();
        assert_eq!(game.effects.slot(62).unwrap().attach, Attach::Entity(2));
    }

    #[test]
    fn kill_matching_attach_frees_only_the_matching_slots() {
        let mut pool = EffectPool::new();
        for (parent, effect_type) in [(0u8, 9u8), (1, 9), (0, 8), (0, 9)] {
            let slot = pool.claim().unwrap();
            let effect = pool.slot_mut(slot).unwrap();
            effect.anim_id = 2;
            effect.effect_type = effect_type;
            effect.attach = Attach::from_parent(parent);
        }
        assert_eq!(pool.free_slots(), 60);
        assert_eq!(pool.kill_matching_attach(9, Attach::Identity), 2);
        assert_eq!(pool.free_slots(), 62);
        assert_eq!(pool.active_count(), 2);
        assert_eq!(pool.active().count(), 2);
    }

    #[test]
    fn kill_matching_depth_uses_the_low_depth_byte() {
        let mut pool = EffectPool::new();
        for (effect_type, depth) in [(9u8, 7u8), (9, 8), (9, 7), (8, 7)] {
            let slot = pool.claim().unwrap();
            let effect = pool.slot_mut(slot).unwrap();
            effect.anim_id = 2;
            effect.effect_type = effect_type;
            effect.depth_group = depth;
        }
        assert_eq!(pool.kill_matching_depth(9, 7), 2);
        assert_eq!(pool.free_slots(), 62);
        assert_eq!(pool.active_count(), 2);
    }

    #[test]
    fn clear_frees_every_slot() {
        let mut pool = EffectPool::new();
        for index in 0..EFFECT_POOL_SIZE {
            let slot = pool.claim().unwrap();
            assert_eq!(slot, EFFECT_POOL_SIZE - 1 - index);
            let effect = pool.slot_mut(slot).unwrap();
            effect.anim_id = 1;
            effect.update_id = 1;
        }
        assert_eq!(pool.free_slots(), 0);
        pool.clear();
        assert_eq!(pool.free_slots(), EFFECT_POOL_SIZE as u8);
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn mass_mask_applies_only_the_three_modes() {
        let mut pool = EffectPool::new();
        let slot = pool.claim().unwrap();
        let effect = pool.slot_mut(slot).unwrap();
        effect.anim_id = 2;
        effect.set_flags(0x0010);
        pool.modify_flags(0, 0x0004);
        assert_eq!(pool.slot(slot).unwrap().flags(), 0x0014);
        pool.modify_flags(1, 0x0004);
        assert_eq!(pool.slot(slot).unwrap().flags(), 0x0010);
        pool.modify_flags(2, 0x0001);
        assert_eq!(pool.slot(slot).unwrap().flags(), 0x0011);
        pool.modify_flags(3, 0xFFFF);
        pool.modify_flags(0xFF, 0xFFFF);
        assert_eq!(pool.slot(slot).unwrap().flags(), 0x0011);
    }

    #[test]
    fn mass_mask_skips_free_slots() {
        let mut pool = EffectPool::new();
        pool.modify_flags(0, 0xFFFF);
        assert_eq!(pool.slot(63).unwrap().flags(), 0);
    }
}
