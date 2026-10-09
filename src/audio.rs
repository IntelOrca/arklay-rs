//! WAV parsing and a software mixer feeding one SDL3 stream.
//!
//! [`Mixer`] owns the only SDL audio stream in the engine. Every source is
//! converted to 16-bit signed mono at 22050 Hz on load (8-bit samples are
//! upsampled, other rates are linearly resampled). Each [`Mixer::update`]
//! renders and queues interleaved stereo samples: three independently
//! controlled BGM channels, one dedicated voice (dialogue) channel and a pool
//! of one-shot voices, each with its own gain and pan.
//!
//! A one-shot started on a bank restarts that bank's voice (seek to sample 0)
//! when it is already sounding, matching the original's one DirectSound buffer
//! per sound record; distinct banks append, and the unkeyed test/UI path always
//! appends. The BGM channels and the voice channel each hold one buffer: a load
//! replaces it, a stop rewinds it and keeps it loaded, and a restart begins it
//! again from sample 0, matching the original's fixed bank set. Voices are
//! summed as 32-bit floats and each output sample is saturated to the 16-bit
//! rail, matching the hardware sum the original mixer relies on. A missing
//! device or a failed SDL call leaves the engine silent, never fatal.

use std::ffi::{CStr, c_int};
use std::ptr;

use anyhow::{Context, Result, bail};
use sdl3_sys::audio::{
    SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, SDL_AUDIO_S16LE, SDL_AudioSpec, SDL_AudioStream,
    SDL_ClearAudioStream, SDL_DestroyAudioStream, SDL_GetAudioStreamQueued,
    SDL_GetCurrentAudioDriver, SDL_OpenAudioDeviceStream, SDL_PutAudioStreamData,
    SDL_ResumeAudioStreamDevice,
};
use sdl3_sys::init::{SDL_INIT_AUDIO, SDL_InitSubSystem};

use crate::budget;
use crate::engine::SdlHandle;

/// Sample rate of every mixer voice and of the output stream.
pub const SAMPLE_RATE: u32 = 22050;
/// Maximum number of one-shot voices mixed at once.
pub const MAX_SFX_VOICES: usize = 16;
/// Frames the mixer keeps queued ahead of the device.
const TARGET_QUEUED_FRAMES: usize = 2048;
/// Upper bound on the frames rendered in one `update`.
const MAX_RENDER_FRAMES: usize = 8192;
/// Bytes per output frame: two 16-bit channels.
const BYTES_PER_FRAME: usize = 4;

/// PCM sample format of a parsed WAV file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WavFormat {
    /// Unsigned 8-bit samples.
    U8,
    /// Signed 16-bit little-endian samples.
    S16Le,
}

/// Parsed WAV file: source format plus the raw `data` chunk bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wav {
    /// Sample format taken from the `fmt ` chunk.
    pub format: WavFormat,
    /// Channel count taken from the `fmt ` chunk.
    pub channels: u16,
    /// Sample rate in Hz taken from the `fmt ` chunk.
    pub sample_rate: u32,
    /// Raw bytes of the `data` chunk, still in the source format.
    pub data: Vec<u8>,
}

/// Parse a RIFF/WAVE image, returning its format and `data` chunk bytes.
///
/// Walks the chunk list, skipping anything that is not `fmt ` or `data`.
/// Only integer PCM is supported: 8-bit unsigned or 16-bit little-endian
/// signed, any sample rate, any non-zero channel count.
pub fn parse_wav(bytes: &[u8]) -> Result<Wav> {
    if bytes.len() < 12 {
        bail!("WAV file is smaller than the 12-byte RIFF header");
    }
    if &bytes[0..4] != b"RIFF" {
        bail!("not a RIFF file");
    }
    if &bytes[8..12] != b"WAVE" {
        bail!("RIFF file is not of WAVE form");
    }

    let mut format = None;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut data = None;

    let mut offset = 12usize;
    while offset < bytes.len() {
        if bytes.len() - offset < 8 {
            bail!("truncated chunk header at byte {offset}");
        }
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        let start = offset + 8;
        let end = start
            .checked_add(size)
            .context("WAV chunk size overflows the address space")?;
        if end > bytes.len() {
            bail!(
                "chunk `{}` at byte {offset} is truncated: {} declared bytes, {} available",
                chunk_label(id),
                size,
                bytes.len() - start
            );
        }

        match id {
            b"fmt " => {
                if size < 16 {
                    bail!("`fmt ` chunk is too small: {size} bytes, expected at least 16");
                }
                let fmt = &bytes[start..end];
                let tag = u16::from_le_bytes([fmt[0], fmt[1]]);
                if tag != 1 {
                    bail!("unsupported WAV format tag {tag}: only integer PCM (1) is supported");
                }
                let fmt_channels = u16::from_le_bytes([fmt[2], fmt[3]]);
                let fmt_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                let bits = u16::from_le_bytes([fmt[14], fmt[15]]);
                format = Some(match bits {
                    8 => WavFormat::U8,
                    16 => WavFormat::S16Le,
                    _ => bail!(
                        "unsupported WAV sample width {bits}: only 8- and 16-bit PCM are supported"
                    ),
                });
                if fmt_channels == 0 {
                    bail!("WAV declares zero channels");
                }
                if fmt_rate == 0 {
                    bail!("WAV declares a zero sample rate");
                }
                channels = fmt_channels;
                sample_rate = fmt_rate;
            }
            b"data" => {
                budget::check_len(size, budget::MAX_WAV_DATA, "WAV data chunk size")?;
                data = Some(bytes[start..end].to_vec());
            }
            _ => {}
        }

        offset = end + (size & 1);
    }

    let format = format.context("WAV file has no `fmt ` chunk")?;
    let data = data.context("WAV file has no `data` chunk")?;
    Ok(Wav {
        format,
        channels,
        sample_rate,
        data,
    })
}

/// Render a four-byte chunk id for error messages.
fn chunk_label(id: &[u8]) -> String {
    String::from_utf8_lossy(id).into_owned()
}

/// Convert a parsed WAV to signed 16-bit mono at [`SAMPLE_RATE`].
///
/// 8-bit unsigned samples are centered and scaled up by 8 bits; 16-bit
/// samples are read little-endian. Multi-channel data is averaged down to
/// mono, and any other sample rate is linearly resampled. Both the source
/// sample count and the resampled result are capped at
/// [`budget::MAX_WAV_SAMPLES`], so a tiny sample rate cannot expand a small
/// file into an unbounded buffer.
fn to_mono(wav: &Wav) -> Result<Vec<i16>> {
    let mut mono: Vec<i16> = match wav.format {
        WavFormat::U8 => {
            budget::check_len(wav.data.len(), budget::MAX_WAV_SAMPLES, "WAV sample count")?;
            wav.data
                .iter()
                .map(|&byte| (i16::from(byte) - 128) << 8)
                .collect()
        }
        WavFormat::S16Le => {
            let samples = wav.data.len() / 2;
            budget::check_len(samples, budget::MAX_WAV_SAMPLES, "WAV sample count")?;
            wav.data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| i16::from_le_bytes(*chunk))
                .collect()
        }
    };

    if wav.channels > 1 {
        let channels = usize::from(wav.channels);
        budget::check_len(
            mono.len() / channels,
            budget::MAX_WAV_SAMPLES,
            "WAV frame count",
        )?;
        mono = mono
            .chunks(channels)
            .map(|frame| {
                let sum: i32 = frame.iter().map(|&sample| i32::from(sample)).sum();
                (sum / channels as i32) as i16
            })
            .collect();
    }

    if wav.sample_rate != SAMPLE_RATE {
        mono = resample_linear(&mono, wav.sample_rate, SAMPLE_RATE)?;
    }
    Ok(mono)
}

/// Linearly resample `input` from `from` Hz to `to` Hz.
///
/// The output length is checked against [`budget::MAX_WAV_SAMPLES`] and the
/// buffer is reserved through [`budget::alloc`].
fn resample_linear(input: &[i16], from: u32, to: u32) -> Result<Vec<i16>> {
    if input.is_empty() || from == 0 || to == 0 || from == to {
        return Ok(input.to_vec());
    }

    let out_len_u64 = input.len() as u64 * u64::from(to) / u64::from(from);
    let out_len = usize::try_from(out_len_u64).context("resampled WAV length overflows")?;
    budget::check_len(
        out_len,
        budget::MAX_WAV_SAMPLES,
        "resampled WAV sample count",
    )?;
    let mut out = budget::alloc::<i16>(out_len, "resampled WAV buffer")?;
    for index in 0..out_len {
        let position = index as f64 * f64::from(from) / f64::from(to);
        let base = position.floor() as usize;
        let fraction = (position - base as f64) as f32;
        let a = f32::from(input[base.min(input.len() - 1)]);
        let b = f32::from(input[(base + 1).min(input.len() - 1)]);
        out.push((a + (b - a) * fraction) as i16);
    }
    Ok(out)
}

