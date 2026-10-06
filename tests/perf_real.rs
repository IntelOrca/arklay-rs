//! Performance budgets on the real corpus (ignored, release profile).
//!
//! Run with:
//! `TMPDIR=$PWD/target/tmp-test ARKLAY_RE1_ROOT=/home/ted/openre/assets/re1 \
//!  ARKLAY_RE1_PACK=/home/ted/openre/re1.akpak \
//!  cargo test --release --test perf_real -- --ignored --nocapture`
//!
//! The budgets and their measurement are recorded in `docs/performance.md`.
//! `ARKLAY_PERF_BUDGET_MS` overrides the per-tick p95 budget so a deliberately
//! lower value fails the test, exactly as the milestone acceptance requires.
//! The always-run plumbing test for the report format lives in `tests/cli.rs`.

mod common;

use std::process::Command;
use std::time::Instant;

/// Sampled rooms: a large hall, a corridor and a stage-2 room.
const SAMPLE_ROOMS: [&str; 3] = ["100", "106", "20D"];
/// Fixed ticks per sampled room.
const TICKS: u64 = 600;

/// Per-tick p95 ceiling in milliseconds (reference value with 2x headroom).
const TICK_P95_BUDGET_MS: f64 = 5.0;
/// Room-load ceiling in milliseconds.
const LOAD_BUDGET_MS: f64 = 60.0;
/// One rendered frame's ceiling in milliseconds.
const RENDER_BUDGET_MS: f64 = 30.0;
/// Sum of the script/entity/player update phases over 600 ticks.
const UPDATE_BUDGET_MS: f64 = 5.0;
/// Wall clock for the whole 600-tick run, process start included.
const ROOM_600_BUDGET_MS: u64 = 5_000;

/// The subset of the `--stats` report the budgets read.
#[derive(Debug, Default)]
struct Report {
    ticks: u64,
    tick_min_us: f64,
    tick_avg_us: f64,
    tick_p95_us: f64,
    tick_max_us: f64,
    load_us: f64,
    update_us: f64,
    effect_us: f64,
    render_us: f64,
}

impl Report {
    fn p95_ms(&self) -> f64 {
        self.tick_p95_us / 1000.0
    }
    fn max_ms(&self) -> f64 {
        self.tick_max_us / 1000.0
    }
    fn load_ms(&self) -> f64 {
        self.load_us / 1000.0
    }
    fn update_ms(&self) -> f64 {
        self.update_us / 1000.0
    }
    fn render_ms(&self) -> f64 {
        self.render_us / 1000.0
    }
}

/// Parse the stable `stats` line set.
fn parse_report(text: &str) -> Report {
    let mut report = Report::default();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["stats", "ticks", value] => report.ticks = value.parse().unwrap(),
            [
                "stats",
                "tick_us",
                "min",
                min,
                "avg",
                avg,
                "p95",
                p95,
                "max",
                max,
            ] => {
                report.tick_min_us = min.parse().unwrap();
                report.tick_avg_us = avg.parse().unwrap();
                report.tick_p95_us = p95.parse().unwrap();
                report.tick_max_us = max.parse().unwrap();
            }
            [
                "stats",
                "phase_us",
                "load",
                load,
                "update",
                update,
                "effect",
                effect,
                "render",
                render,
            ] => {
                report.load_us = load.parse().unwrap();
                report.update_us = update.parse().unwrap();
                report.effect_us = effect.parse().unwrap();
                report.render_us = render.parse().unwrap();
            }
            _ => {}
        }
    }
    report
}

#[test]
#[ignore = "requires both ARKLAY_RE1_ROOT and ARKLAY_RE1_PACK; run in release"]
fn real_room_budgets_hold_on_sampled_rooms() {
    let Some((_root, pack_path)) = common::asset_env() else {
        return;
    };
    let tick_p95_budget = std::env::var("ARKLAY_PERF_BUDGET_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(TICK_P95_BUDGET_MS);

    for room in SAMPLE_ROOMS {
        let start = Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_arklay"))
            .arg(&pack_path)
            .args(["--room", room, "--ticks", "600", "--stats"])
            .output()
            .expect("the arklay binary runs");
        let wall_ms = start.elapsed().as_millis() as u64;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "room {room} failed: {stderr}\n{stdout}"
        );

        let report = parse_report(&stdout);
        assert_eq!(report.ticks, TICKS, "room {room}: {stdout}");
        assert!(
            report.tick_min_us <= report.tick_avg_us
                && report.tick_avg_us <= report.tick_p95_us
                && report.tick_p95_us <= report.tick_max_us,
            "room {room}: the stats ordering is broken: {stdout}"
        );
        println!(
            "room {room}: tick p95 {:.2}ms avg {:.2}ms max {:.2}ms | load {:.2}ms \
             update {:.2}ms effect {:.2}ms render {:.2}ms | wall {wall_ms}ms",
            report.p95_ms(),
            report.tick_avg_us / 1000.0,
            report.max_ms(),
            report.load_ms(),
            report.update_ms(),
            report.effect_us / 1000.0,
            report.render_ms(),
        );
        assert!(
            report.p95_ms() <= tick_p95_budget,
            "room {room}: tick p95 {:.2}ms over the {tick_p95_budget:.2}ms budget",
            report.p95_ms()
        );
        assert!(
            report.load_ms() <= LOAD_BUDGET_MS,
            "room {room}: load {:.2}ms over the {LOAD_BUDGET_MS:.2}ms budget",
            report.load_ms()
        );
        assert!(
            report.update_ms() <= UPDATE_BUDGET_MS,
            "room {room}: 600-tick update {:.2}ms over the {UPDATE_BUDGET_MS:.2}ms budget",
            report.update_ms()
        );
        assert!(
            report.render_ms() <= RENDER_BUDGET_MS,
            "room {room}: render {:.2}ms over the {RENDER_BUDGET_MS:.2}ms budget",
            report.render_ms()
        );
        assert!(
            wall_ms <= ROOM_600_BUDGET_MS,
            "room {room}: wall clock {wall_ms}ms over the {ROOM_600_BUDGET_MS}ms budget"
        );
    }
}
