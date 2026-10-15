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
- **Menu fade blend.** The black veil uses the original's
  `(256 - alpha) / 256` destination weight with round-to-nearest
  (`Rendering.cpp:244`), not `/255`. The one-LSB mismatch previously diverged
  every non-black pixel of a fading screen; it fixed the room-entry fade, the
  menu fade-in and the room fade after the close.
- **Draw-then-add ordering.** The menu and room fades capture the pre-step
  accumulator for the frame's draw (`menu_fade_draw` / `room_fade_draw`) while
  the traced state stays the post-step value, so both the pixels and the trace
  match the reference.
- **Menu fade accumulator setup.** The opening and closing fades seed the
  accumulator from the room-entry fade's negative leftover (`set_fading` leaves
  it), reset it on their first step, and the closing fade clears it to the
  original's `0:0` before a two-frame menu-task teardown.
- **Menu exit and open pacing.** The closing fade restores the player to idle on
  the first frame after the cancel (the original's exit state 7), and the menu
  holds one frame after the fade before its first health-bar tick (the original's
  state-0 -> state-1 frame).
- **Item-action submenu open animation.** The original grows the submenu box
  from zero to full (`DAT_00ae9f27` `0x80..=0x88`, width `*6`, height `*3`)
  over eight frames after confirm. `MainMenu::submenu_frames` scales
  `draw_submenu` and gates the action input until it is open.

With these, `inventory_open` and `inventory_examine` match tick-for-tick
(`MATCH: N ticks identical`) and their frames are at the residual level below.

## Open

- **Menu close, player idle counter.** `inventory_open_close` (cancel at tick
  120) now matches through t=134; from t=135 the player's idle settle
  `action_ticks_counter` (`e0.ticks`) diverges: the reference holds 99 for five
  frames before counting down, arklay decrements immediately. Frames: t=120-130
  byte-identical, t=134/136 ~22%/15% (the room fade-in's one-step alpha), then
  the room baseline (~4.9%).
- **EKG line rasterization.** The residual settled-screen difference (~0.3-1%)
  is the health monitor's EKG line: arklay rasterizes a Bresenham polyline. The
  reference's `FUN_00470c60` queues a 1px-wide quad that `MarniDX_GL::DrawLine`
  renders as two alpha-blended triangles through the (llvmpipe) GPU, so the
  covered pixels differ on steep segments. Not exactly matchable without
  replicating the GPU quad coverage.
- **Portrait face edges.** ~55 pixels in the character portrait differ at a
  fine threshold (edge/rounding level, like the room frame backlog).

## Reaching more screens

The new-game inventory is only the Combat Knife and one other item, and the
map/file/radio tabs need scenario items a fresh game does not carry, so the
pause menu is the only screen reachable with `--test-room`. The item box needs
a physical box object `--test-room` does not place the player at.
