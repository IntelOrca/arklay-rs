//! Room billboard effects: the 64-slot pool, the per-room sprite tables and
//! the packed texture pages.
//!
//! Slice 1 of M10 implements the pool and the six SCD effect opcodes.
//! [`pool::create`] resolves an effect type through the room's declared sprite
//! table first and the global weapon-FX metadata (`core00`) second, allocates
//! one slot per animation frame of the selected depth row (in reverse frame
//! order, exactly like the original's build loop) and copies each frame's first
//! 24-byte behaviour block into its slot.
//!
//! Slice 2 implements the asset pipeline: [`room::RoomEffects`] parses the
//! RDT header pointer slots 13/14/15 (the sprite index table, the backward
//! sprite-info offsets and the embedded 4bpp TIMs) and [`room::WeaponEffects`]
//! parses `data/core00.esp`/`core00.etm`; [`pages`] reproduces the original's
//! two-pass texture-page packing and carries the executable's static blend,
//! tint, camera-light and room→sheet tables.

#[cfg(test)]
pub(crate) mod fixtures;
pub mod pages;
pub mod pool;
pub mod room;

pub use pool::{
    Attach, EFFECT_BLOCK_LEN, EFFECT_POOL_SIZE, Effect, EffectBlock, EffectPool, create,
};
pub use room::{EffectSprite, RoomEffects, SpriteAnimation, SpriteInfo, WeaponEffects};
