# M14 integration deviations

The FMV milestone is complete for the scripted, no-enemy scope it set: the AVI
demuxer, the Cinepak decoder, the packed films, the playback session, the
`movie_on` handshake and the opening/ending film chains. The following
behaviours deliberately differ from the original; each one is a bounded
approximation or an explicitly unreachable path rather than a silent failure.
This document supersedes item 8 of `docs/m13-deviations.md` (FMV out of scope).

1. **Attract demo and death film deferred.** The four `DATA/Pdemo*.dat` attract
   recordings (recorded pad input replayed over monster-filled scenes) and the
   death film (id 2, `DIED.TIM`'s trigger) are not packed or played at M14:
   the attract path needed the title idle timer, and the death trigger needed
   the damage/combat path this milestone excludes. The film tables and skip
   masks carry every row, so packing the content later needs no table change.

   *Since M14:* the attract reels and the title idle timer are implemented
   (`src/demo.rs`), and the death flow opens the DIED screen from the combat
   path (E11b). The death film itself remains unpacked.

2. **Skip grace spent at the film gate's rate.** The original's 100-update
   grace advances once per platform loop iteration, and while a film owns the
   screen the platform loop runs its film gate instead of the frame limiter, so
   the grace expires within milliseconds of playback starting. The port polls
   the skip state once per render frame (`MovieSession::poll_skip`), latches
   the pad on the first poll and accepts an unmasked press on any later poll,
   which is the reference's observable behaviour; a button held from before
   the film cannot skip it. The per-film masks stay unit-tested, and the
   fixed-tick `MovieSession::tick` path spends one update per call for headless
   playback.

3. **No subtitle or credits overlays.** The PS1 releases draw FMV subtitles and
   an ending-credits overlay over the films; neither ships in this PC install
   and neither is ported. The films play bare; the post-film sequence that
   follows them (the RESULT screen, the rocket-launcher epilogue and the
   next-cycle save) is implemented - see item 9.

4. **Wall-clock-mastered film pacing.** The film time comes from the elapsed
   wall clock; with a real device the mixer's device-playback cursor (frames
   written to the stream minus frames still queued, the port's equivalent of
   the original's `waveOutGetPosition`) is followed only while it stays within
   the original's tolerance (-0.5 s behind to +0.25 s ahead), and the session
   decodes forward to the frame due at the adopted time, one or several at
   once. The session keeps a two second audio lead queued ahead of the due
   frame, so the playback cursor never starves while the picture waits on a
   decode; a lagging device queue therefore cannot slow the picture past the
   tolerance window. Without a device (captures, the SDL dummy driver) the wall
   clock / fixed 30 Hz ticks are the clock: 10 fps films advance every three
   ticks and 15 fps films every two. The counters (`movie_samples_played`,
   `movie_samples_consumed`, `samples_before`) are exposed for tests, and the
   `samples_consumed` path of `MovieSession::tick` remains the deterministic
   unit-test seam.

5. **Endings unreachable through normal play.** Reaching an ending requires the
   excluded combat and bosses, so `ending::select_id` and `ending::chain` are
   pure functions and the only way to see the sequence is the `--ending <id>`
   debug entry (with `--character`), which now runs the films, the RESULT
   screen, the epilogue and the next-cycle save end to end. The seven-row
   table, the congratulations film rules (plate/character/infinite
   launcher/second playthrough) and the staff-roll flag are transcribed and
   unit-tested, but no gameplay path sets the survivor flags or reaches
   `ending_state`: the post-film screen sequence and the flow's carried bits
   are wired, but only the debug entries drive them.

6. **Automatic films only on the interactive root boot.** The boot logos (28
   then 23), the title opening (0, once per process) and the character intro
   (1) play only when the app is launched without `--ui`, `--room` or
   `--capture`. Captures and `--ui` boots skip every automatic film so their
   deterministic BMPs do not change; the root-boot capture still boots the
   title directly. The absent `vlogo.avi` is logged and skipped by the same
   missing-file path as any unpacked film. `convert-game` resolves the Western
   release's alternate film names (`OU`/`PU`, `EU4`/`EU5` and the `_r` staff
   rolls) to the canonical pack entries, and the Virgin logo, which no digital
   PC release ships, is not reported as a missing conversion input.

7. **Headless drains.** A capture's `--ticks` loop and `simulate_room` take and
   drop every film request, so a `movie_on` can never stall a headless run. A
   pack without the film logs each unavailable film and continues the room;
   the corpus audit asserts zero `0x29` placeholders, zero filtered ids and at
   least one drained request.

8. **Golden decode oracle provenance.** The strongest decode test,
   `decoded_frames_match_the_independent_goldens`, compares sampled frames
   against CRC-32 hashes and BMPs selected by `ARKLAY_MOVIE_GOLDEN`. The
   committed test cannot verify how those goldens were produced: the sets used
   so far come from a same-author Python Cinepak reimplementation rather than a
   third-party decoder (none is installed in this environment), so treat them
   as a cross-check between two implementations sharing a spec, not as fully
   independent verification. The test fails when the variable is set but the
   golden directory, hash file or a sampled BMP is missing; it prints an
   explicit notice and skips only when the variable is unset.

9. **Post-ending sequence approximations.** The RESULT screen and the
   rocket-launcher epilogue follow `ending_result_*`/`ending_epilogue_*`
   (`EndingScreen.cpp:327-836`): the five baked plates from `en05.tim`/`en07.tim`,
   the per-slot `0x800` fade ramps, the step-4 input gate (any button or the
   `0x708` timeout), the ending-6 fifth plate, the two epilogue captions, the
   `rc1121.pix` backdrop, the root->spin->leaf composition of `i73v.ivm` and
   the next-cycle card's grants and flags. Six details are bounded
   approximations:

   - The no-save confetti (`Effect_CreateBillboard(0x0B, 0x0A)`) is a
     deterministic particle burst drawn by the screen, not the original's
     effect-pool billboard; its `play_sfx` pair is not played because the
     app-level screens have no bank-slot sound API (only named UI cues).
   - The ending-6 fifth plate's additive blend (`tex.flags & 0x40000000`) is
     drawn opaque.
   - The play timer advances only while the room is live; the reference also
     ticks it in the pause menu (`MainMenu.cpp:428`), so a menu-heavy run's
     clear time and the four-hour Ingram/Minimi cutoff can differ.
   - The next-cycle save is written by the port's normal save screen; the
     `s_grantRocket` "already infinite" input is carried by the ending flow
     (a real clear, which is unreachable, would read it from the live run).
   - The clear-time and save-count strings use the reference's own grid
     addressing over the shipped font sheet, including the raw `0x5E`
     separator (`ending_draw_text`), rather than the port's encoded-text path,
     so the digits land cell for cell on the original's.
   - The epilogue's model is lit by the sequence's own record
     (`FUN_0040ac80(0, ...)` with `(100,100,100)` and `setBackColor(0xFFF,
     0xFFF, 0xFFF)`), but only light 0 is set: the reference never rewrites
     slots 1 and 2, so they keep whatever the last room or menu left in
     `g_d3dLightData`, which a boot straight into the ending cannot
     reproduce. With the ambient already at full scale their contribution is
     at most the last step to saturation.

   No clear count is persisted (the port has no config/settings store), and the
   Director's Cut epilogue guards (advanced difficulty, the infinite Colt
   Python) are absent with the rest of the DC branches.
