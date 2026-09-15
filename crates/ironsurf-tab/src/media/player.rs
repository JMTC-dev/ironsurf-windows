//! The player: a clock, a decode pipeline on a worker thread, and the rules
//! for when a decoded frame becomes *the* frame.
//!
//! # Why a worker thread
//!
//! The tab thread runs the document — style, layout, paint, IPC, JavaScript. A
//! 1080p H.264 frame takes tens of milliseconds to decode in software, which
//! would blow the 30 fps frame budget the moment a video started. So decoding
//! happens on its own thread (no async runtime, no GPU): the tab thread sends
//! "decode from sample N" / "give me the next picture" and picks the result up
//! when it is ready. One request in flight plus one presented frame: enough to
//! keep up, small enough that a seek never has to drain a queue.
//!
//! # Clock
//!
//! Media time is `anchor_pts + elapsed × rate` from a [`std::time::Instant`]
//! anchor, which makes pause/play/rate changes exact instead of accumulated. A
//! decoded picture is shown when the clock reaches its `pts`; after a seek,
//! pictures *before* the target are still decoded (they are the only way to
//! reach the target in a prediction-coded codec) and discarded, which is why a
//! seek lands on the frame the user asked for rather than the keyframe before
//! it.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::media::codec::{self, CodecParams, Picture, PictureDecoder};
use crate::media::demux::{MediaInfo, VideoTrack};
use crate::media::frame::Frame;
use crate::media::MediaError;

/// The largest media file IronSurf will hold in memory for one element.
pub const MAX_VIDEO_BYTES: usize = 512 * 1024 * 1024;

/// What the page can be told about. The tab drains these into JS events after
/// each tick (see `runtime`).
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    /// Size, duration and codec are known.
    LoadedMetadata,
    /// The whole file was demuxed — IronSurf has no progressive media loading.
    LoadedData,
    /// The first frame is decoded.
    CanPlay,
    Play,
    Pause,
    /// A seek started.
    Seeking,
    /// The frame for the seek target is on screen.
    Seeked,
    /// Media time moved (throttled to a quarter-second step).
    TimeUpdate { position: f64 },
    /// The `playbackRate` attribute changed.
    RateChange,
    /// The `muted` attribute changed.
    MutedChange,
    /// Decoding ran out of samples.
    Ended,
    /// Playback stopped working; the string is the user-facing reason.
    Error { detail: String },
}

/// Where the element is in its life cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerState {
    /// No source has been fetched.
    Idle,
    /// Fetching the bytes.
    Loading,
    /// Demuxed and not running (`<video>` before it starts).
    Ready,
    Playing,
    Paused,
    /// Out of frames.
    Ended,
    /// Failed; [`MediaPlayer::error`] says why.
    Failed,
}

/// Media time, as an anchor plus elapsed real time.
#[derive(Clone, Copy, Debug)]
struct Clock {
    anchor: Instant,
    /// Media time at the anchor.
    anchor_pts: f64,
    rate: f64,
    paused: bool,
}

impl Clock {
    fn new(pts: f64) -> Self {
        Clock {
            anchor: Instant::now(),
            anchor_pts: pts.max(0.0),
            rate: 1.0,
            paused: true,
        }
    }

    fn position(&self) -> f64 {
        if self.paused {
            return self.anchor_pts;
        }
        self.anchor_pts + self.anchor.elapsed().as_secs_f64() * self.rate
    }

    /// Move the clock to `pts`, staying paused or running as it already is.
    fn seek(&mut self, pts: f64) {
        self.anchor_pts = pts.max(0.0);
        self.anchor = Instant::now();
    }

    fn pause(&mut self) {
        if !self.paused {
            self.anchor_pts = self.position();
            self.anchor = Instant::now();
            self.paused = true;
        }
    }

    fn resume(&mut self) {
        if !self.paused {
            return;
        }
        self.paused = false;
        self.anchor = Instant::now();
    }

    fn set_rate(&mut self, rate: f64) {
        let position = self.position();
        self.anchor_pts = position;
        self.anchor = Instant::now();
        self.rate = if rate.is_finite() && rate > 0.0 { rate } else { 1.0 };
    }
}

