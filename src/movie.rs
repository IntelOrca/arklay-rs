//! The film table and the playback session.
//!
//! The original raises a film request through `movie_on` (0x29) and hands it
//! to a four-state player: initialise, open and start, monitor, clean up. This
//! module owns the port's equivalent: the 29-id name table with its per-film
//! skip masks, the prologue cut points and a [`MovieSession`] that demuxes one
//! film from the pack, decodes it frame by frame and produces the film's own
//! PCM for the mixer.
//!
//! Pacing is wall-clock-led while a device is open: the film time comes from
//! the elapsed wall clock, and the mixer's device-playback cursor (the film
//! frames written to the stream minus the frames still queued on it) is
//! accepted only while it sits within the original's tolerance of that clock
//! (-0.5 s to +0.25 s), so a lagging device queue cannot slow the picture. The
//! playback cursor, not the render cursor, is the clock: the mixer pre-renders
//! into the device queue, so its rendered count races ahead of what is audible
//! and stalls whenever the film's own one-frame-ahead queue runs dry. A two
//! second audio lead keeps the device queue fed across the decode jitter. The
//! session decodes forward to the frame due at the adopted time, one or
//! several at once, and pads a short audio track with silence. The fixed 30 Hz
//! tick ([`MovieSession::tick`]) is the deterministic no-device/capture clock:
//! three ticks per 10 fps frame and two per 15 fps frame.
//!
//! The skip state is the original's: a per-film mask, the 100-update grace and
//! an edge-detected button latch. The grace is spent once per platform loop
//! iteration, and while a film owns the screen the reference leaves its frame
//! limiter through the film gate, so those iterations run unbounded and the
//! grace expires within milliseconds of the film starting. The port's
//! platform loop is the render loop (one poll per rendered frame), so it
//! spends the grace on the latch: the first poll latches the pad and any
//! unmasked edge on a later poll skips the film. A button held from before
//! the film therefore cannot skip it, and a single fresh press always can. A
//! mask of zero makes a film unskippable.
//!
//! Id 1 (the intro) drops the Chris-only beat for Jill: the session presents
//! up to the first cut point, decodes the removed frames to keep the Cinepak
//! canvas exact and resumes presenting and queueing audio at the second cut
//! point, which is what the original's restart-at-cut-end achieves.

use anyhow::{Context, Result, bail};

use crate::audio::SAMPLE_RATE;
use crate::avi::{AudioFormat, Avi};
use crate::cinepak::Decoder;
use crate::pack::Pack;

/// Film ids and their lower-case basenames, in id order 0..=28. Id 10 is the
/// original's null entry and has no file.
static NAMES: [Option<&str>; 29] = [
    Some("oj"),
    Some("pj"),
    Some("dmf"),
    Some("dm3"),
    Some("dm4"),
    Some("dm1"),
    Some("dm6"),
    Some("dm7"),
    Some("dm8"),
    Some("dm2"),
    None,
    Some("dmb"),
    Some("dmc"),
    Some("dmd"),
    Some("dme"),
    Some("ed1"),
    Some("ed2"),
    Some("ed3"),
    Some("ed4"),
    Some("ed5"),
    Some("ed6"),
    Some("ed7"),
    Some("ed8"),
    Some("capcom"),
    Some("stfc_b"),
    Some("stfj_b"),
    Some("stfz_b"),
    Some("staf_b"),
    Some("vlogo"),
];

/// The unskippable ids: the death film, the pre-ending film, the eight
/// endings and the four staff rolls carry a zero mask; every other entry
/// accepts any button.
static UNSKIPPABLE: [u8; 14] = [2, 14, 15, 16, 17, 18, 19, 20, 21, 22, 24, 25, 26, 27];

/// The prologue cut window in milliseconds (the JPN/US PC intro, id 1).
const PROLOGUE_CUT_START_MS: u32 = 177_800;
/// The prologue cut resume point in milliseconds.
const PROLOGUE_CUT_END_MS: u32 = 188_500;
/// The intro's id.
const PROLOGUE_ID: u8 = 1;
/// The film audio kept queued above the frame due now, in seconds. The
/// reference decodes the whole track up front, so its waveOut cursor never
/// starves; a two second lead keeps the port's device queue fed without
/// holding a whole film's PCM. See [`MovieSession::advance_due`].
const AUDIO_LEAD_SECONDS: u32 = 2;

/// The lower-case basename of film `id`, or `None` for the null entry and ids
/// past the table.
pub fn name(id: u8) -> Option<&'static str> {
    NAMES.get(usize::from(id)).copied().flatten()
}

/// The number of named film ids the table carries (29 rows minus the null id).
pub fn name_count() -> usize {
    NAMES.iter().filter(|name| name.is_some()).count()
}

/// Pack path for a film basename: `movie/{name}.avi`.
pub fn pack_path(name: &str) -> String {
    format!("movie/{}.avi", name.to_ascii_lowercase())
}

/// The accept/skip button mask of film `id`: `0x0FFF` for every skippable
/// film and `0x0000` for the unskippable class. Ids past the table default to
/// unskippable, like the original's bounds check.
pub fn skip_mask(id: u8) -> u16 {
    if id <= 28 && !UNSKIPPABLE.contains(&id) {
        0x0FFF
    } else {
        0x0000
    }
}

