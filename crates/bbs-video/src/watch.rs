//! Go Live screenshare watching pipelines with dual universal sinks (`PopoutWindow` & `InlineTerminal`).

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bbs_core::{CoreError, CoreResult, Snowflake, VideoFrame, VideoSinkKind};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::VideoFrameExt;
use gstreamer_webrtc as gst_webrtc;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::codec::{
    DecoderSelection, VideoCodec, ensure_gst_init, probe_best_decoder, probe_software_decoder,
};
use crate::dave::{DaveVideoContext, install_dave_decrypt_probe};

/// Default inline terminal frame rate cap (keeps Kitty/Sixel/HalfBlock rendering crisp & light).
pub const DEFAULT_INLINE_FPS: u8 = 12;
/// Default inline terminal max width in pixels.
pub const DEFAULT_INLINE_WIDTH: u32 = 640;
/// Default inline terminal max height in pixels.
pub const DEFAULT_INLINE_HEIGHT: u32 = 360;
/// Default pop-out window frame width in pixels.
pub const DEFAULT_POPOUT_WIDTH: u32 = 1280;
/// Default pop-out window frame height in pixels.
pub const DEFAULT_POPOUT_HEIGHT: u32 = 720;
/// Default pop-out window target framerate.
pub const DEFAULT_POPOUT_FPS: u8 = 60;

/// Configuration for a Go Live watch pipeline.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Canonical Go Live stream key (`guild:<g>:<c>:<u>` or `call:<c>:<u>`).
    pub stream_key: String,
    /// Window title displayed when using [`VideoSinkKind::PopoutWindow`].
    pub window_title: String,
    /// Broadcaster user Snowflake (used to select the sender's DAVE decryptor ratchet).
    pub sender_user_id: Snowflake,
    /// Expected incoming video codec (`H264`, `Vp8`, or `Vp9`).
    pub codec: VideoCodec,
    /// Output sink target (`PopoutWindow` or `InlineTerminal`).
    pub sink: VideoSinkKind,
    /// Target FPS for [`VideoSinkKind::InlineTerminal`] frame emission (`1..=60`).
    pub inline_target_fps: u8,
    /// Target downscaled width for [`VideoSinkKind::InlineTerminal`].
    pub inline_width: u32,
    /// Target downscaled height for [`VideoSinkKind::InlineTerminal`].
    pub inline_height: u32,
    /// Target width for [`VideoSinkKind::PopoutWindow`] raw `mpv` pipe.
    pub popout_width: u32,
    /// Target height for [`VideoSinkKind::PopoutWindow`] raw `mpv` pipe.
    pub popout_height: u32,
    /// Target FPS hint passed to `mpv --demuxer-rawvideo-fps`.
    pub popout_fps: u8,
    /// When `true`, enforce wall-clock FPS throttling in `InlineTerminal` mode inside the `AppSink` callback.
    pub enforce_wall_clock_throttle: bool,
    /// Prefer software decoder over hardware decoder (useful in deterministic headless tests).
    pub prefer_software_decoder: bool,
    /// Optional override binary for pop-out `mpv` (defaults to `"mpv"`).
    pub mpv_binary: String,
    /// DAVE E2EE decryption context shared with Stream RTC signaling.
    pub dave: DaveVideoContext,
}

impl WatchConfig {
    /// Create a sensible default [`WatchConfig`] for watching `stream_key` from `sender_user_id`.
    #[must_use]
    pub fn new(
        stream_key: impl Into<String>,
        sender_user_id: Snowflake,
        codec: VideoCodec,
        sink: VideoSinkKind,
        dave: DaveVideoContext,
    ) -> Self {
        let key = stream_key.into();
        let window_title = format!("bbscord Go Live — {key}");
        Self {
            stream_key: key,
            window_title,
            sender_user_id,
            codec,
            sink,
            inline_target_fps: DEFAULT_INLINE_FPS,
            inline_width: DEFAULT_INLINE_WIDTH,
            inline_height: DEFAULT_INLINE_HEIGHT,
            popout_width: DEFAULT_POPOUT_WIDTH,
            popout_height: DEFAULT_POPOUT_HEIGHT,
            popout_fps: DEFAULT_POPOUT_FPS,
            enforce_wall_clock_throttle: true,
            prefer_software_decoder: false,
            mpv_binary: "mpv".to_string(),
            dave,
        }
    }
}

/// Active backend chosen for rendering a watched Go Live stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveWatchSink {
    /// Borderless low-latency `mpv` child fed over an anonymous stdin pipe.
    MpvPopoutPipe,
    /// Fallback GStreamer `autovideosink` pop-out window.
    GstAutoVideoSink,
    /// Inline terminal RGBA `AppSink` emitting [`VideoFrame`] events to `bbs-core`.
    InlineTerminalAppSink,
}

/// Low-latency borderless `mpv` child process fed raw RGBA frames over an anonymous stdin pipe.
///
/// Always configured with `kill_on_drop(true)` and `stdout`/`stderr` redirected to `/dev/null`
/// so it never corrupts the active terminal UI or leaves orphan processes behind.
pub struct MpvPopoutSink {
    frame_tx: mpsc::Sender<Vec<u8>>,
    alive: Arc<AtomicBool>,
    child: Arc<tokio::sync::Mutex<Option<Child>>>,
    writer_task: Option<tokio::task::JoinHandle<()>>,
    frames_written: Arc<AtomicU64>,
}

impl std::fmt::Debug for MpvPopoutSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpvPopoutSink")
            .field("alive", &self.alive.load(Ordering::Relaxed))
            .field("frames_written", &self.frames_written.load(Ordering::Relaxed))
            .finish()
    }
}

impl MpvPopoutSink {
    /// Build the exact CLI argument list for borderless low-latency raw-RGBA `mpv` playback.
    #[must_use]
    pub fn build_mpv_args(width: u32, height: u32, fps: u8, title: &str) -> Vec<String> {
        vec![
            "--no-config".to_string(),
            "--no-terminal".to_string(),
            "--really-quiet".to_string(),
            "--no-border".to_string(),
            "--no-osc".to_string(),
            "--no-audio".to_string(),
            "--profile=low-latency".to_string(),
            "--untimed".to_string(),
            "--cache=no".to_string(),
            "--demuxer=rawvideo".to_string(),
            format!("--demuxer-rawvideo-w={}", width.max(2)),
            format!("--demuxer-rawvideo-h={}", height.max(2)),
            "--demuxer-rawvideo-mp-format=rgba".to_string(),
            format!("--demuxer-rawvideo-fps={}", fps.max(1)),
            format!("--title={title}"),
            "-".to_string(),
        ]
    }

