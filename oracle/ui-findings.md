# UI / interaction frame-parity findings

Findings from driving both engines through the pause menu with
`scripts/oracle-ui.sh` and a scripted `--input` playback (`oracle/inputs/`).
The state trace is compared tick-for-tick and every frame pixel-by-pixel.

## Fixed

- **RADIO top-tab art.** The original swaps the RADIO tab sprite for a blank
  dash (`g_TextureDesc.texV = 0x40`) while `DAT_00ae9f1f` is clear (no radio
  item). arklay always drew the RADIO label. `ui/main_menu.rs` `draw_tabs`.
- **Menu tile V-flip.** The tiled frame records carry a flag byte the original
  maps to the sprite mirror bits (`(flags & 0xc0) << 16`): bit `0x40` flips V,
  `0x80` flips U. arklay ignored it, so the 160x12 bottom decoration tile (the
  only record with `0x40`) drew upside down. `Framebuffer::draw_indexed_sprite_mirrored`
  + `draw_tile`. Drops the settled inventory screen from ~3.2% to ~0.8%.
- **Health-monitor rand stream.** `menu_draw_health_bar` draws the wave variant
  (and the poison-pick bit) from the game's shared `rand()`. arklay used a
  private xorshift, so the shared stream fell one draw behind from the first
  sweep wrap and `rng`/`rseed` diverged for the rest of the run. The state
  machine also differed: the original's case 0 falls through into case 1 (the
  first frame steps the head to `0x85` and wraps it to `0x35`), the health
  state/colour are recomputed only at the wrap, the poison bit is drawn before
  the variant, and the variant folds away an immediate repeat (`DAT_00ae9f48`).
