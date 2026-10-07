//! The three-channel BGM state machine and its mixer handoff.
//!
//! The shipped game selects music from the per-stage/room state table
//! ([`crate::game::GameState::room_bgm`]). On every room load or transition
//! [`update_room_bgm`] resolves the target state, fades the outgoing banks when
//! the group changes, loads the group's three tracks and starts the channels
//! whose enable bits (`0x08`/`0x10`/`0x20`) are set. `bgmType` (the top two
//! state bits) selects load-and-start (`0`), load-only (`1`), force-restart
//! (`2`) or nothing (`3`).
//!
//! The script opcodes (`bgm_play`, `bgm_stop`, `bgm_restore`, `bgm_stop_all`)
//! mutate the same game state after the scripts run; [`apply_live`] reconciles
//! the engine's mixer with it, so the host never needs the audio device.

use std::collections::{HashMap, HashSet};

use crate::audio::{self, Mixer};
use crate::game::{BGM_CHANNELS, BgmChannelState, GameState};
use crate::music;
use crate::pack::Pack;
use crate::sfx;
use crate::state::RoomId;
use crate::voice;

/// The original's per-step volume scale (`g_SndDistSteps` and the ramp
/// direction both use it).
const STEP_SCALE: i32 = 0x4E;

/// Arm a volume ramp on channel `channel` (`bgm_volume_ramp`, 0x43; the
/// original's `FUN_004804a0`).
///
/// Only an enabled channel with a loaded bank is ramped. `frames == 0` would
/// fault the original's divide; the port refuses it (documented hardening).
pub fn start_volume_ramp(game: &mut GameState, channel: u8, delta: i8, frames: u8) {
    let index = usize::from(channel);
    if index >= BGM_CHANNELS || frames == 0 {
        return;
    }
    if game.bgm.state & (8u16 << index) == 0 {
        return;
    }
    if game.bgm.channels[index].name.is_none() {
        return;
    }
    game.bgm.ramp = crate::game::VolumeRamp {
        channel,
        direction: (i32::from(delta) / i32::from(frames)) * STEP_SCALE,
        frames_left: i32::from(frames) * 2,
    };
}

/// Build the scripted fade table (`snd_fade_set`, 0x27; the original's
/// `BuildSndFadeTbl`).
///
/// Each loaded channel gets the number of `steps * 0x4E` millibel ticks it
/// needs to reach silence; unloaded channels get zero. `steps == 0` would
/// fault the original's divide; the port refuses it (documented hardening).
pub fn build_snd_fade_tbl(game: &mut GameState, steps: i8) {
    let dist_steps = i32::from(steps) * STEP_SCALE;
    game.bgm.fade.dist_steps = dist_steps;
    game.bgm.fade.kind = 0x7F;
    for (index, bank) in game.bgm.channels.iter().enumerate() {
        game.bgm.fade.steps[index] = if bank.name.is_none() || dist_steps == 0 {
            0
        } else {
            ((-10000 - bank.volume) / dist_steps).max(0)
        };
    }
}

/// Advance the scripted fade one tick (`UpdateSoundFadeState`).
///
/// The positive phase steps every channel still carrying a step count down by
/// `dist_steps` millibels; once every count runs out the fade enters the
/// original's negative countdown, which stops the banks and finally destroys
/// them. The original also steps the voice/sfx/enemy banks; the port fades the
/// three BGM channels only (documented).
fn advance_fade(game: &mut GameState) {
    if game.bgm.fade.kind == 0 {
        return;
    }
    if game.bgm.fade.kind < 0 {
        game.bgm.fade.kind += 1;
        match game.bgm.fade.kind {
            // Stop the BGM banks, then the voice line.
            -29 => {
                for bank in &mut game.bgm.channels {
                    bank.stop = true;
                }
            }
            -4 => {
                game.voice.request = None;
                game.voice.stop_requested = true;
                game.clear_voice_playing();
            }
            // Destroy the banks (the original clears every handle).
            -2 => {
                for bank in &mut game.bgm.channels {
                    *bank = BgmChannelState::default();
                }
            }
            // The final `UpdateSoundFade(-1)` nudge and the end of the fade.
            -1 => {
                for bank in &mut game.bgm.channels {
                    if bank.name.is_some() {
                        bank.volume = (bank.volume - 1).clamp(-9999, -1);
                    }
                }
                game.bgm.fade.kind = 0;
            }
            _ => {}
        }
        return;
    }

    // The scripted fade reaches the channels through the original's global
    // `apply_vol_delta(bank, g_SndDistSteps)`: a negative step count scales the
    // per-tick volume towards silence.
    let dist_steps = game.bgm.fade.dist_steps;
    for (index, bank) in game.bgm.channels.iter_mut().enumerate() {
        if game.bgm.fade.steps[index] <= 0 {
            continue;
        }
        if bank.name.is_some() {
            let mut volume = bank.volume;
            if volume == 2000 {
                volume = -9999;
            }
            bank.volume = (volume + dist_steps).clamp(-9999, -1);
        }
        game.bgm.fade.steps[index] -= 1;
    }
    if game.bgm.fade.steps.iter().all(|steps| *steps < 1) {
        game.bgm.fade.kind = -30;
    }
}

