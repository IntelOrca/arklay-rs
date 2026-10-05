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
//!
//! Slice 3 adds [`behaviour::update`]: the per-tick walk over the 64 slots,
//! the tiered behaviour dispatch table, the velocity integration, the sprite
//! animation stepping and the projection the renderer consumes.
//!
//! Slice 4's presentation lives in [`crate::render`] (`EffectQuad`,
//! `rasterize_effect` and `draw_gameplay_scene_with_effects`) and in the
//! engine's `EffectPageCache`/quad builder.
//!
//! # Documented deviations (slice 6)
//!
//! - **Omodel attach.** Parent selectors `>= 0x80` (object models) resolve
//!   through [`pool::Attach::Omodel`] to the object's composed rotation and
//!   world position; a missing or inactive object keeps the identity
//!   fallback.
//! - **Inert combat behaviours.** Behaviour ids 10, 37, 43 and 45 stay counted
//!   placeholders by design (bullet/rocket/flame combat); the corpus audit
//!   proves no shipped script or sprite animation reaches them. Every other
//!   entry has an implementation or a shared helper.
//! - **Unreached special cases.** The static tables carry the original's
//!   stage-specific quirks (the stage-4 fixed-depth lab camera and the stage-5
//!   lesson-room V remap are noted at their render sites); no shipped room
//!   exercises them in the M10 acceptance captures. A full pool makes a spawn
//!   return `None` (the original's `0xFF` sentinel), and an undeclared type is
//!   skipped with a one-shot log instead of the original's NULL read.
//! - **Deterministic RNG.** Behaviour draws use [`crate::game::GameState::rand_seed`],
//!   a fixed-seed xorshift advanced once per gameplay frame, not the platform
//!   `rand()`; the effect trajectories are repeatable but need not match the
//!   original's per-frame values.

pub mod behaviour;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod pages;
pub mod pool;
pub mod room;

pub use pool::{
    Attach, EFFECT_BEHAVIOR_COUNT, EFFECT_BLOCK_LEN, EFFECT_POOL_SIZE, Effect, EffectBlock,
    EffectPool, create, create_attached,
};
pub use room::{EffectSprite, RoomEffects, SpriteAnimation, SpriteInfo, WeaponEffects};
