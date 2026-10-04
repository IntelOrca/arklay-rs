//! Diagnostic scan of `door_aot_set` records across the real RDT corpus.
//!
//! Run with:
//! `ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 cargo test --test doors_scan -- --ignored --nocapture`

mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use arklay::game::{GameState, ScdGameHost};
use arklay::scd::ir::{Decoded, Insn, Operand, StreamKind};
use arklay::scd::vm::{CommandVm, EventVm};
use arklay::state::RoomId;

#[derive(Debug, Clone, Copy)]
struct DoorOperands {
    slot: u8,
    zone: [i16; 4],
    direction: u8,
    sfx: u8,
    door_type: u8,
    camera: u8,
    lock: u8,
    next_room: u8,
    next_pos: [i16; 3],
    next_angle: i16,
    key: u8,
    probe: u8,
}

fn operand(operands: &[Operand], index: usize) -> i64 {
    operands.get(index).map(|op| op.value).unwrap_or(0)
}

fn door_operands(insn: &Insn) -> DoorOperands {
    let o = &insn.operands;
    DoorOperands {
        slot: operand(o, 0) as u8,
        zone: [
            operand(o, 1) as i16,
            operand(o, 2) as i16,
            operand(o, 3) as i16,
            operand(o, 4) as i16,
        ],
        direction: operand(o, 5) as u8,
        sfx: operand(o, 6) as u8,
        door_type: operand(o, 7) as u8,
        camera: operand(o, 8) as u8,
        lock: operand(o, 9) as u8,
        next_room: operand(o, 10) as u8,
        next_pos: [
            operand(o, 11) as i16,
            operand(o, 12) as i16,
            operand(o, 13) as i16,
        ],
        next_angle: operand(o, 14) as i16,
        key: operand(o, 15) as u8,
        probe: operand(o, 16) as u8,
    }
}

fn collect(insns: &[Insn], section: &str, out: &mut Vec<(String, usize, DoorOperands)>) {
    for insn in insns {
        if let Decoded::Command(op) = &insn.decoded
            && op.op == 0x0C
        {
            out.push((section.to_string(), insn.offset, door_operands(insn)));
        }
    }
}

fn rdt_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for stage in 1..=7 {
        let dir = root.join(format!("JPN/STAGE{stage}"));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut stage_paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("rdt"))
            })
            .collect();
        stage_paths.sort();
        paths.extend(stage_paths);
    }
    paths
}

fn room_id_from_path(path: &Path) -> Option<RoomId> {
    let stem = path.file_stem()?.to_str()?;
    let digits = stem.strip_prefix("ROOM")?;
    RoomId::parse(&digits.to_ascii_lowercase()).ok()
}