/// Advance the volume ramp one tick (`UpdateSoundDecay`).
///
/// The step is the original's `vol + (400 / (frames_left + 1) * direction) /
/// 100`; once the volume passes -10000 or the frames run out the channel is
/// stopped and its volume reset to -1.
fn advance_ramp(game: &mut GameState) {
    if !game.bgm.ramp.active() {
        return;
    }
    let index = usize::from(game.bgm.ramp.channel);
    let direction = game.bgm.ramp.direction;
    let Some(bank) = game.bgm.channels.get_mut(index) else {
        game.bgm.ramp = crate::game::VolumeRamp::default();
        return;
    };
    if bank.name.is_none() {
        return;
    }
    let mut volume = bank.volume;
    if volume == 2000 {
        volume = -9999;
    }
    let frames_left = game.bgm.ramp.frames_left - 1;
    let current = volume + (400 / (frames_left + 1) * direction) / 100;
    bank.volume = current;
    game.bgm.ramp.frames_left = frames_left;
    if current < -10000 || frames_left <= 0 {
        game.bgm.ramp = crate::game::VolumeRamp::default();
        bank.volume = -1;
        bank.stop = true;
    }
}

/// The three tracks the original loads muted and later fades up with the
/// scripted volume opcodes.
const MUTED_SEEDS: [&str; 3] = ["Se_01", "Se_4d", "Se_42"];

/// Whether `name` is one of the three muted seed tracks.
fn is_muted_seed(name: &str) -> bool {
    MUTED_SEEDS
        .iter()
        .any(|seed| seed.eq_ignore_ascii_case(name))
}

/// Decoded BGM wav cache, keyed by pack path.
#[derive(Default)]
pub struct BgmCache {
    wavs: HashMap<String, audio::Wav>,
    missing: HashSet<String>,
}

impl BgmCache {
    /// Load and parse the wav for the group-track `name`, caching hits and
    /// misses.
    ///
    /// `Bgm_*` names resolve through [`music::pack_path`]; every other group
    /// track (the `Se_*` tracks and the mixed dialogue `V110_00`) lives in the
    /// install's `sound/` directory and is read from `se/`. A track whose
    /// `voice/` entry is the only one packed (the mixed dialogue) resolves
    /// through the same pack as every other entry.
    pub fn load(&mut self, pack: &Pack, name: &str) -> Option<audio::Wav> {
        let mut candidates: Vec<String> = Vec::new();
        if let Some(path) = music::pack_path(name) {
            candidates.push(path);
        }
        let se = format!("se/{}.wav", name.to_ascii_lowercase());
        if !candidates.iter().any(|path| path == &se) {
            candidates.push(se);
        }
        let voice = voice::pack_path(name);
        if !candidates.iter().any(|path| path == &voice) {
            candidates.push(voice);
        }

        for path in &candidates {
            if let Some(wav) = self.wavs.get(path) {
                return Some(wav.clone());
            }
        }
        for path in &candidates {
            if self.missing.contains(path) {
                continue;
            }
            let Ok(bytes) = pack.read(path) else {
                continue;
            };
            match audio::parse_wav(bytes) {
                Ok(wav) => {
                    self.wavs.insert(path.clone(), wav.clone());
                    return Some(wav);
                }
                Err(err) => {
                    eprintln!("warning: invalid BGM track {path}: {err:#}");
                }
            }
        }
        if let Some(first) = candidates.first() {
            eprintln!("warning: missing BGM track {first}");
        }
        for path in candidates {
            self.missing.insert(path);
        }
        None
    }
}