    /// Spawn a borderless `mpv` child process consuming raw RGBA frames from an anonymous pipe.
    pub fn spawn(
        mpv_binary: &str,
        width: u32,
        height: u32,
        fps: u8,
        title: &str,
    ) -> CoreResult<Self> {
        let args = Self::build_mpv_args(width, height, fps, title);
        Self::spawn_with_args(mpv_binary, &args)
    }

    /// Spawn a child process with custom arguments feeding its stdin over an anonymous pipe.
    ///
    /// Useful for both production `mpv` launching and deterministic headless lifecycle tests.
    pub fn spawn_with_args(binary: &str, args: &[String]) -> CoreResult<Self> {
        let mut cmd = Command::new(binary);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|err| {
            CoreError::Video(format!("failed to spawn pop-out player `{binary}`: {err}"))
        })?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            CoreError::Video(format!(
                "pop-out player `{binary}` did not expose stdin pipe"
            ))
        })?;

        // Bounded queue of 2 frames ensures low latency and non-blocking backpressure.
        let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(2);
        let alive = Arc::new(AtomicBool::new(true));
        let alive_writer = Arc::clone(&alive);
        let frames_written = Arc::new(AtomicU64::new(0));
        let written_writer = Arc::clone(&frames_written);
        let child_arc = Arc::new(tokio::sync::Mutex::new(Some(child)));
        let child_writer = Arc::clone(&child_arc);

        let writer_task = tokio::spawn(async move {
            while let Some(frame_bytes) = frame_rx.recv().await {
                if let Err(err) = stdin.write_all(&frame_bytes).await {
                    debug!(error = %err, "pop-out player stdin pipe closed");
                    break;
                }
                written_writer.fetch_add(1, Ordering::Relaxed);
            }
            let _ = stdin.shutdown().await;
            alive_writer.store(false, Ordering::Relaxed);
            if let Some(mut c) = child_writer.lock().await.take() {
                let _ = c.start_kill();
                let _ = c.wait().await;
            }
        });

        Ok(Self {
            frame_tx,
            alive,
            child: child_arc,
            writer_task: Some(writer_task),
            frames_written,
        })
    }

    /// Non-blocking enqueue of a tightly packed RGBA frame for the pop-out pipe writer.
    ///
    /// Drops the frame cleanly if the player pipe is momentarily saturated so the GStreamer
    /// streaming thread never stalls.
    pub fn try_push_rgba(&self, rgba: Vec<u8>) -> bool {
        if !self.alive.load(Ordering::Relaxed) {
            return false;
        }
        self.frame_tx.try_send(rgba).is_ok()
    }

    /// Return whether the pop-out child pipe is still accepting frames.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Number of raw RGBA frames written into the pop-out pipe so far.
    #[must_use]
    pub fn frames_written(&self) -> u64 {
        self.frames_written.load(Ordering::Relaxed)
    }

    /// Terminate and reap the child process cleanly.
    pub async fn stop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        if let Some(handle) = self.writer_task.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for MpvPopoutSink {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Ok(mut guard) = self.child.try_lock()
            && let Some(ref mut child) = *guard
        {
            let _ = child.start_kill();
        }
        if let Some(handle) = self.writer_task.take() {
            handle.abort();
        }
    }
}

/// Running Go Live watch pipeline supporting both WebRTC RTP reception and encoded `AppSrc` input.
pub struct WatchPipeline {
    pipeline: gst::Pipeline,
    webrtcbin: Option<gst::Element>,
    appsrc: Option<gst_app::AppSrc>,
    active_sink: ActiveWatchSink,
    decoder_selection: DecoderSelection,
    mpv_sink: Option<Arc<Mutex<MpvPopoutSink>>>,
    frames_emitted: Arc<AtomicU64>,
    stream_key: String,
    stopped: AtomicBool,
}

impl std::fmt::Debug for WatchPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchPipeline")
            .field("stream_key", &self.stream_key)
            .field("active_sink", &self.active_sink)
            .field("decoder", &self.decoder_selection)
            .field("frames_emitted", &self.frames_emitted.load(Ordering::Relaxed))
            .finish()
    }
}

impl WatchPipeline {
    /// Start a live WebRTC watch pipeline (`webrtcbin` -> RTP depayload -> DAVE decrypt ->
    /// HW/SW decode -> convert/scale -> `PopoutWindow` or `InlineTerminal`).
    pub fn start_webrtc(
        config: WatchConfig,
        frames_tx: broadcast::Sender<VideoFrame>,
    ) -> CoreResult<Self> {
        ensure_gst_init()?;
        let decoder_selection = select_decoder(config.codec, config.prefer_software_decoder)?;

        let pipeline = gst::Pipeline::builder()
            .name(format!("watch_webrtc_{}", sanitize_gst_name(&config.stream_key)))
            .build();

        let webrtcbin = gst::ElementFactory::make("webrtcbin")
            .name("watch_webrtcbin")
            .build()
            .map_err(|err| CoreError::Video(format!("failed to create webrtcbin: {err}")))?;
        if webrtcbin.find_property("bundle-policy").is_some() {
            webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
        }

        let sink_branch =
            build_sink_branch(&pipeline, &config, frames_tx.clone(), Arc::new(AtomicU64::new(0)))?;

        pipeline
            .add(&webrtcbin)
            .map_err(|err| CoreError::Video(format!("failed to add webrtcbin: {err}")))?;

        // Register a Recvonly video transceiver so `create_webrtc_offer()` emits an `m=video`
        // section with `a=recvonly` for Discord's Stream RTC `SELECT_PROTOCOL` handshake.
        let recv_video_caps = config
            .codec
            .rtp_caps_str(config.codec.default_payload_type())
            .parse::<gst::Caps>()
            .map_err(|err| CoreError::Video(format!("invalid recvonly RTP video caps: {err}")))?;
        let _transceiver = webrtcbin.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
            "add-transceiver",
            &[
                &gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly,
                &recv_video_caps,
            ],
        );

