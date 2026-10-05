//! M13 real-asset tests: voice lines, the F7 wait and the three-channel BGM.
//!
//! Run with:
//! `ARKLAY_RE1_ROOT=... ARKLAY_RE1_PACK=... cargo test --test m13_real -- --ignored --nocapture`
//!
//! Only an unset environment skips; a partial configuration fails loudly. The
//! conversion test additionally writes a full pack pair and takes minutes.

mod common;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use arklay::bgm;
use arklay::convert::{VoicePackOptions, convert_game_with_voice};
use arklay::game::{BgmState, GameState, ScdGameHost};
use arklay::message::MessageWindow;
use arklay::pack::Pack;
use arklay::save::SaveFile;
use arklay::scd::host::ScdHost;
use arklay::scd::ir::{Decoded, Scripts};
use arklay::scd::reader;
use arklay::scd::vm::{CommandVm, EventVm};
use arklay::state::{RoomId, RoomState};
use arklay::voice;

/// The shipped RDT path for a four-digit room identity.
fn room_path(root: &Path, room: &str) -> PathBuf {
    let id = RoomId::parse(room).unwrap();
    root.join("JPN")
        .join(format!("STAGE{}", id.stage))
        .join(format!("ROOM{room}.RDT"))
}

/// Parse a shipped RDT.
fn room_scripts(root: &Path, room: &str) -> Scripts {
    let bytes = fs::read(room_path(root, room)).unwrap();
    reader::parse(&bytes).unwrap()
}

/// A fresh state with the room's init script run, the way the engine boots it.
fn init_room(root: &Path, room: &str) -> GameState {
    let scripts = room_scripts(root, room);
    let id = RoomId::parse(room).unwrap();
    let mut game = GameState::new(id, &RoomState::default());
    let mut vm = CommandVm::new(&scripts);
    let mut host = ScdGameHost::new(&mut game);
    vm.run_init(&mut host);
    game
}

