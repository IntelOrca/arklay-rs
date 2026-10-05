//! Room background-music tables and pack path naming.
//!
//! The shipped game selects music from three per-room tables plus a per-room
//! state byte that remembers which track index was last chosen. The tables are
//! hardcoded here for now.

use crate::state::RoomId;

// TODO: this room BGM data should move to a game init script that lives in the
// game pak; the SCD will set/pan/volume/start/stop tracks.

/// Per-group wav basenames, one per playback slot. `None` is a null entry.
pub const GROUP_TRACKS: [[Option<&str>; 4]; 57] = [
    [Some("Bgm_00"), Some("Se_01"), Some("Bgm_05"), None], // 00
    [Some("Bgm_02"), None, None, None],                    // 01
    [Some("Se_03"), Some("Bgm_29"), None, None],           // 02
    [Some("Bgm_04"), Some("chain1"), None, None],          // 03
    [Some("Se_06"), None, None, None],                     // 04
    [Some("Bgm_07"), None, None, None],                    // 05
    [Some("Bgm_08"), None, None, None],                    // 06
    [Some("Bgm_09"), None, None, None],                    // 07
    [Some("Bgm_0a"), Some("Bgm_0b"), Some("Se_3c"), None], // 08
    [None, None, None, None],                              // 09
    [Some("Bgm_40"), Some("Bgm_0c"), None, None],          // 0A
    [Some("Bgm_13"), None, None, None],                    // 0B
    [Some("Bgm_0e"), None, None, None],                    // 0C
    [Some("Bgm_10"), Some("Bgm_11"), Some("Bgm_12"), None], // 0D
    [Some("Bgm_1f"), Some("Bgm_2b"), None, None],          // 0E
    [Some("Bgm_31"), None, None, None],                    // 0F
    [Some("Bgm_14"), None, None, None],                    // 10
    [Some("Bgm_34"), None, None, None],                    // 11
    [Some("Bgm_36"), Some("Bgm_08"), None, None],          // 12
    [Some("Bgm_2c"), Some("Bgm_3e"), None, None],          // 13
    [Some("Bgm_26"), None, None, None],                    // 14
    [Some("Bgm_26"), None, None, None],                    // 15
    [Some("Bgm_0f"), None, None, None],                    // 16
    [Some("Bgm_15"), None, None, None],                    // 17
    [Some("Bgm_18"), None, Some("Bgm_05"), None],          // 18
    [Some("Bgm_19"), None, None, None],                    // 19
    [Some("Bgm_1a"), None, None, None],                    // 1A
    [Some("Bgm_3d"), Some("Bgm_3f"), None, None],          // 1B
    [Some("Se_41"), None, None, None],                     // 1C
    [None, None, None, None],                              // 1D
    [Some("Se_4c"), Some("Se_4d"), None, None],            // 1E
    [Some("Se_4e"), None, None, None],                     // 1F
    [Some("Se_44"), Some("Bgm_37"), Some("Bgm_38"), None], // 20
    [Some("Bgm_1b"), Some("Se_45"), None, None],           // 21
    [Some("Se_44"), Some("Se_45"), Some("Bgm_1c"), None],  // 22
    [Some("Bgm_23"), None, None, None],                    // 23
    [Some("Se_03"), Some("Se_42"), Some("Se_43"), None],   // 24
    [Some("Se_46"), Some("Se_39lp"), Some("Bgm_25"), None], // 25
    [Some("Se_44"), Some("Bgm_3a"), Some("V110_00"), None], // 26
    [Some("Bgm_23"), None, None, None],                    // 27
    [Some("Bgm_28"), Some("Bgm_57"), None, None],          // 28
    [Some("Bgm_17"), Some("Se_4b"), None, None],           // 29
    [None, None, None, None],                              // 2A
    [Some("Bgm_16"), Some("Bgm_48"), None, None],          // 2B
    [Some("Bgm_33"), None, None, None],                    // 2C
    [Some("Bgm_49"), None, None, None],                    // 2D
    [Some("Bgm_4a"), Some("Bgm_32"), None, None],          // 2E
    [Some("Bgm_1e"), Some("Se_4e"), None, None],           // 2F
    [Some("Bgm_0e"), Some("Bgm_56"), None, None],          // 30
    [Some("Se_53"), Some("Se_54"), None, None],            // 31
    [Some("Bgm_2e"), Some("Bgm_2f"), None, None],          // 32
    [Some("Bgm_2e"), Some("Bgm_3b"), None, None],          // 33
    [Some("Se_55"), Some("Bgm_20"), Some("Bgm_24"), None], // 34
    [Some("Bgm_30"), Some("Se_50"), Some("Se_51"), None],  // 35
    [Some("Bgm_1d"), Some("Bgm_2d"), Some("Se_4f"), None], // 36
    [Some("Se_59"), Some("Se_5a"), None, None],            // 37
    [None, None, None, None],                              // 38
];

