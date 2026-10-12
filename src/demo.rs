//! The attract-demo reels (`data/pdemoN.dat`).
//!
//! Each reel is 0x994 bytes: a 0x30-byte header (recorded room, character,
//! position, items, length and camera), 1202 recorded pad words from +0x30,
//! and a four-byte tail at +0x990 (the recorded controller config and player
//! health, overlapping the last two pad words exactly like the original's
//! single 0x994-byte block).
//!
//! The title's idle timeout hands off to `game_start` with
//! `MSF2_ATTRACT_DEMO`; the engine replays the recorded words through the pad
//! every gameplay frame and ends the reel with the attract death fade when the
//! playback timer runs out or the live pad is touched.

use anyhow::{Result, bail};

use crate::player;
use crate::state::RoomId;
use crate::ui::UiInput;

/// Recorded pad words the file holds (the original's `g_demoPadData` size).
pub const PAD_WORDS: usize = 1202;
/// The header size in bytes.
pub const HEADER_LEN: usize = 0x30;
/// The tail's controller-config offset.
pub const TAIL_CONFIG: usize = 0x990;
/// The tail's player-health offset.
pub const TAIL_HEALTH: usize = 0x992;

/// One attract-demo reel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoReel {
    /// Reel index (`0..=3`), for logging.
    pub index: u8,
    /// The recorded room identity; `player_flag` carries the recorded
    /// character.
    pub room: RoomId,
    /// The recorded character (`0` Chris, `1` Jill).
    pub character: u8,
    /// Held item count from the header.
    pub total_held_items: u8,
    /// The equipped item id.
    pub equipped_item: u8,
    /// The recorded player position (X, 0, Z).
    pub pos: [i32; 3],
    /// The recorded player facing angle.
    pub angle: u16,
    /// The playback counter the load resets to (`g_DemoTimerCur`).
    pub timer_cur: i16,
    /// The recorded reel length in frames (`g_DemoTimerMax`).
    pub timer_max: i16,
    /// The recorded camera id (`g_AttractMode_RoomCameraId`).
    pub camera_id: u8,
    /// The 12 recorded inventory slots, id/quantity pairs.
    pub items: [u8; 24],
    /// The recorded pad words, one per playback frame.
    pub pads: Vec<u16>,
    /// The controller-config word from the tail.
    pub controller_config: u16,
    /// The player health from the tail.
    pub health: i16,
}

impl DemoReel {
    /// Parse a `pdemoN.dat` image.
    pub fn parse(index: u8, data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_LEN + 4 {
            bail!(
                "demo reel {index} is {} bytes; at least {} are required",
                data.len(),
                HEADER_LEN + 4
            );
        }
        let character = data[4] & 1;
        let stage = data[1].wrapping_add(1);
        let room = data[0];
        let read_i16 =
            |offset: usize| -> i16 { i16::from_le_bytes([data[offset], data[offset + 1]]) };
        let mut items = [0u8; 24];
        items.copy_from_slice(&data[0x18..0x30]);
        let pad_end = (HEADER_LEN + PAD_WORDS * 2).min(data.len());
        let pads = data[HEADER_LEN..pad_end]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        // The tail overlaps the last two pad words; read it when present (the
        // Director's Cut's shortest reel is four bytes short).
        let controller_config = if data.len() >= TAIL_CONFIG + 2 {
            u16::from_le_bytes([data[TAIL_CONFIG], data[TAIL_CONFIG + 1]])
        } else {
            0
        };
        let health = if data.len() >= TAIL_HEALTH + 2 {
            read_i16(TAIL_HEALTH)
        } else {
            0
        };
        if room > RoomId::MAX_ROOM || stage > RoomId::MAX_STAGE {
            bail!("demo reel {index} names an invalid room {stage}:{room:#04X}");
        }
        Ok(Self {
            index,
            room: RoomId {
                stage,
                room,
                player_flag: character,
            },
            character,
            total_held_items: data[2],
            equipped_item: data[3],
            pos: [i32::from(read_i16(0x06)), 0, i32::from(read_i16(0x08))],
            angle: (read_i16(0x0A) as u16) & 0x0FFF,
            timer_cur: read_i16(0x0E),
            timer_max: read_i16(0x10),
            camera_id: data[0x14],
            items,
            pads,
            controller_config,
            health,
        })
    }

    /// The reel's pack entry.
    pub fn entry(&self) -> String {
        format!("data/pdemo{}.dat", self.index)
    }
}