/// Apply the per-room BGM state machine for room `id`, entered from `from`.
///
/// `from` is the room the player came from (the original's
/// `g_AttractMode_RoomCameraId`); `None` for a direct boot, new game or save
/// load, where there is no outgoing room.
pub fn update_room_bgm(game: &mut GameState, id: RoomId, from: Option<RoomId>) {
    let stage = usize::from(id.stage.saturating_sub(1));
    let room = usize::from(id.room);
    let Some(&target) = game.room_bgm.get(stage * 32 + room) else {
        return;
    };
    let previous = game.bgm.state;

    if target == 0xFF {
        if previous != 0xFF {
            fade_out_all(game);
        }
        game.bgm.target = 0xFF;
        game.bgm.state = 0xFF;
        return;
    }

    let new_group = music::group_for(stage, room, target);
    let old_group =
        from.and_then(|from| music::group_for(stage, usize::from(from.room), previous as u8));

    // A different track is coming and something is playing: tear the old banks
    // down before the switch below.
    if previous != 0xFF && previous & 0x38 != 0 && new_group != old_group {
        fade_out_all(game);
    }

    // Type 2 consumes bit 7 before every other use.
    let effective = if target >> 6 == 2 {
        target & 0x7F
    } else {
        target
    };
    let mut start_secondary = false;
    match target >> 6 {
        // Load and start. Nothing was playing or the group changed reloads;
        // the same group keeps its banks and only toggles changed channels.
        0 => {
            if previous == 0xFF || new_group != old_group {
                if previous != 0xFF {
                    fade_out_all(game);
                }
                load_and_start(game, id, effective);
                start_secondary = true;
            } else {
                let changed = u16::from(effective) ^ previous;
                for index in 0..BGM_CHANNELS {
                    let bit = 8u16 << index;
                    if changed & bit == 0 {
                        continue;
                    }
                    if game.bgm.channels[index].name.is_none() {
                        continue;
                    }
                    if u16::from(effective) & bit != 0 {
                        game.bgm.channels[index].restart = true;
                    }
                    // A cleared bit is stopped by `apply_live`; only `bgm_stop`
                    // resets the channel's volume.
                }
            }
        }
        // Always reload, never auto-start.
        1 => {
            fade_out_all(game);
            load_and_start(game, id, effective);
        }
        // Force restart and auto-start.
        2 => {
            fade_out_all(game);
            load_and_start(game, id, effective);
            start_secondary = true;
        }
        // Type 3 is unused.
        _ => {}
    }

    if start_secondary {
        start_secondary_slots(game, effective);
    }
    game.bgm.target = effective;
    game.bgm.state = u16::from(effective);
}

/// Load the group's three channel banks for `state` (an already-decoded game
/// state byte, with the type bits still present).
///
/// Every bank is destroyed first; the group's track names and whole-buffer
/// loop flags come from the existing music tables, and the three tracks the
/// original loads muted start at `-9999` millibels. The stage-2 room-7
/// special case drops the third channel unless Jill's exact flag combination
/// holds.
fn load_and_start(game: &mut GameState, id: RoomId, state: u8) {
    let mut slot_count = BGM_CHANNELS;
    if id.stage == 3 && id.room == 0x07 && !underground_third_channel(game, id) {
        slot_count = 2;
    }

    let stage = usize::from(id.stage.saturating_sub(1));
    let room = usize::from(id.room);
    let group = music::group_for(stage, room, state);

    for (index, channel) in game.bgm.channels.iter_mut().enumerate() {
        *channel = BgmChannelState::default();
        if index >= slot_count {
            continue;
        }
        let Some(group) = group else {
            continue;
        };
        let Some(name) = music::GROUP_TRACKS[usize::from(group)][index] else {
            continue;
        };
        *channel = BgmChannelState {
            name: Some(name),
            looping: music::GROUP_LOOPS[usize::from(group)][index],
            volume: if is_muted_seed(name) { -9999 } else { -1 },
            pan: 0,
            pan_pair: (0, 0),
            restart: false,
            pending_load: true,
            stop: false,
        };
    }
}

