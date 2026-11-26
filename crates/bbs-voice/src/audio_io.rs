//! Real-time `cpal` audio capture & playback bridge with graceful headless/CI fallback.
//!
//! All `cpal` host/device probing and stream ownership run on a dedicated OS thread rather than
//! Tokio async workers, keeping blocking ALSA/PipeWire/PulseAudio calls off the async runtime
//! and ensuring real-time audio callbacks never block on locks or channels.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Sample, SampleFormat, Stream, StreamConfig, SupportedStreamConfig};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, warn};

use crate::error::{VoiceError, VoiceResult};
use crate::mixer::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

/// Maximum buffered playback samples (~200 ms of stereo 48 kHz audio = 10 frames).
const MAX_PLAYBACK_QUEUE_SAMPLES: usize = FRAME_SAMPLES * 10;

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

/// Non-blocking stereo 48 kHz PCM playback queue shared between mixer producer and `cpal` output callback.
#[derive(Debug)]
pub struct PlaybackQueue {
    buf: Mutex<VecDeque<f32>>,
}

impl Default for PlaybackQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackQueue {
    /// Construct an empty bounded playback queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Mutex::new(VecDeque::with_capacity(MAX_PLAYBACK_QUEUE_SAMPLES)),
        }
    }

    /// Push interleaved 48 kHz stereo `f32` samples for playback, dropping oldest samples if full.
    pub fn push_samples(&self, samples: &[f32]) {
        let Ok(mut guard) = self.buf.lock() else {
            return;
        };
        if guard.len().saturating_add(samples.len()) > MAX_PLAYBACK_QUEUE_SAMPLES {
            let overflow = guard
                .len()
                .saturating_add(samples.len())
                .saturating_sub(MAX_PLAYBACK_QUEUE_SAMPLES);
            let drop_count = overflow.min(guard.len());
            guard.drain(..drop_count);
        }
        guard.extend(samples.iter().copied());
    }

    /// Clear all queued playback samples (e.g. on mute/deafen/stop).
    pub fn clear(&self) {
        if let Ok(mut guard) = self.buf.lock() {
            guard.clear();
        }
    }

    /// Non-blocking pop used inside real-time `cpal` output callbacks.
    ///
    /// Uses `try_lock` so an audio callback never blocks; fills silence on contention or underrun.
    pub fn pop_into_nonblocking(&self, dst: &mut [f32]) -> usize {
        let Ok(mut guard) = self.buf.try_lock() else {
            dst.fill(0.0);
            return 0;
        };
        let mut read = 0usize;
        for out in dst.iter_mut() {
            if let Some(sample) = guard.pop_front() {
                *out = sample;
                read += 1;
            } else {
                *out = 0.0;
            }
        }
        read
    }

    /// Current number of buffered interleaved samples.
    #[must_use]
    pub fn buffered_samples(&self) -> usize {
        self.buf.lock().map_or(0, |g| g.len())
    }
}

enum WorkerCommand {
    StartCapture {
        tx: tokio_mpsc::Sender<Vec<f32>>,
        reply: std_mpsc::Sender<AudioBackendKind>,
    },
    StopCapture {
        reply: std_mpsc::Sender<()>,
    },
    StartPlayback {
        queue: Arc<PlaybackQueue>,
        reply: std_mpsc::Sender<AudioBackendKind>,
    },
    StopPlayback {
        reply: std_mpsc::Sender<()>,
    },
    Shutdown,
}

/// Thread-safe handle controlling capture and playback streams on a dedicated audio thread.
pub struct AudioIoHandle {
    cmd_tx: std_mpsc::Sender<WorkerCommand>,
    capture_active: Arc<AtomicBool>,
    playback_active: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
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

        let worker = thread::Builder::new()
            .name("bbs-voice-audio-io".to_string())
            .spawn(move || run_audio_worker(mode, cmd_rx, cap_flag, play_flag))
            .map_err(|e| VoiceError::AudioDevice(format!("failed to spawn audio thread: {e}")))?;