        let active_sink = sink_branch.active_sink;
        let mpv_sink = sink_branch.mpv_sink;
        let frames_emitted = sink_branch.frames_emitted.clone();
        let pipeline_weak = pipeline.downgrade();
        let dave = config.dave.clone();
        let sender_user_id = config.sender_user_id;
        let prefer_sw = config.prefer_software_decoder;
        let default_codec = config.codec;
        let sink_entry_elem = sink_branch.entry_element;
        let linked = Arc::new(AtomicBool::new(false));
        let fakesink_seq = Arc::new(AtomicU64::new(0));

        webrtcbin.connect_pad_added(move |_webrtc, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };

            let caps = pad.current_caps().or_else(|| Some(pad.query_caps(None)));
            let (is_video, detected_codec) = classify_rtp_pad_caps(caps.as_ref());

            if !is_video {
                debug!(
                    pad = %pad.name(),
                    "routing non-video incoming WebRTC RTP pad to fakesink"
                );
                let idx = fakesink_seq.fetch_add(1, Ordering::Relaxed);
                if let Err(err) = attach_fakesink_for_pad(&pipeline, pad, idx) {
                    warn!(error = %err, pad = %pad.name(), "failed to park non-video WebRTC pad");
                }
                return;
            }

            if linked.swap(true, Ordering::AcqRel) {
                debug!(
                    pad = %pad.name(),
                    "routing secondary incoming WebRTC video pad to fakesink"
                );
                let idx = fakesink_seq.fetch_add(1, Ordering::Relaxed);
                if let Err(err) = attach_fakesink_for_pad(&pipeline, pad, idx) {
                    warn!(error = %err, pad = %pad.name(), "failed to park secondary video pad");
                }
                return;
            }

            let codec = detected_codec.unwrap_or(default_codec);