/// Per-group, per-slot whole-buffer loop flags.
pub const GROUP_LOOPS: [[bool; 4]; 57] = [
    [true, true, true, false],    // 00
    [true, false, false, false],  // 01
    [true, false, false, false],  // 02
    [true, true, false, false],   // 03
    [true, false, false, false],  // 04
    [true, false, false, false],  // 05
    [true, false, false, false],  // 06
    [true, false, false, false],  // 07
    [true, false, true, false],   // 08
    [false, false, false, false], // 09
    [false, false, false, false], // 0A
    [true, false, false, false],  // 0B
    [true, false, false, false],  // 0C
    [false, true, false, false],  // 0D
    [true, false, false, false],  // 0E
    [true, false, false, false],  // 0F
    [false, false, false, false], // 10
    [false, false, false, false], // 11
    [false, true, false, false],  // 12
    [false, true, false, false],  // 13
    [false, false, false, false], // 14
    [false, false, false, false], // 15
    [false, false, false, false], // 16
    [false, false, false, false], // 17
    [true, false, true, false],   // 18
    [true, false, false, false],  // 19
    [true, false, false, false],  // 1A
    [true, false, false, false],  // 1B
    [false, false, false, false], // 1C
    [false, false, false, false], // 1D
    [true, true, false, false],   // 1E
    [true, false, false, false],  // 1F
    [true, true, false, false],   // 20
    [true, true, false, false],   // 21
    [true, true, true, false],    // 22
    [true, false, false, false],  // 23
    [true, true, false, false],   // 24
    [true, true, true, false],    // 25
    [true, false, false, false],  // 26
    [true, false, false, false],  // 27
    [true, true, false, false],   // 28
    [true, false, false, false],  // 29
    [false, false, false, false], // 2A
    [true, false, false, false],  // 2B
    [true, false, false, false],  // 2C
    [true, false, false, false],  // 2D
    [false, true, false, false],  // 2E
    [true, true, false, false],   // 2F
    [true, true, false, false],   // 30
    [true, true, false, false],   // 31
    [true, false, false, false],  // 32
    [true, false, false, false],  // 33
    [true, true, true, false],    // 34
    [true, true, true, false],    // 35
    [true, false, false, false],  // 36
    [false, false, false, false], // 37
    [false, false, false, false], // 38
];

