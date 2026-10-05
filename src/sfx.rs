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
#[rustfmt::skip]
pub static SE_NAMES: [&str; 115] = [
    "A_mcn03",
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
    "D_gacha",
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
    "Rancher",
    "Reb01",
    "Reb02",
    "Reb03",
    "Reb04",
    "Reb_ef01",
    "Reb_ef02",
    "Reb_ef03",
    "Reb_ef04",
    "Select05",
    "Select06",
    "St_mtl01",
    "St_wcp01",
    "St_wd01",
    "Win95_mg",
    "call",
    "cancel",
    "cursor",
    "decide",
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
    "mv_step",
    "panel02",
    "slide_b2",
    "taore_ca",
    "taore_cp",
    "taore_pl",
    "taore_sp",
    "taore_st",
    "taore_wd",
    "type01",
    "type02",
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

/// Sparse room-table names outside the 12-column footstep window, keyed by
/// `row * 48 + column`.
///
/// Only the entries the shipped script corpus reaches are transcribed; the
/// monster-AI columns stay absent and the corpus audit asserts nothing else is
/// needed. `mv_step` is packed but the corpus never addresses its column.
static EXTRA_ROOM_SOUNDS: &[(u16, &str)] = &[
    // call: the guardhouse crows (rooms 3000/3040).
    (58 * 48 + 23, "call"),
    (62 * 48 + 23, "call"),
    // Rancher: room 3030's scripted guard.
    (61 * 48 + 24, "Rancher"),
    // panel02: the lab's shutter panels (5040/5110) and room 7170.
    (120 * 48 + 24, "panel02"),
    (133 * 48 + 23, "panel02"),
    (197 * 48 + 23, "panel02"),
    // slide_b2: room 7170.
    (197 * 48 + 24, "slide_b2"),
    // D_gacha: room 7140.
    (194 * 48 + 20, "D_gacha"),
];

/// Name for entity/footstep sound column `index` in room row `row`.
///
/// `row` is `stage_index * 29 + room`, the same row layout the original uses.
/// `index` is the packed zone offset plus the entity sound type or a script's
/// bank-2 id; columns below [`FIRST_COLUMN`] resolve through the sparse
/// [`EXTRA_ROOM_SOUNDS`] entries, and rows without shipped names to `None`.
pub fn room_sound(row: usize, index: usize) -> Option<&'static str> {
    if let Some(column) = index.checked_sub(FIRST_COLUMN)
        && let Some(name) = ROOM_SOUNDS.get(row)?.get(column).copied().flatten()
    {
        return Some(name);
    }
    let key = (row * 48 + index) as u16;
    EXTRA_ROOM_SOUNDS
        .iter()
        .find(|(entry, _)| *entry == key)
        .map(|(_, name)| *name)
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
/// `room` names the bank-2 row, `character` selects the bank-3 table and
/// `from`/`to`/`sound` are the camera and source positions. Bank 4 (`bgm`)
/// asks the caller to pan and restart BGM channel 0. Bank 1 is the
/// weapon/menu bank, which no weapon ever loads in this milestone, so it is
/// always a typed no-op.
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
            match room_sfx(0, usize::from(id)) {
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
        gain: volume_gain(pan_volume(left, right)),
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
/// `* 0x103 - 10000` curve.
pub fn pan_volume(left: u8, right: u8) -> i32 {
    let avg = (i32::from(left) + i32::from(right)) / 2;
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
        volume_gain(pan_volume(left, right)),
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
        assert_eq!(SE_NAMES.len(), 115);
        assert!(SE_NAMES.windows(2).all(|pair| pair[0] < pair[1]));
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
    fn room_sound_reads_the_sparse_bank_2_extras() {
        // The corpus-reached entries outside the 36..=47 window.
        assert_eq!(room_sound(58, 23), Some("call"));
        assert_eq!(room_sound(62, 23), Some("call"));
        assert_eq!(room_sound(61, 24), Some("Rancher"));
        assert_eq!(room_sound(120, 24), Some("panel02"));
        assert_eq!(room_sound(133, 23), Some("panel02"));
        assert_eq!(room_sound(197, 23), Some("panel02"));
        assert_eq!(room_sound(197, 24), Some("slide_b2"));
        assert_eq!(room_sound(194, 20), Some("D_gacha"));
        // The monster-AI columns stay absent.
        assert_eq!(room_sound(0, 3), None);
        assert_eq!(room_sound(67, 7), None);
        assert_eq!(room_sound(180, 3), None);
        assert_eq!(room_sound(58, 24), None);
        assert_eq!(room_sound(203, 23), None);
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

        // Bank 2: the room table; row 5's corpus props are absent, but the
        // sparse panel02 row exists.
        assert_eq!(
            play_sfx_3d(&room, 0, 2, 23, camera, target, sound).name,
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
