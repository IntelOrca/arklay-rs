//! WAV parsing and a software mixer feeding one SDL3 stream.
//!
//! [`Mixer`] owns the only SDL audio stream in the engine. Every source is
//! converted to 16-bit signed mono at 22050 Hz on load (8-bit samples are
//! upsampled, other rates are linearly resampled). Each [`Mixer::update`]
//! renders and queues interleaved stereo samples: one looping BGM voice plus a
//! pool of one-shot voices, each with its own gain and pan. A missing device or
//! a failed SDL call leaves the engine silent, never fatal.

use std::ffi::c_int;
use std::ptr;

use anyhow::{Context, Result, bail};

use sdl3_sys::audio::{
    SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, SDL_AUDIO_S16LE, SDL_AudioSpec, SDL_AudioStream,
    SDL_ClearAudioStream, SDL_DestroyAudioStream, SDL_GetAudioStreamQueued,
    SDL_OpenAudioDeviceStream, SDL_PutAudioStreamData, SDL_ResumeAudioStreamDevice,
};
use sdl3_sys::init::{SDL_INIT_AUDIO, SDL_InitSubSystem};

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
            b"data" => data = Some(bytes[start..end].to_vec()),
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
/// mono, and any other sample rate is linearly resampled.
fn to_mono(wav: &Wav) -> Vec<i16> {
    let mut mono: Vec<i16> = match wav.format {
        WavFormat::U8 => wav
            .data
            .iter()
            .map(|&byte| (i16::from(byte) - 128) << 8)
            .collect(),
        WavFormat::S16Le => wav
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| i16::from_le_bytes(*chunk))
            .collect(),
    };

    if wav.channels > 1 {
        let channels = usize::from(wav.channels);
        mono = mono
            .chunks(channels)
            .map(|frame| {
                let sum: i32 = frame.iter().map(|&sample| i32::from(sample)).sum();
                (sum / channels as i32) as i16
            })
            .collect();
    }

    if wav.sample_rate != SAMPLE_RATE {
        mono = resample_linear(&mono, wav.sample_rate, SAMPLE_RATE);
    }
    mono
}

/// Linearly resample `input` from `from` Hz to `to` Hz.
fn resample_linear(input: &[i16], from: u32, to: u32) -> Vec<i16> {
    if input.is_empty() || from == 0 || to == 0 || from == to {
        return input.to_vec();
    }

    let out_len = (input.len() as u64 * u64::from(to) / u64::from(from)) as usize;
    (0..out_len)
        .map(|index| {
            let position = index as f64 * f64::from(from) / f64::from(to);
            let base = position.floor() as usize;
            let fraction = (position - base as f64) as f32;
            let a = f32::from(input[base.min(input.len() - 1)]);
            let b = f32::from(input[(base + 1).min(input.len() - 1)]);
            (a + (b - a) * fraction) as i16
        })
        .collect()
}

/// Constant-power left and right gains for `pan`, where -1 is hard left, 1 is
/// hard right and 0 is centered.
fn pan_gains(pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    (((1.0 - pan) * 0.5).sqrt(), ((1.0 + pan) * 0.5).sqrt())
}

/// One playing buffer with its mix parameters.
#[derive(Clone, Debug)]
struct Voice {
    pcm: Vec<i16>,
    pos: usize,
    gain: f32,
    pan: f32,
    looping: bool,
}

impl Voice {
    fn new(pcm: Vec<i16>, gain: f32, pan: f32, looping: bool) -> Self {
        Self {
            pcm,
            pos: 0,
            gain,
            pan,
            looping,
        }
    }

    /// Whether a one-shot voice has played its whole buffer.
    fn finished(&self) -> bool {
        !self.looping && self.pos >= self.pcm.len()
    }
}

/// Device-independent mixer state: the voices and their mix parameters.
struct MixState {
    bgm: Option<Voice>,
    bgm_gain: f32,
    sfx: Vec<Voice>,
}

impl MixState {
    fn new() -> Self {
        Self {
            bgm: None,
            bgm_gain: 1.0,
            sfx: Vec::new(),
        }
    }

    /// Replace the BGM voice with `pcm`, keeping the current BGM volume.
    fn play_bgm(&mut self, pcm: Vec<i16>) {
        self.bgm = (!pcm.is_empty()).then(|| Voice::new(pcm, 1.0, 0.0, true));
    }

    fn stop_bgm(&mut self) {
        self.bgm = None;
    }

    fn set_bgm_volume(&mut self, gain: f32) {
        self.bgm_gain = gain.max(0.0);
    }

    /// Start a one-shot voice. When the pool is full the voice that has played
    /// the most of its buffer is stolen.
    fn play_sfx(&mut self, pcm: Vec<i16>, gain: f32, pan: f32) {
        if pcm.is_empty() {
            return;
        }
        if self.sfx.len() >= MAX_SFX_VOICES
            && let Some((index, _)) = self
                .sfx
                .iter()
                .enumerate()
                .max_by_key(|(_, voice)| voice.pos)
        {
            self.sfx.remove(index);
        }
        self.sfx.push(Voice::new(pcm, gain.max(0.0), pan, false));
    }

    fn is_silent(&self) -> bool {
        self.bgm.is_none() && self.sfx.is_empty()
    }

