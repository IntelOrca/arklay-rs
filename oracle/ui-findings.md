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
- **START while being attacked.** The reference's menu-button detection
  (game_loop 0x00480de8) only raises the menu request when
  `g_playerEntity.isBeingAttackedFlag == 0`; a grabbed or bitten player stays
  in gameplay through the press. arklay opened the pause menu regardless, so
  the attack froze under the inventory panel and the RNG stream stopped. The
  same gate arms the item box (`open_itembox`, 0x0041b990), which arklay also
  checked only at the menu hand-off. `oracle/inputs/attacked_start.txt` pins
  the menu case (ROOM 101, Chris: the grab sets the flag at t=100 and the first
  bite lands at t=121): the reference ignores the press and takes the bite on
  t=121 (health 140 -> 130), while the unfixed port froze that same tick on the
  inventory panel. Both checks now require the flag to be exactly zero.
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
- **Inventory cancel cue.** The original's state-1 exit plays `play_sfx(3, 5)`
  once on the cancel frame: the cancel bit fails `cond1`
  (`src/game/MainMenu.cpp` 498), so `menu_handle_input` (501) never runs - its
  navigation path owns no cancel cue, only the confirm path's
  `play_sfx(3, 6)` - and the state falls through to
  `menu_exit_cleanup(); play_sfx(3, 5);` (510-511). arklay
  queued the input's cancel cue *and* the close's exit cue, so a cancel close
  sounded the same sample twice. The exit tab's own action (`menu_tab_exit` ->
  `menu_exit_cleanup`, no cue) also picked up the exit cue on top of its
  confirm's decide. `MenuEvent::Close` now owns the state-1 exit cue and the
  new `MenuEvent::CloseSilent` is the exit tab's silent close; the cancel
  input queues no cue of its own. Pinned by
  `engine::tests::the_pause_menu_cancel_plays_the_exit_cue_once` (live mixer
  voices on the cancel frame: 2 before, 1 after) and
  `engine::tests::the_exit_tab_closes_silently_after_its_decide_cue`.
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
- **Continue camera re-home.** `GameSession::from_save` restored the block's
  camera byte and seeded the boot switch-zone walk with it, so a continue kept
  the cut that was live when the game was saved. The original's `room_set`
  zeroes `g_roomCameraId` - the BioCard byte at 0x202 itself, `RoomInit.cpp`
  670-676 - after the room's init script and before `check_camera_switch`, so
  every room load re-homes from camera 0. A save standing inside room 104's
  collision volume booted on cut 2 in arklay and cut 0 in the reference for
  the whole run (`bootcam_chris_104`).
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
- **FILE reader page-turn cue.** The original's `jpn_file_list_input`
  (0x0042bf90) plays `play_sfx(3, 8)` - the character table's slot 8,
  `Chris10` - on each accepted half-page turn, and `play_sfx(3, 4)` (the
  shared `cursor` blip) when the last-page acknowledgement is raised or
  withdrawn. The selector's moves and book changes blip slot 4, its confirm
  opens the reader with slot 6, and the reader's cancel / last-page confirm
  plays slot 5. arklay's file screen queued no cues of its own: the engine's
  generic `menu_cue` supplied a cursor/decide/cancel blip for every edge,
  including presses the reader's busy lock swallowed, and the page-turn
  sample never played. The Lua screen now owns its cues (sending
  `page_turn` for slot 8) and the FILE tab and room document reader drain
  them, so a page turn is one `Chris10` voice and a locked press is silent.
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
  lid swings to -441 (the 41st `check_itembox_state` call: the accumulator
  grows *before* the -199 test reverses it, so the ease-back starts at 21, not
  19) and the settle frame is `game_loop`'s last: the same iteration raises the
  menu request after the room has drawn the settled lid, and the next frame is
  the panel fade (confirmed by logging the reference's lid angle per tick under
  gdb). The lid stays open on screen until the overlay closes; state 4 resets
  it on the first room frame after. arklay settled two ticks early
  (-400, 39 calls) and ran three handshake frames, so the lid snapped shut two
  frames before the overlay (#59); the `itembox_cam` scenario shows it. The
  panel fade steps straight after its arm (no draw-then-add frame). A browse
  cancel (and a confirm on a cursor without a slot) restores the lid and runs
  `menu_exit_cleanup`: the whole pause menu fades out and the room resumes -
  the box does not drop back to the inventory panel.
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
    `[42, 0xA8)` (126px), not `[42, 0xA8]`. All four shadow rects composite
    *over* it (the line's depth is 900, the rects' 501): the first slot's line
    takes the browse list rect's dimming, the last two slots' lines take the
    browse list and row-3 rects'.
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
- **Menu cursor blink.** The reference's slot cursor alternates between the
  bright `texU 0x80` and dim `texU 0xA8` cells on the shared `DAT_00ae9f18`
  counter (dim for the first 32 frames after any move or accepted confirm),
  while arklay always drew it bright. `MainMenu::cursor_blink` now restarts on
  open, move and accepted confirm, counts down on the idle navigation frames
  and holds at zero under the submenu, and `draw_slot_cursor` picks the cell
  from bit `0x20`. Landed with the examine-viewer rework (#57), so the
  `inventory_open` frames that showed 158 differing pixels during the dim
  phases (120-142/175-206/239-270) now match.
- **Pause menu EKG.** `menu_draw_health_bar` queues the wave with the line
  primitive, not a per-column polyline: it evaluates the head-side partial
  from the selected record's slope, draws one line per record (joining the
  records' own heights), and closes with a partial to the tail. arklay
  sampled the spine per screen column, which thickened the steep QRS flanks
  and missed the reference's segment joins. The trailing line's gradient is
  not a per-pixel ramp either: `FUN_00438800` subtracts
  `(colour >> 5) * (x0 - x1)` from the chain per segment and `FUN_00470e60`
  submits only the chain's near colour, so the trail is a flat per-record
  step (the 0xD8 chain draws 0xD7, 0xC5, 0xBF, ...) pre-faded by any
  overshoot past the window's right edge. Four more details came with it:
  - The line primitive itself. The original's line is a quad half a pixel
    either side of the segment (`MarniDX::DrawLine`, submitted as two
    triangles), and its colour bytes go through `byte / 256 * 255`, so every
    EKG channel draws one step down (0xD8 -> 0xD7, 0xFF -> 0xFE).
    `Framebuffer::draw_line` now rasterises that quad with the GPU's
    top-left rule instead of a Bresenham walk: a horizontal run keeps its
    lower row and drops the far column, a vertical run keeps the column left
    of the segment, and a centre exactly on a diagonal edge follows the
    edge's direction. That also brings the heal flush's rows onto the
    reference's 47 columns instead of 48.
  - The face flash `FUN_004387e0` threshold is a byte: the sweep's last four
    heads (0x80..=0x83) wrap `head - 0x35` to 0x4B..=0x4E, so the word keeps
    matching the danger span's tail rather than dropping to zero entries
    (`ekg_poison` t=204-205, 73 pixels).
  - A flush frame draws no condition word (case 2/3 replaces the whole
    monitor). The port also draws nothing before its first monitor tick; the
    reference cannot show that frame because case 0 falls through into case
    1's draw as it first runs.
  - The menu open resets `DAT_00ae9f48` to `0xFF`, so the *first* sweep's
    variant draw folds a 3 away instead of a 0. arklay's seed was 0, which
    picked the neighbouring wave whenever the first draw was 3 - visible
    only when a wave's spike came into the window (`itembox_open` t=153+,
    wave 15 against the reference's 12).
  With these, every pixel the monitor draws (face word included) is
  byte-identical on every frame of `inventory_open`, `inventory_nav`,
  `inventory_examine`, `inventory_open_close`, `itembox_open`,
  `itembox_page`, `file_open`, `file_pages`, `map_open`, `radio_use`,
  `combine_ok`, `combine_refuse`, `equip_weapon`, `use_herb`,
  `attacked_start` and the two new scenarios: `ekg_poison` (red poisoned
  EKG, the serum's blue cure flush, the green+red mix's white heal-and-cure
  flush) and `ekg_caution` (the orange and yellow states). The room keeps one
  pixel a fade rounds one step differently (`map_open` t=262 at (119,160):
  27,16,16 against 28,16,16); no EKG pixel differs in any run.

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
pixels, mean 0.2/255: the panel portrait).

`itembox_open` (which now runs the box cancel, exiting the whole menu through
`menu_exit_cleanup` and resuming the room) and `itembox_page` sit at that same
residual: their box frames differ by 441-513 pixels beyond the 8-step
threshold (mean error 0.19-0.26/255, down from ~10700 / 14.4-15% before) and
the opening fade hand-off frames by 443-469. Every pixel beyond the threshold
is in the panel's portrait - the box overlay, its list, its preview column,
the cursor, the swap and the health strip are pixel-equal - and the other
differing pixels are sub-threshold (the `0x70` dim rounding over the panel
art).

- **Herb combine prompt and MOVE close.** The MOVE confirm on a valid herb
  pair opens the original's `0xF5` yes/no prompt (`menu_item_check_combine`
  returns 2/3 without applying) and applies on Yes through
  `menu_item_apply_combine`; the answer reaches the menu one tick after the
  message task hides the window, and the four-tick MOVE-cursor retract runs
  before the submenu's eight-frame close. `combine_ok` and the green+green
  `combine_refuse` scenario drop from 7.8%/7.3% to the panel's portrait
  residual (~2%).