            if let Err(err) = attach_rtp_decrypt_decode_chain(
                &pipeline,
                pad,
                codec,
                prefer_sw,
                dave.clone(),
                sender_user_id,
                &sink_entry_elem,
            ) {
                warn!(error = %err, codec = ?codec, "failed to link incoming WebRTC video pad");
                linked.store(false, Ordering::Release);
            }
        });

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|err| CoreError::Video(format!("failed to start watch pipeline: {err}")))?;

        info!(
            stream_key = %config.stream_key,
            sink = ?active_sink,
            decoder = decoder_selection.element_name,
            "started Go Live WebRTC watch pipeline"
        );

        Ok(Self {
            pipeline,
            webrtcbin: Some(webrtcbin),
            appsrc: None,
            active_sink,
            decoder_selection,
            mpv_sink,
            frames_emitted,
            stream_key: config.stream_key,
            stopped: AtomicBool::new(false),
        })
    }

    /// Start an `AppSrc`-fed watch pipeline (`appsrc` -> DAVE decrypt -> HW/SW decode ->
    /// convert/scale -> `PopoutWindow` or `InlineTerminal`).
    ///
    /// Shares the exact same DAVE decryption, decoder fallback, stride-safe RGBA conversion,
    /// and dual-sink delivery stages as live WebRTC reception — ideal for loopback testing and
    /// external RTP depacketizers.
    pub fn start_appsrc(
        config: WatchConfig,
        frames_tx: broadcast::Sender<VideoFrame>,
    ) -> CoreResult<Self> {
        ensure_gst_init()?;
        let decoder_selection = select_decoder(config.codec, config.prefer_software_decoder)?;

        let pipeline = gst::Pipeline::builder()
            .name(format!("watch_appsrc_{}", sanitize_gst_name(&config.stream_key)))
            .build();

        let encoded_caps = config.codec.encoded_caps_str().parse::<gst::Caps>().map_err(|err| {
            CoreError::Video(format!("invalid encoded caps for {:?}: {err}", config.codec))
        })?;

        let appsrc = gst_app::AppSrc::builder()
            .name("watch_encoded_src")
            .caps(&encoded_caps)
            .format(gst::Format::Time)
            .build();

        let dave_decrypt = gst::ElementFactory::make("identity")
            .name("dave_decrypt")
            .build()
            .map_err(|err| CoreError::Video(format!("failed to create identity element: {err}")))?;
        let _probe_id = install_dave_decrypt_probe(&dave_decrypt, config.dave.clone(), config.sender_user_id)?;

        let decoder = gst::ElementFactory::make(decoder_selection.element_name)
            .name("watch_video_decoder")
            .build()
            .map_err(|err| {
                CoreError::Video(format!(
                    "failed to create decoder `{}`: {err}",
                    decoder_selection.element_name
                ))
            })?;

        let frames_counter = Arc::new(AtomicU64::new(0));
        let sink_branch =
            build_sink_branch(&pipeline, &config, frames_tx, Arc::clone(&frames_counter))?;
        let active_sink = sink_branch.active_sink;
        let mpv_sink = sink_branch.mpv_sink;

        let appsrc_elem = appsrc.upcast_ref::<gst::Element>();
        pipeline
            .add_many([appsrc_elem, &dave_decrypt, &decoder])
            .map_err(|err| CoreError::Video(format!("failed to add appsrc watch elements: {err}")))?;

        gst::Element::link_many([appsrc_elem, &dave_decrypt, &decoder, &sink_branch.entry_element])
            .map_err(|err| {
                CoreError::Video(format!("failed to link appsrc watch pipeline: {err}"))
            })?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|err| CoreError::Video(format!("failed to start appsrc watch pipeline: {err}")))?;

        Ok(Self {
            pipeline,
            webrtcbin: None,
            appsrc: Some(appsrc),
            active_sink,
            decoder_selection,
            mpv_sink,
            frames_emitted: frames_counter,
            stream_key: config.stream_key,
            stopped: AtomicBool::new(false),
        })
    }

    /// Push a DAVE-encrypted (or passthrough) encoded video frame into an `AppSrc`-backed watch pipeline.
    pub fn push_encoded_frame(
        &self,
        payload: Vec<u8>,
        pts: Option<gst::ClockTime>,
        duration: Option<gst::ClockTime>,
    ) -> CoreResult<()> {
        let appsrc = self.appsrc.as_ref().ok_or_else(|| {
            CoreError::Video("push_encoded_frame requires an AppSrc watch pipeline".to_string())
        })?;
        let mut buf = gst::Buffer::from_mut_slice(payload);
        if let Some(buf_ref) = buf.get_mut() {
            buf_ref.set_pts(pts);
            buf_ref.set_duration(duration);
        }
        appsrc
            .push_buffer(buf)
            .map(|_| ())
            .map_err(|err| CoreError::Video(format!("appsrc push_buffer failed: {err:?}")))
    }

    /// Signal end-of-stream on an `AppSrc`-backed watch pipeline.
    pub fn end_of_stream(&self) -> CoreResult<()> {
        let appsrc = self.appsrc.as_ref().ok_or_else(|| {
            CoreError::Video("end_of_stream requires an AppSrc watch pipeline".to_string())
        })?;
        appsrc
            .end_of_stream()
            .map(|_| ())
            .map_err(|err| CoreError::Video(format!("appsrc end_of_stream failed: {err:?}")))
    }

    /// Wait for the pipeline bus to observe EOS or an error (blocking helper intended for
    /// `spawn_blocking` or synchronous test runners).
    pub fn wait_for_eos_blocking(&self, timeout: Duration) -> CoreResult<()> {
        let Some(bus) = self.pipeline.bus() else {
            return Ok(());
        };
        let gst_timeout = gst::ClockTime::from_nseconds(timeout.as_nanos().min(u64::MAX as u128) as u64);
        match bus.timed_pop_filtered(
            Some(gst_timeout),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(_) => Ok(()),
                gst::MessageView::Error(err) => Err(CoreError::Video(format!(
                    "watch pipeline bus error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                ))),
                _ => Ok(()),
            },
            None => Err(CoreError::Video(format!(
                "watch pipeline timed out waiting for EOS after {timeout:?}"
            ))),
        }
    }

    /// Create a local WebRTC SDP offer on this watch pipeline's `webrtcbin` (non-blocking).
    pub async fn create_webrtc_offer(&self) -> CoreResult<String> {
        let webrtc = self.webrtcbin.as_ref().ok_or_else(|| {
            CoreError::Video("create_webrtc_offer requires a WebRTC watch pipeline".to_string())
        })?;
        webrtc_create_sdp(webrtc, "create-offer", "offer").await
    }

    /// Apply a remote SDP offer or answer to `webrtcbin` (non-blocking).
    pub async fn set_remote_sdp(
        &self,
        sdp_type: gst_webrtc::WebRTCSDPType,
        sdp_text: &str,
    ) -> CoreResult<()> {
        let webrtc = self.webrtcbin.as_ref().ok_or_else(|| {
            CoreError::Video("set_remote_sdp requires a WebRTC watch pipeline".to_string())
        })?;
        webrtc_set_remote_sdp(webrtc, sdp_type, sdp_text).await
    }

    /// Create a local WebRTC SDP answer after applying a remote offer (non-blocking).
    pub async fn create_webrtc_answer(&self) -> CoreResult<String> {
        let webrtc = self.webrtcbin.as_ref().ok_or_else(|| {
            CoreError::Video("create_webrtc_answer requires a WebRTC watch pipeline".to_string())
        })?;
        webrtc_create_sdp(webrtc, "create-answer", "answer").await
    }

    /// Feed a remote ICE candidate (`m_line_index`, `candidate`) into `webrtcbin`.
    pub fn add_ice_candidate(&self, mline_index: u32, candidate: &str) -> CoreResult<()> {
        let webrtc = self.webrtcbin.as_ref().ok_or_else(|| {
            CoreError::Video("add_ice_candidate requires a WebRTC watch pipeline".to_string())
        })?;
        webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline_index, &candidate]);
        Ok(())
    }

    /// Return which output sink backend is active (`MpvPopoutPipe`, `GstAutoVideoSink`, or `InlineTerminalAppSink`).
    #[must_use]
    pub const fn active_sink(&self) -> ActiveWatchSink {
        self.active_sink
    }

    /// Return the dynamically selected video decoder.
    #[must_use]
    pub const fn decoder_selection(&self) -> &DecoderSelection {
        &self.decoder_selection
    }

    /// Return the total number of RGBA frames emitted to the inline sink or pop-out pipe.
    #[must_use]
    pub fn frames_emitted(&self) -> u64 {
        self.frames_emitted.load(Ordering::Relaxed)
    }

    /// Gracefully stop the GStreamer pipeline and terminate any `mpv` pop-out child without
    /// blocking the Tokio executor.
    pub async fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        let pipeline = self.pipeline.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = pipeline.set_state(gst::State::Null);
        })
        .await;

        if let Some(mpv_arc) = self.mpv_sink.take() {
            let extracted = mpv_arc.lock().ok().map(|mut guard| {
                guard.alive.store(false, Ordering::Relaxed);
                (guard.writer_task.take(), Arc::clone(&guard.child))
            });
            if let Some((writer_handle, child_arc)) = extracted {
                if let Some(mut child) = child_arc.lock().await.take() {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
                if let Some(handle) = writer_handle {
                    handle.abort();
                    let _ = handle.await;
                }
            }
        }
    }
}

impl Drop for WatchPipeline {
    fn drop(&mut self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
        if let Some(mpv_arc) = self.mpv_sink.take()
            && let Ok(mut guard) = mpv_arc.lock()
        {
            guard.alive.store(false, Ordering::Relaxed);
            if let Ok(mut child_guard) = guard.child.try_lock()
                && let Some(ref mut child) = *child_guard
            {
                let _ = child.start_kill();
            }
            if let Some(handle) = guard.writer_task.take() {
                handle.abort();
            }
        }
    }
}

struct SinkBranch {
    active_sink: ActiveWatchSink,
    mpv_sink: Option<Arc<Mutex<MpvPopoutSink>>>,
    entry_element: gst::Element,
    frames_emitted: Arc<AtomicU64>,
}

