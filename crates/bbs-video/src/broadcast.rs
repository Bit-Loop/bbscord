//! Cross-platform screen capture (Wayland/X11 XDG Desktop Portal + PipeWire on Linux,
//! AVFoundation on macOS, D3D11/GDI on Windows) and GStreamer WebRTC broadcast engine.

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[cfg(target_os = "linux")]
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
#[cfg(target_os = "linux")]
use ashpd::desktop::{PersistMode, Session};
use bbs_core::{CoreError, CoreResult, StreamQuality, StreamSourceKind};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_webrtc as gst_webrtc;
use tokio::sync::mpsc;
#[cfg(target_os = "linux")]
use tracing::{debug, info};
#[cfg(not(target_os = "linux"))]
use tracing::info;

use crate::codec::{
    EncoderSelection, VideoCodec, create_configured_encoder, create_configured_payloader,
    ensure_gst_init, probe_best_encoder, probe_software_encoder,
};
use crate::dave::{DaveVideoContext, install_dave_encrypt_probe};
use crate::watch::{
    install_h264_strip_sprop_probe, sanitize_gst_name, webrtc_create_sdp, webrtc_set_remote_sdp,
};

/// Active XDG Desktop Portal ScreenCast session owning the authorized PipeWire remote FD (Linux),
/// or a lightweight native screen-capture token on macOS/Windows.
#[cfg(target_os = "linux")]
pub struct PortalCaptureSession {
    session: Session<Screencast>,
    pipewire_fd: OwnedFd,
    node_id: u32,
    pipewire_serial: Option<u64>,
    natural_size: Option<(u32, u32)>,
    source_type: Option<SourceType>,
}

/// Active screen capture token on non-Linux platforms (macOS `avfvideosrc`, Windows `d3d11screencapturesrc`).
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct PortalCaptureSession;

#[cfg(target_os = "linux")]
impl std::fmt::Debug for PortalCaptureSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalCaptureSession")
            .field("node_id", &self.node_id)
            .field("pipewire_serial", &self.pipewire_serial)
            .field("natural_size", &self.natural_size)
            .field("source_type", &self.source_type)
            .finish()
    }
}

#[cfg(target_os = "linux")]
impl PortalCaptureSession {
    /// Open an XDG Desktop Portal ScreenCast picker (Wayland & X11) and acquire an authorized
    /// PipeWire remote file descriptor and stream `node_id`.
    pub async fn acquire(source_kind: StreamSourceKind) -> CoreResult<Self> {
        let source_types = match source_kind {
            StreamSourceKind::PortalPipeWire => SourceType::MONITOR | SourceType::WINDOW,
            StreamSourceKind::Monitor => SourceType::MONITOR,
            StreamSourceKind::ApplicationWindow => SourceType::WINDOW,
            StreamSourceKind::SyntheticTestCard => {
                return Err(CoreError::Video(
                    "SyntheticTestCard does not use XDG Desktop Portal".to_string(),
                ));
            }
        };

        let proxy = Screencast::new().await.map_err(|err| {
            CoreError::Video(format!(
                "XDG Desktop Portal ScreenCast unavailable (Wayland/X11 portal required): {err}"
            ))
        })?;

        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(|err| CoreError::Video(format!("portal create_session failed: {err}")))?;

        let select_opts = SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::EMBEDDED)
            .set_sources(source_types)
            .set_multiple(false)
            .set_persist_mode(PersistMode::DoNot);

        proxy
            .select_sources(&session, select_opts)
            .await
            .map_err(|err| CoreError::Video(format!("portal select_sources failed: {err}")))?;