/// Up to four BGM group ids per room, indexed by stage (0-based), room, and
/// `state & 7`. `0xFF` means no music.
pub const ROOM_GROUPS: [[[u8; 4]; 32]; 7] = [
    // stage 0
    [
        [0x0B, 0x0F, 0xFF, 0xFF], // room 00
        [0xFF, 0xFF, 0xFF, 0xFF], // room 01
        [0xFF, 0xFF, 0xFF, 0xFF], // room 02
        [0x00, 0xFF, 0xFF, 0xFF], // room 03
        [0x00, 0x0D, 0x0E, 0xFF], // room 04
        [0x00, 0xFF, 0xFF, 0xFF], // room 05
        [0x00, 0x01, 0x09, 0xFF], // room 06
        [0x00, 0xFF, 0xFF, 0xFF], // room 07
        [0x00, 0xFF, 0xFF, 0xFF], // room 08
        [0xFF, 0xFF, 0xFF, 0xFF], // room 09
        [0xFF, 0xFF, 0xFF, 0xFF], // room 0A
        [0x00, 0xFF, 0xFF, 0xFF], // room 0B
        [0x1E, 0xFF, 0xFF, 0xFF], // room 0C
        [0x1C, 0xFF, 0xFF, 0xFF], // room 0D
        [0x00, 0xFF, 0xFF, 0xFF], // room 0E
        [0x0D, 0x0E, 0xFF, 0xFF], // room 0F
        [0xFF, 0xFF, 0xFF, 0xFF], // room 10
        [0xFF, 0xFF, 0xFF, 0xFF], // room 11
        [0xFF, 0xFF, 0xFF, 0xFF], // room 12
        [0xFF, 0xFF, 0xFF, 0xFF], // room 13
        [0x02, 0xFF, 0xFF, 0xFF], // room 14
        [0x03, 0xFF, 0xFF, 0xFF], // room 15
        [0xFF, 0xFF, 0xFF, 0xFF], // room 16
        [0x05, 0xFF, 0xFF, 0xFF], // room 17
        [0xFF, 0xFF, 0xFF, 0xFF], // room 18
        [0x02, 0xFF, 0xFF, 0xFF], // room 19
        [0x02, 0xFF, 0xFF, 0xFF], // room 1A
        [0x02, 0xFF, 0xFF, 0xFF], // room 1B
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1C
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1D
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1E
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1F
    ],
    // stage 1
    [
        [0x37, 0xFF, 0xFF, 0xFF], // room 00
        [0x07, 0xFF, 0xFF, 0xFF], // room 01
        [0x07, 0xFF, 0xFF, 0xFF], // room 02
        [0x09, 0xFF, 0xFF, 0xFF], // room 03
        [0x07, 0xFF, 0xFF, 0xFF], // room 04
        [0x1F, 0xFF, 0xFF, 0xFF], // room 05
        [0xFF, 0xFF, 0xFF, 0xFF], // room 06
        [0xFF, 0xFF, 0xFF, 0xFF], // room 07
        [0x07, 0xFF, 0xFF, 0xFF], // room 08
        [0x07, 0xFF, 0xFF, 0xFF], // room 09
        [0x11, 0x07, 0xFF, 0xFF], // room 0A
        [0x04, 0xFF, 0xFF, 0xFF], // room 0B
        [0x0C, 0xFF, 0xFF, 0xFF], // room 0C
        [0x10, 0x16, 0x17, 0x07], // room 0D
        [0x14, 0x15, 0x0F, 0xFF], // room 0E
        [0xFF, 0xFF, 0xFF, 0xFF], // room 0F
        [0x06, 0xFF, 0xFF, 0xFF], // room 10
        [0x08, 0xFF, 0xFF, 0xFF], // room 11
        [0x08, 0xFF, 0xFF, 0xFF], // room 12
        [0x07, 0xFF, 0xFF, 0xFF], // room 13
        [0x07, 0xFF, 0xFF, 0xFF], // room 14
        [0xFF, 0xFF, 0xFF, 0xFF], // room 15
        [0xFF, 0xFF, 0xFF, 0xFF], // room 16
        [0xFF, 0xFF, 0xFF, 0xFF], // room 17
        [0xFF, 0xFF, 0xFF, 0xFF], // room 18
        [0xFF, 0xFF, 0xFF, 0xFF], // room 19
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1A
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1B
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1C
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1D
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1E
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1F
    ],
    // stage 2
    [
        [0x24, 0xFF, 0xFF, 0xFF], // room 00
        [0x24, 0xFF, 0xFF, 0xFF], // room 01
        [0x24, 0xFF, 0xFF, 0xFF], // room 02
        [0x25, 0xFF, 0xFF, 0xFF], // room 03
        [0x24, 0xFF, 0xFF, 0xFF], // room 04
        [0x24, 0xFF, 0xFF, 0xFF], // room 05
        [0x0C, 0xFF, 0xFF, 0xFF], // room 06
        [0x26, 0xFF, 0xFF, 0xFF], // room 07
        [0x20, 0xFF, 0xFF, 0xFF], // room 08
        [0x21, 0xFF, 0xFF, 0xFF], // room 09
        [0xFF, 0xFF, 0xFF, 0xFF], // room 0A
        [0x22, 0xFF, 0xFF, 0xFF], // room 0B
        [0x23, 0xFF, 0xFF, 0xFF], // room 0C
        [0x21, 0xFF, 0xFF, 0xFF], // room 0D
        [0x0F, 0xFF, 0xFF, 0xFF], // room 0E
        [0x21, 0xFF, 0xFF, 0xFF], // room 0F
        [0x37, 0xFF, 0xFF, 0xFF], // room 10
        [0xFF, 0xFF, 0xFF, 0xFF], // room 11
        [0xFF, 0xFF, 0xFF, 0xFF], // room 12
        [0xFF, 0xFF, 0xFF, 0xFF], // room 13
        [0xFF, 0xFF, 0xFF, 0xFF], // room 14
        [0xFF, 0xFF, 0xFF, 0xFF], // room 15
        [0xFF, 0xFF, 0xFF, 0xFF], // room 16
        [0xFF, 0xFF, 0xFF, 0xFF], // room 17
        [0xFF, 0xFF, 0xFF, 0xFF], // room 18
        [0xFF, 0xFF, 0xFF, 0xFF], // room 19
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1A
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1B
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1C
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1D
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1E
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1F
    ],
    // stage 3
    [
        [0x2B, 0xFF, 0xFF, 0xFF], // room 00
        [0x0C, 0xFF, 0xFF, 0xFF], // room 01
        [0x0C, 0xFF, 0xFF, 0xFF], // room 02
        [0x0F, 0xFF, 0xFF, 0xFF], // room 03
        [0x2B, 0xFF, 0xFF, 0xFF], // room 04
        [0x2D, 0x2C, 0xFF, 0xFF], // room 05
        [0x0C, 0x2E, 0xFF, 0xFF], // room 06
        [0x0C, 0xFF, 0xFF, 0xFF], // room 07
        [0x2B, 0xFF, 0xFF, 0xFF], // room 08
        [0x1F, 0xFF, 0xFF, 0xFF], // room 09
        [0x0C, 0xFF, 0xFF, 0xFF], // room 0A
        [0x0C, 0xFF, 0xFF, 0xFF], // room 0B
        [0x27, 0x28, 0xFF, 0xFF], // room 0C
        [0x29, 0xFF, 0xFF, 0xFF], // room 0D
        [0x29, 0xFF, 0xFF, 0xFF], // room 0E
        [0x0C, 0xFF, 0xFF, 0xFF], // room 0F
        [0x29, 0xFF, 0xFF, 0xFF], // room 10
        [0x29, 0xFF, 0xFF, 0xFF], // room 11
        [0xFF, 0xFF, 0xFF, 0xFF], // room 12
        [0xFF, 0xFF, 0xFF, 0xFF], // room 13
        [0xFF, 0xFF, 0xFF, 0xFF], // room 14
        [0xFF, 0xFF, 0xFF, 0xFF], // room 15
        [0xFF, 0xFF, 0xFF, 0xFF], // room 16
        [0xFF, 0xFF, 0xFF, 0xFF], // room 17
        [0xFF, 0xFF, 0xFF, 0xFF], // room 18
        [0xFF, 0xFF, 0xFF, 0xFF], // room 19
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1A
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1B
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1C
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1D
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1E
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1F
    ],
    // stage 4
    [
        [0x36, 0x35, 0xFF, 0xFF], // room 00
        [0x35, 0xFF, 0xFF, 0xFF], // room 01
        [0x36, 0x35, 0x26, 0xFF], // room 02
        [0x36, 0x35, 0xFF, 0xFF], // room 03
        [0x36, 0x35, 0xFF, 0xFF], // room 04
        [0x2F, 0x35, 0xFF, 0xFF], // room 05
        [0x2F, 0x35, 0xFF, 0xFF], // room 06
        [0x2F, 0x35, 0xFF, 0xFF], // room 07
        [0x2F, 0x35, 0xFF, 0xFF], // room 08
        [0x2F, 0x35, 0xFF, 0xFF], // room 09
        [0x2F, 0x35, 0xFF, 0xFF], // room 0A
        [0x30, 0x35, 0xFF, 0xFF], // room 0B
        [0x2F, 0x35, 0xFF, 0xFF], // room 0C
        [0x35, 0xFF, 0xFF, 0xFF], // room 0D
        [0x0F, 0x35, 0xFF, 0xFF], // room 0E
        [0x31, 0x35, 0xFF, 0xFF], // room 0F
        [0x31, 0x35, 0xFF, 0xFF], // room 10
        [0x31, 0x35, 0xFF, 0xFF], // room 11
        [0x30, 0x35, 0xFF, 0xFF], // room 12
        [0x34, 0x35, 0xFF, 0xFF], // room 13
        [0x32, 0x33, 0x35, 0xFF], // room 14
        [0x35, 0xFF, 0xFF, 0xFF], // room 15
        [0xFF, 0xFF, 0xFF, 0xFF], // room 16
        [0xFF, 0xFF, 0xFF, 0xFF], // room 17
        [0xFF, 0xFF, 0xFF, 0xFF], // room 18
        [0xFF, 0xFF, 0xFF, 0xFF], // room 19
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1A
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1B
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1C
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1D
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1E
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1F
    ],
    // stage 5
    [
        [0x0F, 0x0A, 0xFF, 0xFF], // room 00
        [0x13, 0x1B, 0xFF, 0xFF], // room 01
        [0xFF, 0xFF, 0xFF, 0xFF], // room 02
        [0x18, 0xFF, 0xFF, 0xFF], // room 03
        [0x18, 0xFF, 0xFF, 0xFF], // room 04
        [0x18, 0xFF, 0xFF, 0xFF], // room 05
        [0x18, 0xFF, 0xFF, 0xFF], // room 06
        [0x18, 0xFF, 0xFF, 0xFF], // room 07
        [0x18, 0xFF, 0xFF, 0xFF], // room 08
        [0x02, 0xFF, 0xFF, 0xFF], // room 09
        [0x02, 0xFF, 0xFF, 0xFF], // room 0A
        [0x18, 0x02, 0xFF, 0xFF], // room 0B
        [0x1E, 0xFF, 0xFF, 0xFF], // room 0C
        [0x1C, 0xFF, 0xFF, 0xFF], // room 0D
        [0x18, 0xFF, 0xFF, 0xFF], // room 0E
        [0xFF, 0xFF, 0xFF, 0xFF], // room 0F
        [0xFF, 0xFF, 0xFF, 0xFF], // room 10
        [0xFF, 0xFF, 0xFF, 0xFF], // room 11
        [0xFF, 0xFF, 0xFF, 0xFF], // room 12
        [0xFF, 0xFF, 0xFF, 0xFF], // room 13
        [0x02, 0xFF, 0xFF, 0xFF], // room 14
        [0x03, 0xFF, 0xFF, 0xFF], // room 15
        [0xFF, 0xFF, 0xFF, 0xFF], // room 16
        [0x05, 0x02, 0xFF, 0xFF], // room 17
        [0xFF, 0xFF, 0xFF, 0xFF], // room 18
        [0x02, 0xFF, 0xFF, 0xFF], // room 19
        [0x02, 0xFF, 0xFF, 0xFF], // room 1A
        [0x02, 0xFF, 0xFF, 0xFF], // room 1B
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1C
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1D
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1E
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1F
    ],
    // stage 6
    [
        [0x37, 0xFF, 0xFF, 0xFF], // room 00
        [0x19, 0xFF, 0xFF, 0xFF], // room 01
        [0x19, 0xFF, 0xFF, 0xFF], // room 02
        [0xFF, 0xFF, 0xFF, 0xFF], // room 03
        [0x19, 0xFF, 0xFF, 0xFF], // room 04
        [0x1F, 0xFF, 0xFF, 0xFF], // room 05
        [0x13, 0x1B, 0xFF, 0xFF], // room 06
        [0xFF, 0xFF, 0xFF, 0xFF], // room 07
        [0x19, 0xFF, 0xFF, 0xFF], // room 08
        [0x19, 0xFF, 0xFF, 0xFF], // room 09
        [0x19, 0xFF, 0xFF, 0xFF], // room 0A
        [0x04, 0xFF, 0xFF, 0xFF], // room 0B
        [0x0C, 0x12, 0xFF, 0xFF], // room 0C
        [0x19, 0xFF, 0xFF, 0xFF], // room 0D
        [0xFF, 0xFF, 0xFF, 0xFF], // room 0E
        [0x19, 0xFF, 0xFF, 0xFF], // room 0F
        [0xFF, 0xFF, 0xFF, 0xFF], // room 10
        [0x08, 0xFF, 0xFF, 0xFF], // room 11
        [0x08, 0xFF, 0xFF, 0xFF], // room 12
        [0x19, 0xFF, 0xFF, 0xFF], // room 13
        [0x19, 0xFF, 0xFF, 0xFF], // room 14
        [0xFF, 0xFF, 0xFF, 0xFF], // room 15
        [0x0C, 0xFF, 0xFF, 0xFF], // room 16
        [0x0C, 0xFF, 0xFF, 0xFF], // room 17
        [0x1D, 0xFF, 0xFF, 0xFF], // room 18
        [0xFF, 0xFF, 0xFF, 0xFF], // room 19
        [0x1A, 0xFF, 0xFF, 0xFF], // room 1A
        [0x1A, 0xFF, 0xFF, 0xFF], // room 1B
        [0x1A, 0xFF, 0xFF, 0xFF], // room 1C
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1D
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1E
        [0xFF, 0xFF, 0xFF, 0xFF], // room 1F
    ],
];

