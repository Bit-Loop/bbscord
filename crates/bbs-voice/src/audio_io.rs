//! Real-time `cpal` audio capture & playback bridge with graceful headless/CI fallback.
//!
//! All `cpal` host/device probing and stream ownership run on a dedicated OS thread rather than
//! Tokio async workers, keeping blocking ALSA/PipeWire/PulseAudio calls off the async runtime.
//! Both capture and playback callbacks operate on pre-allocated scratch buffers, stateful
//! fractional-phase linear resamplers, and non-blocking `try_lock` bounded queues so steady-state
//! real-time audio callbacks perform zero heap allocations and never block.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Sample, SampleFormat, Stream, StreamConfig, SupportedStreamConfig};
use tokio::sync::{Notify, oneshot};
use tracing::{debug, warn};

use crate::error::{VoiceError, VoiceResult};
use crate::mixer::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

/// Maximum buffered playback samples (~200 ms of stereo 48 kHz audio = 10 frames).
const MAX_PLAYBACK_QUEUE_SAMPLES: usize = FRAME_SAMPLES * 10;
/// Default playback priming cushion (~60 ms of stereo 48 kHz audio = 3 frames) so PipeWire
/// `1024`-frame (`21.33 ms`) quantums never starve immediately after a `20 ms` (`960`-frame) tick.
pub const DEFAULT_PLAYBACK_PREFILL_SAMPLES: usize = FRAME_SAMPLES * 3;
/// Maximum buffered capture samples (~160 ms of stereo 48 kHz audio = 8 frames).
const MAX_CAPTURE_QUEUE_SAMPLES: usize = FRAME_SAMPLES * 8;
/// Initial scratch capacity hoisted outside real-time `cpal` callbacks (~80 ms stereo).
const RT_CALLBACK_SCRATCH_CAPACITY: usize = FRAME_SAMPLES * 4;

/// Policy selecting whether to probe live hardware devices or force headless mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioBackendMode {
    /// Probe `cpal::default_host()`; transparently degrade to headless if no soundcard exists.
    #[default]
    Auto,
    /// Never touch OS audio devices (ideal for CI, containers, and offline tests).
    Headless,
}

/// Active audio I/O backend state reported by [`AudioIoHandle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackendKind {
    /// Live `cpal` audio stream attached to a host soundcard / PipeWire / PulseAudio / ALSA sink.
    CpalLive,
    /// Graceful headless fallback (no hardware device available or headless mode requested).
    Headless,
}

/// Non-blocking stereo 48 kHz PCM capture queue shared between `cpal` input callback and TX worker.
#[derive(Debug)]
pub struct CaptureQueue {
    buf: Mutex<VecDeque<f32>>,
    notify: Notify,
}

impl Default for CaptureQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureQueue {
    /// Construct an empty pre-allocated bounded capture queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Mutex::new(VecDeque::with_capacity(MAX_CAPTURE_QUEUE_SAMPLES)),
            notify: Notify::new(),
        }
    }

    /// Non-blocking push used inside real-time `cpal` input callbacks (`try_lock`, zero allocs).
    ///
    /// Drops oldest samples if the consumer falls behind so live voice never accumulates lag.
    pub fn push_samples_nonblocking(&self, samples: &[f32]) {
        let Ok(mut guard) = self.buf.try_lock() else {
            return;
        };
        let total = guard.len().saturating_add(samples.len());
        if total > MAX_CAPTURE_QUEUE_SAMPLES {
            let overflow = total - MAX_CAPTURE_QUEUE_SAMPLES;
            let drop_count = overflow.min(guard.len());
            guard.drain(..drop_count);
        }
        let available = MAX_CAPTURE_QUEUE_SAMPLES.saturating_sub(guard.len());
        let start = samples.len().saturating_sub(available);
        guard.extend(samples[start..].iter().copied());
        let has_frame = guard.len() >= FRAME_SAMPLES;
        drop(guard);
        if has_frame {
            self.notify.notify_one();
        }
    }

    /// Push a synthetic 20 ms stereo frame from async/test code and wake the consumer.
    pub fn push_samples(&self, samples: &[f32]) {
        let Ok(mut guard) = self.buf.lock() else {
            return;
        };
        let total = guard.len().saturating_add(samples.len());
        if total > MAX_CAPTURE_QUEUE_SAMPLES {
            let overflow = total - MAX_CAPTURE_QUEUE_SAMPLES;
            let drop_count = overflow.min(guard.len());
            guard.drain(..drop_count);
        }
        let available = MAX_CAPTURE_QUEUE_SAMPLES.saturating_sub(guard.len());
        let start = samples.len().saturating_sub(available);
        guard.extend(samples[start..].iter().copied());
        let has_frame = guard.len() >= FRAME_SAMPLES;
        drop(guard);
        if has_frame {
            self.notify.notify_one();
        }
    }

    /// Try to pop one exact 20 ms stereo frame (`1920` samples) into `dst`.
    pub fn try_pop_frame(&self, dst: &mut [f32; FRAME_SAMPLES]) -> bool {
        let Ok(mut guard) = self.buf.lock() else {
            return false;
        };
        if guard.len() < FRAME_SAMPLES {
            return false;
        }
        for slot in dst.iter_mut() {
            *slot = guard.pop_front().unwrap_or(0.0);
        }
        true
    }

    /// Wait asynchronously until one full 20 ms stereo frame (`1920` samples) is available.
    pub async fn recv_frame(&self, dst: &mut [f32; FRAME_SAMPLES]) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.try_pop_frame(dst) {
                return;
            }
            notified.await;
        }
    }

    /// Clear all queued capture samples.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.buf.lock() {
            guard.clear();
        }
    }

    /// Current number of buffered interleaved samples.
    #[must_use]
    pub fn buffered_samples(&self) -> usize {
        self.buf.lock().map_or(0, |g| g.len())
    }
}

