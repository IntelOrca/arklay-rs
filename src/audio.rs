//! WAV parsing and looping music playback through SDL3.
//!
//! [`MusicPlayer`] owns the only SDL audio stream in the engine. It opens a
//! default playback device, feeds a whole WAV `data` chunk to SDL, and
//! re-queues that buffer whenever the stream runs low, matching the original
//! whole-buffer loop. Every `sdl3_sys::audio` call is confined to this module.

use std::ffi::{CStr, c_int};
use std::ptr;

use anyhow::{Context, Result, bail};

use sdl3_sys::audio::{
    SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, SDL_AUDIO_S16LE, SDL_AUDIO_U8, SDL_AudioSpec,
    SDL_AudioStream, SDL_ClearAudioStream, SDL_DestroyAudioStream, SDL_GetAudioStreamQueued,
    SDL_OpenAudioDeviceStream, SDL_PutAudioStreamData, SDL_ResumeAudioStreamDevice,
    SDL_SetAudioStreamFormat, SDL_SetAudioStreamGain,
};
use sdl3_sys::error::SDL_GetError;
use sdl3_sys::init::{SDL_INIT_AUDIO, SDL_InitSubSystem};

/// PCM sample format of a parsed WAV file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WavFormat {
    /// Unsigned 8-bit samples.
    U8,
    /// Signed 16-bit little-endian samples.
    S16Le,
}

/// Parsed WAV file: source format plus the raw `data` chunk bytes.
#[derive(Debug, PartialEq, Eq)]
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

/// A looping music player backed by a single SDL audio stream.
pub struct MusicPlayer {
    stream: *mut SDL_AudioStream,
    pcm: Vec<u8>,
    gain: f32,
    playing: bool,
}

impl MusicPlayer {
    /// Initialize SDL audio and open a default playback device.
    ///
    /// Returns `None` when the audio subsystem or a default playback device
    /// cannot be opened; audio is optional and a failure is never fatal. The
    /// device opens paused and [`MusicPlayer::play`] resumes it once a buffer
    /// has been queued.
    pub fn open() -> Option<MusicPlayer> {
        if !unsafe { SDL_InitSubSystem(SDL_INIT_AUDIO) } {
            return None;
        }
        let spec = SDL_AudioSpec {
            format: SDL_AUDIO_U8,
            channels: 1,
            freq: 22050,
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
        Some(MusicPlayer {
            stream,
            pcm: Vec::new(),
            gain: 1.0,
            playing: false,
        })
    }

    /// Replace the current track with `wav` and start or restart playback.
    ///
    /// The stream's source format is set from the WAV header, so SDL does any
    /// conversion to the device format; the bytes are never converted here.
    pub fn play(&mut self, wav: Wav) -> Result<()> {
        let spec = SDL_AudioSpec {
            format: match wav.format {
                WavFormat::U8 => SDL_AUDIO_U8,
                WavFormat::S16Le => SDL_AUDIO_S16LE,
            },
            channels: c_int::from(wav.channels),
            freq: c_int::try_from(wav.sample_rate)
                .context("WAV sample rate does not fit in an SDL audio spec")?,
        };
        if !unsafe { SDL_SetAudioStreamFormat(self.stream, &spec, ptr::null()) } {
            bail!("SDL_SetAudioStreamFormat failed: {}", sdl_error());
        }
        if !unsafe { SDL_ClearAudioStream(self.stream) } {
            bail!("SDL_ClearAudioStream failed: {}", sdl_error());
        }
        self.pcm = wav.data;
        self.playing = true;
        self.push_buffer()?;
        if !unsafe { SDL_ResumeAudioStreamDevice(self.stream) } {
            bail!("SDL_ResumeAudioStreamDevice failed: {}", sdl_error());
        }
        self.set_volume(self.gain);
        Ok(())
    }

    /// Keep the loop fed; call once per frame.
    ///
    /// When less than one buffer is queued, the whole buffer is queued again,
    /// so playback loops seamlessly and the queue oscillates between roughly
    /// one and two buffers. No-op when nothing is playing.
    pub fn update(&mut self) {
        if !self.playing || self.pcm.is_empty() {
            return;
        }
        let Ok(len) = c_int::try_from(self.pcm.len()) else {
            return;
        };
        let queued = unsafe { SDL_GetAudioStreamQueued(self.stream) };
        if queued >= 0 && queued < len {
            let _ = self.push_buffer();
        }
    }

    /// Clear the stream and mark the player as stopped.
    pub fn stop(&mut self) {
        let _ = unsafe { SDL_ClearAudioStream(self.stream) };
        self.pcm.clear();
        self.playing = false;
    }

    /// Set the stream gain; negative values are clamped to zero.
    pub fn set_volume(&mut self, gain: f32) {
        let gain = gain.max(0.0);
        self.gain = gain;
        let _ = unsafe { SDL_SetAudioStreamGain(self.stream, gain) };
    }

    /// Whether a track is currently loaded and playing.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Queue one copy of the stored buffer.
    fn push_buffer(&mut self) -> Result<()> {
        let len = c_int::try_from(self.pcm.len()).context("music buffer is too large for SDL")?;
        if !unsafe { SDL_PutAudioStreamData(self.stream, self.pcm.as_ptr().cast(), len) } {
            bail!("SDL_PutAudioStreamData failed: {}", sdl_error());
        }
        Ok(())
    }
}

impl Drop for MusicPlayer {
    fn drop(&mut self) {
        unsafe { SDL_DestroyAudioStream(self.stream) };
    }
}

/// Copy SDL's current error string.
fn sdl_error() -> String {
    unsafe { CStr::from_ptr(SDL_GetError()) }
        .to_string_lossy()
        .into_owned()
}

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
    #[ignore = "requires an audio device or SDL dummy driver"]
    fn dummy_driver_plays_loops_and_stops() {
        let _ = unsafe { SDL_SetHint(SDL_HINT_AUDIO_DRIVER, c"dummy".as_ptr()) };
        let mut player = MusicPlayer::open().expect("dummy audio device should open");

        let data: Vec<u8> = (0..2205).map(|i| (i % 251) as u8).collect();
        let wav = parse_wav(&wav_bytes(WavFormat::U8, 1, 22050, &data)).unwrap();
        player.play(wav).unwrap();
        assert!(player.is_playing());
        assert!(unsafe { SDL_GetAudioStreamQueued(player.stream) } > 0);

        player.update();
        player.update();
        player.set_volume(0.5);
        player.stop();
        assert!(!player.is_playing());
    }
}