/// Convert a WAV for playback, logging and dropping an over-cap source.
fn mono_or_warn(wav: &Wav) -> Option<Vec<i16>> {
    match to_mono(wav) {
        Ok(mono) => Some(mono),
        Err(error) => {
            eprintln!("[audio] dropping WAV: {error:#}");
            None
        }
    }
}

/// The original's DirectSound pan law for `pan`, where -1 is hard left, 1 is
/// hard right and 0 is centered.
///
/// The game hands DirectSound a raw pan `(right - left) * 0x4E` in
/// `-10000..=10000` hundredths of a decibel. DirectSound keeps the near
/// channel at full scale and attenuates the far channel by `|pan|` hundredths
/// of a decibel, i.e. `10^(-|pan| / 2000)` in amplitude, so a centered cue
/// plays at full level in both channels and a hard pan leaves the far channel
/// at -100 dB.
fn pan_gains(pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    let far = 10.0f32.powf(-pan.abs() * 5.0);
    if pan > 0.0 { (far, 1.0) } else { (1.0, far) }
}

/// One playing buffer with its mix parameters.
#[derive(Clone, Debug)]
struct Voice {
    pcm: Vec<i16>,
    pos: usize,
    gain: f32,
    pan: f32,
    looping: bool,
    /// Start order among one-shot voices; smaller values are older.
    seq: u64,
    /// The one-shot's bank, when it has one. A repeated cue on the same bank
    /// restarts this voice instead of appending a new one; `None` voices (the
    /// unkeyed test/UI entries) never match.
    bank: Option<u64>,
}

impl Voice {
    fn new(pcm: Vec<i16>, gain: f32, pan: f32, looping: bool, seq: u64) -> Self {
        Self {
            pcm,
            pos: 0,
            gain,
            pan,
            looping,
            seq,
            bank: None,
        }
    }

    /// Re-arm this voice for `bank`: replace the buffer and mix parameters,
    /// seek to sample 0 and take the newest start order.
    fn restart(&mut self, bank: u64, pcm: Vec<i16>, gain: f32, pan: f32, seq: u64) {
        self.pcm = pcm;
        self.pos = 0;
        self.gain = gain.max(0.0);
        self.pan = pan;
        self.looping = false;
        self.seq = seq;
        self.bank = Some(bank);
    }

    /// Whether a one-shot voice has played its whole buffer.
    fn finished(&self) -> bool {
        !self.looping && self.pos >= self.pcm.len()
    }
}

/// One of the three BGM banks: a loaded buffer, its mix parameters and
/// whether it is currently sounding.
///
/// `playing` separates "loaded but silent" (the original's `setSndStop`, or a
/// bank whose enable bit is clear) from "sounding"; the buffer stays loaded
/// either way so a later restart needs no reload.
#[derive(Clone, Debug)]
pub(crate) struct BgmChannel {
    pcm: Vec<i16>,
    pos: usize,
    gain: f32,
    pan: f32,
    looping: bool,
    playing: bool,
}

impl BgmChannel {
    fn new(pcm: Vec<i16>, looping: bool) -> Self {
        Self {
            pcm,
            pos: 0,
            gain: 1.0,
            pan: 0.0,
            looping,
            playing: true,
        }
    }
}

/// One playing film-audio track: interleaved stereo s16 at the mixer's rate,
/// its play offset in stereo frames and how many frames it has rendered.
///
/// The movie loader appends one frame of film audio at a time; the counter is
/// the audio-led playback clock, so it counts only frames the mixer actually
/// rendered (silence after the queued tail does not advance it).
#[derive(Clone, Debug)]
struct MovieAudio {
    pcm: Vec<i16>,
    /// Play offset in stereo frames.
    pos: usize,
    /// Stereo frames rendered since the track was loaded.
    consumed: u64,
}

/// Device-independent mixer state: the BGM banks, the voice channel, the
/// one-shot pool and their mix parameters.
///
/// Visible to the crate so behaviour tests can drive the mixer without an SDL
/// device. Every one-shot start appends a voice, so the same source may be
/// sounding many times over.
pub(crate) struct MixState {
    bgm_channels: [Option<BgmChannel>; 3],
    voice: Option<Voice>,
    voice_finished: bool,
    sfx: Vec<Voice>,
    next_sfx_seq: u64,
    /// The film's stereo audio, mixed alongside the game sounds.
    movie: Option<MovieAudio>,
    /// True while a film holds the game sounds: BGM, voice and one-shots are
    /// skipped but keep their buffers; the film still renders.
    game_paused: bool,
    /// BGM channels that were sounding when a film paused them, so a resume
    /// restarts exactly those from sample 0.
    paused_bgm: [bool; 3],
    /// Whether the voice bank was sounding when a film paused it.
    paused_voice: bool,
}

impl MixState {
    pub(crate) fn new() -> Self {
        Self {
            bgm_channels: [None, None, None],
            voice: None,
            voice_finished: false,
            sfx: Vec::new(),
            next_sfx_seq: 0,
            movie: None,
            game_paused: false,
            paused_bgm: [false; 3],
            paused_voice: false,
        }
    }

    /// Number of BGM channel slots (the original's `g_SndBank`).
    pub(crate) const BGM_CHANNELS: usize = 3;

    /// Number of one-shot voices currently sounding.
    #[cfg(test)]
    pub(crate) fn active_sfx(&self) -> usize {
        self.sfx.len()
    }

    /// Load `pcm` into BGM channel `index`, replacing any loaded bank. The
    /// channel is left stopped; the caller starts it with
    /// [`MixState::restart_bgm_channel`] or [`MixState::play_bgm_channel`].
    pub(crate) fn load_bgm_channel(&mut self, index: usize, pcm: Vec<i16>, looping: bool) {
        if index >= Self::BGM_CHANNELS {
            return;
        }
        self.bgm_channels[index] = (!pcm.is_empty()).then(|| BgmChannel::new(pcm, looping));
    }

    /// Load `pcm` into BGM channel `index` and start it from sample 0.
    fn play_bgm_channel(&mut self, index: usize, pcm: Vec<i16>, looping: bool) {
        self.load_bgm_channel(index, pcm, looping);
        self.restart_bgm_channel(index);
    }

    /// Stop BGM channel `index`, rewinding it and keeping its buffer loaded.
    pub(crate) fn stop_bgm_channel(&mut self, index: usize) {
        if let Some(channel) = self.bgm_channels.get_mut(index).and_then(Option::as_mut) {
            channel.playing = false;
            channel.pos = 0;
        }
    }

    /// Start BGM channel `index` from sample 0 (the original's `SetSndSlot`).
    /// A channel with no loaded buffer is a no-op.
    pub(crate) fn restart_bgm_channel(&mut self, index: usize) {
        if let Some(channel) = self.bgm_channels.get_mut(index).and_then(Option::as_mut) {
            channel.playing = true;
            channel.pos = 0;
        }
    }

    /// Set BGM channel `index`'s gain; negative values are clamped to zero.
    pub(crate) fn set_bgm_channel_volume(&mut self, index: usize, gain: f32) {
        if let Some(channel) = self.bgm_channels.get_mut(index).and_then(Option::as_mut) {
            channel.gain = gain.max(0.0);
        }
    }

    /// Set BGM channel `index`'s pan (-1 left, 1 right).
    pub(crate) fn set_bgm_channel_pan(&mut self, index: usize, pan: f32) {
        if let Some(channel) = self.bgm_channels.get_mut(index).and_then(Option::as_mut) {
            channel.pan = pan.clamp(-1.0, 1.0);
        }
    }

    /// Whether BGM channel `index` has a loaded buffer.
    #[cfg(test)]
    pub(crate) fn bgm_channel_loaded(&self, index: usize) -> bool {
        self.bgm_channels.get(index).is_some_and(Option::is_some)
    }