#[derive(Debug)]
struct PlaybackBufferState {
    samples: VecDeque<f32>,
    primed: bool,
    prefill_samples: usize,
}

/// Non-blocking stereo 48 kHz PCM playback queue shared between mixer producer and `cpal` output
/// callback, with a priming latch (`DEFAULT_PLAYBACK_PREFILL_SAMPLES` = 60 ms) that prevents
/// immediate underruns on PipeWire `1024`-frame quantums and resets cleanly on drain.
#[derive(Debug)]
pub struct PlaybackQueue {
    buf: Mutex<PlaybackBufferState>,
}

impl Default for PlaybackQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackQueue {
    /// Construct an empty pre-allocated bounded playback queue with the default 60 ms priming cushion.
    #[must_use]
    pub fn new() -> Self {
        Self::with_prefill_samples(DEFAULT_PLAYBACK_PREFILL_SAMPLES)
    }

    /// Construct an empty pre-allocated playback queue with an explicit priming threshold in interleaved samples.
    #[must_use]
    pub fn with_prefill_samples(prefill_samples: usize) -> Self {
        Self {
            buf: Mutex::new(PlaybackBufferState {
                samples: VecDeque::with_capacity(MAX_PLAYBACK_QUEUE_SAMPLES),
                primed: false,
                prefill_samples: prefill_samples.min(MAX_PLAYBACK_QUEUE_SAMPLES),
            }),
        }
    }

    /// Push interleaved 48 kHz stereo `f32` samples for playback, dropping oldest samples if full.
    pub fn push_samples(&self, samples: &[f32]) {
        let Ok(mut guard) = self.buf.lock() else {
            return;
        };
        if guard.samples.len().saturating_add(samples.len()) > MAX_PLAYBACK_QUEUE_SAMPLES {
            let overflow = guard
                .samples
                .len()
                .saturating_add(samples.len())
                .saturating_sub(MAX_PLAYBACK_QUEUE_SAMPLES);
            let drop_count = overflow.min(guard.samples.len());
            guard.samples.drain(..drop_count);
        }
        let available = MAX_PLAYBACK_QUEUE_SAMPLES.saturating_sub(guard.samples.len());
        let start = samples.len().saturating_sub(available);
        guard.samples.extend(samples[start..].iter().copied());
    }

    /// Release the priming latch immediately if any samples are queued (e.g. short clip EOF).
    pub fn flush_prefill(&self) {
        if let Ok(mut guard) = self.buf.lock()
            && !guard.samples.is_empty()
        {
            guard.primed = true;
        }
    }

    /// Clear all queued playback samples and reset the priming latch (e.g. on mute/deafen/stop).
    pub fn clear(&self) {
        if let Ok(mut guard) = self.buf.lock() {
            guard.samples.clear();
            guard.primed = false;
        }
    }

    /// Non-blocking pop used inside real-time `cpal` output callbacks.
    ///
    /// Uses `try_lock` so an audio callback never blocks; holds silence while buffering up to
    /// `prefill_samples`, then drains continuously and re-arms the priming latch once fully drained.
    pub fn pop_into_nonblocking(&self, dst: &mut [f32]) -> usize {
        let Ok(mut guard) = self.buf.try_lock() else {
            dst.fill(0.0);
            return 0;
        };
        if !guard.primed {
            if !guard.samples.is_empty() && guard.samples.len() >= guard.prefill_samples {
                guard.primed = true;
            } else {
                dst.fill(0.0);
                return 0;
            }
        }
        let mut read = 0usize;
        for out in dst.iter_mut() {
            if let Some(sample) = guard.samples.pop_front() {
                *out = sample;
                read += 1;
            } else {
                *out = 0.0;
            }
        }
        if guard.samples.is_empty() {
            guard.primed = false;
        }
        read
    }

    /// Returns `true` when the priming latch is currently open and draining audio.
    #[must_use]
    pub fn is_primed(&self) -> bool {
        self.buf.lock().is_ok_and(|g| g.primed)
    }

    /// Current number of buffered interleaved samples.
    #[must_use]
    pub fn buffered_samples(&self) -> usize {
        self.buf.lock().map_or(0, |g| g.samples.len())
    }
}

enum StartReply {
    Sync(std_mpsc::Sender<AudioBackendKind>),
    Async(oneshot::Sender<AudioBackendKind>),
    None,
}

