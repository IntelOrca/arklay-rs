//! Per-room sound name tables and footstep resolution.
//!
//! The shipped rooms name their entity and footstep sounds in a table shaped
//! as `stage * 29 + room` rows of 48 slots. [`room_sound`] resolves any named
//! slot: the footstep window (columns 36..=47), the enemy-AI columns the
//! entity sounds index and the item/UI columns the scripts address directly.
//!
//! A footstep zone (parsed from the RDT's `.flr` table) packs a surface type
//! in the high byte of its lookup result and a sound offset in the low byte.
//! The offset plus the entity sound type names the column inside the room row.

use crate::state::RoomState;

/// Rooms per stage in the room sound table.
const ROOMS_PER_STAGE: usize = 29;
/// One-shot entity sound types are 0, 1 and 2.
pub const MAX_ENTITY_SOUND_TYPE: u8 = 3;

/// Every named slot of the original per-room sound table.
///
/// The table is 203 rows of 48 slots (`row = stage_index * 29 + room`); the
/// shipped data names 1835 slots with 337 distinct sounds, covering the
/// footstep window (columns 36-47), the enemy-AI columns the entity sounds
/// index and the item/UI columns the scripts address directly. Entries are
/// `(row, column, name)`, sorted by `(row, column)` so [`room_sound`] can
/// binary search; slots the shipped data leaves empty are absent.
#[rustfmt::skip]
static ROOM_SOUNDS: &[(u16, u8, &str)] = &[
    (0, 23, "spray"),
    (0, 25, "R_chris"),
    (0, 29, "cancel"),
    (0, 30, "type01"),
    (0, 31, "type02"),
    (0, 32, "item02"),
    (0, 33, "item01"),
    (0, 45, "ft_wdA"),
    (0, 46, "ft_wdB"),
    (0, 47, "taore_wd"),
    (1, 0, "z_taore"),
    (1, 1, "z_ftL"),
    (1, 2, "z_ftR"),
    (1, 3, "z_kamu"),
    (1, 4, "z_k02"),
    (1, 5, "z_k01"),
    (1, 6, "z_head"),
    (1, 7, "z_haki"),
    (1, 8, "z_sanj"),
    (1, 9, "z_k03"),
    (1, 20, "D_gacha"),
    (1, 34, "key_door"),
    (1, 42, "ft_floA"),
    (1, 43, "ft_floB"),
    (1, 44, "taore_wd"),
    (1, 45, "ft_stwd"),
    (2, 36, "drw_opwd"),
    (2, 37, "drw_shwd"),
    (2, 38, "key_desk"),
    (2, 45, "ft_cpA"),
    (2, 46, "ft_cpB"),
    (2, 47, "taore_cp"),
    (3, 0, "z_taore"),
    (3, 1, "z_ftL"),
    (3, 2, "z_ftR"),
    (3, 3, "z_kamu"),
    (3, 4, "z_isi02"),
    (3, 5, "z_isi01"),
    (3, 6, "z_head"),
    (3, 7, "z_Hkick"),
    (3, 8, "z_Ugoron"),
    (3, 9, "z_isi03"),
    (3, 20, "D_gacha"),
    (3, 33, "key_indr"),
    (3, 34, "key_door"),
    (3, 45, "ft_wdA"),
    (3, 46, "ft_wdB"),
    (3, 47, "taore_wd"),
    (4, 0, "z_taore"),
    (4, 1, "z_ftL"),
    (4, 2, "z_ftR"),
    (4, 3, "z_kamu"),
    (4, 4, "z_mika02"),
    (4, 5, "z_mika01"),
    (4, 6, "z_head"),
    (4, 7, "z_Hkick"),
    (4, 8, "z_Ugoron"),
    (4, 9, "z_mika03"),
    (4, 20, "D_gacha"),
    (4, 34, "key_door"),
    (4, 42, "ft_wdA"),
    (4, 43, "ft_wdB"),
    (4, 44, "taore_wd"),
    (4, 45, "ft_cpA"),
    (4, 46, "ft_cpB"),
    (4, 47, "taore_cp"),
    (5, 0, "z_taore"),
    (5, 1, "z_ftL"),
    (5, 2, "z_ftR"),
    (5, 3, "z_kamu"),
    (5, 4, "z_mika02"),
    (5, 5, "z_mika01"),
    (5, 6, "z_head"),
    (5, 7, "z_Hkick"),
    (5, 8, "z_Ugoron"),
    (5, 9, "z_mika03"),
    (5, 23, "emblemA"),
    (5, 24, "magnum01"),
    (5, 26, "ClkDX4LR"),
    (5, 27, "mv_clk"),
    (5, 45, "ft_rmA"),
    (5, 46, "ft_rmB"),
    (5, 47, "taore_st"),
    (6, 20, "D_gacha"),
    (6, 23, "S&W+1"),
    (6, 25, "Dr_wd01"),
    (6, 26, "Dr_wd02"),
    (6, 29, "cancel"),
    (6, 30, "type01"),
    (6, 31, "type02"),
    (6, 32, "item02"),
    (6, 33, "item01"),
    (6, 34, "key_door"),
    (6, 39, "ft_haA"),
    (6, 40, "ft_haB"),
    (6, 41, "taore_st"),
    (6, 42, "ft_cpA"),
    (6, 43, "ft_cpB"),
    (6, 44, "taore_cp"),
    (6, 45, "ft_stwp"),
    (7, 0, "z_taore"),
    (7, 1, "z_ftL"),
    (7, 2, "z_ftR"),
    (7, 3, "z_kamu"),
    (7, 4, "z_aoya02"),
    (7, 5, "z_aoya01"),
    (7, 6, "z_head"),
    (7, 7, "z_Hkick"),
    (7, 8, "z_Ugoron"),
    (7, 9, "z_aoya03"),
    (7, 20, "D_gacha"),
    (7, 22, "mv_show"),
    (7, 23, "mv_step"),
    (7, 34, "key_door"),
    (7, 35, "ft_step"),
    (7, 42, "ft_linA"),
    (7, 43, "ft_linB"),
    (7, 44, "taore_st"),
    (7, 45, "ft_wdA"),
    (7, 46, "ft_wdB"),
    (7, 47, "taore_wd"),
    (8, 0, "cer_foot"),
    (8, 1, "cer_taoA"),
    (8, 2, "cer_unar"),
    (8, 3, "cer_bite"),
    (8, 4, "cer_cryA"),
    (8, 5, "cer_taoB"),
    (8, 6, "cer_jkMX"),
    (8, 7, "cer_kamu"),
    (8, 8, "cer_cryB"),
    (8, 9, "cer_runMX"),
    (8, 22, "mv_show"),
    (8, 23, "glass"),
    (8, 45, "ft_cpA"),
    (8, 46, "ft_cpB"),
    (8, 47, "taore_cp"),
    (9, 0, "z_taore"),
    (9, 1, "z_ftL"),
    (9, 2, "z_ftR"),
    (9, 3, "z_kamu"),
    (9, 4, "z_osou"),
    (9, 5, "z_unaruA"),
    (9, 6, "z_head"),
    (9, 7, "z_Hkick"),
    (9, 8, "z_Ugoron"),
    (9, 9, "z_unaruB"),
    (9, 20, "D_gacha"),
    (9, 25, "ceilpres"),
    (9, 34, "key_mtr"),
    (9, 45, "ft_wdA"),
    (9, 46, "ft_wdB"),
    (9, 47, "taore_wd"),
    (10, 0, "z_taore"),
    (10, 1, "z_ftL"),
    (10, 2, "z_ftR"),
    (10, 3, "z_kamu"),
    (10, 4, "z_isi202"),
    (10, 5, "z_isi201"),
    (10, 6, "z_head"),
    (10, 7, "z_Hkick"),
    (10, 8, "z_Ugoron"),
    (10, 9, "z_isi203"),
    (10, 20, "D_gacha"),
    (10, 33, "key_indr"),
    (10, 34, "key_door"),
    (10, 45, "ft_wdA"),
    (10, 46, "ft_wdB"),
    (10, 47, "taore_wd"),
    (11, 0, "z_taore"),
    (11, 1, "z_ftL"),
    (11, 2, "z_ftR"),
    (11, 3, "z_kamu"),
    (11, 4, "z_k02"),
    (11, 5, "z_k01"),
    (11, 6, "z_head"),
    (11, 7, "z_haki"),
    (11, 8, "z_sanj"),
    (11, 9, "z_k03"),
    (11, 20, "D_gacha"),
    (11, 42, "ft_wdA"),
    (11, 43, "ft_wdB"),
    (11, 44, "taore_wd"),
    (11, 45, "ft_stwd"),
    (12, 0, "VN_whip"),
    (12, 1, "VN_hitA"),
    (12, 2, "VN_hitB"),
    (12, 3, "VN_sime"),
    (12, 4, "VN_OUT"),
    (12, 23, "ch_sime"),
    (12, 24, "ji_sime"),
    (12, 45, "ft_linA"),
    (12, 46, "ft_linB"),
    (12, 47, "taore_st"),
    (13, 23, "TIGEREYE"),
    (13, 45, "ft_linA"),
    (13, 46, "ft_linB"),
    (13, 47, "taore_st"),
    (14, 0, "z_taore"),
    (14, 1, "z_ftL"),
    (14, 2, "z_ftR"),
    (14, 3, "z_kamu"),
    (14, 4, "z_osou"),
    (14, 5, "z_unaruA"),
    (14, 6, "z_head"),
    (14, 7, "z_Hkick"),
    (14, 8, "z_Ugoron"),
    (14, 9, "z_unaruB"),
    (14, 23, "clst_op"),
    (14, 24, "clst_hit"),
    (14, 25, "clst_bad"),
    (14, 42, "ft_wdA"),
    (14, 43, "ft_wdB"),
    (14, 44, "taore_wd"),
    (14, 45, "ft_cpA"),
    (14, 46, "ft_cpB"),
    (14, 47, "taore_cp"),
    (15, 0, "z_taore"),
    (15, 1, "z_ftL"),
    (15, 2, "z_ftR"),
    (15, 3, "z_kamu"),
    (15, 4, "z_osou"),
    (15, 5, "z_unaruA"),
    (15, 6, "z_head"),
    (15, 7, "z_Hkick"),
    (15, 8, "z_Ugoron"),
    (15, 9, "z_unaruB"),
    (15, 22, "mv_show"),
    (15, 23, "mv_wall"),
    (15, 25, "walldown"),
    (15, 26, "emblemA"),
    (15, 27, "emblemB"),
    (15, 42, "ft_linA"),
    (15, 43, "ft_linB"),
    (15, 44, "taore_st"),
    (15, 45, "ft_wdA"),
    (15, 46, "ft_wdB"),
    (15, 47, "taore_wd"),
    (17, 0, "z_taore"),
    (17, 1, "z_ftL"),
    (17, 2, "z_ftR"),
    (17, 3, "z_kamu"),
    (17, 4, "z_simo02"),
    (17, 5, "z_simo01"),
    (17, 6, "z_head"),
    (17, 7, "z_Hkick"),
    (17, 8, "z_Ugoron"),
    (17, 9, "z_simo03"),
    (17, 36, "drw_opwd"),
    (17, 37, "drw_shwd"),
    (17, 38, "key_desk"),
    (17, 45, "ft_linA"),
    (17, 46, "ft_linB"),
    (17, 47, "taore_st"),
    (18, 0, "z_taore"),
    (18, 1, "z_ftL"),
    (18, 2, "z_ftR"),
    (18, 3, "z_kamu"),
    (18, 4, "z_aoya02"),
    (18, 5, "z_aoya01"),
    (18, 6, "z_head"),
    (18, 7, "z_Hkick"),
    (18, 8, "z_Ugoron"),
    (18, 9, "z_aoya03"),
    (18, 20, "D_gacha"),
    (18, 34, "key_door"),
    (18, 45, "ft_linA"),
    (18, 46, "ft_linB"),
    (18, 47, "taore_st"),
    (19, 23, "bathMIX"),
    (19, 42, "ft_wdA"),
    (19, 43, "ft_wdB"),
    (19, 44, "taore_wd"),
    (19, 45, "ft_cpA"),
    (19, 46, "ft_cpB"),
    (19, 47, "taore_cp"),
    (20, 0, "cer_foot"),
    (20, 1, "cer_taoA"),
    (20, 2, "cer_unar"),
    (20, 3, "cer_bite"),
    (20, 4, "cer_cryA"),
    (20, 5, "cer_taoB"),
    (20, 6, "cer_jkMX"),
    (20, 7, "cer_kamu"),
    (20, 8, "cer_cryB"),
    (20, 9, "cer_runMX"),
    (20, 45, "ft_rmA"),
    (20, 46, "ft_rmB"),
    (20, 47, "taore_st"),
    (21, 20, "D_gacha"),
    (21, 23, "kickdoor"),
    (21, 24, "lock_mix"),
    (21, 25, "ceilpres"),
    (21, 29, "tao_wall"),
    (21, 45, "ft_linA"),
    (21, 46, "ft_linB"),
    (21, 47, "taore_st"),
    (22, 23, "gatan"),
    (22, 24, "sw_trap"),
    (22, 25, "ceilpres"),
    (22, 42, "ft_cpA"),
    (22, 43, "ft_cpB"),
    (22, 44, "taore_cp"),
    (22, 45, "ft_wdA"),
    (22, 46, "ft_wdB"),
    (22, 47, "taore_wd"),
    (23, 0, "RVcar1"),
    (23, 1, "RVpat"),
    (23, 2, "RVcar2"),
    (23, 3, "RVwing1"),
    (23, 4, "RVwing2"),
    (23, 5, "RVfryed"),
    (23, 23, "sw_btn"),
    (23, 24, "frame_ga"),
    (23, 25, "frame_fa"),
    (23, 45, "ft_wdA"),
    (23, 46, "ft_wdB"),
    (23, 47, "taore_wd"),
    (24, 29, "cancel"),
    (24, 30, "type01"),
    (24, 31, "type02"),
    (24, 32, "item02"),
    (24, 33, "item01"),
    (24, 45, "ft_wdA"),
    (24, 46, "ft_wdB"),
    (24, 47, "taore_wd"),
    (26, 0, "cer_foot"),
    (26, 1, "cer_taoA"),
    (26, 2, "cer_unar"),
    (26, 3, "cer_bite"),
    (26, 4, "cer_cryA"),
    (26, 5, "cer_taoB"),
    (26, 6, "cer_jkMX"),
    (26, 7, "cer_kamu"),
    (26, 8, "cer_cryB"),
    (26, 9, "cer_runMX"),
    (26, 20, "DM_gacha"),
    (26, 23, "sw_medal"),
    (26, 24, "lockout"),
    (26, 45, "ft_rmA"),
    (26, 46, "ft_rmB"),
    (26, 47, "taore_st"),
    (27, 22, "mv_step"),
    (27, 35, "ft_step"),
    (27, 45, "ft_rmA"),
    (27, 46, "ft_rmB"),
    (27, 47, "taore_st"),
    (28, 23, "kigae1"),
    (28, 24, "kigae2"),
    (28, 25, "Zipper3"),
    (28, 26, "Zipper1"),
    (28, 27, "Zipper2"),
    (28, 35, "ft_step"),
    (28, 45, "ft_linA"),
    (28, 46, "ft_linB"),
    (28, 47, "taore_st"),
    (30, 0, "z_taore"),
    (30, 1, "z_ftL"),
    (30, 2, "z_ftR"),
    (30, 3, "z_kamu"),
    (30, 4, "z_suzu02"),
    (30, 5, "z_suzu01"),
    (30, 6, "z_head"),
    (30, 7, "z_haki"),
    (30, 8, "z_sanj"),
    (30, 9, "z_suzu03"),
    (30, 20, "D_gacha"),
    (30, 34, "key_door"),
    (30, 42, "ft_floA"),
    (30, 43, "ft_floB"),
    (30, 44, "taore_wd"),
    (30, 45, "ft_stwd"),
    (31, 0, "z_taore"),
    (31, 1, "z_ftL"),
    (31, 2, "z_ftR"),
    (31, 3, "z_kamu"),
    (31, 4, "z_osou"),
    (31, 5, "z_unaruA"),
    (31, 6, "z_head"),
    (31, 7, "z_Hkick"),
    (31, 8, "z_Ugoron"),
    (31, 9, "z_unaruB"),
    (31, 22, "mv_stn"),
    (31, 23, "BRK_stn"),
    (31, 45, "ft_cpA"),
    (31, 46, "ft_cpB"),
    (31, 47, "taore_cp"),
    (32, 0, "z_taore"),
    (32, 1, "z_ftL"),
    (32, 2, "z_ftR"),
    (32, 3, "z_kamu"),
    (32, 4, "z_osou"),
    (32, 5, "z_unaruA"),
    (32, 6, "z_head"),
    (32, 7, "z_haki"),
    (32, 8, "z_sanj"),
    (32, 9, "z_unaruB"),
    (32, 42, "ft_cpA"),
    (32, 43, "ft_cpB"),
    (32, 44, "taore_cp"),
    (32, 45, "ft_stwp"),
    (33, 0, "z_taore"),
    (33, 1, "z_ftL"),
    (33, 2, "z_ftR"),
    (33, 3, "z_kamu"),
    (33, 4, "z_isi302"),
    (33, 5, "z_isi301"),
    (33, 6, "z_head"),
    (33, 7, "z_Hkick"),
    (33, 8, "z_Ugoron"),
    (33, 9, "z_isi303"),
    (33, 20, "D_gacha"),
    (33, 34, "key_door"),
    (33, 45, "ft_cpA"),
    (33, 46, "ft_cpB"),
    (33, 47, "taore_cp"),
    (34, 22, "mv_stn"),
    (34, 23, "macha"),
    (34, 24, "shuter"),
    (34, 45, "ft_linA"),
    (34, 46, "ft_linB"),
    (34, 47, "taore_st"),
    (35, 33, "key_indr"),
    (35, 45, "ft_cpA"),
    (35, 46, "ft_cpB"),
    (35, 47, "taore_cp"),
    (36, 0, "z_taore"),
    (36, 1, "z_ftL"),
    (36, 2, "z_ftR"),
    (36, 3, "z_kamu"),
    (36, 4, "z_suzu02"),
    (36, 5, "z_suzu01"),
    (36, 6, "z_head"),
    (36, 7, "z_haki"),
    (36, 8, "z_sanj"),
    (36, 9, "z_suzu03"),
    (36, 20, "D_gacha"),
    (36, 33, "key_indr"),
    (36, 34, "key_door"),
    (36, 39, "ft_cpA"),
    (36, 40, "ft_cpB"),
    (36, 41, "taore_cp"),
    (36, 42, "ft_wdA"),
    (36, 43, "ft_wdB"),
    (36, 44, "taore_wd"),
    (36, 45, "ft_stwd"),
    (37, 0, "z_taore"),
    (37, 1, "z_ftL"),
    (37, 2, "z_ftR"),
    (37, 3, "z_kamu"),
    (37, 4, "z_isi302"),
    (37, 5, "z_isi301"),
    (37, 6, "z_head"),
    (37, 7, "z_Hkick"),
    (37, 8, "z_Ugoron"),
    (37, 9, "z_isi303"),
    (37, 45, "ft_cpA"),
    (37, 46, "ft_cpB"),
    (37, 47, "taore_cp"),
    (38, 42, "ft_cpA"),
    (38, 43, "ft_cpB"),
    (38, 44, "taore_cp"),
    (38, 45, "ft_wdA"),
    (38, 46, "ft_wdB"),
    (38, 47, "taore_wd"),
    (39, 22, "mv_cp"),
    (39, 23, "mv_cp"),
    (39, 24, "sw_push"),
    (39, 25, "aquarium"),
    (39, 42, "ft_wdA"),
    (39, 43, "ft_wdB"),
    (39, 44, "taore_wd"),
    (39, 45, "ft_cpA"),
    (39, 46, "ft_cpB"),
    (39, 47, "taore_cp"),
    (40, 0, "z_taore"),
    (40, 1, "z_ftL"),
    (40, 2, "z_ftR"),
    (40, 3, "z_kamu"),
    (40, 4, "z_osou"),
    (40, 5, "z_unaruA"),
    (40, 6, "z_head"),
    (40, 7, "z_Hkick"),
    (40, 8, "z_Ugoron"),
    (40, 9, "z_unaruB"),
    (40, 20, "D_gacha"),
    (40, 34, "key_door"),
    (40, 42, "ft_cpA"),
    (40, 43, "ft_cpB"),
    (40, 44, "taore_cp"),
    (40, 45, "ft_wdA"),
    (40, 46, "ft_wdB"),
    (40, 47, "taore_wd"),
    (42, 45, "ft_linA"),
    (42, 46, "ft_linB"),
    (42, 47, "taore_st"),
    (43, 0, "z_taore"),
    (43, 1, "z_ftL"),
    (43, 2, "z_ftR"),
    (43, 3, "z_kamu"),
    (43, 4, "z_k02"),
    (43, 5, "z_k01"),
    (43, 6, "z_head"),
    (43, 7, "z_haki"),
    (43, 8, "z_sanj"),
    (43, 9, "z_k03"),
    (43, 20, "D_gacha"),
    (43, 23, "zuruzuru"),
    (43, 34, "key_door"),
    (43, 42, "ft_linA"),
    (43, 43, "ft_linB"),
    (43, 44, "taore_st"),
    (43, 45, "ft_stwd"),
    (44, 0, "z_taore"),
    (44, 1, "z_ftL"),
    (44, 2, "z_ftR"),
    (44, 3, "z_kamu"),
    (44, 4, "z_osou"),
    (44, 5, "z_unaruA"),
    (44, 6, "z_head"),
    (44, 7, "z_Hkick"),
    (44, 8, "z_Ugoron"),
    (44, 9, "z_unaruB"),
    (44, 22, "mv_cp"),
    (44, 42, "ft_wdA"),
    (44, 43, "ft_wdB"),
    (44, 44, "taore_wd"),
    (44, 45, "ft_cpA"),
    (44, 46, "ft_cpB"),
    (44, 47, "taore_cp"),
    (45, 0, "GV_move"),
    (45, 1, "GV_dino"),
    (45, 2, "GV_p_at"),
    (45, 3, "GV_swing"),
    (45, 4, "GV_bite"),
    (45, 5, "GV_blood"),
    (45, 6, "GV_gulpA"),
    (45, 7, "GV_gulpB"),
    (45, 23, "ch_nom"),
    (45, 24, "ji_nom"),
    (45, 45, "ft_wdA"),
    (45, 46, "ft_wdB"),
    (45, 47, "taore_wd"),
    (46, 45, "ft_cpA"),
    (46, 46, "ft_cpB"),
    (46, 47, "taore_cp"),
    (47, 0, "RVcar1"),
    (47, 1, "RVpat"),
    (47, 2, "RVcar2"),
    (47, 3, "RVwing1"),
    (47, 4, "RVwing2"),
    (47, 5, "RVfryed"),
    (47, 24, "RVpatA"),
    (47, 25, "RVpatB"),
    (47, 45, "ft_wdA"),
    (47, 46, "ft_wdB"),
    (47, 47, "taore_wd"),
    (50, 22, "mv_step"),
    (50, 23, "sw_btn"),
    (50, 24, "TIGEREYE"),
    (50, 35, "ft_step"),
    (50, 42, "ft_cpA"),
    (50, 43, "ft_cpB"),
    (50, 44, "taore_cp"),
    (50, 45, "ft_wdA"),
    (50, 46, "ft_wdB"),
    (50, 47, "taore_wd"),
    (58, 0, "cer_foot"),
    (58, 1, "cer_taoA"),
    (58, 2, "cer_unar"),
    (58, 3, "cer_bite"),
    (58, 4, "cer_cryA"),
    (58, 5, "cer_taoB"),
    (58, 6, "cer_jkMX"),
    (58, 7, "cer_kamu"),
    (58, 8, "cer_cryB"),
    (58, 9, "cer_runMX"),
    (58, 23, "call"),
    (58, 33, "Elv1"),
    (58, 34, "Elv2"),
    (58, 45, "ft_rmA"),
    (58, 46, "ft_rmB"),
    (58, 47, "taore_st"),
    (59, 0, "PY_mena"),
    (59, 1, "PY_hit2"),
    (59, 2, "PY_fall"),
    (59, 23, "chakuchi"),
    (59, 24, "crank"),
    (59, 25, "watergt"),
    (59, 33, "Elv1"),
    (59, 34, "Elv2"),
    (59, 35, "ft_lad"),
    (59, 42, "ft_concA"),
    (59, 43, "ft_concB"),
    (59, 44, "taore_st"),
    (59, 45, "ft_rmA"),
    (59, 46, "ft_rmB"),
    (59, 47, "taore_st"),
    (60, 0, "cer_foot"),
    (60, 1, "cer_taoA"),
    (60, 2, "cer_unar"),
    (60, 3, "cer_bite"),
    (60, 4, "cer_cryA"),
    (60, 5, "cer_taoB"),
    (60, 6, "cer_jkMX"),
    (60, 7, "cer_kamu"),
    (60, 8, "cer_cryB"),
    (60, 9, "cer_runMX"),
    (60, 23, "battery"),
    (60, 33, "ELVX1"),
    (60, 35, "ELVX2"),
    (60, 42, "ft_concA"),
    (60, 43, "ft_concB"),
    (60, 44, "taore_st"),
    (60, 45, "ft_rmA"),
    (60, 46, "ft_rmB"),
    (60, 47, "taore_st"),
    (61, 0, "TY_foot"),
    (61, 1, "TY_kaze"),
    (61, 2, "TY_slice"),
    (61, 3, "TY_HIT"),
    (61, 4, "TY_trust"),
    (61, 5, "TY_slef"),
    (61, 7, "TY_nage"),
    (61, 24, "Rancher"),
    (61, 27, "Ty_bomb"),
    (61, 28, "VB00_31a"),
    (61, 29, "VB00_31b"),
    (61, 30, "VB00_31c"),
    (61, 31, "TY_sube"),
    (61, 32, "TY_crash"),
    (61, 45, "ft_concA"),
    (61, 46, "ft_concB"),
    (61, 47, "taore_st"),
    (62, 0, "cer_foot"),
    (62, 1, "cer_taoA"),
    (62, 2, "cer_unar"),
    (62, 3, "cer_bite"),
    (62, 4, "cer_cryA"),
    (62, 5, "cer_taoB"),
    (62, 6, "cer_jkMX"),
    (62, 7, "cer_kamu"),
    (62, 8, "cer_cryB"),
    (62, 9, "cer_runMX"),
    (62, 23, "call"),
    (62, 45, "ft_rmA"),
    (62, 46, "ft_rmB"),
    (62, 47, "taore_st"),
    (63, 23, "sw_medal2"),
    (63, 33, "ELV1"),
    (63, 34, "ELV2"),
    (63, 42, "ft_sts"),
    (63, 45, "ft_rmA"),
    (63, 46, "ft_rmB"),
    (63, 47, "taore_st"),
    (64, 22, "mv_stn"),
    (64, 23, "sw_push2"),
    (64, 24, "crank"),
    (64, 25, "R_pass"),
    (64, 26, "undergat"),
    (64, 45, "ft_caveA"),
    (64, 46, "ft_caveB"),
    (64, 47, "taore_ca"),
    (65, 23, "R_pass"),
    (65, 24, "crank"),
    (65, 29, "cancel"),
    (65, 30, "type01"),
    (65, 31, "type02"),
    (65, 45, "ft_caveA"),
    (65, 46, "ft_caveB"),
    (65, 47, "taore_ca"),
    (66, 0, "He_walkA"),
    (66, 1, "He_walkB"),
    (66, 2, "He_jump"),
    (66, 3, "He_att"),
    (66, 4, "He_land"),
    (66, 5, "He_smash"),
    (66, 6, "He_dam"),
    (66, 7, "He_Nout"),
    (66, 20, "DM_gacha"),
    (66, 23, "hookmix"),
    (66, 24, "lockoutA"),
    (66, 25, "lockoutB"),
    (66, 26, "magnum"),
    (66, 45, "ft_caveA"),
    (66, 46, "ft_caveB"),
    (66, 47, "taore_ca"),
    (67, 0, "He_walkA"),
    (67, 1, "He_walkB"),
    (67, 2, "He_jump"),
    (67, 3, "He_att"),
    (67, 4, "He_land"),
    (67, 5, "He_smash"),
    (67, 6, "He_dam"),
    (67, 7, "He_Nout"),
    (67, 23, "magnum"),
    (67, 45, "ft_caveA"),
    (67, 46, "ft_caveB"),
    (67, 47, "taore_ca"),
    (68, 0, "He_walkA"),
    (68, 1, "He_walkB"),
    (68, 2, "He_jump"),
    (68, 3, "He_att"),
    (68, 4, "He_land"),
    (68, 5, "He_smash"),
    (68, 6, "He_dam"),
    (68, 7, "He_Nout"),
    (68, 23, "gun_pf"),
    (68, 25, "yk_30a"),
    (68, 26, "taore_ca"),
    (68, 27, "v00d_02"),
    (68, 45, "ft_caveA"),
    (68, 46, "ft_caveB"),
    (68, 47, "taore_ca"),
    (69, 0, "He_walkA"),
    (69, 1, "He_walkB"),
    (69, 2, "He_jump"),
    (69, 3, "He_att"),
    (69, 4, "He_land"),
    (69, 5, "He_smash"),
    (69, 6, "He_dam"),
    (69, 7, "He_Nout"),
    (69, 20, "DM_gacha"),
    (69, 23, "hookmix"),
    (69, 24, "lockoutA"),
    (69, 25, "lockoutB"),
    (69, 26, "rck_hitA"),
    (69, 27, "rck_hitB"),
    (69, 28, "rck_brok"),
    (69, 30, "rck_stop"),
    (69, 45, "ft_caveA"),
    (69, 46, "ft_caveB"),
    (69, 47, "taore_ca"),
    (70, 0, "kuasi_A"),
    (70, 1, "kuasi_B"),
    (70, 2, "kuasi_C"),
    (70, 3, "sp_rakk"),
    (70, 4, "sp_atck"),
    (70, 5, "sp_bomb"),
    (70, 6, "sp_fumu"),
    (70, 7, "sp_Doku"),
    (70, 8, "poison"),
    (70, 45, "ft_spA"),
    (70, 46, "ft_spB"),
    (70, 47, "taore_sp"),
    (71, 0, "PYe_mena"),
    (71, 1, "PYe_hit"),
    (71, 2, "PYe_fall"),
    (71, 20, "DM_gacha"),
    (71, 22, "mv_cp"),
    (71, 23, "hookmix"),
    (71, 24, "lockoutA"),
    (71, 25, "lockoutB"),
    (71, 45, "ft_caveA"),
    (71, 46, "ft_caveB"),
    (71, 47, "taore_ca"),
    (72, 29, "cancel"),
    (72, 30, "type01"),
    (72, 31, "type02"),
    (72, 32, "item02"),
    (72, 33, "item01"),
    (72, 45, "ft_rmA"),
    (72, 46, "ft_rmB"),
    (72, 47, "taore_st"),
    (73, 23, "R_pass"),
    (73, 24, "crank"),
    (73, 26, "rck_hitA"),
    (73, 27, "rck_hitB"),
    (73, 30, "rck_stop"),
    (73, 33, "ELV1"),
    (73, 34, "ELV2"),
    (73, 45, "ft_caveA"),
    (73, 46, "ft_caveB"),
    (73, 47, "taore_ca"),
    (74, 45, "ft_plaA"),
    (74, 46, "ft_plaB"),
    (74, 47, "taore_pl"),
    (87, 0, "VN_kazea"),
    (87, 1, "VN_hitA"),
    (87, 2, "VN_hitB"),
    (87, 3, "VN_sime"),
    (87, 4, "VN_OUT"),
    (87, 22, "mv_stn"),
    (87, 23, "ch_sime"),
    (87, 24, "ji_sime"),
    (87, 45, "ft_wdA"),
    (87, 46, "ft_wdB"),
    (87, 47, "taore_wd"),
    (88, 0, "z_taore"),
    (88, 1, "z_ftL"),
    (88, 2, "z_ftR"),
    (88, 3, "z_kamu"),
    (88, 4, "z_simo02"),
    (88, 5, "z_simo01"),
    (88, 6, "z_head"),
    (88, 7, "z_Hkick"),
    (88, 8, "z_Ugoron"),
    (88, 9, "z_simo03"),
    (88, 36, "drw_opwd"),
    (88, 37, "drw_shwd"),
    (88, 38, "key_desk"),
    (88, 42, "ft_cpA"),
    (88, 43, "ft_cpB"),
    (88, 44, "taore_cp"),
    (88, 45, "ft_wdA"),
    (88, 46, "ft_wdB"),
    (88, 47, "taore_wd"),
    (89, 23, "bathMIX"),
    (89, 42, "ft_cpA"),
    (89, 43, "ft_cpB"),
    (89, 44, "taore_cp"),
    (89, 45, "ft_wdA"),
    (89, 46, "ft_wdB"),
    (89, 47, "taore_wd"),
    (90, 29, "cancel"),
    (90, 30, "type01"),
    (90, 31, "type02"),
    (90, 32, "item02"),
    (90, 33, "item01"),
    (90, 42, "ft_cpA"),
    (90, 43, "ft_cpB"),
    (90, 44, "taore_cp"),
    (90, 45, "ft_wdA"),
    (90, 46, "ft_wdB"),
    (90, 47, "taore_wd"),
    (91, 0, "kuasi_A"),
    (91, 1, "kuasi_B"),
    (91, 2, "kuasi_C"),
    (91, 3, "sp_rakk"),
    (91, 4, "sp_atck"),
    (91, 5, "sp_bomb"),
    (91, 6, "sp_fumu"),
    (91, 7, "sp_Doku"),
    (91, 8, "sp_sanj2"),
    (91, 45, "ft_wdA"),
    (91, 46, "ft_wdB"),
    (91, 47, "taore_wd"),
    (92, 0, "bee4_ed"),
    (92, 1, "hatinage"),
    (92, 2, "bee_fumu"),
    (92, 20, "D_gacha"),
    (92, 22, "mv_stn"),
    (92, 23, "gun_pf"),
    (92, 25, "yk_405_"),
    (92, 34, "key_apar"),
    (92, 45, "ft_wdA"),
    (92, 46, "ft_wdB"),
    (92, 47, "taore_wd"),
    (93, 22, "mv_cp"),
    (93, 35, "ft_lad"),
    (93, 36, "drw_opwd"),
    (93, 37, "drw_shwd"),
    (93, 38, "key_desk"),
    (93, 42, "ft_cpA"),
    (93, 43, "ft_cpB"),
    (93, 44, "taore_cp"),
    (93, 45, "ft_wdA"),
    (93, 46, "ft_wdB"),
    (93, 47, "taore_wd"),
    (94, 0, "z_taore"),
    (94, 1, "z_ftL"),
    (94, 2, "z_ftR"),
    (94, 3, "z_kamu"),
    (94, 4, "z_aoya02"),
    (94, 5, "z_aoya01"),
    (94, 6, "z_head"),
    (94, 7, "z_Hkick"),
    (94, 8, "z_Ugoron"),
    (94, 9, "z_aoya03"),
    (94, 42, "ft_cpA"),
    (94, 43, "ft_cpB"),
    (94, 44, "taore_cp"),
    (94, 45, "ft_wdA"),
    (94, 46, "ft_wdB"),
    (94, 47, "taore_wd"),
    (95, 0, "bee4_ed"),
    (95, 1, "hatinage"),
    (95, 2, "bee_fumu"),
    (95, 20, "D_gacha"),
    (95, 23, "BEEP"),
    (95, 24, "keyopen"),
    (95, 34, "key_apar"),
    (95, 45, "ft_wdA"),
    (95, 46, "ft_wdB"),
    (95, 47, "taore_wd"),
    (96, 45, "ft_wdA"),
    (96, 46, "ft_wdB"),
    (96, 47, "taore_wd"),
    (97, 23, "slide_bk"),
    (97, 36, "drw_opwd"),
    (97, 37, "drw_shwd"),
    (97, 38, "key_desk"),
    (97, 42, "ft_cpA"),
    (97, 43, "ft_cpB"),
    (97, 44, "taore_cp"),
    (97, 45, "ft_wdA"),
    (97, 46, "ft_wdB"),
    (97, 47, "taore_wd"),
    (98, 0, "z_taore"),
    (98, 1, "z_ftL"),
    (98, 2, "z_ftR"),
    (98, 3, "z_kamu"),
    (98, 4, "z_aoya02"),
    (98, 5, "z_aoya01"),
    (98, 6, "z_head"),
    (98, 7, "z_Hkick"),
    (98, 8, "z_Ugoron"),
    (98, 9, "z_aoya03"),
    (98, 42, "ft_cpA"),
    (98, 43, "ft_cpB"),
    (98, 44, "taore_cp"),
    (98, 45, "ft_wdA"),
    (98, 46, "ft_wdB"),
    (98, 47, "taore_wd"),
    (99, 0, "VN_air"),
    (99, 1, "VN_kazeA"),
    (99, 2, "VN_hitA"),
    (99, 3, "VN_kazeB"),
    (99, 4, "VN_hitL"),
    (99, 5, "VN_hitB"),
    (99, 6, "VN_sime"),
    (99, 7, "VN_OUT"),
    (99, 8, "VN_fall"),
    (99, 9, "VN_OUT2"),
    (99, 23, "ch_sime"),
    (99, 24, "ji_sime"),
    (99, 25, "taore_s1"),
    (99, 26, "taore_wa"),
    (99, 27, "filefall"),
    (99, 28, "VN_body"),
    (99, 29, "sliding"),
    (99, 30, "blaze"),
    (99, 42, "ft_wdA"),
    (99, 43, "ft_wdB"),
    (99, 44, "taore_wd"),
    (99, 45, "ft_cpA"),
    (99, 46, "ft_cpB"),
    (99, 47, "taore_cp"),
    (100, 22, "mv_cp"),
    (100, 23, "inwater"),
    (100, 35, "ft_lad"),
    (100, 39, "ft_kibA"),
    (100, 40, "ft_kibB"),
    (100, 41, "taore_st"),
    (100, 42, "ft_swimA"),
    (100, 43, "ft_swimB"),
    (100, 44, "taore_st"),
    (100, 45, "ft_concA"),
    (100, 46, "ft_concB"),
    (100, 47, "taore_st"),
    (101, 0, "nep_attB"),
    (101, 1, "nep_attA"),
    (101, 2, "nep_nomu"),
    (101, 3, "nep_tura"),
    (101, 4, "nep_twis"),
    (101, 5, "nep_jump"),
    (101, 20, "DM_gacha"),
    (101, 34, "key_mtr"),
    (101, 42, "ft_swimA"),
    (101, 43, "ft_swimB"),
    (101, 44, "taore_st"),
    (101, 45, "ft_concA"),
    (101, 46, "ft_concB"),
    (101, 47, "taore_st"),
    (102, 23, "V_JOLT"),
    (102, 24, "pakiA"),
    (102, 25, "pakiB"),
    (102, 42, "ft_swimA"),
    (102, 43, "ft_swimB"),
    (102, 44, "taore_st"),
    (102, 45, "ft_concA"),
    (102, 46, "ft_concB"),
    (102, 47, "taore_st"),
    (103, 45, "ft_concA"),
    (103, 46, "ft_concB"),
    (103, 47, "taore_st"),
    (104, 23, "Sw_lever"),
    (104, 24, "Sw_411"),
    (104, 42, "ft_swimA"),
    (104, 43, "ft_swimB"),
    (104, 44, "taore_st"),
    (104, 45, "ft_concA"),
    (104, 46, "ft_concB"),
    (104, 47, "taore_st"),
    (116, 20, "DM_gacha"),
    (116, 34, "key_mtr"),
    (116, 35, "ft_lad"),
    (116, 42, "ft_mtnA"),
    (116, 43, "ft_mtnB"),
    (116, 44, "taore_pl"),
    (116, 45, "ft_concA"),
    (116, 46, "ft_concB"),
    (116, 47, "taore_st"),
    (117, 23, "HUNTER"),
    (117, 24, "battery"),
    (117, 45, "ft_concA"),
    (117, 46, "ft_concB"),
    (117, 47, "taore_st"),
    (118, 32, "item02"),
    (118, 33, "item01"),
    (118, 35, "ft_lad"),
    (118, 45, "ft_concA"),
    (118, 46, "ft_concB"),
    (118, 47, "taore_st"),
    (119, 0, "z_taore"),
    (119, 1, "ze_ftL"),
    (119, 2, "z_ftR"),
    (119, 3, "ze_kamu"),
    (119, 4, "ze_tomo2"),
    (119, 5, "ze_tomo1"),
    (119, 6, "ze_head"),
    (119, 7, "ze_haki"),
    (119, 8, "ze_sanj"),
    (119, 9, "ze_tomo3"),
    (119, 20, "DM_gacha"),
    (119, 39, "ft_mtnA"),
    (119, 40, "ft_mtnB"),
    (119, 41, "taore_pl"),
    (119, 42, "ft_concA"),
    (119, 43, "ft_concB"),
    (119, 44, "taore_st"),
    (119, 45, "ft_stmt"),
    (120, 23, "panel01"),
    (120, 24, "panel02"),
    (120, 25, "pillar"),
    (120, 26, "slide1"),
    (120, 27, "slide2"),
    (120, 28, "slide3"),
    (120, 45, "ft_concA"),
    (120, 46, "ft_concB"),
    (120, 47, "taore_st"),
    (121, 0, "z_taore"),
    (121, 1, "zep_ftL"),
    (121, 2, "z_ftR"),
    (121, 3, "ze_kamu"),
    (121, 4, "z_nisi2"),
    (121, 5, "z_nisi1"),
    (121, 6, "ze_head"),
    (121, 7, "ze_haki"),
    (121, 8, "ze_sanj"),
    (121, 9, "z_nisi3"),
    (121, 10, "FL_walk"),
    (121, 11, "FL_jump"),
    (121, 12, "steam_b"),
    (121, 13, "FL_ceil"),
    (121, 14, "FL_fall"),
    (121, 15, "FL_slash"),
    (121, 16, "FL_att"),
    (121, 17, "FL_dam"),
    (121, 18, "FL_out"),
    (121, 20, "DM_gacha"),
    (121, 23, "kns_tetu"),
    (121, 42, "ft_concA"),
    (121, 43, "ft_concB"),
    (121, 44, "taore_st"),
    (121, 45, "ft_stmt"),
    (122, 23, "power-on"),
    (122, 24, "sl_click"),
    (122, 25, "sl_crmov"),
    (122, 26, "Dhit_ch"),
    (122, 27, "Dhit_ji"),
    (122, 28, "Gutspose"),
    (122, 45, "ft_linA"),
    (122, 46, "ft_linB"),
    (122, 47, "taore_st"),
    (123, 20, "DM_gacha"),
    (123, 22, "mv_cp"),
    (123, 23, "mv_step"),
    (123, 24, "type02"),
    (123, 25, "chakuchi"),
    (123, 26, "sw_push3"),
    (123, 33, "key_indr"),
    (123, 35, "ft_step"),
    (123, 36, "drw_c_op"),
    (123, 37, "drw_c_sh"),
    (123, 38, "key_desk"),
    (123, 45, "ft_concA"),
    (123, 46, "ft_concB"),
    (123, 47, "taore_st"),
    (124, 20, "DM_gacha"),
    (124, 23, "dlbeep"),
    (124, 24, "dbrock"),
    (124, 45, "ft_concA"),
    (124, 46, "ft_concB"),
    (124, 47, "taore_st"),
    (125, 0, "z_taore"),
    (125, 1, "zep_ftL"),
    (125, 2, "z_ftR"),
    (125, 3, "ze_kamu"),
    (125, 4, "z_nisi2"),
    (125, 5, "z_nisi1"),
    (125, 6, "ze_head"),
    (125, 7, "ze_haki"),
    (125, 8, "ze_sanj"),
    (125, 9, "z_nisi3"),
    (125, 24, "type02"),
    (125, 45, "ft_concA"),
    (125, 46, "ft_concB"),
    (125, 47, "taore_st"),
    (126, 22, "mv_cp"),
    (126, 23, "sw_btn"),
    (126, 36, "drw_opmt"),
    (126, 37, "drw_shmt"),
    (126, 38, "key_desk"),
    (126, 45, "ft_concA"),
    (126, 46, "ft_concB"),
    (126, 47, "taore_st"),
    (127, 20, "DM_gacha"),
    (127, 34, "key_mtr"),
    (127, 45, "ft_concA"),
    (127, 46, "ft_concB"),
    (127, 47, "taore_st"),
    (128, 0, "z_taore"),
    (128, 1, "ze_ftL"),
    (128, 2, "z_ftR"),
    (128, 3, "ze_kamu"),
    (128, 4, "ze_tomo2"),
    (128, 5, "ze_tomo1"),
    (128, 6, "ze_head"),
    (128, 7, "ze_haki"),
    (128, 8, "ze_sanj"),
    (128, 9, "ze_tomo3"),
    (128, 24, "sw_btn"),
    (128, 25, "ELV_ON"),
    (128, 45, "ft_concA"),
    (128, 46, "ft_concB"),
    (128, 47, "taore_st"),
    (129, 23, "escELV"),
    (129, 45, "ft_plaA"),
    (129, 46, "ft_plaB"),
    (129, 47, "taore_pl"),
    (130, 29, "cancel"),
    (130, 30, "type01"),
    (130, 31, "type02"),
    (130, 32, "item02"),
    (130, 33, "item01"),
    (130, 45, "ft_concA"),
    (130, 46, "ft_concB"),
    (130, 47, "taore_st"),
    (131, 0, "FL_walk"),
    (131, 1, "FL_jump"),
    (131, 2, "steam_b"),
    (131, 3, "FL_ceil"),
    (131, 4, "FL_fall"),
    (131, 5, "FL_slash"),
    (131, 6, "FL_att"),
    (131, 7, "FL_dam"),
    (131, 8, "FL_out"),
    (131, 23, "conpane"),
    (131, 39, "ft_mtnA"),
    (131, 40, "ft_mtnB"),
    (131, 41, "taore_pl"),
    (131, 42, "ft_plaA"),
    (131, 43, "ft_plaB"),
    (131, 44, "taore_pl"),
    (131, 45, "ft_concA"),
    (131, 46, "ft_concB"),
    (131, 47, "taore_st"),
    (132, 0, "FL_walk"),
    (132, 1, "FL_jump"),
    (132, 2, "steam_b"),
    (132, 3, "FL_ceil"),
    (132, 4, "FL_fall"),
    (132, 5, "FL_slash"),
    (132, 6, "FL_att"),
    (132, 7, "FL_dam"),
    (132, 8, "FL_out"),
    (132, 23, "type02"),
    (132, 24, "steam_a"),
    (132, 39, "ft_mtnA"),
    (132, 40, "ft_mtnB"),
    (132, 41, "taore_pl"),
    (132, 42, "ft_plaA"),
    (132, 43, "ft_plaB"),
    (132, 44, "taore_pl"),
    (132, 45, "ft_concA"),
    (132, 46, "ft_concB"),
    (132, 47, "taore_st"),
    (133, 0, "FL_walk"),
    (133, 1, "FL_jump"),
    (133, 2, "steam_b"),
    (133, 3, "FL_ceil"),
    (133, 4, "FL_fall"),
    (133, 5, "FL_slash"),
    (133, 6, "FL_att"),
    (133, 7, "FL_dam"),
    (133, 8, "FL_out"),
    (133, 23, "panel02"),
    (133, 24, "steam_a"),
    (133, 39, "ft_mtnA"),
    (133, 40, "ft_mtnB"),
    (133, 41, "taore_pl"),
    (133, 42, "ft_plaA"),
    (133, 43, "ft_plaB"),
    (133, 44, "taore_pl"),
    (133, 45, "ft_concA"),
    (133, 46, "ft_concB"),
    (133, 47, "taore_st"),
    (134, 45, "ft_concA"),
    (134, 46, "ft_concB"),
    (134, 47, "taore_st"),
    (135, 0, "TY_foot"),
    (135, 1, "TY_kaze"),
    (135, 2, "TY_slice"),
    (135, 3, "TY_HIT"),
    (135, 4, "TY_trust"),
    (135, 6, "TY_taore"),
    (135, 7, "TY_nage"),
    (135, 23, "bubble_S"),
    (135, 24, "bubble_L"),
    (135, 25, "crackMIX"),
    (135, 26, "glass"),
    (135, 29, "conpane"),
    (135, 30, "key_lost"),
    (135, 42, "ft_mtnA"),
    (135, 43, "ft_mtnB"),
    (135, 44, "taore_pl"),
    (135, 45, "ft_plaA"),
    (135, 46, "ft_plaB"),
    (135, 47, "taore_pl"),
    (136, 24, "40S&W"),
    (136, 27, "smash"),
    (136, 28, "taore_we"),
    (136, 42, "ft_mtnA"),
    (136, 43, "ft_mtnB"),
    (136, 44, "taore_pl"),
    (136, 45, "ft_concA"),
    (136, 46, "ft_concB"),
    (136, 47, "taore_st"),
    (137, 23, "Elv515"),
    (137, 45, "ft_plaA"),
    (137, 46, "ft_plaB"),
    (137, 47, "taore_pl"),
    (145, 29, "cancel"),
    (145, 30, "type01"),
    (145, 31, "type02"),
    (145, 32, "item02"),
    (145, 33, "item01"),
    (145, 45, "ft_wdA"),
    (145, 46, "ft_wdB"),
    (145, 47, "taore_wd"),
    (146, 0, "HU_walkA"),
    (146, 1, "HU_walkB"),
    (146, 2, "HU_jump"),
    (146, 3, "HU_att"),
    (146, 4, "HU_land"),
    (146, 5, "HU_smash"),
    (146, 6, "HU_dam"),
    (146, 7, "HU_Nout"),
    (146, 10, "Reb01"),
    (146, 11, "Reb02"),
    (146, 12, "Reb03"),
    (146, 13, "Reb04"),
    (146, 14, "reb_scr"),
    (146, 15, "slback_c"),
    (146, 20, "D_gacha"),
    (146, 42, "ft_floA"),
    (146, 43, "ft_floB"),
    (146, 44, "taore_wd"),
    (146, 45, "ft_stwd"),
    (147, 36, "drw_opwd"),
    (147, 37, "drw_shwd"),
    (147, 38, "key_desk"),
    (147, 45, "ft_cpA"),
    (147, 46, "ft_cpB"),
    (147, 47, "taore_cp"),
    (148, 0, "HU_walkA"),
    (148, 1, "HU_walkB"),
    (148, 2, "HU_jump"),
    (148, 3, "HU_att"),
    (148, 4, "HU_land"),
    (148, 5, "HU_smash"),
    (148, 6, "HU_dam"),
    (148, 7, "HU_Nout"),
    (148, 20, "D_gacha"),
    (148, 33, "key_indr"),
    (148, 34, "key_door"),
    (148, 45, "ft_wdA"),
    (148, 46, "ft_wdB"),
    (148, 47, "taore_wd"),
    (149, 0, "HU_walkA"),
    (149, 1, "HU_walkB"),
    (149, 2, "HU_jump"),
    (149, 3, "HU_att"),
    (149, 4, "HU_land"),
    (149, 5, "HU_smash"),
    (149, 6, "HU_dam"),
    (149, 7, "HU_Nout"),
    (149, 20, "D_gacha"),
    (149, 34, "key_door"),
    (149, 42, "ft_wdA"),
    (149, 43, "ft_wdB"),
    (149, 44, "taore_wd"),
    (149, 45, "ft_cpA"),
    (149, 46, "ft_cpB"),
    (149, 47, "taore_cp"),
    (150, 45, "ft_rmA"),
    (150, 46, "ft_rmB"),
    (150, 47, "taore_st"),
    (151, 20, "D_gacha"),
    (151, 29, "cancel"),
    (151, 30, "type01"),
    (151, 31, "type02"),
    (151, 34, "key_door"),
    (151, 39, "ft_haA"),
    (151, 40, "ft_haB"),
    (151, 41, "taore_st"),
    (151, 42, "ft_cpA"),
    (151, 43, "ft_cpB"),
    (151, 44, "taore_cp"),
    (151, 45, "ft_stwp"),
    (152, 20, "D_gacha"),
    (152, 22, "mv_show"),
    (152, 23, "mv_step"),
    (152, 34, "key_door"),
    (152, 35, "ft_step"),
    (152, 42, "ft_linA"),
    (152, 43, "ft_linB"),
    (152, 44, "taore_st"),
    (152, 45, "ft_wdA"),
    (152, 46, "ft_wdB"),
    (152, 47, "taore_wd"),
    (153, 0, "kuasi_A"),
    (153, 1, "kuasi_B"),
    (153, 2, "kuasi_C"),
    (153, 3, "sp_rakk"),
    (153, 4, "sp_atck"),
    (153, 5, "sp_bomb"),
    (153, 6, "sp_fumu"),
    (153, 7, "sp_Doku"),
    (153, 8, "sp_sanj2"),
    (153, 22, "mv_show"),
    (153, 45, "ft_cpA"),
    (153, 46, "ft_cpB"),
    (153, 47, "taore_cp"),
    (154, 0, "HU_walkA"),
    (154, 1, "HU_walkB"),
    (154, 2, "HU_jump"),
    (154, 3, "HU_att"),
    (154, 4, "HU_land"),
    (154, 5, "HU_smash"),
    (154, 6, "HU_dam"),
    (154, 7, "HU_Nout"),
    (154, 20, "DM_gacha"),
    (154, 34, "key_mtr"),
    (154, 45, "ft_wdA"),
    (154, 46, "ft_wdB"),
    (154, 47, "taore_wd"),
    (155, 0, "HU_walkA"),
    (155, 1, "HU_walkB"),
    (155, 2, "HU_jump"),
    (155, 3, "HU_att"),
    (155, 4, "HU_land"),
    (155, 5, "HU_smash"),
    (155, 6, "HU_dam"),
    (155, 7, "HU_Nout"),
    (155, 20, "D_gacha"),
    (155, 33, "key_indr"),
    (155, 34, "key_door"),
    (155, 45, "ft_wdA"),
    (155, 46, "ft_wdB"),
    (155, 47, "taore_wd"),
    (156, 0, "HU_walkA"),
    (156, 1, "HU_walkB"),
    (156, 2, "HU_jump"),
    (156, 3, "HU_att"),
    (156, 4, "HU_land"),
    (156, 5, "HU_smash"),
    (156, 6, "HU_dam"),
    (156, 7, "HU_Nout"),
    (156, 20, "D_gacha"),
    (156, 42, "ft_wdA"),
    (156, 43, "ft_wdB"),
    (156, 44, "taore_wd"),
    (156, 45, "ft_stwd"),
    (157, 45, "ft_linA"),
    (157, 46, "ft_linB"),
    (157, 47, "taore_st"),
    (158, 23, "TIGEREYE"),
    (158, 45, "ft_linA"),
    (158, 46, "ft_linB"),
    (158, 47, "taore_st"),
    (159, 0, "z_taore"),
    (159, 1, "z_ftL"),
    (159, 2, "z_ftR"),
    (159, 3, "z_kamu"),
    (159, 4, "z_osou"),
    (159, 5, "z_unaruA"),
    (159, 6, "z_head"),
    (159, 7, "z_Hkick"),
    (159, 8, "z_Ugoron"),
    (159, 9, "z_unaruB"),
    (159, 23, "clst_op"),
    (159, 24, "clst_hit"),
    (159, 25, "clst_bad"),
    (159, 42, "ft_wdA"),
    (159, 43, "ft_wdB"),
    (159, 44, "taore_wd"),
    (159, 45, "ft_cpA"),
    (159, 46, "ft_cpB"),
    (159, 47, "taore_cp"),
    (160, 22, "mv_cp"),
    (160, 42, "ft_linA"),
    (160, 43, "ft_linB"),
    (160, 44, "taore_st"),
    (160, 45, "ft_wdA"),
    (160, 46, "ft_wdB"),
    (160, 47, "taore_wd"),
    (161, 33, "key_indr"),
    (161, 42, "ft_wdA"),
    (161, 43, "ft_wdB"),
    (161, 44, "taore_wd"),
    (161, 45, "ft_stwd"),
    (162, 0, "z_taore"),
    (162, 1, "z_ftL"),
    (162, 2, "z_ftR"),
    (162, 3, "z_kamu"),
    (162, 4, "z_simo02"),
    (162, 5, "z_simo01"),
    (162, 6, "z_head"),
    (162, 7, "z_Hkick"),
    (162, 8, "z_Ugoron"),
    (162, 9, "z_simo03"),
    (162, 36, "drw_opwd"),
    (162, 37, "drw_shwd"),
    (162, 38, "key_desk"),
    (162, 45, "ft_linA"),
    (162, 46, "ft_linB"),
    (162, 47, "taore_st"),
    (163, 0, "HU_walkA"),
    (163, 1, "HU_walkB"),
    (163, 2, "HU_jump"),
    (163, 3, "HU_att"),
    (163, 4, "HU_land"),
    (163, 5, "HU_smash"),
    (163, 6, "HU_dam"),
    (163, 7, "HU_Nout"),
    (163, 20, "D_gacha"),
    (163, 34, "key_door"),
    (163, 45, "ft_linA"),
    (163, 46, "ft_linB"),
    (163, 47, "taore_st"),
    (164, 23, "bathMIX"),
    (164, 42, "ft_wdA"),
    (164, 43, "ft_wdB"),
    (164, 44, "taore_wd"),
    (164, 45, "ft_cpA"),
    (164, 46, "ft_cpB"),
    (164, 47, "taore_cp"),
    (165, 0, "z_taore"),
    (165, 1, "z_ftL"),
    (165, 2, "z_ftR"),
    (165, 3, "z_kamu"),
    (165, 4, "z_isi02"),
    (165, 5, "z_isi01"),
    (165, 6, "z_head"),
    (165, 7, "z_Hkick"),
    (165, 8, "z_Ugoron"),
    (165, 9, "z_isi03"),
    (165, 45, "ft_linA"),
    (165, 46, "ft_linB"),
    (165, 47, "taore_st"),
    (166, 20, "D_gacha"),
    (166, 23, "kickdoor"),
    (166, 24, "lock_mix"),
    (166, 25, "ceilpres"),
    (166, 29, "tao_wall"),
    (166, 45, "ft_linA"),
    (166, 46, "ft_linB"),
    (166, 47, "taore_st"),
    (167, 23, "gatan"),
    (167, 24, "sw_trap"),
    (167, 25, "ceilpres"),
    (167, 42, "ft_cpA"),
    (167, 43, "ft_cpB"),
    (167, 44, "taore_cp"),
    (167, 45, "ft_wdA"),
    (167, 46, "ft_wdB"),
    (167, 47, "taore_wd"),
    (168, 0, "RVcar1"),
    (168, 1, "RVpat"),
    (168, 2, "RVcar2"),
    (168, 3, "RVwing1"),
    (168, 4, "RVwing2"),
    (168, 5, "RVfryed"),
    (168, 23, "sw_btn"),
    (168, 24, "frame_ga"),
    (168, 25, "frame_fa"),
    (168, 45, "ft_wdA"),
    (168, 46, "ft_wdB"),
    (168, 47, "taore_wd"),
    (169, 29, "cancel"),
    (169, 30, "type01"),
    (169, 31, "type02"),
    (169, 32, "item02"),
    (169, 33, "item01"),
    (169, 45, "ft_wdA"),
    (169, 46, "ft_wdB"),
    (169, 47, "taore_wd"),
    (170, 23, "sw_btn"),
    (170, 36, "drw_opwd"),
    (170, 37, "drw_shwd"),
    (170, 38, "key_desk"),
    (170, 42, "ft_cpA"),
    (170, 43, "ft_cpB"),
    (170, 44, "taore_cp"),
    (170, 45, "ft_wdA"),
    (170, 46, "ft_wdB"),
    (170, 47, "taore_wd"),
    (171, 0, "HU_walkA"),
    (171, 1, "HU_walkB"),
    (171, 2, "HU_jump"),
    (171, 3, "HU_att"),
    (171, 4, "HU_land"),
    (171, 5, "HU_smash"),
    (171, 6, "HU_dam"),
    (171, 7, "HU_Nout"),
    (171, 45, "ft_rmA"),
    (171, 46, "ft_rmB"),
    (171, 47, "taore_st"),
    (172, 22, "mv_step"),
    (172, 35, "ft_step"),
    (172, 45, "ft_rmA"),
    (172, 46, "ft_rmB"),
    (172, 47, "taore_st"),
    (173, 23, "kigae1"),
    (173, 24, "kigae2"),
    (173, 25, "Zipper3"),
    (173, 26, "Zipper1"),
    (173, 27, "Zipper2"),
    (173, 35, "ft_step"),
    (173, 45, "ft_linA"),
    (173, 46, "ft_linB"),
    (173, 47, "taore_st"),
    (174, 45, "ft_wdA"),
    (174, 46, "ft_wdB"),
    (174, 47, "taore_wd"),
    (175, 0, "HU_walkA"),
    (175, 1, "HU_walkB"),
    (175, 2, "HU_jump"),
    (175, 3, "HU_att"),
    (175, 4, "HU_land"),
    (175, 5, "HU_smash"),
    (175, 6, "HU_dam"),
    (175, 7, "HU_Nout"),
    (175, 20, "D_gacha"),
    (175, 23, "BEEP"),
    (175, 34, "key_door"),
    (175, 42, "ft_floA"),
    (175, 43, "ft_floB"),
    (175, 44, "taore_wd"),
    (175, 45, "ft_stwd"),
    (176, 0, "HU_walkA"),
    (176, 1, "HU_walkB"),
    (176, 2, "HU_jump"),
    (176, 3, "HU_att"),
    (176, 4, "HU_land"),
    (176, 5, "HU_smash"),
    (176, 6, "HU_dam"),
    (176, 7, "HU_Nout"),
    (176, 22, "mv_stn"),
    (176, 23, "BRK_stn"),
    (176, 45, "ft_cpA"),
    (176, 46, "ft_cpB"),
    (176, 47, "taore_cp"),
    (177, 42, "ft_cpA"),
    (177, 43, "ft_cpB"),
    (177, 44, "taore_cp"),
    (177, 45, "ft_stwp"),
    (178, 0, "HU_walkA"),
    (178, 1, "HU_walkB"),
    (178, 2, "HU_jump"),
    (178, 3, "HU_att"),
    (178, 4, "HU_land"),
    (178, 5, "HU_smash"),
    (178, 6, "HU_dam"),
    (178, 7, "HU_Nout"),
    (178, 20, "D_gacha"),
    (178, 34, "key_door"),
    (178, 45, "ft_cpA"),
    (178, 46, "ft_cpB"),
    (178, 47, "taore_cp"),
    (179, 22, "mv_stn"),
    (179, 23, "macha"),
    (179, 24, "shuter"),
    (179, 45, "ft_linA"),
    (179, 46, "ft_linB"),
    (179, 47, "taore_st"),
    (180, 0, "HU_walkA"),
    (180, 1, "HU_walkB"),
    (180, 2, "HU_jump"),
    (180, 3, "HU_att"),
    (180, 4, "HU_land"),
    (180, 5, "HU_smash"),
    (180, 6, "HU_dam"),
    (180, 7, "HU_Nout"),
    (180, 10, "Reb01"),
    (180, 11, "Reb02"),
    (180, 12, "Reb03"),
    (180, 13, "Reb04"),
    (180, 14, "reb_scr"),
    (180, 15, "slback_c"),
    (180, 45, "ft_cpA"),
    (180, 46, "ft_cpB"),
    (180, 47, "taore_cp"),
    (181, 0, "HU_walkA"),
    (181, 1, "HU_walkB"),
    (181, 2, "HU_jump"),
    (181, 3, "HU_att"),
    (181, 4, "HU_land"),
    (181, 5, "HU_smash"),
    (181, 6, "HU_dam"),
    (181, 7, "HU_Nout"),
    (181, 20, "D_gacha"),
    (181, 33, "key_indr"),
    (181, 34, "key_door"),
    (181, 39, "ft_cpA"),
    (181, 40, "ft_cpB"),
    (181, 41, "taore_cp"),
    (181, 42, "ft_wdA"),
    (181, 43, "ft_wdB"),
    (181, 44, "taore_wd"),
    (181, 45, "ft_stwd"),
    (182, 0, "z_taore"),
    (182, 1, "z_ftL"),
    (182, 2, "z_ftR"),
    (182, 3, "z_kamu"),
    (182, 4, "z_osou"),
    (182, 5, "z_unaruA"),
    (182, 6, "z_head"),
    (182, 7, "z_Hkick"),
    (182, 8, "z_Ugoron"),
    (182, 9, "z_unaruB"),
    (182, 23, "d_gigi"),
    (182, 45, "ft_cpA"),
    (182, 46, "ft_cpB"),
    (182, 47, "taore_cp"),
    (183, 42, "ft_cpA"),
    (183, 43, "ft_cpB"),
    (183, 44, "taore_cp"),
    (183, 45, "ft_wdA"),
    (183, 46, "ft_wdB"),
    (183, 47, "taore_wd"),
    (184, 22, "mv_cp"),
    (184, 23, "mv_cp"),
    (184, 24, "sw_push"),
    (184, 25, "aquarium"),
    (184, 42, "ft_wdA"),
    (184, 43, "ft_wdB"),
    (184, 44, "taore_wd"),
    (184, 45, "ft_cpA"),
    (184, 46, "ft_cpB"),
    (184, 47, "taore_cp"),
    (185, 0, "HU_walkA"),
    (185, 1, "HU_walkB"),
    (185, 2, "HU_jump"),
    (185, 3, "HU_att"),
    (185, 4, "HU_land"),
    (185, 5, "HU_smash"),
    (185, 6, "HU_dam"),
    (185, 7, "HU_Nout"),
    (185, 20, "D_gacha"),
    (185, 34, "key_door"),
    (185, 42, "ft_cpA"),
    (185, 43, "ft_cpB"),
    (185, 44, "taore_cp"),
    (185, 45, "ft_wdA"),
    (185, 46, "ft_wdB"),
    (185, 47, "taore_wd"),
    (186, 0, "GV_move"),
    (186, 1, "GV_dino"),
    (186, 2, "GV_p_at"),
    (186, 3, "GV_swing"),
    (186, 4, "GV_bite"),
    (186, 5, "GV_blood"),
    (186, 6, "GV_gulpA"),
    (186, 7, "GV_gulpB"),
    (186, 23, "ch_nom"),
    (186, 24, "ji_nom"),
    (186, 25, "GV_hakai"),
    (186, 35, "ft_lad"),
    (186, 42, "ft_linA"),
    (186, 43, "ft_linB"),
    (186, 44, "taore_st"),
    (186, 45, "ft_caveA"),
    (186, 46, "ft_caveB"),
    (186, 47, "taore_ca"),
    (187, 45, "ft_linA"),
    (187, 46, "ft_linB"),
    (187, 47, "taore_st"),
    (188, 42, "ft_linA"),
    (188, 43, "ft_linB"),
    (188, 44, "taore_st"),
    (188, 45, "ft_stwd"),
    (189, 22, "mv_cp"),
    (189, 42, "ft_wdA"),
    (189, 43, "ft_wdB"),
    (189, 44, "taore_wd"),
    (189, 45, "ft_cpA"),
    (189, 46, "ft_cpB"),
    (189, 47, "taore_cp"),
    (190, 45, "ft_wdA"),
    (190, 46, "ft_wdB"),
    (190, 47, "taore_wd"),
    (191, 45, "ft_cpA"),
    (191, 46, "ft_cpB"),
    (191, 47, "taore_cp"),
    (192, 0, "RVcar1"),
    (192, 1, "RVpat"),
    (192, 2, "RVcar2"),
    (192, 3, "RVwing1"),
    (192, 4, "RVwing2"),
    (192, 5, "RVfryed"),
    (192, 24, "RVpatA"),
    (192, 25, "RVpatB"),
    (192, 45, "ft_wdA"),
    (192, 46, "ft_wdB"),
    (192, 47, "taore_wd"),
    (193, 0, "z_taore"),
    (193, 1, "z_ftL"),
    (193, 2, "z_ftR"),
    (193, 3, "z_kamu"),
    (193, 4, "z_osou"),
    (193, 5, "z_unaruA"),
    (193, 6, "z_head"),
    (193, 7, "z_Hkick"),
    (193, 8, "z_Ugoron"),
    (193, 9, "z_unaruB"),
    (193, 45, "ft_wdA"),
    (193, 46, "ft_wdB"),
    (193, 47, "taore_wd"),
    (194, 0, "z_taore"),
    (194, 1, "z_ftL"),
    (194, 2, "z_ftR"),
    (194, 3, "z_kamu"),
    (194, 4, "z_isi301"),
    (194, 5, "z_isi302"),
    (194, 6, "z_head"),
    (194, 7, "z_Hkick"),
    (194, 8, "z_Ugoron"),
    (194, 9, "z_isi303"),
    (194, 20, "D_gacha"),
    (194, 45, "ft_linA"),
    (194, 46, "ft_linB"),
    (194, 47, "taore_st"),
    (195, 22, "mv_step"),
    (195, 23, "sw_btn"),
    (195, 24, "TIGEREYE"),
    (195, 35, "ft_step"),
    (195, 42, "ft_cpA"),
    (195, 43, "ft_cpB"),
    (195, 44, "taore_cp"),
    (195, 45, "ft_wdA"),
    (195, 46, "ft_wdB"),
    (195, 47, "taore_wd"),
    (196, 0, "z_taore"),
    (196, 1, "z_ftL"),
    (196, 2, "z_ftR"),
    (196, 3, "z_kamu"),
    (196, 4, "z_osou"),
    (196, 5, "z_unaruA"),
    (196, 6, "z_head"),
    (196, 7, "z_Hkick"),
    (196, 8, "z_Ugoron"),
    (196, 9, "z_unaruB"),
    (196, 22, "mv_cp"),
    (196, 36, "drw_opwd"),
    (196, 37, "drw_shwd"),
    (196, 38, "key_desk"),
    (196, 45, "ft_linA"),
    (196, 46, "ft_linB"),
    (196, 47, "taore_st"),
    (197, 22, "mv_stn"),
    (197, 23, "panel02"),
    (197, 24, "slide_b2"),
    (197, 45, "ft_linA"),
    (197, 46, "ft_linB"),
    (197, 47, "taore_st"),
    (198, 45, "ft_linA"),
    (198, 46, "ft_linB"),
    (198, 47, "taore_st"),
    (199, 45, "ft_floA"),
    (199, 46, "ft_floB"),
    (199, 47, "taore_wd"),
    (200, 0, "z_taore"),
    (200, 1, "z_ftL"),
    (200, 2, "z_ftR"),
    (200, 3, "z_kamu"),
    (200, 4, "z_suzu02"),
    (200, 5, "z_suzu01"),
    (200, 6, "z_head"),
    (200, 7, "z_Hkick"),
    (200, 8, "z_Ugoron"),
    (200, 9, "z_suzu03"),
    (200, 10, "z_taore"),
    (200, 11, "z_ftL"),
    (200, 12, "z_ftR"),
    (200, 13, "z_kamu"),
    (200, 14, "z_suzu02"),
    (200, 15, "z_suzu01"),
    (200, 16, "z_head"),
    (200, 17, "z_Hkick"),
    (200, 18, "z_Ugoron"),
    (200, 19, "z_suzu03"),
    (200, 35, "ft_lad"),
    (200, 45, "ft_coefA"),
    (200, 46, "ft_coefB"),
    (200, 47, "taore_st"),
    (201, 10, "z_taore"),
    (201, 11, "z_ftL"),
    (201, 12, "z_ftR"),
    (201, 13, "z_kamu"),
    (201, 14, "z_k02"),
    (201, 15, "z_k01"),
    (201, 16, "z_head"),
    (201, 17, "z_Hkick"),
    (201, 18, "z_Ugoron"),
    (201, 19, "z_k03"),
    (201, 33, "key_indr"),
    (201, 45, "ft_coefA"),
    (201, 46, "ft_coefB"),
    (201, 47, "taore_st"),
    (202, 0, "z_taore"),
    (202, 1, "z_ftL"),
    (202, 2, "z_ftR"),
    (202, 3, "z_kamu"),
    (202, 4, "z_isi02"),
    (202, 5, "z_isi01"),
    (202, 6, "z_head"),
    (202, 7, "z_Hkick"),
    (202, 8, "z_Ugoron"),
    (202, 9, "z_isi03"),
    (202, 20, "D_gacha"),
    (202, 45, "ft_concA"),
    (202, 46, "ft_concB"),
    (202, 47, "taore_st"),
];

