//! The real-asset integration suite: every real-asset test module in one
//! binary, so the suite compiles and links once instead of once per file.
//! The performance budgets live in their own `tests/perf_real.rs` binary so
//! the suite's parallel tests cannot take the cores they measure.
//!
//! The real-asset tests here are `#[ignore]`d because they need the game
//! install and a converted pack; `-- --ignored` runs them:
//!
//! ```sh
//! TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/path/to/re1 \
//!   ARKLAY_RE1_PACK=/path/to/re1.akpak \
//!   cargo test --profile test-release --test real -- --ignored
//! ```
//!
//! `--profile test-release` is the release profile without thin LTO: the
//! tests run at release speed, but the link skips an optimisation pass only a
//! shipped binary needs. `scripts/check.sh --full` runs it as part of its
//! ignored pass.
//!
//! Target one module or one test with a name filter, the way
//! `cargo test --test <file>` used to before the files were merged:
//!
//! ```sh
//! cargo test --profile test-release --test real stairs_real:: -- --ignored
//! cargo test --profile test-release --test real \
//!   stairs_real::lab_ladder_zone_latches_flags_and_base -- --ignored
//! ```
//!
//! The full-game soak is the one real-asset test that stays in its own binary
//! (`tests/soak_real.rs`): its counting allocator measures process-wide live
//! bytes, so it must not run beside other tests. It is release-only and needs
//! `ARKLAY_SOAK=1`; see its header.

// `common` sits beside `tests/` because both this binary and the soak binary
// include it.
#[path = "../common/mod.rs"]
mod common;

mod actor_real;
mod combat_real;
mod death_real;
mod door_anim_real;
mod doors_real;
mod doors_scan;
mod effects_real;
mod enemy_census;
mod enemy_real;
mod entities_real;
mod file_real;
mod fire_real;
mod font_real;
mod hd_real;
mod inventory_real;
mod item_box_real;
mod item_view_real;
mod language_real;
mod m11_real;
mod m12_items_real;
mod m12_real;
mod m12_sparkles_real;
mod m13_real;
mod m14_real;
mod m15_real;
mod m16_game_real;
mod m16_ui_real;
mod m6_wiring_real;
mod mask_render_real;
mod npc_real;
mod objects_real;
mod save_real;
mod shadow_real;
mod stairs_real;
mod transition_real;
mod walk_real;
mod weapons_real;