impl StartReply {
    fn send(self, kind: AudioBackendKind) {
        match self {
            Self::Sync(tx) => {
                let _ = tx.send(kind);
            }
            Self::Async(tx) => {
                let _ = tx.send(kind);
            }
            Self::None => {}
        }
    }
}

enum WorkerCommand {
    StartCapture {
        queue: Arc<CaptureQueue>,
        reply: StartReply,
    },
    StopCapture {
        reply: Option<std_mpsc::Sender<()>>,
    },
    StartPlayback {
        queue: Arc<PlaybackQueue>,
        reply: StartReply,
    },
    StopPlayback {
        reply: Option<std_mpsc::Sender<()>>,
    },
    Shutdown {
        reply: Option<std_mpsc::Sender<()>>,
    },
}

/// Thread-safe handle controlling capture and playback streams on a dedicated audio thread.
///
/// Dropping [`AudioIoHandle`] sends a non-blocking shutdown command so teardown never blocks
/// a Tokio async executor thread; callers may also await [`Self::shutdown`] explicitly.
pub struct AudioIoHandle {
    cmd_tx: std_mpsc::Sender<WorkerCommand>,
    capture_active: Arc<AtomicBool>,
    playback_active: Arc<AtomicBool>,
}

impl std::fmt::Debug for AudioIoHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioIoHandle")
            .field("capture_active", &self.capture_active.load(Ordering::Relaxed))
            .field(
                "playback_active",
                &self.playback_active.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl AudioIoHandle {
    /// Spawn the dedicated audio I/O control thread under `mode`.
    pub fn spawn(mode: AudioBackendMode) -> VoiceResult<Self> {
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<WorkerCommand>();
        let capture_active = Arc::new(AtomicBool::new(false));
        let playback_active = Arc::new(AtomicBool::new(false));

        let cap_flag = Arc::clone(&capture_active);
        let play_flag = Arc::clone(&playback_active);

        thread::Builder::new()
            .name("bbs-voice-audio-io".to_string())
            .spawn(move || run_audio_worker(mode, cmd_rx, cap_flag, play_flag))
            .map_err(|e| VoiceError::AudioDevice(format!("failed to spawn audio thread: {e}")))?;

        Ok(Self {
            cmd_tx,
            capture_active,
            playback_active,
        })
    }

    /// Non-blockingly enqueue microphone capture start on the audio thread.
    ///
    /// Returns an async [`oneshot::Receiver`] that resolves with [`AudioBackendKind`] once the
    /// audio thread opens (or degrades) the stream, without ever stalling a `tokio::select!` loop.
    #[must_use]
    pub fn request_start_capture(
        &self,
        queue: Arc<CaptureQueue>,
    ) -> oneshot::Receiver<AudioBackendKind> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(WorkerCommand::StartCapture {
                queue,
                reply: StartReply::Async(reply_tx),
            })
            .is_err()
        {
            let (fallback_tx, fallback_rx) = oneshot::channel();
            let _ = fallback_tx.send(AudioBackendKind::Headless);
            return fallback_rx;
        }
        reply_rx
    }

    /// Non-blockingly enqueue microphone capture teardown on the audio thread.
    pub fn request_stop_capture(&self) {
        let _ = self.cmd_tx.send(WorkerCommand::StopCapture { reply: None });
    }

    /// Non-blockingly enqueue speaker playback start on the audio thread.
    pub fn request_start_playback(&self, queue: Arc<PlaybackQueue>) {
        let _ = self.cmd_tx.send(WorkerCommand::StartPlayback {
            queue,
            reply: StartReply::None,
        });
    }

    /// Non-blockingly enqueue speaker playback teardown on the audio thread.
    pub fn request_stop_playback(&self) {
        let _ = self
            .cmd_tx
            .send(WorkerCommand::StopPlayback { reply: None });
    }

    /// Start microphone capture, writing 20 ms 48 kHz stereo frames into `queue`.
    ///
    /// Gracefully returns [`AudioBackendKind::Headless`] if no input device is available.
    pub async fn start_capture(&self, queue: Arc<CaptureQueue>) -> VoiceResult<AudioBackendKind> {
        let cmd_tx = self.cmd_tx.clone();
        tokio::task::spawn_blocking(move || {
            let (reply_tx, reply_rx) = std_mpsc::channel();
            cmd_tx
                .send(WorkerCommand::StartCapture {
                    queue,
                    reply: StartReply::Sync(reply_tx),
                })
                .map_err(|_| VoiceError::AudioDevice("audio I/O thread stopped".to_string()))?;
            reply_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| VoiceError::AudioDevice("timed out starting capture stream".to_string()))
        })
        .await
        .map_err(|e| VoiceError::Internal(format!("audio start_capture task failed: {e}")))?
    }

    /// Stop active microphone capture, if running.
    pub async fn stop_capture(&self) -> VoiceResult<()> {
        let cmd_tx = self.cmd_tx.clone();
        tokio::task::spawn_blocking(move || {
            let (reply_tx, reply_rx) = std_mpsc::channel();
            let _ = cmd_tx.send(WorkerCommand::StopCapture {
                reply: Some(reply_tx),
            });
            let _ = reply_rx.recv_timeout(Duration::from_secs(5));
        })
        .await
        .map_err(|e| VoiceError::Internal(format!("audio stop_capture task failed: {e}")))?;
        Ok(())
    }

    /// Start speaker playback draining 48 kHz stereo frames from `queue`.
    ///
    /// Gracefully returns [`AudioBackendKind::Headless`] if no output device is available.
    pub async fn start_playback(&self, queue: Arc<PlaybackQueue>) -> VoiceResult<AudioBackendKind> {
        let cmd_tx = self.cmd_tx.clone();
        tokio::task::spawn_blocking(move || {
            let (reply_tx, reply_rx) = std_mpsc::channel();
            cmd_tx
                .send(WorkerCommand::StartPlayback {
                    queue,
                    reply: StartReply::Sync(reply_tx),
                })
                .map_err(|_| VoiceError::AudioDevice("audio I/O thread stopped".to_string()))?;
            reply_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| VoiceError::AudioDevice("timed out starting playback stream".to_string()))
        })
        .await
        .map_err(|e| VoiceError::Internal(format!("audio start_playback task failed: {e}")))?
    }

    /// Stop active speaker playback, if running.
    pub async fn stop_playback(&self) -> VoiceResult<()> {
        let cmd_tx = self.cmd_tx.clone();
        tokio::task::spawn_blocking(move || {
            let (reply_tx, reply_rx) = std_mpsc::channel();
            let _ = cmd_tx.send(WorkerCommand::StopPlayback {
                reply: Some(reply_tx),
            });
            let _ = reply_rx.recv_timeout(Duration::from_secs(5));
        })
        .await
        .map_err(|e| VoiceError::Internal(format!("audio stop_playback task failed: {e}")))?;
        Ok(())
    }

    /// Asynchronously shut down the dedicated audio worker and wait for stream teardown
    /// without blocking any Tokio executor thread.
    pub async fn shutdown(&self) {
        let cmd_tx = self.cmd_tx.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let (reply_tx, reply_rx) = std_mpsc::channel();
            if cmd_tx
                .send(WorkerCommand::Shutdown {
                    reply: Some(reply_tx),
                })
                .is_ok()
            {
                let _ = reply_rx.recv_timeout(Duration::from_secs(5));
            }
        })
        .await;
    }

    /// Returns `true` if live hardware capture is currently running.
    #[must_use]
    pub fn is_capture_live(&self) -> bool {
        self.capture_active.load(Ordering::Relaxed)
    }

    /// Returns `true` if live hardware playback is currently running.
    #[must_use]
    pub fn is_playback_live(&self) -> bool {
        self.playback_active.load(Ordering::Relaxed)
    }
}

