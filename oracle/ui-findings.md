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
  With this, `inventory_open`/`inventory_nav`/`inventory_examine` match tick-for-tick
  except the menu `fade` field, and the shared PRNG stays in step.
- **Missing menu-open frame seed.** On the frame START opens the menu the
  reference still draws `g_RandSeed = rand()` (game_loop's first statement)
  before it detects the request; arklay opened the menu ahead of the room tick
  and skipped the draw. `GameState::draw_frame_rand` on the open path.

## Open (found, not yet fixed)

- **Menu-open fade timing.** The reference shows `game_loop` suspended, so its
  menu-open screen is the menu's black background with the fade over it: the
  screen is black ~2 frames after START. arklay keeps the frozen room and
  darkens it over the whole `0xC00` fade (`render` skips the menu while
  `MenuFadePhase::Out`), so the two agree again only once the menu has faded in
  (~14 frames). The trace's `fade` field also differs (arklay's menu fade is a
  separate accumulator from the room-entry fade the trace reads).
- **Item-action submenu open animation.** The original grows the submenu box
  from zero to full (`DAT_00ae9f27` `0x80..=0x88`, width `*6`, height `*3`)
  over ~8 frames after confirm. arklay draws the full box immediately, so the
  examine/use/equip submenu differs by ~3-5% of pixels for those 8 frames.
- **EKG line rasterization.** The residual settled-screen difference (~0.3-1%)
  is the health monitor's EKG line: arklay rasterizes a Bresenham polyline, the
  original fills per-column vertical spans, so steep segments cover a different
  pixel set.
- **Portrait face edges.** ~55 pixels in the character portrait differ at a
  fine threshold (edge/rounding level, like the room frame backlog).

## Reaching more screens

The new-game inventory is only the Combat Knife and one other item, and the
map/file/radio tabs need scenario items a fresh game does not carry, so the
pause menu is the only screen reachable with `--test-room`. The item box needs
a physical box object `--test-room` does not place the player at.
