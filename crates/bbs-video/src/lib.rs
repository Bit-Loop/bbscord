//! Discord Go Live screenshare watching, Wayland/X11 PipeWire broadcasting, WebRTC signaling,
//! and DAVE E2EE media transform engine for `bbscord`.
//!
//! # Architecture
//! - [`codec`]: GStreamer initialization, video codec specs (`H264` / `VP8` / `VP9`), and dynamic
//!   hardware/software encoder & decoder probing (`vah264enc`/`nvh264enc` -> `x264enc` -> `vp8enc`,
//!   `vah264dec`/`nvdec` -> `avdec_h264`/`vp8dec`).
//! - [`dave`]: Thread-safe [`DaveVideoContext`] wrapping `davey::DaveSession` plus GStreamer
//!   buffer pad probes for frame-level video encryption and decryption.
//! - [`signaling`]: Canonical [`StreamKey`] (`guild:<g>:<c>:<u>` and `call:<c>:<u>`), Discord Main
//!   Gateway Go Live commands/dispatches (`STREAM_CREATE`, `STREAM_WATCH`, `STREAM_SET_PAUSED`,
//!   `STREAM_DELETE`), and async Stream RTC WebSocket (`SELECT_PROTOCOL` WebRTC SDP, SSRC/RTX
//!   mapping, and DAVE binary/JSON opcodes).
//! - [`watch`]: Receive pipelines with dual universal sinks (`VideoSinkKind::PopoutWindow` via
//!   low-latency borderless `mpv` pipe with `autovideosink` fallback, and
//!   `VideoSinkKind::InlineTerminal` via downscaled/throttled RGBA `AppSink`).
//! - [`broadcast`]: Wayland/X11 XDG Desktop Portal (`ashpd`) + PipeWire (`pipewiresrc`) screen
//!   capture, dynamic HW/SW encoding, DAVE encryption, `webrtcbin`, and RTCP PLI/FIR keyframe
//!   handling.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod broadcast;
pub mod codec;
pub mod dave;
pub mod signaling;
pub mod watch;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bbs_core::{
    CoreError, CoreResult, DaveSessionState, Snowflake, StreamDescriptor, StreamQuality,
    StreamSourceKind, VideoFrame, VideoSinkKind, VideoStreamEngine,
};
use gstreamer_webrtc as gst_webrtc;
use tokio::sync::{RwLock, broadcast as tokio_broadcast};
use tracing::{debug, info, warn};

use crate::broadcast::{BroadcastConfig, BroadcastOutput, BroadcastPipeline, PortalCaptureSession};
use crate::codec::{VideoCodec, ensure_gst_init};
use crate::dave::DaveVideoContext;
use crate::signaling::{
    GatewayStreamCommand, GatewayStreamEvent, RedactedStreamToken, StreamKey, StreamRtcClient,
    StreamRtcConfig, StreamRtcEvent,
};
use crate::watch::{
    DEFAULT_INLINE_FPS, DEFAULT_INLINE_HEIGHT, DEFAULT_INLINE_WIDTH, WatchConfig, WatchPipeline,
};

/// Configuration knobs for [`GStreamerVideoStreamEngine`].
#[derive(Debug, Clone)]
pub struct VideoEngineConfig {
    /// Local signed-in Discord user Snowflake.
    pub local_user_id: Snowflake,
    /// Local user display name for outbound broadcast descriptors.
    pub local_display_name: String,
    /// Target FPS for [`VideoSinkKind::InlineTerminal`] RGBA frame emission.
    pub inline_target_fps: u8,
    /// Target resolution `(width, height)` for [`VideoSinkKind::InlineTerminal`].
    pub inline_resolution: (u32, u32),
    /// Preferred pop-out player binary (`"mpv"` by default).
    pub mpv_binary: String,
    /// Whether to advertise and require DAVE E2EE on Stream RTC sessions.
    pub enable_dave: bool,
    /// Optional preferred outbound codec (`None` selects hardware H.264 -> software cascade).
    pub preferred_codec: Option<VideoCodec>,
}

impl VideoEngineConfig {
    /// Construct default production configuration for `local_user_id`.
    #[must_use]
    pub fn new(local_user_id: Snowflake, local_display_name: impl Into<String>) -> Self {
        Self {
            local_user_id,
            local_display_name: local_display_name.into(),
            inline_target_fps: DEFAULT_INLINE_FPS,
            inline_resolution: (DEFAULT_INLINE_WIDTH, DEFAULT_INLINE_HEIGHT),
            mpv_binary: "mpv".to_string(),
            enable_dave: true,
            preferred_codec: None,
        }
    }
}