    /// Whether BGM channel `index` is currently sounding.
    pub(crate) fn bgm_channel_playing(&self, index: usize) -> bool {
        self.bgm_channels
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|channel| channel.playing)
    }

    /// Drop every BGM bank and its buffer.
    pub(crate) fn stop_all_bgm(&mut self) {
        self.bgm_channels = [None, None, None];
    }

    /// Replace the voice (dialogue) buffer and start it from sample 0.
    pub(crate) fn play_voice(&mut self, pcm: Vec<i16>, gain: f32, pan: f32) {
        self.voice_finished = false;
        self.voice = (!pcm.is_empty()).then(|| Voice::new(pcm, gain.max(0.0), pan, false, 0));
    }

    /// Stop the voice channel and clear its finished report.
    pub(crate) fn stop_voice(&mut self) {
        self.voice = None;
        self.voice_finished = false;
    }

    /// Whether the voice channel has a buffer loaded.
    pub(crate) fn voice_playing(&self) -> bool {
        self.voice.is_some()
    }

    /// Whether the voice channel has played a buffer to its end since the last
    /// [`MixState::play_voice`] or [`MixState::stop_voice`]. The flag is sticky
    /// until then, so the engine cannot miss a completion between ticks.
    pub(crate) fn voice_finished(&self) -> bool {
        self.voice_finished
    }

    /// Start a one-shot voice on `bank`, the original's one-buffer-per-sound
    /// rule.
    ///
    /// When the bank already has a one-shot voice it is restarted (buffer and
    /// mix parameters replaced, seek to sample 0) instead of appending, so two
    /// cues on the same bank cut each other off exactly like DirectSound's
    /// single buffer does. Distinct banks still append, and a full pool steals
    /// the voice that has played the most of its buffer; voices tied on
    /// progress give way oldest-first, so a burst of new sounds can never
    /// evict the copy that was just started.
    pub(crate) fn play_sfx_on_bank(&mut self, bank: u64, pcm: Vec<i16>, gain: f32, pan: f32) {
        if pcm.is_empty() {
            return;
        }
        let seq = self.next_sfx_seq;
        if let Some(voice) = self.sfx.iter_mut().find(|voice| voice.bank == Some(bank)) {
            self.next_sfx_seq += 1;
            voice.restart(bank, pcm, gain, pan, seq);
            return;
        }
        if self.sfx.len() >= MAX_SFX_VOICES
            && let Some((index, _)) = self
                .sfx
                .iter()
                .enumerate()
                .max_by_key(|(_, voice)| (voice.pos, std::cmp::Reverse(voice.seq)))
        {
            self.sfx.remove(index);
        }
        self.next_sfx_seq += 1;
        let mut voice = Voice::new(pcm, gain.max(0.0), pan, false, seq);
        voice.bank = Some(bank);
        self.sfx.push(voice);
    }

    /// Start an unkeyed one-shot voice, always appending.
    ///
    /// The UI cues and the offline tests use this path; the gameplay paths key
    /// their cues through [`MixState::play_sfx_on_bank`].
    pub(crate) fn play_sfx(&mut self, pcm: Vec<i16>, gain: f32, pan: f32) {
        if pcm.is_empty() {
            return;
        }
        if self.sfx.len() >= MAX_SFX_VOICES
            && let Some((index, _)) = self
                .sfx
                .iter()
                .enumerate()
                .max_by_key(|(_, voice)| (voice.pos, std::cmp::Reverse(voice.seq)))
        {
            self.sfx.remove(index);
        }
        let seq = self.next_sfx_seq;
        self.next_sfx_seq += 1;
        self.sfx
            .push(Voice::new(pcm, gain.max(0.0), pan, false, seq));
    }

    fn bgm_any_playing(&self) -> bool {
        self.bgm_channels
            .iter()
            .flatten()
            .any(|channel| channel.playing)
    }

    /// Queue one film-audio chunk, starting the movie channel on the first
    /// call. Chunks are appended in stream order.
    pub(crate) fn load_movie_audio(&mut self, pcm: Vec<i16>) {
        if pcm.is_empty() {
            return;
        }
        match &mut self.movie {
            Some(movie) => movie.pcm.extend_from_slice(&pcm),
            None => {
                self.movie = Some(MovieAudio {
                    pcm,
                    pos: 0,
                    consumed: 0,
                });
            }
        }
    }

    /// Stereo frames the movie channel has rendered, the audio-led film clock.
    pub(crate) fn movie_samples_consumed(&self) -> u64 {
        self.movie.as_ref().map_or(0, |movie| movie.consumed)
    }

    /// Whether a film-audio buffer is loaded.
    pub(crate) fn movie_active(&self) -> bool {
        self.movie.is_some()
    }

    /// Drop the film-audio buffer and its counter.
    pub(crate) fn stop_movie_audio(&mut self) {
        self.movie = None;
    }

    /// Suspend the game sounds (BGM, voice, one-shots) for a film.
    ///
    /// The original's `PauseSounds` stops the sounding BGM banks and the voice
    /// bank; `ResumePausedSounds` starts exactly those again from sample 0. The
    /// port does the same for the BGM channels and the voice channel. The
    /// one-shot pool is the port's own model (the original has no pool), so it
    /// is only frozen and keeps its position.
    pub(crate) fn pause_game_sounds(&mut self) {
        self.game_paused = true;
        for (index, channel) in self.bgm_channels.iter_mut().enumerate() {
            let Some(channel) = channel else { continue };
            self.paused_bgm[index] = channel.playing;
            if channel.playing {
                channel.playing = false;
                channel.pos = 0;
            }
        }
        self.paused_voice = self
            .voice
            .as_ref()
            .is_some_and(|voice| voice.pos < voice.pcm.len());
        if self.paused_voice
            && let Some(voice) = &mut self.voice
        {
            voice.pos = 0;
        }
    }

    /// Undo [`MixState::pause_game_sounds`], restarting the banks it stopped
    /// from sample 0.
    pub(crate) fn resume_game_sounds(&mut self) {
        self.game_paused = false;
        let paused = self.paused_bgm;
        self.paused_bgm = [false; 3];
        for (channel, was_playing) in self.bgm_channels.iter_mut().zip(paused) {
            if was_playing && let Some(channel) = channel {
                channel.playing = true;
                channel.pos = 0;
            }
        }
        if self.paused_voice {
            if let Some(voice) = &mut self.voice {
                voice.pos = 0;
            }
            self.voice_finished = false;
            self.paused_voice = false;
        }
    }

    fn is_silent(&self) -> bool {
        if self.movie.is_some() {
            return false;
        }
        if self.game_paused {
            return true;
        }
        !self.bgm_any_playing() && self.voice.is_none() && self.sfx.is_empty()
    }

    /// Mix `frames` stereo frames and append them to `out`.
    ///
    /// Voices are summed without a per-voice headroom term; each sample is
    /// saturated by [`push_sample`], which is how the original's hardware mixer
    /// handles overlapping banks. A paused mixer skips the game sounds but
    /// still renders the film.
    pub(crate) fn render(&mut self, frames: usize, out: &mut Vec<u8>) {
        for _ in 0..frames {
            let mut left = 0.0f32;
            let mut right = 0.0f32;

            if !self.game_paused {
                for channel in self.bgm_channels.iter_mut().flatten() {
                    if !channel.playing {
                        continue;
                    }
                    if channel.pos >= channel.pcm.len() {
                        if channel.looping {
                            channel.pos = 0;
                        } else {
                            channel.playing = false;
                            continue;
                        }
                    }
                    let sample = f32::from(channel.pcm[channel.pos]) * channel.gain;
                    let (l, r) = pan_gains(channel.pan);
                    left += sample * l;
                    right += sample * r;
                    channel.pos += 1;
                    if channel.pos >= channel.pcm.len() {
                        if channel.looping {
                            channel.pos = 0;
                        } else {
                            channel.playing = false;
                        }
                    }
                }

                if let Some(voice) = &mut self.voice
                    && voice.pos < voice.pcm.len()
                {
                    let sample = f32::from(voice.pcm[voice.pos]) * voice.gain;
                    let (l, r) = pan_gains(voice.pan);
                    left += sample * l;
                    right += sample * r;
                    voice.pos += 1;
                    if voice.pos >= voice.pcm.len() {
                        self.voice_finished = true;
                    }
                }
                if self.voice_finished {
                    self.voice = None;
                }

                for voice in &mut self.sfx {
                    if voice.pos >= voice.pcm.len() {
                        continue;
                    }
                    let sample = f32::from(voice.pcm[voice.pos]) * voice.gain;
                    let (l, r) = pan_gains(voice.pan);
                    left += sample * l;
                    right += sample * r;
                    voice.pos += 1;
                }
                self.sfx.retain(|voice| !voice.finished());
            }

            if let Some(movie) = &mut self.movie {
                let samples = movie.pcm.len() / 2;
                if movie.pos < samples {
                    left += f32::from(movie.pcm[movie.pos * 2]);
                    right += f32::from(movie.pcm[movie.pos * 2 + 1]);
                    movie.pos += 1;
                    movie.consumed += 1;
                }
            }

            push_sample(out, left);
            push_sample(out, right);
        }
    }

    /// Legacy single-track start: load channel 0 and start it.
    fn play_bgm(&mut self, pcm: Vec<i16>) {
        self.play_bgm_channel(0, pcm, true);
    }

    /// Legacy single-track stop: stop channel 0, keeping its buffer loaded.
    fn stop_bgm(&mut self) {
        self.stop_bgm_channel(0);
    }

    /// Legacy single-track volume: channel 0's gain.
    fn set_bgm_volume(&mut self, gain: f32) {
        self.set_bgm_channel_volume(0, gain);
    }
}