- **Pause-menu item viewer backdrop.** The radio-tab and CHECK viewers draw
  over the live menu; the reference's viewport there is black, while the
  standalone `--ui view` screen (a port-only capture) keeps the navy backdrop.
  `item_view` takes a `menu` flag and fills black for the overlay, dropping
  `radio_use`/`use_herb` from 22.3% to the portrait residual (~1.3-1.6%).
- **Door/room fade rounding.** `render::draw_fade_overlay` truncated the alpha
  blend; the reference rounds `(src*(255-a) + target*a) / 255`. The truncation
  left up to 80% of a fading door frame one LSB dark; `door_101` now matches
  all 411 frames byte-for-byte and `door_207` leaves only a 42-pixel residual.
- **Run-to-walk hand-off.** The release tick now finishes the run handler
  (`Joint_move`, run speed and steering) and writes the walk re-entry frame at
  its tail; the walk handler applies it on the next tick with the three-step
  blend. `walk_forward` drops from 1.41% on every frame after t=160 to a
  one-pixel-per-frame residual.

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

`bootcam_chris_104` covers the boot camera itself: the block's cut (2) is the
one the room had live when it was written while the saved position selects
cut 0, so the continue proves the load discards the saved id and re-homes
from camera 0 with no input at all (state and pixels match for 200 ticks).