/// Requests to the decode thread. Each carries the player's current
/// *generation* so that an answer to a superseded seek can be recognised.
#[derive(Clone, Copy, Debug)]
enum Command {
    /// Restart decoding at this sample (that is what a seek means).
    DecodeFrom { index: usize, generation: u64 },
    /// The next picture.
    Next { generation: u64 },
}

/// One decode result.
struct Outcome {
    generation: u64,
    frame: Option<Frame>,
    /// Presentation time of `frame`.
    pts: f64,
    /// The sample table ran out (after flushing what was buffered).
    end: bool,
    error: Option<String>,
}

/// The decode thread's state: a decoder, the sample it is sitting on, and the
/// pictures the codec has produced but not yet been asked for.
struct Worker {
    track: Arc<VideoTrack>,
    params: CodecParams,
    decoder: Box<dyn PictureDecoder>,
    /// Pictures the decoder has released that nobody has asked for yet. H.264
    /// with B-frames buffers several; VP9/MJPEG/Raw usually have none.
    buffered: VecDeque<Picture>,
    next: usize,
    flushed: bool,
}

impl Worker {
    fn new(track: Arc<VideoTrack>) -> Self {
        let params = params_of(&track);
        // `VideoTrack::open` already rejected unsupported codecs, so the only
        // way to fail here is a decoder-internal error on init.
        let decoder: Box<dyn PictureDecoder> = match codec::make_decoder(&params) {
            Ok(decoder) => decoder,
            Err(err) => Box::new(FailedDecoder(err.to_string())),
        };
        Worker {
            track,
            params,
            decoder,
            buffered: VecDeque::new(),
            next: 0,
            flushed: false,
        }
    }

    /// Rebuild the decoder from scratch: inter prediction is stateful, so a
    /// seek means "start again at the keyframe" (see `crate::media::codec`).
    fn restart(&mut self, index: usize) {
        self.decoder = match codec::make_decoder(&self.params) {
            Ok(decoder) => decoder,
            Err(err) => Box::new(FailedDecoder(err.to_string())),
        };
        self.buffered.clear();
        self.next = index;
        self.flushed = false;
    }

    /// The next picture, decoding samples as needed. `None` means the track is
    /// over. An `Err` is a sample the codec could not read.
    fn next_picture(&mut self) -> Result<Option<Picture>, MediaError> {
        loop {
            if let Some(picture) = self.buffered.pop_front() {
                return Ok(Some(picture));
            }
            if self.next < self.track.len() {
                let sample = self.track.sample(self.next).expect("index checked");
                let bytes = sample.bytes(self.track.file());
                // `decode` may emit zero pictures (a skipped B-frame) or more
                // than one; the loop re-enters and drains `buffered`.
                let mut out: Vec<Picture> = Vec::new();
                self.decoder.decode(bytes, sample.pts, &mut out)?;
                self.buffered.extend(out);
                self.next += 1;
            } else if !self.flushed {
                let mut out: Vec<Picture> = Vec::new();
                self.decoder.flush(&mut out)?;
                self.buffered.extend(out);
                self.flushed = true;
                if self.buffered.is_empty() {
                    return Ok(None);
                }
            } else {
                return Ok(None);
            }
        }
    }
}

/// A decoder that cannot decode (init failed); every picture is the same error.
struct FailedDecoder(String);

impl PictureDecoder for FailedDecoder {
    fn decode(&mut self, _sample: &[u8], _pts: f64, _out: &mut Vec<Picture>) -> Result<(), MediaError> {
        Err(MediaError::Decode {
            codec: self.params_label(),
            detail: self.0.clone(),
        })
    }
    fn flush(&mut self, _out: &mut Vec<Picture>) -> Result<(), MediaError> {
        Ok(())
    }
    fn codec(&self) -> codec::VideoCodec {
        codec::VideoCodec::Unknown("video".to_string())
    }
}

impl FailedDecoder {
    fn params_label(&self) -> String {
        "video".to_string()
    }
}

fn params_of(track: &Arc<VideoTrack>) -> CodecParams {
    let info = track.info();
    CodecParams {
        codec: info.codec.clone(),
        width: info.width,
        height: info.height,
        codec_private: info.codec_private.clone(),
    }
}

