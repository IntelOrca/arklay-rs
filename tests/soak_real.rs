//! Full-game soak: walk every `room/*.rdt`, follow scripted transitions and
//! round-trip a save per room, three times, under a counting allocator.
//!
//! Run with (release profile; the budgets are documented there):
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 \
//!  ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak \
//!  cargo test --release --test soak_real -- --ignored --nocapture`
//!
//! The synthetic transition test in `src/engine.rs` (the M16 slice-10 test
//! extended with this file's save round-trip) is the always-run version.
//!
//! The soak never decodes a film: `simulate_room_with_input` takes and drops
//! every `movie_on` request, so no AVI pixels are touched.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use arklay::engine;
use arklay::game::{GameState, RoomActionKind};
use arklay::pack::Pack;
use arklay::player;
use arklay::save::SaveFile;
use arklay::state::RoomId;

/// Ticks simulated per room and per transition hop.
const TICKS: usize = 300;
/// Maximum rooms followed from one start room.
const MAX_HOPS: usize = 4;
/// Passes over the corpus: the first warms every lazily built cache, the
/// second and third must not grow live bytes.
const PASSES: usize = 3;
/// Live bytes may move by this much between passes 2 and 3 before it counts as
/// a leak. The number is deliberately an order of magnitude above allocator
/// jitter and the test harness's own buffers.
const MAX_LIVE_DELTA_BYTES: usize = 32 << 20;
/// Extra live bytes (above the pack's own allocation) a pass may peak at.
const MAX_PASS_EXTRA_BYTES: usize = 512 << 20;
/// Documented wall-clock ceiling for one full pass.
const MAX_PASS_SECONDS: u64 = 900;

/// The only placeholder arm the shipped corpus dispatches, recorded in the M16
/// audit and `docs/m17-deviations.md`: opcode `0x05` with an out-of-range flag
/// bank. Any other opcode reaching a placeholder arm fails the soak.
const KNOWN_PLACEHOLDERS: [u8; 1] = [0x05];

/// Typewriter rooms the headless seam samples: the two mansion save rooms
/// `tests/save_real.rs` also drives. Every other room that declares the
/// typewriter action is counted but not driven: its prompt depends on
/// progression state the fresh-game soak does not hold, and the seam's probe
/// loop is the expensive fallback path.
const TYPEWRITER_SAMPLE: [(u8, u8, u8); 2] = [(1, 0, 0), (1, 0, 1)];

/// A counting allocator: live bytes (allocation minus deallocation) and the
/// peak since the last reset. Atomics only, so it works on Linux and Windows.
struct CountingAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn peak_bytes() -> usize {
    PEAK.load(Ordering::Relaxed)
}

