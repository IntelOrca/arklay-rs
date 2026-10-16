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
- **MAP tab browser.** The original's tab browser composites one art page into
  the panel - `map_tab_load_map` loads the file of the availability mask's
  *highest* layout bit (`map0e`, `map00`, `map0c` or `map0d`) and samples every
  browser sprite from that single page: the 160x72 base window at (33,32), the
  four layout markers (the current layout's at full brightness, the others at
  `colorMul` 0x40), the area-name row (the 80x16 sprite at (57,104) whose
  `texV` is `layout * 0x10 + 0x48` - the "Mansion 1F" caption) and the current
  floor's number glyph (24x16 at (137,104), `texV` `(6 - room - 4) * 0x10`,
  which wraps past the page for layout 3's room index and is then dropped by
  the page search), plus the up/down arrows while the neighbouring room exists,
  blinking between two art frames on the reference's per-frame `MAP_HL_COUNT`
  cycle. arklay drew a private blue-grid backdrop with a centred plan instead
  (`plan_image`), so the whole display window differed ~29%. Two further
  reference details came with it: the tab action runs one frame *after* the
  confirm (`menu_handle_input` only sets `DAT_00ae9f20 = 2` on the confirm
  frame, so the browser's first frame is the next one and its blink counter
  starts at 1), and the panel's right-hand edge frame part is at a depth
  *nearer* than the map sprites, so its dithered column shows over the
  composite. The scenario's map frames (t=111-179) drop from 21.6k-23.0k
  differing pixels (28.9-29.9%, mean 13.7-15.4/255) to 525-605 (~0.7-0.8%,
  mean 0.16-0.21), which is the panel's own portrait/EKG residual seen on every
  non-map frame - the map window itself is now byte-identical - and the
  hand-off frame t=110 goes from 16.5k (21.5%) to 525.
- **Item box contents and model.** The box's page/confirm state machine and
  overlay were a pass behind the reference's `menu_itembox_interaction`
  (0x004941F0):
  - The original's state 3 (a slide is running) reads *no* buttons, so the
    confirm that lands mid-slide is dropped and the swap uses the slot the
    slide landed on; a forward page only advances the box cursor when its
    15-frame slide ends, a backward page moves it at once. arklay swapped on
    the confirm frame of the slide and advanced the cursor on the press
    frame, so `itembox_open` deposited the knife into slot 1 instead of
    leaving the box alone, and `itembox_page`'s swap landed 20 frames early.
  - The box interaction draws the shared slot cursor itself
    (`itembox_draw_cursor`): while `DAT_00ae9f18` bit 0x20 is clear it is the
    dim `texU 0xA8` cell, and the arm/init reset the counter (an armed box
    never counts down, so it keeps the dim frame). arklay left the menu's
    bright cursor in place.
  - The overlay's layer order is the original's pending-sprite depth, not its
    submission order: the name rows and the preview icon/quantity sort
    *behind* the two 50% shadow rects (which is what dims the rows above and
    below the selected one), the browse-mode pair sorts behind the box-full
    line and the two row shadows in front of it, and the two scrollbar-track
    rects are the same 44%-black `0x60000000` variant as the shadows rather
    than opaque black (they darken the MAP/FILE panel to 0.56, not to 0).
  - The page slide draws the incoming slot's icon from `texV + state*2` in a
    window that shrinks from the bottom, both quantities ride up with their
    slots (the box's quantity at `screenX - 2`), a 4th name row fills the
    list's gap, and nothing is painted below the panel's lower mask
    (`0x77 + 9`). arklay animated the wrong window, drew the quantity at the
    slot x and let the incoming icon spill over the panel below the mask.
  - The empty-slot label comes from the original's `PrintText8x14`
    (0x00455520), whose plain-byte grid has **no** `+2` row bias (unlike
    `PrintFormattedText`), in the grey CLUT row: `_Nothing_` therefore lands
    on the Latin cells of the Japanese sheet. arklay drew its ASCII bytes
    through the encoded grid and in white.
  - The box-full line's condition is the byte-underflowed
    `0x2C < (unsigned char)(cursor - 1)`, so the first slot draws it too,
    at `0x31 - state` with no cursor term; the original's line spans
    `[42, 0xA8)` (126px), not `[42, 0xA8]`.
  `oracle/inputs/itembox_page.txt` now pages with the D-pad up/down: the
  reference's `--test-input` has no L1/R1 raw bits at all (the port's pad
  layout binds neither), so the box's own page keys are the only pager both
  engines can receive.
- **Menu cursor/name reveal.** The reference starts drawing the cursor and the
  0xBA item-name line on the menu's first *input* frame: neither the
  fade-complete frame nor the state-0 transition frame draws them, and the
  item box's own cursor waits with them. arklay revealed both on the
  fade-complete frame, two frames early (`itembox_open` 118-119,
  `inventory_open` 77-78, ~800 extra differing pixels on each). Fixed with
  `menu_reveal_delay`.

With these, `inventory_open`, `inventory_examine`, `inventory_nav`,
`inventory_open_close`, `radio_use`, `combine_ok`, `combine_refuse`,
`use_herb`, `equip_weapon`, `file_open`, `file_pages`, `map_open`,
`itembox_open`, `itembox_page` and `save_screen` all match tick-for-tick in
save mode (`MATCH: 180-480 ticks identical`); the menu, flush and
save-screen frames are pixel-verified at capture every tick, and
`save_screen` (which now includes the cancel and the room fade back in)
matches all 420 ticks with the fade and settled frames at the room/menu
residual (3.5-4.4% differing, the plain-room player-mesh baseline).

`itembox_open` and `itembox_page` sit at that same residual: their steady
frames differ by 430-511 pixels beyond 8 (0.56-0.67%, mean error 0.19-0.26 /
255) and the fade hand-off frames by ~595, down from ~10700 (14.4-15%) before.
Every one of those pixels is in the portrait or the EKG strip below - the box
overlay, its list, its preview column, the cursor and the swap are pixel-equal
on every compared frame.

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
- **MAP full-display zoom.** The browser's confirm switches to the full map
  display (`map_display_update`: the area plan's zoom sprites, the blue
  backdrop page and the browser's own composite dimmed underneath). No
  scenario reaches it - `map_open` only opens the browser, whose second
  confirm would be needed - and the port's single-swap `display_image`
  compositor stays the documented M16 simplification
  (`docs/m16-deviations.md` item 4).
- **Menu cursor blink.** The reference's slot cursor alternates between the
  bright `texU 0x80` and dim `texU 0xA8` cells every 32 frames of
  `DAT_00ae9f18` (dim for the first 32 frames after any move/arm); arklay's
  *menu* cursor is always bright, which shows up on the inventory/other menu
  screens (not the item box, which draws its own blinking cursor now) as 158
  differing pixels per frame during the dim phases - e.g. `inventory_open`
  at frames 120-142/175-206/239-270 versus 0 outside them. The item box's own
  counter and frame selection are already exact (`cursor_dim`).
- **Portrait and health strip residual.** Every settled menu frame of every
  scenario keeps ~330 differing pixels on the character portrait
  (`ui/statface.tim`, the 30x30 window at x 22-51, y 146-175: the blue plate's
  edge sits one step over in arklay) plus 20-200 on the health strip (the EKG
  line below, and its small face icon), i.e. 0.46-0.67% of the screen with a
  local max of 246-255. Both are present identically in `inventory_open`
  before and after this change, so they are menu-wide, not item-box, gaps.
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
