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
- **Item-action hand-off.** The original's submenu confirm only switches to
  state 4 (`DAT_00ae9f21 = 4; DAT_00ae9f22 = 0`) - the use/view action runs on
  the *next* frame, and the equip toggle and every successful use then run the
  submenu's eight-frame close animation (state 3) back to the inventory.
  arklay ran the action on the confirm frame and left the submenu open, so a
  use healed one tick early (t=105) and a following cancel closed the wrong
  level.
- **Health monitor under menus/modals.** The original's `menu_draw_health_bar`
  keeps running while a message window owns the input, while the FILE/MAP tabs
  and the item box are up, and under the item model viewer; its shared
  `rand()` draws must keep pace. arklay stopped the monitor in those states, so
  the first wrap after a screen opened diverged (`rng` at t=158 for
  FILE/MAP/combine).
- **RADIO tab action.** Confirm on the RADIO tab checks the room's per-frame
  item-use latch (bank 9 selector `0x3F`): with the latch up it records used
  item 0x4D and exits through the normal close fade; without it, it opens the
  radio's model viewer (the STATUS-screen item view), which keeps the room
  frozen. arklay always closed the menu instantly, resuming gameplay one tick
  early.
- **FILE selector/reader hand-off.** The original darkens the selector with
  the `pickup_fade_update` rect (`+0x20` per frame, input locked), then the
  reader takes over on its opaque black backdrop after two load frames, turns
  pages with the same slides arklay uses (whose start falls through the input
  frame), and returns to the selector through an exit slide, a black hold and
  the reverse fade. arklay switched modes instantly, kept the whole screen
  black behind the selector, and had no busy lock. The hand-off, the panel
  behind the selector and the input lock now match (the 140-147 transition
  frames are byte-identical); the page-turn mid-slide frames and the exit's
  final phase are still open below.

With these, `inventory_open`, `inventory_examine`, `inventory_nav`,
`inventory_open_close`, `radio_use`, `combine_ok`, `combine_refuse`,
`use_herb` and `equip_weapon` all match tick-for-tick in save mode
(`MATCH: 180-220 ticks identical`), with the menu/flush frames pixel-verified
at capture every tick; the residual frames are the room/player-mesh baseline.

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

- **FILE page turns and reader exit.** The selector/reader hand-off now
  matches, but the page-turn frames still differ ~22%: the reader's slide
  phase drifts because the reference's page-turn inputs are only accepted
  when its busy lock is clear, and its turns have a slightly different
  idle window than arklay's (the state trace cannot see either). The reader's
  exit path is likewise phase-shifted (~20 frames), so the scenario inputs
  stop before the exit for now.
- **MAP full-display rendering.** The original zooms the floor plan from the
  inventory panel to the full-display map as soon as the tab opens (with the
  "Mansion 1F" caption); arklay keeps the panel browser until a confirm and
  then swaps. With both engines in the zoomed view the settled frames still
  differ ~33%: arklay's full-display plan is missing the corridor walls and
  door markers and is positioned differently, so the zoom is not just an
  animation gap. This is the documented M16 map-tab simplification
  (`docs/m16-deviations.md` item 4); the full-display compositor needs the
  same pass the reference's `menu_update_map_animation`/`map_display_update`
  performs. The scenario runs at the state level until then.
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