        let start_response = proxy
            .start(&session, None, StartCastOptions::default())
            .await
            .map_err(|err| CoreError::Video(format!("portal start failed: {err}")))?
            .response()
            .map_err(|err| match err {
                ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
                    CoreError::Cancelled
                }
                other => CoreError::Video(format!("portal screencast start rejected: {other}")),
            })?;

        let stream = start_response.streams().first().ok_or_else(|| {
            CoreError::Video("portal screencast returned no PipeWire streams".to_string())
        })?;

        let node_id = stream.pipe_wire_node_id();
        let pipewire_serial = stream.pipewire_serial();
        let natural_size = stream.size().and_then(|(w, h)| {
            if w > 0 && h > 0 {
                Some((w as u32, h as u32))
            } else {
                None
            }
        });
        let source_type = stream.source_type();

        let pipewire_fd = proxy
            .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
            .await
            .map_err(|err| {
                CoreError::Video(format!("portal open_pipe_wire_remote failed: {err}"))
            })?;

        info!(
            node_id,
            pipewire_serial,
            natural_size = ?natural_size,
            "acquired XDG Desktop Portal PipeWire screencast stream"
        );

        Ok(Self {
            session,
            pipewire_fd,
            node_id,
            pipewire_serial,
            natural_size,
            source_type,
        })
    }

    /// PipeWire stream `node_id` returned by the portal.
    #[must_use]
    pub const fn node_id(&self) -> u32 {
        self.node_id
    }

    /// Optional monotonic PipeWire object serial (portal v6+).
    #[must_use]
    pub const fn pipewire_serial(&self) -> Option<u64> {
        self.pipewire_serial
    }

    /// Natural capture resolution reported by the portal, if available.
    #[must_use]
    pub const fn natural_size(&self) -> Option<(u32, u32)> {
        self.natural_size
    }

    /// Borrow the raw PipeWire file descriptor for `pipewiresrc fd={fd}`.
    ///
    /// Safety invariant: `PortalCaptureSession` MUST stay alive until the GStreamer pipeline
    /// reaches `gst::State::Null`.
    #[must_use]
    pub fn raw_fd(&self) -> i32 {
        self.pipewire_fd.as_raw_fd()
    }

    /// Close the XDG Desktop Portal session cleanly.
    pub async fn close(&self) {
        if let Err(err) = self.session.close().await {
            debug!(error = %err, "portal screencast session close returned error");
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl PortalCaptureSession {
    /// Acquire a native screen capture handle on macOS (`avfvideosrc`) or Windows
    /// (`d3d11screencapturesrc` / `gdiscreencapturesrc`).
    pub async fn acquire(source_kind: StreamSourceKind) -> CoreResult<Self> {
        if source_kind == StreamSourceKind::SyntheticTestCard {
            return Err(CoreError::Video(
                "SyntheticTestCard does not require a platform capture session".to_string(),
            ));
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            info!(source = ?source_kind, "using native OS GStreamer screen capture source");
            Ok(Self)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(CoreError::Video(
                "screen capture is supported on Linux (PipeWire), macOS (AVFoundation), and Windows (D3D11/GDI)"
                    .to_string(),
            ))
        }
    }

    /// Natural capture resolution hint (determined dynamically by GStreamer on macOS/Windows).
    #[must_use]
    pub const fn natural_size(&self) -> Option<(u32, u32)> {
        None
    }

    /// Close the native capture token (no-op on macOS/Windows).
    pub async fn close(&self) {}
}

/// Encoded (and DAVE-encrypted) video frame emitted when running a broadcast pipeline in
/// channel-tap mode (used for diagnostics and headless loopback integration tests).
#[derive(Debug, Clone)]
pub struct EncodedVideoPacket {
    /// Video codec of the underlying elementary stream.
    pub codec: VideoCodec,
    /// Encoded (and DAVE-encrypted when active) payload bytes.
    pub payload: Vec<u8>,
    /// Presentation timestamp (`PTS`) from GStreamer.
    pub pts: Option<gst::ClockTime>,
    /// Frame duration from GStreamer.
    pub duration: Option<gst::ClockTime>,
    /// `true` if this buffer is a keyframe (sync point / IDR).
    pub is_keyframe: bool,
}

/// Output destination for a [`BroadcastPipeline`].
pub enum BroadcastOutput {
    /// Live WebRTC outbound branch: `rtppay -> rtp caps -> webrtcbin`.
    WebRtc {
        /// RTP payload type negotiated via `SELECT_PROTOCOL` (default matches codec).
        payload_type: Option<u8>,
    },
    /// Encoded packet tap after DAVE encryption (`AppSink` -> bounded Tokio channel).
    EncodedChannel {
        /// Channel receiving encoded (and DAVE-encrypted) packets.
        tx: mpsc::Sender<EncodedVideoPacket>,
        /// When `true`, passes frames through `rtppay -> rtpdepay` before `AppSink` to verify
        /// full RTP packetization/depacketization compatibility alongside DAVE E2EE.
        through_rtp: bool,
    },
}

/// Configuration for building a Go Live outbound [`BroadcastPipeline`].
pub struct BroadcastConfig {
    /// Canonical Go Live stream key (`guild:<g>:<c>:<u>` or `call:<c>:<u>`).
    pub stream_key: String,
    /// Capture source (`PortalPipeWire`, `Monitor`, `ApplicationWindow`, or `SyntheticTestCard`).
    pub source_kind: StreamSourceKind,
    /// Requested resolution, framerate, and max bitrate.
    pub quality: StreamQuality,
    /// Optional preferred video codec (falls back dynamically if unavailable).
    pub preferred_codec: Option<VideoCodec>,
    /// Prefer software encoder over hardware encoder (useful for deterministic headless CI tests).
    pub prefer_software_encoder: bool,
    /// For `SyntheticTestCard`, optional finite frame count (`None` = continuous live stream).
    pub synthetic_num_buffers: Option<i32>,
    /// For `SyntheticTestCard`, whether `videotestsrc` paces frames in real time (`is-live`).
    pub synthetic_is_live: bool,
    /// DAVE E2EE encryption context shared with Stream RTC signaling.
    pub dave: DaveVideoContext,
}

impl std::fmt::Debug for BroadcastConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BroadcastConfig")
            .field("stream_key", &self.stream_key)
            .field("source_kind", &self.source_kind)
            .field("quality", &self.quality)
            .field("preferred_codec", &self.preferred_codec)
            .field("prefer_software_encoder", &self.prefer_software_encoder)
            .field("synthetic_num_buffers", &self.synthetic_num_buffers)
            .field("dave", &self.dave)
            .finish()
    }
}