/// The playback state of one attract-demo session.
#[derive(Debug, Clone)]
pub struct AttractState {
    /// The reel being played.
    pub reel: DemoReel,
    /// The playback counter (`g_DemoTimerCur`); `0` stops input injection.
    pub timer_cur: i16,
    /// The reel length (`g_DemoTimerMax`).
    pub timer_max: i16,
    /// The playback gate (`g_message_flags` bit `0x200`): the first gameplay
    /// tick runs with it clear, like the original's resumed outer setup.
    pub gate: bool,
    /// The previous frame's pad word, for edge detection.
    prev_pad: u16,
    /// The reel's playback ended and the attract death fade was armed.
    pub ended: bool,
}

/// The translated input for one demo frame.
pub struct DemoInput {
    /// The UI/edge input.
    pub ui: UiInput,
    /// The player input.
    pub player: player::Input,
    /// The action level (`Action` held or pressed).
    pub action: bool,
}

impl AttractState {
    /// A freshly loaded reel: `timer_cur = 1`, gate clear (the original
    /// resets the playback counter after the file load and sets the gate on
    /// the second gameplay tick).
    pub fn new(reel: DemoReel) -> Self {
        let timer_cur = if reel.timer_cur == 0 {
            1
        } else {
            reel.timer_cur
        };
        Self {
            timer_max: reel.timer_max,
            reel,
            timer_cur,
            gate: false,
            prev_pad: 0,
            ended: false,
        }
    }

    /// The raw pad word this gameplay tick plays: the word the original read
    /// on the previous frame. The reference's pad update runs after the
    /// player's behaviour update, so the recorded word lands one tick later
    /// than the frame that reads it.
    pub fn pad_word(&self) -> u16 {
        if !self.gate || self.timer_cur == 0 {
            return 0;
        }
        let index = i32::from(self.timer_cur) - 1;
        usize::try_from(index)
            .ok()
            .and_then(|index| self.reel.pads.get(index).copied())
            .unwrap_or(0)
    }

    /// Translate a raw recorded pad word into the engine's input shapes.
    ///
    /// The raw word is the PC build's pad word (the one `JoyToPSX` produces):
    /// `0x1000/0x2000/0x4000/0x8000` are up/down/left/right, `0x0080` the
    /// action/confirm button, `0x0040` cancel/run and `0x0008` aim. The
    /// original's `PlayerPad_Update` remaps raw to its internal dpad word and
    /// clears the left/right bits when up/down are held; the player machine
    /// then reads that word, so the same clearing is applied here.
    pub fn input(word: u16) -> DemoInput {
        let action = word & 0x0080 != 0;
        let aim = word & 0x0008 != 0;
        let run = word & 0x0040 != 0;
        let up = word & 0x1000 != 0;
        let down = word & 0x2000 != 0;
        let mut left = word & 0x4000 != 0;
        let mut right = word & 0x8000 != 0;
        // `if (prevDpadState & 1) clear bit2` (left); `if (& 2) clear bit3`
        // (right): up masks left, down masks right.
        if up {
            left = false;
        }
        if down {
            right = false;
        }
        DemoInput {
            player: player::Input {
                up,
                down,
                left,
                right,
                run,
                action_held: action,
                action_pressed: false,
                // The original has no separate fire button: the action button
                // fires while the weapon is raised. The aim machine only reads
                // it once the aim state owns the tick.
                fire: action,
                fire_pressed: false,
                aim,
                aim_pressed: false,
                slow_motion: false,
                message_blocked: false,
            },
            action,
            ui: UiInput::default(),
        }
    }

    /// Latch this tick's word and derive the edge input from the previous one.
    pub fn tick_input(&mut self) -> DemoInput {
        let word = self.pad_word();
        let pressed = !self.prev_pad & word;
        let mut input = Self::input(word);
        input.player.action_pressed = pressed & 0x0080 != 0;
        input.player.aim_pressed = pressed & 0x0008 != 0;
        input.player.fire_pressed = input.player.action_pressed;
        input.ui.up = pressed & 0x1000 != 0;
        input.ui.down = pressed & 0x2000 != 0;
        input.ui.left = pressed & 0x4000 != 0;
        input.ui.right = pressed & 0x8000 != 0;
        input.ui.confirm = pressed & 0x0080 != 0;
        input.ui.cancel = pressed & 0x0040 != 0;
        input.ui.start = pressed & 0x0800 != 0;
        input.ui.any = pressed != 0;
        self.prev_pad = word;
        input
    }

    /// Advance the playback counter one gameplay frame. The original's
    /// `UpdateDemoTimer` only counts while the gate is set and wraps to zero
    /// one frame before the recorded length.
    pub fn advance(&mut self) {
        if !self.gate || self.timer_cur == 0 {
            return;
        }
        self.timer_cur = self.timer_cur.wrapping_add(1);
        if i32::from(self.timer_max) - 1 <= i32::from(self.timer_cur as u16) {
            self.timer_cur = 0;
        }
    }