/// The prologue cut window for film `id` and `character` (0 Chris, 1 Jill):
/// `Some((start, end))` in milliseconds when the Chris-only beat is removed.
pub fn prologue_cut_ms(id: u8, character: u8) -> Option<(u32, u32)> {
    (id == PROLOGUE_ID && character & 1 == 1)
        .then_some((PROLOGUE_CUT_START_MS, PROLOGUE_CUT_END_MS))
}

/// The frame index a millisecond cut falls on for `frame_rate` (num/den),
/// rounded down.
pub fn frame_at_ms(ms: u32, frame_rate: (u32, u32)) -> usize {
    let (num, den) = frame_rate;
    if den == 0 {
        return 0;
    }
    (u64::from(ms) * u64::from(num) / (1000 * u64::from(den))) as usize
}

/// The 30 Hz ticks one frame spans at `frame_rate`.
pub fn ticks_per_frame(frame_rate: (u32, u32)) -> u64 {
    let (num, den) = frame_rate;
    if num == 0 {
        return 0;
    }
    30 * u64::from(den) / u64::from(num)
}

/// What one [`MovieSession::tick`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovieTick {
    /// No frame was due yet.
    Waiting,
    /// The film advanced to its next frame.
    Advanced,
    /// The film reached its end.
    Finished,
    /// A skip button ended the film.
    Skipped,
}

/// One film being played: the demuxed file, the Cinepak canvas, the audio queue
/// and the skip state.
pub struct MovieSession {
    /// The film's table id.
    id: u8,
    /// The character the session opened for (0 Chris, 1 Jill); selects the
    /// prologue cut and travels with the session for chain reporting.
    character: u8,
    avi: Avi,
    decoder: Decoder,
    /// The RGBA scratch [`MovieSession::frame_rgba`] hands out.
    rgba: Vec<u8>,
    /// Output frame index currently presented, counted over the kept frames.
    presented: usize,
    /// Frames this session presents: the AVI count minus a removed beat.
    kept_frames: usize,
    /// The removed beat in AVI frame indices (`start`, `end`), if any.
    cut: Option<(usize, usize)>,
    /// Next AVI frame index to decode.
    decoded_through: usize,
    /// Kept frames whose audio was queued, exclusive.
    audio_queued: usize,
    /// Audio the mixer has not taken yet.
    pending_audio: Vec<i16>,
    /// The film's accept/skip button mask.
    skip_mask: u16,
    /// Whether the first poll still has to latch the pad without an edge.
    latch_pending: bool,
    /// The pad word at the previous poll, for edge detection.
    last_buttons: u16,
    /// Kept frames of audio kept queued ahead of the presented frame; see
    /// [`AUDIO_LEAD_SECONDS`].
    audio_lead: usize,
    /// Whether the interactive loops own the skip sampling. [`Self::poll_skip`]
    /// sets it, so once a loop polls each render frame the session's own
    /// [`Self::tick`] never samples as well; a headless caller that only ticks
    /// leaves it clear and the tick drives the state machine.
    external_poll: bool,
    /// The AVI frame rate (num/den), the wall-clock film clock's frame length.
    frame_rate: (u32, u32),
    /// Elapsed wall clock for the timed path, in milliseconds.
    wall_ms: f64,
    /// 30 Hz ticks advanced, the no-device clock.
    ticks: u64,
    /// Output samples one presented frame spans.
    samples_per_frame: u64,
    /// 30 Hz ticks one presented frame spans.
    ticks_per_frame: u64,
    /// The film ended normally.
    finished: bool,
    /// The film ended through a skip.
    skipped: bool,
}

impl MovieSession {
    /// Resolve `id`, read `movie/{name}.avi` from `pack`, parse it, check the
    /// format and arm playback at frame 0.
    pub fn open(pack: &Pack, id: u8, character: u8) -> Result<Self> {
        let name = name(id).with_context(|| format!("film id {id} has no file"))?;
        let entry = pack_path(name);
        let bytes = pack
            .read(&entry)
            .with_context(|| format!("missing film {entry}"))?
            .to_vec();
        let avi = Avi::parse(bytes).with_context(|| format!("failed to parse {entry}"))?;
        Self::from_avi(avi, id, character)
    }