/// Clamp and append one mixed sample as little-endian signed 16-bit.
fn push_sample(out: &mut Vec<u8>, sample: f32) {
    let sample = sample.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
    out.extend_from_slice(&sample.to_le_bytes());
}

/// A software mixer backed by a single SDL audio stream.
pub struct Mixer {
    stream: *mut SDL_AudioStream,
    state: MixState,
    /// A share of the SDL lifetime: declared last so [`Drop`] destroys the
    /// stream before this releases the ref, and `SDL_Quit` cannot tear the
    /// audio subsystem down under a live stream.
    _sdl: SdlHandle,
}

impl Mixer {
    /// Initialize SDL audio and open a default playback device.
    ///
    /// Returns `None` when the audio subsystem or a default playback device
    /// cannot be opened; audio is optional and a failure is never fatal.
    pub fn open() -> Option<Mixer> {
        if !unsafe { SDL_InitSubSystem(SDL_INIT_AUDIO) } {
            return None;
        }
        let spec = SDL_AudioSpec {
            format: SDL_AUDIO_S16LE,
            channels: 2,
            freq: SAMPLE_RATE as c_int,
        };
        let stream = unsafe {
            SDL_OpenAudioDeviceStream(
                SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK,
                &spec,
                None,
                ptr::null_mut(),
            )
        };
        if stream.is_null() {
            return None;
        }
        Some(Mixer {
            stream,
            state: MixState::new(),
            _sdl: SdlHandle::retain(),
        })
    }

    /// Load `wav` into BGM channel `index` and start it from sample 0.
    ///
    /// The stream is fed on the next [`Mixer::update`], so the caller can set
    /// the channel's volume and pan before the first samples are rendered (a
    /// bank that loads muted must not leak a full-gain burst).
    pub fn play_bgm_channel(&mut self, index: usize, wav: Wav) {
        let Some(mono) = mono_or_warn(&wav) else {
            return;
        };
        self.state.load_bgm_channel(index, mono, true);
        self.state.restart_bgm_channel(index);
        self.resume();
    }

    /// Load `wav` into BGM channel `index` without starting it.
    pub fn load_bgm_channel(&mut self, index: usize, wav: Wav) {
        let Some(mono) = mono_or_warn(&wav) else {
            return;
        };
        self.state.load_bgm_channel(index, mono, true);
        self.resume();
    }

    /// Stop BGM channel `index`, rewinding it and keeping its buffer loaded.
    pub fn stop_bgm_channel(&mut self, index: usize) {
        self.state.stop_bgm_channel(index);
    }

    /// Start BGM channel `index` from sample 0.
    pub fn restart_bgm_channel(&mut self, index: usize) {
        self.state.restart_bgm_channel(index);
        self.resume();
    }

    /// Set BGM channel `index`'s gain; negative values are clamped to zero.
    pub fn set_bgm_channel_volume(&mut self, index: usize, gain: f32) {
        self.state.set_bgm_channel_volume(index, gain);
    }

    /// Set BGM channel `index`'s pan (-1 left, 1 right).
    pub fn set_bgm_channel_pan(&mut self, index: usize, pan: f32) {
        self.state.set_bgm_channel_pan(index, pan);
    }

    /// Whether BGM channel `index` is currently sounding.
    pub fn bgm_channel_playing(&self, index: usize) -> bool {
        self.state.bgm_channel_playing(index)
    }

    /// Whether BGM channel `index` has a loaded buffer.
    #[cfg(test)]
    pub(crate) fn bgm_channel_loaded(&self, index: usize) -> bool {
        self.state.bgm_channel_loaded(index)
    }

    /// Stop and drop every BGM channel.
    pub fn stop_all_bgm(&mut self) {
        self.state.stop_all_bgm();
    }

    /// Replace the looping BGM track and start it on channel 0.
    ///
    /// This is the legacy single-track entry point; it still clears the
    /// queued stream so a fresh track starts clean, exactly as M5 left it.
    pub fn play_bgm(&mut self, wav: Wav) -> Result<()> {
        self.state.stop_bgm();
        if !unsafe { SDL_ClearAudioStream(self.stream) } {
            bail!("SDL_ClearAudioStream failed");
        }
        self.state.play_bgm(to_mono(&wav)?);
        self.resume();
        self.update();
        Ok(())
    }

    /// Stop channel 0 and clear anything still queued.
    pub fn stop_bgm(&mut self) {
        self.state.stop_bgm();
        let _ = unsafe { SDL_ClearAudioStream(self.stream) };
    }

    /// Set channel 0's gain; negative values are clamped to zero.
    pub fn set_bgm_volume(&mut self, gain: f32) {
        self.state.set_bgm_volume(gain);
    }

    /// Replace the voice (dialogue) buffer and start it from sample 0.
    pub fn play_voice(&mut self, wav: Wav, gain: f32, pan: f32) {
        let Some(mono) = mono_or_warn(&wav) else {
            return;
        };
        self.state.play_voice(mono, gain, pan);
        self.resume();
    }

    /// Stop the voice channel.
    pub fn stop_voice(&mut self) {
        self.state.stop_voice();
    }

    /// Whether the voice channel has a buffer loaded.
    pub fn voice_playing(&self) -> bool {
        self.state.voice_playing()
    }

    /// Whether the voice channel has played its buffer to the end.
    pub fn voice_finished(&self) -> bool {
        self.state.voice_finished()
    }

    /// Start a one-shot voice with `gain` and `pan` (-1 left, 1 right).
    ///
    /// The unkeyed path always appends; the engine's gameplay cues use
    /// [`Mixer::play_sfx_on_bank`] instead.
    pub fn play_sfx(&mut self, wav: Wav, gain: f32, pan: f32) {
        let Some(mono) = mono_or_warn(&wav) else {
            return;
        };
        self.state.play_sfx(mono, gain, pan);
        self.resume();
        self.update();
    }

    /// Start a one-shot voice on `bank`, restarting that bank's voice when it
    /// is already sounding (the original's one DirectSound buffer per sound
    /// record).
    pub fn play_sfx_on_bank(&mut self, bank: u64, wav: Wav, gain: f32, pan: f32) {
        let Some(mono) = mono_or_warn(&wav) else {
            return;
        };
        self.state.play_sfx_on_bank(bank, mono, gain, pan);
        self.resume();
        self.update();
    }

    /// Append one film-audio chunk (interleaved stereo s16 at
    /// [`SAMPLE_RATE`]), starting the movie channel on the first call.
    pub fn load_movie_audio(&mut self, pcm: Vec<i16>) {
        self.state.load_movie_audio(pcm);
        self.resume();
    }

    /// Stereo frames the movie channel has rendered, the audio-led film clock.
    pub fn movie_samples_consumed(&self) -> u64 {
        self.state.movie_samples_consumed()
    }

    /// Whether a film-audio buffer is loaded.
    pub fn movie_active(&self) -> bool {
        self.state.movie_active()
    }