impl Drop for AudioIoHandle {
    fn drop(&mut self) {
        // Non-blocking shutdown signal: the dedicated audio thread tears down any open cpal
        // streams and exits on its own without ever blocking a Tokio worker thread.
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown { reply: None });
    }
}

fn run_audio_worker(
    mode: AudioBackendMode,
    cmd_rx: std_mpsc::Receiver<WorkerCommand>,
    capture_active: Arc<AtomicBool>,
    playback_active: Arc<AtomicBool>,
) {
    let mut capture_stream: Option<Stream> = None;
    let mut playback_stream: Option<Stream> = None;

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WorkerCommand::StartCapture { queue, reply } => {
                capture_stream = None;
                capture_active.store(false, Ordering::Relaxed);
                let kind = if mode == AudioBackendMode::Headless {
                    AudioBackendKind::Headless
                } else {
                    match open_cpal_capture(queue) {
                        Ok(stream) => {
                            capture_stream = Some(stream);
                            capture_active.store(true, Ordering::Relaxed);
                            AudioBackendKind::CpalLive
                        }
                        Err(err) => {
                            warn!("cpal capture unavailable, using headless fallback: {err}");
                            AudioBackendKind::Headless
                        }
                    }
                };
                reply.send(kind);
            }
            WorkerCommand::StopCapture { reply } => {
                capture_stream = None;
                capture_active.store(false, Ordering::Relaxed);
                if let Some(tx) = reply {
                    let _ = tx.send(());
                }
            }
            WorkerCommand::StartPlayback { queue, reply } => {
                playback_stream = None;
                playback_active.store(false, Ordering::Relaxed);
                let kind = if mode == AudioBackendMode::Headless {
                    AudioBackendKind::Headless
                } else {
                    match open_cpal_playback(queue) {
                        Ok(stream) => {
                            playback_stream = Some(stream);
                            playback_active.store(true, Ordering::Relaxed);
                            AudioBackendKind::CpalLive
                        }
                        Err(err) => {
                            warn!("cpal playback unavailable, using headless fallback: {err}");
                            AudioBackendKind::Headless
                        }
                    }
                };
                reply.send(kind);
            }
            WorkerCommand::StopPlayback { reply } => {
                playback_stream = None;
                playback_active.store(false, Ordering::Relaxed);
                if let Some(tx) = reply {
                    let _ = tx.send(());
                }
            }
            WorkerCommand::Shutdown { reply } => {
                drop(capture_stream.take());
                drop(playback_stream.take());
                capture_active.store(false, Ordering::Relaxed);
                playback_active.store(false, Ordering::Relaxed);
                if let Some(tx) = reply {
                    let _ = tx.send(());
                }
                break;
            }
        }
    }
}

