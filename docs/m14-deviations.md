# M14 integration deviations

The FMV milestone is complete for the scripted, no-enemy scope it set: the AVI
demuxer, the Cinepak decoder, the packed films, the playback session, the
`movie_on` handshake and the opening/ending film chains. The following
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

2. **Skip grace follows the screen's frame limiter (closed).** The original's
   100-update grace advances once per platform frame: ~33 ms (3.3 s) during
   gameplay, the prologue and the boot logos, and ~16 ms (1.6 s) on the title
   opening and the ending screens. The port now polls the skip state once per
   render frame at the film's own period (`movie::skip_period_ms`,
   `MovieSession::poll_skip`) instead of once per 30 Hz tick, with the grace
   and the per-film masks still unit-tested.

3. **No subtitle or credits overlays.** The PS1 releases draw FMV subtitles and
   an ending-credits overlay over the films; neither ships in this PC install
   and neither is ported. The films play bare, and the ending path stops after
   the film chain: no RESULT screen, no rocket-launcher epilogue, no
   next-cycle save carryover. `--ending` exists precisely because those
   screens are out of scope.

4. **Wall-clock-mastered film pacing.** The film time comes from the elapsed
   wall clock; with a real device the mixer's consumed-sample cursor is
   followed only while it stays within the original's tolerance (-0.5 s behind
   to +0.25 s ahead), and the session decodes forward to the frame due at the
   adopted time, one or several at once. A lagging device queue therefore
   cannot slow the picture past that window. Without a device (captures, the
   SDL dummy driver) the wall clock / fixed 30 Hz ticks are the clock: 10 fps
   films advance every three ticks and 15 fps films every two. The counters
   (`movie_samples_consumed`, `samples_before`) are exposed for tests, and the
   `samples_consumed` path of `MovieSession::tick` remains the deterministic
   unit-test seam.

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
