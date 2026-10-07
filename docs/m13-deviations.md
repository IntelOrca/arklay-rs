# M13 integration deviations

The M13 audio layer is complete for the scripted, no-enemy scope the milestone
set. The following behaviours deliberately differ from the original; each one
is a bounded no-op or a documented approximation rather than a silent failure.

1. **Weapon banks (bank 1).** Weapon progression is out of scope, so the
   weapon/menu bank is never loaded. The 12 shipped `se_play_3d` sites that
   name it (the attic rooms 303/3031 ids 7/8) resolve to a typed no-op and are
   counted in the corpus audit.

2. **Enemy position type 2.** No enemy entity ever allocates, so a request
   whose position type names an enemy slot is counted in
   `snd3d_enemy_drops` and dropped. The 80 shipped sites are all scripted
   cues.

3. **Room action effect zones (0x0B).** `room_action_effect` raises the
   `MSF2_EFFECT_ZONE` bit while the player stands in its zone, so footsteps
   shift their room-table column by -3 like the original. The dust billboards
   the original also spawns under a moving player are cosmetic and are not
   drawn.

4. **Scripted fade scope.** `snd_fade_set` steps the three BGM channels down
   and then stops the voice line, but the original's `UpdateSoundFade` also
   sweeps the SFX, room-SFX, character and enemy banks. Those banks are
   one-shots in the port, whose gains are fixed at play time, so the fade does
   not reach them; the BGM and voice behaviour matches.

5. **Unreferenced voice files.** 47 shipped `voice/*.WAV` files (6.0 MiB) are
   not named by any stage table row and stay out of the pack. The conversion
   summary lists them; a script that somehow named one records a miss and never
   raises the F7 wait, so it cannot deadlock.

6. **FMV (`movie_on`, 0x29).** Films are out of scope: the opcode keeps its
   graceful no-op, so a scene that requests one advances instead of waiting.
   No AVI demuxer, Cinepak decoder or attract mode is part of this milestone.

7. **Costume model swap.** `costume_set`/`costume_ck` (0x4F/0x50) store and
   branch on the original's one-byte variant in the wardrobe rooms (11C in
   stages 1 and 6), but the alternate player model (`em1030`/`em1031`) is not
   loaded: the port's player assets resolve the base `player/{character}.emd`
   only, so the outfit does not visibly change. The model selection is left for
   the milestone that owns the outfit/menu path.