    /// Build a session over an already parsed AVI.
    fn from_avi(avi: Avi, id: u8, character: u8) -> Result<Self> {
        let format = avi.format().clone();
        if format.audio.channels != 2 {
            bail!(
                "film id {id} has {} audio channels, expected stereo",
                format.audio.channels
            );
        }
        match (format.audio.sample_rate, format.audio.bits_per_sample) {
            (22_050, 16) | (44_100, 8) => {}
            (rate, bits) => bail!("film id {id} has unsupported audio: {rate} Hz {bits}-bit PCM"),
        }

        // A cut past the end of the film is ignored, never fatal: the request
        // plays the whole film.
        let cut = prologue_cut_ms(id, character).and_then(|(start, end)| {
            let start = frame_at_ms(start, format.frame_rate);
            let end = frame_at_ms(end, format.frame_rate);
            (end > start && end <= avi.frame_count()).then_some((start, end))
        });
        let kept_frames = avi.frame_count() - cut.map_or(0, |(start, end)| end - start);
        if kept_frames == 0 {
            bail!("film id {id} has no frames");
        }

        let samples_per_frame = avi.samples_per_frame() as u64 * u64::from(SAMPLE_RATE)
            / u64::from(format.audio.sample_rate.max(1));
        let (rate_num, rate_den) = format.frame_rate;
        let audio_lead =
            (AUDIO_LEAD_SECONDS as usize * rate_num as usize / rate_den.max(1) as usize).max(2);
        let mut session = MovieSession {
            id,
            character: character & 1,
            avi,
            decoder: Decoder::new(320, 240)?,
            rgba: vec![0u8; 320 * 240 * 4],
            presented: 0,
            kept_frames,
            cut,
            decoded_through: 0,
            audio_queued: 0,
            pending_audio: Vec::new(),
            skip_mask: skip_mask(id),
            latch_pending: true,
            last_buttons: 0,
            audio_lead,
            external_poll: false,
            frame_rate: format.frame_rate,
            wall_ms: 0.0,
            ticks: 0,
            samples_per_frame,
            ticks_per_frame: ticks_per_frame(format.frame_rate),
            finished: false,
            skipped: false,
        };
        session.ensure_audio_through(session.audio_lead);
        session
            .present_kept(0)
            .with_context(|| format!("failed to decode film id {id} frame 0"))?;
        Ok(session)
    }

    /// The AVI frame index kept frame `kept` presents, jump-corrected over the
    /// removed prologue beat.
    fn avi_index(&self, kept: usize) -> usize {
        match self.cut {
            Some((start, end)) if kept >= start => kept + (end - start),
            _ => kept,
        }
    }

    /// Decode every AVI frame up to and including kept frame `kept`, then make
    /// its RGBA canvas current.
    fn present_kept(&mut self, kept: usize) -> Result<()> {
        let target = self.avi_index(kept);
        while self.decoded_through <= target {
            let frame = self
                .avi
                .video(self.decoded_through)
                .with_context(|| format!("AVI frame {} is missing", self.decoded_through))?;
            self.decoder
                .decode(frame)
                .with_context(|| format!("AVI frame {} failed to decode", self.decoded_through))?;
            self.decoded_through += 1;
        }
        self.presented = kept;
        self.decoder.rgba_into(&mut self.rgba);
        Ok(())
    }

    /// Queue the audio of every kept frame below `exclusive` (clamped to the
    /// kept count), in order.
    fn ensure_audio_through(&mut self, exclusive: usize) {
        let exclusive = exclusive.min(self.kept_frames);
        while self.audio_queued < exclusive {
            let index = self.avi_index(self.audio_queued);
            let samples = self.frame_audio(index);
            self.pending_audio.extend_from_slice(&samples);
            self.audio_queued += 1;
        }
    }

    /// One kept frame's audio as interleaved stereo at [`SAMPLE_RATE`],
    /// converted from the AVI stream and padded to exactly one frame's worth of
    /// silence when the track ended early.
    fn frame_audio(&self, avi_index: usize) -> Vec<i16> {
        let samples = self.samples_per_frame as usize;
        let mut out = match self.avi.audio(avi_index) {
            Some(bytes) => decode_pcm(bytes, &self.avi.format().audio, samples),
            None => Vec::new(),
        };
        out.resize(samples * 2, 0);
        out
    }

    /// Poll the skip state with the currently held pad word. Returns `true`
    /// when the film was skipped.
    ///
    /// The original calls its film state machine once per platform loop
    /// iteration, which during a film is the unbounded film gate rather than
    /// the frame limiter, so its 100-update grace is spent within milliseconds
    /// of playback starting. The first poll latches the pad without comparing
    /// it; every later poll accepts an unmasked, edge-detected press. Calling
    /// this makes the session the sampler, so the fixed ticks of the same
    /// frame do not sample a second time ([`Self::tick`] drives the state
    /// machine only for headless callers).
    pub fn poll_skip(&mut self, buttons: u16) -> bool {
        self.external_poll = true;
        self.advance_skip(buttons)
    }

    /// One update of the skip state machine: latch the first sample, then
    /// accept an unmasked press edge against the previous sample.
    ///
    /// The edge is detected sample to sample (`mask & !previous & current`) on
    /// the raw held word, so a press that arrives and is released between two
    /// samples is still caught by the next sample, and only a genuinely held
    /// button produces no edge.
    fn advance_skip(&mut self, buttons: u16) -> bool {
        if self.finished {
            return self.skipped;
        }
        if self.latch_pending {
            self.latch_pending = false;
            self.last_buttons = buttons;
            return false;
        }
        let pressed = self.skip_mask & !self.last_buttons & buttons;
        self.last_buttons = buttons;
        if pressed != 0 {
            self.finished = true;
            self.skipped = true;
            return true;
        }
        false
    }