#[derive(Debug, Default)]
struct PendingStreamRtcInfo {
    rtc_server_id: Option<Snowflake>,
    endpoint: Option<String>,
    token: Option<RedactedStreamToken>,
}

struct ActiveWatchSession {
    sink: VideoSinkKind,
    dave: DaveVideoContext,
    pipeline: Arc<tokio::sync::Mutex<WatchPipeline>>,
    rtc_client: Option<StreamRtcClient>,
    bridge_task: Option<tokio::task::JoinHandle<()>>,
}

struct ActiveBroadcastSession {
    descriptor: StreamDescriptor,
    dave: DaveVideoContext,
    pipeline: Arc<tokio::sync::Mutex<BroadcastPipeline>>,
    rtc_client: Option<StreamRtcClient>,
    bridge_task: Option<tokio::task::JoinHandle<()>>,
    paused: bool,
}

/// Production Go Live [`VideoStreamEngine`] backed by GStreamer WebRTC, PipeWire/Portal capture,
/// dual universal sinks (`mpv` pop-out + inline terminal RGBA), and DAVE E2EE (`davey`).
pub struct GStreamerVideoStreamEngine {
    config: VideoEngineConfig,
    gateway_session_id: RwLock<Option<String>>,
    streams: RwLock<HashMap<String, StreamDescriptor>>,
    pending_rtc: RwLock<HashMap<String, PendingStreamRtcInfo>>,
    watchers: RwLock<HashMap<String, ActiveWatchSession>>,
    broadcast: RwLock<Option<ActiveBroadcastSession>>,
    frames_tx: tokio_broadcast::Sender<VideoFrame>,
    gateway_cmd_tx: tokio_broadcast::Sender<GatewayStreamCommand>,
}

impl std::fmt::Debug for GStreamerVideoStreamEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GStreamerVideoStreamEngine")
            .field("config", &self.config)
            .finish()
    }
}

impl GStreamerVideoStreamEngine {
    /// Construct a new [`GStreamerVideoStreamEngine`] and verify GStreamer initialization.
    pub fn new(config: VideoEngineConfig) -> CoreResult<Self> {
        ensure_gst_init()?;
        let (frames_tx, _) = tokio_broadcast::channel(32);
        let (gateway_cmd_tx, _) = tokio_broadcast::channel(32);
        Ok(Self {
            config,
            gateway_session_id: RwLock::new(None),
            streams: RwLock::new(HashMap::new()),
            pending_rtc: RwLock::new(HashMap::new()),
            watchers: RwLock::new(HashMap::new()),
            broadcast: RwLock::new(None),
            frames_tx,
            gateway_cmd_tx,
        })
    }

    /// Record the active Discord Main Gateway `session_id` needed for Stream RTC `IDENTIFY`,
    /// immediately advancing any Go Live sessions whose RTC credentials arrived earlier.
    pub async fn set_gateway_session_id(&self, session_id: impl Into<String>) {
        *self.gateway_session_id.write().await = Some(session_id.into());
        let pending_keys: Vec<StreamKey> = self
            .pending_rtc
            .read()
            .await
            .keys()
            .filter_map(|k| StreamKey::parse(k).ok())
            .collect();
        for key in pending_keys {
            if let Err(err) = self.try_connect_stream_rtc(key).await {
                warn!(
                    stream_key = %key,
                    error = %err,
                    "deferred Stream RTC connection attempt failed"
                );
            }
        }
    }

    /// Subscribe to outbound [`GatewayStreamCommand`]s (`STREAM_CREATE`, `STREAM_WATCH`,
    /// `STREAM_SET_PAUSED`, `STREAM_DELETE`) that should be forwarded over the Main Gateway.
    #[must_use]
    pub fn subscribe_gateway_commands(&self) -> tokio_broadcast::Receiver<GatewayStreamCommand> {
        self.gateway_cmd_tx.subscribe()
    }

    /// Register or update a known Go Live stream descriptor (e.g., from `VOICE_STATE_UPDATE`
    /// `self_stream = true` or `STREAM_CREATE`).
    pub async fn upsert_stream(&self, descriptor: StreamDescriptor) -> CoreResult<()> {
        let _ = StreamKey::parse(&descriptor.stream_key)?;
        self.streams
            .write()
            .await
            .insert(descriptor.stream_key.clone(), descriptor);
        Ok(())
    }

    /// Remove a stream descriptor and stop watching it if currently watched.
    pub async fn remove_stream(&self, stream_key: &str) {
        self.streams.write().await.remove(stream_key);
        self.pending_rtc.write().await.remove(stream_key);
        if let Some(session) = self.watchers.write().await.remove(stream_key) {
            teardown_watch_session(session).await;
        }
    }