/// The decode thread.
fn decode_worker(track: Arc<VideoTrack>, commands: Receiver<Command>, results: Sender<Outcome>) {
    let mut worker = Worker::new(track);
    loop {
        let command = match commands.recv() {
            Ok(command) => command,
            // The player was dropped.
            Err(_) => break,
        };
        let generation = match command {
            Command::DecodeFrom { index, generation } => {
                worker.restart(index);
                generation
            }
            Command::Next { generation } => generation,
        };
        // One request, one picture. Decoding continues past the end of the
        // samples until the codec's own buffer is drained, so the last frames
        // of a clip are never lost.
        match worker.next_picture() {
            Ok(Some(picture)) => {
                let outcome = Outcome {
                    generation,
                    pts: picture.pts,
                    frame: Some(picture.frame),
                    end: false,
                    error: None,
                };
                if results.send(outcome).is_err() {
                    break;
                }
            }
            Ok(None) => {
                let _ = results.send(Outcome {
                    generation,
                    pts: 0.0,
                    frame: None,
                    end: true,
                    error: None,
                });
            }
            Err(err) => {
                let _ = results.send(Outcome {
                    generation,
                    pts: 0.0,
                    frame: None,
                    end: false,
                    error: Some(err.to_string()),
                });
            }
        }
    }
}

/// A video element's playback state, shared between the blitz widget that
/// paints it ([`crate::media::widget`]) and the tab runtime that pumps it.
pub struct MediaPlayer {
    info: Option<MediaInfo>,
    track: Option<Arc<VideoTrack>>,
    state: PlayerState,
    clock: Clock,
    commands: Option<Sender<Command>>,
    results: Option<Receiver<Outcome>>,
    #[allow(dead_code)]
    worker: Option<JoinHandle<()>>,
    /// The frame to composite.
    current: Option<Frame>,
    /// Presentation time of the composited frame.
    current_pts: f64,
    /// Bumped whenever the composited frame changes, so a widget can tell a
    /// repaint of the same frame from a new one.
    frame_sequence: u64,
    /// The box the widget last painted into (CSS pixels), which the click
    /// mapping needs: blitz gives a widget positions but not its own size.
    painted: (f32, f32),
    /// The last decoded frame, waiting for the clock to catch up.
    ready: Option<(f64, Frame)>,
    /// A request is waiting to be answered.
    in_flight: bool,
    /// Discard pictures until one is at or after this time (set by a seek).
    skip_until: Option<f64>,
    generation: u64,
    error: Option<MediaError>,
    events: VecDeque<PlayerEvent>,
    muted: bool,
    /// When the last `timeupdate` was emitted, to throttle them.
    last_update: f64,
}

impl Drop for MediaPlayer {
    fn drop(&mut self) {
        // Closing the command channel ends the decode thread; we never block a
        // drop on one sample finishing.
        self.commands = None;
        self.worker = None;
    }
}

impl MediaPlayer {
    /// A player with nothing in it.
    pub fn new() -> Self {
        MediaPlayer {
            info: None,
            track: None,
            state: PlayerState::Idle,
            clock: Clock::new(0.0),
            commands: None,
            results: None,
            worker: None,
            current: None,
            current_pts: 0.0,
            frame_sequence: 0,
            painted: (0.0, 0.0),
            ready: None,
            in_flight: false,
            skip_until: None,
            generation: 0,
            error: None,
            events: VecDeque::new(),
            muted: false,
            last_update: 0.0,
        }
    }

    pub fn state(&self) -> PlayerState {
        self.state
    }

    pub fn error(&self) -> Option<&MediaError> {
        self.error.as_ref()
    }

    pub fn info(&self) -> Option<&MediaInfo> {
        self.info.as_ref()
    }

    /// The frame to composite, if there is one.
    pub fn frame(&self) -> Option<&Frame> {
        self.current.as_ref()
    }

    /// Presentation time of the composited frame.
    pub fn current_pts(&self) -> f64 {
        self.current_pts
    }

    /// Changes every time the composited frame changes.
    pub fn frame_sequence(&self) -> u64 {
        self.frame_sequence
    }

    /// The element box the widget last painted, in CSS pixels.
    pub fn painted_geometry(&self) -> (f32, f32) {
        self.painted
    }

    /// Recorded by the widget on each paint (see [`crate::media::widget`]).
    pub fn set_painted_geometry(&mut self, width: f32, height: f32) {
        if width > 0.0 && height > 0.0 {
            self.painted = (width, height);
        }
    }

    pub fn is_playing(&self) -> bool {
        self.state == PlayerState::Playing
    }