/// Initial per-room BGM state bytes (7 stages x 32 rooms), taken from the
/// game's initial state data at offset 0x33C of `bio_card.dat`.
pub const ROOM_STATE: [u8; 224] = [
    0x40, 0xFF, 0xFF, 0x08, 0x40, 0x10, 0x41, 0x08, 0x08, 0xFF, 0xFF, 0x08, 0x18, 0x40, 0x08, 0x40,
    0xFF, 0xFF, 0xFF, 0xFF, 0x08, 0x40, 0xFF, 0x48, 0xFF, 0x40, 0x08, 0x08, 0xFF, 0xFF, 0x01,
    0xFF, // stage 0
    0x40, 0x08, 0x18, 0x40, 0x08, 0x40, 0xFF, 0xFF, 0x08, 0x08, 0x40, 0x40, 0xFF, 0x40, 0x40, 0xFF,
    0x40, 0x08, 0x28, 0x08, 0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02,
    0xFF, // stage 1
    0x08, 0x08, 0x18, 0x08, 0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0xFF, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x40, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x03,
    0xFF, // stage 2
    0x08, 0x08, 0x08, 0x08, 0x08, 0xFF, 0x08, 0x08, 0x08, 0x40, 0x08, 0x08, 0xFF, 0x08, 0x08, 0x08,
    0x08, 0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x04,
    0xFF, // stage 3
    0x08, 0xFF, 0x08, 0x08, 0x08, 0x08, 0x40, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0xFF, 0x08, 0x08,
    0x08, 0x18, 0x10, 0x18, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x05,
    0xFF, // stage 4
    0x08, 0x40, 0xFF, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x40, 0x40, 0x41, 0x18, 0x40, 0x08, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0x08, 0x40, 0xFF, 0x41, 0xFF, 0x08, 0x08, 0x08, 0xFF, 0xFF, 0x06,
    0xFF, // stage 5
    0x40, 0x08, 0x08, 0xFF, 0x08, 0x40, 0xFF, 0xFF, 0x08, 0x08, 0x08, 0x40, 0x41, 0x08, 0xFF, 0x08,
    0xFF, 0x08, 0x28, 0x08, 0x08, 0xFF, 0x08, 0x08, 0x40, 0xFF, 0x08, 0x08, 0x08, 0xFF, 0x07,
    0xFF, // stage 6
];