    /// Query the active [`VideoSinkKind`] for `stream_key`, if currently being watched.
    pub async fn active_watch_sink(&self, stream_key: &str) -> Option<VideoSinkKind> {
        self.watchers
            .read()
            .await
            .get(stream_key)
            .map(|session| session.sink)
    }

    /// Query the current DAVE E2EE state for either a watched stream or the active broadcast.
    pub async fn dave_state_for_stream(&self, stream_key: &str) -> DaveSessionState {
        if let Some(session) = self.watchers.read().await.get(stream_key) {
            return session.dave.session_state();
        }
        if let Some(broadcast) = self.broadcast.read().await.as_ref()
            && broadcast.descriptor.stream_key == stream_key
        {
            return broadcast.dave.session_state();
        }
        DaveSessionState::Disabled
    }

    /// Pause or resume the active outbound Go Live broadcast (`STREAM_SET_PAUSED`).
    pub async fn set_broadcast_paused(&self, paused: bool) -> CoreResult<()> {
        let stream_key = {
            let mut guard = self.broadcast.write().await;
            let active = guard.as_mut().ok_or_else(|| {
                CoreError::NotFound("no active Go Live broadcast to pause/resume".to_string())
            })?;
            active.paused = paused;
            StreamKey::parse(&active.descriptor.stream_key)?
        };
        let _ = self
            .gateway_cmd_tx
            .send(GatewayStreamCommand::SetPaused { stream_key, paused });
        Ok(())
    }

    /// Trigger an immediate keyframe on the active outbound broadcast (e.g., on RTCP PLI/FIR).
    pub async fn request_broadcast_keyframe(&self) -> CoreResult<()> {
        let pipeline_arc = {
            let guard = self.broadcast.read().await;
            guard
                .as_ref()
                .map(|b| Arc::clone(&b.pipeline))
                .ok_or_else(|| CoreError::NotFound("no active Go Live broadcast".to_string()))?
        };
        let pipeline = pipeline_arc.lock().await;
        pipeline.request_keyframe()
    }

    /// Apply an inbound Discord Main Gateway Go Live event (`STREAM_CREATE`,
    /// `STREAM_SERVER_UPDATE`, `STREAM_UPDATE`, `STREAM_DELETE`), automatically advancing Stream
    /// RTC signaling when credentials arrive.
    pub async fn apply_gateway_event(&self, event: GatewayStreamEvent) -> CoreResult<()> {
        match event {
            GatewayStreamEvent::StreamCreate {
                stream_key,
                rtc_server_id,
                viewer_ids,
                ..
            } => {
                let canonical = stream_key.to_canonical_string();
                {
                    let mut streams = self.streams.write().await;
                    streams
                        .entry(canonical.clone())
                        .and_modify(|desc| desc.viewer_count = viewer_ids.len() as u32)
                        .or_insert_with(|| StreamDescriptor {
                            stream_key: canonical.clone(),
                            guild_id: stream_key.guild_id(),
                            channel_id: stream_key.channel_id(),
                            owner_id: stream_key.user_id(),
                            owner_name: format!("user-{}", stream_key.user_id()),
                            title: if stream_key.is_call() {
                                "DM Call Screenshare".to_string()
                            } else {
                                "Go Live Screenshare".to_string()
                            },
                            resolution: (1280, 720),
                            fps: 30,
                            viewer_count: viewer_ids.len() as u32,
                            has_audio: true,
                            dave_encrypted: self.config.enable_dave,
                        });
                }
                {
                    let mut pending = self.pending_rtc.write().await;
                    pending
                        .entry(canonical.clone())
                        .or_default()
                        .rtc_server_id = Some(rtc_server_id);
                }
                self.try_connect_stream_rtc(stream_key).await?;
            }
            GatewayStreamEvent::StreamServerUpdate {
                stream_key,
                endpoint,
                token,
            } => {
                let canonical = stream_key.to_canonical_string();
                {
                    let mut pending = self.pending_rtc.write().await;
                    let entry = pending.entry(canonical).or_default();
                    entry.endpoint = Some(endpoint);
                    entry.token = Some(token);
                }
                self.try_connect_stream_rtc(stream_key).await?;
            }
            GatewayStreamEvent::StreamUpdate {
                stream_key,
                viewer_ids,
                ..
            } => {
                let canonical = stream_key.to_canonical_string();
                if let Some(desc) = self.streams.write().await.get_mut(&canonical) {
                    desc.viewer_count = viewer_ids.len() as u32;
                }
                if let Some(active) = self.broadcast.write().await.as_mut()
                    && active.descriptor.stream_key == canonical
                {
                    active.descriptor.viewer_count = viewer_ids.len() as u32;
                }
            }
            GatewayStreamEvent::StreamDelete {
                stream_key, reason, ..
            } => {
                let canonical = stream_key.to_canonical_string();
                info!(stream_key = %canonical, reason = ?reason, "Go Live stream deleted");
                self.remove_stream(&canonical).await;
                let is_our_broadcast = self
                    .broadcast
                    .read()
                    .await
                    .as_ref()
                    .is_some_and(|b| b.descriptor.stream_key == canonical);
                if is_our_broadcast {
                    self.stop_broadcast().await?;
                }
            }
        }
        Ok(())
    }