fn build_sink_branch(
    pipeline: &gst::Pipeline,
    config: &WatchConfig,
    frames_tx: broadcast::Sender<VideoFrame>,
    frames_emitted: Arc<AtomicU64>,
) -> CoreResult<SinkBranch> {
    let videoconvert = gst::ElementFactory::make("videoconvert")
        .name("watch_videoconvert")
        .build()
        .map_err(|err| CoreError::Video(format!("failed to create videoconvert: {err}")))?;
    let videoscale = gst::ElementFactory::make("videoscale")
        .name("watch_videoscale")
        .build()
        .map_err(|err| CoreError::Video(format!("failed to create videoscale: {err}")))?;

    match config.sink {
        VideoSinkKind::PopoutWindow => {
            match MpvPopoutSink::spawn(
                &config.mpv_binary,
                config.popout_width,
                config.popout_height,
                config.popout_fps,
                &config.window_title,
            ) {
                Ok(mpv) => {
                    let mpv_arc = Arc::new(Mutex::new(mpv));
                    let caps = gst_video::VideoCapsBuilder::new()
                        .format(gst_video::VideoFormat::Rgba)
                        .width(config.popout_width.max(2) as i32)
                        .height(config.popout_height.max(2) as i32)
                        .build();

                    let appsink = gst_app::AppSink::builder()
                        .name("watch_mpv_pipe_sink")
                        .caps(&caps)
                        .sync(false)
                        .max_buffers(2)
                        .drop(true)
                        .build();

                    let mpv_cb = Arc::clone(&mpv_arc);
                    let emitted_cb = Arc::clone(&frames_emitted);
                    appsink.set_callbacks(
                        gst_app::AppSinkCallbacks::builder()
                            .new_sample(move |sink| {
                                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                                if let Ok((_w, _h, rgba)) = extract_packed_rgba(&sample)
                                    && let Ok(guard) = mpv_cb.lock()
                                    && guard.try_push_rgba(rgba)
                                {
                                    emitted_cb.fetch_add(1, Ordering::Relaxed);
                                }
                                Ok(gst::FlowSuccess::Ok)
                            })
                            .build(),
                    );

                    let appsink_elem = appsink.upcast_ref::<gst::Element>();
                    pipeline
                        .add_many([&videoconvert, &videoscale, appsink_elem])
                        .map_err(|err| {
                            CoreError::Video(format!("failed to add mpv sink branch: {err}"))
                        })?;
                    gst::Element::link_many([&videoconvert, &videoscale, appsink_elem]).map_err(
                        |err| CoreError::Video(format!("failed to link mpv sink branch: {err}")),
                    )?;

                    Ok(SinkBranch {
                        active_sink: ActiveWatchSink::MpvPopoutPipe,
                        mpv_sink: Some(mpv_arc),
                        entry_element: videoconvert,
                        frames_emitted,
                    })
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        "mpv pop-out unavailable; falling back to GStreamer autovideosink"
                    );
                    let autovideosink = gst::ElementFactory::make("autovideosink")
                        .name("watch_autovideosink")
                        .build()
                        .map_err(|gst_err| {
                            CoreError::Video(format!(
                                "mpv unavailable ({err}) and autovideosink failed: {gst_err}"
                            ))
                        })?;
                    if autovideosink.find_property("sync").is_some() {
                        autovideosink.set_property("sync", false);
                    }
                    pipeline
                        .add_many([&videoconvert, &videoscale, &autovideosink])
                        .map_err(|e| {
                            CoreError::Video(format!("failed to add autovideosink branch: {e}"))
                        })?;
                    gst::Element::link_many([&videoconvert, &videoscale, &autovideosink]).map_err(
                        |e| CoreError::Video(format!("failed to link autovideosink branch: {e}")),
                    )?;

                    Ok(SinkBranch {
                        active_sink: ActiveWatchSink::GstAutoVideoSink,
                        mpv_sink: None,
                        entry_element: videoconvert,
                        frames_emitted,
                    })
                }
            }
        }
        VideoSinkKind::InlineTerminal => {
            let target_fps = config.inline_target_fps.clamp(1, 60);
            let caps = gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
                .width(config.inline_width.max(2) as i32)
                .height(config.inline_height.max(2) as i32)
                .build();

            let appsink = gst_app::AppSink::builder()
                .name("watch_inline_appsink")
                .caps(&caps)
                .sync(false)
                .max_buffers(4)
                .drop(false)
                .build();

            let stream_key = config.stream_key.clone();
            let emitted_cb = Arc::clone(&frames_emitted);
            let min_interval = Duration::from_secs_f64(1.0 / f64::from(target_fps));
            let enforce_wall_clock = config.enforce_wall_clock_throttle;
            let last_emit: Mutex<Option<Instant>> = Mutex::new(None);
            let seq_counter = Arc::new(AtomicU64::new(0));

            appsink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        if enforce_wall_clock
                            && let Ok(mut guard) = last_emit.lock()
                        {
                            let now = Instant::now();
                            if guard.is_some_and(|prev| now.duration_since(prev) < min_interval) {
                                return Ok(gst::FlowSuccess::Ok);
                            }
                            *guard = Some(now);
                        }

                        let (width, height, rgba) = match extract_packed_rgba(&sample) {
                            Ok(frame) => frame,
                            Err(err) => {
                                warn!(error = %err, "failed to extract RGBA video frame");
                                return Ok(gst::FlowSuccess::Ok);
                            }
                        };

                        let timestamp_ms = sample
                            .buffer()
                            .and_then(gst::BufferRef::pts)
                            .map_or(0, gst::ClockTime::mseconds);
                        let seq = seq_counter.fetch_add(1, Ordering::Relaxed) + 1;
                        emitted_cb.fetch_add(1, Ordering::Relaxed);

                        let _ = frames_tx.send(VideoFrame {
                            stream_key: stream_key.clone(),
                            seq,
                            timestamp_ms,
                            width,
                            height,
                            rgba,
                        });
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );

            let appsink_elem = appsink.upcast_ref::<gst::Element>();
            pipeline
                .add_many([&videoconvert, &videoscale, appsink_elem])
                .map_err(|err| CoreError::Video(format!("failed to add inline sink branch: {err}")))?;
            gst::Element::link_many([&videoconvert, &videoscale, appsink_elem]).map_err(
                |err| CoreError::Video(format!("failed to link inline sink branch: {err}")),
            )?;

            Ok(SinkBranch {
                active_sink: ActiveWatchSink::InlineTerminalAppSink,
                mpv_sink: None,
                entry_element: videoconvert,
                frames_emitted,
            })
        }
    }
}