/// Running Go Live broadcast pipeline capturing PipeWire (or synthetic test cards), encoding via
/// dynamically probed HW/SW encoders, encrypting frames with DAVE, and feeding `webrtcbin` or an
/// encoded loopback tap.
pub struct BroadcastPipeline {
    pipeline: gst::Pipeline,
    encoder: gst::Element,
    webrtcbin: Option<gst::Element>,
    encoder_selection: EncoderSelection,
    portal_session: Option<PortalCaptureSession>,
    keyframe_requests: Arc<AtomicU64>,
    frames_encoded: Arc<AtomicU64>,
    stream_key: String,
    stopped: AtomicBool,
}

impl std::fmt::Debug for BroadcastPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BroadcastPipeline")
            .field("stream_key", &self.stream_key)
            .field("encoder", &self.encoder_selection)
            .field(
                "keyframe_requests",
                &self.keyframe_requests.load(Ordering::Relaxed),
            )
            .field("frames_encoded", &self.frames_encoded.load(Ordering::Relaxed))
            .finish()
    }
}

impl BroadcastPipeline {
    /// Acquire screen capture (via XDG Desktop Portal + PipeWire unless `SyntheticTestCard`) and
    /// launch the GStreamer broadcast pipeline.
    pub async fn start(config: BroadcastConfig, output: BroadcastOutput) -> CoreResult<Self> {
        let portal_session = match config.source_kind {
            StreamSourceKind::SyntheticTestCard => None,
            StreamSourceKind::PortalPipeWire
            | StreamSourceKind::Monitor
            | StreamSourceKind::ApplicationWindow => {
                Some(PortalCaptureSession::acquire(config.source_kind).await?)
            }
        };

        tokio::task::spawn_blocking(move || Self::start_with_portal(config, portal_session, output))
            .await
            .map_err(|err| CoreError::Video(format!("broadcast start task panicked: {err}")))?
    }