    async fn try_connect_stream_rtc(&self, stream_key: StreamKey) -> CoreResult<()> {
        let canonical = stream_key.to_canonical_string();
        let Some(session_id) = self.gateway_session_id.read().await.clone() else {
            debug!(
                stream_key = %canonical,
                "deferring Stream RTC dial until Main Gateway session_id is set"
            );
            return Ok(());
        };

        // Ensure all required credentials are buffered BEFORE probing/allocating offers.
        let creds_ready = self.pending_rtc.read().await.get(&canonical).is_some_and(|p| {
            p.rtc_server_id.is_some() && p.endpoint.is_some() && p.token.is_some()
        });
        if !creds_ready {
            return Ok(());
        }

        // Confirm an active broadcast or watcher exists BEFORE claiming buffered credentials so
        // early STREAM_CREATE / STREAM_SERVER_UPDATE dispatches are never discarded.
        let broadcast_target = {
            let guard = self.broadcast.read().await;
            guard.as_ref().and_then(|b| {
                if b.descriptor.stream_key == canonical {
                    Some((
                        Arc::clone(&b.pipeline),
                        b.dave.clone(),
                        StreamQuality {
                            width: b.descriptor.resolution.0,
                            height: b.descriptor.resolution.1,
                            fps: b.descriptor.fps,
                            max_bitrate_kbps: 2_500,
                        },
                    ))
                } else {
                    None
                }
            })
        };

        let watch_target = if broadcast_target.is_none() {
            let guard = self.watchers.read().await;
            guard
                .get(&canonical)
                .map(|w| (Arc::clone(&w.pipeline), w.dave.clone()))
        } else {
            None
        };

        if broadcast_target.is_none() && watch_target.is_none() {
            debug!(
                stream_key = %canonical,
                "buffering Stream RTC credentials until watch_stream or start_broadcast activates"
            );
            return Ok(());
        }

        // Generate the local WebRTC SDP offer BEFORE removing credentials; propagate errors rather
        // than sending an empty SDP string in `SELECT_PROTOCOL`.
        let local_sdp = if let Some((ref pipeline_arc, _, _)) = broadcast_target {
            let pipe = pipeline_arc.lock().await;
            pipe.create_webrtc_offer().await.map_err(|err| {
                warn!(stream_key = %canonical, error = %err, "broadcast create_webrtc_offer failed");
                err
            })?
        } else if let Some((ref pipeline_arc, _)) = watch_target {
            let pipe = pipeline_arc.lock().await;
            pipe.create_webrtc_offer().await.map_err(|err| {
                warn!(stream_key = %canonical, error = %err, "watch create_webrtc_offer failed");
                err
            })?
        } else {
            return Ok(());
        };

        if local_sdp.trim().is_empty() {
            return Err(CoreError::Video(format!(
                "webrtcbin produced empty SDP offer for stream `{canonical}`"
            )));
        }

        let codecs = tokio::task::spawn_blocking(VideoCodec::negotiate_host_codec_specs)
            .await
            .map_err(|err| CoreError::Video(format!("codec negotiation probe failed: {err}")))?;

        let creds = {
            let mut pending = self.pending_rtc.write().await;
            let still_ready = pending.get(&canonical).is_some_and(|p| {
                p.rtc_server_id.is_some() && p.endpoint.is_some() && p.token.is_some()
            });
            if !still_ready {
                return Ok(());
            }
            pending.remove(&canonical)
        };

        let Some(PendingStreamRtcInfo {
            rtc_server_id: Some(rtc_server_id),
            endpoint: Some(endpoint),
            token: Some(token),
        }) = creds
        else {
            return Ok(());
        };

        // Case 1: Active outbound broadcast matching this stream key.
        if let Some((pipeline_arc, dave, quality)) = broadcast_target {
            let rtc_cfg = StreamRtcConfig {
                stream_key,
                rtc_server_id,
                user_id: self.config.local_user_id,
                session_id,
                endpoint,
                token,
                codecs,
                local_sdp_offer: local_sdp,
                broadcast_quality: Some(quality),
                enable_dave: self.config.enable_dave,
            };
            let client = StreamRtcClient::connect(rtc_cfg, dave).await?;
            let mut rx = client.subscribe();
            let pipe_bridge = Arc::clone(&pipeline_arc);
            let bridge = tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    if let StreamRtcEvent::SessionDescription { remote_sdp, .. } = ev
                        && !remote_sdp.is_empty()
                    {
                        let pipe = pipe_bridge.lock().await;
                        if let Err(err) = pipe
                            .set_remote_sdp(gst_webrtc::WebRTCSDPType::Answer, &remote_sdp)
                            .await
                        {
                            warn!(error = %err, "failed to apply remote SDP answer on broadcast");
                        }
                    }
                }
            });

            let (cleanup_rtc, cleanup_bridge) = {
                let mut guard = self.broadcast.write().await;
                if let Some(active) = guard.as_mut()
                    && active.descriptor.stream_key == canonical
                {
                    let prev_rtc = active.rtc_client.replace(client);
                    let prev_bridge = active.bridge_task.replace(bridge);
                    (prev_rtc, prev_bridge)
                } else {
                    // Broadcast stopped concurrently while `connect()` was awaiting.
                    (Some(client), Some(bridge))
                }
            };
            cleanup_rtc_and_bridge(cleanup_rtc, cleanup_bridge).await;
            return Ok(());
        }

        // Case 2: Active watch session matching this stream key.
        if let Some((pipeline_arc, dave)) = watch_target {
            let rtc_cfg = StreamRtcConfig {
                stream_key,
                rtc_server_id,
                user_id: self.config.local_user_id,
                session_id,
                endpoint,
                token,
                codecs,
                local_sdp_offer: local_sdp,
                broadcast_quality: None,
                enable_dave: self.config.enable_dave,
            };
            let client = StreamRtcClient::connect(rtc_cfg, dave).await?;
            let mut rx = client.subscribe();
            let pipe_bridge = Arc::clone(&pipeline_arc);
            let bridge = tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    if let StreamRtcEvent::SessionDescription { remote_sdp, .. } = ev
                        && !remote_sdp.is_empty()
                    {
                        let pipe = pipe_bridge.lock().await;
                        if let Err(err) = pipe
                            .set_remote_sdp(gst_webrtc::WebRTCSDPType::Answer, &remote_sdp)
                            .await
                        {
                            warn!(error = %err, "failed to apply remote SDP answer on watcher");
                        }
                    }
                }
            });

            let (cleanup_rtc, cleanup_bridge) = {
                let mut guard = self.watchers.write().await;
                if let Some(active) = guard.get_mut(&canonical) {
                    let prev_rtc = active.rtc_client.replace(client);
                    let prev_bridge = active.bridge_task.replace(bridge);
                    (prev_rtc, prev_bridge)
                } else {
                    // Watcher stopped concurrently while `connect()` was awaiting.
                    (Some(client), Some(bridge))
                }
            };
            cleanup_rtc_and_bridge(cleanup_rtc, cleanup_bridge).await;
        }

        Ok(())
    }

    /// Gracefully stop all active watchers and any active broadcast.
    pub async fn shutdown(&self) {
        let all_watchers: Vec<ActiveWatchSession> = self
            .watchers
            .write()
            .await
            .drain()
            .map(|(_, s)| s)
            .collect();
        for session in all_watchers {
            teardown_watch_session(session).await;
        }
        let _ = self.stop_broadcast().await;
    }
}

