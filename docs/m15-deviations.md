# M15 integration deviations

M15 adds the authoring layer: the pack manifest, layered packs, the text-SCD
assembler and the script override, the mod builder and demo sources, the
sandboxed Lua hooks and the runtime mod flags. This document records the
deliberate limits and approximations the milestone leaves; the no-enemy,
no-combat scope of the engine is unchanged and is covered by
`docs/m14-deviations.md`.

1. **No RDT writer and no patch format.** A `scd/{id}.scd` override replaces
   the room's whole init/main/event script set; the `.rdt` itself stays
   byte-for-byte as converted, and there is no way to patch a single script
   section or a sub-file (backgrounds, models, messages) through the script
   path. The demo therefore has inert doors and no vanilla events in its room
   (RDT 1000). A future patcher can build on the same assembler.

2. **No runtime compilation.** Packs ship assembled `.scd` bytecode and Lua
   source. Only the CLI compiles `.s` text (`arklay scd build`, `arklay mod
   build`); the engine never assembles at play time.

3. **Lua sandbox limits.** The VM loads only the base, `string` and `table`
   libraries. `os`, `io`, `package`, `debug`, `coroutine` and `math` are
   absent, so hooks cannot read files, open sockets, read the clock, schedule
   coroutines or use nondeterministic maths. There is no module loading, no
   `require`, and no binary chunk support. Enforcement is the standard-library
   set plus the process's safe-mode Lua state, not a bytecode verifier: a hook
   can still allocate unbounded tables (bounded in practice by the
   instruction budget below).

4. **Hook error policy.** A chunk that fails to compile or run is logged once
   with a `[lua]` prefix and skipped; the remaining chunks still load. A hook
   call that fails is logged once and disables that hook only
   (`on_room_load`, `on_tick` or `on_message`); the others and the game
   session keep running. The errors are reported on stderr, never as a
   session failure and never as a capture change.

5. **Instruction budget.** A C-level Lua hook runs every 10,000 VM
   instructions and aborts a call after roughly five million instructions,
   which logs the budget error and disables the hook. This is a runaway guard,
   not a precise instruction quota: a hook doing heavy but finite work close
   to the limit can still be cut off, and the count only covers instructions
   the VM executes (C library calls count as the instructions that invoke
   them).

6. **Lua state persists across rooms.** One VM is created per session; chunks
   run once. `on_room_load` fires on every room entry and transition, but Lua
   globals (and upvalues such as the demo's `granted`) are not reset, so
   per-room bookkeeping is the author's job. The demo grants its item only
   once per process for exactly this reason.

7. **Message rewrite timing.** `on_message` only sees a message *raised during
   the tick that just ran*: the engine tracks the window's menu-active bit
   across the room tick. A message raised by the init script (before the first
   tick) or by the door-transition adapter is not filtered, and the hook's
   replacement id must fit the message id byte. The rewrite happens before the
   window resolves the message's encoded bytes, so the new id is displayed.

8. **Headless message harness.** Headless runs (`--capture --ticks`,
   `simulate_room`) release the message menu-choice bit every tick so a
   script's F7 wait cannot stall without input. A script that raises a message
   every tick therefore re-requests it each tick in headless runs, while the
   interactive window keeps it up until the player dismisses it. This is the
   pre-existing harness behaviour; M15's `on_message` detects fresh requests
   through the same bit.

9. **Base packs without manifests.** A shipped or third-party base pack with
   no `manifest.toml` is tolerated: it is treated as `id = <file stem>`,
   `kind = "base"`, RE1 dialects, with a warning, and mods are checked against
   that stem id. A mod without a manifest is always rejected.

10. **Demo source location.** The plan named `games/demo/`; the checked-in
    sources live in `mods/demo/` (the merged M15 slice and its tests use that
    path). The build command otherwise matches the plan:
    `arklay mod build mods/demo --out demo.akpak`. There is no `games/`
    directory.

11. **`--room` is three hex digits.** The demo acceptance boots RDT 1000 via
    `--room 100` (stage 1, room 0x00); the four-digit RDT number is what the
    pack stores (`room/1000.rdt`). The docs and tests use the RDT number when
    naming the room and the three-digit form for the flag.

12. **CLI tool layers.** `--no-mods` is accepted by the runtime only; the
    `list`, `extract` and `pack info` tools apply exactly the `--mod` layers
    they are given and never auto-discover a sibling `mods/` directory. This
    keeps their output stable for a pack whose directory happens to contain a
    `mods/` folder.