/// Inspect incoming `webrtcbin` RTP pad caps to determine whether the pad carries video and
/// which [`VideoCodec`] was negotiated.
fn classify_rtp_pad_caps(caps: Option<&gst::Caps>) -> (bool, Option<VideoCodec>) {
    let Some(structure) = caps.and_then(|c| c.structure(0)) else {
        return (false, None);
    };
    let media = structure.get::<String>("media").ok();
    let encoding = structure.get::<String>("encoding-name").ok();
    let codec = encoding.as_deref().and_then(VideoCodec::from_sdp_name);
    let is_video = matches!(media.as_deref(), Some("video"));
    (is_video, codec)
}

/// Route an auxiliary (e.g., audio or secondary simulcast) `webrtcbin` src pad to a `fakesink`
/// so `webrtcbin` never stalls with `GST_FLOW_NOT_LINKED`.
fn attach_fakesink_for_pad(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    seq: u64,
) -> CoreResult<()> {
    let sink = gst::ElementFactory::make("fakesink")
        .name(format!("watch_aux_fakesink_{seq}"))
        .build()
        .map_err(|err| CoreError::Video(format!("failed to create aux fakesink: {err}")))?;
    if sink.find_property("sync").is_some() {
        sink.set_property("sync", false);
    }
    if sink.find_property("async").is_some() {
        sink.set_property("async", false);
    }

    pipeline
        .add(&sink)
        .map_err(|err| CoreError::Video(format!("failed to add aux fakesink: {err}")))?;

    let link_res = (|| -> CoreResult<()> {
        let sink_pad = sink
            .static_pad("sink")
            .ok_or_else(|| CoreError::Video("aux fakesink missing static sink pad".to_string()))?;
        src_pad
            .link(&sink_pad)
            .map_err(|err| CoreError::Video(format!("failed to link aux pad to fakesink: {err}")))?;
        sink.sync_state_with_parent()
            .map_err(|err| CoreError::Video(format!("failed to sync aux fakesink state: {err}")))?;
        Ok(())
    })();

    if let Err(err) = link_res {
        let _ = sink.set_state(gst::State::Null);
        let _ = pipeline.remove(&sink);
        return Err(err);
    }
    Ok(())
}

fn rollback_added_elements(
    pipeline: &gst::Pipeline,
    elements: &[gst::Element],
    sink_entry: &gst::Element,
) {
    if let Some(last) = elements.last() {
        last.unlink(sink_entry);
    }
    for pair in elements.windows(2) {
        pair[0].unlink(&pair[1]);
    }
    for elem in elements {
        let _ = elem.set_state(gst::State::Null);
        let _ = pipeline.remove(elem);
    }
}

fn attach_rtp_decrypt_decode_chain(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    codec: VideoCodec,
    prefer_software: bool,
    dave: DaveVideoContext,
    sender_user_id: Snowflake,
    sink_entry: &gst::Element,
) -> CoreResult<()> {
    let depay = gst::ElementFactory::make(codec.rtp_depayloader_name())
        .name("watch_rtp_depay")
        .build()
        .map_err(|err| {
            CoreError::Video(format!(
                "failed to create RTP depayloader `{}`: {err}",
                codec.rtp_depayloader_name()
            ))
        })?;
    if codec == VideoCodec::H264 {
        install_h264_strip_sprop_probe(&depay)?;
    }

    let capsfilter = gst::ElementFactory::make("capsfilter")
        .name("watch_encoded_caps")
        .build()
        .map_err(|err| CoreError::Video(format!("failed to create capsfilter: {err}")))?;
    let caps = codec.encoded_caps_str().parse::<gst::Caps>().map_err(|err| {
        CoreError::Video(format!("invalid encoded caps for {codec:?}: {err}"))
    })?;
    capsfilter.set_property("caps", &caps);

    let dave_decrypt = gst::ElementFactory::make("identity")
        .name("dave_decrypt")
        .build()
        .map_err(|err| CoreError::Video(format!("failed to create identity element: {err}")))?;
    let _probe_id = install_dave_decrypt_probe(&dave_decrypt, dave, sender_user_id)?;

    let dec_sel = select_decoder(codec, prefer_software)?;
    let decoder = gst::ElementFactory::make(dec_sel.element_name)
        .name("watch_dynamic_decoder")
        .build()
        .map_err(|err| {
            CoreError::Video(format!(
                "failed to create decoder `{}`: {err}",
                dec_sel.element_name
            ))
        })?;

    let chain = [depay, capsfilter, dave_decrypt, decoder];
    let refs: [&gst::Element; 4] = [&chain[0], &chain[1], &chain[2], &chain[3]];

    pipeline
        .add_many(refs)
        .map_err(|err| CoreError::Video(format!("failed to add RTP decode chain: {err}")))?;

    let attach_result = (|| -> CoreResult<()> {
        let depay_sink_pad = chain[0].static_pad("sink").ok_or_else(|| {
            CoreError::Video("RTP depayloader missing static sink pad".to_string())
        })?;

        // Link upstream `webrtcbin` pad -> `depay` FIRST so caps mismatches fail cleanly before
        // touching `sink_entry`.
        src_pad.link(&depay_sink_pad).map_err(|err| {
            CoreError::Video(format!("failed to link webrtcbin pad to depay: {err}"))
        })?;

        gst::Element::link_many([&chain[0], &chain[1], &chain[2], &chain[3], sink_entry]).map_err(
            |err| CoreError::Video(format!("failed to link RTP decode chain: {err}")),
        )?;

        for elem in &chain {
            elem.sync_state_with_parent().map_err(|err| {
                CoreError::Video(format!(
                    "failed to sync state of `{}`: {err}",
                    elem.name()
                ))
            })?;
        }
        Ok(())
    })();

    if let Err(err) = attach_result {
        if let Some(depay_sink_pad) = chain[0].static_pad("sink") {
            let _ = src_pad.unlink(&depay_sink_pad);
        }
        rollback_added_elements(pipeline, &chain, sink_entry);
        return Err(err);
    }

    info!(
        codec = ?codec,
        decoder = dec_sel.element_name,
        hardware = dec_sel.hardware_accelerated,
        "linked incoming Go Live RTP stream through DAVE decryptor"
    );
    Ok(())
}