## Open

- **Bare `--room` boot camera.** The headless room boot (`arklay --room ROOM
  --ticks/--capture/--trace`, `run_with_options`) places the player at the
  walk-zone spawn *before* its switch-zone scan, so it picks the camera at that
  point. The reference scans at the new-game position and only then moves the
  player to the walk-zone centre without re-scanning (`room_set`'s camera
  block, then `TestHooks_PlaceRoomSpawn`), which is what `--ui game --room`
  reproduces: room 104 boots `cam=0` bare and `cam=2` there, while the
  reference's room-mode trace says 2 in both cases. Left alone here because it
  only feeds the port's own headless captures (the m16 golden scenarios) and
  the oracle paths all use `--ui game`. `--save-out` on the bare path also
  writes the block before the camera scan, so an authored block carries the
  pre-scan id.
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
- **Room document pick-up (state 8) pacing.** The filed line now matches: the
  reader and the 0xC6 line sit on the original's black surround (no inventory
  panel), the state-8 decide cue plays, and the filed frames are byte-identical
  in a room-206 scenario (document `0x6E`, zone `[4190, 9110, 1800, 1800]`).
  The open and close still use the port's direct viewer instead of the
  reference's state-8 task: the reference spends one gameplay frame before the
  menu takes over, fades the room to black (`set_fading(2, 0xC00)`), and leaves
  the state-8 loop directly when the line clears, while arklay opens at the
  action's tick and closes through the pause menu's `0xC00` fade. An oracle
  scenario over the whole flow therefore still diverges on `fade` (32), the
  frozen player's `as`/`ticks` (~80/113) and the post-close `rng`/`rseed`
  follow-on (162, the extra ~10 frozen frames). Add
  `oracle/inputs/document_pickup.txt` and a room-206 save once that pacing is
  ported.