    /// Mix `frames` stereo frames and append them to `out`.
    fn render(&mut self, frames: usize, out: &mut Vec<u8>) {
        for _ in 0..frames {
            let mut left = 0.0f32;
            let mut right = 0.0f32;

            if let Some(bgm) = &mut self.bgm
                && bgm.pos < bgm.pcm.len()
            {
                let sample = f32::from(bgm.pcm[bgm.pos]) * self.bgm_gain;
                let (l, r) = pan_gains(bgm.pan);
                left += sample * l;
                right += sample * r;
                bgm.pos += 1;
                if bgm.looping && bgm.pos >= bgm.pcm.len() {
                    bgm.pos = 0;
                }
            }
            if self.bgm.as_ref().is_some_and(Voice::finished) {
                self.bgm = None;
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

            push_sample(out, left);
            push_sample(out, right);
        }
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
        })
    }

    /// Replace the looping BGM track and start playing it.
    pub fn play_bgm(&mut self, wav: Wav) -> Result<()> {
        self.state.stop_bgm();
        if !unsafe { SDL_ClearAudioStream(self.stream) } {
            bail!("SDL_ClearAudioStream failed");
        }
        self.state.play_bgm(to_mono(&wav));
        self.resume();
        self.update();
        Ok(())
    }

    /// Stop the BGM voice and clear anything still queued.
    pub fn stop_bgm(&mut self) {
        self.state.stop_bgm();
        let _ = unsafe { SDL_ClearAudioStream(self.stream) };
    }

    /// Set the BGM gain; negative values are clamped to zero.
    pub fn set_bgm_volume(&mut self, gain: f32) {
        self.state.set_bgm_volume(gain);
    }

    /// Start a one-shot voice with `gain` and `pan` (-1 left, 1 right).
    pub fn play_sfx(&mut self, wav: Wav, gain: f32, pan: f32) {
        self.state.play_sfx(to_mono(&wav), gain, pan);
        self.resume();
        self.update();
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

    /// Whether a BGM track is loaded and playing.
    pub fn is_playing(&self) -> bool {
        self.state.bgm.is_some()
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
}

impl Drop for Mixer {
    fn drop(&mut self) {
        unsafe { SDL_DestroyAudioStream(self.stream) };
    }
}

/// Compatibility alias for the engine's music path.
pub type MusicPlayer = Mixer;

#[cfg(test)]
mod tests {
    use super::*;

    use sdl3_sys::hints::{SDL_HINT_AUDIO_DRIVER, SDL_SetHint};

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
        assert_eq!(to_mono(&wav), vec![-32768, 0, 32512]);
    }

    #[test]
    fn mixer_averages_channels_to_mono() {
        let data = s16_bytes(&[1000, 3000, -1000, -3000]);
        let wav = parse_wav(&wav_bytes(WavFormat::S16Le, 2, 22050, &data)).unwrap();
        assert_eq!(to_mono(&wav), vec![2000, -2000]);
    }

    #[test]
    fn mixer_resamples_44100_to_22050() {
        let data = s16_bytes(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let wav = parse_wav(&wav_bytes(WavFormat::S16Le, 1, 44100, &data)).unwrap();
        assert_eq!(to_mono(&wav), vec![0, 2, 4, 6, 8]);
    }

    #[test]
    fn mixer_pan_is_constant_power() {
        let center = std::f32::consts::FRAC_1_SQRT_2;
        let (left, right) = pan_gains(0.0);
        assert!((left - center).abs() < 1e-6, "{left}");
        assert!((right - center).abs() < 1e-6, "{right}");

        assert_eq!(pan_gains(-1.0), (1.0, 0.0));
        assert_eq!(pan_gains(1.0), (0.0, 1.0));
        assert_eq!(pan_gains(-2.0), (1.0, 0.0));
        assert_eq!(pan_gains(2.0), (0.0, 1.0));
    }

    #[test]
    fn mixer_applies_gain_and_pan() {
        let mut state = MixState::new();
        state.play_bgm(vec![1000, 1000]);
        state.set_bgm_volume(0.5);

        let mut out = Vec::new();
        state.render(1, &mut out);
        let expected = (1000.0 * 0.5 * std::f32::consts::FRAC_1_SQRT_2) as i16;
        assert_eq!(out_samples(&out), vec![expected, expected]);
    }

    #[test]
    fn mixer_bgm_loops_its_whole_buffer() {
        let mut state = MixState::new();
        state.play_bgm(vec![100, -100, 50]);
        assert!(!state.is_silent());

        let mut out = Vec::new();
        state.render(3, &mut out);
        assert_eq!(out_samples(&out), vec![70, 70, -70, -70, 35, 35]);
        assert!(state.bgm.is_some());
        assert_eq!(state.bgm.as_ref().unwrap().pos, 0);

        state.stop_bgm();
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
    fn mixer_steals_the_most_played_voice() {
        let mut state = MixState::new();
        for index in 0..MAX_SFX_VOICES {
            state.play_sfx(vec![index as i16; 8], 1.0, 0.0);
        }
        assert_eq!(state.sfx.len(), MAX_SFX_VOICES);

        let mut out = Vec::new();
        state.render(3, &mut out);
        assert!(state.sfx.iter().all(|voice| voice.pos == 3));

        state.play_sfx(vec![999; 8], 1.0, 0.0);
        assert_eq!(state.sfx.len(), MAX_SFX_VOICES);
        assert_eq!(state.sfx.last().unwrap().pcm[0], 999);
        assert!(!state.sfx.iter().any(|voice| voice.pcm[0] == 15));
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
        player.play_sfx(wav, 1.0, 0.0);
        player.update();
        player.update();
    }
}