/// Empty table slot.
const N: Option<&'static str> = None;

/// The room SE pair table, indexed by the door record's SFX id.
#[rustfmt::skip]
static ROOM_SFX: [&[Option<&'static str>]; 15] = [
    &[Some("Dr_wd01"), Some("Dr_wd02")], // 0
    &[Some("Dr_mtl01"), Some("Dr_mtl02")], // 1
    &[N, Some("Dr_brk01")], // 2
    &[Some("Dr_reb01"), Some("Dr_reb02")], // 3
    &[Some("St_wcp01"), N], // 4
    &[Some("St_wd01"), N], // 5
    &[Some("Ev_mv"), Some("Ev_mv")], // 6
    &[Some("Ev_new01"), Some("Ev_new02")], // 7
    &[Some("Dr_gat01"), Some("Dr_gat02")], // 8
    &[Some("St_mtl01"), N], // 9
    &[Some("Ev_old01"), Some("Ev_old02")], // 10
    &[Some("Ladder01"), N], // 11
    &[Some("Dr_air01"), Some("Dr_air02")], // 12
    &[Some("Ev_lab01"), N], // 13
    &[Some("Ev_ftn01"), Some("Ev_ftn02")], // 14
];

/// UI cue names shared by every character table (slots 4/5/6): the port's
/// screens queue these through [`crate::ui::UiCue`].
pub const UI_CURSOR: &str = "cursor";
/// Confirm cue name (see [`UI_CURSOR`]).
pub const UI_CANCEL: &str = "cancel";
/// Decide cue name (see [`UI_CURSOR`]).
pub const UI_DECIDE: &str = "decide";

/// The non-combat global sound banks (`g_st11`..=`g_st15`), one 16-slot row
/// each. Only the entries the title, character-select, ending and misc screens
/// name are transcribed; `N` is a silent slot. The rows are indexed by
/// `bank - 11`, so `global_sfx` accepts bank `11..=15`.
static GLOBAL_BANKS: [[Option<&'static str>; 16]; 5] = [
    // 11: Bio (the live-action intro's cue).
    [
        Some("Bio01"),
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        Some(UI_CANCEL),
        Some("type01"),
        Some("type02"),
    ],
    // 12: Evil / title.
    [
        Some("Evil01"),
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        Some(UI_CANCEL),
        Some("type01"),
        Some("type02"),
    ],
    // 13: Select (character-select cursor and decide).
    [
        Some("Select06"),
        Some("Select05"),
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
    ],
    // 14: Ending.
    [
        Some("Ending07"),
        N,
        Some("Ending06"),
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        Some(UI_CANCEL),
        Some("type01"),
        Some("type02"),
    ],
    // 15: Win95 / misc.
    [
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        Some("Win95_mg"),
        N,
        N,
        Some("A_mcn03"),
        N,
        N,
        N,
        N,
        N,
    ],
];

/// Name in global bank `bank` (`11..=15`), slot `id` (`0..=15`).
pub fn global_sfx(bank: u8, id: u8) -> Option<&'static str> {
    let row = GLOBAL_BANKS.get(usize::from(bank.checked_sub(11)?))?;
    row.get(usize::from(id)).copied().flatten()
}

/// Every sound effect name shipped in the pack, sorted.
///
/// The room-table names (every named slot of the 203x48 per-room table),
/// the shared character/menu tables, the door pairs and the UI cues. Names
/// that differ only by case share one lowercased pack entry and appear once.
#[rustfmt::skip]
pub static SE_NAMES: [&str; 389] = [
    "40S&W",
    "A_mcn03",
    "BEEP",
    "BRK_stn",
    "Bio01",
    "Ch_ef01",
    "Ch_ef02",
    "Ch_ef03",
    "Ch_ef04",
    "Chris01",
    "Chris02",
    "Chris03",
    "Chris04",
    "Chris08",
    "Chris09",
    "Chris10",
    "ClkDX4LR",
    "DM_gacha",
    "D_gacha",
    "Dhit_ch",
    "Dhit_ji",
    "Dr_air01",
    "Dr_air02",
    "Dr_brk01",
    "Dr_gat01",
    "Dr_gat02",
    "Dr_mtl01",
    "Dr_mtl02",
    "Dr_reb01",
    "Dr_reb02",
    "Dr_wd01",
    "Dr_wd02",
    "ELV1",
    "ELV2",
    "ELVX1",
    "ELVX2",
    "ELV_ON",
    "Elv515",
    "Ending06",
    "Ending07",
    "Ev_ftn01",
    "Ev_ftn02",
    "Ev_lab01",
    "Ev_mv",
    "Ev_new01",
    "Ev_new02",
    "Ev_old01",
    "Ev_old02",
    "Evil01",
    "FL_att",
    "FL_ceil",
    "FL_dam",
    "FL_fall",
    "FL_jump",
    "FL_out",
    "FL_slash",
    "FL_walk",
    "GV_bite",
    "GV_blood",
    "GV_dino",
    "GV_gulpA",
    "GV_gulpB",
    "GV_hakai",
    "GV_move",
    "GV_p_at",
    "GV_swing",
    "Gutspose",
    "HUNTER",
    "HU_Nout",
    "HU_att",
    "HU_dam",
    "HU_jump",
    "HU_land",
    "HU_smash",
    "HU_walkA",
    "HU_walkB",
    "He_Nout",
    "He_att",
    "He_dam",
    "He_jump",
    "He_land",
    "He_smash",
    "He_walkA",
    "He_walkB",
    "Jill01",
    "Jill02",
    "Jill03",
    "Jill04",
    "Jill_ef01",
    "Jill_ef02",
    "Jill_ef03",
    "Jill_ef04",
    "Ladder01",
    "Mapled",
    "PY_fall",
    "PY_hit2",
    "PY_mena",
    "PYe_fall",
    "PYe_hit",
    "PYe_mena",
    "RVcar1",
    "RVcar2",
    "RVfryed",
    "RVpat",
    "RVpatA",
    "RVpatB",
    "RVwing1",
    "RVwing2",
    "R_chris",
    "R_pass",
    "Rancher",
    "Reb01",
    "Reb02",
    "Reb03",
    "Reb04",
    "Reb_ef01",
    "Reb_ef02",
    "Reb_ef03",
    "Reb_ef04",
    "S&W+1",
    "Select05",
    "Select06",
    "St_mtl01",
    "St_wcp01",
    "St_wd01",
    "Sw_411",
    "Sw_lever",
    "TIGEREYE",
    "TY_HIT",
    "TY_crash",
    "TY_foot",
    "TY_kaze",
    "TY_nage",
    "TY_slef",
    "TY_slice",
    "TY_sube",
    "TY_taore",
    "TY_trust",
    "Ty_bomb",
    "VB00_31a",
    "VB00_31b",
    "VB00_31c",
    "VN_OUT",
    "VN_OUT2",
    "VN_air",
    "VN_body",
    "VN_fall",
    "VN_hitA",
    "VN_hitB",
    "VN_hitL",
    "VN_kazeA",
    "VN_kazeB",
    "VN_sime",
    "VN_whip",
    "V_JOLT",
    "Win95_mg",
    "Zipper1",
    "Zipper2",
    "Zipper3",
    "aquarium",
    "bathMIX",
    "battery",
    "bee4_ed",
    "bee_fumu",
    "blaze",
    "bubble_L",
    "bubble_S",
    "call",
    "cancel",
    "ceilpres",
    "cer_bite",
    "cer_cryA",
    "cer_cryB",
    "cer_foot",
    "cer_jkMX",
    "cer_kamu",
    "cer_runMX",
    "cer_taoA",
    "cer_taoB",
    "cer_unar",
    "ch_nom",
    "ch_sime",
    "chakuchi",
    "clst_bad",
    "clst_hit",
    "clst_op",
    "conpane",
    "crackMIX",
    "crank",
    "cursor",
    "d_gigi",
    "dbrock",
    "decide",
    "dlbeep",
    "drw_c_op",
    "drw_c_sh",
    "drw_opmt",
    "drw_opwd",
    "drw_shmt",
    "drw_shwd",
    "emblemA",
    "emblemB",
    "escELV",
    "filefall",
    "frame_fa",
    "frame_ga",
    "ft_caveA",
    "ft_caveB",
    "ft_coefA",
    "ft_coefB",
    "ft_concA",
    "ft_concB",
    "ft_cpA",
    "ft_cpB",
    "ft_floA",
    "ft_floB",
    "ft_haA",
    "ft_haB",
    "ft_kibA",
    "ft_kibB",
    "ft_lad",
    "ft_linA",
    "ft_linB",
    "ft_mtnA",
    "ft_mtnB",
    "ft_plaA",
    "ft_plaB",
    "ft_rmA",
    "ft_rmB",
    "ft_spA",
    "ft_spB",
    "ft_step",
    "ft_stmt",
    "ft_sts",
    "ft_stwd",
    "ft_stwp",
    "ft_swimA",
    "ft_swimB",
    "ft_wdA",
    "ft_wdB",
    "gatan",
    "glass",
    "gun_pf",
    "hatinage",
    "hookmix",
    "inwater",
    "item01",
    "item02",
    "ji_nom",
    "ji_sime",
    "key_apar",
    "key_desk",
    "key_door",
    "key_indr",
    "key_lost",
    "key_mtr",
    "keyopen",
    "kickdoor",
    "kigae1",
    "kigae2",
    "kns_tetu",
    "kuasi_A",
    "kuasi_B",
    "kuasi_C",
    "lock_mix",
    "lockout",
    "lockoutA",
    "lockoutB",
    "macha",
    "magnum",
    "magnum01",
    "mv_clk",
    "mv_cp",
    "mv_show",
    "mv_step",
    "mv_stn",
    "mv_wall",
    "nep_attA",
    "nep_attB",
    "nep_jump",
    "nep_nomu",
    "nep_tura",
    "nep_twis",
    "pakiA",
    "pakiB",
    "panel01",
    "panel02",
    "pillar",
    "poison",
    "power-on",
    "rck_brok",
    "rck_hitA",
    "rck_hitB",
    "rck_stop",
    "reb_scr",
    "shuter",
    "sl_click",
    "sl_crmov",
    "slback_c",
    "slide1",
    "slide2",
    "slide3",
    "slide_b2",
    "slide_bk",
    "sliding",
    "smash",
    "sp_Doku",
    "sp_atck",
    "sp_bomb",
    "sp_fumu",
    "sp_rakk",
    "sp_sanj2",
    "spray",
    "steam_a",
    "steam_b",
    "sw_btn",
    "sw_medal",
    "sw_medal2",
    "sw_push",
    "sw_push2",
    "sw_push3",
    "sw_trap",
    "tao_wall",
    "taore_ca",
    "taore_cp",
    "taore_pl",
    "taore_s1",
    "taore_sp",
    "taore_st",
    "taore_wa",
    "taore_wd",
    "taore_we",
    "type01",
    "type02",
    "undergat",
    "v00d_02",
    "walldown",
    "watergt",
    "yk_30a",
    "yk_405_",
    "z_Hkick",
    "z_Ugoron",
    "z_aoya01",
    "z_aoya02",
    "z_aoya03",
    "z_ftL",
    "z_ftR",
    "z_haki",
    "z_head",
    "z_isi01",
    "z_isi02",
    "z_isi03",
    "z_isi201",
    "z_isi202",
    "z_isi203",
    "z_isi301",
    "z_isi302",
    "z_isi303",
    "z_k01",
    "z_k02",
    "z_k03",
    "z_kamu",
    "z_mika01",
    "z_mika02",
    "z_mika03",
    "z_nisi1",
    "z_nisi2",
    "z_nisi3",
    "z_osou",
    "z_sanj",
    "z_simo01",
    "z_simo02",
    "z_simo03",
    "z_suzu01",
    "z_suzu02",
    "z_suzu03",
    "z_taore",
    "z_unaruA",
    "z_unaruB",
    "ze_ftL",
    "ze_haki",
    "ze_head",
    "ze_kamu",
    "ze_sanj",
    "ze_tomo1",
    "ze_tomo2",
    "ze_tomo3",
    "zep_ftL",
    "zuruzuru",
];

/// The eight 16-slot character SFX tables (`g_charactersSfxTable`), selected
/// by the player/character model id. Tables 2/3 and 6/7 share their rows.
static CHARACTER_HEADS: [[Option<&'static str>; 4]; 8] = [
    [
        Some("Chris01"),
        Some("Chris02"),
        Some("Chris03"),
        Some("Chris04"),
    ],
    [
        Some("Jill01"),
        Some("Jill02"),
        Some("Jill03"),
        Some("Jill04"),
    ],
    [Some("Reb01"), Some("Reb02"), Some("Reb03"), Some("Reb04")],
    [Some("Reb01"), Some("Reb02"), Some("Reb03"), Some("Reb04")],
    [
        Some("Ch_ef01"),
        Some("Ch_ef02"),
        Some("Ch_ef03"),
        Some("Ch_ef04"),
    ],
    [
        Some("Jill_ef01"),
        Some("Jill_ef02"),
        Some("Jill_ef03"),
        Some("Jill_ef04"),
    ],
    [
        Some("Reb_ef01"),
        Some("Reb_ef02"),
        Some("Reb_ef03"),
        Some("Reb_ef04"),
    ],
    [
        Some("Reb_ef01"),
        Some("Reb_ef02"),
        Some("Reb_ef03"),
        Some("Reb_ef04"),
    ],
];

/// Slots 4..=10 shared by every character table.
static CHARACTER_SHARED: [Option<&'static str>; 7] = [
    Some(UI_CURSOR),
    Some(UI_CANCEL),
    Some(UI_DECIDE),
    Some("Chris08"),
    Some("Chris10"),
    Some("Chris09"),
    Some("Mapled"),
];

/// Name in character table `table` (`0..=7`), slot `id` (`0..=15`).
pub fn character_sfx(table: u8, id: u8) -> Option<&'static str> {
    let head = CHARACTER_HEADS.get(usize::from(table))?;
    if id < 4 {
        return head[usize::from(id)];
    }
    CHARACTER_SHARED.get(usize::from(id) - 4).copied().flatten()
}

/// Name for entity/footstep sound column `index` in room row `row`.
///
/// `row` is `stage_index * 29 + room`, the same row layout the original uses.
/// `index` is the packed zone offset plus the entity sound type or a script's
/// bank-2 id. A slot the shipped data names resolves to its wav basename; a
/// slot the shipped data leaves empty resolves to `None`.
pub fn room_sound(row: usize, index: usize) -> Option<&'static str> {
    let row = u16::try_from(row).ok()?;
    let column = u8::try_from(index).ok()?;
    let found = ROOM_SOUNDS
        .binary_search_by_key(&(row, column), |(row, column, _)| (*row, *column))
        .ok()?;
    Some(ROOM_SOUNDS[found].2)
}