#[async_trait]
impl VideoStreamEngine for GStreamerVideoStreamEngine {
    async fn list_streams(&self, channel_id: Snowflake) -> CoreResult<Vec<StreamDescriptor>> {
        let guard = self.streams.read().await;
        Ok(guard
            .values()
            .filter(|desc| desc.channel_id == channel_id)
            .cloned()
            .collect())
    }

    async fn watch_stream(
        &self,
        stream_key: &str,
        sink: VideoSinkKind,
    ) -> CoreResult<StreamDescriptor> {
        let parsed_key = StreamKey::parse(stream_key)?;
        let canonical = parsed_key.to_canonical_string();

        let descriptor = self
            .streams
            .read()
            .await
            .get(&canonical)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(format!("stream `{canonical}`")))?;

        if let Some(previous) = self.watchers.write().await.remove(&canonical) {
            teardown_watch_session(previous).await;
        }

        let dave = if descriptor.dave_encrypted && self.config.enable_dave {
            DaveVideoContext::new(
                self.config.local_user_id,
                parsed_key.channel_id(),
                davey::DAVE_PROTOCOL_VERSION,
            )?
        } else {
            DaveVideoContext::passthrough(self.config.local_user_id, parsed_key.channel_id())
        };

        let mut watch_cfg = WatchConfig::new(
            canonical.clone(),
            descriptor.owner_id,
            self.config.preferred_codec.unwrap_or(VideoCodec::H264),
            sink,
            dave.clone(),
        );
        watch_cfg.window_title = format!("bbscord — {} ({})", descriptor.title, descriptor.owner_name);
        watch_cfg.inline_target_fps = self.config.inline_target_fps;
        watch_cfg.inline_width = self.config.inline_resolution.0;
        watch_cfg.inline_height = self.config.inline_resolution.1;
        watch_cfg.popout_width = descriptor.resolution.0.max(320);
        watch_cfg.popout_height = descriptor.resolution.1.max(240);
        watch_cfg.popout_fps = descriptor.fps.clamp(15, 60);
        watch_cfg.mpv_binary = self.config.mpv_binary.clone();