- **Missing menu-open frame seed.** On the frame START opens the menu the
  reference still draws `g_RandSeed = rand()` (game_loop's first statement)
  before it detects the request; arklay opened the menu ahead of the room tick
  and skipped the draw. `GameState::draw_frame_rand` on the open path.
- **Menu fade blend.** The black veil is `round(channel * (255 - alpha) / 255)`
  (verified exact against the oracle's fade frames), not `(256 - alpha) / 256`
  and not the truncated `/255`. The one-LSB mismatch diverged ~19% (round) or
  ~60% (truncate) of a fading screen; fixed the room-entry fade, the menu
  fade-in and the room fade after the close.
- **Draw-then-add ordering.** The menu and room fades capture the pre-step
  accumulator for the frame's draw (`menu_fade_draw` / `room_fade_draw`) while
  the traced state stays the post-step value, so both the pixels and the trace
  match the reference.
- **Menu fade accumulator setup.** The opening and closing fades seed the
  accumulator from the room-entry fade's negative leftover (`set_fading` leaves
  it), reset it on their first step, and the closing fade clears it to the
  original's `0:0` before a two-frame menu-task teardown.
- **Menu exit and open pacing.** The closing fade restores the player to idle on
  the first frame after the cancel (the original's exit state 7), the menu holds
  one frame after the fade before its first health-bar tick (the original's
  state-0 -> state-1 frame), and the room-entry fade after the close freezes the
  player machine and entities for its run (the reference suspends game_loop),
  with the first frame after the menu task exiting still running one tick.
- **Item-action submenu open animation.** The original grows the submenu box
  from zero to full (`DAT_00ae9f27` `0x80..=0x88`, width `*6`, height `*3`)
  over eight frames after confirm. `MainMenu::submenu_frames` scales
  `draw_submenu` and gates the action input until it is open.
- **Menu-close entity freeze.** The room-entry fade after a menu close suspends
  the reference's `game_loop` for its whole run. On its first frame the
  restored word has the player bit but not the entity/monster/effect bits, so
  the player machine resumes one tick before the NPCs: arklay used to run the
  entity pass on that frame too, leaving every NPC one tick ahead from then on
  (`inventory_open_close` diverged at t=134, `e1/e2/e3.ticks`).
- **Save block stage byte.** The original's BioCard 0x200 is the 0-based
  `g_stageId`; the port stored the 1-based RDT stage digit. arklay's own
  round-trip hid it, but a save written by arklay booted stage+1 in the
  reference (and vice versa). `SaveFile::from_state`/`apply_to` convert.
- **Continue position rollback.** `GameSession::from_save` restored the saved
  position but left the player's collision mirror at `player::spawn`'s
  walk-zone centre. The first room tick anchored the collision resolve at that
  mirror and teleported the player back to the spawn whenever the saved spot
  differed, so any save not already standing on the spawn diverged on tick 1.

With these, `inventory_open`, `inventory_examine`, `inventory_nav` and
`inventory_open_close` all match tick-for-tick in **both** room mode and
save mode (`MATCH: 200 ticks identical`); the open/close fade frames are
byte-identical or at the residual level below.

## Save-mode oracle

`scripts/oracle-ui.sh --save oracle/saves/<file>.dat` boots both engines from
the same checked-in 0x800-byte BioCard block: arklay with
`--ui game --load <file>`, the reference with `--test-load <file>` (port-only
hook in `TestHooks.cpp`; `DebugQuick_LoadFile` shares the F1 debug load's
restore/arming path). The blocks are generated deterministically with
`--setup-script` + `--save-out` from the chunks in `oracle/saves/`
(`scripts/oracle-saves.sh`); the reference's `--test-load` arms its trace on
the room the save records.

The pause-menu suite is identical from the save path; the room-mode table
above was re-verified after the save-path fixes. The save path also booted the
first FILE/MAP/radio/combine/use/equip/item-box/typewriter scenario attempts
into the same tick-0 state on both engines (rng, fade, entities and pixels to
match), which is what the save interchange exists for.

## Open

- **FILE/MAP screens hold the room one tick differently.** From the save path
  (and room mode), opening the FILE or MAP tab freezes the room on both
  engines, but the reference runs exactly one gameplay tick (one `rand()` draw)
  at t=158 that arklay skips, so `rng` diverges for the rest of the run while
  every other field stays identical. Same shape as the fixed menu-open seed:
  the reference's `game_loop` first statement runs on the frame the screen
  closes its entry animation, arklay's screen owns the tick instead. Scenario
  inputs `file_open`/`file_pages`/`map_open`.
- **RADIO use menu-close pacing.** Confirm on the RADIO tab records used item
  0x4D and closes the menu. The reference resumes gameplay one tick later than
  arklay's `close_menu()` path (divergence at t=100, `e0.as` then `rng`), the
  same class as the paused menu cancel path that `begin_menu_close` models.
  Scenario input `radio_use`.
- **USE/MOVE action timing.** The green herb's heal lands at t=105 on arklay
  and ~t=110+ on the reference (the submenu close/flush frames), and the
  combine MOVE action leaves the same one-tick `rand()` seed gap as FILE/MAP
  at t=158. Scenarios `use_herb`, `combine_ok`, `combine_refuse`,
  `equip_weapon`.
- **Item box / typewriter world action not reached.** The authored saves place
  the player inside the dining room's `aot_set` rects (item box slot 2,
  typewriter slot 7) and press action, but the overlay does not open on either
  engine, so the current `itembox_*`/`save_screen` runs are vacuous (matched
  but exercised nothing). The scenarios need the real interaction spot
  (facing/reach or a short scripted walk) before they count.
- **EKG line rasterization.** The residual settled-screen difference (~0.3-1%)
  is the health monitor's EKG line: arklay rasterizes a Bresenham polyline. The
  reference's `FUN_00470c60` queues a 1px-wide quad that `MarniDX_GL::DrawLine`
  renders as two alpha-blended triangles through the (llvmpipe) GPU, so the
  covered pixels differ on steep segments. Not exactly matchable without
  replicating the GPU quad coverage.
- **Room/player frame backlog.** The room-close frames sit at the same ~4.9%
  baseline as the plain room captures (player-mesh edges), unrelated to the
  menu.

## Reaching more screens

Every screen is reachable through the save-mode oracle: the setup chunk in
`oracle/saves/*.lua` authors the inventory, item box, health, flags, files,
keys and position the interaction needs, and both engines boot the same block
(`--load` / `--test-load`). The room-mode capture seams (`--ui box`/`file`/
`map`/`view`) remain arklay-only and cannot replace the differential oracle,
because the reference has no equivalent.

The checklist runs above used `oracle/inputs/*.txt`; the saves they boot are
listed in `oracle/inputs/README.md`.