/// One resolved 3D SE request (`Play3DSnd`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sfx3dPlay {
    /// The wav basename to load and play, or `None` for a typed no-op.
    pub name: Option<&'static str>,
    /// Bank 4: pan and restart BGM channel 0 instead of playing a one-shot.
    pub bgm: bool,
    /// Linear mixer gain from [`sound_gain_pan`].
    pub gain: f32,
    /// Mixer pan in `-1..=1` from [`sound_gain_pan`].
    pub pan: f32,
    /// Raw DirectSound pan `(right - left) * 0x4E` (the BGM channel stores
    /// this form).
    pub raw_pan: i32,
}

impl Sfx3dPlay {
    /// A typed no-op (an unloaded bank, an out-of-range id or an absent table
    /// entry).
    fn noop() -> Self {
        Self {
            name: None,
            bgm: false,
            gain: 0.0,
            pan: 0.0,
            raw_pan: 0,
        }
    }
}

/// Resolve one 3D sound request through the five sound banks and compute its
/// mixer gain and pan (`Play3DSnd` + `Calc3DSndPan` + `CalcPanVolume`).
///
/// `room` names the bank-2 row and carries the live bank-0 SFX pair
/// ([`RoomState::room_sfx`], 0 at boot and reloaded from every door record),
/// `character` selects the bank-3 table and `from`/`to`/`sound` are the camera
/// and source positions. Bank 4 (`bgm`) asks the caller to pan and restart BGM
/// channel 0. Bank 1 is the weapon/menu bank, which no weapon ever loads in
/// this milestone, so it is always a typed no-op.
pub fn play_sfx_3d(
    room: &RoomState,
    character: u8,
    bank: u8,
    id: u8,
    from: [i32; 3],
    to: [i32; 3],
    sound: [i32; 3],
) -> Sfx3dPlay {
    match bank {
        0 => {
            if id > 1 {
                return Sfx3dPlay::noop();
            }
            match room_sfx(usize::from(room.room_sfx), usize::from(id)) {
                Some(name) => with_gain_pan(name, from, to, sound),
                None => Sfx3dPlay::noop(),
            }
        }
        // The weapon/menu bank: 12 shipped sites name it, but with no weapons
        // the bank is unloaded and only gains sounds once a weapon loads a
        // different progression (out of scope), so every request is a no-op.
        1 => Sfx3dPlay::noop(),
        2 => {
            if id > 0x2F {
                return Sfx3dPlay::noop();
            }
            match room_sound(room_row(room.stage, room.room), usize::from(id)) {
                Some(name) => with_gain_pan(name, from, to, sound),
                None => Sfx3dPlay::noop(),
            }
        }
        3 => {
            if id > 0x0F {
                return Sfx3dPlay::noop();
            }
            match character_sfx(character & 7, id) {
                Some(name) => with_gain_pan(name, from, to, sound),
                None => Sfx3dPlay::noop(),
            }
        }
        4 => {
            if id >= 0x30 {
                return Sfx3dPlay::noop();
            }
            // Bank 4 pans BGM channel 0 to the source and restarts it.
            let (left, right) = scene_pan(from, to, sound);
            Sfx3dPlay {
                name: None,
                bgm: true,
                gain: 0.0,
                pan: pan_position(left, right),
                raw_pan: (i32::from(right) - i32::from(left)) * 0x4E,
            }
        }
        _ => Sfx3dPlay::noop(),
    }
}