/// The lowercased `voice/` file stems in the install.
fn voice_files(root: &Path) -> HashSet<String> {
    let dir = root.join("JPN/voice");
    fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("failed to list {}: {error}", dir.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .trim_end_matches(".wav")
                .to_owned()
        })
        .collect()
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_1000s_opening_lines_resolve_to_v004() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let scripts = room_scripts(&root, "1000");
    let mut by_id = std::collections::BTreeMap::new();
    for insn in scripts
        .init
        .iter()
        .flat_map(|block| block.insns.iter())
        .chain(scripts.main.iter().flat_map(|block| block.insns.iter()))
        .chain(scripts.events.iter().flat_map(|stream| stream.insns.iter()))
    {
        let Decoded::Command(op) = insn.decoded else {
            continue;
        };
        if op.op != 0x1E {
            continue;
        }
        let id = insn
            .operands
            .get(1)
            .map_or(0, |operand| operand.value as u16);
        by_id.insert(id, ());
    }
    for id in 9..=17u16 {
        let name = voice::name(0, id).unwrap_or_else(|| panic!("id {id} has no name"));
        assert_eq!(name, format!("V004_{:02x}", id - 9), "id {id}");
        assert!(by_id.contains_key(&id), "ROOM1000 never uses id {id}");
    }
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn every_shipped_xa_on_resolves_to_a_present_file() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let files = voice_files(&root);
    let mut sites = 0usize;
    let mut missing = Vec::new();
    for stage in 1..=7u8 {
        let dir = root.join("JPN").join(format!("STAGE{stage}"));
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(digits) = name
                .to_ascii_lowercase()
                .strip_prefix("room")
                .and_then(|name| name.strip_suffix(".rdt"))
                .map(str::to_owned)
            else {
                continue;
            };
            let id = RoomId::parse(&digits).unwrap();
            let bytes = fs::read(entry.path()).unwrap();
            let Ok(scripts) = reader::parse(&bytes) else {
                continue;
            };
            for insn in scripts
                .init
                .iter()
                .flat_map(|block| block.insns.iter())
                .chain(scripts.main.iter().flat_map(|block| block.insns.iter()))
                .chain(scripts.events.iter().flat_map(|stream| stream.insns.iter()))
            {
                let Decoded::Command(op) = insn.decoded else {
                    continue;
                };
                if op.op != 0x1E {
                    continue;
                }
                let id_voice = insn
                    .operands
                    .get(1)
                    .map_or(0, |operand| operand.value as u16);
                let stage0 = id.stage - 1;
                match voice::name(stage0, id_voice) {
                    Some(name) if files.contains(&name.to_ascii_lowercase()) => sites += 1,
                    Some(name) => missing.push(format!("{name} (room {digits} id {id_voice})")),
                    None => missing.push(format!(
                        "empty (room {digits} stage {stage0} id {id_voice})"
                    )),
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} unresolved xa_on site(s): {:?}",
        missing.len(),
        missing
    );
    assert_eq!(sites, 710, "the shipped corpus has 710 xa_on sites");
    println!("resolved {sites} xa_on sites");
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_1000_stalls_on_f7_while_a_line_plays() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let scripts = room_scripts(&root, "1000");
    let id = RoomId::parse("1000").unwrap();

    // The scripted run: the first xa_on raises the wait bit, and the F7 that
    // follows must hold until the engine clears it.
    let mut game = GameState::new(id, &RoomState::default());
    {
        let mut vm = CommandVm::new(&scripts);
        let mut host = ScdGameHost::new(&mut game);
        vm.run_init(&mut host);
    }
    let mut event_vm = EventVm::new(&scripts);
    event_vm.start(0, 0);
    let mut first = None;
    for _ in 0..400 {
        {
            let mut host = ScdGameHost::new(&mut game);
            event_vm.step(&mut host);
        }
        if let Some(request) = game.voice.request {
            first = Some(request.name);
            break;
        }
    }
    assert_eq!(first, Some("V004_00"), "the first line must be V004_00");
    assert!(game.voice_playing(), "xa_on raised the wait bit");

    // Held: the next xa_on must not replace the active line.
    for _ in 0..60 {
        let mut host = ScdGameHost::new(&mut game);
        event_vm.step(&mut host);
    }
    assert_eq!(
        game.voice.request.map(|request| request.name),
        Some("V004_00"),
        "F7 held the script while the line played"
    );

    // Released: the script advances to the next line. The message the script
    // opened at the same time is dismissed too (the player's confirm).
    game.clear_voice_playing();
    game.message = MessageWindow::default();
    let mut second = None;
    for _ in 0..200 {
        {
            let mut host = ScdGameHost::new(&mut game);
            event_vm.step(&mut host);
        }
        if game.voice.request.map(|request| request.name) != Some("V004_00")
            && game.voice.request.is_some()
        {
            second = game.voice.request.map(|request| request.name);
            break;
        }
    }
    assert_eq!(second, Some("V004_01"), "the wait released the next line");
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn a_voice_less_run_advances_past_the_first_wait() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let scripts = room_scripts(&root, "1000");
    let id = RoomId::parse("1000").unwrap();
    let mut game = GameState::new(id, &RoomState::default());
    {
        let mut vm = CommandVm::new(&scripts);
        let mut host = ScdGameHost::new(&mut game);
        vm.run_init(&mut host);
    }
    let mut event_vm = EventVm::new(&scripts);
    event_vm.start(0, 0);
    // The engine's no-device tick releases the wait every frame; emulate it by
    // clearing the bit after each step.
    let mut names = Vec::new();
    for _ in 0..600 {
        {
            let mut host = ScdGameHost::new(&mut game);
            event_vm.step(&mut host);
        }
        if let Some(request) = game.voice.request
            && names.last() != Some(&request.name)
        {
            names.push(request.name);
        }
        game.clear_voice_playing();
        // A message can hold the same F7 through the menu-choice bit; dismiss
        // it like the player would so only the voice wait remains.
        game.message = MessageWindow::default();
        if names.len() >= 2 {
            break;
        }
    }
    assert_eq!(
        names,
        vec!["V004_00", "V004_01"],
        "a voice-less run reaches the second line"
    );
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn the_courtyard_group_loads_v110_00_as_the_third_channel() {
    let Some((_root, _pack)) = common::asset_env() else {
        return;
    };
    let chris = RoomId::parse("3070").unwrap();
    let mut game = GameState::new(chris, &RoomState::default());
    // The room's own table row points at group 0x26 for entry 0.
    game.room_bgm[2 * 32 + 0x07] = 0x08;
    bgm::update_room_bgm(&mut game, chris, None);
    assert_eq!(game.bgm.channels[0].name, Some("Se_44"));
    assert_eq!(game.bgm.channels[1].name, Some("Bgm_3a"));
    assert!(game.bgm.channels[2].name.is_none(), "Chris drops channel 3");

    let jill = RoomId::parse("3071").unwrap();
    let mut game = GameState::new(jill, &RoomState::default());
    game.room_bgm[2 * 32 + 0x07] = 0x08;
    game.flags[1].apply(0x48, 0);
    game.flags[1].apply(0x5C, 0);
    bgm::update_room_bgm(&mut game, jill, None);
    assert_eq!(game.bgm.channels[2].name, Some("V110_00"));
    assert!(!game.bgm.channels[2].looping);
    assert!(game.bgm.channels[2].pending_load);
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn rooms_1000_and_1001_share_the_bgm_group_without_a_restart() {
    let Some((_root, _pack)) = common::asset_env() else {
        return;
    };
    let chris = RoomId::parse("1000").unwrap();
    let jill = RoomId::parse("1001").unwrap();
    let mut game = GameState::new(chris, &RoomState::default());
    // Both variants share room 0's row; a type-0 state on group entry 0 is
    // Bgm_13. Once loaded, crossing to the sibling variant keeps the bank.
    game.room_bgm[0] = 0x08;
    bgm::update_room_bgm(&mut game, chris, None);
    assert_eq!(game.bgm.channels[0].name, Some("Bgm_13"));
    assert!(game.bgm.channels[0].pending_load);
    // Simulate the engine having loaded and started the bank.
    game.bgm.channels[0].pending_load = false;
    game.bgm.channels[0].restart = false;

    bgm::update_room_bgm(&mut game, jill, Some(chris));
    assert_eq!(game.bgm.state, 0x08);
    assert_eq!(game.bgm.channels[0].name, Some("Bgm_13"));
    assert!(
        !game.bgm.channels[0].pending_load,
        "the same group does not reload the bank"
    );
    assert!(!game.bgm.channels[0].restart, "and does not restart it");
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn room_10f_toggles_its_scripted_bgm_channels() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let scripts = room_scripts(&root, "10F0");
    let id = RoomId::parse("10F0").unwrap();
    let mut game = GameState::new(id, &RoomState::default());
    {
        let mut vm = CommandVm::new(&scripts);
        let mut host = ScdGameHost::new(&mut game);
        vm.run_init(&mut host);
    }
    bgm::update_room_bgm(&mut game, id, None);

    // The room's scripts drive all three channels: 0, 1 and 2 are played and
    // 0/2 are stopped.
    let mut plays = HashSet::new();
    let mut stops = HashSet::new();
    let mut sequence = Vec::new();
    for insn in scripts
        .init
        .iter()
        .flat_map(|block| block.insns.iter())
        .chain(scripts.main.iter().flat_map(|block| block.insns.iter()))
        .chain(scripts.events.iter().flat_map(|stream| stream.insns.iter()))
    {
        let Decoded::Command(op) = insn.decoded else {
            continue;
        };
        if op.op != 0x15 && op.op != 0x16 {
            continue;
        }
        let channel = insn.operands.first().map_or(0, |operand| operand.value) as u8;
        if op.op == 0x15 {
            plays.insert(channel);
        } else {
            stops.insert(channel);
        }
        sequence.push((op.op, channel));
    }
    assert!(plays.contains(&0) && plays.contains(&1) && plays.contains(&2));
    assert!(stops.contains(&0) && stops.contains(&2));

    // Replay the scripted sequence: every play sets its channel's enable bit
    // and every stop clears it, exactly like the live host handlers.
    for (opcode, channel) in sequence {
        let bit = 1u16 << (u32::from(channel) + 3);
        {
            let mut host = ScdGameHost::new(&mut game);
            let values = [i64::from(channel)];
            let operands: Vec<arklay::scd::ir::Operand> = values
                .iter()
                .map(|&value| arklay::scd::ir::Operand {
                    value,
                    target: None,
                })
                .collect();
            let op = arklay::scd::opcode::command_op(opcode).unwrap();
            host.on_sound(op, &operands);
        }
        if opcode == 0x15 {
            assert_ne!(game.bgm.state & bit, 0, "bgm_play({channel})");
        } else {
            assert_eq!(game.bgm.state & bit, 0, "bgm_stop({channel})");
        }
    }
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
fn a_save_after_tbl37_set_reloads_the_modified_table() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    // ROOM1001's init performs a `tbl37_set(0, 0, 9)`; the live table must be
    // what the save captures, not the shipped constant at that slot.
    let game = init_room(&root, "1001");
    assert_eq!(game.room_bgm[0], 9, "the init script's write is live");
    assert_ne!(game.room_bgm[0], arklay::music::ROOM_STATE[0]);

    let file = SaveFile::from_state(&game);
    assert_eq!(file.room_bgm[0], 9);
    let parsed = SaveFile::from_bytes(&file.to_bytes()).unwrap();
    let mut restored = GameState::default();
    parsed.apply_to(&mut restored);
    assert_eq!(restored.room_bgm[0], 9);
    assert_eq!(restored.bgm, BgmState::default());
}

#[test]
#[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT; writes ~470 MiB"]
fn conversion_writes_the_referenced_voice_pack() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };
    let out = std::env::temp_dir().join(format!("arklay-m13-{}.akpak", std::process::id()));
    let voice_out =
        std::env::temp_dir().join(format!("arklay-m13-{}.voice.akpak", std::process::id()));
    let _ = fs::remove_file(&out);
    let _ = fs::remove_file(&voice_out);

    convert_game_with_voice(
        &root,
        &out,
        None,
        0,
        &VoicePackOptions::Sibling(Some(voice_out.clone())),
    )
    .unwrap();

    let pack = Pack::open(&voice_out).unwrap();
    let referenced = voice::referenced_names();
    assert_eq!(
        pack.len(),
        referenced.len(),
        "one voice entry per referenced name"
    );
    let bytes: usize = pack.entries().map(|entry| entry.size()).sum();
    println!("voice pack: {} entries, {bytes} bytes", pack.len());
    let mut missing = Vec::new();
    for name in &referenced {
        let path = voice::pack_path(name);
        if !pack.contains(&path) {
            missing.push(path);
        }
    }
    assert!(missing.is_empty(), "unpacked voice entries: {missing:?}");

    let _ = fs::remove_file(&out);
    let _ = fs::remove_file(&voice_out);
}