    /// Whether the reel's watchdog ends the playback this frame: the counter
    /// is within ten frames of the recorded end, or the live pad is touched.
    pub fn should_end(&self, live_input: bool) -> bool {
        // The original continues while `cur + 10 <= max` and no live button is
        // held, so the reel ends once fewer than ten frames remain.
        let remaining = (i32::from(self.timer_max)) - i32::from(self.timer_cur as u16);
        remaining < 10 || self.timer_cur == 0 || live_input
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reel_bytes() -> Vec<u8> {
        let mut data = vec![0u8; HEADER_LEN + PAD_WORDS * 2];
        data[0] = 0x07;
        data[1] = 0x00;
        data[2] = 0x05;
        data[3] = 0x01;
        data[4] = 0x01;
        data[0x06..0x08].copy_from_slice(&2700i16.to_le_bytes());
        data[0x08..0x0A].copy_from_slice(&6600i16.to_le_bytes());
        data[0x0E..0x10].copy_from_slice(&0i16.to_le_bytes());
        data[0x10..0x12].copy_from_slice(&1046i16.to_le_bytes());
        data[0x14] = 6;
        // The first recorded word is zero; the reel's first input is the up
        // press at index 5.
        data[0x30 + 5 * 2..0x30 + 6 * 2].copy_from_slice(&0x1000u16.to_le_bytes());
        data
    }

    #[test]
    fn parses_the_header_and_tail() {
        let mut data = reel_bytes();
        // The tail overlaps the last two pad words, exactly like the file.
        data[0x990..0x992].copy_from_slice(&5u16.to_le_bytes());
        data[0x992..0x994].copy_from_slice(&96i16.to_le_bytes());
        let reel = DemoReel::parse(2, &data).unwrap();
        assert_eq!(reel.index, 2);
        assert_eq!(reel.room.stage, 1);
        assert_eq!(reel.room.room, 0x07);
        assert_eq!(reel.room.player_flag, 1);
        assert_eq!(reel.character, 1);
        assert_eq!(reel.pos, [2700, 0, 6600]);
        assert_eq!(reel.timer_cur, 0);
        assert_eq!(reel.timer_max, 1046);
        assert_eq!(reel.camera_id, 6);
        assert_eq!(reel.pads[0], 0);
        assert_eq!(reel.pads[5], 0x1000);
        assert_eq!(reel.health, 96);
    }

    #[test]
    fn the_raw_pad_translates_to_the_engine_buttons() {
        let input = AttractState::input(0x1000 | 0x0080 | 0x0008);
        assert!(input.player.up);
        assert!(!input.player.down);
        assert!(input.player.action_held);
        assert!(input.player.fire);
        assert!(input.player.aim);
        assert!(input.action);

        let input = AttractState::input(0x4000 | 0x8000 | 0x2000);
        assert!(input.player.down);
        assert!(input.player.left);
        assert!(!input.player.right, "down masks right");
        assert!(!input.action);

        let input = AttractState::input(0x8000);
        assert!(input.player.right);

        // Up masks left; down masks right, exactly like the remap.
        let input = AttractState::input(0x1000 | 0x4000);
        assert!(input.player.up);
        assert!(!input.player.left);
    }

    #[test]
    fn the_counter_and_gate_follow_the_original() {
        let reel = DemoReel::parse(0, &reel_bytes()).unwrap();
        let max = reel.timer_max;
        let mut state = AttractState::new(reel);
        assert_eq!(state.timer_cur, 1);
        assert!(!state.gate);
        assert_eq!(state.pad_word(), 0, "the closed gate forces zero");

        state.gate = true;
        assert_eq!(state.pad_word(), 0, "counter 1 plays the word read at 0");
        for _ in 0..5 {
            state.advance();
        }
        assert_eq!(state.timer_cur, 6);
        assert_eq!(state.pad_word(), 0x1000, "the held word lands one tick on");

        state.timer_cur = max - 1;
        state.advance();
        assert_eq!(state.timer_cur, 0, "the counter wraps at max - 1");
        assert_eq!(state.pad_word(), 0);
        assert!(state.should_end(false));
    }

    #[test]
    fn the_watchdog_ends_near_the_recorded_end_or_on_live_input() {
        let reel = DemoReel::parse(0, &reel_bytes()).unwrap();
        let mut state = AttractState::new(reel);
        state.gate = true;
        state.timer_cur = 5;
        assert!(!state.should_end(false));
        assert!(state.should_end(true));
        state.timer_cur = state.timer_max - 10;
        assert!(!state.should_end(false), "cur + 10 == max still plays");
        state.timer_cur = state.timer_max - 9;
        assert!(state.should_end(false));
    }
}