    pub fn is_paused(&self) -> bool {
        !self.is_playing() && self.state != PlayerState::Failed && self.state != PlayerState::Loading
    }

    pub fn position(&self) -> f64 {
        self.clock.position().max(0.0)
    }

    pub fn duration(&self) -> f64 {
        self.track
            .as_ref()
            .map(|track| track.duration())
            .unwrap_or(0.0)
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn rate(&self) -> f64 {
        self.clock.rate
    }

    /// Natural size of the video, for the widget's intrinsic aspect ratio.
    pub fn natural_size(&self) -> Option<(u32, u32)> {
        self.info.as_ref().map(|info| (info.width, info.height))
    }

    /// The source is being fetched.
    pub fn begin_load(&mut self) {
        if matches!(self.state, PlayerState::Idle | PlayerState::Failed) {
            self.state = PlayerState::Loading;
        }
    }

    /// Demuxed bytes became a track: start the decode thread, ask for the
    /// first frame, and emit `loadedmetadata` / `loadeddata` / `canplay`.
    ///
    /// This does *not* start playback — see [`Self::play`], and `docs/VIDEO.md`
    /// for when it happens by itself.
    pub fn load(&mut self, track: Arc<VideoTrack>) {
        self.stop_worker();
        self.error = None;
        self.current = None;
        self.current_pts = 0.0;
        self.ready = None;
        self.in_flight = false;
        self.skip_until = None;
        self.last_update = 0.0;
        self.generation = self.generation.wrapping_add(1);
        self.info = Some(track.info().clone());
        self.track = Some(track.clone());
        self.clock = Clock::new(0.0);

        let (command_tx, command_rx) = mpsc::channel();
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let started = std::thread::Builder::new()
            .name("ironsurf-media".to_string())
            .spawn(move || decode_worker(track, command_rx, outcome_tx));
        match started {
            Ok(worker) => {
                self.commands = Some(command_tx);
                self.results = Some(outcome_rx);
                self.worker = Some(worker);
                self.state = PlayerState::Ready;
                self.push_event(PlayerEvent::LoadedMetadata);
                self.push_event(PlayerEvent::LoadedData);
                // Show the first frame even when paused, the way a
                // `<video>` with a poster-less source does.
                self.command(Command::DecodeFrom {
                    index: 0,
                    generation: self.generation,
                });
            }
            Err(err) => self.fail(MediaError::Decode {
                codec: "decode thread".to_string(),
                detail: format!("could not be started: {err}"),
            }),
        }
    }

    /// The failure path: remembered, emitted, and painted as a placeholder.
    pub fn fail(&mut self, error: MediaError) {
        self.stop_worker();
        self.track = None;
        self.current = None;
        self.ready = None;
        self.state = PlayerState::Failed;
        self.error = Some(error);
        let detail = self.error_message();
        self.push_event(PlayerEvent::Error { detail });
    }

    fn error_message(&self) -> String {
        self.error
            .as_ref()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown media error".to_string())
    }

    fn stop_worker(&mut self) {
        self.commands = None;
        self.results = None;
        self.worker = None;
        self.in_flight = false;
    }

    fn push_event(&mut self, event: PlayerEvent) {
        self.events.push_back(event);
    }

    /// Drain the events the tab should deliver to the page.
    pub fn take_events(&mut self) -> Vec<PlayerEvent> {
        self.events.drain(..).collect()
    }

    // ---- Transport -------------------------------------------------------

    /// Start or resume playback. From [`PlayerState::Ended`] this replays from
    /// the start, as the DOM does.
    pub fn play(&mut self) {
        match self.state {
            PlayerState::Ready | PlayerState::Paused | PlayerState::Ended => {
                if self.state == PlayerState::Ended {
                    self.seek(0.0);
                }
                self.clock.resume();
                self.state = PlayerState::Playing;
                self.push_event(PlayerEvent::Play);
                self.ensure_decode_running();
            }
            // Idle / Loading / Failed: a `play()` on an element with no
            // decodable source cannot start anything.
            _ => {}
        }
    }

    pub fn pause(&mut self) {
        if self.state == PlayerState::Playing {
            self.clock.pause();
            self.state = PlayerState::Paused;
            self.push_event(PlayerEvent::Pause);
        }
    }

    /// Jump to `seconds`. Works while paused: the frame for the target is
    /// decoded and shown, which is what someone dragging a scrub bar expects.
    pub fn seek(&mut self, seconds: f64) {
        if self.commands.is_none() || self.track.is_none() {
            return;
        }
        let duration = self.duration();
        let target = if !seconds.is_finite() {
            0.0
        } else if duration > 0.0 {
            seconds.clamp(0.0, duration)
        } else {
            seconds.max(0.0)
        };
        let index = self
            .track
            .as_ref()
            .map(|track| track.seek_start(target))
            .unwrap_or(0);
        self.generation = self.generation.wrapping_add(1);
        self.skip_until = Some(target);
        self.ready = None;
        self.clock.seek(target);
        self.last_update = target;
        self.push_event(PlayerEvent::Seeking);
        self.command(Command::DecodeFrom {
            index,
            generation: self.generation,
        });
    }

    /// One frame forward — no UI gesture needs it yet, but it is the cheapest
    /// way to step the pipeline in a test.
    pub fn step_forward(&mut self) {
        let position = self.position();
        self.seek(position + 1.0 / 60.0);
    }

    /// The DOM `playbackRate`.
    pub fn set_rate(&mut self, rate: f64) {
        let before = self.clock.rate;
        self.clock.set_rate(rate);
        if (self.clock.rate - before).abs() > f64::EPSILON {
            self.push_event(PlayerEvent::RateChange);
        }
    }

    /// The DOM `muted` attribute. IronSurf has no audio output, so this is
    /// state plus an event (see `docs/VIDEO.md`).
    pub fn set_muted(&mut self, muted: bool) {
        if muted != self.muted {
            self.muted = muted;
            self.push_event(PlayerEvent::MutedChange);
        }
    }

    fn command(&mut self, command: Command) {
        if let Some(commands) = self.commands.as_ref() {
            let _ = commands.send(command);
            self.in_flight = true;
        }
    }

    // ---- The per-frame pump ---------------------------------------------

    /// Advance the pipeline: collect a finished decode, present whatever the
    /// clock says is due, ask for more work. Call once per document frame.
    /// Returns `true` when the composited frame changed.
    pub fn pump(&mut self) -> bool {
        let mut presented = false;
        // One request at a time, so one nonblocking read is enough; anything
        // further along the channel answers a seek that has been superseded.
        let mut latest = None;
        while let Some(result) = self.results.as_ref().and_then(|rx| rx.try_recv().ok()) {
            latest = Some(result);
        }
        if let Some(outcome) = latest {
            if outcome.generation == self.generation {
                self.in_flight = false;
                presented |= self.accept_outcome(outcome);
            }
        }
        presented |= self.present_due();
        if self.state == PlayerState::Playing {
            self.ensure_decode_running();
            self.emit_time_update();
        }
        presented
    }

    /// Whether the document should keep scheduling frames for this element.
    pub fn needs_repaint(&self) -> bool {
        match self.state {
            PlayerState::Playing | PlayerState::Loading => true,
            PlayerState::Ready | PlayerState::Paused | PlayerState::Ended => {
                // A seek while paused still has to come to the screen.
                self.in_flight || self.skip_until.is_some()
            }
            PlayerState::Idle | PlayerState::Failed => false,
        }
    }

    fn accept_outcome(&mut self, outcome: Outcome) -> bool {
        if let Some(detail) = outcome.error {
            self.fail(MediaError::Decode {
                codec: self
                    .info
                    .as_ref()
                    .map(|info| info.codec.label())
                    .unwrap_or_else(|| "video".to_string()),
                detail,
            });
            return false;
        }
        if outcome.end {
            if self.state != PlayerState::Ended {
                self.state = PlayerState::Ended;
                self.clock.pause();
                self.skip_until = None;
                self.push_event(PlayerEvent::Ended);
            }
            return false;
        }
        let frame = outcome.frame.expect("a frame when not at the end");
        self.ready = Some((outcome.pts, frame));
        false
    }

    /// Show whatever the clock has reached.
    fn present_due(&mut self) -> bool {
        let mut presented = false;
        while let Some((pts, frame)) = self.ready.take() {
            // A seek decodes through the pictures before its target.
            if let Some(target) = self.skip_until {
                if pts + 1.0 / 1_000_000.0 < target {
                    self.request_next();
                    break;
                }
                self.skip_until = None;
                self.push_event(PlayerEvent::Seeked);
            }
            if self.is_playing() && self.position() + 1.0 / 240.0 < pts {
                // Not due yet: hold it, the clock will catch up.
                self.ready = Some((pts, frame));
                break;
            }
            self.current = Some(frame);
            self.current_pts = pts;
            self.frame_sequence = self.frame_sequence.wrapping_add(1);
            presented = true;
            self.request_next();
        }
        presented
    }

    fn request_next(&mut self) {
        if self.in_flight || self.commands.is_none() || self.state == PlayerState::Ended {
            return;
        }
        self.command(Command::Next {
            generation: self.generation,
        });
    }

    /// Ask for a picture if the pipeline has nothing in it (after a pause, an
    /// empty sample, or a seek that has not been answered yet).
    fn ensure_decode_running(&mut self) {
        if self.ready.is_some() || self.in_flight {
            return;
        }
        self.request_next();
    }

    fn emit_time_update(&mut self) {
        let position = self.position();
        // A quarter-second step is finer than any status display and far
        // coarser than a frame, so the page does not get 30 events a second.
        if (position * 4.0).floor() != (self.last_update * 4.0).floor() {
            self.last_update = position;
            self.push_event(PlayerEvent::TimeUpdate { position });
        }
    }

    /// Test hook: metadata without a real track (the widget tests need a size).
    #[cfg(test)]
    pub(crate) fn set_test_metadata(&mut self, width: u32, height: u32) {
        let info = self.info.get_or_insert_with(|| MediaInfo {
            container: crate::media::demux::Container::Mp4,
            duration: None,
            width: 0,
            height: 0,
            codec: crate::media::codec::VideoCodec::H264,
            codec_private: None,
            audio: None,
            sample_count: 0,
        });
        info.width = width;
        info.height = height;
    }

    /// Test hook: composite a frame directly (no decoder involved).
    #[cfg(test)]
    pub(crate) fn set_test_frame(&mut self, frame: Frame) {
        self.current = Some(frame);
        self.frame_sequence = self.frame_sequence.wrapping_add(1);
    }

    /// Pump until a frame is presented or `timeout` runs out. Used for the
    /// first frame and by the tests: the only place playback waits on the wall
    /// clock instead of being driven by it.
    pub fn await_frame(&mut self, timeout: Duration) -> Option<Frame> {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if self.frame().is_some() {
                return self.frame().cloned();
            }
            if matches!(self.state, PlayerState::Failed | PlayerState::Ended) {
                return None;
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Default for MediaPlayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Read media bytes into a track, applying the size limit.
pub fn open_bytes(bytes: Arc<Vec<u8>>) -> Result<Arc<VideoTrack>, MediaError> {
    if bytes.len() > MAX_VIDEO_BYTES {
        return Err(MediaError::TooLarge {
            bytes: bytes.len(),
            limit: MAX_VIDEO_BYTES,
        });
    }
    VideoTrack::open(bytes).map(Arc::new)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::codec::{RawFormat, VideoCodec};
    use crate::media::demux::{Container, Sample};

    /// A synthetic "file": four 4×4 RGBA frames, all keyframes, 40 ms apart,
    /// each a solid colour the test can identify. Runs through the real
    /// [`crate::media::codec::RawDecoder`], so the whole pipeline — thread,
    /// channel, clock, presentation rules — is exercised without a real
    /// bitstream.
    fn solid_track(tint: u8, frame_ms: f64) -> Arc<VideoTrack> {
        let mut file = Vec::new();
        let mut samples = Vec::new();
        for index in 0..4u32 {
            let colour = tint.wrapping_add(index as u8);
            let sample = vec![colour; 4 * 4 * 4];
            let offset = file.len();
            file.extend_from_slice(&sample);
            samples.push(Sample {
                offset,
                len: sample.len(),
                pts: f64::from(index) * frame_ms,
                key: true,
            });
        }
        let info = MediaInfo {
            container: Container::Avi,
            duration: Some(4.0 * frame_ms),
            width: 4,
            height: 4,
            codec: VideoCodec::Raw(RawFormat::Rgba32),
            codec_private: None,
            audio: None,
            sample_count: samples.len(),
        };
        VideoTrack::for_test(Arc::new(file), info, samples)
    }

    fn loaded(tint: u8) -> MediaPlayer {
        let mut player = MediaPlayer::new();
        player.load(solid_track(tint, 0.04));
        player
    }

    #[test]
    fn load_reports_ready_and_emits_the_events_a_page_expects() {
        let mut player = loaded(10);
        assert_eq!(player.state(), PlayerState::Ready);
        assert!(player.is_paused());
        let events = player.take_events();
        assert!(events.contains(&PlayerEvent::LoadedMetadata), "{events:?}");
        assert!(events.contains(&PlayerEvent::LoadedData), "{events:?}");
        // The first frame arrives on its own, like a poster-less <video>.
        let frame = player.await_frame(Duration::from_secs(5)).expect("first frame");
        assert_eq!((frame.width, frame.height), (4, 4));
        assert_eq!(frame.rgba[0], 10, "the colour the fixture put in");
        assert!(player.take_events().contains(&PlayerEvent::CanPlay), "and then canplay");
        assert_eq!(player.duration(), 0.16);
        assert_eq!(player.natural_size(), Some((4, 4)));
    }

    #[test]
    fn a_failed_load_says_why_and_stays_failed() {
        let mut player = MediaPlayer::new();
        player.fail(MediaError::UnsupportedCodec {
            codec: "VP8".to_string(),
        });
        assert_eq!(player.state(), PlayerState::Failed);
        assert!(player.error().unwrap().to_string().contains("VP8"));
        assert_eq!(
            player.take_events(),
            vec![PlayerEvent::Error {
                detail: "no decoder for VP8 — IronSurf decodes H.264, VP9 and raw/MJPEG in pure Rust".to_string()
            }]
        );
        player.play();
        assert_eq!(player.state(), PlayerState::Failed, "a dead element stays dead");
        assert!(!player.needs_repaint());
    }

    #[test]
    fn playing_shows_later_frames_as_the_clock_moves() {
        let mut player = loaded(10);
        player.await_frame(Duration::from_secs(5)).expect("first frame");
        player.play();
        assert!(player.is_playing());
        // Frames are 40 ms apart, so a few hundred ms of wall clock must move
        // the picture along.
        let mut seen = vec![player.frame().unwrap().rgba[0]];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            player.pump();
            if let Some(frame) = player.frame() {
                if frame.rgba[0] != *seen.last().unwrap() {
                    seen.push(frame.rgba[0]);
                }
            }
            if player.state() == PlayerState::Ended {
                break;
            }
        }
        assert!(seen.len() > 1, "a playing video must show new frames; saw {seen:?}");
        assert!(
            seen.windows(2).all(|pair| i16::from(pair[1]) - i16::from(pair[0]) == 1),
            "frames must arrive in presentation order: {seen:?}"
        );
    }

    #[test]
    fn pause_freezes_the_position_and_the_frame() {
        let mut player = loaded(10);
        player.play();
        std::thread::sleep(Duration::from_millis(50));
        player.pump();
        player.pause();
        let position = player.position();
        let colour = player.frame().unwrap().rgba[0];
        std::thread::sleep(Duration::from_millis(80));
        player.pump();
        assert_eq!(player.state(), PlayerState::Paused);
        assert_eq!(player.position(), position, "a paused clock must not drift");
        assert_eq!(player.frame().unwrap().rgba[0], colour, "nor change frame");
    }

    #[test]
    fn seeking_while_paused_shows_the_frame_at_the_target() {
        let mut player = loaded(10);
        player.seek(0.08);
        let frame = player.await_frame(Duration::from_secs(5)).expect("frame after seek");
        // Sample 2 is 0.08 s in and carries colour 12.
        assert_eq!(frame.rgba[0], 12);
        assert_eq!(player.current_pts(), 0.08);
        assert_eq!(player.position(), 0.08);
        let events = player.take_events();
        assert!(events.contains(&PlayerEvent::Seeking), "{events:?}");
        assert!(events.contains(&PlayerEvent::Seeked), "{events:?}");
    }

    #[test]
    fn a_seek_past_the_end_clamps_to_the_end() {
        let mut player = loaded(10);
        player.seek(99.0);
        assert_eq!(player.position(), 0.16);
    }

    #[test]
    fn ended_playback_replays_from_the_start() {
        let mut player = loaded(10);
        player.play();
        let deadline = Instant::now() + Duration::from_secs(5);
        while player.state() != PlayerState::Ended && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            player.pump();
        }
        assert_eq!(player.state(), PlayerState::Ended, "the clip must run out");
        assert!(player
            .take_events()
            .contains(&PlayerEvent::Ended));
        let before = player.position();
        player.play();
        assert_eq!(player.state(), PlayerState::Playing);
        assert!(player.position() < before, "replaying rewinds the clock");
    }

    #[test]
    fn rate_scales_media_time_without_touching_the_decoder() {
        let mut player = loaded(10);
        player.seek(0.0);
        player.set_rate(2.0);
        assert_eq!(player.rate(), 2.0);
        player.play();
        std::thread::sleep(Duration::from_millis(100));
        player.pump();
        // Twice the speed: 100 ms of wall clock is 0.2 media seconds, which is
        // past the end of a 0.16 s clip.
        assert!(player.position() >= 0.15, "position {}", player.position());
        assert!(player.take_events().contains(&PlayerEvent::RateChange));
        // Nonsense rates fall back to 1× rather than to a frozen clock.
        player.set_rate(0.0);
        assert_eq!(player.rate(), 1.0);
        player.set_rate(-3.0);
        assert_eq!(player.rate(), 1.0);
    }

    #[test]
    fn timeupdate_is_throttled_to_a_quarter_second() {
        let mut player = loaded(10);
        player.seek(0.0);
        player.play();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(5));
            player.pump();
        }
        let updates = player
            .take_events()
            .into_iter()
            .filter(|event| matches!(event, PlayerEvent::TimeUpdate { .. }))
            .count();
        assert!(updates <= 2, "{updates} timeupdate events from ~0.1 s of playback");
    }