fn reset_peak() {
    PEAK.store(live_bytes(), Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Self-deleting temporary directory.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("arklay-soak-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The deterministic per-tick soak schedule: idle, walk, turn, then action
/// while walking, 45 ticks per phase.
fn schedule(tick: usize) -> player::Input {
    const PHASE: usize = 45;
    match (tick / PHASE) % 4 {
        0 => player::Input::default(),
        1 => player::Input {
            up: true,
            ..player::Input::default()
        },
        2 => player::Input {
            left: tick % 8 < 4,
            right: tick % 8 >= 4,
            ..player::Input::default()
        },
        _ => player::Input {
            up: true,
            action_pressed: tick.is_multiple_of(12),
            ..player::Input::default()
        },
    }
}

/// Coverage counters the soak prints.
#[derive(Default)]
struct Coverage {
    starts: usize,
    rooms: usize,
    skipped: usize,
    skipped_ids: std::collections::BTreeSet<(u8, u8, u8)>,
    moved: usize,
    hops: usize,
    transitions: usize,
    door_transitions: usize,
    destinations: BTreeMap<(u8, u8), usize>,
    saves: usize,
    typewriters: usize,
    typewriter_rooms: usize,
    cuts: usize,
    frames: usize,
    placeholders: BTreeMap<u8, u64>,
}

/// Assert one state's save round-trip is stable and reproduces the persisted
/// fields.
fn check_save_round_trip(state: &GameState, label: &str) {
    let file = SaveFile::from_state(state);
    let bytes = file.to_bytes();
    let parsed = SaveFile::from_bytes(&bytes)
        .unwrap_or_else(|err| panic!("{label}: written block does not parse: {err:#}"));
    assert_eq!(
        parsed.to_bytes(),
        bytes,
        "{label}: to_bytes/from_bytes is not idempotent"
    );

    let mut restored = state.clone();
    parsed.apply_to(&mut restored);
    assert_eq!(
        restored.state_bytes, parsed.state_bytes,
        "{label}: the state image did not round-trip"
    );
    assert_eq!(
        restored.inventory, state.inventory,
        "{label}: the inventory did not round-trip"
    );
    assert_eq!(
        restored.item_box, state.item_box,
        "{label}: the item box did not round-trip"
    );
    assert_eq!(
        restored.camera.current_cut,
        usize::from(file.camera),
        "{label}: the camera did not round-trip"
    );
    assert_eq!(
        restored.entities[0].health, state.entities[0].health,
        "{label}: entity[0] health did not round-trip"
    );
    assert_eq!(
        [restored.entities[0].pos[0], restored.entities[0].pos[2]],
        [state.entities[0].pos[0], state.entities[0].pos[2]],
        "{label}: entity[0] position did not round-trip"
    );
    assert_eq!(
        restored.entities[0].angle, state.entities[0].angle,
        "{label}: entity[0] angle did not round-trip"
    );
    assert_eq!(
        &restored.flags[0].bytes()[..16],
        &file.scenario[..],
        "{label}: scenario bank 0 did not round-trip"
    );
    assert_eq!(
        &restored.flags[1].bytes()[..],
        &file.scenario2[..],
        "{label}: scenario bank 2 did not round-trip"
    );
    assert_eq!(
        &restored.flags[2].bytes()[..8],
        &file.locks[..],
        "{label}: the lock bank did not round-trip"
    );
    assert_eq!(
        &restored.flags[3].bytes()[..],
        &file.enemies[..],
        "{label}: the enemy bank did not round-trip"
    );
    assert_eq!(
        &restored.flags[7].bytes()[..],
        &file.room_items[..],
        "{label}: the room-item bank did not round-trip"
    );
    assert_eq!(
        &restored.flags[8].bytes()[..20],
        &file.room_flags[..20],
        "{label}: the room-flag bank did not round-trip"
    );
}

/// What one soak pass over one starting room did.
enum RoomOutcome {
    /// The room could not load (a stub without camera cuts, like the M16
    /// corpus audit skips).
    Skipped,
    /// The room simulated and stayed in place.
    Stayed,
    /// The room simulated and its last scripted door reached `destination`.
    Moved(RoomId),
}

/// Simulate one room and everything the soak asserts about it.
fn soak_room(
    pack: &Pack,
    id: RoomId,
    save_dir: &std::path::Path,
    coverage: &mut Coverage,
) -> RoomOutcome {
    let room_start = Instant::now();
    let sim = match engine::simulate_room_with_input(pack, id, TICKS, schedule) {
        Ok(sim) => sim,
        Err(_) => {
            coverage.skipped += 1;
            coverage
                .skipped_ids
                .insert((id.stage, id.room, id.player_flag));
            return RoomOutcome::Skipped;
        }
    };
    if room_start.elapsed() > Duration::from_secs(60) {
        panic!("ROOM{id:?} exceeded 60 seconds");
    }

    assert!(
        sim.player
            .pos
            .iter()
            .all(|value| (-1_000_000..=1_000_000).contains(value)),
        "ROOM{id:?}: the player position is not finite: {:?}",
        sim.player.pos
    );
    assert_eq!(
        (sim.frame.width, sim.frame.height),
        (320, 240),
        "ROOM{id:?}: the rendered frame is not 320x240"
    );
    for (&op, &count) in &sim.game.placeholders {
        *coverage.placeholders.entry(op).or_default() += count;
        assert!(
            KNOWN_PLACEHOLDERS.contains(&op),
            "ROOM{id:?} dispatched a new placeholder opcode {op:#04x} ({count} times)"
        );
    }

    check_save_round_trip(&sim.game, &format!("ROOM{id:?}"));
    coverage.saves += 1;
    coverage.rooms += 1;
    coverage.cuts += sim.room.cuts.len();
    coverage.frames += 1;

    let declares_typewriter = sim
        .game
        .room_actions
        .iter()
        .flatten()
        .any(|action| action.kind == RoomActionKind::Typewriter);
    if declares_typewriter {
        coverage.typewriter_rooms += 1;
    }
    if declares_typewriter && TYPEWRITER_SAMPLE.contains(&(id.stage, id.room, id.player_flag)) {
        // A fresh slot directory per attempt: a reused directory would raise
        // the overwrite prompt, which the headless typewriter seam does not
        // drive.
        let room_save_dir = save_dir.join(format!("{:04x}", id.rdt_number()));
        let _ = std::fs::remove_dir_all(&room_save_dir);
        std::fs::create_dir_all(&room_save_dir).unwrap();
        let typewriter = engine::simulate_typewriter(pack, id, &room_save_dir)
            .unwrap_or_else(|err| panic!("ROOM{id:?}: the typewriter flow failed: {err:#}"));
        assert_eq!(
            typewriter.saved.to_bytes().len(),
            arklay::save::SAVE_BLOCK_SIZE,
            "ROOM{id:?}: the typewriter wrote a short block"
        );
        let parsed = SaveFile::from_bytes(&typewriter.saved.to_bytes())
            .unwrap_or_else(|err| panic!("ROOM{id:?}: typewriter block: {err:#}"));
        assert_eq!(
            parsed.to_bytes(),
            typewriter.saved.to_bytes(),
            "ROOM{id:?}: the typewriter block is not stable"
        );
        check_save_round_trip(
            &typewriter.state_after_save,
            &format!("ROOM{id:?} typewriter"),
        );
        coverage.typewriters += 1;
    }

    if !sim.transitions.is_empty() {
        coverage.moved += 1;
        coverage.transitions += sim.transitions.len();
        for destination in &sim.transitions {
            *coverage
                .destinations
                .entry((destination.stage, destination.room))
                .or_default() += 1;
        }
        return RoomOutcome::Moved(sim.game.id);
    }

    // The walk schedule raises no scripted door anywhere in the corpus (the
    // M16 audit lists none), so the soak also drives every registered door
    // slot through the real `.dor` transition path. Locked and keyed doors
    // return an error and are skipped; a camera-only door stays in the room.
    let mut destination = None;
    for slot in 0..sim.game.doors.len() {
        if sim.game.doors[slot].is_none() {
            continue;
        }
        let Ok(door) = engine::simulate_door(pack, id, slot as u8, None) else {
            continue;
        };
        assert_eq!(
            door.game.id, door.target,
            "the destination room was not booted"
        );
        check_save_round_trip(&door.game, &format!("ROOM{id:?} door {slot}"));
        coverage.saves += 1;
        coverage.door_transitions += 1;
        *coverage
            .destinations
            .entry((door.target.stage, door.target.room))
            .or_default() += 1;
        if door.target != id {
            destination = Some(door.target);
            break;
        }
    }
    match destination {
        Some(destination) => {
            coverage.moved += 1;
            RoomOutcome::Moved(destination)
        }
        None => RoomOutcome::Stayed,
    }
}

/// Inventory the pack's room entries.
fn room_ids(pack: &Pack) -> Vec<RoomId> {
    let mut ids: Vec<RoomId> = pack
        .paths()
        .filter(|path| path.starts_with("room/") && path.ends_with(".rdt"))
        .filter_map(|path| RoomId::parse(&path[5..9]).ok())
        .collect();
    ids.sort_by_key(|id| (id.stage, id.room, id.player_flag));
    ids.dedup_by_key(|id| (id.stage, id.room, id.player_flag));
    ids
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK"]
fn real_soak_walks_the_corpus_transitions_and_saves() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let pack = Pack::open(&pack_path).unwrap();
    let ids = room_ids(&pack);
    assert!(
        ids.len() >= 348,
        "expected the shipped room corpus, found {}",
        ids.len()
    );

    let save_dir = TempDir::new("saves");
    let baseline = live_bytes();
    let mut coverage = Coverage::default();
    let mut live_after = [0usize; PASSES];
    let mut peak_of = [0usize; PASSES];
    let mut seconds_of = [0.0f64; PASSES];
    let soak_start = Instant::now();

    for pass in 0..PASSES {
        reset_peak();
        let pass_start = Instant::now();
        for id in &ids {
            coverage.starts += 1;
            let mut current = *id;
            let mut visited = std::collections::HashSet::new();
            for hop in 0..MAX_HOPS {
                if !visited.insert((current.stage, current.room)) {
                    break;
                }
                if hop > 0 {
                    coverage.hops += 1;
                }
                match soak_room(&pack, current, &save_dir.0, &mut coverage) {
                    RoomOutcome::Skipped | RoomOutcome::Stayed => break,
                    // Follow the destination with a fresh schedule, exactly
                    // like continuing a game through the door.
                    RoomOutcome::Moved(destination) => current = destination,
                }
            }
        }
        live_after[pass] = live_bytes();
        peak_of[pass] = peak_bytes();
        seconds_of[pass] = pass_start.elapsed().as_secs_f64();
    }
    let soak_seconds = soak_start.elapsed().as_secs_f64();

    let delta = live_after[2].abs_diff(live_after[1]);
    let peak_extra = |pass: usize| peak_of[pass].saturating_sub(baseline);

    println!(
        "soak coverage: {} rooms simulated, {} skipped, {} moved, {} transitions, \
         {} door transitions, {} unique destinations, {} saves, {} typewriter \
         rooms ({} driven), {} cuts, {} frames",
        coverage.rooms,
        coverage.skipped,
        coverage.moved,
        coverage.transitions,
        coverage.door_transitions,
        coverage.destinations.len(),
        coverage.saves,
        coverage.typewriter_rooms,
        coverage.typewriters,
        coverage.cuts,
        coverage.frames
    );
    println!("soak skipped rooms: {:02x?}", coverage.skipped_ids);
    println!("soak destinations: {:02x?}", coverage.destinations);
    println!("soak placeholders: {:02x?}", coverage.placeholders);
    println!(
        "soak memory: baseline {:.1} MiB, live after passes {:.1}/{:.1}/{:.1} MiB, \
         peak extra {:.1}/{:.1}/{:.1} MiB",
        baseline as f64 / (1 << 20) as f64,
        live_after[0] as f64 / (1 << 20) as f64,
        live_after[1] as f64 / (1 << 20) as f64,
        live_after[2] as f64 / (1 << 20) as f64,
        peak_extra(0) as f64 / (1 << 20) as f64,
        peak_extra(1) as f64 / (1 << 20) as f64,
        peak_extra(2) as f64 / (1 << 20) as f64,
    );
    println!(
        "soak wall clock: {:.1}s total, passes {:.1}/{:.1}/{:.1}s",
        soak_seconds, seconds_of[0], seconds_of[1], seconds_of[2]
    );

    assert_eq!(
        coverage.starts,
        ids.len() * PASSES,
        "every room in the corpus must be started in every pass"
    );
    assert_eq!(
        coverage.rooms + coverage.skipped,
        coverage.starts + coverage.hops,
        "every start and followed destination must be simulated or explicitly skipped"
    );
    assert!(
        coverage.rooms >= 300 * PASSES,
        "only {} room simulations",
        coverage.rooms
    );
    assert!(
        coverage.door_transitions > 0,
        "no transition was followed in the whole corpus"
    );
    assert!(
        coverage.saves >= coverage.rooms,
        "not every room round-tripped a save"
    );
    assert!(
        coverage.typewriters >= TYPEWRITER_SAMPLE.len() * PASSES,
        "not every sampled room ran the typewriter save flow in every pass"
    );
    assert!(
        delta <= MAX_LIVE_DELTA_BYTES,
        "live bytes grew by {delta} between passes 2 and 3 (limit {MAX_LIVE_DELTA_BYTES})"
    );
    for (index, &seconds) in seconds_of.iter().enumerate() {
        assert!(
            peak_extra(index) <= MAX_PASS_EXTRA_BYTES,
            "pass {} peaked {} bytes above baseline (limit {MAX_PASS_EXTRA_BYTES})",
            index + 1,
            peak_extra(index)
        );
        assert!(
            seconds <= MAX_PASS_SECONDS as f64,
            "pass {} took {seconds:.1}s (limit {MAX_PASS_SECONDS}s)",
            index + 1,
        );
    }
}