        let frames_tx = self.frames_tx.clone();
        let pipeline = tokio::task::spawn_blocking(move || {
            WatchPipeline::start_webrtc(watch_cfg, frames_tx)
        })
        .await
        .map_err(|err| CoreError::Video(format!("watch pipeline spawn task failed: {err}")))??;

        self.watchers.write().await.insert(
            canonical,
            ActiveWatchSession {
                sink,
                dave,
                pipeline: Arc::new(tokio::sync::Mutex::new(pipeline)),
                rtc_client: None,
                bridge_task: None,
            },
        );

        let _ = self.gateway_cmd_tx.send(GatewayStreamCommand::Watch {
            stream_key: parsed_key,
        });

        // Claim any Stream RTC credentials that were already buffered before watch_stream ran.
        self.try_connect_stream_rtc(parsed_key).await?;

        Ok(descriptor)
    }

    async fn stop_watching(&self, stream_key: &str) -> CoreResult<()> {
        let canonical = StreamKey::parse(stream_key)
            .map(|k| k.to_canonical_string())
            .unwrap_or_else(|_| stream_key.to_string());

        if let Some(session) = self.watchers.write().await.remove(&canonical) {
            teardown_watch_session(session).await;
        }
        if let Ok(parsed) = StreamKey::parse(&canonical) {
            let _ = self
                .gateway_cmd_tx
                .send(GatewayStreamCommand::Delete { stream_key: parsed });
        }
        Ok(())
    }

    async fn start_broadcast(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        source: StreamSourceKind,
        quality: StreamQuality,
    ) -> CoreResult<StreamDescriptor> {
        self.stop_broadcast().await?;

        let stream_key = StreamKey::from_ids(guild_id, channel_id, self.config.local_user_id);
        let canonical = stream_key.to_canonical_string();

        // Acquire portal capture asynchronously (Wayland/X11) before launching GStreamer in spawn_blocking.
        let portal_session = match source {
            StreamSourceKind::SyntheticTestCard => None,
            StreamSourceKind::PortalPipeWire
            | StreamSourceKind::Monitor
            | StreamSourceKind::ApplicationWindow => {
                Some(PortalCaptureSession::acquire(source).await?)
            }
        };

        let dave = if self.config.enable_dave && source != StreamSourceKind::SyntheticTestCard {
            DaveVideoContext::new(
                self.config.local_user_id,
                channel_id,
                davey::DAVE_PROTOCOL_VERSION,
            )?
        } else {
            DaveVideoContext::passthrough(self.config.local_user_id, channel_id)
        };

        let effective_quality = StreamQuality {
            width: quality.width.max(160),
            height: quality.height.max(120),
            fps: quality.fps.clamp(1, 60),
            max_bitrate_kbps: quality.max_bitrate_kbps.clamp(250, 25_000),
        };

        let broadcast_cfg = BroadcastConfig {
            stream_key: canonical.clone(),
            source_kind: source,
            quality: effective_quality,
            preferred_codec: self.config.preferred_codec,
            prefer_software_encoder: false,
            synthetic_num_buffers: None,
            synthetic_is_live: true,
            dave: dave.clone(),
        };

        let pipeline = tokio::task::spawn_blocking(move || {
            BroadcastPipeline::start_with_portal(
                broadcast_cfg,
                portal_session,
                BroadcastOutput::WebRtc { payload_type: None },
            )
        })
        .await
        .map_err(|err| CoreError::Video(format!("broadcast spawn task failed: {err}")))??;

        let title = match source {
            StreamSourceKind::PortalPipeWire => "Wayland/PipeWire Screen Share".to_string(),
            StreamSourceKind::Monitor => "Monitor Screen Share".to_string(),
            StreamSourceKind::ApplicationWindow => "Window Screen Share".to_string(),
            StreamSourceKind::SyntheticTestCard => "SMPTE Synthetic Broadcast".to_string(),
        };

        let descriptor = StreamDescriptor {
            stream_key: canonical.clone(),
            guild_id,
            channel_id,
            owner_id: self.config.local_user_id,
            owner_name: self.config.local_display_name.clone(),
            title,
            resolution: (effective_quality.width, effective_quality.height),
            fps: effective_quality.fps,
            viewer_count: 0,
            has_audio: false,
            dave_encrypted: self.config.enable_dave && source != StreamSourceKind::SyntheticTestCard,
        };

        self.streams
            .write()
            .await
            .insert(canonical, descriptor.clone());
        *self.broadcast.write().await = Some(ActiveBroadcastSession {
            descriptor: descriptor.clone(),
            dave,
            pipeline: Arc::new(tokio::sync::Mutex::new(pipeline)),
            rtc_client: None,
            bridge_task: None,
            paused: false,
        });

        let _ = self.gateway_cmd_tx.send(GatewayStreamCommand::Create {
            stream_key,
            preferred_region: None,
        });

        // Claim any Stream RTC credentials that were already buffered before start_broadcast ran.
        self.try_connect_stream_rtc(stream_key).await?;

        Ok(descriptor)
    }

    async fn stop_broadcast(&self) -> CoreResult<()> {
        let existing = self.broadcast.write().await.take();
        if let Some(mut active) = existing {
            let canonical = active.descriptor.stream_key.clone();
            self.streams.write().await.remove(&canonical);
            if let Some(handle) = active.bridge_task.take() {
                handle.abort();
                let _ = handle.await;
            }
            if let Some(mut rtc) = active.rtc_client.take() {
                rtc.disconnect().await;
            }
            active.pipeline.lock().await.stop().await;
            if let Ok(parsed) = StreamKey::parse(&canonical) {
                let _ = self
                    .gateway_cmd_tx
                    .send(GatewayStreamCommand::Delete { stream_key: parsed });
            }
        }
        Ok(())
    }

    async fn active_broadcast(&self) -> CoreResult<Option<StreamDescriptor>> {
        Ok(self
            .broadcast
            .read()
            .await
            .as_ref()
            .map(|b| b.descriptor.clone()))
    }

    fn subscribe_frames(&self) -> tokio_broadcast::Receiver<VideoFrame> {
        self.frames_tx.subscribe()
    }
}