/// Run init and a few main ticks + events, the way the engine drives a room.
fn run_room(scripts: &arklay::scd::ir::Scripts, state: &mut GameState) {
    let mut vm = CommandVm::new(scripts);
    {
        let mut host = ScdGameHost::new(state);
        vm.run_init(&mut host);
    }
    let mut event_vm = EventVm::new(scripts);
    for _ in 0..30 {
        {
            let mut host = ScdGameHost::new(state);
            vm.run_main(&mut host);
        }
        for (slot, event) in std::mem::take(&mut state.pending_events) {
            event_vm.start(usize::from(slot), event);
        }
        {
            let mut host = ScdGameHost::new(state);
            event_vm.step(&mut host);
        }
        state.advance_frame();
    }
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn scan_doors_in_the_corpus() {
    let Some((root, _pack)) = common::asset_env() else {
        return;
    };

    let mut lock_hist: BTreeMap<u8, u32> = BTreeMap::new();
    let mut key_hist: BTreeMap<u8, u32> = BTreeMap::new();
    let mut probe_hist: BTreeMap<u8, u32> = BTreeMap::new();
    let mut type_hist: BTreeMap<u8, u32> = BTreeMap::new();
    let mut next_hist: BTreeMap<u8, u32> = BTreeMap::new();
    let mut slot_hist: BTreeMap<u8, u32> = BTreeMap::new();
    let mut aot_handler_hist: BTreeMap<u8, u32> = BTreeMap::new();
    let mut item_flags_hist: BTreeMap<u8, u32> = BTreeMap::new();
    let mut total_init = 0u32;
    let mut total_main = 0u32;
    let mut total_event = 0u32;
    let mut registered = 0u32;
    let mut would_transition_unlocked = 0u32;
    let mut would_transition_now = 0u32;
    let mut rooms_with_doors = 0u32;
    let mut misses = 0u32;
    let mut failures: Vec<(String, u8, u8, u8, u8, u8)> = Vec::new();
    let mut tsv = String::new();
    let _ = writeln!(
        tsv,
        "room\tsection\toffset\tslot\tzx\tzz\tzw\tzd\tdir\tsfx\ttype\tcam\tlock\tdest\tpx\tpy\tpz\tangle\tkey\tprobe\tregistered\ttransitions"
    );

    for path in rdt_paths(&root) {
        let Some(id) = room_id_from_path(&path) else {
            continue;
        };
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let Ok(room) = arklay::rdt::parse(&data, id) else {
            continue;
        };
        let Ok(scripts) = arklay::scd::reader::parse(&data) else {
            continue;
        };

        let mut found: Vec<(String, usize, DoorOperands)> = Vec::new();
        for block in &scripts.init {
            collect(&block.insns, "init", &mut found);
        }
        for block in &scripts.main {
            collect(&block.insns, "main", &mut found);
        }
        for stream in &scripts.events {
            let section = match stream.kind {
                StreamKind::Event(index) => format!("event{index}"),
                StreamKind::Init => "init".to_string(),
                StreamKind::Main => "main".to_string(),
            };
            collect(&stream.insns, &section, &mut found);
        }
        let n_init = found.iter().filter(|(s, _, _)| s == "init").count() as u32;
        let n_main = found.iter().filter(|(s, _, _)| s == "main").count() as u32;
        let n_event = found.len() as u32 - n_init - n_main;
        total_init += n_init;
        total_main += n_main;
        total_event += n_event;
        if found.is_empty() {
            continue;
        }
        rooms_with_doors += 1;

        let mut state = GameState::new(id, &room);
        run_room(&scripts, &mut state);

        let doors: Vec<_> = (0..20u8)
            .filter_map(|slot| state.doors[usize::from(slot)].map(|door| (slot, door)))
            .collect();
        registered += doors.len() as u32;

        for (_, door) in &doors {
            *lock_hist.entry(door.lock).or_default() += 1;
            *key_hist.entry(door.key).or_default() += 1;
            *probe_hist.entry(door.sub_type).or_default() += 1;
            *next_hist.entry(door.next_room).or_default() += 1;
        }
        for (_, _, o) in &found {
            *slot_hist.entry(o.slot).or_default() += 1;
            *type_hist.entry(o.door_type).or_default() += 1;
        }
        for insn in scripts
            .init
            .iter()
            .chain(&scripts.main)
            .flat_map(|block| &block.insns)
        {
            if let Decoded::Command(op) = &insn.decoded {
                if op.op == 0x0D {
                    *aot_handler_hist
                        .entry(operand(&insn.operands, 5) as u8)
                        .or_default() += 1;
                }
                if op.op == 0x18 {
                    *item_flags_hist
                        .entry(operand(&insn.operands, 14) as u8)
                        .or_default() += 1;
                }
            }
        }

        // A site is registered when a door with the same slot and next_pos
        // exists in the state.
        for (section, offset, o) in &found {
            let is_registered = state.doors[usize::from(o.slot)].is_some();
            if !is_registered {
                misses += 1;
            }
            let mut unlocked = state.clone();
            locked_open_flags(&mut unlocked, o.lock);
            let transitions = unlocked.try_door(o.slot)
                && unlocked
                    .transition
                    .is_some_and(|t| same_destination(&t, id, o));
            if section == "init" && !transitions {
                // Detail later.
            }
            let _ = writeln!(
                tsv,
                "{}\t{}\t{:#x}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:#04x}\t{:#04x}\t{}\t{}\t{}\t{}\t{}\t{:#04x}\t{:#04x}\t{}\t{}",
                id.room3(),
                section,
                offset,
                o.slot,
                o.zone[0],
                o.zone[1],
                o.zone[2],
                o.zone[3],
                o.direction,
                o.sfx,
                o.door_type,
                o.camera,
                o.lock,
                o.next_room,
                o.next_pos[0],
                o.next_pos[1],
                o.next_pos[2],
                o.next_angle,
                o.key,
                o.probe,
                is_registered,
                transitions
            );
        }

        for (slot, _) in &doors {
            let mut probe_state = state.clone();
            // Open any lock flag so the lock itself is not the blocker, then
            // probe twice: a key turn only unlocks on the first probe.
            if let Some(action) = probe_state.doors[usize::from(*slot)] {
                locked_open_flags(&mut probe_state, action.lock);
                let open_here = probe_state.try_door(*slot) || probe_state.try_door(*slot);
                if open_here {
                    would_transition_unlocked += 1;
                } else {
                    let door = action;
                    failures.push((
                        id.room3(),
                        *slot,
                        door.next_room,
                        door.lock,
                        door.key,
                        door.sub_type,
                    ));
                }
            }
            if state.doors[usize::from(*slot)].is_some_and(|door| door.lock & 0x80 == 0) {
                let mut fresh = state.clone();
                if fresh.try_door(*slot) {
                    would_transition_now += 1;
                }
            }
        }
    }

    println!(
        "door_aot_set sites: init={total_init} main={total_main} event={total_event} total={}",
        total_init + total_main + total_event
    );
    println!(
        "rooms with door sites: {rooms_with_doors}, registered after init+30 mains: {registered}"
    );
    println!("sites whose slot is not registered: {misses}");
    println!(
        "try_door with lock flag forced open: {would_transition_unlocked} would transition; locked-free doors transitioning now: {would_transition_now}"
    );
    println!("slot histogram: {slot_hist:?}");
    println!("aot_set handler histogram: {aot_handler_hist:?}");
    println!("item_aot_set entry-flag histogram: {item_flags_hist:?}");
    println!("lock histogram: {lock_hist:?}");
    println!("key histogram: {key_hist:?}");
    println!("probe histogram: {probe_hist:?}");
    println!("door type histogram: {type_hist:?}");
    println!("next_room histogram: {next_hist:?}");
    failures.sort();
    failures.dedup();
    println!(
        "doors that still fail with the lock forced open: {}",
        failures.len()
    );
    for (room, slot, dest, lock, key, probe) in &failures {
        println!(
            "  room={room} slot={slot} dest={dest:#04x} lock={lock:#04x} key={key:#04x} probe={probe:#04x}"
        );
    }
    let out = std::path::Path::new("target/doors.tsv");
    std::fs::write(out, &tsv).expect("write target/doors.tsv");
    println!("wrote {}", out.display());
}

/// Set the lock's flag so `door_locked` passes for flag-locked doors; 0xFF
/// locks stay locked.
fn locked_open_flags(state: &mut GameState, lock: u8) {
    if lock & 0x80 != 0 && lock != 0xFF {
        let _ = state.apply_flag(2, lock & 0x3F, 0);
    }
}

fn same_destination(
    transition: &arklay::game::RoomTransition,
    source: RoomId,
    operands: &DoorOperands,
) -> bool {
    let (stage, room) = if operands.next_room < 0x20 {
        (source.stage, operands.next_room)
    } else {
        (
            ((operands.next_room >> 5) - 1) + 1,
            operands.next_room & 0x1F,
        )
    };
    transition.target.stage == stage
        && transition.target.room == room
        && transition.pos
            == [
                i32::from(operands.next_pos[0]),
                i32::from(operands.next_pos[1]),
                i32::from(operands.next_pos[2]),
            ]
}