    /// Drop the film-audio buffer, its counter and any film audio already
    /// queued on the device, so the tail cannot bleed past the film's end.
    pub fn stop_movie_audio(&mut self) {
        self.state.stop_movie_audio();
        self.clear_stream();
    }

    /// Suspend the game sounds while a film plays. The sounding banks are
    /// stopped and will restart from sample 0 on resume, and the device queue
    /// is cleared so queued game audio cannot bleed over the film's start.
    pub fn pause_game_sounds(&mut self) {
        self.state.pause_game_sounds();
        self.clear_stream();
    }

    /// Resume the game sounds after a film, restarting the paused banks from
    /// sample 0.
    pub fn resume_game_sounds(&mut self) {
        self.state.resume_game_sounds();
    }

    /// Whether the game sounds are suspended for a film. Test seam for the
    /// engine's film hand-off.
    #[cfg(test)]
    pub(crate) fn game_sounds_paused(&self) -> bool {
        self.state.game_paused
    }

    /// Whether SDL selected the silent dummy driver, whose stream is never
    /// consumed: the engine then treats the run as device-less and paces films
    /// with the fixed 30 Hz tick.
    pub fn is_dummy(&self) -> bool {
        let driver = unsafe { SDL_GetCurrentAudioDriver() };
        if driver.is_null() {
            return false;
        }
        unsafe { CStr::from_ptr(driver) }.to_bytes() == b"dummy"
    }

    /// Render and queue enough samples to keep the device fed; call once per
    /// frame. A no-op when no voice is active.
    pub fn update(&mut self) {
        if self.state.is_silent() {
            return;
        }
        let queued = unsafe { SDL_GetAudioStreamQueued(self.stream) };
        if queued < 0 {
            return;
        }
        let queued_frames = queued as usize / BYTES_PER_FRAME;
        let frames = TARGET_QUEUED_FRAMES
            .saturating_sub(queued_frames)
            .min(MAX_RENDER_FRAMES);
        if frames == 0 {
            return;
        }

        let mut pcm = Vec::with_capacity(frames * BYTES_PER_FRAME);
        self.state.render(frames, &mut pcm);
        let Ok(len) = c_int::try_from(pcm.len()) else {
            return;
        };
        let _ = unsafe { SDL_PutAudioStreamData(self.stream, pcm.as_ptr().cast(), len) };
    }

    /// Advance the mixer state by `frames` without the queued-frames throttle,
    /// so a test can play a short buffer to its end deterministically.
    #[cfg(test)]
    pub(crate) fn render_for_test(&mut self, frames: usize) {
        let mut pcm = Vec::new();
        self.state.render(frames, &mut pcm);
    }

    /// Whether BGM channel 0 is currently sounding.
    pub fn is_playing(&self) -> bool {
        self.state.bgm_channel_playing(0)
    }

    /// Number of live one-shot voices. Test seam for the engine's bank keys.
    #[cfg(test)]
    pub(crate) fn active_sfx(&self) -> usize {
        self.state.active_sfx()
    }

    /// Alias for [`Mixer::play_bgm`], kept for the engine's music path.
    pub fn play(&mut self, wav: Wav) -> Result<()> {
        self.play_bgm(wav)
    }

    /// Alias for [`Mixer::stop_bgm`].
    pub fn stop(&mut self) {
        self.stop_bgm();
    }

    /// Alias for [`Mixer::set_bgm_volume`].
    pub fn set_volume(&mut self, gain: f32) {
        self.set_bgm_volume(gain);
    }

    /// Resume the device, ignoring failure: audio is best-effort.
    fn resume(&self) {
        let _ = unsafe { SDL_ResumeAudioStreamDevice(self.stream) };
    }

    /// Drop every sample already queued on the device; the next
    /// [`Mixer::update`] refills from the current mixer state.
    fn clear_stream(&self) {
        let _ = unsafe { SDL_ClearAudioStream(self.stream) };
    }
}

impl Drop for Mixer {
    fn drop(&mut self) {
        unsafe { SDL_DestroyAudioStream(self.stream) };
    }
}

/// Compatibility alias for the engine's music path.
pub type MusicPlayer = Mixer;

/// Serializes the tests that open SDL audio or video.
///
/// SDL's lifetime and the dummy-driver hint are process-wide globals, so
/// tests that open devices concurrently race on SDL's setup and teardown.
/// Every test that opens a [`Mixer`] or a display must hold this lock for its
/// whole body.
#[cfg(test)]
pub(crate) mod test_lock {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    pub(crate) fn sdl() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sdl3_sys::hints::{SDL_HINT_AUDIO_DRIVER, SDL_ResetHint, SDL_SetHint};

    fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(id);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn fmt_payload(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let block_align = channels * bits.div_ceil(8);
        let mut payload = Vec::new();
        payload.extend_from_slice(&tag.to_le_bytes());
        payload.extend_from_slice(&channels.to_le_bytes());
        payload.extend_from_slice(&rate.to_le_bytes());
        payload.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
        payload.extend_from_slice(&block_align.to_le_bytes());
        payload.extend_from_slice(&bits.to_le_bytes());
        payload
    }

    fn riff(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(body);
        out
    }

    fn wav_bytes(format: WavFormat, channels: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let bits = match format {
            WavFormat::U8 => 8,
            WavFormat::S16Le => 16,
        };
        let mut body = chunk(b"fmt ", &fmt_payload(1, channels, rate, bits));
        body.extend_from_slice(&chunk(b"data", data));
        riff(&body)
    }

    fn parse_body(body: &[u8]) -> String {
        parse_wav(&riff(body)).unwrap_err().to_string()
    }

    fn s16_bytes(samples: &[i16]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    fn out_samples(bytes: &[u8]) -> Vec<i16> {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| i16::from_le_bytes(*chunk))
            .collect()
    }

    #[test]
    fn parses_u8_mono() {
        let data: Vec<u8> = (0..=255).collect();
        let wav = parse_wav(&wav_bytes(WavFormat::U8, 1, 22050, &data)).unwrap();
        assert_eq!(wav.format, WavFormat::U8);
        assert_eq!(wav.channels, 1);
        assert_eq!(wav.sample_rate, 22050);
        assert_eq!(wav.data, data);
    }

    #[test]
    fn parses_s16le_stereo() {
        let data: Vec<u8> = (0..=15).collect();
        let wav = parse_wav(&wav_bytes(WavFormat::S16Le, 2, 44100, &data)).unwrap();
        assert_eq!(wav.format, WavFormat::S16Le);
        assert_eq!(wav.channels, 2);
        assert_eq!(wav.sample_rate, 44100);
        assert_eq!(wav.data, data);
    }

    #[test]
    fn accepts_empty_data() {
        let wav = parse_wav(&wav_bytes(WavFormat::U8, 1, 8000, &[])).unwrap();
        assert!(wav.data.is_empty());
    }

    #[test]
    fn skips_unknown_and_odd_sized_chunks() {
        let data: Vec<u8> = (0..100).collect();
        let mut body = chunk(b"LIST", b"INFOxxxx");
        body.extend_from_slice(&chunk(b"fmt ", &fmt_payload(1, 2, 11025, 16)));
        body.extend_from_slice(&chunk(b"junk", b"odd-sized"));
        body.extend_from_slice(&chunk(b"data", &data));
        let wav = parse_wav(&riff(&body)).unwrap();
        assert_eq!(wav.format, WavFormat::S16Le);
        assert_eq!(wav.channels, 2);
        assert_eq!(wav.sample_rate, 11025);
        assert_eq!(wav.data, data);
    }

    #[test]
    fn parses_data_chunk_before_fmt() {
        let data = vec![7u8; 33];
        let mut body = chunk(b"data", &data);
        body.extend_from_slice(&chunk(b"fmt ", &fmt_payload(1, 1, 8000, 8)));
        let wav = parse_wav(&riff(&body)).unwrap();
        assert_eq!(wav.data, data);
        assert_eq!(wav.sample_rate, 8000);
    }

    #[test]
    fn rejects_missing_fmt() {
        let body = chunk(b"data", &[0u8; 4]);
        assert!(parse_body(&body).contains("fmt"));
    }

    #[test]
    fn rejects_missing_data() {
        let body = chunk(b"fmt ", &fmt_payload(1, 1, 22050, 8));
        assert!(parse_body(&body).contains("data"));
    }