/// Strip `sprop-parameter-sets` from incoming H.264 RTP caps on `rtph264depay`'s sink pad so
/// `rtph264depay` never duplicates out-of-band SPS/PPS ahead of the in-band DAVE-authenticated
/// IDR Access Unit (which would otherwise shift DAVE's `unencrypted_ranges` byte offsets).
pub(crate) fn install_h264_strip_sprop_probe(depay: &gst::Element) -> CoreResult<()> {
    let sink_pad = depay.static_pad("sink").ok_or_else(|| {
        CoreError::Video("rtph264depay missing static sink pad".to_string())
    })?;
    sink_pad
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |_pad, info| {
            let Some(gst::PadProbeData::Event(ref mut event)) = info.data else {
                return gst::PadProbeReturn::Ok;
            };
            if let gst::EventView::Caps(caps_ev) = event.view() {
                let mut caps = caps_ev.caps().to_owned();
                let needs_strip = caps
                    .structure(0)
                    .is_some_and(|s| s.has_field("sprop-parameter-sets"));
                if needs_strip {
                    if let Some(structure) = caps.make_mut().structure_mut(0) {
                        structure.remove_field("sprop-parameter-sets");
                    }
                    *event = gst::event::Caps::new(&caps);
                }
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| {
            CoreError::Video("failed to install H.264 sprop strip probe".to_string())
        })?;
    Ok(())
}

/// Extract tightly packed RGBA8 pixels (`width * height * 4`) from a GStreamer `Sample`,
/// handling arbitrary decoder row strides safely.
pub fn extract_packed_rgba(sample: &gst::Sample) -> CoreResult<(u32, u32, Vec<u8>)> {
    let caps = sample
        .caps()
        .ok_or_else(|| CoreError::Video("GStreamer sample missing caps".to_string()))?;
    let buffer = sample
        .buffer()
        .ok_or_else(|| CoreError::Video("GStreamer sample missing buffer".to_string()))?;
    let info = gst_video::VideoInfo::from_caps(caps)
        .map_err(|err| CoreError::Video(format!("failed to parse VideoInfo from caps: {err}")))?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|_| CoreError::Video("failed to map GStreamer video frame readable".to_string()))?;

    let width = info.width();
    let height = info.height();
    let row_bytes = (width as usize).saturating_mul(4);
    let expected_total = row_bytes.saturating_mul(height as usize);
    let stride = usize::try_from(frame.plane_stride()[0])
        .map_err(|_| CoreError::Video("negative RGBA video frame stride unsupported".to_string()))?;
    let plane = frame
        .plane_data(0)
        .map_err(|_| CoreError::Video("missing RGBA plane 0 in GStreamer frame".to_string()))?;

    let mut rgba = Vec::with_capacity(expected_total);
    if stride == row_bytes && plane.len() >= expected_total {
        rgba.extend_from_slice(&plane[..expected_total]);
    } else {
        for row in 0..(height as usize) {
            let start = row.saturating_mul(stride);
            let end = start.saturating_add(row_bytes);
            if end > plane.len() {
                return Err(CoreError::Video(format!(
                    "video plane truncated at row {row} (need {end}, have {})",
                    plane.len()
                )));
            }
            rgba.extend_from_slice(&plane[start..end]);
        }
    }

    Ok((width, height, rgba))
}

/// Shared helper to create an SDP offer or answer from a GStreamer `webrtcbin` without blocking Tokio.
pub(crate) async fn webrtc_create_sdp(
    webrtc: &gst::Element,
    signal_name: &str,
    reply_field: &'static str,
) -> CoreResult<String> {
    let (tx, rx) = oneshot::channel::<CoreResult<String>>();
    let tx_slot = Arc::new(Mutex::new(Some(tx)));
    let webrtc_clone = webrtc.clone();

    let promise = gst::Promise::with_change_func(move |reply| {
        let outcome = match reply {
            Ok(Some(structure)) => match structure.get::<gst_webrtc::WebRTCSessionDescription>(reply_field) {
                Ok(desc) => {
                    webrtc_clone.emit_by_name::<()>(
                        "set-local-description",
                        &[&desc, &None::<gst::Promise>],
                    );
                    Ok(desc.sdp().as_text().unwrap_or_default())
                }
                Err(err) => Err(CoreError::Video(format!(
                    "webrtcbin promise missing `{reply_field}`: {err}"
                ))),
            },
            Ok(None) => Err(CoreError::Video(
                "webrtcbin returned empty SDP promise reply".to_string(),
            )),
            Err(err) => Err(CoreError::Video(format!(
                "webrtcbin SDP promise failed: {err:?}"
            ))),
        };
        if let Ok(mut guard) = tx_slot.lock()
            && let Some(sender) = guard.take()
        {
            let _ = sender.send(outcome);
        }
    });

    webrtc.emit_by_name::<()>(signal_name, &[&None::<gst::Structure>, &promise]);

    rx.await
        .map_err(|_| CoreError::Video("webrtcbin SDP promise dropped before completion".to_string()))?
}

/// Shared helper to apply a remote SDP offer or answer to `webrtcbin` without blocking Tokio.
pub(crate) async fn webrtc_set_remote_sdp(
    webrtc: &gst::Element,
    sdp_type: gst_webrtc::WebRTCSDPType,
    sdp_text: &str,
) -> CoreResult<()> {
    let sdp = gst_webrtc::gst_sdp::SDPMessage::parse_buffer(sdp_text.as_bytes())
        .map_err(|err| CoreError::Protocol(format!("invalid remote SDP: {err}")))?;
    let desc = gst_webrtc::WebRTCSessionDescription::new(sdp_type, sdp);

    let (tx, rx) = oneshot::channel::<CoreResult<()>>();
    let tx_slot = Arc::new(Mutex::new(Some(tx)));

    let promise = gst::Promise::with_change_func(move |reply| {
        let res = match reply {
            Ok(_) => Ok(()),
            Err(err) => Err(CoreError::Video(format!(
                "webrtcbin set-remote-description failed: {err:?}"
            ))),
        };
        if let Ok(mut guard) = tx_slot.lock()
            && let Some(sender) = guard.take()
        {
            let _ = sender.send(res);
        }
    });

    webrtc.emit_by_name::<()>("set-remote-description", &[&desc, &promise]);

    rx.await
        .map_err(|_| CoreError::Video("set-remote-description promise dropped".to_string()))?
}