/// Returns `true` if `cfg` already matches Discord's native 48 kHz mono/stereo PCM format.
///
/// Using the default config directly when it is already 48 kHz avoids redundant ALSA
/// `supported_*_configs()` hardware range probing, keeping ALSA plugin stderr chatter quiet
/// without requiring any `unsafe` C error-handler FFI.
fn is_directly_usable_default_config(cfg: &SupportedStreamConfig) -> bool {
    cfg.sample_rate() == SAMPLE_RATE
        && (1..=2).contains(&cfg.channels())
        && matches!(
            cfg.sample_format(),
            SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
        )
}

fn select_preferred_config<I>(configs: I, fallback: SupportedStreamConfig) -> SupportedStreamConfig
where
    I: Iterator<Item = cpal::SupportedStreamConfigRange>,
{
    let mut best: Option<SupportedStreamConfig> = None;
    let mut best_score = -1i32;

    for range in configs {
        let Some(cfg) = range.try_with_sample_rate(SAMPLE_RATE) else {
            continue;
        };
        let mut score = 0;
        if cfg.channels() == CHANNELS as u16 {
            score += 100;
        } else if cfg.channels() == 1 {
            score += 50;
        }
        match cfg.sample_format() {
            SampleFormat::F32 => score += 30,
            SampleFormat::I16 => score += 20,
            SampleFormat::U16 => score += 10,
            _ => continue,
        }
        if score > best_score {
            best_score = score;
            best = Some(cfg);
        }
    }

    best.unwrap_or(fallback)
}

/// Stateful fractional-phase linear resampler converting arbitrary device capture rates/channels
/// into continuous 48 kHz interleaved stereo PCM without boundary clicks or clock drift.
#[derive(Debug)]
pub(crate) struct CaptureResampler {
    src_channels: usize,
    src_rate: u32,
    step: f64,
    read_pos: f64,
    in_stereo: Vec<f32>,
    out_48k_stereo: Vec<f32>,
}

impl CaptureResampler {
    pub(crate) fn new(src_channels: usize, src_rate: u32) -> Self {
        let rate = src_rate.max(1);
        Self {
            src_channels: src_channels.max(1),
            src_rate: rate,
            step: f64::from(rate) / f64::from(SAMPLE_RATE),
            read_pos: 0.0,
            in_stereo: Vec::with_capacity(RT_CALLBACK_SCRATCH_CAPACITY),
            out_48k_stereo: Vec::with_capacity(RT_CALLBACK_SCRATCH_CAPACITY),
        }
    }

    pub(crate) fn ingest(&mut self, input: &[f32], queue: &CaptureQueue) {
        if input.is_empty() {
            return;
        }
        let src_frames = input.len() / self.src_channels;
        if src_frames == 0 {
            return;
        }

        self.out_48k_stereo.clear();

        if self.src_rate == SAMPLE_RATE {
            for f in 0..src_frames {
                let base = f * self.src_channels;
                let left = input[base];
                let right = if self.src_channels >= 2 {
                    input[base + 1]
                } else {
                    left
                };
                self.out_48k_stereo.push(left);
                self.out_48k_stereo.push(right);
            }
            queue.push_samples_nonblocking(&self.out_48k_stereo);
            return;
        }

        for f in 0..src_frames {
            let base = f * self.src_channels;
            let left = input[base];
            let right = if self.src_channels >= 2 {
                input[base + 1]
            } else {
                left
            };
            self.in_stereo.push(left);
            self.in_stereo.push(right);
        }

        let avail_frames = self.in_stereo.len() / CHANNELS;
        while self.read_pos + 1.0 < avail_frames as f64 {
            let i0 = self.read_pos.floor() as usize;
            let i1 = i0 + 1;
            let frac = (self.read_pos - i0 as f64) as f32;

            let l0 = self.in_stereo[i0 * CHANNELS];
            let r0 = self.in_stereo[i0 * CHANNELS + 1];
            let l1 = self.in_stereo[i1 * CHANNELS];
            let r1 = self.in_stereo[i1 * CHANNELS + 1];

            self.out_48k_stereo.push(l0 + (l1 - l0) * frac);
            self.out_48k_stereo.push(r0 + (r1 - r0) * frac);
            self.read_pos += self.step;
        }

        let consumed_frames = self.read_pos.floor() as usize;
        if consumed_frames > 0 {
            let drop_samples = (consumed_frames * CHANNELS).min(self.in_stereo.len());
            self.in_stereo.drain(..drop_samples);
            self.read_pos -= consumed_frames as f64;
        }

        if !self.out_48k_stereo.is_empty() {
            queue.push_samples_nonblocking(&self.out_48k_stereo);
        }
    }
}

/// Stateful fractional-phase linear resampler adapting 48 kHz interleaved stereo PCM from
/// [`PlaybackQueue`] to arbitrary device output rates/channels without clicks or aliasing.
#[derive(Debug)]
pub(crate) struct PlaybackResampler {
    dst_channels: usize,
    dst_rate: u32,
    step: f64,
    read_pos: f64,
    in_stereo: Vec<f32>,
    scratch: Vec<f32>,
}