/// Compose a resolved name with the 3D gain/pan curves.
fn with_gain_pan(name: &'static str, from: [i32; 3], to: [i32; 3], sound: [i32; 3]) -> Sfx3dPlay {
    let (left, right) = scene_pan(from, to, sound);
    Sfx3dPlay {
        name: Some(name),
        bgm: false,
        gain: volume_gain(pan_volume(i32::from(left), i32::from(right))),
        pan: pan_position(left, right),
        raw_pan: (i32::from(right) - i32::from(left)) * 0x4E,
    }
}

/// Name in the room SE pair table: entry `index`, slot `slot`.
pub fn room_sfx(index: usize, slot: usize) -> Option<&'static str> {
    ROOM_SFX.get(index)?.get(slot).copied().flatten()
}

/// The room sound table row for a 1-based stage and room number.
pub fn room_row(stage: u8, room: u8) -> usize {
    usize::from(stage.saturating_sub(1)) * ROOMS_PER_STAGE + usize::from(room)
}

/// Entity sound column: the zone offset plus the sound type, reduced by three
/// when `slow` is set. Wraps like the original's byte arithmetic.
pub fn entity_sound_index(zone_low: u8, sound_type: u8, slow: bool) -> u8 {
    if slow {
        zone_low.wrapping_add(sound_type).wrapping_sub(3)
    } else {
        zone_low.wrapping_add(sound_type)
    }
}