    #[test]
    fn rejects_truncated_data_chunk() {
        let mut bytes = wav_bytes(WavFormat::U8, 1, 22050, &[1, 2, 3, 4]);
        bytes.truncate(bytes.len() - 2);
        let err = parse_wav(&bytes).unwrap_err().to_string();
        assert!(err.contains("truncated"), "{err}");
    }

    #[test]
    fn rejects_truncated_fmt_chunk() {
        let mut bytes = wav_bytes(WavFormat::U8, 1, 22050, &[1, 2, 3, 4]);
        bytes.truncate(12 + 8 + 4);
        let err = parse_wav(&bytes).unwrap_err().to_string();
        assert!(err.contains("truncated"), "{err}");
    }

    #[test]
    fn rejects_short_fmt_chunk() {
        let mut body = chunk(b"fmt ", &[0u8; 12]);
        body.extend_from_slice(&chunk(b"data", &[0u8; 4]));
        let err = parse_body(&body);
        assert!(err.contains("too small"), "{err}");
    }

    #[test]
    fn rejects_bad_containers() {
        assert!(parse_wav(b"nope").unwrap_err().to_string().contains("RIFF"));
        let mut bytes = wav_bytes(WavFormat::U8, 1, 22050, &[1]);
        bytes[8..12].copy_from_slice(b"AVI ");
        assert!(parse_wav(&bytes).unwrap_err().to_string().contains("WAVE"));
    }

    #[test]
    fn rejects_unsupported_encodings() {
        let mut body = chunk(b"fmt ", &fmt_payload(3, 1, 22050, 8));
        body.extend_from_slice(&chunk(b"data", &[0u8; 4]));
        let err = parse_body(&body);
        assert!(err.contains("format tag 3"), "{err}");

        let mut body = chunk(b"fmt ", &fmt_payload(1, 1, 22050, 24));
        body.extend_from_slice(&chunk(b"data", &[0u8; 4]));
        let err = parse_body(&body);
        assert!(err.contains("sample width 24"), "{err}");

        let mut body = chunk(b"fmt ", &fmt_payload(1, 0, 22050, 8));
        body.extend_from_slice(&chunk(b"data", &[0u8; 4]));
        let err = parse_body(&body);
        assert!(err.contains("zero channels"), "{err}");

        let mut body = chunk(b"fmt ", &fmt_payload(1, 1, 0, 8));
        body.extend_from_slice(&chunk(b"data", &[0u8; 4]));
        let err = parse_body(&body);
        assert!(err.contains("zero sample rate"), "{err}");
    }

    #[test]
    fn mixer_upsamples_u8_to_s16() {
        let wav = parse_wav(&wav_bytes(WavFormat::U8, 1, 22050, &[0, 128, 255])).unwrap();
        assert_eq!(to_mono(&wav).unwrap(), vec![-32768, 0, 32512]);
    }

    #[test]
    fn mixer_averages_channels_to_mono() {
        let data = s16_bytes(&[1000, 3000, -1000, -3000]);
        let wav = parse_wav(&wav_bytes(WavFormat::S16Le, 2, 22050, &data)).unwrap();
        assert_eq!(to_mono(&wav).unwrap(), vec![2000, -2000]);
    }