    /// The next tick of the 30 Hz clock (or the consumed-sample clock when
    /// `samples_consumed` is given) and the pad word `buttons`.
    ///
    /// This is the deterministic no-device/capture seam: without a device the
    /// fixed tick clock presents every `ticks_per_frame` ticks, and the
    /// consumed-sample counter remains a direct frame clock for the unit
    /// tests. Interactive playback uses [`Self::tick_timed`], where the wall
    /// clock is the master. The first call latches `buttons` without an edge,
    /// exactly like the original's start state; later calls end the film on an
    /// unmasked button edge. An interactive loop that already polled the skip
    /// state reaches the same state through [`Self::poll_skip`].
    pub fn tick(&mut self, buttons: u16, samples_consumed: Option<u64>) -> MovieTick {
        if self.finished {
            return if self.skipped {
                MovieTick::Skipped
            } else {
                MovieTick::Finished
            };
        }

        if !self.external_poll && self.advance_skip(buttons) {
            return MovieTick::Skipped;
        }

        self.ticks += 1;
        let due = match samples_consumed {
            Some(consumed) => (consumed / self.samples_per_frame.max(1)) as usize,
            None => (self.ticks / self.ticks_per_frame.max(1)) as usize,
        };
        self.advance_due(due)
    }

    /// One tick paced by the wall clock: `elapsed_ms` is the real time since
    /// the previous call (the fixed step on the interactive path).
    ///
    /// The wall clock is the master, exactly like the original's platform film
    /// tick: the film time is the elapsed wall time, and `samples_consumed`
    /// (the mixer's device-playback cursor, which lags the render cursor by
    /// the queued device frames) is followed only while it lies within the
    /// original's tolerance, `-0.5 s` to `+0.25 s`, of the wall clock. The
    /// session then decodes forward to the frame due at that time, one or
    /// several frames at once, so a stalled or lagging device queue can never
    /// make the picture fall behind the wall clock.
    pub fn tick_timed(
        &mut self,
        buttons: u16,
        samples_consumed: Option<u64>,
        elapsed_ms: f64,
    ) -> MovieTick {
        if self.finished {
            return if self.skipped {
                MovieTick::Skipped
            } else {
                MovieTick::Finished
            };
        }

        let elapsed = elapsed_ms.max(0.0);
        if !self.external_poll && self.advance_skip(buttons) {
            return MovieTick::Skipped;
        }

        self.wall_ms += elapsed;
        let wall_seconds = self.wall_ms / 1000.0;
        let seconds = match samples_consumed {
            Some(consumed) => {
                let audio_seconds = consumed as f64 / f64::from(SAMPLE_RATE);
                if (wall_seconds - 0.5..=wall_seconds + 0.25).contains(&audio_seconds) {
                    audio_seconds
                } else {
                    wall_seconds
                }
            }
            None => wall_seconds,
        };
        // A tiny epsilon keeps an exact frame boundary (three fixed ticks, say)
        // from landing just under it in floating point.
        let fps = f64::from(self.frame_rate.0) / f64::from(self.frame_rate.1.max(1));
        let due = (seconds * fps + 1e-6) as usize;
        self.advance_due(due)
    }

    /// Present every kept frame due at `due`, ending the film when the target
    /// has passed the last frame. A stall that jumps several frames (or the
    /// line) is caught up in one call, and the prologue cut mapping applies
    /// through [`Self::present_kept`] exactly as for a one-frame advance.
    fn advance_due(&mut self, due: usize) -> MovieTick {
        if due >= self.kept_frames {
            // Show the last frame if the clock skipped over it, then end.
            let last = self.kept_frames - 1;
            if last > self.presented {
                if let Err(err) = self.present_kept(last) {
                    eprintln!("warning: film frame {last} failed to decode: {err:#}");
                } else {
                    self.ensure_audio_through(last + self.audio_lead);
                }
            }
            self.finished = true;
            return MovieTick::Finished;
        }
        if due <= self.presented {
            return MovieTick::Waiting;
        }
        if let Err(err) = self.present_kept(due) {
            eprintln!("warning: film frame {due} failed to decode: {err:#}");
            self.finished = true;
            return MovieTick::Finished;
        }
        self.ensure_audio_through(due + self.audio_lead);
        MovieTick::Advanced
    }

    /// Take the audio queued since the last call, for the mixer.
    pub fn take_audio(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.pending_audio)
    }

    /// The presented frame as 320x240 RGBA8, refreshed on every advance.
    pub fn frame_rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// The film's table id.
    pub fn id(&self) -> u8 {
        self.id
    }

    /// The character the session opened for (0 Chris, 1 Jill).
    pub fn character(&self) -> u8 {
        self.character
    }

    /// The presented kept frame index (0 after [`MovieSession::open`]).
    pub fn frame_index(&self) -> usize {
        self.presented
    }

    /// The presented frame's AVI index (differs after a prologue cut).
    pub fn avi_frame_index(&self) -> usize {
        self.avi_index(self.presented)
    }

    /// How many frames this session presents.
    pub fn frame_count(&self) -> usize {
        self.kept_frames
    }

    /// Output samples presented before kept frame `index`, the film's
    /// sample-position reference for the audio cursor. For every 22050 Hz film
    /// this is `avi.samples_before(index)`; the 44100 Hz 8-bit logo is
    /// converted to the mixer's rate first.
    pub fn samples_before(&self, index: usize) -> u64 {
        index as u64 * self.samples_per_frame
    }

    /// The output samples one presented frame spans.
    pub fn samples_per_frame(&self) -> u64 {
        self.samples_per_frame
    }

    /// The fixed 30 Hz ticks one presented frame spans.
    pub fn ticks_per_frame(&self) -> u64 {
        self.ticks_per_frame
    }

    /// The film's skip mask.
    pub fn skip_mask(&self) -> u16 {
        self.skip_mask
    }

    /// Whether the film ended (normally or through a skip).
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Whether the film ended through a skip.
    pub fn skipped(&self) -> bool {
        self.skipped
    }
}