impl PlaybackResampler {
    pub(crate) fn new(dst_channels: usize, dst_rate: u32) -> Self {
        let rate = dst_rate.max(1);
        Self {
            dst_channels: dst_channels.max(1),
            dst_rate: rate,
            step: f64::from(SAMPLE_RATE) / f64::from(rate),
            read_pos: 0.0,
            in_stereo: Vec::with_capacity(RT_CALLBACK_SCRATCH_CAPACITY),
            scratch: Vec::with_capacity(RT_CALLBACK_SCRATCH_CAPACITY),
        }
    }

    pub(crate) fn render(&mut self, queue: &PlaybackQueue, out: &mut [f32]) {
        if out.is_empty() {
            return;
        }

        if self.dst_channels == CHANNELS && self.dst_rate == SAMPLE_RATE {
            queue.pop_into_nonblocking(out);
            return;
        }

        let dst_frames = out.len() / self.dst_channels;
        if dst_frames == 0 {
            return;
        }

        if self.dst_rate == SAMPLE_RATE {
            let needed = dst_frames * CHANNELS;
            self.scratch.resize(needed, 0.0);
            queue.pop_into_nonblocking(&mut self.scratch[..needed]);
            for df in 0..dst_frames {
                let l = self.scratch[df * CHANNELS];
                let r = self.scratch[df * CHANNELS + 1];
                write_device_frame(out, df, self.dst_channels, l, r);
            }
            return;
        }

        let last_pos = self.read_pos + ((dst_frames.saturating_sub(1)) as f64) * self.step;
        let needed_src_frames = (last_pos.floor() as usize).saturating_add(2);
        let have_src_frames = self.in_stereo.len() / CHANNELS;
        if have_src_frames < needed_src_frames {
            let missing_samples = (needed_src_frames - have_src_frames) * CHANNELS;
            self.scratch.resize(missing_samples, 0.0);
            queue.pop_into_nonblocking(&mut self.scratch[..missing_samples]);
            self.in_stereo.extend_from_slice(&self.scratch[..missing_samples]);
        }

        let avail_frames = self.in_stereo.len() / CHANNELS;
        let max_idx = avail_frames.saturating_sub(1);
        for df in 0..dst_frames {
            let i0 = (self.read_pos.floor() as usize).min(max_idx);
            let i1 = (i0 + 1).min(max_idx);
            let frac = (self.read_pos - i0 as f64).clamp(0.0, 1.0) as f32;

            let l0 = self.in_stereo[i0 * CHANNELS];
            let r0 = self.in_stereo[i0 * CHANNELS + 1];
            let l1 = self.in_stereo[i1 * CHANNELS];
            let r1 = self.in_stereo[i1 * CHANNELS + 1];

            let l = l0 + (l1 - l0) * frac;
            let r = r0 + (r1 - r0) * frac;
            write_device_frame(out, df, self.dst_channels, l, r);
            self.read_pos += self.step;
        }

        let consumed_frames = self.read_pos.floor() as usize;
        if consumed_frames > 0 {
            let drop_samples = (consumed_frames * CHANNELS).min(self.in_stereo.len());
            self.in_stereo.drain(..drop_samples);
            self.read_pos -= consumed_frames as f64;
        }
    }
}

#[inline]
fn write_device_frame(out: &mut [f32], frame_idx: usize, dst_channels: usize, l: f32, r: f32) {
    let base = frame_idx * dst_channels;
    if dst_channels == 1 {
        out[base] = 0.5 * (l + r);
    } else {
        out[base] = l;
        out[base + 1] = r;
        for ch in 2..dst_channels {
            out[base + ch] = 0.0;
        }
    }
}