    /// Launch a broadcast pipeline from an already-acquired [`PortalCaptureSession`] (or `None`
    /// for `SyntheticTestCard`).
    pub fn start_with_portal(
        config: BroadcastConfig,
        portal_session: Option<PortalCaptureSession>,
        output: BroadcastOutput,
    ) -> CoreResult<Self> {
        ensure_gst_init()?;

        let encoder_selection = if config.prefer_software_encoder {
            probe_software_encoder(config.preferred_codec)
                .or_else(|_| probe_best_encoder(config.preferred_codec))?
        } else {
            probe_best_encoder(config.preferred_codec)?
        };

        let pipeline = gst::Pipeline::builder()
            .name(format!("broadcast_{}", sanitize_gst_name(&config.stream_key)))
            .build();

        // 1. Source element: PipeWire portal stream or synthetic SMPTE test card.
        let source = match (&config.source_kind, &portal_session) {
            (StreamSourceKind::SyntheticTestCard, _) => {
                let src = gst::ElementFactory::make("videotestsrc")
                    .name("broadcast_test_src")
                    .build()
                    .map_err(|err| {
                        CoreError::Video(format!("failed to create videotestsrc: {err}"))
                    })?;
                if src.find_property("pattern").is_some() {
                    src.set_property_from_str("pattern", "smpte");
                }
                if src.find_property("is-live").is_some() {
                    src.set_property("is-live", config.synthetic_is_live);
                }
                if let Some(count) = config.synthetic_num_buffers
                    && src.find_property("num-buffers").is_some()
                {
                    src.set_property("num-buffers", count);
                }
                src
            }
            (_, Some(portal)) => {
                #[cfg(target_os = "linux")]
                {
                    let src = gst::ElementFactory::make("pipewiresrc")
                        .name("broadcast_pipewire_src")
                        .build()
                        .map_err(|err| {
                            CoreError::Video(format!("failed to create pipewiresrc: {err}"))
                        })?;
                    src.set_property("fd", portal.raw_fd());
                    src.set_property("do-timestamp", true);
                    if let Some(serial) = portal.pipewire_serial()
                        && src.find_property("target-object").is_some()
                    {
                        src.set_property("target-object", serial.to_string());
                    } else {
                        src.set_property("path", portal.node_id().to_string());
                    }
                    src
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = portal;
                    let src = gst::ElementFactory::make("avfvideosrc")
                        .name("broadcast_avf_src")
                        .build()
                        .map_err(|err| {
                            CoreError::Video(format!("failed to create avfvideosrc: {err}"))
                        })?;
                    if src.find_property("capture-screen").is_some() {
                        src.set_property("capture-screen", true);
                    }
                    if src.find_property("capture-screen-cursor").is_some() {
                        src.set_property("capture-screen-cursor", true);
                    }
                    src
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = portal;
                    gst::ElementFactory::make("d3d11screencapturesrc")
                        .name("broadcast_win_src")
                        .build()
                        .or_else(|_| {
                            gst::ElementFactory::make("gdiscreencapturesrc")
                                .name("broadcast_win_src")
                                .build()
                        })
                        .map_err(|err| {
                            CoreError::Video(format!(
                                "failed to create Windows screen capture source: {err}"
                            ))
                        })?
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
                {
                    let _ = portal;
                    return Err(CoreError::Video(
                        "native screen capture is unavailable on this OS".to_string(),
                    ));
                }
            }
            (_, None) => {
                return Err(CoreError::Video(
                    "PipeWire broadcast requires an active PortalCaptureSession".to_string(),
                ));
            }
        };

        // 2. Capture conditioning: videoconvert -> videoscale -> videorate -> raw caps -> videoconvert
        let pre_convert = gst::ElementFactory::make("videoconvert")
            .name("broadcast_pre_convert")
            .build()
            .map_err(|err| CoreError::Video(format!("failed to create videoconvert: {err}")))?;
        let videoscale = gst::ElementFactory::make("videoscale")
            .name("broadcast_videoscale")
            .build()
            .map_err(|err| CoreError::Video(format!("failed to create videoscale: {err}")))?;
        let videorate = gst::ElementFactory::make("videorate")
            .name("broadcast_videorate")
            .property("skip-to-first", true)
            .build()
            .map_err(|err| CoreError::Video(format!("failed to create videorate: {err}")))?;
        let raw_capsfilter = gst::ElementFactory::make("capsfilter")
            .name("broadcast_raw_caps")
            .build()
            .map_err(|err| CoreError::Video(format!("failed to create capsfilter: {err}")))?;

        let width = config.quality.width.max(16) as i32;
        let height = config.quality.height.max(16) as i32;
        let fps = i32::from(config.quality.fps.clamp(1, 60));
        let raw_caps = gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("width", width)
            .field("height", height)
            .field("framerate", gst::Fraction::new(fps, 1))
            .build();
        raw_capsfilter.set_property("caps", &raw_caps);

        let enc_convert = gst::ElementFactory::make("videoconvert")
            .name("broadcast_enc_convert")
            .build()
            .map_err(|err| CoreError::Video(format!("failed to create enc videoconvert: {err}")))?;

        // 3. Dynamically probed encoder + RTCP PLI/FIR upstream ForceKeyUnit telemetry probe
        let encoder = create_configured_encoder(&encoder_selection, config.quality)?;
        let keyframe_requests = Arc::new(AtomicU64::new(0));
        let frames_encoded = Arc::new(AtomicU64::new(0));
        install_keyframe_and_frame_probes(
            &encoder,
            Arc::clone(&keyframe_requests),
            Arc::clone(&frames_encoded),
        )?;

        // 4. Encoded elementary-stream caps (enforces Annex-B byte-stream for H.264 DAVE parsing)
        let encoded_capsfilter = gst::ElementFactory::make("capsfilter")
            .name("broadcast_encoded_caps")
            .build()
            .map_err(|err| {
                CoreError::Video(format!("failed to create encoded capsfilter: {err}"))
            })?;
        let encoded_caps = encoder_selection
            .codec
            .encoded_caps_str()
            .parse::<gst::Caps>()
            .map_err(|err| {
                CoreError::Video(format!(
                    "invalid encoded caps for {:?}: {err}",
                    encoder_selection.codec
                ))
            })?;
        encoded_capsfilter.set_property("caps", &encoded_caps);

        // 5. DAVE E2EE media frame encryptor pad probe
        let dave_encrypt = gst::ElementFactory::make("identity")
            .name("dave_encrypt")
            .build()
            .map_err(|err| {
                CoreError::Video(format!("failed to create dave_encrypt identity: {err}"))
            })?;
        let _enc_probe =
            install_dave_encrypt_probe(&dave_encrypt, config.dave.clone(), encoder_selection.codec)?;

        pipeline
            .add_many([
                &source,
                &pre_convert,
                &videoscale,
                &videorate,
                &raw_capsfilter,
                &enc_convert,
                &encoder,
                &encoded_capsfilter,
                &dave_encrypt,
            ])
            .map_err(|err| {
                CoreError::Video(format!("failed to add broadcast capture/encode elements: {err}"))
            })?;

        gst::Element::link_many([
            &source,
            &pre_convert,
            &videoscale,
            &videorate,
            &raw_capsfilter,
            &enc_convert,
            &encoder,
            &encoded_capsfilter,
            &dave_encrypt,
        ])
        .map_err(|err| {
            CoreError::Video(format!("failed to link broadcast capture/encode pipeline: {err}"))
        })?;

        // 6. Output branch: live WebRTC (`rtppay -> webrtcbin`) or encoded channel tap (`AppSink`)
        let webrtcbin = match output {
            BroadcastOutput::WebRtc { payload_type } => {
                let pt = payload_type.unwrap_or_else(|| encoder_selection.codec.default_payload_type());
                let payloader = create_configured_payloader(encoder_selection.codec, pt)?;
                let rtp_capsfilter = gst::ElementFactory::make("capsfilter")
                    .name("broadcast_rtp_caps")
                    .build()
                    .map_err(|err| {
                        CoreError::Video(format!("failed to create RTP capsfilter: {err}"))
                    })?;
                let rtp_caps = encoder_selection
                    .codec
                    .rtp_caps_str(pt)
                    .parse::<gst::Caps>()
                    .map_err(|err| CoreError::Video(format!("invalid RTP caps: {err}")))?;
                rtp_capsfilter.set_property("caps", &rtp_caps);

                let queue = gst::ElementFactory::make("queue")
                    .name("broadcast_rtp_queue")
                    .build()
                    .map_err(|err| CoreError::Video(format!("failed to create queue: {err}")))?;
                if queue.find_property("leaky").is_some() {
                    queue.set_property_from_str("leaky", "downstream");
                }
                if queue.find_property("max-size-buffers").is_some() {
                    queue.set_property("max-size-buffers", 8u32);
                }

                let webrtc = gst::ElementFactory::make("webrtcbin")
                    .name("broadcast_webrtcbin")
                    .build()
                    .map_err(|err| {
                        CoreError::Video(format!("failed to create webrtcbin: {err}"))
                    })?;
                if webrtc.find_property("bundle-policy").is_some() {
                    webrtc.set_property_from_str("bundle-policy", "max-bundle");
                }

                pipeline
                    .add_many([&payloader, &rtp_capsfilter, &queue, &webrtc])
                    .map_err(|err| {
                        CoreError::Video(format!("failed to add WebRTC output branch: {err}"))
                    })?;
                gst::Element::link_many([
                    &dave_encrypt,
                    &payloader,
                    &rtp_capsfilter,
                    &queue,
                    &webrtc,
                ])
                .map_err(|err| {
                    CoreError::Video(format!("failed to link WebRTC output branch: {err}"))
                })?;

                Some(webrtc)
            }
            BroadcastOutput::EncodedChannel { tx, through_rtp } => {
                let appsink = gst_app::AppSink::builder()
                    .name("broadcast_encoded_sink")
                    .sync(false)
                    .max_buffers(32)
                    .drop(false)
                    .build();

                let codec = encoder_selection.codec;
                let tx_slot = Arc::new(std::sync::Mutex::new(Some(tx)));
                let tx_for_sample = Arc::clone(&tx_slot);
                let tx_for_eos = Arc::clone(&tx_slot);
                appsink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |sink| {
                            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                            let Some(buffer) = sample.buffer() else {
                                return Ok(gst::FlowSuccess::Ok);
                            };
                            let Ok(map) = buffer.map_readable() else {
                                return Ok(gst::FlowSuccess::Ok);
                            };
                            let packet = EncodedVideoPacket {
                                codec,
                                payload: map.as_slice().to_vec(),
                                pts: buffer.pts(),
                                duration: buffer.duration(),
                                is_keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
                            };
                            drop(map);
                            let sender = tx_for_sample
                                .lock()
                                .ok()
                                .and_then(|guard| guard.as_ref().cloned());
                            let Some(sender) = sender else {
                                return Err(gst::FlowError::Eos);
                            };
                            if sender.blocking_send(packet).is_err() {
                                return Err(gst::FlowError::Eos);
                            }
                            Ok(gst::FlowSuccess::Ok)
                        })
                        .eos(move |_sink| {
                            if let Ok(mut guard) = tx_for_eos.lock() {
                                guard.take();
                            }
                        })
                        .build(),
                );

                let appsink_elem = appsink.upcast_ref::<gst::Element>();
                if through_rtp {
                    let pt = encoder_selection.codec.default_payload_type();
                    let pay = create_configured_payloader(encoder_selection.codec, pt)?;
                    let depay = gst::ElementFactory::make(encoder_selection.codec.rtp_depayloader_name())
                        .name("broadcast_loopback_depay")
                        .build()
                        .map_err(|err| {
                            CoreError::Video(format!("failed to create loopback depayloader: {err}"))
                        })?;
                    if encoder_selection.codec == VideoCodec::H264 {
                        install_h264_strip_sprop_probe(&depay)?;
                    }
                    let depay_capsfilter = gst::ElementFactory::make("capsfilter")
                        .name("broadcast_loopback_depay_caps")
                        .build()
                        .map_err(|err| {
                            CoreError::Video(format!("failed to create loopback depay caps: {err}"))
                        })?;
                    depay_capsfilter.set_property("caps", &encoded_caps);
                    pipeline
                        .add_many([&pay, &depay, &depay_capsfilter, appsink_elem])
                        .map_err(|err| {
                            CoreError::Video(format!("failed to add RTP tap branch: {err}"))
                        })?;
                    gst::Element::link_many([&dave_encrypt, &pay, &depay, &depay_capsfilter, appsink_elem]).map_err(
                        |err| CoreError::Video(format!("failed to link RTP tap branch: {err}")),
                    )?;
                } else {
                    pipeline.add(appsink_elem).map_err(|err| {
                        CoreError::Video(format!("failed to add encoded appsink: {err}"))
                    })?;
                    dave_encrypt.link(appsink_elem).map_err(|err| {
                        CoreError::Video(format!("failed to link encoded appsink: {err}"))
                    })?;
                }
                None
            }
        };