async fn cleanup_rtc_and_bridge(
    rtc_client: Option<StreamRtcClient>,
    bridge_task: Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(handle) = bridge_task {
        handle.abort();
        let _ = handle.await;
    }
    if let Some(mut rtc) = rtc_client {
        rtc.disconnect().await;
    }
}

async fn teardown_watch_session(mut session: ActiveWatchSession) {
    cleanup_rtc_and_bridge(session.rtc_client.take(), session.bridge_task.take()).await;
    session.pipeline.lock().await.stop().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn engine_coordinates_guild_and_call_streams_and_synthetic_broadcast() {
        let engine = GStreamerVideoStreamEngine::new(VideoEngineConfig::new(
            Snowflake::new(1001),
            "bitloop",
        ))
        .expect("init video engine");

        let mut cmd_rx = engine.subscribe_gateway_commands();

        // Register a DM/Group DM call stream via Gateway STREAM_CREATE dispatch.
        engine
            .apply_gateway_event(GatewayStreamEvent::StreamCreate {
                stream_key: StreamKey::Call {
                    channel_id: Snowflake::new(500),
                    user_id: Snowflake::new(2002),
                },
                rtc_server_id: Snowflake::new(9999),
                rtc_channel_id: None,
                region: Some("us-central".to_string()),
                viewer_ids: vec![Snowflake::new(3003)],
                paused: false,
            })
            .await
            .expect("apply STREAM_CREATE");

        let listed = engine
            .list_streams(Snowflake::new(500))
            .await
            .expect("list streams");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].stream_key, "call:500:2002");
        assert_eq!(listed[0].viewer_count, 1);

        // Start watching via InlineTerminal (100% headless!).
        let watched = engine
            .watch_stream("call:500:2002", VideoSinkKind::InlineTerminal)
            .await
            .expect("watch inline stream");
        assert_eq!(watched.stream_key, "call:500:2002");
        assert_eq!(
            engine.active_watch_sink("call:500:2002").await,
            Some(VideoSinkKind::InlineTerminal)
        );
        let cmd = cmd_rx.recv().await.expect("STREAM_WATCH command emitted");
        assert_eq!(cmd.opcode(), signaling::gateway_op::STREAM_WATCH);

        engine
            .stop_watching("call:500:2002")
            .await
            .expect("stop watching");
        assert_eq!(engine.active_watch_sink("call:500:2002").await, None);
        let del_cmd = cmd_rx.recv().await.expect("STREAM_DELETE command emitted");
        assert_eq!(del_cmd.opcode(), signaling::gateway_op::STREAM_DELETE);

        // Start & pause & stop a headless SyntheticTestCard broadcast in a Guild Voice channel.
        let desc = engine
            .start_broadcast(
                Some(Snowflake::new(77)),
                Snowflake::new(88),
                StreamSourceKind::SyntheticTestCard,
                StreamQuality {
                    width: 320,
                    height: 240,
                    fps: 15,
                    max_bitrate_kbps: 800,
                },
            )
            .await
            .expect("start synthetic broadcast");
        assert_eq!(desc.stream_key, "guild:77:88:1001");
        assert!(engine.active_broadcast().await.expect("query").is_some());
        let create_cmd = cmd_rx.recv().await.expect("STREAM_CREATE emitted");
        assert_eq!(create_cmd.opcode(), signaling::gateway_op::STREAM_CREATE);

        engine.set_broadcast_paused(true).await.expect("pause");
        let pause_cmd = cmd_rx.recv().await.expect("STREAM_SET_PAUSED emitted");
        assert_eq!(pause_cmd.opcode(), signaling::gateway_op::STREAM_SET_PAUSED);

        engine.stop_broadcast().await.expect("stop broadcast");
        assert!(engine.active_broadcast().await.expect("query").is_none());
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn buffered_rtc_credentials_survive_until_watch_and_session_id_arrive() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local mock RTC WS");
        let addr = listener.local_addr().expect("local addr");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("ws accept");
            // 1. Expect IDENTIFY, then send READY so client sends SELECT_PROTOCOL with SDP offer.
            let _identify = ws.next().await;
            let ready = serde_json::json!({
                "op": signaling::rtc_op::READY,
                "d": {
                    "ssrc": 1000,
                    "ip": "127.0.0.1",
                    "port": 50000,
                    "modes": ["aead_xchacha20_poly1305_rtpsize"],
                    "streams": [{ "type": "video", "ssrc": 1001, "rtx_ssrc": 1002, "rid": "100", "quality": 100, "active": true }]
                }
            });
            ws.send(WsMessage::text(ready.to_string()))
                .await
                .expect("send READY");

            let mut captured_sdp = String::new();
            while let Ok(Some(Ok(msg))) =
                tokio::time::timeout(std::time::Duration::from_secs(3), ws.next()).await
            {
                if let WsMessage::Text(txt) = msg
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(txt.as_str())
                    && val["op"] == signaling::rtc_op::SELECT_PROTOCOL
                {
                    captured_sdp = val["d"]["sdp"].as_str().unwrap_or_default().to_string();
                    break;
                }
            }
            captured_sdp
        });

        let engine = GStreamerVideoStreamEngine::new(VideoEngineConfig::new(
            Snowflake::new(1001),
            "bitloop",
        ))
        .expect("init video engine");

        let stream_key = StreamKey::Call {
            channel_id: Snowflake::new(777),
            user_id: Snowflake::new(2002),
        };

        // Arrive BEFORE watch_stream AND BEFORE set_gateway_session_id: must stay buffered!
        engine
            .apply_gateway_event(GatewayStreamEvent::StreamCreate {
                stream_key,
                rtc_server_id: Snowflake::new(5555),
                rtc_channel_id: None,
                region: Some("us-central".to_string()),
                viewer_ids: Vec::new(),
                paused: false,
            })
            .await
            .expect("apply STREAM_CREATE");

        engine
            .apply_gateway_event(GatewayStreamEvent::StreamServerUpdate {
                stream_key,
                endpoint: format!("ws://{addr}"),
                token: RedactedStreamToken::new("ephemeral-test-token".to_string()),
            })
            .await
            .expect("apply STREAM_SERVER_UPDATE");

        // Now start watching, then supply Gateway session_id -> deferred credentials must connect!
        engine
            .watch_stream(&stream_key.to_canonical_string(), VideoSinkKind::InlineTerminal)
            .await
            .expect("watch_stream");
        engine.set_gateway_session_id("gw-session-123").await;

        let sdp_offer = server.await.expect("mock server result");
        assert!(
            sdp_offer.contains("m=video"),
            "SELECT_PROTOCOL SDP offer must contain m=video, got:\n{sdp_offer}"
        );
        assert!(
            sdp_offer.contains("a=recvonly"),
            "SELECT_PROTOCOL SDP offer must contain a=recvonly, got:\n{sdp_offer}"
        );

        engine.shutdown().await;
    }
}
