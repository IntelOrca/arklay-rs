//! HD film decoding: H.264 through OpenH264 on a worker thread, AAC through
//! Symphonia on the caller.
//!
//! A 1280x960 H.264 frame costs roughly 20 ms to decode on one core, half of
//! a 24 fps frame's budget, so the picture is decoded on a worker that runs a
//! few frames ahead of playback through a bounded channel. The worker also
//! converts each picture to RGBA (the GPU renderer scales it to the window)
//! and box-filters it onto the 320x240 canvas the software renderer and the
//! headless capture draw. The AAC track is cheap and decoded on demand by the
//! caller, resampled to the mixer rate as a continuous stream.
//!
//! The worker decodes access units one at a time with OpenH264's automatic
//! flush disabled: flushing after every call breaks the reorder queue of a
//! stream with B-frames. Pictures come out in display order, and decoding
//! that restarts at an IDR (the prologue cut's jump) maps the IDR's decode
//! index to its display index, which a closed GOP guarantees. A unit that
//! fails (or an empty AVI drop chunk) still counts as one display frame, so
//! a damaged stream holds the previous picture for a frame instead of
//! shifting every later frame against the audio.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use symphonia_codec_aac::AacDecoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

use crate::audio::SAMPLE_RATE;
use crate::media::{self, Colour, Media, Planes, StereoResampler};

/// The software canvas width.
pub const CANVAS_WIDTH: usize = 320;
/// The software canvas height.
pub const CANVAS_HEIGHT: usize = 240;
/// Decoded frames the worker may hold ready ahead of playback.
const LOOKAHEAD: usize = 3;
/// Consecutive access units that may fail before the film is abandoned.
const MAX_FAILED_UNITS: usize = 48;
/// AAC frames per packet: a packet that fails to decode stands in as this
/// much silence.
const AAC_FRAME: usize = 1024;

/// The source of [`VideoFrame::serial`].
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

/// One decoded picture: the full-size RGBA and its 320x240 canvas.
#[derive(Debug, Default)]
pub struct VideoFrame {
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    /// The full-size picture, RGBA8.
    pub rgba: Vec<u8>,
    /// The picture letterboxed onto 320x240, RGBA8.
    pub canvas: Vec<u8>,
    /// Unique to each picture put on screen, so a renderer can skip
    /// re-uploading one it already has.
    pub serial: u64,
}

/// What the worker sends: a display-indexed frame, or a fatal error.
type WorkerMessage = std::result::Result<(usize, VideoFrame), String>;

/// One HD film in playback.
pub struct HdFilm {
    media: Arc<Media>,
    frames: Option<Receiver<WorkerMessage>>,
    spare: Sender<VideoFrame>,
    worker: Option<JoinHandle<()>>,
    /// Set on drop: the worker checks it between access units.
    stop: Arc<AtomicBool>,
    /// The frame on screen and its display index.
    current: VideoFrame,
    current_index: Option<usize>,
    /// A frame received ahead of the one requested.
    pending: Option<(usize, VideoFrame)>,
    /// The worker has sent its last frame.
    ended: bool,
    audio: Option<AudioStream>,
}

impl HdFilm {
    /// Start decoding `media`, presenting only the display frames inside
    /// `segments` (ascending, non-overlapping), and wait for the first one.
    pub fn open(media: Media, segments: Vec<Range<usize>>) -> Result<Self> {
        media.validate()?;
        let audio = match &media.audio {
            Some(track) => Some(AudioStream::new(&track.config)?),
            None => None,
        };
        let media = Arc::new(media);
        let (frame_tx, frame_rx) = mpsc::sync_channel(LOOKAHEAD);
        let (spare_tx, spare_rx) = mpsc::channel();
        let worker_media = Arc::clone(&media);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("film-h264".to_owned())
            .spawn(move || {
                let worker = Worker {
                    media: &worker_media,
                    frames: &frame_tx,
                    spare: &spare_rx,
                    stop: &worker_stop,
                };
                worker.run(&segments);
            })
            .context("failed to start the film decode thread")?;
        let mut film = HdFilm {
            media,
            frames: Some(frame_rx),
            spare: spare_tx,
            worker: Some(worker),
            stop,
            current: VideoFrame::default(),
            current_index: None,
            pending: None,
            ended: false,
            audio,
        };
        film.receive()?;
        match &film.pending {
            Some((index, _)) => film.present(*index)?,
            None => bail!("film decoded no frames"),
        }
        Ok(film)
    }