/// Resolve the footstep sound for `pos` in `room`: look up the floor zone,
/// apply the entity sound type and name the resulting column.
pub fn footstep_sound(
    room: &RoomState,
    pos: [i32; 3],
    sound_type: u8,
    slow: bool,
) -> Option<&'static str> {
    let zone = room.footstep_zone(pos[0], pos[2])?;
    let index = entity_sound_index(zone as u8, sound_type, slow);
    room_sound(room_row(room.stage, room.room), usize::from(index))
}

/// 12-bit XZ angle from `(from_x, from_z)` to `(to_x, to_z)`.
///
/// The original's `CalculateAngleBetweenPointsXZ`: the difference is taken in
/// wrapping 16-bit arithmetic, the slope is `dz * 4096 / dx`, and the
/// resulting quadrant angle is negated modulo 0x1000.
pub fn angle_between_xz(from_x: i32, from_z: i32, to_x: i32, to_z: i32) -> u16 {
    let dx = (to_x as i16).wrapping_sub(from_x as i16);
    let dz = (to_z as i16).wrapping_sub(from_z as i16);
    if dx != 0 {
        let slope = (i32::from(dz) * 4096) / i32::from(dx);
        let angle = (f64::from(slope) / 4096.0).atan() * (2048.0 / std::f64::consts::PI);
        let quadrant = if dx < 0 { 0x800 } else { 0 };
        return ((-(quadrant + angle as i32)) as u32 & 0x0FFF) as u16;
    }
    ((if dz > 0 { 0x800 } else { 0 }) + 0x400) as u16
}