fn open_cpal_capture(queue: Arc<CaptureQueue>) -> VoiceResult<Stream> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| VoiceError::AudioDevice("no default input device found".to_string()))?;

    let default_cfg = device
        .default_input_config()
        .map_err(|e| VoiceError::AudioDevice(format!("default_input_config failed: {e}")))?;

    let chosen = if is_directly_usable_default_config(&default_cfg) {
        default_cfg
    } else {
        device
            .supported_input_configs()
            .map(|iter| select_preferred_config(iter, default_cfg))
            .unwrap_or(default_cfg)
    };

    let dev_channels = usize::from(chosen.channels().max(1));
    let dev_rate = chosen.sample_rate().max(1);
    let sample_format = chosen.sample_format();
    let mut stream_cfg: StreamConfig = chosen.config();
    stream_cfg.buffer_size = BufferSize::Default;

    debug!(
        "opening cpal capture: {} Hz, {} ch, {:?}",
        dev_rate, dev_channels, sample_format
    );

    let err_fn = |err| warn!("cpal capture stream error: {err}");
    // Pre-allocate stateful resampler & conversion buffers OUTSIDE real-time callbacks.
    let mut resampler = CaptureResampler::new(dev_channels, dev_rate);
    let mut converted = Vec::<f32>::with_capacity(RT_CALLBACK_SCRATCH_CAPACITY);

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            stream_cfg,
            move |data: &[f32], _| {
                resampler.ingest(data, &queue);
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        SampleFormat::I16 => device.build_input_stream(
            stream_cfg,
            move |data: &[i16], _| {
                converted.clear();
                converted.extend(data.iter().map(|&s| f32::from_sample(s)));
                resampler.ingest(&converted, &queue);
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        SampleFormat::U16 => device.build_input_stream(
            stream_cfg,
            move |data: &[u16], _| {
                converted.clear();
                converted.extend(data.iter().map(|&s| f32::from_sample(s)));
                resampler.ingest(&converted, &queue);
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        other => {
            return Err(VoiceError::AudioDevice(format!(
                "unsupported cpal capture sample format: {other:?}"
            )));
        }
    }
    .map_err(|e| VoiceError::AudioDevice(format!("build_input_stream failed: {e}")))?;

    stream
        .play()
        .map_err(|e| VoiceError::AudioDevice(format!("capture stream play failed: {e}")))?;
    Ok(stream)
}

fn open_cpal_playback(queue: Arc<PlaybackQueue>) -> VoiceResult<Stream> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| VoiceError::AudioDevice("no default output device found".to_string()))?;

    let default_cfg = device
        .default_output_config()
        .map_err(|e| VoiceError::AudioDevice(format!("default_output_config failed: {e}")))?;

    let chosen = if is_directly_usable_default_config(&default_cfg) {
        default_cfg
    } else {
        device
            .supported_output_configs()
            .map(|iter| select_preferred_config(iter, default_cfg))
            .unwrap_or(default_cfg)
    };

    let dev_channels = usize::from(chosen.channels().max(1));
    let dev_rate = chosen.sample_rate().max(1);
    let sample_format = chosen.sample_format();
    let mut stream_cfg: StreamConfig = chosen.config();
    stream_cfg.buffer_size = BufferSize::Default;

    debug!(
        "opening cpal playback: {} Hz, {} ch, {:?}",
        dev_rate, dev_channels, sample_format
    );

    let err_fn = |err| warn!("cpal playback stream error: {err}");
    // Pre-allocate stateful resampler & output conversion buffers OUTSIDE real-time callbacks.
    let mut resampler = PlaybackResampler::new(dev_channels, dev_rate);
    let mut f32_out = Vec::<f32>::with_capacity(RT_CALLBACK_SCRATCH_CAPACITY);

    let stream = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            stream_cfg,
            move |out: &mut [f32], _| {
                resampler.render(&queue, out);
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        SampleFormat::I16 => device.build_output_stream(
            stream_cfg,
            move |out: &mut [i16], _| {
                f32_out.resize(out.len(), 0.0);
                resampler.render(&queue, &mut f32_out);
                for (dst, &src) in out.iter_mut().zip(f32_out.iter()) {
                    *dst = i16::from_sample(src.clamp(-1.0, 1.0));
                }
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        SampleFormat::U16 => device.build_output_stream(
            stream_cfg,
            move |out: &mut [u16], _| {
                f32_out.resize(out.len(), 0.0);
                resampler.render(&queue, &mut f32_out);
                for (dst, &src) in out.iter_mut().zip(f32_out.iter()) {
                    *dst = u16::from_sample(src.clamp(-1.0, 1.0));
                }
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        other => {
            return Err(VoiceError::AudioDevice(format!(
                "unsupported cpal playback sample format: {other:?}"
            )));
        }
    }
    .map_err(|e| VoiceError::AudioDevice(format!("build_output_stream failed: {e}")))?;

    stream
        .play()
        .map_err(|e| VoiceError::AudioDevice(format!("playback stream play failed: {e}")))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn headless_audio_io_handle_starts_stops_and_shuts_down_nonblockingly() {
        let handle = AudioIoHandle::spawn(AudioBackendMode::Headless).expect("spawn headless");
        let cap_queue = Arc::new(CaptureQueue::new());
        let cap_kind = handle
            .start_capture(Arc::clone(&cap_queue))
            .await
            .expect("start capture");
        assert_eq!(cap_kind, AudioBackendKind::Headless);
        assert!(!handle.is_capture_live());

        // Exercise non-blocking request_* methods used by Voice Gateway select! loop.
        let async_kind = handle
            .request_start_capture(Arc::clone(&cap_queue))
            .await
            .expect("oneshot reply");
        assert_eq!(async_kind, AudioBackendKind::Headless);
        handle.request_stop_capture();

        let play_queue = Arc::new(PlaybackQueue::new());
        handle.request_start_playback(Arc::clone(&play_queue));
        handle.request_stop_playback();

        let play_kind = handle
            .start_playback(Arc::clone(&play_queue))
            .await
            .expect("start playback");
        assert_eq!(play_kind, AudioBackendKind::Headless);
        assert!(!handle.is_playback_live());

        handle.stop_capture().await.expect("stop capture");
        handle.stop_playback().await.expect("stop playback");
        handle.shutdown().await;
    }

    #[test]
    fn playback_and_capture_queues_handle_mono_upmix_without_panics() {
        let queue = PlaybackQueue::with_prefill_samples(0);
        let src: Vec<f32> = (0..FRAME_SAMPLES).map(|i| (i as f32) / 2000.0).collect();
        queue.push_samples(&src);
        assert_eq!(queue.buffered_samples(), FRAME_SAMPLES);

        let mut out = vec![0.0f32; FRAME_SAMPLES];
        let mut playback = PlaybackResampler::new(CHANNELS, SAMPLE_RATE);
        playback.render(&queue, &mut out);
        assert_eq!(out, src);
        assert_eq!(queue.buffered_samples(), 0);

        // Mono 48 kHz capture -> stereo 48 kHz 20 ms frame via CaptureQueue.
        let cap_queue = CaptureQueue::new();
        let mut capture = CaptureResampler::new(1, SAMPLE_RATE);
        let mono_in = vec![0.25f32; 960];
        capture.ingest(&mono_in, &cap_queue);
        let mut frame = [0.0f32; FRAME_SAMPLES];
        assert!(cap_queue.try_pop_frame(&mut frame));
        assert!((frame[0] - 0.25).abs() < f32::EPSILON);
        assert!((frame[1] - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn playback_queue_prefill_priming_prevents_pipewire_1024_quantum_underruns() {
        let queue = PlaybackQueue::new();
        assert!(!queue.is_primed());

        let frame = vec![0.5f32; FRAME_SAMPLES];
        let mut pipewire_quantum = vec![0.0f32; 1024 * CHANNELS];

        // Pushing 1 or 2 frames (< 60 ms cushion) holds silence without starving mid-quantum.
        queue.push_samples(&frame);
        queue.push_samples(&frame);
        assert_eq!(queue.pop_into_nonblocking(&mut pipewire_quantum), 0);
        assert!(pipewire_quantum.iter().all(|&s| s == 0.0));
        assert!(!queue.is_primed());
        assert_eq!(queue.buffered_samples(), FRAME_SAMPLES * 2);

        // Pushing 3rd frame reaches 60 ms cushion -> priming latch opens and serves full 1024-frame quantum!
        queue.push_samples(&frame);
        let read = queue.pop_into_nonblocking(&mut pipewire_quantum);
        assert_eq!(read, 1024 * CHANNELS);
        assert!(pipewire_quantum.iter().all(|&s| (s - 0.5).abs() < f32::EPSILON));
        assert!(queue.is_primed());

        // Drain the rest; once empty, the priming latch resets cleanly for the next talkspurt.
        while queue.buffered_samples() > 0 {
            let _ = queue.pop_into_nonblocking(&mut pipewire_quantum);
        }
        assert!(!queue.is_primed());
    }

    #[test]
    fn stateful_resamplers_preserve_phase_and_continuity_across_ragged_chunks() {
        // 1. Capture: 44.1 kHz mono sine -> 48 kHz stereo across ragged chunk sizes.
        let cap_queue = CaptureQueue::new();
        let mut cap = CaptureResampler::new(1, 44_100);
        let mut total_in = 0usize;
        let mut out_48k = Vec::new();
        let mut frame = [0.0f32; FRAME_SAMPLES];

        for chunk_len in [256usize, 441, 512, 333, 441, 600, 441, 512, 874] {
            let mut chunk = Vec::with_capacity(chunk_len);
            for i in 0..chunk_len {
                let t = (total_in + i) as f64 / 44_100.0;
                chunk.push((0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as f32);
            }
            total_in += chunk_len;
            cap.ingest(&chunk, &cap_queue);
            while cap_queue.try_pop_frame(&mut frame) {
                out_48k.extend_from_slice(&frame);
            }
        }

        let expected_frames = (total_in as f64 * 48_000.0 / 44_100.0) as usize;
        let actual_frames = (out_48k.len() + cap_queue.buffered_samples()) / CHANNELS;
        assert!(
            actual_frames.abs_diff(expected_frames) <= 4,
            "capture 44.1k->48k drift too high: actual={actual_frames}, expected={expected_frames}"
        );
        // Verify smooth continuity (no boundary click spikes > theoretical max step of 440 Hz @ 48 kHz).
        let max_step = out_48k
            .chunks_exact(CHANNELS)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1][0] - w[0][0]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_step < 0.06,
            "capture resampler produced discontinuity spike: max_step={max_step}"
        );

        // 2. Playback: 48 kHz stereo sine -> 44.1 kHz stereo across ragged output chunks.
        let play_queue = PlaybackQueue::with_prefill_samples(0);
        let mut src_48k = Vec::with_capacity(4800 * CHANNELS);
        for i in 0..4800 {
            let t = i as f64 / 48_000.0;
            let s = (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as f32;
            src_48k.push(s);
            src_48k.push(s);
        }
        play_queue.push_samples(&src_48k);

        let mut play = PlaybackResampler::new(CHANNELS, 44_100);
        let mut out_44k = Vec::new();
        for out_frames in [300usize, 512, 441, 1024, 700, 1024] {
            let mut buf = vec![0.0f32; out_frames * CHANNELS];
            play.render(&play_queue, &mut buf);
            out_44k.extend_from_slice(&buf);
        }

        let max_play_step = out_44k
            .chunks_exact(CHANNELS)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1][0] - w[0][0]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_play_step < 0.06,
            "playback resampler produced discontinuity spike: max_play_step={max_play_step}"
        );
    }
}
