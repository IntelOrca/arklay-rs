# Performance budgets

Policy numbers owned by this project, not claims about the original. They are
measured in the release profile on the reference machine below, rounded up
with headroom, and asserted only by ignored tests; the always-run test checks
the report's shape and ordering, never wall-clock values.

## Reference machine

| | |
|---|---|
| CPU | 4 vCPU, Intel Core Processor (Haswell, no TSX) |
| OS | Linux 7.0.0 (x86_64) |
| Toolchain | rustc 1.98.0 stable |
| Profile | `release` (`lto = "thin"`) |
| Packs | `re1.akpak` (2541 entries, 408,551,500 bytes), `re1.voice.akpak` (517 entries), `re1.movie.akpak` (27 entries) |

## Budgets and measurements

All times are release-profile wall clock on the reference machine. `--stats`
measures the headless fixed-tick path (no input, no display, audio stubbed), so
these numbers exclude the interactive mixer and presentation costs.

| Budget | Measured (2026-10-06) | Budget | Headroom |
|---|---|---|---|
| Gameplay tick p95 | <= 0.004 ms on the sampled rooms | 5 ms | >1000x |
| Gameplay tick max | <= 0.86 ms (room 106, the heaviest sample) | not asserted | - |
| Room load (RDT parse, cuts, scripts, player assets) | 3.0-5.8 ms | 60 ms | ~10x |
| One capture render | 2.9-3.7 ms | 30 ms | ~8x |
| Script/entity/player update, 600 ticks | 0.54-1.30 ms | 5 ms | ~4x |
| Effect update, 600 ticks | 0.24-0.31 ms | not asserted | - |
| 600-tick room run, process start and pack open included | 318-524 ms | 5 s | ~10x |
| Full-game soak, one pass | 394.1 / 395.8 / 393.1 s | 900 s | ~2.3x |
| Full-game soak, all three passes | 1183.0 s | - | - |
| Soak peak above baseline | 11.0 MiB | 512 MiB | ~45x |
| Soak live-byte drift between passes 2 and 3 | 0 MiB | 32 MiB | - |
| Conversion, main pack (408,678,361 bytes) | 1.5-1.9 s (~215-270 MB/s) | - | - |
| Conversion, all three packs (761,027,595 bytes) | 1.5-1.9 s (~400-500 MB/s) | - | - |
| Conversion peak RSS | 660 MB | - | - |

The soak's 2026-10-06 coverage: 3003 room simulations over the three passes
(348 starts per pass, 28 stub rooms without camera cuts skipped, 973 followed
destinations per pass), 2919 `.dor` door transitions, 89 unique destinations,
5922 save round-trips, 42 typewriter flows over the sampled rooms, 17,649
camera cuts and 3003 rendered frames. The only placeholder opcode dispatched
is the documented `0x05` flag-bank fallback. The soak's live bytes are exactly
the pack's own allocation after every pass (390.1 MiB), so the three-pass
check sees no drift at all on the reference machine.

The p95 is the upper edge of a power-of-two microsecond histogram bucket
(`src/stats.rs`), clamped between the mean and the maximum. A tick p95 below
one microsecond therefore prints as `0` in the microsecond report; the budgets
are stated in milliseconds so they stay meaningful on slower machines.

The soak budgets live in `tests/soak_real.rs`: `MAX_PASS_SECONDS` (900),
`MAX_PASS_EXTRA_BYTES` (512 MiB) and `MAX_LIVE_DELTA_BYTES` (32 MiB). The soak
is a coverage tool, not a benchmark; its ceilings exist to catch a runaway,
not to enforce a tight frame time.

## The performance test

`tests/perf_real.rs` (ignored) runs `arklay <pack> --room <id> --ticks 600
--stats` on sampled rooms and asserts the budgets above in the release
profile:

```sh
ARKLAY_RE1_ROOT=/path/to/re1 ARKLAY_RE1_PACK=/path/re1.akpak \
  cargo test --release --test perf_real -- --ignored --nocapture
```

`ARKLAY_PERF_BUDGET_MS=<ms>` overrides the per-tick p95 budget. Setting it
below the measured value fails the test, which is how the milestone's
acceptance checks the override path:

```sh
ARKLAY_PERF_BUDGET_MS=0.001 ... cargo test --release --test perf_real -- --ignored
```

Any conservative budget can be raised with `<ms>`; the docs table is updated
in the same change.

## Raising a decode cap

Caps in `src/budget.rs` are policy chosen above the shipped corpus with
headroom. When a legitimate pack trips one, the failure names the format, the
limit and the observed value. To raise a cap:

1. Confirm the input is legitimate (parse it on the original or a trusted
   tool) and note the observed value.
2. Raise the specific cap to at least 2x the observed value; never raise a
   global cap to fix one format.
3. Add the new corpus entry to the format's budget test and record the change
   in the milestone's deviations file.
4. Re-run the torture matrix, the memory test and `arklay verify` on the new
   pack.