/// Integer square root with the original GTE routine's truncation.
fn integer_sqrt(value: i32) -> i32 {
    if value <= 0 {
        0
    } else {
        (f64::from(value)).sqrt() as i32
    }
}

/// The original's `Calc3DSndPan`: stereo pan bytes `(left, right)` for a sound
/// at `sound` heard from the camera at `from` looking at `to`.
///
/// Both bytes are in `0x1E..=0x7F`; equal bytes mean centred. Distance
/// attenuates both channels by `dist3D / 500` while a wider angle pulls the
/// near channel down towards `0x1E`.
pub fn scene_pan(from: [i32; 3], to: [i32; 3], sound: [i32; 3]) -> (u8, u8) {
    let dx = i32::from((from[0] as i16).wrapping_sub(sound[0] as i16));
    let dz = i32::from((from[2] as i16).wrapping_sub(sound[2] as i16));
    let horiz_dist = integer_sqrt(dx * dx + dz * dz);
    let dy = from[1] - sound[1];
    let dist3d = integer_sqrt(horiz_dist * horiz_dist + dy * dy);

    let angle_to_sound = angle_between_xz(from[0], from[2], sound[0], sound[2]);
    let angle_to_target = angle_between_xz(from[0], from[2], to[0], to[2]);
    let angle_diff = angle_to_sound.wrapping_sub(angle_to_target) & 0x0FFF;

    let mut left = 0x7Fu16;
    let mut right = 0x7Fu16;
    if angle_diff != 0 && angle_diff != 0x1000 && angle_diff != 0x800 {
        let is_right = angle_diff < 0x801;
        let mut abs_angle = if is_right {
            angle_diff
        } else {
            (!angle_diff) & 0x7FF
        };
        if abs_angle > 0x400 {
            abs_angle = !abs_angle;
        }
        if abs_angle & 0x7FF > 0x40 {
            let divisor = dist3d / 2000 + 0x18;
            let pan_offset = i32::from((abs_angle & 0x7FF) as i16) / divisor;
            let right_pan = (pan_offset + 0x7F).clamp(0, 0x7F);
            let mut left_pan = 0x7F - pan_offset;
            if left_pan < 0x1E {
                left_pan = 0x1E;
            }
            if is_right {
                right = right_pan as u16;
                left = left_pan as u16;
            } else {
                right = left_pan as u16;
                left = right_pan as u16;
            }
        }
    }

    // Attenuate both channels by distance, wrapping/truncating like the
    // original's unsigned-short arithmetic, then clamp and mask.
    let attenuation = (dist3d / -500) as u16;
    left = left.wrapping_add(attenuation);
    right = right.wrapping_add(attenuation);
    if left < 0x1E {
        left = 0x1E;
    }
    if right < 0x1E {
        right = 0x1E;
    }
    ((left & 0x7F) as u8, (right & 0x7F) as u8)
}