- **FILE selector animation.** The reader is matched, but the *selector's* own
  animation states (`FUN_00481d00` cases 2-12) are still the port's simplified
  slide: the reference slides the current cover out (to `-0x78` at doubling
  speed), swaps the book byte mid-slide and eases the new cover in from
  `+0x7f` at a halving speed with `FILE_BUSY` held throughout, and a cancel
  slides the cover down to `-0x68` before raising `FILE_EXITFLAG`; arklay
  changes the book immediately and slides the new cover from `+/-0x78` at a
  constant 9px/frame without a busy lock, and closes the tab instantly. No
  scenario exercises either (both cancels in the FILE scenarios are in the
  reader), so this is uncovered rather than measured.
- **Item box START.** The reference's `menu_itembox_interaction` tests START
  *held* before its state machine and exits the whole menu; the port's item
  box consumes the pad before `ui.start` is read, so START is silently lost
  while the box is up. No scenario presses it; low impact.
- **Tab-cycle menu close.** The port's documented Tab shortcut cycles the top
  tabs (`MainMenu::cycle_tab`); landing on the exit tab calls `close_menu`,
  which drops the panel instantly and silently. The reference has no tab-cycle
  button: START closes the whole menu through the state-1 exit
  (`play_sfx(3, 5)` once, then the `0xC00` fade). Kept as the port shortcut;
  the remaining cue gaps are tracked in #95.
- **MAP full-display zoom.** The browser's confirm switches to the full map
  display (`map_display_update`: the area plan's zoom sprites, the blue
  backdrop page and the browser's own composite dimmed underneath). No
  scenario reaches it - `map_open` only opens the browser, whose second
  confirm would be needed - and the port's single-swap `display_image`
  compositor stays the documented M16 simplification
  (`docs/m16-deviations.md` item 4).
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
- **Room/player frame backlog — resolved by #69.** The ~4.9% room-close
  baseline (player-mesh edges) was the software rasteriser's approximation of
  the reference's lit 3D geometry. With the llvmpipe-exact rasteriser and the
  reference's per-position lighting, `scripts/oracle-sweep.sh` reports 320/320
  runs with state MATCH and no run above 0.1% differing pixels (worst 512 p0 at
  0.05%), so the room-entry frames in `file_open`/`file_pages` are no longer a
  separate backlog item.

## Reaching more screens

Every screen is reachable through the save-mode oracle: the setup chunk in
`oracle/saves/*.lua` authors the inventory, item box, health, flags, files,
keys and position the interaction needs, and both engines boot the same block
(`--load` / `--test-load`). The room-mode capture seams (`--ui box`/`file`/
`map`/`view`) remain arklay-only and cannot replace the differential oracle,
because the reference has no equivalent.

The checklist runs above used `oracle/inputs/*.txt`; the saves they boot are
listed in `oracle/inputs/README.md`.