/// BGM group selected by `state` for the room at 0-based `stage` and `room`.
///
/// `state & 7` picks the four-group row entry; `0xFF` (and a `0xFF` entry)
/// means no music. A `0xFF` state is checked first because its low bits would
/// otherwise read past the row's four entries.
pub fn group_for(stage: usize, room: usize, state: u8) -> Option<u8> {
    if state == 0xFF {
        return None;
    }
    let group = *ROOM_GROUPS
        .get(stage)?
        .get(room)?
        .get(usize::from(state & 7))?;
    if group == 0xFF { None } else { Some(group) }
}

/// Every non-`Bgm_*` group-track basename, sorted and deduplicated.
///
/// The `Bgm_*` tracks live in `bgm/`; the rest (the three muted seeds,
/// `chain1`, the `Se_3*`/`Se_4*`/`Se_5*` cues and the mixed dialogue track
/// `V110_00`) are read from `se/`, so conversion packs them alongside the
/// named sound effects.
pub fn se_track_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = GROUP_TRACKS
        .iter()
        .flatten()
        .filter_map(|slot| *slot)
        .filter(|name| {
            !name
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bgm_"))
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// The non-null `(basename, loop)` slots of a group, in slot order.
pub fn tracks_for(group: u8) -> Vec<(&'static str, bool)> {
    let Some(names) = GROUP_TRACKS.get(usize::from(group)) else {
        return Vec::new();
    };
    let loops = &GROUP_LOOPS[usize::from(group)];
    names
        .iter()
        .zip(loops)
        .filter_map(|(name, &looped)| name.map(|name| (name, looped)))
        .collect()
}

/// The first non-null track of the group selected for `id`'s shipped state.
///
/// This is the M5 single-track view, kept for the pack-loading check; the
/// running engine goes through [`crate::bgm::update_room_bgm`], which loads all
/// three channels and follows the restart/reload state types.
pub fn primary_track(id: RoomId) -> Option<(&'static str, bool)> {
    let stage = usize::from(id.stage.checked_sub(1)?);
    let room = usize::from(id.room);
    let state = *ROOM_STATE.get(stage * 32 + room)?;
    tracks_for(group_for(stage, room, state)?)
        .into_iter()
        .next()
}

/// Pack path for a BGM basename or file name, e.g. `Bgm_13` or `BGM_13.WAV`.
///
/// `Bgm_<id>` maps to `bgm/<id:03x>.wav` and `Bgm_<id><letter>` to
/// `bgm/<id:03x>_<letter:02>.wav` with `A` = 0. The id is at most two hex
/// digits, so in an ambiguous name such as `Bgm_24a` the trailing letter is a
/// variant while in `Bgm_0a` the two hex digits are the id. Names outside
/// `Bgm_` return `None`.
pub fn pack_path(name: &str) -> Option<String> {
    let stem = match name.get(name.len().saturating_sub(4)..) {
        Some(suffix) if name.len() >= 4 && suffix.eq_ignore_ascii_case(".wav") => {
            name.get(..name.len() - 4)?
        }
        _ => name,
    };
    if !stem.get(..4)?.eq_ignore_ascii_case("bgm_") {
        return None;
    }

    let mut value = 0u32;
    let mut digits = 0usize;
    let mut variant = None;
    let mut chars = stem.get(4..)?.chars();
    while digits < 2 {
        let Some(c) = chars.next() else {
            break;
        };
        match c.to_digit(16) {
            Some(digit) => {
                value = value * 16 + digit;
                digits += 1;
            }
            None if digits > 0 && c.is_ascii_alphabetic() => {
                variant = Some((c.to_ascii_uppercase() as u8) - b'A');
                break;
            }
            None => return None,
        }
    }
    if variant.is_none()
        && digits == 2
        && let Some(c) = chars.next()
    {
        if c.is_ascii_alphabetic() {
            variant = Some((c.to_ascii_uppercase() as u8) - b'A');
        } else {
            return None;
        }
    }
    if digits == 0 || chars.next().is_some() {
        return None;
    }

    Some(match variant {
        Some(variant) => format!("bgm/{value:03x}_{variant:02}.wav"),
        None => format!("bgm/{value:03x}.wav"),
    })
}

/// Pack path of a BGM id and optional variant, e.g. `bgm/013.wav`.
pub fn bgm_entry(id: u8, variant: Option<u8>) -> String {
    match variant {
        Some(variant) => format!("bgm/{id:03x}_{variant:02}.wav"),
        None => format!("bgm/{id:03x}.wav"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> RoomId {
        RoomId::parse(text).unwrap()
    }

    /// The shipped state byte for `id` resolved to its group.
    fn room_group(id: RoomId) -> Option<u8> {
        let stage = usize::from(id.stage.checked_sub(1)?);
        let room = usize::from(id.room);
        group_for(stage, room, ROOM_STATE[stage * 32 + room])
    }

    #[test]
    fn save_room_plays_bgm_13() {
        let room = id("1001");
        assert_eq!(room_group(room), Some(0x0B));
        assert_eq!(primary_track(room), Some(("Bgm_13", true)));
        assert_eq!(pack_path("Bgm_13"), Some("bgm/013.wav".to_owned()));
    }

    #[test]
    fn groups_match_known_rooms() {
        assert_eq!(room_group(id("1000")), Some(0x0B));
        assert_eq!(room_group(id("1040")), Some(0x00));
        assert_eq!(room_group(id("10A0")), None);
        assert_eq!(room_group(id("2040")), Some(0x07));
        assert_eq!(room_group(id("3000")), Some(0x24));
        assert_eq!(room_group(id("2170")), None);
        assert_eq!(room_group(id("6000")), Some(0x0F));
        assert_eq!(primary_track(id("6000")), Some(("Bgm_31", true)));
        assert_eq!(room_group(id("7000")), Some(0x37));
        assert_eq!(primary_track(id("7000")), Some(("Se_59", false)));
    }

    #[test]
    fn group_lookup_takes_the_state_byte() {
        // Room 1000's row entry 0 is Bgm_13's group; the shipped state 0x40
        // selects it because 0x40 & 7 == 0.
        assert_eq!(ROOM_GROUPS[0][0][0], 0x0B);
        assert_eq!(group_for(0, 0, 0x40), Some(0x0B));
        assert_eq!(group_for(0, 0, 0xFF), None);
        // Same room, different low bits select a different entry.
        assert_eq!(group_for(0, 0, 0x41), Some(0x0F));
        assert_eq!(group_for(7, 0, 0x40), None, "there is no stage 8");
    }

    #[test]
    fn tracks_keep_slot_order_and_loop_flags() {
        assert_eq!(tracks_for(0x0B), vec![("Bgm_13", true)]);
        assert_eq!(
            tracks_for(0x00),
            vec![("Bgm_00", true), ("Se_01", true), ("Bgm_05", true)]
        );
        assert_eq!(
            tracks_for(0x0D),
            vec![("Bgm_10", false), ("Bgm_11", true), ("Bgm_12", false)]
        );
        assert!(tracks_for(0x09).is_empty());
        assert!(tracks_for(0xFF).is_empty());
    }

    #[test]
    fn every_room_group_exists_in_the_track_table() {
        for stage in ROOM_GROUPS {
            for room in stage {
                for group in room {
                    if group != 0xFF {
                        assert!(
                            usize::from(group) < GROUP_TRACKS.len(),
                            "group {group:#04X} has no track row"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn pack_path_names_wav_entries() {
        assert_eq!(pack_path("Bgm_13"), Some("bgm/013.wav".to_owned()));
        assert_eq!(pack_path("BGM_0A"), Some("bgm/00a.wav".to_owned()));
        assert_eq!(pack_path("bgm_57"), Some("bgm/057.wav".to_owned()));
        assert_eq!(pack_path("Bgm_24a"), Some("bgm/024_00.wav".to_owned()));
        assert_eq!(pack_path("BGM_24B"), Some("bgm/024_01.wav".to_owned()));
        assert_eq!(pack_path("BGM_24A.WAV"), Some("bgm/024_00.wav".to_owned()));
        assert_eq!(pack_path("bgm_13.wav"), Some("bgm/013.wav".to_owned()));
    }

    #[test]
    fn pack_path_rejects_non_bgm_names() {
        assert_eq!(pack_path("Se_01"), None);
        assert_eq!(pack_path("chain1"), None);
        assert_eq!(pack_path("Bgm_"), None);
        assert_eq!(pack_path("Bgm_13.wav.txt"), None);
        assert_eq!(pack_path("Bgm_24ab"), None);
        assert_eq!(pack_path(""), None);
    }

    #[test]
    fn bgm_entries_format_ids_and_variants() {
        assert_eq!(bgm_entry(0x13, None), "bgm/013.wav");
        assert_eq!(bgm_entry(0x24, Some(0)), "bgm/024_00.wav");
        assert_eq!(bgm_entry(0x24, Some(1)), "bgm/024_01.wav");
    }
}