/// Whether the courtyard underground-entry cutscene keeps its third channel:
/// Jill (`(player & 3) == 1`) with scenario-2 flags `0x5C` and `0x48` set and
/// `0x55` clear. Every other combination drops the channel and never loads
/// `V110_00`.
fn underground_third_channel(game: &GameState, id: RoomId) -> bool {
    if id.stage != 3 || id.room != 0x07 {
        return true;
    }
    let flags = &game.flags[1];
    id.player_flag & 3 == 1 && flags.bit(0x5C) && flags.bit(0x48) && !flags.bit(0x55)
}

/// Start every channel whose enable bit is set in `state`.
fn start_secondary_slots(game: &mut GameState, state: u8) {
    if state == 0xFF {
        return;
    }
    for index in 0..BGM_CHANNELS {
        if state & (8 << index) == 0 {
            continue;
        }
        if game.bgm.channels[index].name.is_some() {
            game.bgm.channels[index].restart = true;
        }
        game.bgm.state |= 8 << index;
    }
}

/// Tear every BGM bank down and stop the voice line, the original's
/// `bgm_fade_out_all` (without its sleep-driven crossfade).
///
/// The mixer banks are only signalled through the
/// [`crate::game::BgmState::stop_all`] edge: [`apply_live`] consumes it and
/// calls `Mixer::stop_all_bgm`, so the script host can keep running without an
/// audio device.
fn fade_out_all(game: &mut GameState) {
    for channel in &mut game.bgm.channels {
        *channel = BgmChannelState::default();
    }
    game.voice.request = None;
    game.voice.stop_requested = true;
    game.clear_voice_playing();
    game.bgm.stop_all = true;
}