        pipeline.set_state(gst::State::Playing).map_err(|err| {
            CoreError::Video(format!("failed to start Go Live broadcast pipeline: {err}"))
        })?;

        info!(
            stream_key = %config.stream_key,
            encoder = encoder_selection.element_name,
            codec = ?encoder_selection.codec,
            hardware = encoder_selection.hardware_accelerated,
            width,
            height,
            fps,
            "started Go Live broadcast pipeline"
        );

        Ok(Self {
            pipeline,
            encoder,
            webrtcbin,
            encoder_selection,
            portal_session,
            keyframe_requests,
            frames_encoded,
            stream_key: config.stream_key,
            stopped: AtomicBool::new(false),
        })
    }

    /// Trigger an immediate upstream `GstForceKeyUnit` event on the encoder (matching RTCP PLI/FIR
    /// keyframe requests when a new viewer joins or recovers from packet loss).
    pub fn request_keyframe(&self) -> CoreResult<()> {
        let src_pad = self.encoder.static_pad("src").ok_or_else(|| {
            CoreError::Video("broadcast encoder missing static src pad".to_string())
        })?;
        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if !src_pad.send_event(event) {
            return Err(CoreError::Video(
                "broadcast encoder rejected upstream ForceKeyUnit event".to_string(),
            ));
        }
        Ok(())
    }

    /// Create a local WebRTC SDP offer on this broadcast pipeline's `webrtcbin` (non-blocking).
    pub async fn create_webrtc_offer(&self) -> CoreResult<String> {
        let webrtc = self.webrtcbin.as_ref().ok_or_else(|| {
            CoreError::Video("create_webrtc_offer requires a WebRTC broadcast pipeline".to_string())
        })?;
        webrtc_create_sdp(webrtc, "create-offer", "offer").await
    }

    /// Apply a remote SDP answer (from Discord `SESSION_DESCRIPTION`) to `webrtcbin`.
    pub async fn set_remote_sdp(
        &self,
        sdp_type: gst_webrtc::WebRTCSDPType,
        sdp_text: &str,
    ) -> CoreResult<()> {
        let webrtc = self.webrtcbin.as_ref().ok_or_else(|| {
            CoreError::Video("set_remote_sdp requires a WebRTC broadcast pipeline".to_string())
        })?;
        webrtc_set_remote_sdp(webrtc, sdp_type, sdp_text).await
    }

    /// Feed a remote ICE candidate into the broadcast `webrtcbin`.
    pub fn add_ice_candidate(&self, mline_index: u32, candidate: &str) -> CoreResult<()> {
        let webrtc = self.webrtcbin.as_ref().ok_or_else(|| {
            CoreError::Video("add_ice_candidate requires a WebRTC broadcast pipeline".to_string())
        })?;
        webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline_index, &candidate]);
        Ok(())
    }

    /// Wait for the pipeline bus to observe EOS or an error (blocking helper for finite synthetic
    /// runs; invoke inside `spawn_blocking` when called from async contexts).
    pub fn wait_for_eos_blocking(&self, timeout: Duration) -> CoreResult<()> {
        let Some(bus) = self.pipeline.bus() else {
            return Ok(());
        };
        let gst_timeout =
            gst::ClockTime::from_nseconds(timeout.as_nanos().min(u64::MAX as u128) as u64);
        match bus.timed_pop_filtered(
            Some(gst_timeout),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(_) => Ok(()),
                gst::MessageView::Error(err) => Err(CoreError::Video(format!(
                    "broadcast pipeline bus error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                ))),
                _ => Ok(()),
            },
            None => Err(CoreError::Video(format!(
                "broadcast pipeline timed out waiting for EOS after {timeout:?}"
            ))),
        }
    }

    /// Return the dynamically selected encoder and codec.
    #[must_use]
    pub const fn encoder_selection(&self) -> &EncoderSelection {
        &self.encoder_selection
    }

    /// Return the count of RTCP PLI/FIR or explicit `ForceKeyUnit` requests handled.
    #[must_use]
    pub fn keyframe_request_count(&self) -> u64 {
        self.keyframe_requests.load(Ordering::Relaxed)
    }

    /// Return the number of encoded video frames produced by the encoder so far.
    #[must_use]
    pub fn frames_encoded(&self) -> u64 {
        self.frames_encoded.load(Ordering::Relaxed)
    }

    /// Gracefully stop the GStreamer pipeline FIRST (releasing `pipewiresrc`), then close the
    /// XDG Desktop Portal session cleanly without blocking Tokio.
    pub async fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        let pipeline = self.pipeline.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = pipeline.set_state(gst::State::Null);
        })
        .await;

        if let Some(portal) = self.portal_session.take() {
            portal.close().await;
            drop(portal);
        }
    }
}