/// The original's `CalcPanVolume`: DirectSound attenuation in hundredths of a
/// decibel for a `(left, right)` pan pair. The average of the two channels is
/// scaled `* 18` with the `-0x8E4` bias; the low-average branch uses the
/// `* 0x103 - 10000` curve. The 3D pan bytes are unsigned; the `snd_pan_vol_set`
/// pair is sign-extended before it reaches here.
pub fn pan_volume(left: i32, right: i32) -> i32 {
    let avg = (left + right) / 2;
    if avg < 0x20 {
        avg * 0x103 - 10000
    } else {
        avg * 0x12 - 0x8E4
    }
}

/// Linear mixer amplitude for DirectSound millibels: `10^(vol / 2000)`.
pub fn volume_gain(millibels: i32) -> f32 {
    10f32.powf(millibels as f32 / 2000.0)
}

/// Mixer pan in `-1..=1` for a `(left, right)` pan pair.
///
/// The original feeds `(right - left) * 0x4E` to DirectSound, whose pan range
/// is `-10000..=10000`.
pub fn pan_position(left: u8, right: u8) -> f32 {
    ((i32::from(right) - i32::from(left)) as f32 * 0x4E as f32 / 10000.0).clamp(-1.0, 1.0)
}

/// Mixer pan in `-1..=1` for a raw DirectSound pan (`(right - left) * 0x4E`).
pub fn pan_from_raw(pan: i32) -> f32 {
    (pan as f32 / 10000.0).clamp(-1.0, 1.0)
}