/// Convert one AVI audio chunk to the mixer's 22050 Hz stereo s16, truncated
/// or padded to `samples` stereo frames.
///
/// The corpus is 22050 Hz s16 or (the Capcom logo) 44100 Hz u8; the 44.1 kHz
/// logo is decimated by dropping every other frame, the closest rate match
/// that needs no resampler.
fn decode_pcm(bytes: &[u8], format: &AudioFormat, samples: usize) -> Vec<i16> {
    let mut out = Vec::with_capacity(samples * 2);
    if format.sample_rate == SAMPLE_RATE && format.bits_per_sample == 16 {
        let (pairs, _) = bytes.as_chunks::<2>();
        out.extend(
            pairs
                .iter()
                .take(samples * 2)
                .map(|pair| i16::from_le_bytes(*pair)),
        );
    } else if format.sample_rate == 2 * SAMPLE_RATE && format.bits_per_sample == 8 {
        let frames = bytes.len() / 2;
        out.reserve(samples * 2);
        for frame in (0..frames).step_by(2) {
            for channel in 0..2 {
                let sample = i16::from(bytes[frame * 2 + channel]) - 128;
                out.push(sample << 8);
            }
        }
    }
    out.truncate(samples * 2);
    out
}

/// A synthetic film for cross-module tests: `frames` empty 10 fps frames with
/// full s16 stereo audio.
#[cfg(test)]
pub(crate) fn test_avi(frames: usize) -> Vec<u8> {
    tests::synthetic_avi(frames, (10, 1), frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::pack::PackWriter;

    /// One 10-byte empty intra frame: a valid Cinepak header with no strips, so
    /// the decoder accepts it and leaves the canvas black.
    fn empty_frame() -> Vec<u8> {
        vec![0, 0, 0, 10, 1, 64, 0, 240, 0, 0]
    }

    /// A minimal single-video, single-audio RIFF/AVI with `frames` empty
    /// frames and `audio_frames` s16 stereo chunks (the remaining chunks are
    /// missing, exercising the silence pad).
    pub(super) fn synthetic_avi(frames: usize, rate: (u32, u32), audio_frames: usize) -> Vec<u8> {
        fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(id);
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(body);
            if body.len() % 2 == 1 {
                out.push(0);
            }
            out
        }
        fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut inner = Vec::new();
            inner.extend_from_slice(kind);
            inner.extend_from_slice(body);
            chunk(b"LIST", &inner)
        }

        let (num, den) = rate;
        let us_per_frame = 1_000_000 * den / num;
        let total = frames as u32;

        let mut avih = Vec::new();
        avih.extend_from_slice(&us_per_frame.to_le_bytes());
        avih.extend_from_slice(&0u32.to_le_bytes());
        avih.extend_from_slice(&0u32.to_le_bytes());
        avih.extend_from_slice(&0x10u32.to_le_bytes());
        avih.extend_from_slice(&total.to_le_bytes());
        avih.extend_from_slice(&0u32.to_le_bytes());
        avih.extend_from_slice(&1u32.to_le_bytes());
        avih.extend_from_slice(&0u32.to_le_bytes());
        avih.extend_from_slice(&320u32.to_le_bytes());
        avih.extend_from_slice(&240u32.to_le_bytes());
        avih.extend_from_slice(&[0u8; 16]);

        let mut strh = Vec::new();
        strh.extend_from_slice(b"vids");
        strh.extend_from_slice(b"cvid");
        strh.extend_from_slice(&[0u8; 4]);
        strh.extend_from_slice(&0u16.to_le_bytes());
        strh.extend_from_slice(&0u16.to_le_bytes());
        strh.extend_from_slice(&0u32.to_le_bytes());
        strh.extend_from_slice(&1_000_000u32.to_le_bytes());
        strh.extend_from_slice(&0u32.to_le_bytes());
        strh.extend_from_slice(&total.to_le_bytes());
        strh.extend_from_slice(&0u32.to_le_bytes());
        strh.extend_from_slice(&0u32.to_le_bytes());
        strh.extend_from_slice(&[0u8; 16]);

        let mut strf = Vec::new();
        strf.extend_from_slice(&40u32.to_le_bytes());
        strf.extend_from_slice(&320i32.to_le_bytes());
        strf.extend_from_slice(&240i32.to_le_bytes());
        strf.extend_from_slice(&1u16.to_le_bytes());
        strf.extend_from_slice(&24u16.to_le_bytes());
        strf.extend_from_slice(b"cvid");
        strf.extend_from_slice(&[0u8; 20]);

        let mut audio_strh = Vec::new();
        audio_strh.extend_from_slice(b"auds");
        audio_strh.extend_from_slice(&[0u8; 4]);
        audio_strh.extend_from_slice(&[0u8; 4]);
        audio_strh.extend_from_slice(&0u16.to_le_bytes());
        audio_strh.extend_from_slice(&0u16.to_le_bytes());
        audio_strh.extend_from_slice(&0u32.to_le_bytes());
        audio_strh.extend_from_slice(&(1_000_000 * den / num).to_le_bytes());
        audio_strh.extend_from_slice(&0u32.to_le_bytes());
        audio_strh.extend_from_slice(&total.to_le_bytes());
        audio_strh.extend_from_slice(&0u32.to_le_bytes());
        audio_strh.extend_from_slice(&0u32.to_le_bytes());
        audio_strh.extend_from_slice(&[0u8; 16]);

        let block_align = 4u16;
        let mut audio_strf = Vec::new();
        audio_strf.extend_from_slice(&1u16.to_le_bytes());
        audio_strf.extend_from_slice(&2u16.to_le_bytes());
        audio_strf.extend_from_slice(&22_050u32.to_le_bytes());
        audio_strf.extend_from_slice(&(22_050u32 * u32::from(block_align)).to_le_bytes());
        audio_strf.extend_from_slice(&block_align.to_le_bytes());
        audio_strf.extend_from_slice(&16u16.to_le_bytes());

        let video_strl = list(
            b"strl",
            &[chunk(b"strh", &strh), chunk(b"strf", &strf)].concat(),
        );
        let audio_strl = list(
            b"strl",
            &[chunk(b"strh", &audio_strh), chunk(b"strf", &audio_strf)].concat(),
        );
        let hdrl = list(
            b"hdrl",
            &[chunk(b"avih", &avih), video_strl, audio_strl].concat(),
        );

        let mut movi_body = Vec::new();
        for index in 0..frames {
            movi_body.extend_from_slice(&chunk(b"00dc", &empty_frame()));
            if index < audio_frames {
                movi_body.extend_from_slice(&chunk(b"01wb", &[0x11u8; 4]));
            }
        }
        let movi = list(b"movi", &movi_body);

        let body = [hdrl, movi].concat();
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
        file.extend_from_slice(b"AVI ");
        file.extend_from_slice(&body);
        file
    }

    fn session(frames: usize, rate: (u32, u32), audio_frames: usize) -> MovieSession {
        let mut writer = PackWriter::new();
        writer
            .add("movie/oj.avi", synthetic_avi(frames, rate, audio_frames))
            .unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        MovieSession::open(&pack, 0, 0).unwrap()
    }

    #[test]
    fn name_table_locks_all_29_rows() {
        let expected = [
            "oj", "pj", "dmf", "dm3", "dm4", "dm1", "dm6", "dm7", "dm8", "dm2", "", "dmb", "dmc",
            "dmd", "dme", "ed1", "ed2", "ed3", "ed4", "ed5", "ed6", "ed7", "ed8", "capcom",
            "stfc_b", "stfj_b", "stfz_b", "staf_b", "vlogo",
        ];
        for (id, expected) in expected.iter().enumerate() {
            if expected.is_empty() {
                assert_eq!(name(id as u8), None, "id {id} is the null entry");
            } else {
                assert_eq!(name(id as u8), Some(*expected), "id {id}");
            }
        }
        assert_eq!(name(29), None);
        assert_eq!(name(255), None);
        assert_eq!(pack_path("PJ"), "movie/pj.avi");
        assert_eq!(pack_path("staf_b"), "movie/staf_b.avi");
    }

    #[test]
    fn skip_classes_match_the_shipped_masks() {
        let unskippable = [2u8, 14, 15, 16, 17, 18, 19, 20, 21, 22, 24, 25, 26, 27];
        for id in 0..=28u8 {
            let expected = if unskippable.contains(&id) { 0 } else { 0x0FFF };
            assert_eq!(skip_mask(id), expected, "id {id}");
        }
        assert_eq!(
            skip_mask(29),
            0,
            "off-table ids are unskippable like the original"
        );
        assert_eq!(skip_mask(255), 0);
        assert_eq!(
            skip_mask(10),
            0x0FFF,
            "the null entry defaults to skippable"
        );
    }

    #[test]
    fn prologue_cut_is_jills_only() {
        assert_eq!(prologue_cut_ms(1, 1), Some((177_800, 188_500)));
        assert_eq!(prologue_cut_ms(1, 0), None);
        assert_eq!(prologue_cut_ms(2, 1), None);
        assert_eq!(prologue_cut_ms(0, 1), None);
    }

    #[test]
    fn frame_at_ms_converts_at_both_shipped_rates() {
        assert_eq!(frame_at_ms(177_800, (10, 1)), 1778);
        assert_eq!(frame_at_ms(188_500, (10, 1)), 1885);
        assert_eq!(frame_at_ms(177_800, (15, 1)), 2667);
        assert_eq!(frame_at_ms(188_500, (15, 1)), 2827);
        assert_eq!(frame_at_ms(0, (10, 1)), 0);
        assert_eq!(frame_at_ms(10_000, (10, 1)), 100);
        assert_eq!(ticks_per_frame((10, 1)), 3);
        assert_eq!(ticks_per_frame((15, 1)), 2);
    }

    #[test]
    fn tick_clock_paces_ten_and_fifteen_fps() {
        let mut ten = session(7, (10, 1), 7);
        for expected in 1..7 {
            assert_eq!(ten.tick(0, None), MovieTick::Waiting);
            assert_eq!(ten.tick(0, None), MovieTick::Waiting);
            assert_eq!(ten.tick(0, None), MovieTick::Advanced);
            assert_eq!(ten.frame_index(), expected);
        }
        // The last frame holds for its three ticks, then the film finishes.
        assert_eq!(ten.tick(0, None), MovieTick::Waiting);
        assert_eq!(ten.tick(0, None), MovieTick::Waiting);
        assert_eq!(ten.tick(0, None), MovieTick::Finished);
        assert!(ten.finished());
        assert!(!ten.skipped());

        let mut fifteen = session(3, (15, 1), 3);
        assert_eq!(fifteen.tick(0, None), MovieTick::Waiting);
        assert_eq!(fifteen.tick(0, None), MovieTick::Advanced);
        assert_eq!(fifteen.frame_index(), 1);
        assert_eq!(fifteen.ticks_per_frame(), 2);
    }

    #[test]
    fn audio_led_clock_advances_on_consumed_samples() {
        let mut session = session(3, (10, 1), 3);
        assert_eq!(session.samples_per_frame(), 2205);
        // The two second lead already covers the whole short synthetic film.
        let queued = session.take_audio();
        assert_eq!(queued.len(), 2205 * 2 * 3);
        assert_eq!(session.samples_before(1), 2205);
        assert_eq!(session.samples_before(3), 6615);

        assert_eq!(session.tick(0, Some(0)), MovieTick::Waiting);
        assert_eq!(session.tick(0, Some(2204)), MovieTick::Waiting);
        assert_eq!(session.tick(0, Some(2205)), MovieTick::Advanced);
        assert_eq!(session.frame_index(), 1);
        // The lead queued everything at open, so no later tick adds audio.
        assert!(session.take_audio().is_empty());
        assert_eq!(session.tick(0, Some(4409)), MovieTick::Waiting);
        assert_eq!(session.tick(0, Some(4410)), MovieTick::Advanced);
        assert_eq!(session.tick(0, Some(6615)), MovieTick::Finished);
    }

    #[test]
    fn short_audio_tracks_pad_silence() {
        let mut session = session(4, (10, 1), 1);
        // The lead queues every frame at open; frames past the audio track are
        // silence.
        let queued = session.take_audio();
        assert_eq!(queued.len(), 2205 * 2 * 4);
        assert!(queued[..2205 * 2].iter().any(|&sample| sample != 0));
        assert!(queued[2205 * 2..].iter().all(|&sample| sample == 0));
        // The later ticks cannot produce more audio than the film holds.
        assert_eq!(session.tick(0, None), MovieTick::Waiting);
        assert_eq!(session.tick(0, None), MovieTick::Waiting);
        assert_eq!(session.tick(0, None), MovieTick::Advanced);
        assert!(session.take_audio().is_empty());
    }

    #[test]
    fn a_held_button_never_skips_and_a_fresh_press_does() {
        let mut session = session(400, (10, 1), 400);
        const BUTTON: u16 = 1;
        // The first tick latches a held button: no edge, no skip.
        assert_eq!(session.tick(BUTTON, None), MovieTick::Waiting);
        // Held on: a held button never produces an edge. The film itself keeps
        // advancing on the tick clock.
        for _ in 0..200 {
            session.tick(BUTTON, None);
        }
        assert!(!session.skipped());
        // Released and pressed again: the fresh edge skips on its first tick.
        // The reference's film loop runs unbounded and spends its 100-update
        // grace within milliseconds, so this is its observable behaviour.
        for _ in 0..50 {
            session.tick(0, None);
        }
        assert_eq!(session.tick(BUTTON, None), MovieTick::Skipped);
        assert!(session.skipped());
    }

    #[test]
    fn a_press_after_the_latch_skips_at_once() {
        let mut session = session(400, (10, 1), 400);
        const BUTTON: u16 = 1;
        assert_eq!(session.tick(0, None), MovieTick::Waiting);
        // A clean edge on the very next update ends the film.
        assert_eq!(session.tick(BUTTON, None), MovieTick::Skipped);
        assert!(session.skipped());
    }

    #[test]
    fn poll_skip_latches_then_accepts_a_press() {
        let mut session = session(4000, (10, 1), 4000);
        // The first poll latches the pad without an edge...
        assert!(!session.poll_skip(0));
        // ...and the next poll accepts an unmasked press.
        assert!(session.poll_skip(1));
        assert!(session.skipped());
    }

    #[test]
    fn a_held_button_latched_by_a_poll_cannot_skip() {
        let mut session = session(4000, (10, 1), 4000);
        // The poll that latches a held button never treats it as an edge.
        assert!(!session.poll_skip(1));
        for _ in 0..100 {
            assert!(!session.poll_skip(1));
        }
        // Only a release and re-press is an edge.
        assert!(!session.poll_skip(0));
        assert!(session.poll_skip(1));
        assert!(session.skipped());
    }

    #[test]
    fn a_release_and_re_press_between_polls_is_a_fresh_edge() {
        let mut session = session(4000, (10, 1), 4000);
        // The latch holds a button that was already down when the film began.
        assert!(!session.poll_skip(1));
        // Held across polls: no edge.
        for _ in 0..10 {
            assert!(!session.poll_skip(1));
        }
        // Release and re-press: sample-to-sample detection sees the edge even
        // though the button was held at the previous poll.
        assert!(!session.poll_skip(0));
        assert!(session.poll_skip(1));
        assert!(session.skipped());
    }

    #[test]
    fn an_interactive_poll_owns_the_skip_sampling() {
        let mut session = session(4000, (10, 1), 4000);
        // Once a poll has run, the paired ticks must not latch or edge.
        assert!(!session.poll_skip(0));
        assert!(!session.poll_skip(0));
        assert!(!session.skipped());
        // A tick with a press while the external poll owns the sampling does
        // not consume the edge...
        assert!(matches!(
            session.tick(1, None),
            MovieTick::Waiting | MovieTick::Advanced
        ));
        assert!(!session.skipped());
        // ...the poll does, and the tick then reports the skip.
        assert!(session.poll_skip(1));
        assert_eq!(session.tick(1, None), MovieTick::Skipped);
    }

    #[test]
    fn wall_time_masters_a_stalled_audio_cursor() {
        let mut session = session(10, (10, 1), 10);
        // The mixer never renders a sample. While the stalled cursor is inside
        // the -0.5 s tolerance it is still followed, exactly like the
        // original; once the wall clock has left it behind, the wall clock
        // takes over and the film advances to the frame due now.
        for _ in 0..5 {
            assert_eq!(session.tick_timed(0, Some(0), 100.0), MovieTick::Waiting);
        }
        assert_eq!(session.frame_index(), 0);
        assert_eq!(session.tick_timed(0, Some(0), 100.0), MovieTick::Advanced);
        assert_eq!(session.frame_index(), 6);
    }

    #[test]
    fn an_audio_cursor_ahead_of_wall_time_is_ignored() {
        let mut session = session(3, (10, 1), 3);
        // The cursor claims a full second with no wall time passed: outside
        // the +0.25 s bound, so the picture stays where the wall clock is.
        assert_eq!(
            session.tick_timed(0, Some(u64::from(SAMPLE_RATE)), 0.0),
            MovieTick::Waiting
        );
        assert_eq!(session.frame_index(), 0);
    }

    #[test]
    fn a_stall_catches_up_every_due_frame_at_once() {
        let mut session = session(5, (10, 1), 5);
        // 350 ms of wall time is three 100 ms frames plus a bit: the film
        // jumps straight from frame 0 to frame 3 (the original's decode
        // forward to `target`).
        assert_eq!(session.tick_timed(0, None, 350.0), MovieTick::Advanced);
        assert_eq!(session.frame_index(), 3);
    }

    #[test]
    fn unskippable_films_ignore_every_edge() {
        let mut avi = synthetic_avi(400, (10, 1), 400);
        let mut writer = PackWriter::new();
        writer
            .add("movie/dmf.avi", std::mem::take(&mut avi))
            .unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        let mut session = MovieSession::open(&pack, 2, 0).unwrap();
        assert_eq!(session.skip_mask(), 0);
        for _ in 0..300 {
            session.tick(0x0FFF, None);
            session.tick(0, None);
        }
        assert!(!session.skipped());
    }

    #[test]
    fn prologue_cut_jumps_to_the_second_cut_point() {
        // 45 frames at 10 fps = 4.5 s; the real cut points are far out, so use
        // the AVI directly to place the cut within the test film.
        let avi = Avi::parse(synthetic_avi(3000, (10, 1), 3000)).unwrap();
        let mut session = MovieSession::from_avi(avi, 1, 1).unwrap();
        let (start, end) = session.cut.expect("Jill's intro carries the cut");
        assert_eq!((start, end), (1778, 1885));
        assert_eq!(session.frame_count(), 3000 - (1885 - 1778));
        assert_eq!(session.frame_index(), 0);
        assert_eq!(session.avi_frame_index(), 0);
        // Drive the no-device clock up to the frame before the cut: the last
        // tick before frame `start` is due leaves avi frame `start - 1`
        // presented, then the next advance resumes at the second cut point.
        for _ in 0..(start * 3 - 1) {
            session.tick(0, None);
        }
        assert_eq!(session.avi_frame_index(), start - 1);
        assert_eq!(session.tick(0, None), MovieTick::Advanced);
        assert_eq!(session.avi_frame_index(), end);
        assert_eq!(session.frame_index(), start);
    }

    #[test]
    fn open_rejects_the_null_id_and_missing_entries() {
        let mut writer = PackWriter::new();
        writer
            .add("movie/oj.avi", synthetic_avi(2, (10, 1), 2))
            .unwrap();
        let pack = Pack::from_bytes(writer.to_bytes().unwrap()).unwrap();
        assert!(MovieSession::open(&pack, 10, 0).is_err());
        assert!(MovieSession::open(&pack, 3, 0).is_err());
        assert!(MovieSession::open(&pack, 0, 0).is_ok());
    }

    #[test]
    fn capcom_audio_is_decimated_to_the_mixer_rate() {
        // 44100 Hz u8 stereo: one output frame spans 2205 converted samples.
        let format = AudioFormat {
            format_tag: 1,
            channels: 2,
            sample_rate: 44_100,
            avg_bytes_per_sec: 88_200,
            block_align: 2,
            bits_per_sample: 8,
        };
        // Four input frames (L,R bytes) become two output frames.
        let bytes = [0u8, 255, 128, 128, 0, 255, 128, 128];
        let out = decode_pcm(&bytes, &format, 2);
        assert_eq!(out, vec![-32768, 32512, -32768, 32512]);
    }
}