    #[test]
    fn mixer_resamples_44100_to_22050() {
        let data = s16_bytes(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let wav = parse_wav(&wav_bytes(WavFormat::S16Le, 1, 44100, &data)).unwrap();
        assert_eq!(to_mono(&wav).unwrap(), vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn rejects_a_resample_bomb_before_allocating_it() {
        // A 1 Hz source expands by 22,050x; a small file would otherwise ask
        // for hundreds of millions of samples.
        let wav = Wav {
            format: WavFormat::U8,
            channels: 1,
            sample_rate: 1,
            data: vec![0u8; 8192],
        };
        let message = budget::assert_cap_error(to_mono(&wav));
        assert!(message.contains("resampled WAV sample count"), "{message}");
    }

    #[test]
    fn mixer_pan_is_the_directsound_decibel_law() {
        // Centered is unity on both channels; only the far channel attenuates.
        assert_eq!(pan_gains(0.0), (1.0, 1.0));
        assert_eq!(pan_gains(-1.0), (1.0, 1e-5));
        assert_eq!(pan_gains(1.0), (1e-5, 1.0));
        // Half pan: the far channel is down 50 dB.
        assert!((pan_gains(-0.5).0 - 1.0).abs() < 1e-6);
        assert!((pan_gains(-0.5).1 - 10.0f32.powf(-2.5)).abs() < 1e-6);
        assert!((pan_gains(0.5).0 - 10.0f32.powf(-2.5)).abs() < 1e-6);
        assert!((pan_gains(0.5).1 - 1.0).abs() < 1e-6);
        assert_eq!(pan_gains(-2.0), pan_gains(-1.0));
        assert_eq!(pan_gains(2.0), pan_gains(1.0));
    }

    #[test]
    fn mixer_pan_follows_the_3d_curves_at_the_documented_angles() {
        // The sfx tests' camera geometry: straight ahead at 1000 units, 90
        // degrees off the look axis, and the far wide-angle case. The pan pair
        // runs through `pan_position` and then the DirectSound dB law.
        let gains = |from, to, sound| {
            let (_, pan) = crate::sfx::sound_gain_pan(from, to, sound);
            pan_gains(pan)
        };
        let ahead = gains([0, 0, 0], [1000, 0, 0], [1000, 0, 0]);
        assert_eq!(ahead, (1.0, 1.0));

        // scene_pan gives (123, 83): pan = (83 - 123) * 78 / 10000 = -0.312,
        // so the right channel is down 31.2 dB.
        let wide = gains([0, 0, 0], [1000, 0, 0], [0, 0, 2000]);
        assert!((wide.0 - 1.0).abs() < 1e-6, "{wide:?}");
        assert!((wide.1 - 10.0f32.powf(-1.56)).abs() < 1e-5, "{wide:?}");

        // scene_pan gives (67, 41) far away: pan = (41 - 67) * 78 / 10000.
        let far = gains([0, 0, 0], [1000, 0, 0], [0, 0, 30000]);
        assert!((far.0 - 1.0).abs() < 1e-6, "{far:?}");
        assert!((far.1 - 10.0f32.powf(-1.014)).abs() < 1e-5, "{far:?}");
    }

    #[test]
    fn mixer_applies_gain_and_pan() {
        let mut state = MixState::new();
        state.play_bgm(vec![1000, 1000]);
        state.set_bgm_volume(0.5);

        let mut out = Vec::new();
        state.render(1, &mut out);
        // Centered: full gain in both channels.
        assert_eq!(out_samples(&out), vec![500, 500]);
    }

    #[test]
    fn mixer_bgm_loops_its_whole_buffer() {
        let mut state = MixState::new();
        state.play_bgm(vec![100, -100, 50]);
        assert!(!state.is_silent());

        let mut out = Vec::new();
        state.render(3, &mut out);
        assert_eq!(out_samples(&out), vec![100, 100, -100, -100, 50, 50]);
        assert!(state.bgm_channel_loaded(0));
        assert!(state.bgm_channel_playing(0));
        assert_eq!(state.bgm_channels[0].as_ref().unwrap().pos, 0);

        state.stop_bgm();
        assert!(!state.bgm_channel_playing(0));
        assert!(state.is_silent());
    }

    #[test]
    fn mixer_one_shots_expire() {
        let mut state = MixState::new();
        state.play_sfx(vec![100, 200], 1.0, 0.0);
        assert_eq!(state.sfx.len(), 1);

        let mut out = Vec::new();
        state.render(1, &mut out);
        assert_eq!(state.sfx.len(), 1);
        state.render(1, &mut out);
        assert!(state.sfx.is_empty());
        assert!(state.is_silent());

        // Rendering after expiry is a silent no-op.
        let mut out = Vec::new();
        state.render(2, &mut out);
        assert_eq!(out_samples(&out), vec![0; 4]);
    }

    #[test]
    fn mixer_movie_channel_renders_and_counts() {
        let mut state = MixState::new();
        assert!(!state.movie_active());
        state.load_movie_audio(vec![1000, -1000, 2000, -2000]);
        assert!(state.movie_active());

        let mut out = Vec::new();
        state.render(1, &mut out);
        assert_eq!(out_samples(&out), vec![1000, -1000]);
        assert_eq!(state.movie_samples_consumed(), 1);

        // The counter stops at the queued tail instead of counting silence.
        state.render(5, &mut out);
        assert_eq!(state.movie_samples_consumed(), 2);
        state.load_movie_audio(vec![300, 400]);
        state.render(1, &mut out);
        assert_eq!(state.movie_samples_consumed(), 3);
        assert_eq!(out_samples(&out)[out_samples(&out).len() - 2..], [300, 400]);

        state.stop_movie_audio();
        assert!(!state.movie_active());
        assert_eq!(state.movie_samples_consumed(), 0);
    }

    #[test]
    fn mixer_movie_pauses_and_restarts_the_sound_banks() {
        let mut state = MixState::new();
        state.play_bgm_channel(0, vec![100, 200, 300, 400], true);
        state.play_voice(vec![10, 20, 30, 40], 1.0, 0.0);
        state.load_movie_audio(vec![1000, 1000, 2000, 2000, 3000, 3000]);

        // Unpaused: the looping BGM, the voice bank and the film sum. Both
        // banks are centered, so each contributes its full sample per channel.
        let mut out = Vec::new();
        state.render(1, &mut out);
        let mixed = (100.0 + 10.0) as i16;
        assert_eq!(out_samples(&out), vec![1000 + mixed, 1000 + mixed]);

        // Pausing stops the BGM and voice banks and rewinds them; the film
        // still renders.
        state.pause_game_sounds();
        let mut out = Vec::new();
        state.render(1, &mut out);
        assert_eq!(out_samples(&out), vec![2000, 2000]);
        assert!(!state.bgm_channel_playing(0));
        assert_eq!(state.bgm_channels[0].as_ref().unwrap().pos, 0);

        // Resuming restarts the paused banks from sample 0, exactly like the
        // original's stop/play pairs, so the first BGM and voice samples sound
        // again rather than continuing at sample 1.
        state.resume_game_sounds();
        let mut out = Vec::new();
        state.render(1, &mut out);
        assert_eq!(out_samples(&out), vec![3000 + mixed, 3000 + mixed]);
        assert!(state.bgm_channel_playing(0));
        assert_eq!(state.bgm_channels[0].as_ref().unwrap().pos, 1);
        assert_eq!(state.voice.as_ref().unwrap().pos, 1);
    }

    #[test]
    fn mixer_pause_only_restarts_the_banks_that_were_playing() {
        let mut state = MixState::new();
        state.play_bgm_channel(0, vec![100, 200, 300], true);
        state.stop_bgm_channel(0);
        state.play_bgm_channel(1, vec![400, 500, 600], true);
        state.stop_bgm_channel(1);
        state.play_voice(vec![7, 8, 9], 1.0, 0.0);
        state.stop_voice();

        state.pause_game_sounds();
        state.resume_game_sounds();
        assert!(
            !state.bgm_channel_playing(0),
            "a stopped bank stays stopped"
        );
        assert!(
            !state.bgm_channel_playing(1),
            "a stopped bank stays stopped"
        );
        assert!(!state.voice_playing(), "a stopped voice stays stopped");
    }

    #[test]
    fn mixer_same_bank_restarts_the_voice() {
        let mut state = MixState::new();
        state.play_sfx_on_bank(7, vec![1000i16; 8], 1.0, 0.0);
        assert_eq!(state.active_sfx(), 1);
        let mut out = Vec::new();
        state.render(1, &mut out);
        assert_eq!(out_samples(&out), vec![1000, 1000]);

        // The same bank one tick later must restart that one voice from sample
        // 0 with the new buffer and parameters, not append a second copy.
        state.play_sfx_on_bank(7, vec![2000i16; 8], 1.0, 0.0);
        assert_eq!(state.active_sfx(), 1, "the second cue appended a voice");
        assert_eq!(state.sfx[0].pos, 0, "the bank voice did not seek to zero");
        assert_eq!(state.sfx[0].pcm[0], 2000, "the buffer was not replaced");

        let mut out = Vec::new();
        state.render(1, &mut out);
        assert_eq!(out_samples(&out), vec![2000, 2000]);

        // A voice restarted after expiry starts fresh, not at old progress.
        state.render(7, &mut out);
        assert!(state.sfx.is_empty());
        state.play_sfx_on_bank(7, vec![100i16; 2], 1.0, 0.0);
        assert_eq!(state.active_sfx(), 1);
        assert_eq!(state.sfx[0].pos, 0);
    }

    #[test]
    fn mixer_distinct_banks_append_and_sum() {
        let mut state = MixState::new();
        state.play_sfx_on_bank(1, vec![1000i16; 8], 1.0, 0.0);
        assert_eq!(state.active_sfx(), 1);

        // A different bank is a different one-shot: it appends and sums.
        state.play_sfx_on_bank(2, vec![1000i16; 8], 1.0, 0.0);
        assert_eq!(state.active_sfx(), 2, "the distinct cue replaced the first");
        assert_eq!(state.sfx[0].pos, 0);
        assert_eq!(state.sfx[1].pos, 0);

        let mut out = Vec::new();
        state.render(1, &mut out);
        assert_eq!(out_samples(&out), vec![2000, 2000]);
        assert_eq!(state.sfx[0].pos, 1);
        assert_eq!(state.sfx[1].pos, 1);
    }

    #[test]
    fn mixer_four_distinct_banks_stack() {
        let mut state = MixState::new();
        for bank in 0..4 {
            state.play_sfx_on_bank(bank, vec![1000i16; 4], 1.0, 0.0);
        }
        assert_eq!(state.active_sfx(), 4);

        let mut out = Vec::new();
        state.render(1, &mut out);
        assert_eq!(out_samples(&out), vec![4000, 4000]);
        assert!(state.sfx.iter().all(|voice| voice.pos == 1));

        // All four expire together and the mixer goes silent.
        state.render(3, &mut out);
        assert_eq!(state.active_sfx(), 0);
        assert!(state.is_silent());
    }

    #[test]
    fn mixer_pool_exhaustion_steals_the_furthest_voice() {
        let mut state = MixState::new();
        state.play_sfx(vec![111; 32], 1.0, 0.0);
        let mut out = Vec::new();
        state.render(5, &mut out);
        for index in 1..MAX_SFX_VOICES {
            state.play_sfx(vec![index as i16; 32], 1.0, 0.0);
        }
        assert_eq!(state.active_sfx(), MAX_SFX_VOICES);
        assert_eq!(state.sfx[0].pos, 5, "the lone older voice should lead");

        // The full pool drops the most-played voice, not the newest one.
        state.play_sfx(vec![999; 32], 1.0, 0.0);
        assert_eq!(state.active_sfx(), MAX_SFX_VOICES);
        assert!(
            !state.sfx.iter().any(|voice| voice.pcm[0] == 111),
            "the furthest voice was not the one stolen"
        );
        assert!(state.sfx.iter().any(|voice| voice.pcm[0] == 999));
        assert!(state.sfx.iter().any(|voice| voice.pcm[0] == 15));
    }

    #[test]
    fn mixer_pool_exhaustion_ties_break_oldest_first() {
        let mut state = MixState::new();
        for index in 0..MAX_SFX_VOICES {
            state.play_sfx(vec![index as i16; 8], 1.0, 0.0);
        }
        // Nothing has rendered, so every voice is tied at position zero: the
        // oldest voice must be the one evicted.
        state.play_sfx(vec![999; 8], 1.0, 0.0);
        assert_eq!(state.active_sfx(), MAX_SFX_VOICES);
        assert!(
            !state.sfx.iter().any(|voice| voice.pcm[0] == 0),
            "the oldest voice was not the one stolen"
        );
        assert!(state.sfx.iter().any(|voice| voice.pcm[0] == 15));
        assert_eq!(state.sfx.last().unwrap().pcm[0], 999);
    }

    #[test]
    fn mixer_long_bgm_mixes_with_overlapping_sfx() {
        let mut state = MixState::new();
        let bgm = vec![100i16; 64];
        state.play_bgm(bgm);
        state.set_bgm_volume(1.0);

        // Two identical one-shots over a BGM that keeps looping.
        state.play_sfx(vec![1000i16; 8], 1.0, 0.0);
        state.play_sfx(vec![1000i16; 8], 1.0, 0.0);

        let expected = (100.0 + 2.0 * 1000.0) as i16;
        let mut out = Vec::new();
        state.render(1, &mut out);
        assert_eq!(out_samples(&out), vec![expected, expected]);
        assert!(state.bgm_channel_playing(0), "the BGM channel was dropped");
        assert_eq!(state.active_sfx(), 2);

        // Render past the BGM buffer and the one-shots: the loop survives.
        state.render(64, &mut out);
        assert!(state.bgm_channel_playing(0));
        assert_eq!(state.active_sfx(), 0);
        assert!(!state.is_silent());
    }

    #[test]
    fn mixer_many_voices_clamp_instead_of_wrapping() {
        let mut state = MixState::new();
        for _ in 0..4 {
            state.play_sfx(vec![20000i16; 2], 2.0, -1.0);
        }
        let mut out = Vec::new();
        state.render(1, &mut out);
        // Four copies of 20000 at gain 2 sum to 160000 on the hard-left
        // channel: it saturates at the rail and never wraps negative. The
        // unused channel keeps only the far-channel attenuation tail.
        assert_eq!(out_samples(&out), vec![i16::MAX, 1]);
    }

    #[test]
    fn mixer_empty_sources_are_ignored() {
        let mut state = MixState::new();
        state.play_bgm(Vec::new());
        state.play_sfx(Vec::new(), 1.0, 0.0);
        assert!(state.is_silent());
    }

    #[test]
    #[ignore = "requires an audio device or SDL dummy driver"]
    fn dummy_driver_plays_loops_and_stops() {
        let _ = unsafe { SDL_SetHint(SDL_HINT_AUDIO_DRIVER, c"dummy".as_ptr()) };
        let mut player = Mixer::open().expect("dummy audio device should open");
        let _ = unsafe { SDL_ResetHint(SDL_HINT_AUDIO_DRIVER) };

        let data: Vec<u8> = (0..2205).map(|i| (i % 251) as u8).collect();
        let wav = parse_wav(&wav_bytes(WavFormat::U8, 1, 22050, &data)).unwrap();
        player.play_bgm(wav).unwrap();
        assert!(player.is_playing());
        assert!(unsafe { SDL_GetAudioStreamQueued(player.stream) } > 0);

        player.update();
        player.update();
        player.set_bgm_volume(0.5);

        let sfx = parse_wav(&wav_bytes(WavFormat::S16Le, 1, 22050, &[0u8; 64])).unwrap();
        player.play_sfx(sfx, 0.25, 0.5);
        player.update();

        player.stop_bgm();
        assert!(!player.is_playing());

        // Stopping a film drops its queued tail from the device stream.
        player.load_movie_audio(vec![1234; 512 * 4]);
        player.update();
        assert!(unsafe { SDL_GetAudioStreamQueued(player.stream) } > 0);
        player.stop_movie_audio();
        assert_eq!(unsafe { SDL_GetAudioStreamQueued(player.stream) }, 0);

        // Pausing for a film drops the queued game audio too.
        let wav = parse_wav(&wav_bytes(WavFormat::U8, 1, 22050, &data)).unwrap();
        player.play_bgm(wav).unwrap();
        player.update();
        assert!(unsafe { SDL_GetAudioStreamQueued(player.stream) } > 0);
        player.pause_game_sounds();
        assert_eq!(unsafe { SDL_GetAudioStreamQueued(player.stream) }, 0);
        player.resume_game_sounds();
        assert!(player.is_playing());
    }

    #[test]
    #[ignore = "requires a real RE1 installation via ARKLAY_RE1_ROOT"]
    fn real_footstep_wav_parses_and_plays() {
        let Ok(root) = std::env::var("ARKLAY_RE1_ROOT") else {
            return;
        };
        let sound = std::path::Path::new(&root).join("JPN/sound");
        let entry = std::fs::read_dir(&sound)
            .unwrap_or_else(|error| panic!("failed to list {}: {error}", sound.display()))
            .filter_map(|entry| entry.ok())
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("ft_wda.wav")
            })
            .unwrap_or_else(|| panic!("no ft_wdA.wav in {}", sound.display()));
        let wav = parse_wav(&std::fs::read(entry.path()).unwrap()).unwrap();
        assert_eq!(wav.sample_rate, SAMPLE_RATE);
        assert!(wav.channels >= 1);

        let _ = unsafe { SDL_SetHint(SDL_HINT_AUDIO_DRIVER, c"dummy".as_ptr()) };
        let mut player = Mixer::open().expect("dummy audio device should open");
        let _ = unsafe { SDL_ResetHint(SDL_HINT_AUDIO_DRIVER) };
        player.play_sfx(wav, 1.0, 0.0);
        player.update();
        player.update();
    }

    #[test]
    fn mixer_bgm_channels_are_independent() {
        let mut state = MixState::new();
        state.play_bgm_channel(0, vec![1000, 1000], true);
        state.play_bgm_channel(2, vec![1000, 1000], true);
        assert!(state.bgm_channel_playing(0));
        assert!(!state.bgm_channel_playing(1));
        assert!(state.bgm_channel_playing(2));

        state.set_bgm_channel_volume(1, 0.25);
        assert!(
            !state.bgm_channel_playing(1),
            "volume does not start a bank"
        );

        state.stop_bgm_channel(0);
        state.set_bgm_channel_volume(2, 0.5);
        state.set_bgm_channel_pan(2, 1.0);

        let mut out = Vec::new();
        state.render(1, &mut out);
        // Only channel 2 sounds: half gain, hard right.
        assert_eq!(out_samples(&out), vec![0, 500]);
        assert!(state.bgm_channel_loaded(0), "stop keeps the buffer loaded");
    }

    #[test]
    fn mixer_bgm_restart_seeks_to_sample_zero() {
        let mut state = MixState::new();
        state.play_bgm_channel(0, vec![100, 200, 300], false);
        let mut out = Vec::new();
        state.render(2, &mut out);
        assert_eq!(out_samples(&out).len(), 4);

        state.restart_bgm_channel(0);
        let mut out = Vec::new();
        state.render(1, &mut out);
        let expected = 100;
        assert_eq!(out_samples(&out), vec![expected, expected]);

        // A non-looping bank stops itself at the end but stays loaded.
        state.render(4, &mut out);
        assert!(!state.bgm_channel_playing(0));
        assert!(state.bgm_channel_loaded(0));
        state.restart_bgm_channel(0);
        assert!(state.bgm_channel_playing(0));
    }

    #[test]
    fn mixer_stop_all_bgm_drops_every_bank() {
        let mut state = MixState::new();
        for index in 0..MixState::BGM_CHANNELS {
            state.play_bgm_channel(index, vec![100, 100], true);
        }
        assert!(state.bgm_any_playing());
        state.stop_all_bgm();
        for index in 0..MixState::BGM_CHANNELS {
            assert!(!state.bgm_channel_loaded(index));
            assert!(!state.bgm_channel_playing(index));
        }
        assert!(state.is_silent());
    }

    #[test]
    fn mixer_voice_replaces_and_reports_finish() {
        let mut state = MixState::new();
        state.play_voice(vec![100, 200], 1.0, 0.0);
        assert!(state.voice_playing());
        assert!(!state.voice_finished());

        let mut out = Vec::new();
        state.render(1, &mut out);
        assert!(state.voice_playing());
        assert!(!state.voice_finished());
        state.render(1, &mut out);
        assert!(!state.voice_playing());
        assert!(state.voice_finished());

        // The finish report is sticky until the next play or stop.
        state.render(4, &mut out);
        assert!(state.voice_finished());

        // A replacement restarts the buffer and clears the report.
        state.play_voice(vec![7, 8], 1.0, 0.0);
        assert!(state.voice_playing());
        assert!(!state.voice_finished());
        state.stop_voice();
        assert!(!state.voice_playing());
        assert!(!state.voice_finished());
    }

    #[test]
    fn mixer_voice_mixes_with_bgm_and_one_shots() {
        let mut state = MixState::new();
        state.play_bgm(vec![100, 100]);
        state.play_voice(vec![1000, 1000], 1.0, -1.0);
        state.play_sfx(vec![1000, 1000], 1.0, 1.0);

        let mut out = Vec::new();
        state.render(1, &mut out);
        let left = (100.0 + 1000.0) as i16;
        let right = (100.0 + 1000.0) as i16;
        let samples = out_samples(&out);
        assert!(
            (i32::from(samples[0]) - i32::from(left)).abs() <= 1,
            "{samples:?}"
        );
        assert!(
            (i32::from(samples[1]) - i32::from(right)).abs() <= 1,
            "{samples:?}"
        );
        assert!(state.voice_playing());
    }
}