        Ok(Self {
            cmd_tx,
            capture_active,
            playback_active,
            worker: Some(worker),
        })
    }

    /// Start microphone capture, streaming 20 ms 48 kHz stereo frames (`1920` `f32` samples) to `tx`.
    ///
    /// Gracefully returns [`AudioBackendKind::Headless`] if no input device is available.
    pub async fn start_capture(
        &self,
        tx: tokio_mpsc::Sender<Vec<f32>>,
    ) -> VoiceResult<AudioBackendKind> {
        let cmd_tx = self.cmd_tx.clone();
        tokio::task::spawn_blocking(move || {
            let (reply_tx, reply_rx) = std_mpsc::channel();
            cmd_tx
                .send(WorkerCommand::StartCapture {
                    tx,
                    reply: reply_tx,
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
            let _ = cmd_tx.send(WorkerCommand::StopCapture { reply: reply_tx });
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
                    reply: reply_tx,
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
            let _ = cmd_tx.send(WorkerCommand::StopPlayback { reply: reply_tx });
            let _ = reply_rx.recv_timeout(Duration::from_secs(5));
        })
        .await
        .map_err(|e| VoiceError::Internal(format!("audio stop_playback task failed: {e}")))?;
        Ok(())
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
        let _ = self.cmd_tx.send(WorkerCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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
            WorkerCommand::StartCapture { tx, reply } => {
                capture_stream = None;
                capture_active.store(false, Ordering::Relaxed);
                let kind = if mode == AudioBackendMode::Headless {
                    AudioBackendKind::Headless
                } else {
                    match open_cpal_capture(tx) {
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
                let _ = reply.send(kind);
            }
            WorkerCommand::StopCapture { reply } => {
                capture_stream = None;
                capture_active.store(false, Ordering::Relaxed);
                let _ = reply.send(());
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
                let _ = reply.send(kind);
            }
            WorkerCommand::StopPlayback { reply } => {
                playback_stream = None;
                playback_active.store(false, Ordering::Relaxed);
                let _ = reply.send(());
            }
            WorkerCommand::Shutdown => {
                drop(capture_stream.take());
                drop(playback_stream.take());
                capture_active.store(false, Ordering::Relaxed);
                playback_active.store(false, Ordering::Relaxed);
                break;
            }
        }
    }
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

fn open_cpal_capture(tx: tokio_mpsc::Sender<Vec<f32>>) -> VoiceResult<Stream> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| VoiceError::AudioDevice("no default input device found".to_string()))?;

    let default_cfg = device
        .default_input_config()
        .map_err(|e| VoiceError::AudioDevice(format!("default_input_config failed: {e}")))?;

    let chosen = device
        .supported_input_configs()
        .map(|iter| select_preferred_config(iter, default_cfg))
        .unwrap_or(default_cfg);

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
    let mut pending = Vec::<f32>::with_capacity(FRAME_SAMPLES * 2);

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            stream_cfg,
            move |data: &[f32], _| {
                ingest_captured_samples(data, dev_channels, dev_rate, &mut pending, &tx);
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        SampleFormat::I16 => device.build_input_stream(
            stream_cfg,
            move |data: &[i16], _| {
                let converted: Vec<f32> = data.iter().map(|&s| f32::from_sample(s)).collect();
                ingest_captured_samples(&converted, dev_channels, dev_rate, &mut pending, &tx);
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        SampleFormat::U16 => device.build_input_stream(
            stream_cfg,
            move |data: &[u16], _| {
                let converted: Vec<f32> = data.iter().map(|&s| f32::from_sample(s)).collect();
                ingest_captured_samples(&converted, dev_channels, dev_rate, &mut pending, &tx);
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

    let chosen = device
        .supported_output_configs()
        .map(|iter| select_preferred_config(iter, default_cfg))
        .unwrap_or(default_cfg);

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
    let mut scratch = Vec::<f32>::new();

    let stream = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            stream_cfg,
            move |out: &mut [f32], _| {
                render_playback_samples(&queue, out, dev_channels, dev_rate, &mut scratch);
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        SampleFormat::I16 => device.build_output_stream(
            stream_cfg,
            move |out: &mut [i16], _| {
                let mut f32_out = vec![0.0f32; out.len()];
                render_playback_samples(&queue, &mut f32_out, dev_channels, dev_rate, &mut scratch);
                for (dst, src) in out.iter_mut().zip(f32_out) {
                    *dst = i16::from_sample(src.clamp(-1.0, 1.0));
                }
            },
            err_fn,
            Some(Duration::from_secs(2)),
        ),
        SampleFormat::U16 => device.build_output_stream(
            stream_cfg,
            move |out: &mut [u16], _| {
                let mut f32_out = vec![0.0f32; out.len()];
                render_playback_samples(&queue, &mut f32_out, dev_channels, dev_rate, &mut scratch);
                for (dst, src) in out.iter_mut().zip(f32_out) {
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

/// Convert arbitrary device-rate/channel-count capture PCM into 48 kHz interleaved stereo 20 ms frames.
pub(crate) fn ingest_captured_samples(
    input: &[f32],
    src_channels: usize,
    src_rate: u32,
    pending_48k_stereo: &mut Vec<f32>,
    tx: &tokio_mpsc::Sender<Vec<f32>>,
) {
    if input.is_empty() || src_channels == 0 || src_rate == 0 {
        return;
    }

    let src_frames = input.len() / src_channels;
    if src_frames == 0 {
        return;
    }

    if src_rate == SAMPLE_RATE {
        for f in 0..src_frames {
            let base = f * src_channels;
            let left = input[base];
            let right = if src_channels >= 2 {
                input[base + 1]
            } else {
                left
            };
            pending_48k_stereo.push(left);
            pending_48k_stereo.push(right);
        }
    } else {
        let ratio = f64::from(src_rate) / f64::from(SAMPLE_RATE);
        let dst_frames = ((src_frames as f64) / ratio).round().max(1.0) as usize;
        for df in 0..dst_frames {
            let src_pos = (df as f64) * ratio;
            let idx0 = (src_pos.floor() as usize).min(src_frames.saturating_sub(1));
            let idx1 = (idx0 + 1).min(src_frames.saturating_sub(1));
            let frac = (src_pos - idx0 as f64) as f32;

            let l0 = input[idx0 * src_channels];
            let l1 = input[idx1 * src_channels];
            let r0 = if src_channels >= 2 {
                input[idx0 * src_channels + 1]
            } else {
                l0
            };
            let r1 = if src_channels >= 2 {
                input[idx1 * src_channels + 1]
            } else {
                l1
            };
            pending_48k_stereo.push(l0 + (l1 - l0) * frac);
            pending_48k_stereo.push(r0 + (r1 - r0) * frac);
        }
    }

    while pending_48k_stereo.len() >= FRAME_SAMPLES {
        let frame: Vec<f32> = pending_48k_stereo.drain(..FRAME_SAMPLES).collect();
        // Non-blocking send: drop if consumer stalled rather than blocking the RT audio callback.
        let _ = tx.try_send(frame);
    }
}

/// Drain 48 kHz interleaved stereo samples from `queue` and adapt to device channels & rate.
pub(crate) fn render_playback_samples(
    queue: &PlaybackQueue,
    out: &mut [f32],
    dst_channels: usize,
    dst_rate: u32,
    scratch: &mut Vec<f32>,
) {
    if out.is_empty() || dst_channels == 0 || dst_rate == 0 {
        return;
    }

    if dst_channels == CHANNELS && dst_rate == SAMPLE_RATE {
        queue.pop_into_nonblocking(out);
        return;
    }

    let dst_frames = out.len() / dst_channels;
    let src_frames = if dst_rate == SAMPLE_RATE {
        dst_frames
    } else {
        (((dst_frames as f64) * f64::from(SAMPLE_RATE) / f64::from(dst_rate))
            .round()
            .max(1.0)) as usize
    };
    let needed = src_frames * CHANNELS;
    scratch.resize(needed, 0.0);
    queue.pop_into_nonblocking(scratch);

    for df in 0..dst_frames {
        let sf = if dst_rate == SAMPLE_RATE {
            df
        } else {
            (((df as f64) * f64::from(SAMPLE_RATE) / f64::from(dst_rate)) as usize)
                .min(src_frames.saturating_sub(1))
        };
        let l = scratch[sf * 2];
        let r = scratch[sf * 2 + 1];
        let base = df * dst_channels;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn headless_audio_io_handle_starts_and_stops_cleanly() {
        let handle = AudioIoHandle::spawn(AudioBackendMode::Headless).expect("spawn headless");
        let (tx, _rx) = tokio_mpsc::channel(4);
        let cap_kind = handle.start_capture(tx).await.expect("start capture");
        assert_eq!(cap_kind, AudioBackendKind::Headless);
        assert!(!handle.is_capture_live());

        let queue = Arc::new(PlaybackQueue::new());
        let play_kind = handle
            .start_playback(Arc::clone(&queue))
            .await
            .expect("start playback");
        assert_eq!(play_kind, AudioBackendKind::Headless);
        assert!(!handle.is_playback_live());

        handle.stop_capture().await.expect("stop capture");
        handle.stop_playback().await.expect("stop playback");
    }

    #[test]
    fn playback_queue_and_mono_resample_bridge_work() {
        let queue = PlaybackQueue::new();
        let src: Vec<f32> = (0..FRAME_SAMPLES).map(|i| (i as f32) / 2000.0).collect();
        queue.push_samples(&src);
        assert_eq!(queue.buffered_samples(), FRAME_SAMPLES);

        let mut out = vec![0.0f32; FRAME_SAMPLES];
        let mut scratch = Vec::new();
        render_playback_samples(&queue, &mut out, CHANNELS, SAMPLE_RATE, &mut scratch);
        assert_eq!(out, src);
        assert_eq!(queue.buffered_samples(), 0);

        // Mono 48 kHz capture -> stereo 48 kHz 20 ms frame
        let (tx, mut rx) = tokio_mpsc::channel(4);
        let mut pending = Vec::new();
        let mono_in = vec![0.25f32; 960];
        ingest_captured_samples(&mono_in, 1, SAMPLE_RATE, &mut pending, &tx);
        let frame = rx.try_recv().expect("frame produced");
        assert_eq!(frame.len(), FRAME_SAMPLES);
        assert!((frame[0] - 0.25).abs() < f32::EPSILON);
        assert!((frame[1] - 0.25).abs() < f32::EPSILON);
    }
}
