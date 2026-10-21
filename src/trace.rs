//! Oracle trace v1 emission (`oracle/trace-v1.md`).
//!
//! The shared, engine-neutral per-tick trace both the C++ reference oracle and
//! arklay emit: one line per gameplay tick, plus an `oracle-trace v1
//! engine=ark` header and an FNV-1a frame hash per line. [`TraceWriter`] is
//! the streaming file; [`TraceTick`] is the engine state one line carries.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::game::Entity;
use crate::render::Framebuffer;
use crate::state::RoomId;

/// FNV-1a 32-bit offset basis.
const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
/// FNV-1a 32-bit prime.
const FNV_PRIME: u32 = 0x0100_0193;

/// FNV-1a (32-bit) of a rendered frame.
///
/// The width and height go in as little-endian `u32`s, then every RGBA byte
/// row-major with the top row first. `Framebuffer::rgba` is already top-down.
pub fn frame_hash(framebuffer: &Framebuffer) -> u32 {
    let mut hash = FNV_OFFSET_BASIS;
    let mut feed = |byte: u8| {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    };
    for byte in framebuffer.width.to_le_bytes() {
        feed(byte);
    }
    for byte in framebuffer.height.to_le_bytes() {
        feed(byte);
    }
    for &byte in &framebuffer.rgba {
        feed(byte);
    }
    hash
}

/// One tick's state for [`TraceWriter::write_tick`].
pub struct TraceTick<'a> {
    /// The live room identity.
    pub room: RoomId,
    /// The platform PRNG state (`GameState::rand_state`).
    pub rng: u32,
    /// The per-frame seed (`GameState::rand_seed`).
    pub rseed: u16,
    /// The room fade step (the reference's `g_fading_counter`).
    pub fade_counter: i16,
    /// The room fade accumulator (the reference's `g_fading_state`).
    pub fade_state: i16,
    /// Current camera cut index.
    pub cam: u8,
    /// The player's `PlayerEntity.flags` byte (`GameState::player_flags`): the
    /// aim direction bits. The reference reads the player's status byte at
    /// entity +0x00, which the player entity shares with this flags byte, so
    /// slot 0's `status` field is this ORed with the entity's active bit.
    pub player_flags: u8,
    /// Every entity slot; slot 0 is the player and is always emitted.
    pub entities: &'a [Entity],
    /// [`frame_hash`] of the rendered frame.
    pub frame_hash: u32,
}

/// Where a headless oracle run writes its trace and capture frames.
#[derive(Debug, Clone, Copy, Default)]
pub struct TraceOptions<'a> {
    /// The trace file, or `None` for a plain run.
    pub path: Option<&'a Path>,
    /// Write a BMP every N traced ticks; `0` disables the cadence.
    pub capture_every: u32,
    /// Directory the `f%05d.bmp` captures are written to.
    pub capture_dir: Option<&'a Path>,
    /// Fixed ticks to run.
    pub ticks: u32,
    /// Scripted per-tick input playback (`--input`), or `None` for an idle run.
    pub input: Option<&'a crate::engine::InputScript>,
}

/// The streaming oracle trace file.
///
/// The header is written on creation; [`TraceWriter::write_tick`] emits one
/// line per traced gameplay tick with `t` starting at 0.
pub struct TraceWriter {
    out: BufWriter<File>,
    ticks: u32,
}

impl TraceWriter {
    /// Create `path` and write the engine header line.
    pub fn create(path: &Path) -> Result<Self> {
        let file =
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
        let mut out = BufWriter::new(file);
        out.write_all(b"oracle-trace v1 engine=ark\n")
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(Self { out, ticks: 0 })
    }

    /// The `t` of the next tick line.
    pub fn tick_index(&self) -> u32 {
        self.ticks
    }

    /// Emit one tick line.
    ///
    /// Slot 0 (the player) is always emitted; every other slot only while the
    /// entity's active status bit is set. `t` increments once per line.
    pub fn write_tick(&mut self, tick: &TraceTick<'_>) -> Result<()> {
        write!(
            self.out,
            "t={} room={}:{} rng={} rseed={} fade={}:{}",
            self.ticks,
            tick.room.stage,
            tick.room.room,
            tick.rng,
            tick.rseed,
            tick.fade_counter,
            tick.fade_state
        )?;
        match tick.entities.first() {
            Some(player) => write!(
                self.out,
                " p={},{},{},{},{},{},{}",
                player.pos[0],
                player.pos[1],
                player.pos[2],
                player.angle & 0x0FFF,
                player.action_behavior,
                player.action_state,
                player.health
            )?,
            None => write!(self.out, " p=")?,
        }
        write!(self.out, " cam={}", tick.cam)?;
        for (slot, entity) in tick.entities.iter().enumerate() {
            if slot > 0 && !entity.active() {
                continue;
            }
            // The reference reads one byte at 0x8A: `isBeingAttackedFlag` for
            // the player and `hit_state` for every other entity. The port
            // keeps the player's flag in its own field, so slot 0 emits that
            // one.
            let hit = if slot == 0 {
                entity.is_being_attacked
            } else {
                entity.hit_state
            };
            write!(
                self.out,
                " e={},{},{},{},{},{},{},{},{},{},{},{},{}",
                slot,
                entity.id,
                entity.pos[0],
                entity.pos[1],
                entity.pos[2],
                entity.angle & 0x0FFF,
                entity.state(),
                entity.action_behavior,
                entity.action_state,
                entity.health,
                hit,
                if slot == 0 {
                    entity.status_flags | tick.player_flags
                } else {
                    entity.status_flags
                },
                entity.action_ticks_counter
            )?;
        }
        writeln!(self.out, " f={}", tick.frame_hash)?;
        self.ticks += 1;
        Ok(())
    }