/// Reconcile the mixer with the game's BGM/voice state.
///
/// Runs after the scripts each tick and after a room load: banks marked
/// `pending_load` are read from the pack and loaded, `restart` edges seek the
/// mixer channel to sample 0, disabled channels stop, and the scripted volume
/// and pan reach the mixer.
pub fn apply_live(
    mixer: &mut Option<Mixer>,
    game: &mut GameState,
    cache: &mut BgmCache,
    pack: &Pack,
) {
    // The original runs the fade state machine before the decay ramp; both
    // advance once per tick even when no audio device is open, so scripts and
    // tests observe the same counters.
    advance_fade(game);
    advance_ramp(game);

    // Consume the fade-out edge even without a device so it cannot leak into a
    // later session.
    let stop_all = std::mem::take(&mut game.bgm.stop_all);

    let Some(mixer) = mixer.as_mut() else {
        return;
    };
    if stop_all {
        // `bgm_fade_out_all` stopped and destroyed every bank. Without this the
        // channel records are already cleared, so the loop below would leave
        // the old mixer buffers sounding (a 0xFF room would loop forever).
        mixer.stop_all_bgm();
    }
    for index in 0..BGM_CHANNELS {
        let enabled = game.bgm.state & (8 << index) != 0;
        let bank = game.bgm.channels[index];
        if bank.pending_load {
            game.bgm.channels[index].pending_load = false;
            if let Some(name) = bank.name
                && let Some(wav) = cache.load(pack, name)
            {
                mixer.play_bgm_channel(index, wav);
                mixer.set_bgm_channel_volume(index, sfx::volume_gain(bank.volume));
                mixer.set_bgm_channel_pan(index, sfx::pan_from_raw(bank.pan));
                if !enabled {
                    // Loaded but not enabled: keep the buffer, stay silent
                    // until a `bgm_play` (or the enable bit) restarts it.
                    mixer.stop_bgm_channel(index);
                }
            }
        }
        if game.bgm.channels[index].stop {
            game.bgm.channels[index].stop = false;
            mixer.stop_bgm_channel(index);
        }
        if game.bgm.channels[index].restart {
            game.bgm.channels[index].restart = false;
            if enabled {
                mixer.restart_bgm_channel(index);
            }
        }
        if !enabled && mixer.bgm_channel_playing(index) {
            mixer.stop_bgm_channel(index);
        }
        let bank = game.bgm.channels[index];
        if bank.name.is_some() {
            mixer.set_bgm_channel_volume(index, sfx::volume_gain(bank.volume));
            mixer.set_bgm_channel_pan(index, sfx::pan_from_raw(bank.pan));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RoomState;

    fn game_for(id: RoomId) -> GameState {
        GameState::new(id, &RoomState::default())
    }

    /// Store `value` in the room's BGM row (the `room_bgm_state_set` write).
    fn set_row(game: &mut GameState, id: RoomId, value: u8) {
        let stage = usize::from(id.stage - 1);
        game.room_bgm[stage * 32 + usize::from(id.room)] = value;
    }

    fn names(game: &GameState) -> Vec<Option<&'static str>> {
        game.bgm
            .channels
            .iter()
            .map(|channel| channel.name)
            .collect()
    }

    fn mark_loaded(game: &mut GameState, name: &'static str) {
        game.bgm.channels[0] = BgmChannelState {
            name: Some(name),
            looping: true,
            volume: 0,
            pan: 0,
            pan_pair: (0, 0),
            restart: false,
            pending_load: false,
            stop: false,
        };
    }

    #[test]
    fn type_zero_first_load_starts_the_enabled_channel() {
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        // Group entry 0 of room 1000 is Bgm_13's group 0x0B.
        set_row(&mut game, id, 0x08);
        update_room_bgm(&mut game, id, None);
        assert_eq!(game.bgm.state, 0x08);
        assert_eq!(names(&game)[0], Some("Bgm_13"));
        assert!(game.bgm.channels[0].pending_load);
        assert!(game.bgm.channels[0].restart, "bit 3 starts the channel");
        assert!(game.bgm.channels[0].looping);
        assert_eq!(game.bgm.channels[0].volume, -1);
        assert!(game.bgm.channels[1].name.is_none());
    }

    #[test]
    fn type_zero_same_group_toggles_only_changed_channels() {
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        set_row(&mut game, id, 0x10);
        game.bgm.state = 0x08;
        mark_loaded(&mut game, "Bgm_13");
        // `from` is the same room, so the resolved group is unchanged.
        update_room_bgm(&mut game, id, Some(id));
        assert_eq!(game.bgm.state, 0x10);
        assert_eq!(game.bgm.channels[0].name, Some("Bgm_13"));
        assert!(!game.bgm.channels[0].restart, "channel 0's bit cleared");
        assert!(!game.bgm.channels[0].pending_load, "no reload happened");
        assert!(
            game.bgm.channels[1].name.is_none(),
            "channel 1 was not loaded"
        );
    }

    #[test]
    fn group_change_fades_and_reloads() {
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        // Room 1060's entry 0 is group 0x00, a different group from 1000's
        // 0x0B; the previous live state had channel 0 running.
        let from = RoomId::parse("1060").unwrap();
        set_row(&mut game, id, 0x08);
        game.bgm.state = 0x08;
        mark_loaded(&mut game, "Bgm_00");
        update_room_bgm(&mut game, id, Some(from));
        assert_eq!(game.bgm.state, 0x08);
        assert_eq!(
            game.bgm.channels[0].name,
            Some("Bgm_13"),
            "the new group's bank replaced the old one"
        );
        assert!(game.bgm.channels[0].pending_load);
        assert!(game.bgm.channels[0].restart);
    }

    #[test]
    fn type_one_loads_without_starting() {
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        set_row(&mut game, id, 0x40);
        update_room_bgm(&mut game, id, None);
        assert_eq!(game.bgm.state, 0x40);
        assert_eq!(names(&game)[0], Some("Bgm_13"));
        assert!(game.bgm.channels[0].pending_load);
        assert!(
            !game.bgm.channels[0].restart,
            "type 1 never auto-starts a channel"
        );
    }

    #[test]
    fn type_two_clears_bit_seven_and_starts() {
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        // 0x88: type 2, channel 0; the low state is 0x08 after bit 7 drops.
        set_row(&mut game, id, 0x88);
        game.bgm.state = 0xFF;
        update_room_bgm(&mut game, id, None);
        assert_eq!(game.bgm.state, 0x08);
        assert_eq!(names(&game)[0], Some("Bgm_13"));
        assert!(game.bgm.channels[0].restart);
    }

    #[test]
    fn type_three_is_inert_but_stores_the_state() {
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        set_row(&mut game, id, 0xC0);
        game.bgm.state = 0xFF;
        update_room_bgm(&mut game, id, None);
        assert_eq!(game.bgm.state, 0xC0);
        assert!(
            game.bgm
                .channels
                .iter()
                .all(|channel| channel.name.is_none())
        );
    }

    #[test]
    fn ff_target_fades_and_destroys() {
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        set_row(&mut game, id, 0xFF);
        game.bgm.state = 0x08;
        mark_loaded(&mut game, "Bgm_13");
        update_room_bgm(&mut game, id, Some(id));
        assert_eq!(game.bgm.state, 0xFF);
        assert!(
            game.bgm
                .channels
                .iter()
                .all(|channel| channel.name.is_none())
        );
    }

    #[test]
    fn ff_target_when_already_silent_stays_ff() {
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        set_row(&mut game, id, 0xFF);
        game.bgm.state = 0xFF;
        update_room_bgm(&mut game, id, None);
        assert_eq!(game.bgm.state, 0xFF);
    }

    #[test]
    fn load_loops_follow_the_group_table() {
        // Room 1040's entry 1 is group 0x0D: Bgm_10 (non-loop), Bgm_11 (loop),
        // Bgm_12 (non-loop). State 0x09 is type 0 with channel 0.
        let id = RoomId::parse("1040").unwrap();
        let mut game = game_for(id);
        set_row(&mut game, id, 0x09);
        update_room_bgm(&mut game, id, None);
        assert_eq!(
            names(&game),
            vec![Some("Bgm_10"), Some("Bgm_11"), Some("Bgm_12")]
        );
        let loops: Vec<bool> = game
            .bgm
            .channels
            .iter()
            .map(|channel| channel.looping)
            .collect();
        assert_eq!(loops, vec![false, true, false]);
    }

    #[test]
    fn muted_seeds_load_at_minus_9999() {
        // Room 1030's entry 0 is group 0x00: Bgm_00, Se_01, Bgm_05.
        let id = RoomId::parse("1030").unwrap();
        let mut game = game_for(id);
        set_row(&mut game, id, 0x08);
        update_room_bgm(&mut game, id, None);
        assert_eq!(
            names(&game),
            vec![Some("Bgm_00"), Some("Se_01"), Some("Bgm_05")]
        );
        assert_eq!(game.bgm.channels[0].volume, -1);
        assert_eq!(game.bgm.channels[1].volume, -9999);
        assert_eq!(game.bgm.channels[2].volume, -1);
    }

    #[test]
    fn muted_seeds_fade_up_on_the_scripted_volume_pair() {
        use crate::game::ScdGameHost;
        use crate::scd::host::ScdHost;
        use crate::scd::ir::Operand;
        use crate::scd::opcode::command_op;

        // Room 1050's shipped state 0x10 starts group 0 on channel 1, whose
        // track is the muted Se_01 seed.
        let id = RoomId::parse("1050").unwrap();
        let mut game = game_for(id);
        update_room_bgm(&mut game, id, None);
        assert_eq!(game.bgm.channels[1].name, Some("Se_01"));
        assert_eq!(game.bgm.channels[1].volume, -9999, "the seed loads muted");

        // The script pairs a `snd_pan_vol_set` set on channel 1; it must replace the
        // seed's -9999 with the pan pair's millibels.
        {
            let mut host = ScdGameHost::new(&mut game);
            let operands: Vec<Operand> = [1i64, 85, 85]
                .iter()
                .map(|&value| Operand {
                    value,
                    target: None,
                })
                .collect();
            host.on_sound(command_op(0x2F).unwrap(), &operands);
        }
        let volume = game.bgm.channels[1].volume;
        assert_eq!(volume, sfx::pan_volume(85, 85));
        assert!(
            sfx::volume_gain(volume) > sfx::volume_gain(-9999),
            "the seed's gain rose from silence"
        );
        assert_eq!(game.bgm.channels[1].pan_pair, (85, 85));
    }

    fn courtyard_game(player: u8, flags: &[u8]) -> (RoomId, GameState) {
        let id = RoomId {
            stage: 3,
            room: 0x07,
            player_flag: player,
        };
        let mut game = game_for(id);
        for &flag in flags {
            game.flags[1].apply(flag, 0);
        }
        set_row(&mut game, id, 0x08);
        (id, game)
    }

    #[test]
    fn stage_two_room_seven_keeps_the_third_channel_for_jill_only() {
        // Chris never keeps it.
        let (id, mut chris) = courtyard_game(0, &[0x48, 0x5C]);
        update_room_bgm(&mut chris, id, None);
        assert_eq!(chris.bgm.channels[0].name, Some("Se_44"));
        assert_eq!(chris.bgm.channels[1].name, Some("Bgm_3a"));
        assert!(chris.bgm.channels[2].name.is_none());

        // Jill without both flags drops it.
        let (id, mut jill) = courtyard_game(1, &[0x48]);
        update_room_bgm(&mut jill, id, None);
        assert!(jill.bgm.channels[2].name.is_none());

        // Jill with 0x55 set drops it too.
        let (id, mut jill) = courtyard_game(1, &[0x48, 0x5C, 0x55]);
        update_room_bgm(&mut jill, id, None);
        assert!(jill.bgm.channels[2].name.is_none());

        // Jill with 0x48 and 0x5C set and 0x55 clear keeps V110_00.
        let (id, mut jill) = courtyard_game(1, &[0x48, 0x5C]);
        update_room_bgm(&mut jill, id, None);
        assert_eq!(jill.bgm.channels[2].name, Some("V110_00"));
        assert!(!jill.bgm.channels[2].looping);
        assert!(jill.bgm.channels[2].pending_load);
    }

    /// An empty on-disk pack for driving `apply_live`'s counter advances.
    fn empty_pack(name: &str) -> (std::path::PathBuf, Pack) {
        let path =
            std::env::temp_dir().join(format!("arklay-bgm-{}-{}.akpak", std::process::id(), name));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        crate::pack::PackWriter::new().write(&path).unwrap();
        let pack = Pack::open(&path).unwrap();
        (path, pack)
    }

    #[test]
    fn volume_ramp_steps_towards_silence_and_stops() {
        let (_path, pack) = empty_pack("ramp");
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        game.bgm.state = 0x10;
        game.bgm.channels[1].name = Some("Se_01");
        game.bgm.channels[1].volume = -9999;
        start_volume_ramp(&mut game, 1, 95, 30);
        assert_eq!(game.bgm.ramp.direction, (95 / 30) * 0x4E);
        assert_eq!(game.bgm.ramp.frames_left, 60);
        let mut cache = BgmCache::default();
        apply_live(&mut None, &mut game, &mut cache, &pack);
        assert!(
            game.bgm.channels[1].volume > -9999,
            "the first ramp tick raises the seed"
        );
        assert_eq!(game.bgm.ramp.frames_left, 59);

        // A ramp that passes silence stops the channel and resets the volume.
        game.bgm.channels[1].volume = -9999;
        start_volume_ramp(&mut game, 1, -95, 1);
        apply_live(&mut None, &mut game, &mut cache, &pack);
        assert!(!game.bgm.ramp.active());
        assert_eq!(game.bgm.channels[1].volume, -1);
        assert!(game.bgm.channels[1].stop);
    }

    #[test]
    fn snd_fade_steps_the_channels_down_then_stops_them() {
        let (_path, pack) = empty_pack("fade");
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        game.bgm.channels[0].name = Some("Bgm_13");
        game.bgm.channels[0].volume = -1;
        game.bgm.channels[1].name = Some("Se_01");
        game.bgm.channels[1].volume = -1;
        game.bgm.state = 0x08 | 0x10;
        build_snd_fade_tbl(&mut game, -5);
        assert_eq!(game.bgm.fade.dist_steps, -5 * 0x4E);
        assert_eq!(game.bgm.fade.kind, 0x7F);
        assert_eq!(game.bgm.fade.steps[0], (-10000 + 1) / (-5 * 0x4E));
        assert_eq!(game.bgm.fade.steps[1], game.bgm.fade.steps[0]);
        assert_eq!(game.bgm.fade.steps[2], 0, "channel 2 has no bank");

        let mut cache = BgmCache::default();
        let steps = game.bgm.fade.steps[0];
        for _ in 0..steps {
            apply_live(&mut None, &mut game, &mut cache, &pack);
        }
        assert_eq!(
            game.bgm.channels[0].volume,
            (-1 - steps * 5 * 0x4E).max(-9999),
            "the fade stepped the channel down"
        );
        assert_eq!(game.bgm.fade.steps[0], 0);
        assert_eq!(game.bgm.fade.kind, -30, "the teardown countdown started");

        // The countdown stops the banks and finally clears them.
        for _ in 0..30 {
            apply_live(&mut None, &mut game, &mut cache, &pack);
        }
        assert!(!game.bgm.fade.active());
        assert!(game.bgm.channels.iter().all(|bank| bank.name.is_none()));
    }

    #[test]
    fn bgm_stop_all_stops_banks_and_voice() {
        use crate::game::ScdGameHost;
        use crate::scd::host::ScdHost;
        use crate::scd::ir::Operand;
        use crate::scd::opcode::command_op;

        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);
        set_row(&mut game, id, 0x08);
        update_room_bgm(&mut game, id, None);
        game.voice.request = Some(crate::game::VoiceRequest {
            name: "V004_00",
            volume: 0,
            pan: 0,
        });
        game.set_voice_playing();
        let operands = [Operand {
            value: 0,
            target: None,
        }];
        {
            let mut host = ScdGameHost::new(&mut game);
            host.on_sound(command_op(0x4B).unwrap(), &operands);
        }
        assert_eq!(game.bgm.state, 0x08 << 8, "the mask moved to the high byte");
        assert!(!game.voice_playing(), "the voice wait flag cleared");
        assert!(game.voice.request.is_none());
        assert!(game.voice.stop_requested);
    }

    #[test]
    fn ff_room_target_stops_the_mixer_banks() {
        let (_path, pack) = empty_pack("ff-stop");
        let id = RoomId::parse("1000").unwrap();
        let mut game = game_for(id);

        let _ = unsafe {
            sdl3_sys::hints::SDL_SetHint(sdl3_sys::hints::SDL_HINT_AUDIO_DRIVER, c"dummy".as_ptr())
        };
        let Some(mut mixer) = Mixer::open() else {
            eprintln!("skipping ff-room mixer test: no audio device");
            return;
        };
        mixer.play_bgm_channel(
            0,
            audio::Wav {
                format: audio::WavFormat::S16Le,
                channels: 1,
                sample_rate: 22050,
                data: vec![0, 0, 1, 0, 2, 0],
            },
        );
        assert!(mixer.bgm_channel_playing(0), "the fake bank is sounding");

        // The live state says channel 0 is enabled and loaded, then a 0xFF
        // room target fades everything out.
        game.bgm.state = 0x08;
        game.bgm.channels[0].name = Some("Bgm_13");
        set_row(&mut game, id, 0xFF);
        update_room_bgm(&mut game, id, Some(id));
        assert!(game.bgm.stop_all, "the fade armed the mixer tear-down edge");

        let mut music = Some(mixer);
        let mut cache = BgmCache::default();
        apply_live(&mut music, &mut game, &mut cache, &pack);
        let mixer = music.as_ref().unwrap();
        assert!(
            !mixer.bgm_channel_playing(0),
            "a 0xFF room stops the sounding bank"
        );
        assert!(
            !mixer.bgm_channel_loaded(0),
            "fade_out_all destroys the mixer bank"
        );
        assert!(!game.bgm.stop_all, "the edge is consumed");
    }
}