/// The gain and stereo pan for a one-shot at `sound` heard from the camera
/// `from`/`to`, composed exactly like `Calc3DSndPan` + `CalcPanVolume`.
pub fn sound_gain_pan(from: [i32; 3], to: [i32; 3], sound: [i32; 3]) -> (f32, f32) {
    let (left, right) = scene_pan(from, to, sound);
    (
        volume_gain(pan_volume(i32::from(left), i32::from(right))),
        pan_position(left, right),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FootstepZone;

    fn room_1001() -> RoomState {
        RoomState {
            stage: 1,
            room: 0,
            footstep_zones: vec![FootstepZone {
                base_x: 900,
                base_z: 1100,
                width: 8700,
                height: 9800,
                sound_data: 45,
            }],
            ..RoomState::default()
        }
    }

    #[test]
    fn room_sound_reads_the_full_room_table() {
        assert_eq!(room_sound(0, 45), Some("ft_wdA"));
        assert_eq!(room_sound(0, 46), Some("ft_wdB"));
        assert_eq!(room_sound(0, 47), Some("taore_wd"));
        // The item/UI columns below the footstep window are named too.
        assert_eq!(room_sound(0, 23), Some("spray"));
        assert_eq!(room_sound(0, 25), Some("R_chris"));
        assert_eq!(room_sound(0, 30), Some("type01"));
        assert_eq!(room_sound(0, 35), None);
        assert_eq!(room_sound(0, 48), None);
        assert_eq!(room_sound(0, 0), None);
        // The enemy-AI columns the entity sounds index.
        assert_eq!(room_sound(5, 4), Some("z_mika02"));
        assert_eq!(room_sound(67, 7), Some("He_Nout"));
        assert_eq!(room_sound(180, 3), Some("HU_att"));
        assert_eq!(room_sound(1, 45), Some("ft_stwd"));
        assert_eq!(room_sound(2, 36), Some("drw_opwd"));
        assert_eq!(room_sound(5, 23), Some("emblemA"));
        assert_eq!(room_sound(5, 24), Some("magnum01"));
        assert_eq!(room_sound(5, 26), Some("ClkDX4LR"));
        assert_eq!(room_sound(5, 27), Some("mv_clk"));
        assert_eq!(room_sound(16, 45), None);
        assert_eq!(room_sound(25, 45), None);
        assert_eq!(room_sound(105, 45), None);
        assert_eq!(room_sound(203, 45), None);
        assert_eq!(room_sound(0, 255), None);
        assert_eq!(room_sound(usize::MAX, 45), None);
    }

    #[test]
    fn room_row_uses_the_original_layout() {
        assert_eq!(room_row(1, 0), 0);
        assert_eq!(room_row(1, 1), 1);
        assert_eq!(room_row(2, 0), 29);
        assert_eq!(room_row(7, 28), 202);
        assert_eq!(room_row(0, 0), 0);
    }

    #[test]
    fn room_sfx_pairs_are_present() {
        assert_eq!(room_sfx(0, 0), Some("Dr_wd01"));
        assert_eq!(room_sfx(0, 1), Some("Dr_wd02"));
        assert_eq!(room_sfx(2, 0), None);
        assert_eq!(room_sfx(2, 1), Some("Dr_brk01"));
        assert_eq!(room_sfx(5, 0), Some("St_wd01"));
        assert_eq!(room_sfx(5, 1), None);
        assert_eq!(room_sfx(14, 1), Some("Ev_ftn02"));
        assert_eq!(room_sfx(15, 0), None);
    }

    #[test]
    fn entity_sound_index_adds_type_and_slow_offset() {
        assert_eq!(entity_sound_index(45, 0, false), 45);
        assert_eq!(entity_sound_index(45, 1, false), 46);
        assert_eq!(entity_sound_index(45, 2, false), 47);
        assert_eq!(entity_sound_index(45, 0, true), 42);
        assert_eq!(entity_sound_index(3, 0, true), 0);
        assert_eq!(entity_sound_index(1, 0, true), 254);
    }

    #[test]
    fn footstep_sound_slow_flag_shifts_the_column() {
        // Stage 1 room 6's row has ft_stwp at column 45 and ft_cpA at column
        // 42: the slow flag's -3 offset lands on the concrete variant.
        let room = RoomState {
            stage: 1,
            room: 6,
            footstep_zones: vec![FootstepZone {
                base_x: 0,
                base_z: 0,
                width: 1000,
                height: 1000,
                sound_data: 45,
            }],
            ..RoomState::default()
        };
        assert_eq!(
            footstep_sound(&room, [10, 0, 10], 0, false),
            Some("ft_stwp"),
            "zone 45 names its own column"
        );
        assert_eq!(
            footstep_sound(&room, [10, 0, 10], 0, true),
            Some("ft_cpA"),
            "the slow flag drops the column to 42"
        );
    }

    #[test]
    fn footstep_sound_composes_zone_and_table() {
        let room = room_1001();
        assert_eq!(
            footstep_sound(&room, [4850, 0, 4950], 0, false),
            Some("ft_wdA")
        );
        assert_eq!(
            footstep_sound(&room, [4850, 0, 4950], 1, false),
            Some("ft_wdB")
        );
        // Slow drops the column to 42, which room 1001 leaves empty.
        assert_eq!(footstep_sound(&room, [4850, 0, 4950], 0, true), None);
        // Outside the zone, and a sound type outside the one-shot range.
        assert_eq!(footstep_sound(&room, [20000, 0, 4950], 0, false), None);
        assert_eq!(
            footstep_sound(&room, [4850, 0, 4950], MAX_ENTITY_SOUND_TYPE, false),
            None
        );
    }

    #[test]
    fn angle_between_points_matches_the_original_quadrants() {
        // Along +X is angle 0, -Z is 0x400, -X is 0x800, +Z is 0xC00
        // (increasing yaw turns towards -Z).
        assert_eq!(angle_between_xz(0, 0, 100, 0), 0x000);
        assert_eq!(angle_between_xz(0, 0, 0, -100), 0x400);
        assert_eq!(angle_between_xz(0, 0, -100, 0), 0x800);
        assert_eq!(angle_between_xz(0, 0, 0, 100), 0xC00);
    }

    #[test]
    fn scene_pan_centres_sounds_on_the_look_axis() {
        // A sound straight ahead behind the camera distance attenuates both
        // channels equally by dist/500 (1000 units -> -2).
        assert_eq!(scene_pan([0, 0, 0], [1000, 0, 0], [1000, 0, 0]), (125, 125));
        // At the camera itself the angle is degenerate (0x400) and the pan
        // lands on the original's fallback: the right channel is pulled down.
        assert_eq!(scene_pan([0, 0, 0], [1000, 0, 0], [0, 0, 0]), (85, 127));
    }

    #[test]
    fn scene_pan_offsets_a_wide_angle_and_wraps_distance() {
        // 90 degrees off the look axis at 2000 units: pan offset 40 with a
        // 4-unit distance attenuation.
        assert_eq!(scene_pan([0, 0, 0], [1000, 0, 0], [0, 0, 2000]), (123, 83));
        // Far away the angle offset is 26 and the distance attenuation 60:
        // (127 - 60, 101 - 60).
        let (left, right) = scene_pan([0, 0, 0], [1000, 0, 0], [0, 0, 30000]);
        assert_eq!((left, right), (67, 41));
    }

    #[test]
    fn pan_volume_and_gain_follow_the_millibel_curves() {
        // Centre at zero distance: avg 0x7F -> 0x7F * 18 - 0x8E4 = 10 mB.
        assert_eq!(pan_volume(0x7F, 0x7F), 10);
        assert!((volume_gain(10) - 1.0115795).abs() < 1e-6);
        // The low-average branch: avg 0x1F -> 0x1F * 0x103 - 10000.
        assert_eq!(pan_volume(0x1F, 0x1F), 0x1F * 0x103 - 10000);
        // The formula bottoms out at -10000 mB; the mixer's floor is silence.
        assert_eq!(volume_gain(-10000), 1e-5);
        assert_eq!(volume_gain(0), 1.0);
    }

    #[test]
    fn pan_position_scales_the_channel_difference() {
        assert_eq!(pan_position(0x7F, 0x7F), 0.0);
        // (right - left) * 0x4E / 10000 = 97 * 78 / 10000.
        assert!((pan_position(0x1E, 0x7F) - 0.7566).abs() < 1e-4);
        assert!((pan_position(0x7F, 0x1E) + 0.7566).abs() < 1e-4);
    }

    #[test]
    fn sound_gain_pan_composes_both_curves() {
        let (gain, pan) = sound_gain_pan([0, 0, 0], [1000, 0, 0], [1000, 0, 0]);
        assert!((gain - volume_gain(pan_volume(125, 125))).abs() < 1e-6);
        assert_eq!(pan, 0.0);
    }

    #[test]
    fn se_names_are_sorted_and_unique() {
        assert_eq!(SE_NAMES.len(), 389);
        assert!(SE_NAMES.windows(2).all(|pair| pair[0] < pair[1]));
        let lowered: std::collections::HashSet<String> = SE_NAMES
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        assert_eq!(
            lowered.len(),
            SE_NAMES.len(),
            "one lowercased pack entry per name"
        );
    }

    #[test]
    fn global_banks_expose_only_the_named_slots() {
        // The title/select/ending/bio/misc rows the non-combat screens use.
        assert_eq!(global_sfx(11, 0), Some("Bio01"));
        assert_eq!(global_sfx(12, 0), Some("Evil01"));
        assert_eq!(global_sfx(13, 0), Some("Select06"));
        assert_eq!(global_sfx(13, 1), Some("Select05"));
        assert_eq!(global_sfx(14, 0), Some("Ending07"));
        assert_eq!(global_sfx(14, 2), Some("Ending06"));
        assert_eq!(global_sfx(15, 7), Some("Win95_mg"));
        assert_eq!(global_sfx(15, 10), Some("A_mcn03"));
        for bank in [11, 12, 14] {
            assert_eq!(global_sfx(bank, 13), Some(UI_CANCEL), "bank {bank}");
            assert_eq!(global_sfx(bank, 14), Some("type01"), "bank {bank}");
            assert_eq!(global_sfx(bank, 15), Some("type02"), "bank {bank}");
        }
        // Silent slots and out-of-range banks/ids resolve to nothing.
        assert_eq!(global_sfx(12, 1), None);
        assert_eq!(global_sfx(13, 2), None);
        assert_eq!(global_sfx(15, 0), None);
        assert_eq!(global_sfx(10, 0), None);
        assert_eq!(global_sfx(16, 0), None);
        assert_eq!(global_sfx(12, 16), None);
    }

    #[test]
    fn character_tables_select_by_id_and_share_the_tail() {
        assert_eq!(character_sfx(0, 0), Some("Chris01"));
        assert_eq!(character_sfx(0, 3), Some("Chris04"));
        assert_eq!(character_sfx(1, 0), Some("Jill01"));
        assert_eq!(character_sfx(2, 1), Some("Reb02"));
        assert_eq!(character_sfx(3, 1), Some("Reb02"));
        assert_eq!(character_sfx(4, 0), Some("Ch_ef01"));
        assert_eq!(character_sfx(5, 3), Some("Jill_ef04"));
        assert_eq!(character_sfx(6, 0), Some("Reb_ef01"));
        assert_eq!(character_sfx(7, 0), Some("Reb_ef01"));
        for table in 0..8 {
            assert_eq!(character_sfx(table, 4), Some("cursor"));
            assert_eq!(character_sfx(table, 10), Some("Mapled"));
            assert_eq!(character_sfx(table, 11), None);
            assert_eq!(character_sfx(table, 15), None);
        }
        assert_eq!(character_sfx(8, 0), None, "there are eight tables");
    }

    #[test]
    fn room_sound_reads_the_enemy_and_script_columns() {
        // Scripted enemy cues reach the monster-AI columns.
        assert_eq!(room_sound(58, 0), Some("cer_foot"));
        assert_eq!(room_sound(61, 24), Some("Rancher"));
        assert_eq!(room_sound(62, 23), Some("call"));
        assert_eq!(room_sound(67, 23), Some("magnum"));
        assert_eq!(room_sound(180, 10), Some("Reb01"));
        // The door/item/UI columns the scripts address by hand.
        assert_eq!(room_sound(0, 23), Some("spray"));
        assert_eq!(room_sound(1, 34), Some("key_door"));
        assert_eq!(room_sound(120, 24), Some("panel02"));
        assert_eq!(room_sound(133, 23), Some("panel02"));
        assert_eq!(room_sound(194, 20), Some("D_gacha"));
        assert_eq!(room_sound(197, 22), Some("mv_stn"));
        assert_eq!(room_sound(197, 23), Some("panel02"));
        assert_eq!(room_sound(197, 24), Some("slide_b2"));
        // Empty slots and out-of-range rows/columns stay absent.
        assert_eq!(room_sound(58, 24), None);
        assert_eq!(room_sound(61, 6), None);
        assert_eq!(room_sound(203, 23), None);
        assert_eq!(room_sound(0, 47 + 1), None);
    }

    #[test]
    fn play_sfx_3d_resolves_each_bank() {
        let room = RoomState {
            stage: 1,
            room: 0x05,
            ..RoomState::default()
        };
        let camera = [0, 0, 0];
        let target = [1000, 0, 0];
        let sound = [1000, 0, 0];

        // Bank 0: the room SFX pair, slots 0/1 only.
        let play = play_sfx_3d(&room, 0, 0, 0, camera, target, sound);
        assert_eq!(play.name, Some("Dr_wd01"));
        assert_eq!(play.pan, 0.0, "straight ahead is centred");
        assert_eq!(play.gain, volume_gain(pan_volume(125, 125)));
        assert_eq!(
            play_sfx_3d(&room, 0, 0, 1, camera, target, sound).name,
            Some("Dr_wd02")
        );
        assert_eq!(
            play_sfx_3d(&room, 0, 0, 2, camera, target, sound).name,
            None
        );

        // Bank 1 is the unloaded weapon bank: typed no-op.
        assert_eq!(
            play_sfx_3d(&room, 0, 1, 7, camera, target, sound).name,
            None
        );

        // Bank 2: the room table; row 5's item columns resolve.
        assert_eq!(
            play_sfx_3d(&room, 0, 2, 23, camera, target, sound).name,
            Some("emblemA")
        );
        assert_eq!(
            play_sfx_3d(&room, 0, 2, 24, camera, target, sound).name,
            Some("magnum01")
        );
        assert_eq!(
            play_sfx_3d(&room, 0, 2, 26, camera, target, sound).name,
            Some("ClkDX4LR")
        );
        assert_eq!(
            play_sfx_3d(&room, 0, 2, 27, camera, target, sound).name,
            Some("mv_clk")
        );
        // An absent column and an out-of-range id stay typed no-ops.
        assert_eq!(
            play_sfx_3d(&room, 0, 2, 25, camera, target, sound).name,
            None
        );
        assert_eq!(
            play_sfx_3d(&room, 0, 2, 0x30, camera, target, sound).name,
            None
        );
        let room_panel = RoomState {
            stage: 5,
            room: 4,
            ..RoomState::default()
        };
        assert_eq!(
            play_sfx_3d(&room_panel, 0, 2, 24, camera, target, sound).name,
            Some("panel02")
        );

        // Bank 3: the selected character table.
        let chris = play_sfx_3d(&room, 0, 3, 2, camera, target, sound);
        assert_eq!(chris.name, Some("Chris03"));
        let jill = play_sfx_3d(&room, 1, 3, 2, camera, target, sound);
        assert_eq!(jill.name, Some("Jill03"));
        assert_eq!(
            play_sfx_3d(&room, 0, 3, 16, camera, target, sound).name,
            None
        );

        // Bank 4: BGM pan, no one-shot.
        let play = play_sfx_3d(&room, 0, 4, 23, camera, target, sound);
        assert!(play.bgm && play.name.is_none());
        assert_eq!(play.raw_pan, 0);
        assert!(!play_sfx_3d(&room, 0, 4, 0x30, camera, target, sound).bgm);

        // Unknown banks are typed no-ops.
        assert_eq!(
            play_sfx_3d(&room, 0, 9, 0, camera, target, sound).name,
            None
        );
    }

    #[test]
    fn play_sfx_3d_bank_zero_uses_the_live_room_sfx_pair() {
        let mut room = RoomState {
            stage: 1,
            room: 0x05,
            ..RoomState::default()
        };
        let camera = [0, 0, 0];
        let target = [1000, 0, 0];
        let sound = [1000, 0, 0];
        // At boot the pair index is 0: the wooden-door pair.
        assert_eq!(room.room_sfx, 0);
        assert_eq!(
            play_sfx_3d(&room, 0, 0, 0, camera, target, sound).name,
            Some("Dr_wd01")
        );
        // A door record with sfx 1 loads the metal-door pair.
        room.room_sfx = 1;
        assert_eq!(
            play_sfx_3d(&room, 0, 0, 0, camera, target, sound).name,
            Some("Dr_mtl01")
        );
        assert_eq!(
            play_sfx_3d(&room, 0, 0, 1, camera, target, sound).name,
            Some("Dr_mtl02")
        );
        // An out-of-range pair index (the original's unchecked table read)
        // resolves to a typed no-op instead.
        room.room_sfx = 15;
        assert_eq!(
            play_sfx_3d(&room, 0, 0, 0, camera, target, sound).name,
            None
        );
    }

    #[test]
    fn play_sfx_3d_follows_the_3d_pan_curves() {
        let room = RoomState::default();
        let (gain, pan) = sound_gain_pan([0, 0, 0], [1000, 0, 0], [0, 0, 2000]);
        let play = play_sfx_3d(&room, 0, 0, 0, [0, 0, 0], [1000, 0, 0], [0, 0, 2000]);
        assert_eq!(play.name, Some("Dr_wd01"));
        assert!((play.gain - gain).abs() < 1e-6);
        assert!((play.pan - pan).abs() < 1e-6);
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn real_room_1001_zone_resolves_to_ft_wda() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let path = std::path::Path::new(&root).join("JPN/STAGE1/ROOM1001.RDT");
        let data = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let room = crate::rdt::parse(&data, crate::state::RoomId::parse("1001").unwrap()).unwrap();

        assert_eq!(room.footstep_zones.len(), 1);
        assert_eq!(
            footstep_sound(&room, [4850, 0, 4950], 0, false),
            Some("ft_wdA")
        );
    }
}
