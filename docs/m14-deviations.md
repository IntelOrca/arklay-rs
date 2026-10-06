# M14 integration deviations

The FMV milestone is complete for the scripted, no-enemy scope it set: the AVI
demuxer, the Cinepak decoder, the optional movie pack, the playback session,
the `movie_on` handshake and the opening/ending film chains. The following
behaviours deliberately differ from the original; each one is a bounded
approximation or an explicitly unreachable path rather than a silent failure.
This document supersedes item 8 of `docs/m13-deviations.md` (FMV out of scope).

1. **Attract demo and death film deferred.** The four `DATA/Pdemo*.dat` attract
   recordings (recorded pad input replayed over monster-filled scenes) and the
   death film (id 2, `DIED.TIM`'s trigger) are not packed or played: the
   attract path needs the title idle timer, and the death trigger needs the
   damage/combat path this milestone excludes. The title therefore waits on
   PRESS forever, with its `0x708` idle countdown still the documented TODO
   from M13. The film tables and skip masks carry every row, so packing the
   content later needs no table change.

2. **Skip grace at the port's tick rate.** The original's 100-count grace
   window is transcribed exactly, but the port decrements it on its fixed
   30 Hz tick, so every film becomes skippable after ~3.3 s. In the original
   the counter advances with the main loop, which is paced at ~33 ms during
   gameplay and ~16 ms on the boot, title and ending screens: a gameplay film
   matches the port's ~3.3 s, while a boot/title/ending film becomes skippable
   after ~1.7 s. The grace and the per-film masks are constants with unit
   tests; the interactive sweep confirms the feel.

3. **No subtitle or credits overlays.** The PS1 releases draw FMV subtitles and
   an ending-credits overlay over the films; neither ships in this PC install
   and neither is ported. The films play bare, and the ending path stops after
   the film chain: no RESULT screen, no rocket-launcher epilogue, no
   next-cycle save carryover. `--ending` exists precisely because those
   screens are out of scope.

4. **Audio-led tolerance.** With a real device the mixer keeps about 93 ms
   (~2048 stereo frames) queued and presents each frame on the consumed-sample
   clock with one frame queued ahead, so drift stays under one frame. Without
   a device (captures, the SDL dummy driver) the fixed 30 Hz tick is the clock:
   10 fps films advance every three ticks and 15 fps films every two. The
   counters (`movie_samples_consumed`, `samples_before`) are exposed for tests,
   and the audio/video relationship is an approximation of the original's
   MCI-driven playback.

5. **Endings unreachable through normal play.** Reaching an ending requires the
   excluded combat and bosses, so `ending::select_id` and `ending::chain` are
   pure functions and the only way to see the sequence is the `--ending <id>`
   debug entry (with `--character`). The seven-row table, the congratulations
   film rules (plate/character/infinite launcher/second playthrough) and the
   staff-roll flag are transcribed and unit-tested, but no gameplay path sets
   the survivor flags or reaches `ending_state`.

6. **Automatic films only on the interactive root boot.** The boot logos (28
   then 23), the title opening (0, once per process) and the character intro
   (1) play only when the app is launched without `--ui`, `--room` or
   `--capture`. Captures and `--ui` boots skip every automatic film so their
   deterministic BMPs do not change; the root-boot capture still boots the
   title directly. The absent `vlogo.avi` is logged and skipped by the same
   missing-file path as any unpacked film.

7. **Headless drains.** A capture's `--ticks` loop and `simulate_room` take and
   drop every film request, so a `movie_on` can never stall a headless run. A
   run without the movie pack logs each unavailable film and continues the
   room; the corpus audit asserts zero `0x29` placeholders, zero filtered ids
   and at least one drained request.

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