    /// Flush the buffered lines.
    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::game::{ENTITY_STATUS_ACTIVE, GameState};

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("arklay-trace-{}-{label}.txt", std::process::id()))
    }

    #[test]
    fn frame_hash_matches_the_reference_vectors() {
        let one = Framebuffer {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255],
            clip: None,
            view: crate::video::View::default(),
            hd: None,
        };
        assert_eq!(frame_hash(&one), 0x6dc1_4b23);

        let two = Framebuffer {
            width: 2,
            height: 1,
            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
            clip: None,
            view: crate::video::View::default(),
            hd: None,
        };
        assert_eq!(frame_hash(&two), 0xa4af_09a6);

        let blank = Framebuffer {
            width: 320,
            height: 240,
            rgba: vec![0; 320 * 240 * 4],
            clip: None,
            view: crate::video::View::default(),
            hd: None,
        };
        assert_eq!(frame_hash(&blank), 0xfae3_db1a);
    }

    #[test]
    fn tick_lines_emit_the_player_and_active_slots_in_order() {
        let mut game = GameState::default();
        game.entities[0].id = 0;
        game.entities[0].pos = [17000, 0, 5000];
        // The yaw is emitted masked to 12 bits: the sign-extended form the
        // reference stores (`0xFC00`) prints as `0x0C00` (3072).
        game.entities[0].angle = 0xFC00;
        game.entities[0].health = 140;
        // Slot 0 is emitted even while its active bit is clear.
        game.entities[0].status_flags = 0;
        game.entities[1].id = 0x24;
        game.entities[1].pos = [1, 2, 3];
        game.entities[1].angle = 0x1C04;
        game.entities[1].health = 5;
        game.entities[1].hit_state = 6;
        game.entities[1].action_behavior = 7;
        game.entities[1].action_state = 8;
        game.entities[1].action_ticks_counter = 9;
        game.entities[1].status_flags = ENTITY_STATUS_ACTIVE;
        game.entities[2].id = 0x21;
        game.entities[2].pos = [10, 11, 12];
        game.entities[2].status_flags = 0;

        let path = temp_path("shape");
        let mut writer = TraceWriter::create(&path).unwrap();
        assert_eq!(writer.tick_index(), 0);
        let tick = TraceTick {
            room: RoomId {
                stage: 1,
                room: 0x06,
                player_flag: 0,
            },
            rng: 1,
            rseed: 2,
            fade_counter: 0,
            fade_state: -1,
            cam: 3,
            player_flags: 0,
            entities: &*game.entities,
            frame_hash: 0xDEAD_BEEF,
        };
        writer.write_tick(&tick).unwrap();
        assert_eq!(writer.tick_index(), 1);
        writer.flush().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            text,
            "oracle-trace v1 engine=ark\n\
             t=0 room=1:6 rng=1 rseed=2 fade=0:-1 p=17000,0,5000,3072,0,0,140 cam=3 \
             e=0,0,17000,0,5000,3072,0,0,0,140,0,0,0 \
             e=1,36,1,2,3,3076,0,7,8,5,6,1,9 f=3735928559\n"
        );
    }

    #[test]
    fn tick_index_increments_once_per_line() {
        let game = GameState::default();
        let path = temp_path("counter");
        let mut writer = TraceWriter::create(&path).unwrap();
        let tick = TraceTick {
            room: RoomId {
                stage: 1,
                room: 0x06,
                player_flag: 0,
            },
            rng: 0,
            rseed: 0,
            fade_counter: 0,
            fade_state: -1,
            cam: 0,
            player_flags: 0,
            entities: &*game.entities,
            frame_hash: 0,
        };
        for index in 0..3 {
            assert_eq!(writer.tick_index(), index);
            writer.write_tick(&tick).unwrap();
        }
        writer.flush().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("oracle-trace v1 engine=ark"));
        assert!(lines[1].starts_with("t=0 "));
        assert!(lines[2].starts_with("t=1 "));
        assert!(lines[3].starts_with("t=2 "));
    }
}