    /// The film's frame rate.
    pub fn frame_rate(&self) -> (u32, u32) {
        self.media.frame_rate
    }

    /// The number of access units, the film's frame count.
    pub fn frame_count(&self) -> usize {
        self.media.video.len()
    }

    /// Make display frame `index` (or the latest frame before it) current,
    /// waiting on the worker as needed. A stream that ends early keeps its
    /// last frame on screen.
    pub fn present(&mut self, index: usize) -> Result<()> {
        loop {
            if let Some((next, _)) = &self.pending {
                if *next > index {
                    return Ok(());
                }
                let (next, mut frame) = self.pending.take().expect("pending frame");
                frame.serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
                let old = std::mem::replace(&mut self.current, frame);
                // The worker may already have exited; the buffer is then dropped.
                let _ = self.spare.send(old);
                self.current_index = Some(next);
                if next == index {
                    return Ok(());
                }
            }
            if self.ended {
                return Ok(());
            }
            self.receive()?;
        }
    }

    /// Receive the worker's next frame into `pending`, or note its end.
    fn receive(&mut self) -> Result<()> {
        let Some(frames) = &self.frames else {
            self.ended = true;
            return Ok(());
        };
        match frames.recv() {
            Ok(Ok(frame)) => self.pending = Some(frame),
            Ok(Err(message)) => {
                self.ended = true;
                return Err(anyhow!(message));
            }
            Err(_) => self.ended = true,
        }
        Ok(())
    }

    /// The frame on screen.
    pub fn frame(&self) -> &VideoFrame {
        &self.current
    }

    /// The display index of the frame on screen.
    pub fn frame_index(&self) -> Option<usize> {
        self.current_index
    }

    /// Append the film's audio for output frames `start..end` (at
    /// [`SAMPLE_RATE`], stereo) to `out`, padding with silence past the
    /// track's end. Calls must not go backwards.
    pub fn audio(&mut self, start: u64, end: u64, out: &mut Vec<i16>) {
        match &mut self.audio {
            Some(stream) => stream.read(&self.media, start, end, out),
            None => out.resize(out.len() + (end.saturating_sub(start) as usize) * 2, 0),
        }
    }
}

impl Drop for HdFilm {
    fn drop(&mut self) {
        // The flag stops a worker between access units; closing the channel
        // fails a send it is blocked on.
        self.stop.store(true, Ordering::Relaxed);
        self.frames = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The decode thread's view of the film and its channels.
struct Worker<'a> {
    media: &'a Media,
    frames: &'a SyncSender<WorkerMessage>,
    spare: &'a Receiver<VideoFrame>,
    stop: &'a AtomicBool,
}

impl Worker<'_> {
    /// Decode the segments, reporting a fatal error to playback.
    fn run(&self, segments: &[Range<usize>]) {
        if let Err(err) = self.decode_segments(segments) {
            let _ = self.frames.send(Err(format!("{err:#}")));
        }
    }

