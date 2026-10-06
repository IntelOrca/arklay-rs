//! Always-run memory harness for the decode budgets.
//!
//! A counting global allocator tracks live and peak bytes while the engine's
//! public parsers run over adversarial synthetic inputs and over a valid
//! synthetic pack loaded and dropped repeatedly. The tests are deliberately
//! always-run (synthetic bytes only, no ignored attribute): they are the cheap
//! local mirror of the ignored soak's memory ceilings and pin the cap-first
//! allocation policy documented in `docs/architecture.md`.
//!
//! Every test takes a process-wide lock, so the counters only see this
//! binary's own measured section.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use arklay::pack::{Pack, PackWriter};
use arklay::state::RoomId;
use arklay::{rdt, scd};

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

/// Serializes the measured sections: the counters are process-wide and the
/// test harness runs tests in parallel threads.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Extra live bytes one adversarial parse may allocate before it fails.
const MAX_ADVERSARIAL_EXTRA_BYTES: usize = 4 << 20;
/// Live bytes may drift by this much across the load/drop loop.
const MAX_LEAK_BYTES: usize = 64 << 10;
/// Load/drop iterations for the leak check.
const LOAD_DROPS: usize = 256;

/// A valid RDT holding one init block and no events.
fn scripted_rdt() -> Vec<u8> {
    let mut data = vec![0u8; 0x94];
    let init = data.len();
    data.extend_from_slice(&4u16.to_le_bytes());
    data.extend_from_slice(&[0x0E, 0x00]);
    data.extend_from_slice(&0u16.to_le_bytes());
    data[0x48 + 6 * 4..0x48 + 7 * 4].copy_from_slice(&(init as u32).to_le_bytes());
    data
}

/// A valid three-entry pack image.
fn pack_bytes() -> Vec<u8> {
    let mut writer = PackWriter::new();
    writer
        .add(
            arklay::manifest::ENTRY,
            arklay::manifest::Manifest::base("re1")
                .render()
                .into_bytes(),
        )
        .unwrap();
    writer.add("room/1000.rdt", b"rdt bytes".to_vec()).unwrap();
    writer.add("ui/blue.tim", vec![0u8; 32]).unwrap();
    writer.to_bytes().unwrap()
}

/// An 8 MiB RDT whose event table would grow a `usize` list for every 4 input
/// bytes if the cap were only checked after the loop.
fn huge_event_table_rdt() -> Vec<u8> {
    const TABLE_BYTES: usize = 8 << 20;
    let mut data = vec![0u8; 0x94 + TABLE_BYTES];
    let table = 0x94;
    for (index, chunk) in data[table..].as_chunks_mut::<4>().0.iter_mut().enumerate() {
        *chunk = (index as u32 + 1).to_le_bytes();
    }
    data[0x48 + 8 * 4..0x48 + 9 * 4].copy_from_slice(&(table as u32).to_le_bytes());
    data
}

#[test]
fn an_adversarial_event_table_fails_within_the_peak_budget() {
    let _guard = serial();
    let rdt = huge_event_table_rdt();

    reset_peak();
    let baseline = live_bytes();
    let error = scd::reader::parse(&rdt).unwrap_err();
    let peak_extra = peak_bytes().saturating_sub(baseline);
    let message = format!("{error:#}");

    assert!(message.contains("limit"), "{message}");
    assert!(
        peak_extra <= MAX_ADVERSARIAL_EXTRA_BYTES,
        "the event-table cap allocated {peak_extra} bytes before failing"
    );
}

#[test]
fn over_cap_headers_fail_within_the_peak_budget() {
    let _guard = serial();
    // A pack whose declared entry count is far over `MAX_PACK_ENTRIES`.
    let mut over_cap_pack = Vec::new();
    over_cap_pack.extend_from_slice(b"APAK");
    over_cap_pack.extend_from_slice(&1u16.to_le_bytes());
    over_cap_pack.extend_from_slice(&u32::MAX.to_le_bytes());
    // An RDT whose first collision quadrant declares `i32::MAX` records.
    let mut over_cap_rdt = vec![0u8; 0x94];
    let collision = over_cap_rdt.len();
    over_cap_rdt[0x48 + 4..0x48 + 8].copy_from_slice(&(collision as u32).to_le_bytes());
    over_cap_rdt.extend_from_slice(&0i16.to_le_bytes());
    over_cap_rdt.extend_from_slice(&0i16.to_le_bytes());
    over_cap_rdt.extend_from_slice(&i32::MAX.to_le_bytes());
    for _ in 0..4 {
        over_cap_rdt.extend_from_slice(&0i32.to_le_bytes());
    }

    reset_peak();
    let baseline = live_bytes();
    let pack_error = Pack::from_bytes(over_cap_pack).unwrap_err();
    let rdt_error = rdt::parse(&over_cap_rdt, RoomId::parse("1000").unwrap()).unwrap_err();
    let peak_extra = peak_bytes().saturating_sub(baseline);

    assert!(
        format!("{pack_error:#}").contains("limit"),
        "{pack_error:#}"
    );
    assert!(format!("{rdt_error:#}").contains("limit"), "{rdt_error:#}");
    assert!(
        peak_extra <= MAX_ADVERSARIAL_EXTRA_BYTES,
        "over-cap headers allocated {peak_extra} bytes before failing"
    );
}

#[test]
fn repeated_lifecycles_return_to_the_baseline() {
    let _guard = serial();
    let pack = pack_bytes();
    let rdt = scripted_rdt();

    // Warm up one-time allocations before the baseline.
    for _ in 0..4 {
        let _ = Pack::from_bytes(pack.clone());
        let _ = scd::reader::parse(&rdt);
    }
    let baseline = live_bytes();

    for _ in 0..LOAD_DROPS {
        let loaded = Pack::from_bytes(pack.clone()).expect("the synthetic pack parses");
        assert_eq!(loaded.len(), 3);
        drop(loaded);
        let scripts = scd::reader::parse(&rdt).expect("the synthetic RDT parses");
        assert_eq!(scripts.init.len(), 1);
        drop(scripts);
    }
    let leaked = live_bytes().saturating_sub(baseline);

    assert!(
        leaked <= MAX_LEAK_BYTES,
        "load/drop leaked {leaked} live bytes over {LOAD_DROPS} iterations"
    );
}