impl Drop for BroadcastPipeline {
    fn drop(&mut self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
        // `portal_session` (and its `OwnedFd`) drops strictly AFTER `pipeline.set_state(Null)`.
    }
}

fn install_keyframe_and_frame_probes(
    encoder: &gst::Element,
    keyframe_requests: Arc<AtomicU64>,
    frames_encoded: Arc<AtomicU64>,
) -> CoreResult<()> {
    let src_pad = encoder.static_pad("src").ok_or_else(|| {
        CoreError::Video("broadcast encoder missing static src pad for probes".to_string())
    })?;

    src_pad
        .add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_pad, info| {
            if let Some(event) = info.event()
                && gst_video::UpstreamForceKeyUnitEvent::parse(event).is_ok()
            {
                let count = keyframe_requests.fetch_add(1, Ordering::Relaxed) + 1;
                debug!(
                    keyframe_requests = count,
                    "handling RTCP PLI/FIR ForceKeyUnit request on Go Live encoder"
                );
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| {
            CoreError::Video("failed to install ForceKeyUnit telemetry probe".to_string())
        })?;

    src_pad
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            frames_encoded.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| CoreError::Video("failed to install encoded frame probe".to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbs_core::Snowflake;

    #[tokio::test]
    async fn synthetic_broadcast_handles_pli_keyframe_request_and_encodes() {
        let (tx, mut rx) = mpsc::channel(32);
        let dave = DaveVideoContext::passthrough(Snowflake::new(10), Snowflake::new(20));
        let config = BroadcastConfig {
            stream_key: "call:20:10".to_string(),
            source_kind: StreamSourceKind::SyntheticTestCard,
            quality: StreamQuality {
                width: 320,
                height: 240,
                fps: 30,
                max_bitrate_kbps: 1_000,
            },
            preferred_codec: Some(VideoCodec::Vp8),
            prefer_software_encoder: true,
            synthetic_num_buffers: Some(8),
            synthetic_is_live: false,
            dave,
        };

        let mut broadcast = BroadcastPipeline::start(
            config,
            BroadcastOutput::EncodedChannel {
                tx,
                through_rtp: false,
            },
        )
        .await
        .expect("start synthetic broadcast");

        // Trigger explicit PLI/FIR ForceKeyUnit request cleanly.
        let _ = broadcast.request_keyframe();

        let mut received = 0usize;
        while let Ok(Some(pkt)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            assert!(!pkt.payload.is_empty());
            received += 1;
            if received >= 8 {
                break;
            }
        }
        assert!(received >= 5, "expected encoded frames, got {received}");
        assert!(broadcast.frames_encoded() >= 5);
        broadcast.stop().await;
    }
}