    /// Feed access units, convert the pictures inside the segments and send
    /// them in display order. `Ok` also covers playback ending early (the
    /// film was dropped).
    fn decode_segments(&self, segments: &[Range<usize>]) -> Result<()> {
        let media = self.media;
        let colour = media.colour();
        let mut decoder: Option<Decoder> = None;
        let mut next_unit = 0usize;
        let mut out_index = 0usize;
        let mut unit = Vec::new();
        let mut warned = false;
        let mut failed_run = 0usize;
        for segment in segments {
            let start_unit = media.idr_at_or_before(segment.start);
            if decoder.is_none() || start_unit > next_unit {
                decoder = Some(new_decoder()?);
                next_unit = start_unit;
                out_index = start_unit;
            }
            let active = decoder.as_mut().expect("decoder");
            while out_index < segment.end && next_unit < media.video.len() {
                if self.stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let fed = next_unit;
                next_unit += 1;
                let decoded = match media.access_unit(fed, &mut unit) {
                    // An AVI drop chunk repeats the previous picture.
                    Ok(()) if unit.is_empty() => Ok(None),
                    Ok(()) => active.decode(&unit).map_err(|err| anyhow!(err)),
                    Err(err) => Err(err),
                };
                match decoded {
                    Ok(Some(picture)) => {
                        failed_run = 0;
                        if segment.contains(&out_index)
                            && !self.send_picture(&picture, out_index, colour)
                        {
                            return Ok(());
                        }
                        out_index += 1;
                    }
                    Ok(None) if unit.is_empty() => out_index += 1,
                    Ok(None) => failed_run = 0,
                    Err(err) => {
                        warn_once(&mut warned, fed, &err);
                        failed_run += 1;
                        if failed_run > MAX_FAILED_UNITS {
                            bail!("{failed_run} film access units in a row failed to decode");
                        }
                        // The lost picture's slot keeps the previous one.
                        out_index += 1;
                    }
                }
            }
            if out_index < segment.end {
                // Every unit is in; the reorder queue holds the tail.
                let pictures = active
                    .flush_remaining()
                    .map_err(|err| anyhow!("H.264 flush failed: {err}"))?;
                for picture in pictures {
                    if segment.contains(&out_index)
                        && !self.send_picture(&picture, out_index, colour)
                    {
                        return Ok(());
                    }
                    out_index += 1;
                }
                decoder = None;
            }
        }
        Ok(())
    }

    /// Convert and send one picture; `false` when playback has hung up.
    fn send_picture(&self, picture: &DecodedYUV<'_>, index: usize, colour: Colour) -> bool {
        let mut frame = self.spare.try_recv().unwrap_or_default();
        let (width, height) = picture.dimensions();
        let (y_stride, u_stride, v_stride) = picture.strides();
        let planes = Planes {
            width,
            height,
            y: picture.y(),
            u: picture.u(),
            v: picture.v(),
            y_stride,
            u_stride,
            v_stride,
        };
        media::yuv420_to_rgba(&planes, colour, &mut frame.rgba);
        // The canvas is built here, off the main thread, even for a frame
        // playback may skip: building it lazily would put the box filter on
        // the main thread, which the software renderer always needs.
        media::downscale_letterboxed(
            &frame.rgba,
            width,
            height,
            CANVAS_WIDTH,
            CANVAS_HEIGHT,
            &mut frame.canvas,
        );
        frame.width = width as u32;
        frame.height = height as u32;
        self.frames.send(Ok((index, frame))).is_ok()
    }
}

/// A decoder that takes one access unit per call and keeps its reorder
/// queue: OpenH264's default flush-after-decode corrupts B-frame streams.
fn new_decoder() -> Result<Decoder> {
    let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
    Decoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|err| anyhow!("failed to start the H.264 decoder: {err}"))
}

/// Report the first bad access unit of a film; later ones stay quiet.
fn warn_once(warned: &mut bool, unit: usize, err: &anyhow::Error) {
    if !*warned {
        *warned = true;
        eprintln!("warning: film access unit {unit} failed to decode: {err:#}");
    }
}

/// The AAC track decoded on demand into a continuous mixer-rate stream.
struct AudioStream {
    decoder: AacDecoder,
    next_packet: usize,
    /// Created from the first decoded packet's rate.
    resampler: Option<StereoResampler>,
    /// Interleaved output from frame `buffer_start` on.
    buffer: Vec<i16>,
    buffer_start: u64,
    scratch: Vec<f32>,
    stereo: Vec<f32>,
    /// Decoded frames still to drop as encoder priming, once the decoder's
    /// rate is known.
    priming: Option<u64>,
    warned: bool,
}