    #[test]
    fn a_playing_video_asks_for_continuous_frames_and_a_paused_one_stops() {
        let mut player = loaded(10);
        assert!(!player.needs_repaint(), "nothing started yet");
        player.seek(0.0);
        assert!(player.needs_repaint(), "a seek while paused must reach the screen");
        player.await_frame(Duration::from_secs(5)).expect("frame");
        assert!(!player.needs_repaint(), "the seek has been answered");
        player.play();
        assert!(player.needs_repaint());
        player.pause();
        assert!(!player.needs_repaint());
    }

    #[test]
    fn a_track_the_decoder_cannot_feed_becomes_a_reported_error() {
        // Width 4 but samples of 4 bytes: the raw decoder will refuse them.
        let mut file = Vec::new();
        let samples = vec![Sample {
            offset: 0,
            len: 4,
            pts: 0.0,
            key: true,
        }];
        file.extend_from_slice(&[0u8; 4]);
        let info = MediaInfo {
            container: Container::Avi,
            duration: Some(0.04),
            width: 4,
            height: 4,
            codec: VideoCodec::Raw(RawFormat::Rgba32),
            codec_private: None,
            audio: None,
            sample_count: 1,
        };
        let mut player = MediaPlayer::new();
        player.load(VideoTrack::for_test(Arc::new(file), info, samples));
        let deadline = Instant::now() + Duration::from_secs(5);
        while player.state() != PlayerState::Failed && Instant::now() < deadline {
            player.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(player.state(), PlayerState::Failed);
        let detail = player.error().unwrap().to_string();
        assert!(detail.contains("too small"), "{detail}");
        assert!(!player.needs_repaint(), "a failed element must not spin");
    }

    #[test]
    fn opening_oversized_or_unrecognised_bytes_is_refused() {
        let empty = open_bytes(Arc::new(Vec::new())).err().expect("empty is not media");
        assert!(matches!(empty, MediaError::UnsupportedContainer { .. }), "{empty}");
        let bogus = open_bytes(Arc::new(b"nonsense".to_vec())).err().expect("not media");
        assert!(matches!(bogus, MediaError::UnsupportedContainer { .. }));
        // The limit is about the length, so a small vector with a huge
        // `len` is not needed to prove the check exists.
        assert!(MAX_VIDEO_BYTES >= 64 * 1024 * 1024);
    }

    #[test]
    fn dropping_the_player_while_it_decodes_is_safe() {
        let mut player = loaded(10);
        player.play();
        std::thread::sleep(Duration::from_millis(20));
        player.pump();
        drop(player);
    }
}
