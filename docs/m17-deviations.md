# M17 deviations and limits

M17 is verification, not behaviour. No rendering, camera, audio-law,
animation, script-semantics, save-layout or pack-format input/output changes:
every valid input produces the same frames, states, saves and pack bytes as
before. What follows is what the milestone's tools deliberately do not prove.

## `verify`

- **Classification is by path.** `room/*.rdt`, `roommask/*.bmp`, `text/*.bin`,
  `map/tables.bin`, `data/bio_card.dat` and `manifest.toml` are matched by
  prefix/name, everything else by extension. A known format under an unknown
  name is counted opaque; `--strict` turns that into a failure. This is
  deliberate: future pack entries must not be able to break the command.
- **Cross-references are partial.** For every RDT, each camera's
  `roomcut/{room}_{camera:03}.bmp` must exist, and the manifest's Lua hooks
  must exist. Script-named assets (door ids, per-room sound names, effect
  sheets) are not resolved; a script that names a missing resource is a
  runtime warning, not a verify failure.
- **`data/core00.esp`/`core00.etm` are opaque.** Their pair parser exists but
  is not in the path table; verify reports them as opaque and does not parse
  them.
- **A parse does not prove usability.** An entry can parse and still be
  rejected later by a stricter consumer (for example a TIM whose declared
  texture pages exceed what a specific renderer wants); `verify` proves the
  format readers accept the bytes, nothing more.

## Fuzzing

- **Time budgets are not proof.** The stable torture matrix applies a fixed
  mutation schedule and the scheduled cargo-fuzz job runs each target for 60
  seconds per invocation. Both find panics on the inputs they happen to try;
  neither is exhaustive, and a clean run is not a proof of memory safety.
- **The workflow is gated while the package is absent.** `.github/workflows/
  fuzz.yml` skips cleanly unless `fuzz/Cargo.toml` exists, so the always-run
  torture matrix (its own slice) is the gate that runs on every change until
  the `fuzz/` package lands.

## The soak

- **The input schedule is synthetic.** Every room is driven with the fixed
  `idle -> walk -> turn -> action` cycle. Reachable script states are covered
  probabilistically by that cycle, not exhaustively; a state only reachable
  through a particular item/flag sequence is not visited. The M16 corpus audit
  and the golden captures remain the behavioural check.
- **The walk schedule raises no door in the shipped corpus.** The M16 audit's
  transition list is empty, so the soak also drives every registered door slot
  through the real `.dor` transition path (`simulate_door`): locked and keyed
  doors are skipped, a camera-only door stays in the room, and the first door
  that reaches another room is followed. This is broader than an input walk
  but still a sample: a door gated by an item or story flag the fresh state
  does not hold is not entered.
- **Transition following is bounded.** From each start room the soak follows
  up to four hops, with a visited set, so cycles and long chains cannot run
  away.
- **Typewriters are sampled.** The soak drives `simulate_typewriter` for rooms
  1000/1001 (the two `tests/save_real.rs` also drives) and counts the other
  rooms that declare the action without driving them: the headless seam's
  prompt probe is the expensive fallback and the other typewriters depend on
  progression state the fresh-game soak does not hold. Each sampled attempt
  gets a fresh slot directory, because a reused one raises the overwrite
  prompt the seam does not answer.
- **Memory deltas are order-of-magnitude.** The counting allocator includes
  the test harness's own allocations and allocator jitter, so the leak check
  compares live bytes between passes with a generous delta (32 MiB) and caps
  the per-pass peak rather than asserting exact bytes.
- **No film decodes.** The soak drains every `movie_on` request, exactly like
  the other headless seams, so the film decoders are exercised by their own
  tests and fuzz targets, not here.

## Budgets and caps

- **Budgets are machine-specific policy.** `docs/performance.md` records the
  reference machine and the measured values; only the ignored release test
  asserts hard numbers, `ARKLAY_PERF_BUDGET_MS` overrides the p95 budget, and
  the always-run test checks only report shape and ordering.
- **Caps are policy chosen above the shipped corpus.** Each cap in
  `src/budget.rs` has headroom over the largest shipped asset; a legitimate
  larger pack needs the cap raised following the procedure in
  `docs/performance.md`. Caps are not applied to opaque entries.
- **`--stats` measures the headless fixed-tick path.** No input is fed, the
  mixers are stubbed and no display is opened, so the reported tick and phase
  times exclude the interactive mixer and presentation costs.

## Output and process behaviour

- **Atomic writes rename a sibling temporary file.** A crash between the
  write and the rename can leave a `.<name>.<pid>.tmp` file behind; it can
  never leave a partial destination. Rename atomicity is the filesystem's.
- **CLI messages are not a stable interface.** The tests assert exit status
  and a message substring, not exact wording.

## Goldens

- The M16 UI/capture harness and the M14 movie goldens were generated by this
  project on a local install; they are regression/cross-checks against the
  engine's own past output, not an independent oracle of the original. M17
  changes none of them, and no captured frame, state hash, save byte or pack
  byte for a valid input changes.
