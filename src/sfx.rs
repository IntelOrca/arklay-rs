//! Per-room sound name tables and footstep resolution.
//!
//! The shipped rooms name their entity and footstep sounds in a table shaped
//! as `stage * 29 + room` rows of 48 slots. Only slots 36..=47 ever hold a
//! name in the shipped data, so [`room_sound`] resolves that window and treats
//! every other slot as silent.
//!
//! A footstep zone (parsed from the RDT's `.flr` table) packs a surface type
//! in the high byte of its lookup result and a sound offset in the low byte.
//! The offset plus the entity sound type names the column inside the room row.

use crate::state::RoomState;

/// Rooms per stage in the room sound table.
const ROOMS_PER_STAGE: usize = 29;
/// First sound table column with shipped names.
pub const FIRST_COLUMN: usize = 36;
/// Number of sound table columns with shipped names.
pub const COLUMN_COUNT: usize = 12;
/// One-shot entity sound types are 0, 1 and 2.
pub const MAX_ENTITY_SOUND_TYPE: u8 = 3;

/// Empty table slot.
const N: Option<&'static str> = None;

/// A row without any shipped name in columns 36..=47.
#[rustfmt::skip]
static NO_SOUND: [Option<&'static str>; COLUMN_COUNT] = [N; COLUMN_COUNT];

/// Per-room entity sound names for columns 36..=47, indexed by
/// `stage_index * 29 + room`. `N` is an empty slot.
#[rustfmt::skip]
static ROOM_SOUNDS: [&[Option<&'static str>]; 203] = [
    //   0: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //   1: 
    &[N, N, N, N, N, N, Some("ft_floA"), Some("ft_floB"), Some("taore_wd"), Some("ft_stwd"), N, N],
    //   2: 
    &[Some("drw_opwd"), Some("drw_shwd"), Some("key_desk"), N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //   3: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //   4: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //   5: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //   6: 
    &[N, N, N, Some("ft_haA"), Some("ft_haB"), Some("taore_st"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_stwp"), N, N],
    //   7: 
    &[N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //   8: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //   9: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  10: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  11: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_stwd"), N, N],
    //  12: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    //  13: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    //  14: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //  15: 
    &[N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  16: no sounds
    &NO_SOUND,
    //  17: 
    &[Some("drw_opwd"), Some("drw_shwd"), Some("key_desk"), N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    //  18: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    //  19: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //  20: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //  21: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    //  22: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  23: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  24: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  25: no sounds
    &NO_SOUND,
    //  26: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //  27: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //  28: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    //  29: no sounds
    &NO_SOUND,
    //  30: 
    &[N, N, N, N, N, N, Some("ft_floA"), Some("ft_floB"), Some("taore_wd"), Some("ft_stwd"), N, N],
    //  31: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //  32: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_stwp"), N, N],
    //  33: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //  34: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    //  35: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //  36: 
    &[N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_stwd"), N, N],
    //  37: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //  38: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  39: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //  40: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  41: no sounds
    &NO_SOUND,
    //  42: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    //  43: 
    &[N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st"), Some("ft_stwd"), N, N],
    //  44: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //  45: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  46: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    //  47: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  48: no sounds
    &NO_SOUND,
    //  49: no sounds
    &NO_SOUND,
    //  50: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  51: no sounds
    &NO_SOUND,
    //  52: no sounds
    &NO_SOUND,
    //  53: no sounds
    &NO_SOUND,
    //  54: no sounds
    &NO_SOUND,
    //  55: no sounds
    &NO_SOUND,
    //  56: no sounds
    &NO_SOUND,
    //  57: no sounds
    &NO_SOUND,
    //  58: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //  59: 
    &[N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st"), Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //  60: 
    &[N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st"), Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //  61: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    //  62: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //  63: 
    &[N, N, N, N, N, N, Some("ft_sts"), N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //  64: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_caveA"), Some("ft_caveB"), Some("taore_ca")],
    //  65: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_caveA"), Some("ft_caveB"), Some("taore_ca")],
    //  66: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_caveA"), Some("ft_caveB"), Some("taore_ca")],
    //  67: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_caveA"), Some("ft_caveB"), Some("taore_ca")],
    //  68: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_caveA"), Some("ft_caveB"), Some("taore_ca")],
    //  69: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_caveA"), Some("ft_caveB"), Some("taore_ca")],
    //  70: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_spA"), Some("ft_spB"), Some("taore_sp")],
    //  71: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_caveA"), Some("ft_caveB"), Some("taore_ca")],
    //  72: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    //  73: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_caveA"), Some("ft_caveB"), Some("taore_ca")],
    //  74: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_plaA"), Some("ft_plaB"), Some("taore_pl")],
    //  75: no sounds
    &NO_SOUND,
    //  76: no sounds
    &NO_SOUND,
    //  77: no sounds
    &NO_SOUND,
    //  78: no sounds
    &NO_SOUND,
    //  79: no sounds
    &NO_SOUND,
    //  80: no sounds
    &NO_SOUND,
    //  81: no sounds
    &NO_SOUND,
    //  82: no sounds
    &NO_SOUND,
    //  83: no sounds
    &NO_SOUND,
    //  84: no sounds
    &NO_SOUND,
    //  85: no sounds
    &NO_SOUND,
    //  86: no sounds
    &NO_SOUND,
    //  87: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  88: 
    &[Some("drw_opwd"), Some("drw_shwd"), Some("key_desk"), N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  89: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  90: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  91: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  92: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  93: 
    &[Some("drw_opwd"), Some("drw_shwd"), Some("key_desk"), N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  94: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  95: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  96: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  97: 
    &[Some("drw_opwd"), Some("drw_shwd"), Some("key_desk"), N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  98: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    //  99: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 100: 
    &[N, N, N, Some("ft_kibA"), Some("ft_kibB"), Some("taore_st"), Some("ft_swimA"), Some("ft_swimB"), Some("taore_st"), Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 101: 
    &[N, N, N, N, N, N, Some("ft_swimA"), Some("ft_swimB"), Some("taore_st"), Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 102: 
    &[N, N, N, N, N, N, Some("ft_swimA"), Some("ft_swimB"), Some("taore_st"), Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 103: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 104: 
    &[N, N, N, N, N, N, Some("ft_swimA"), Some("ft_swimB"), Some("taore_st"), Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 105: no sounds
    &NO_SOUND,
    // 106: no sounds
    &NO_SOUND,
    // 107: no sounds
    &NO_SOUND,
    // 108: no sounds
    &NO_SOUND,
    // 109: no sounds
    &NO_SOUND,
    // 110: no sounds
    &NO_SOUND,
    // 111: no sounds
    &NO_SOUND,
    // 112: no sounds
    &NO_SOUND,
    // 113: no sounds
    &NO_SOUND,
    // 114: no sounds
    &NO_SOUND,
    // 115: no sounds
    &NO_SOUND,
    // 116: 
    &[N, N, N, N, N, N, Some("ft_mtnA"), Some("ft_mtnB"), Some("taore_pl"), Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 117: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 118: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 119: 
    &[N, N, N, Some("ft_mtnA"), Some("ft_mtnB"), Some("taore_pl"), Some("ft_concA"), Some("ft_concB"), Some("taore_st"), Some("ft_stmt"), N, N],
    // 120: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 121: 
    &[N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st"), Some("ft_stmt"), N, N],
    // 122: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 123: 
    &[Some("drw_c_op"), Some("drw_c_sh"), Some("key_desk"), N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 124: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 125: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 126: 
    &[Some("drw_opmt"), Some("drw_shmt"), Some("key_desk"), N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 127: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 128: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 129: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_plaA"), Some("ft_plaB"), Some("taore_pl")],
    // 130: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 131: 
    &[N, N, N, Some("ft_mtnA"), Some("ft_mtnB"), Some("taore_pl"), Some("ft_plaA"), Some("ft_plaB"), Some("taore_pl"), Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 132: 
    &[N, N, N, Some("ft_mtnA"), Some("ft_mtnB"), Some("taore_pl"), Some("ft_plaA"), Some("ft_plaB"), Some("taore_pl"), Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 133: 
    &[N, N, N, Some("ft_mtnA"), Some("ft_mtnB"), Some("taore_pl"), Some("ft_plaA"), Some("ft_plaB"), Some("taore_pl"), Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 134: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 135: 
    &[N, N, N, N, N, N, Some("ft_mtnA"), Some("ft_mtnB"), Some("taore_pl"), Some("ft_plaA"), Some("ft_plaB"), Some("taore_pl")],
    // 136: 
    &[N, N, N, N, N, N, Some("ft_mtnA"), Some("ft_mtnB"), Some("taore_pl"), Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
    // 137: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_plaA"), Some("ft_plaB"), Some("taore_pl")],
    // 138: no sounds
    &NO_SOUND,
    // 139: no sounds
    &NO_SOUND,
    // 140: no sounds
    &NO_SOUND,
    // 141: no sounds
    &NO_SOUND,
    // 142: no sounds
    &NO_SOUND,
    // 143: no sounds
    &NO_SOUND,
    // 144: no sounds
    &NO_SOUND,
    // 145: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 146: 
    &[N, N, N, N, N, N, Some("ft_floA"), Some("ft_floB"), Some("taore_wd"), Some("ft_stwd"), N, N],
    // 147: 
    &[Some("drw_opwd"), Some("drw_shwd"), Some("key_desk"), N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 148: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 149: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 150: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    // 151: 
    &[N, N, N, Some("ft_haA"), Some("ft_haB"), Some("taore_st"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_stwp"), N, N],
    // 152: 
    &[N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 153: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 154: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 155: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 156: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_stwd"), N, N],
    // 157: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 158: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 159: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 160: 
    &[N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 161: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_stwd"), N, N],
    // 162: 
    &[Some("drw_opwd"), Some("drw_shwd"), Some("key_desk"), N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 163: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 164: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 165: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 166: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 167: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 168: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 169: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 170: 
    &[Some("drw_opwd"), Some("drw_shwd"), Some("key_desk"), N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 171: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    // 172: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_rmA"), Some("ft_rmB"), Some("taore_st")],
    // 173: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 174: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 175: 
    &[N, N, N, N, N, N, Some("ft_floA"), Some("ft_floB"), Some("taore_wd"), Some("ft_stwd"), N, N],
    // 176: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 177: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_stwp"), N, N],
    // 178: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 179: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 180: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 181: 
    &[N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_stwd"), N, N],
    // 182: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 183: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 184: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 185: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 186: 
    &[N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st"), Some("ft_caveA"), Some("ft_caveB"), Some("taore_ca")],
    // 187: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 188: 
    &[N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st"), Some("ft_stwd"), N, N],
    // 189: 
    &[N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd"), Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 190: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 191: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp")],
    // 192: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 193: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 194: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 195: 
    &[N, N, N, N, N, N, Some("ft_cpA"), Some("ft_cpB"), Some("taore_cp"), Some("ft_wdA"), Some("ft_wdB"), Some("taore_wd")],
    // 196: 
    &[Some("drw_opwd"), Some("drw_shwd"), Some("key_desk"), N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 197: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 198: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_linA"), Some("ft_linB"), Some("taore_st")],
    // 199: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_floA"), Some("ft_floB"), Some("taore_wd")],
    // 200: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_coefA"), Some("ft_coefB"), Some("taore_st")],
    // 201: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_coefA"), Some("ft_coefB"), Some("taore_st")],
    // 202: 
    &[N, N, N, N, N, N, N, N, N, Some("ft_concA"), Some("ft_concB"), Some("taore_st")],
];

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

/// Every sound effect name shipped in the pack, sorted.
#[rustfmt::skip]
pub static SE_NAMES: [&str; 68] = [
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
    "Ev_ftn01",
    "Ev_ftn02",
    "Ev_lab01",
    "Ev_mv",
    "Ev_new01",
    "Ev_new02",
    "Ev_old01",
    "Ev_old02",
    "Ladder01",
    "St_mtl01",
    "St_wcp01",
    "St_wd01",
    "drw_c_op",
    "drw_c_sh",
    "drw_opmt",
    "drw_opwd",
    "drw_shmt",
    "drw_shwd",
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
    "ft_stmt",
    "ft_sts",
    "ft_stwd",
    "ft_stwp",
    "ft_swimA",
    "ft_swimB",
    "ft_wdA",
    "ft_wdB",
    "key_desk",
    "taore_ca",
    "taore_cp",
    "taore_pl",
    "taore_sp",
    "taore_st",
    "taore_wd",
];

/// Name for entity/footstep sound column `index` in room row `row`.
///
/// `row` is `stage_index * 29 + room`, the same row layout the original uses.
/// `index` is the packed zone offset plus the entity sound type; columns below
/// [`FIRST_COLUMN`] and rows without shipped names resolve to `None`.
pub fn room_sound(row: usize, index: usize) -> Option<&'static str> {
    let column = index.checked_sub(FIRST_COLUMN)?;
    ROOM_SOUNDS.get(row)?.get(column).copied().flatten()
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
    fn room_sound_reads_only_the_named_window() {
        assert_eq!(room_sound(0, 45), Some("ft_wdA"));
        assert_eq!(room_sound(0, 46), Some("ft_wdB"));
        assert_eq!(room_sound(0, 47), Some("taore_wd"));
        assert_eq!(room_sound(0, 35), None);
        assert_eq!(room_sound(0, 48), None);
        assert_eq!(room_sound(0, 0), None);
        assert_eq!(room_sound(1, 45), Some("ft_stwd"));
        assert_eq!(room_sound(2, 36), Some("drw_opwd"));
        assert_eq!(room_sound(16, 45), None);
        assert_eq!(room_sound(25, 45), None);
        assert_eq!(room_sound(203, 45), None);
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
    fn se_names_are_sorted_and_unique() {
        assert_eq!(SE_NAMES.len(), 68);
        assert!(SE_NAMES.windows(2).all(|pair| pair[0] < pair[1]));
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