fn select_decoder(codec: VideoCodec, prefer_software: bool) -> CoreResult<DecoderSelection> {
    if prefer_software {
        probe_software_decoder(codec).or_else(|_| probe_best_decoder(codec))
    } else {
        probe_best_decoder(codec)
    }
}

pub(crate) fn sanitize_gst_name(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpv_args_configure_borderless_low_latency_rawvideo() {
        let args = MpvPopoutSink::build_mpv_args(1280, 720, 60, "bbscord test");
        assert!(args.contains(&"--no-border".to_string()));
        assert!(args.contains(&"--no-terminal".to_string()));
        assert!(args.contains(&"--profile=low-latency".to_string()));
        assert!(args.contains(&"--demuxer=rawvideo".to_string()));
        assert!(args.contains(&"--demuxer-rawvideo-w=1280".to_string()));
        assert!(args.contains(&"--demuxer-rawvideo-h=720".to_string()));
        assert!(args.contains(&"--demuxer-rawvideo-mp-format=rgba".to_string()));
        assert!(args.contains(&"--demuxer-rawvideo-fps=60".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("-"));
    }

    #[tokio::test]
    async fn popout_pipe_sink_streams_frames_and_cleans_up_child() {
        // Use a cross-platform stdin-draining child (`sh -c "cat >/dev/null"` on Unix,
        // `cmd /C "more >NUL"` on Windows) to verify anonymous pipe I/O & kill_on_drop
        // cleanly in 100% headless CI without opening any GUI window.
        #[cfg(windows)]
        let (prog, args) = ("cmd", vec!["/C".to_string(), "more >NUL".to_string()]);
        #[cfg(not(windows))]
        let (prog, args) = ("sh", vec!["-c".to_string(), "cat >/dev/null".to_string()]);

        let mut sink =
            MpvPopoutSink::spawn_with_args(prog, &args).expect("spawn headless pipe sink");

        assert!(sink.is_alive());
        let frame = vec![0x7Fu8; 32 * 32 * 4];
        assert!(sink.try_push_rgba(frame));
        for _ in 0..100 {
            if sink.frames_written() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(sink.frames_written() >= 1);

        sink.stop().await;
        assert!(!sink.is_alive());
    }

    #[tokio::test]
    async fn missing_mpv_binary_returns_clean_error_for_fallback() {
        let err = MpvPopoutSink::spawn(
            "nonexistent-bbscord-mpv-binary-0000",
            640,
            360,
            30,
            "fallback test",
        )
        .expect_err("missing mpv binary should fail cleanly");
        assert!(err.to_string().contains("failed to spawn pop-out player"));
    }

    #[test]
    fn rtp_pad_caps_classification_distinguishes_audio_and_video() {
        ensure_gst_init().expect("gst init");
        let audio_caps: gst::Caps =
            "application/x-rtp,media=audio,encoding-name=OPUS,payload=111,clock-rate=48000"
                .parse()
                .expect("parse audio caps");
        assert_eq!(classify_rtp_pad_caps(Some(&audio_caps)), (false, None));

        let video_caps: gst::Caps =
            "application/x-rtp,media=video,encoding-name=H264,payload=101,clock-rate=90000"
                .parse()
                .expect("parse video caps");
        assert_eq!(
            classify_rtp_pad_caps(Some(&video_caps)),
            (true, Some(VideoCodec::H264))
        );
        assert_eq!(classify_rtp_pad_caps(None), (false, None));
    }

    #[tokio::test]
    async fn webrtc_watch_offer_includes_recvonly_video_mline() {
        let dave = DaveVideoContext::passthrough(Snowflake::new(11), Snowflake::new(22));
        let mut cfg = WatchConfig::new(
            "call:22:11",
            Snowflake::new(11),
            VideoCodec::H264,
            VideoSinkKind::InlineTerminal,
            dave,
        );
        cfg.prefer_software_decoder = true;
        let (tx, _rx) = broadcast::channel(8);
        let mut watch = WatchPipeline::start_webrtc(cfg, tx).expect("start webrtc watch");

        let offer = watch
            .create_webrtc_offer()
            .await
            .expect("create webrtc offer");
        assert!(
            offer.contains("m=video"),
            "watch WebRTC offer must include m=video line, got:\n{offer}"
        );
        assert!(
            offer.contains("a=recvonly"),
            "watch WebRTC offer must include a=recvonly attribute, got:\n{offer}"
        );
        watch.stop().await;
    }

    #[test]
    fn failed_rtp_chain_attachment_rolls_back_all_elements() {
        ensure_gst_init().expect("gst init");
        let pipeline = gst::Pipeline::new();
        let audio_src = gst::ElementFactory::make("audiotestsrc")
            .name("test_audio_src")
            .build()
            .expect("audiotestsrc");
        let sink_entry = gst::ElementFactory::make("videoconvert")
            .name("test_sink_entry")
            .build()
            .expect("videoconvert");
        pipeline
            .add_many([&audio_src, &sink_entry])
            .expect("add test elements");

        let audio_pad = audio_src.static_pad("src").expect("audio src pad");
        let dave = DaveVideoContext::passthrough(Snowflake::new(1), Snowflake::new(2));

        let err = attach_rtp_decrypt_decode_chain(
            &pipeline,
            &audio_pad,
            VideoCodec::H264,
            true,
            dave,
            Snowflake::new(1),
            &sink_entry,
        )
        .expect_err("raw audio pad must fail to link to rtph264depay");
        assert!(err.to_string().contains("failed to link"));

        // Verify zero orphaned elements remain in pipeline and sink_entry stays unlinked.
        assert!(pipeline.by_name("watch_rtp_depay").is_none());
        assert!(pipeline.by_name("watch_encoded_caps").is_none());
        assert!(pipeline.by_name("dave_decrypt").is_none());
        assert!(pipeline.by_name("watch_dynamic_decoder").is_none());
        assert!(
            !sink_entry
                .static_pad("sink")
                .expect("sink_entry sink pad")
                .is_linked()
        );
        let _ = pipeline.set_state(gst::State::Null);
    }
}
