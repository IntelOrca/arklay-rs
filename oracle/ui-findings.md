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
  reader takes over on its opaque black backdrop, turns pages with the same
  slides arklay uses (whose start falls through the input frame), and returns
  to the selector through an exit slide, a black hold and the reverse fade.
  arklay switched modes instantly, kept the whole screen black behind the
  selector, and had no busy lock. The hand-off, the panel behind the selector
  and the input lock now match (the 140-147 transition frames are
  byte-identical).
- **FILE reader timing (page turns and exit).** The reader's state byte
  (`state[2]` in the JPN `jpn_file_screen_render` 0x0042c080) opens with
  sixteen black backdrop-load frames: `DAT_004af48c` ramps `8..0x80` while the
  `g_jpnReaderFrame` pieces are skipped (`DAT_004af600` clear), so the whole
  entry window after a `140 confirm` is black (t=148..164) and the page only
  pages in on the state-2 frame (t=165: slide `146, 71, 34, 16, 7, 3, 1, 0`).
  The busy lock (`state[9]`, FILE_BUSY) clears one frame *after* the slide
  lands and the input is only read the frame after that, so the reader's first
  accepted tick is 174 — a press inside the load window is dropped by both
  engines now. arklay also turned pages on L1/R1 and on confirm; the original's
  reader reads neither: pages turn with the D-pad right (`0x2000`) and left
  (`0x8000`) only, and confirm leaves the reader just once the last half-page
  has been acknowledged (FILE_SAVEFLAG, `state[0x10]`). A cancel is noticed
  *after* the reader's own render (`menu_file_state_machine` sets FILE_REQUEST
  `0xd` last), so the cancel frame still draws the settled page and the slide
  (`0xd`/`0xe`) starts on the next frame; the exit is that nine-frame slide out
  to the left, sixteen frames of `DAT_004af48c` ramping back down to black (the
  page parked clear at slide `-511`), the `0x10`/`0x11` black frames (the
  selector's cover pages back into the shared slot) and the `0x12` hand-off.
  The hand-off only rewrites the block's first four bytes, so the selector
  inherits the reader's `-511` slide: its cover and slot list stay parked
  off-screen (only the fixed-position book indicators, scroll arrows and title
  draw, dimmed) through the mode-2 fade, and `pickup_fade_update(1)` clears the
  pair when the ramp completes (t=286 after a 250 cancel).
- **FILE selector art and masks.** The scroll arrows, book indicators, slot
  icons and cursor are `(texU, 0)` rectangles of the cover page (`DAT_004d27d0`
  0x004d27d0, `0x004d27f4`/`0x004d2818`, `0x004d283c`/`0x004d2860`; only texU
  animates); arklay read them as `(0, texU)` off the cover's empty left edge and
  drew nothing. The slot icons and cursor also follow the cover's slide (the
  original rewrites their descriptor X/Y every frame), the selector's layer
  sits *behind* the panel's black masking rects (980) so a sliding cover cannot
  spill over the frame — arklay clips the selector to the panel's
  `0x28,0x18 0xf0x0c0` screen area to match — the FILE task's first frame draws
  the panel alone (`FUN_00481ae0`'s init runs a frame after the tab-row
  confirm), and the reader's backdrop pieces use the descriptor's variant-2
  half blend: `round(texel * 2 * ramp / 255)` modulated, then `127/256` over
  the black fill, rounding at both stages exactly like the reference's GPU chain
  (the un-clamped `2 * ramp` matters: an art value of 148 reads 74 at the fully
  ramped backdrop).

- **Item box world action and handshake.** `open_itembox` arms the lid; the
  reference then runs three frames of menu handshake between the lid settling
  and the panel fade (the room keeps ticking with the player frozen), and the
  panel fade steps straight after its arm (no draw-then-add frame). The box
  cancel restores the lid and drops back to the inventory with no fade; a
  further cancel closes the menu.
- **Typewriter save flow.** The save prompt's Yes arms the fade on the *next*
  frame (`check_typewriter_state` 2->3, counter 0x1000) while the room keeps
  ticking and the player stays frozen; the fade's last frame still runs the
  room tick, the save screen opens after it, and the screen owns the frame
  after that. arklay opened the screen immediately with no fade.
- **Typewriter save exit.** The in-game save screen draws no fade of its own -
  the original's `LoadSaveGameState` returns straight to
  `check_typewriter_state`, which arms a room fade-in at counter `0xF000`
  (`g_fading_state` `0x7FFF`, the traced state already stepped to `0x6FFF` on
  the cancel frame while that frame still draws the full `0x7FFF` black). The
  player-machine gate is restored the frame AFTER the step that wraps the
  accumulator negative, exactly like the original's frame-top case 4, so the
  frozen room's last gated tick comes before the gate opens; the frame
  hashes, the fade's step table (`0x6FFF` down to `-1`) and the player's
  action tick counter now match through the cancel, the exit fade and the
  settled room.