impl AudioStream {
    fn new(config: &[u8]) -> Result<Self> {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_AAC)
            .with_extra_data(config.to_vec().into_boxed_slice());
        let decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default())
            .map_err(|err| anyhow!("unsupported AAC track: {err}"))?;
        Ok(AudioStream {
            decoder,
            next_packet: 0,
            resampler: None,
            buffer: Vec::new(),
            buffer_start: 0,
            scratch: Vec::new(),
            stereo: Vec::new(),
            priming: None,
            warned: false,
        })
    }

    /// Decode the next packet onto the buffer; `false` at the track's end.
    fn decode_next(&mut self, media: &Media) -> bool {
        let Some(track) = &media.audio else {
            return false;
        };
        let Some(range) = track.packets.get(self.next_packet) else {
            return false;
        };
        self.next_packet += 1;
        let Some(data) = media.data.get(range.clone()) else {
            return true;
        };
        let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(AAC_FRAME as u64), data);
        self.stereo.clear();
        let rate = match self.decoder.decode_ref(&packet) {
            Ok(decoded) => {
                let channels = decoded.spec().channels().count().max(1);
                self.scratch.resize(decoded.frames() * channels, 0.0);
                decoded.copy_to_slice_interleaved::<f32, _>(&mut self.scratch);
                for frame in self.scratch.chunks_exact(channels) {
                    let left = frame[0];
                    let right = if channels > 1 { frame[1] } else { left };
                    self.stereo.extend_from_slice(&[left, right]);
                }
                decoded.spec().rate()
            }
            Err(err) => {
                if !self.warned {
                    self.warned = true;
                    eprintln!("warning: film audio packet failed to decode: {err}");
                }
                // Silence keeps the rest of the track in sync.
                self.stereo.resize(AAC_FRAME * 2, 0.0);
                match &self.resampler {
                    Some(resampler) => resampler.input_rate(),
                    None => track.sample_rate,
                }
            }
        };
        let (priming, timescale) = track.priming;
        let skip = self.priming.get_or_insert_with(|| {
            priming.saturating_mul(u64::from(rate)) / u64::from(timescale.max(1))
        });
        let dropped = (*skip).min((self.stereo.len() / 2) as u64);
        *skip -= dropped;
        let resampler = self
            .resampler
            .get_or_insert_with(|| StereoResampler::new(rate, SAMPLE_RATE));
        resampler.push(&self.stereo[(dropped * 2) as usize..], &mut self.buffer);
        true
    }

    /// The output frame just past the buffer.
    fn buffered_end(&self) -> u64 {
        self.buffer_start + (self.buffer.len() / 2) as u64
    }

    /// Append output frames `start..end` to `out`.
    fn read(&mut self, media: &Media, start: u64, end: u64, out: &mut Vec<i16>) {
        let end = end.max(start);
        while self.buffered_end() < end && self.decode_next(media) {}
        let buffered_end = self.buffered_end();
        // Drop what lies before the request (a prologue cut's skipped beat).
        let skip = start.clamp(self.buffer_start, buffered_end) - self.buffer_start;
        self.buffer.drain(..(skip * 2) as usize);
        self.buffer_start += skip;
        // A request before the buffer (never made in order) reads silence.
        let gap = self.buffer_start.saturating_sub(start).min(end - start);
        out.resize(out.len() + (gap * 2) as usize, 0);
        let take = (end - start - gap).min((self.buffer.len() / 2) as u64);
        out.extend(self.buffer.drain(..(take * 2) as usize));
        self.buffer_start += take;
        let missing = end - start - gap - take;
        out.resize(out.len() + (missing * 2) as usize, 0);
    }
}
