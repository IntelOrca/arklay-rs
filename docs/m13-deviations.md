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

3. **Monster-AI room columns.** The sparse bank-2 room table transcribes only
   the columns the shipped scripts reach plus the six extra names they use.
   The monster-AI columns 0-9 stay absent; the corpus reaches them exactly
   three times (bank-2 ids 3/7 in rows 67 and 180), and those play nothing
   rather than a wrong sound.

4. **One-shot vs restart mixer semantics.** The original restarts one buffer
   per bank; this mixer appends one-shot voices, so two identical cues can
   overlap. Scripted bursts are serialised by the queued consumer, and the
   audible difference is limited to rapid repeats.

5. **Pan law.** `sfx::sound_gain_pan` uses the shipped room-table pair through
   the existing millibel mapping rather than reproducing the original's exact
   per-sample pan curve; the direction and the endpoints match, the
   intermediate curve is an approximation.

6. **Unreferenced voice files.** 47 shipped `voice/*.WAV` files (6.0 MiB) are
   not named by any stage table row and stay out of the voice pack. The
   conversion summary lists them; a script that somehow named one records a
   miss and never raises the F7 wait, so it cannot deadlock.

7. **FMV (`movie_on`, 0x29).** Films are out of scope: the opcode keeps its
   graceful no-op, so a scene that requests one advances instead of waiting.
   No AVI demuxer, Cinepak decoder or attract mode is part of this milestone.

8. **Costume model swap.** `costume_set`/`costume_ck` (0x4F/0x50) store and
   branch on the original's one-byte variant in the wardrobe rooms (11C in
   stages 1 and 6), but the alternate player model (`em1030`/`em1031`) is not
   loaded: the port's player assets resolve the base `player/{character}.emd`
   only, so the outfit does not visibly change. The model selection is left for
   the milestone that owns the outfit/menu path.