With these, `inventory_open`, `inventory_examine`, `inventory_nav`,
`inventory_open_close`, `radio_use`, `combine_ok`, `combine_refuse`,
`use_herb`, `equip_weapon`, `file_open`, `file_pages`, `map_open`,
`itembox_open`, `itembox_page` and `save_screen` all match tick-for-tick in
save mode (`MATCH: 180-480 ticks identical`); the menu, flush and
save-screen frames are pixel-verified at capture every tick, and
`save_screen` (which now includes the cancel and the room fade back in)
matches all 420 ticks with the fade and settled frames at the room/menu
residual (3.5-4.4% differing, the plain-room player-mesh baseline).
`file_open` and `file_pages` now run the reader's whole exit as well: every
frame that draws the reader (the entry's load window, both page-turn
directions, the cancel frame, the exit slide, the sixteen-frame backdrop
ramp-down, the black hold and the hand-off) is byte-identical, and the
selector frames either side sit at the room/menu residual (~600 differing
pixels, mean 0.2/255, plus the EKG line).

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

- **FILE reader blink phase.** The reader's arrows and the selector's icons
  blink off the state block's shared `state[0x12]` counter, which the original
  advances only in the selector's display state and the reader's settled state
  4. arklay now advances it in the same two states, but one frame of the
  reader's entry is counted by one engine and not the other, so a phase
  boundary (`blink & 0x30 == 0`) that falls on a reader-idle tick draws the
  other blink frame: exactly one 4x7 arrow (16 pixels, at most 0.02% of the
  frame, max channel delta 83) on `file_open` t=172 and `file_pages`
  t=172/248-250. Not yet traced to the source frame; every other reader frame
  is byte-identical.
- **FILE reader ramp-frame rounding.** The exit's first backdrop ramp frame
  (`file_open` t=241, `file_pages` t=261) leaves five FILEI-scrap pixels one
  step bright: the reference's GPU chain is reproduced as
  `round(round(texel * 2 * ramp / 255) * 127 / 256)` modulated over the black
  fill, which matches every other ramp frame (and the fully ramped backdrop)
  byte-for-byte, so those five pixels are a residual GPU rounding difference
  (max channel delta 1, mean 0.000/255).
- **MAP full-display rendering.** The original zooms the floor plan from the
  inventory panel to the full-display map as soon as the tab opens (with the
  "Mansion 1F" caption); arklay keeps the panel browser until a confirm and
  then swaps. The display now composites the area's own 128x100 plan
  (`map/map01..0b.tim` by area) over the blue grid backdrop with the
  original's index-0 key on the plan only, unvisited markers transparent,
  the current room in the original's red highlight and both layers at the
  `colorMul` half brightness (`MapScreen::display_image`), which drops the
  settled-frame error from 13.3 to 4.4 mean/255. Still open: the exact
  per-layer blend (some grid pixels read full brightness in the reference),
  the area-name caption drawn at the plan's foot, and the frame-by-frame
  zoom/light animation (the documented M16 simplification,
  `docs/m16-deviations.md` item 4).
- **Item box contents/model.** The box overlay opens and the trace matches,
  but the frames differ ~15%: the item swapped by the scenario's confirms and
  the model drawn in the viewport differ (the reference keeps the box object
  and swaps a different slot after the D-pad press), so the box cursor/state
  and the item viewer model need another pass.
- **EKG line rasterization.** The residual settled-screen difference (~0.3-1%)
  is the health monitor's EKG line: arklay rasterizes a Bresenham polyline. The
  reference's `FUN_00470c60` queues a 1px-wide quad that `MarniDX_GL::DrawLine`
  renders as two alpha-blended triangles through the (llvmpipe) GPU, so the
  covered pixels differ on steep segments. Not exactly matchable without
  replicating the GPU quad coverage.
- **Room/player frame backlog.** The room-close frames sit at the same ~4.9%
  baseline as the plain room captures (player-mesh edges), unrelated to the
  menu. In `file_open`/`file_pages` the room-entry frames before the menu opens
  (t=5..60, the player mesh and the portrait) are the only frames above 1500
  differing pixels (mean 0.53-0.58/255, max 164).

## Reaching more screens

Every screen is reachable through the save-mode oracle: the setup chunk in
`oracle/saves/*.lua` authors the inventory, item box, health, flags, files,
keys and position the interaction needs, and both engines boot the same block
(`--load` / `--test-load`). The room-mode capture seams (`--ui box`/`file`/
`map`/`view`) remain arklay-only and cannot replace the differential oracle,
because the reference has no equivalent.

The checklist runs above used `oracle/inputs/*.txt`; the saves they boot are
listed in `oracle/inputs/README.md`.
