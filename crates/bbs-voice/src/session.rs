//! Discord Voice Gateway v8 session, Guild + 1:1 DM / Group DM call coordination,
//! UDP media transport, DAVE MLS E2EE integration, and [`VoiceEngine`] implementation.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bbs_core::error::{CoreError, CoreResult};
use bbs_core::events::BackendEvent;
use bbs_core::model::{
    Attachment, CallRingState, DaveSessionState, Snowflake, VoiceChannelStatus,
    VoiceConnectionState, VoiceParticipant, VoicePlaybackState,
};
use bbs_core::traits::VoiceEngine;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use crate::audio_io::{AudioBackendMode, AudioIoHandle, CaptureQueue, PlaybackQueue};
use crate::dave::{
    DaveController, DaveOutboundMessage, DaveWireFrame, MAX_DAVE_PROTOCOL_VERSION,
    parse_binary_dave_frame, parse_json_dave_frame,
};
use crate::error::{VoiceError, VoiceResult};
use crate::mixer::{
    AudioMixer, DEFAULT_JITTER_CUSHION_FRAMES, EncodedMicPacket, FRAME_SAMPLES, InputMode,
    MAX_VOLUME_PERCENT, MIN_RMS_DB, MicCaptureProcessor,
};
use crate::player::VoiceMessagePlayer;
use crate::rtp::{
    RtpPacketizer, RtpTransportCipher, TransportEncryptionMode, build_ip_discovery_request,
    is_rtcp_packet, parse_ip_discovery_response,
};

/// Voice Gateway Op 0: `IDENTIFY` (C -> S).
pub const VOICE_OP_IDENTIFY: u8 = 0;
/// Voice Gateway Op 1: `SELECT_PROTOCOL` (C -> S).
pub const VOICE_OP_SELECT_PROTOCOL: u8 = 1;
/// Voice Gateway Op 2: `READY` (S -> C).
pub const VOICE_OP_READY: u8 = 2;
/// Voice Gateway Op 3: `HEARTBEAT` (C -> S).
pub const VOICE_OP_HEARTBEAT: u8 = 3;
/// Voice Gateway Op 4: `SESSION_DESCRIPTION` (S -> C).
pub const VOICE_OP_SESSION_DESCRIPTION: u8 = 4;
/// Voice Gateway Op 5: `SPEAKING` (Bidirectional).
pub const VOICE_OP_SPEAKING: u8 = 5;
/// Voice Gateway Op 6: `HEARTBEAT_ACK` (S -> C).
pub const VOICE_OP_HEARTBEAT_ACK: u8 = 6;
/// Voice Gateway Op 7: `RESUME` (C -> S).
pub const VOICE_OP_RESUME: u8 = 7;
/// Voice Gateway Op 8: `HELLO` (S -> C).
pub const VOICE_OP_HELLO: u8 = 8;
/// Voice Gateway Op 9: `RESUMED` (S -> C).
pub const VOICE_OP_RESUMED: u8 = 9;
/// Voice Gateway Op 11: `CLIENTS_CONNECT` (S -> C).
pub const VOICE_OP_CLIENTS_CONNECT: u8 = 11;
/// Voice Gateway Op 13: `CLIENT_DISCONNECT` (S -> C).
pub const VOICE_OP_CLIENT_DISCONNECT: u8 = 13;

/// Speaking bitflag: standard microphone audio transmission (`1 << 0`).
pub const SPEAKING_FLAG_MICROPHONE: u32 = 1 << 0;
/// Speaking bitflag: soundshare audio transmission (`1 << 1`).
pub const SPEAKING_FLAG_SOUNDSHARE: u32 = 1 << 1;
/// Speaking bitflag: priority speaker (`1 << 2`).
pub const SPEAKING_FLAG_PRIORITY: u32 = 1 << 2;

/// Default signaling timeout waiting for main Gateway `VOICE_STATE_UPDATE` + `VOICE_SERVER_UPDATE`.
pub const DEFAULT_SIGNALING_TIMEOUT: Duration = Duration::from_secs(15);

/// Main Gateway Op 4 (`UPDATE_VOICE_STATE`) command sent when joining/leaving/muting/deafening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceStateUpdateCommand {
    /// Target guild Snowflake (`None` for 1:1 DM and Group DM calls).
    pub guild_id: Option<Snowflake>,
    /// Target voice or DM/Group DM channel Snowflake (`None` disconnects from voice).
    pub channel_id: Option<Snowflake>,
    /// Local microphone mute state.
    pub self_mute: bool,
    /// Local audio output deafen state.
    pub self_deaf: bool,
}

/// Inbound main Gateway `VOICE_STATE_UPDATE` dispatch payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceStateUpdateEvent {
    /// Owning guild Snowflake (`None` for 1:1 DM and Group DM calls).
    pub guild_id: Option<Snowflake>,
    /// Current channel Snowflake (`None` means user left voice).
    pub channel_id: Option<Snowflake>,
    /// Affected user Snowflake.
    pub user_id: Snowflake,
    /// Gateway voice session ID (opaque, redacted in logs).
    pub session_id: String,
    /// User self-mute flag.
    pub self_mute: bool,
    /// User self-deaf flag.
    pub self_deaf: bool,
    /// Server-enforced mute flag.
    pub mute: bool,
    /// Server-enforced deafen flag.
    pub deaf: bool,
    /// Whether the user is broadcasting a Go Live stream.
    pub self_stream: bool,
    /// Optional display name if resolved by caller.
    pub display_name: Option<String>,
}

/// Inbound main Gateway `VOICE_SERVER_UPDATE` dispatch payload (supports both Guild and DM calls).
#[derive(Clone)]
pub struct VoiceServerUpdateEvent {
    /// Owning guild Snowflake (`None` for 1:1 DM / Group DM calls).
    pub guild_id: Option<Snowflake>,
    /// DM / Group DM channel Snowflake (populated by Discord when `guild_id` is `None`).
    pub channel_id: Option<Snowflake>,
    /// Voice server WebSocket host/endpoint (e.g. `"us-east123.discord.media:443"` or `"ws://127.0.0.1:9000"`).
    pub endpoint: Option<String>,
    /// Ephemeral voice gateway authentication token (zeroized on drop; never logged).
    pub token: Zeroizing<String>,
}

impl std::fmt::Debug for VoiceServerUpdateEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceServerUpdateEvent")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field("endpoint", &self.endpoint)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Inbound main Gateway `CALL_CREATE` / `CALL_UPDATE` event for 1:1 DM & Group DM calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallUpdateEvent {
    /// DM or Group DM channel Snowflake.
    pub channel_id: Snowflake,
    /// User IDs currently being rung.
    pub ringing: Vec<Snowflake>,
    /// User IDs currently connected to the call.
    pub connected_user_ids: Vec<Snowflake>,
    /// Optional caller ID (when a remote user initiates the ring).
    pub caller_id: Option<Snowflake>,
    /// Optional caller display name.
    pub caller_name: Option<String>,
    /// Whether this call belongs to a multi-user Group DM (`ChannelKind::GroupDm`).
    pub is_group_dm: bool,
}

/// Resolved credentials needed to connect to Discord Voice Gateway v8.
#[derive(Clone)]
pub struct VoiceGatewayCredentials {
    /// Owning guild Snowflake (`None` for 1:1 DM / Group DM calls).
    pub guild_id: Option<Snowflake>,
    /// Voice or DM/Group DM channel Snowflake.
    pub channel_id: Snowflake,
    /// Optional human-readable channel/call display name.
    pub channel_name: Option<String>,
    /// Authenticated local user Snowflake.
    pub user_id: Snowflake,
    /// Voice session ID from `VOICE_STATE_UPDATE`.
    pub session_id: Zeroizing<String>,
    /// Voice token from `VOICE_SERVER_UPDATE`.
    pub token: Zeroizing<String>,
    /// Voice gateway endpoint (`host[:port]` or explicit `ws://`/`wss://` URL).
    pub endpoint: String,
}

impl std::fmt::Debug for VoiceGatewayCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceGatewayCredentials")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field("user_id", &self.user_id)
            .field("endpoint", &self.endpoint)
            .field("session_id", &"[REDACTED]")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl VoiceGatewayCredentials {
    /// Compute the Voice Gateway `server_id` field:
    /// - For guild voice/stage channels: `guild_id`.
    /// - For 1:1 DM and Group DM calls (`guild_id == None`): `channel_id`.
    #[must_use]
    pub const fn server_id(&self) -> Snowflake {
        match self.guild_id {
            Some(gid) => gid,
            None => self.channel_id,
        }
    }

    /// Build the Voice Gateway v8 WebSocket URL (`wss://<endpoint>/?v=8`).
    ///
    /// Preserves explicit `ws://` / `wss://` prefixes so local loopback tests can use `ws://127.0.0.1:...`.
    #[must_use]
    pub fn gateway_ws_url(&self) -> String {
        let trimmed = self.endpoint.trim().trim_end_matches('/');
        let base = if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
            trimmed.to_string()
        } else {
            format!("wss://{trimmed}")
        };
        if base.contains("v=8") {
            base
        } else if base.contains('?') {
            format!("{base}&v=8")
        } else {
            format!("{base}/?v=8")
        }
    }
}

/// Pluggable bridge sending main Discord Gateway Op 4 (`UPDATE_VOICE_STATE`) frames.
#[async_trait]
pub trait VoiceSignalingBridge: Send + Sync {
    /// Transmit an `UPDATE_VOICE_STATE` command over the main Discord Gateway.
    async fn send_voice_state_update(&self, cmd: VoiceStateUpdateCommand) -> VoiceResult<()>;
}

/// Configuration for [`DiscordVoiceEngine`].
#[derive(Debug, Clone)]
pub struct VoiceEngineConfig {
    /// Local authenticated Discord user Snowflake.
    pub local_user_id: Snowflake,
    /// Local user display name for voice participant snapshots.
    pub local_display_name: String,
    /// Initial microphone input mode (VAD or Push-to-Talk).
    pub input_mode: InputMode,
    /// Audio hardware backend mode (`Auto` probes `cpal`, `Headless` skips soundcard probing).
    pub audio_mode: AudioBackendMode,
    /// Jitter buffer prefill cushion in 20 ms frames.
    pub jitter_cushion_frames: usize,
    /// Timeout waiting for main Gateway voice server signaling during `join_voice`.
    pub signaling_timeout: Duration,
}

impl VoiceEngineConfig {
    /// Construct a default configuration for `local_user_id`.
    #[must_use]
    pub fn new(local_user_id: Snowflake, local_display_name: impl Into<String>) -> Self {
        Self {
            local_user_id,
            local_display_name: local_display_name.into(),
            input_mode: InputMode::default(),
            audio_mode: AudioBackendMode::Auto,
            jitter_cushion_frames: DEFAULT_JITTER_CUSHION_FRAMES,
            signaling_timeout: DEFAULT_SIGNALING_TIMEOUT,
        }
    }
}

/// Build the Voice Gateway v8 `IDENTIFY` (Op 0) JSON payload.
#[must_use]
pub fn build_identify_payload(creds: &VoiceGatewayCredentials) -> Value {
    json!({
        "op": VOICE_OP_IDENTIFY,
        "d": {
            "server_id": creds.server_id().to_string(),
            "user_id": creds.user_id.to_string(),
            "session_id": creds.session_id.as_str(),
            "token": creds.token.as_str(),
            "max_dave_protocol_version": MAX_DAVE_PROTOCOL_VERSION,
        }
    })
}

/// Build the Voice Gateway v8 `RESUME` (Op 7) JSON payload.
#[must_use]
pub fn build_resume_payload(creds: &VoiceGatewayCredentials, seq_ack: Option<u64>) -> Value {
    let mut data = json!({
        "server_id": creds.server_id().to_string(),
        "session_id": creds.session_id.as_str(),
        "token": creds.token.as_str(),
    });
    if let Some(seq) = seq_ack
        && let Some(obj) = data.as_object_mut()
    {
        obj.insert("seq_ack".to_string(), json!(seq));
    }
    json!({
        "op": VOICE_OP_RESUME,
        "d": data,
    })
}

/// Build the Voice Gateway v8 `SELECT_PROTOCOL` (Op 1) JSON payload.
#[must_use]
pub fn build_select_protocol_payload(
    external_addr: SocketAddr,
    mode: TransportEncryptionMode,
) -> Value {
    json!({
        "op": VOICE_OP_SELECT_PROTOCOL,
        "d": {
            "protocol": "udp",
            "data": {
                "address": external_addr.ip().to_string(),
                "port": external_addr.port(),
                "mode": mode.as_str(),
            }
        }
    })
}

/// Build the Voice Gateway v8 `HEARTBEAT` (Op 3) JSON payload.
///
/// Per Discord Voice Gateway v8 (`?v=8`), `seq_ack` MUST be `-1` before any sequenced
/// event has been received.
#[must_use]
pub fn build_heartbeat_payload(nonce_ms: u64, seq_ack: Option<u64>) -> Value {
    let ack = seq_ack.map_or(-1i64, |s| i64::try_from(s).unwrap_or(i64::MAX));
    json!({
        "op": VOICE_OP_HEARTBEAT,
        "d": {
            "t": nonce_ms,
            "seq_ack": ack,
        }
    })
}

/// Build the Voice Gateway v8 `SPEAKING` (Op 5) JSON payload.
#[must_use]
pub fn build_speaking_payload(flags: u32, delay: u32, ssrc: u32) -> Value {
    json!({
        "op": VOICE_OP_SPEAKING,
        "d": {
            "speaking": flags,
            "delay": delay,
            "ssrc": ssrc,
        }
    })
}

#[derive(Debug, Default)]
struct PendingSignaling {
    target_guild_id: Option<Snowflake>,
    target_channel_id: Option<Snowflake>,
    session_id: Option<Zeroizing<String>>,
    endpoint: Option<String>,
    token: Option<Zeroizing<String>>,
}

struct SharedSessionState {
    local_user_id: Snowflake,
    local_display_name: String,
    active_session_id: Option<String>,
    status: Option<VoiceChannelStatus>,
    call_rings: HashMap<Snowflake, CallRingState>,
    user_volumes: HashMap<Snowflake, u8>,
    master_volume: u8,
    input_mode: InputMode,
    ptt_active: bool,
    high_pass_enabled: bool,
    dave: Option<DaveController>,
    mixer: AudioMixer,
    pending_signaling: PendingSignaling,
}

impl SharedSessionState {
    fn new(config: &VoiceEngineConfig) -> Self {
        let mut mixer = AudioMixer::new(config.jitter_cushion_frames);
        mixer.set_local_user(config.local_user_id);
        Self {
            local_user_id: config.local_user_id,
            local_display_name: config.local_display_name.clone(),
            active_session_id: None,
            status: None,
            call_rings: HashMap::new(),
            user_volumes: HashMap::new(),
            master_volume: 100,
            input_mode: config.input_mode,
            ptt_active: false,
            high_pass_enabled: true,
            dave: None,
            mixer,
            pending_signaling: PendingSignaling::default(),
        }
    }

    fn ensure_participant(&mut self, user_id: Snowflake, display_name: Option<String>) {
        let volume = self.user_volumes.get(&user_id).copied().unwrap_or(100);
        let dave_verified = self
            .dave
            .as_ref()
            .is_some_and(|d| d.is_user_in_verified_group(user_id));

        let Some(status) = self.status.as_mut() else {
            return;
        };

        if let Some(existing) = status.participants.iter_mut().find(|p| p.user_id == user_id) {
            if let Some(name) = display_name
                && !name.is_empty()
            {
                existing.display_name = name;
            }
            existing.dave_verified = dave_verified;
            return;
        }

        let name = display_name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                if user_id == self.local_user_id {
                    self.local_display_name.clone()
                } else {
                    format!("user-{user_id}")
                }
            });

        status.participants.push(VoiceParticipant {
            user_id,
            display_name: name,
            speaking: false,
            self_mute: if user_id == self.local_user_id {
                status.self_mute
            } else {
                false
            },
            self_deaf: if user_id == self.local_user_id {
                status.self_deaf
            } else {
                false
            },
            server_mute: false,
            server_deaf: false,
            volume,
            rms_db: MIN_RMS_DB,
            dave_verified,
            streaming: false,
        });
    }

    fn sync_dave_into_status(&mut self) {
        let dave_state = self
            .dave
            .as_ref()
            .map_or(DaveSessionState::Disabled, DaveController::state);
        let verified_ids: Vec<Snowflake> = self
            .status
            .as_ref()
            .map(|st| {
                st.participants
                    .iter()
                    .filter(|p| {
                        self.dave
                            .as_ref()
                            .is_some_and(|d| d.is_user_in_verified_group(p.user_id))
                    })
                    .map(|p| p.user_id)
                    .collect()
            })
            .unwrap_or_default();

        if let Some(status) = self.status.as_mut() {
            status.dave = dave_state;
            for p in &mut status.participants {
                p.dave_verified = verified_ids.contains(&p.user_id);
            }
        }
    }
}

enum RuntimeControl {
    SetMute(bool),
    SetDeaf(bool),
    SetInputMode(InputMode),
    SetPttActive(bool),
    SetHighPass(bool),
    InjectMicFrame(Vec<f32>),
}

struct RunningSession {
    stop_tx: watch::Sender<bool>,
    ctrl_tx: mpsc::Sender<RuntimeControl>,
    task: JoinHandle<()>,
}

/// Full Discord Voice Engine supporting Guild Voice/Stage, 1:1 DM & Group DM calls,
/// mandatory DAVE MLS E2EE, real-time Opus mixing, and Ogg/Opus voice-message playback.
pub struct DiscordVoiceEngine {
    config: VoiceEngineConfig,
    state: Arc<Mutex<SharedSessionState>>,
    signaling: Option<Arc<dyn VoiceSignalingBridge>>,
    signaling_notify: Arc<Notify>,
    audio_io: Arc<AudioIoHandle>,
    playback_queue: Arc<PlaybackQueue>,
    player: VoiceMessagePlayer,
    events: broadcast::Sender<BackendEvent>,
    running: tokio::sync::Mutex<Option<RunningSession>>,
}

impl std::fmt::Debug for DiscordVoiceEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordVoiceEngine")
            .field("local_user_id", &self.config.local_user_id)
            .field("audio_mode", &self.config.audio_mode)
            .finish()
    }
}

impl DiscordVoiceEngine {
    /// Construct a new [`DiscordVoiceEngine`] with an optional main Gateway [`VoiceSignalingBridge`].
    pub fn new(
        config: VoiceEngineConfig,
        signaling: Option<Arc<dyn VoiceSignalingBridge>>,
    ) -> VoiceResult<Self> {
        let (events, _) = broadcast::channel(512);
        let audio_io = Arc::new(AudioIoHandle::spawn(config.audio_mode)?);
        let player = VoiceMessagePlayer::new(events.clone(), config.audio_mode)?;
        let state = Arc::new(Mutex::new(SharedSessionState::new(&config)));

        Ok(Self {
            config,
            state,
            signaling,
            signaling_notify: Arc::new(Notify::new()),
            audio_io,
            playback_queue: Arc::new(PlaybackQueue::new()),
            player,
            events,
            running: tokio::sync::Mutex::new(None),
        })
    }

    /// Borrow the inline Ogg/Opus [`VoiceMessagePlayer`] for pause/resume/seek and waveform buckets.
    #[must_use]
    pub const fn player(&self) -> &VoiceMessagePlayer {
        &self.player
    }

    /// Switch microphone gating mode between VAD and Push-to-Talk.
    pub async fn set_input_mode(&self, mode: InputMode) -> VoiceResult<()> {
        let status_to_emit = {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;
            guard.input_mode = mode;
            if let Some(status) = guard.status.as_mut() {
                status.push_to_talk = matches!(mode, InputMode::PushToTalk);
                Some(status.clone())
            } else {
                None
            }
        };

        if let Some(ctrl) = self.control_sender().await {
            let _ = ctrl.send(RuntimeControl::SetInputMode(mode)).await;
        }
        if let Some(st) = status_to_emit {
            let _ = self.events.send(BackendEvent::VoiceStatusUpdated(st));
        }
        Ok(())
    }

    /// Update Push-to-Talk pressed state (`true` = transmitting, `false` = released).
    pub async fn set_ptt_active(&self, active: bool) -> VoiceResult<()> {
        {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;
            guard.ptt_active = active;
        }
        if let Some(ctrl) = self.control_sender().await {
            let _ = ctrl.send(RuntimeControl::SetPttActive(active)).await;
        }
        Ok(())
    }

    /// Enable or disable the 80 Hz microphone DC/rumble high-pass filter.
    pub async fn set_high_pass_enabled(&self, enabled: bool) -> VoiceResult<()> {
        {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;
            guard.high_pass_enabled = enabled;
        }
        if let Some(ctrl) = self.control_sender().await {
            let _ = ctrl.send(RuntimeControl::SetHighPass(enabled)).await;
        }
        Ok(())
    }

    /// Set master output gain (`0..=200%`).
    pub fn set_master_volume(&self, volume: u8) -> VoiceResult<u8> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;
        let clamped = guard.mixer.set_master_volume(volume);
        guard.master_volume = clamped;
        Ok(clamped)
    }

    /// Compute the 45-digit pairwise DAVE SAS verification code for `peer_user_id`.
    pub fn peer_verification_code(&self, peer_user_id: Snowflake) -> VoiceResult<String> {
        let guard = self
            .state
            .lock()
            .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;
        let dave = guard
            .dave
            .as_ref()
            .ok_or_else(|| VoiceError::Dave("no active DAVE session".to_string()))?;
        dave.peer_verification_code(peer_user_id)
    }

    /// Inject a synthetic 20 ms stereo PCM frame (`1920` `f32` samples) into the active TX pipeline.
    ///
    /// Used by headless loopback tests when running without a physical microphone.
    pub async fn inject_mic_frame(&self, pcm_stereo_20ms: Vec<f32>) -> VoiceResult<()> {
        let ctrl = self
            .control_sender()
            .await
            .ok_or(VoiceError::NotConnected)?;
        ctrl.send(RuntimeControl::InjectMicFrame(pcm_stereo_20ms))
            .await
            .map_err(|_| VoiceError::NotConnected)
    }

    /// Feed an inbound main Gateway `VOICE_STATE_UPDATE` dispatch event into the engine.
    ///
    /// Supports both Guild voice (`guild_id: Some(...)`) and 1:1 / Group DM calls (`guild_id: None`),
    /// tracking remote moves, disconnect/other-client takeovers, and remote participant state.
    pub async fn handle_voice_state_update(&self, event: VoiceStateUpdateEvent) {
        let mut should_leave = false;
        let mut notify_signaling = false;
        let mut emit_status: Option<VoiceChannelStatus> = None;

        if let Ok(mut guard) = self.state.lock() {
            if event.user_id == guard.local_user_id {
                let matches_pending = guard.pending_signaling.target_channel_id == event.channel_id
                    && guard.pending_signaling.target_guild_id == event.guild_id;

                if matches_pending && event.channel_id.is_some() {
                    guard.pending_signaling.session_id =
                        Some(Zeroizing::new(event.session_id.clone()));
                    guard.active_session_id = Some(event.session_id.clone());
                    notify_signaling = true;
                } else {
                    let same_session = guard
                        .active_session_id
                        .as_deref()
                        .is_none_or(|sid| sid == event.session_id);
                    if let Some(status) = guard.status.as_mut() {
                        let same_scope = status.guild_id == event.guild_id
                            || (status.guild_id.is_none()
                                && event.guild_id.is_none()
                                && (event.channel_id == Some(status.channel_id)
                                    || event.channel_id.is_none()));

                        if same_scope {
                            match event.channel_id {
                                Some(new_ch) if same_session => {
                                    status.channel_id = new_ch;
                                    status.self_mute = event.self_mute;
                                    status.self_deaf = event.self_deaf;
                                    emit_status = Some(status.clone());
                                }
                                _ => {
                                    // Disconnected or moved to another Discord client session.
                                    should_leave = true;
                                }
                            }
                        }
                    }
                }
            } else if let Some(active_ch) = guard.status.as_ref().map(|s| s.channel_id) {
                if event.channel_id == Some(active_ch) {
                    guard.ensure_participant(event.user_id, event.display_name.clone());
                    if let Some(dave) = guard.dave.as_mut() {
                        dave.add_recognized_user(event.user_id);
                    }
                    if let Some(status) = guard.status.as_mut() {
                        if let Some(p) = status
                            .participants
                            .iter_mut()
                            .find(|p| p.user_id == event.user_id)
                        {
                            p.self_mute = event.self_mute;
                            p.self_deaf = event.self_deaf;
                            p.server_mute = event.mute;
                            p.server_deaf = event.deaf;
                            p.streaming = event.self_stream;
                        }
                        emit_status = Some(status.clone());
                    }
                } else if let Some(status) = guard.status.as_mut() {
                    let before = status.participants.len();
                    status.participants.retain(|p| p.user_id != event.user_id);
                    if status.participants.len() != before {
                        emit_status = Some(status.clone());
                        guard.mixer.remove_user(event.user_id);
                        if let Some(dave) = guard.dave.as_mut() {
                            dave.remove_recognized_user(event.user_id);
                        }
                    }
                }
            }
        }

        if notify_signaling {
            self.signaling_notify.notify_waiters();
        }
        if let Some(st) = emit_status {
            let _ = self.events.send(BackendEvent::VoiceStatusUpdated(st));
        }
        if should_leave {
            let _ = self.stop_running_session().await;
        }
    }

    /// Feed an inbound main Gateway `VOICE_SERVER_UPDATE` event into the engine.
    ///
    /// Works identically for Guild voice (`guild_id: Some(...)`) and 1:1 / Group DM calls
    /// (`guild_id: None`, matched via `channel_id`).
    pub async fn handle_voice_server_update(&self, event: VoiceServerUpdateEvent) {
        let mut notify_signaling = false;
        if let Ok(mut guard) = self.state.lock() {
            let matches_guild = event.guild_id.is_some()
                && guard.pending_signaling.target_guild_id == event.guild_id;
            let matches_dm_call = event.guild_id.is_none()
                && guard.pending_signaling.target_guild_id.is_none()
                && event.channel_id.is_some()
                && guard.pending_signaling.target_channel_id == event.channel_id;

            if matches_guild || matches_dm_call {
                guard.pending_signaling.endpoint = event.endpoint;
                guard.pending_signaling.token = Some(event.token);
                notify_signaling = true;
            }
        }
        if notify_signaling {
            self.signaling_notify.notify_waiters();
        }
    }

    /// Feed an inbound main Gateway `CALL_CREATE` or `CALL_UPDATE` event for 1:1 / Group DM calls.
    pub fn handle_call_update(&self, event: CallUpdateEvent) {
        let ring_state = if !event.connected_user_ids.is_empty() && event.ringing.is_empty() {
            CallRingState::Active {
                channel_id: event.channel_id,
                connected_user_ids: event.connected_user_ids.clone(),
            }
        } else if event.ringing.contains(&self.config.local_user_id) {
            CallRingState::IncomingRinging {
                channel_id: event.channel_id,
                caller_id: event.caller_id.unwrap_or(Snowflake(0)),
                caller_name: event
                    .caller_name
                    .unwrap_or_else(|| "Incoming Call".to_string()),
                is_group_dm: event.is_group_dm,
            }
        } else if !event.ringing.is_empty() {
            CallRingState::OutgoingRinging {
                channel_id: event.channel_id,
                target_user_ids: event.ringing.clone(),
            }
        } else if !event.connected_user_ids.is_empty() {
            CallRingState::Active {
                channel_id: event.channel_id,
                connected_user_ids: event.connected_user_ids.clone(),
            }
        } else {
            CallRingState::Idle
        };

        let mut emit_status = None;
        if let Ok(mut guard) = self.state.lock() {
            guard
                .call_rings
                .insert(event.channel_id, ring_state.clone());
            if let Some(status) = guard.status.as_mut()
                && status.channel_id == event.channel_id
            {
                status.call_ring = ring_state.clone();
                emit_status = Some(status.clone());
            }
        }

        let _ = self.events.send(BackendEvent::CallRingUpdated(ring_state));
        if let Some(st) = emit_status {
            let _ = self.events.send(BackendEvent::VoiceStatusUpdated(st));
        }
    }

    /// Feed an inbound main Gateway `CALL_DELETE` event when a DM/Group DM call ends.
    pub fn handle_call_delete(&self, channel_id: Snowflake) {
        let mut emit_status = None;
        if let Ok(mut guard) = self.state.lock() {
            guard.call_rings.insert(channel_id, CallRingState::Idle);
            if let Some(status) = guard.status.as_mut()
                && status.channel_id == channel_id
            {
                status.call_ring = CallRingState::Idle;
                emit_status = Some(status.clone());
            }
        }
        let _ = self
            .events
            .send(BackendEvent::CallRingUpdated(CallRingState::Idle));
        if let Some(st) = emit_status {
            let _ = self.events.send(BackendEvent::VoiceStatusUpdated(st));
        }
    }

    /// Connect directly to a Voice Gateway v8 endpoint using resolved [`VoiceGatewayCredentials`].
    ///
    /// Supports both Guild voice channels (`guild_id: Some(...)`) and 1:1 / Group DM calls
    /// (`guild_id: None`).
    pub async fn connect_voice_gateway(
        &self,
        creds: VoiceGatewayCredentials,
    ) -> VoiceResult<VoiceChannelStatus> {
        self.stop_running_session().await?;

        let channel_name = creds.channel_name.clone().unwrap_or_else(|| {
            if creds.guild_id.is_none() {
                format!("DM Call {}", creds.channel_id)
            } else {
                format!("Voice {}", creds.channel_id)
            }
        });

        let initial_status = {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;
            guard.mixer.clear_speakers();
            guard.mixer.set_local_user(creds.user_id);
            guard.active_session_id = Some(creds.session_id.as_str().to_string());
            guard.dave = Some(DaveController::new(creds.user_id, creds.channel_id));

            let call_ring = if creds.guild_id.is_none() {
                guard
                    .call_rings
                    .get(&creds.channel_id)
                    .cloned()
                    .unwrap_or_else(|| CallRingState::Active {
                        channel_id: creds.channel_id,
                        connected_user_ids: vec![creds.user_id],
                    })
            } else {
                CallRingState::Idle
            };

            let status = VoiceChannelStatus {
                guild_id: creds.guild_id,
                channel_id: creds.channel_id,
                channel_name,
                connection_state: VoiceConnectionState::Connecting,
                self_mute: false,
                self_deaf: false,
                push_to_talk: matches!(guard.input_mode, InputMode::PushToTalk),
                ping_ms: None,
                packet_loss_pct: 0.0,
                dave: DaveSessionState::Handshaking,
                call_ring,
                participants: Vec::new(),
            };
            guard.status = Some(status);
            let local_name = guard.local_display_name.clone();
            guard.ensure_participant(creds.user_id, Some(local_name));
            guard
                .status
                .clone()
                .ok_or_else(|| VoiceError::Internal("missing voice status".to_string()))?
        };

        let _ = self
            .events
            .send(BackendEvent::VoiceStatusUpdated(initial_status.clone()));

        let (stop_tx, stop_rx) = watch::channel(false);
        let (ctrl_tx, ctrl_rx) = mpsc::channel(64);
        let state = Arc::clone(&self.state);
        let audio_io = Arc::clone(&self.audio_io);
        let playback_queue = Arc::clone(&self.playback_queue);
        let events = self.events.clone();

        let task = tokio::spawn(async move {
            let result = run_voice_gateway_session(
                creds,
                Arc::clone(&state),
                Arc::clone(&audio_io),
                Arc::clone(&playback_queue),
                events.clone(),
                stop_rx,
                ctrl_rx,
            )
            .await;

            // Guaranteed cleanup on both clean exit and error/zombie termination.
            let _ = audio_io.stop_capture().await;
            let _ = audio_io.stop_playback().await;
            playback_queue.clear();

            if let Ok(mut guard) = state.lock()
                && let Some(st) = guard.status.as_mut()
            {
                st.connection_state = VoiceConnectionState::Disconnected;
                let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
            }

            if let Err(err) = result {
                warn!("voice gateway session ended with error: {err}");
                let _ = events.send(BackendEvent::ErrorBanner {
                    scope: "voice".to_string(),
                    message: err.to_string(),
                    recoverable: true,
                });
            }
        });

        let mut running = self.running.lock().await;
        *running = Some(RunningSession {
            stop_tx,
            ctrl_tx,
            task,
        });

        Ok(initial_status)
    }

    async fn control_sender(&self) -> Option<mpsc::Sender<RuntimeControl>> {
        let running = self.running.lock().await;
        running.as_ref().map(|r| r.ctrl_tx.clone())
    }

    async fn stop_running_session(&self) -> VoiceResult<()> {
        let prev = {
            let mut running = self.running.lock().await;
            running.take()
        };
        if let Some(session) = prev {
            let _ = session.stop_tx.send(true);
            let _ = session.task.await;
        }
        let _ = self.audio_io.stop_capture().await;
        let _ = self.audio_io.stop_playback().await;
        self.playback_queue.clear();

        if let Ok(mut guard) = self.state.lock() {
            guard.status = None;
            guard.dave = None;
            guard.active_session_id = None;
            guard.mixer.clear_speakers();
            guard.pending_signaling = PendingSignaling::default();
        }
        Ok(())
    }

    async fn wait_for_signaling_credentials(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    ) -> VoiceResult<VoiceGatewayCredentials> {
        let deadline = tokio::time::Instant::now() + self.config.signaling_timeout;
        loop {
            // Register and enable the waiter BEFORE checking shared state to prevent lost wakeups.
            let notified = self.signaling_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let maybe_ready = {
                let guard = self
                    .state
                    .lock()
                    .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;
                if guard.pending_signaling.target_guild_id == guild_id
                    && guard.pending_signaling.target_channel_id == Some(channel_id)
                {
                    match (
                        &guard.pending_signaling.session_id,
                        &guard.pending_signaling.endpoint,
                        &guard.pending_signaling.token,
                    ) {
                        (Some(sid), Some(endpoint), Some(token)) => Some(VoiceGatewayCredentials {
                            guild_id,
                            channel_id,
                            channel_name: None,
                            user_id: guard.local_user_id,
                            session_id: sid.clone(),
                            token: token.clone(),
                            endpoint: endpoint.clone(),
                        }),
                        _ => None,
                    }
                } else {
                    None
                }
            };

            if let Some(creds) = maybe_ready {
                return Ok(creds);
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(VoiceError::Signaling(format!(
                    "timed out waiting for VOICE_STATE_UPDATE and VOICE_SERVER_UPDATE for channel {channel_id}"
                )));
            }
        }
    }
}

#[async_trait]
impl VoiceEngine for DiscordVoiceEngine {
    async fn join_voice(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    ) -> CoreResult<VoiceChannelStatus> {
        self.stop_running_session().await.map_err(CoreError::from)?;

        {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| CoreError::Internal("voice state lock poisoned".to_string()))?;
            guard.pending_signaling = PendingSignaling {
                target_guild_id: guild_id,
                target_channel_id: Some(channel_id),
                session_id: None,
                endpoint: None,
                token: None,
            };
        }

        let Some(bridge) = self.signaling.as_ref() else {
            return Err(CoreError::Voice(
                "no main gateway signaling bridge configured; use connect_voice_gateway for direct connections".to_string(),
            ));
        };

        bridge
            .send_voice_state_update(VoiceStateUpdateCommand {
                guild_id,
                channel_id: Some(channel_id),
                self_mute: false,
                self_deaf: false,
            })
            .await
            .map_err(CoreError::from)?;

        let creds = self
            .wait_for_signaling_credentials(guild_id, channel_id)
            .await
            .map_err(CoreError::from)?;

        self.connect_voice_gateway(creds)
            .await
            .map_err(CoreError::from)
    }

    async fn leave_voice(&self) -> CoreResult<()> {
        let prev_guild = self
            .state
            .lock()
            .ok()
            .and_then(|g| g.status.as_ref().map(|s| s.guild_id));

        self.stop_running_session().await.map_err(CoreError::from)?;

        if let (Some(bridge), Some(guild_id)) = (self.signaling.as_ref(), prev_guild) {
            bridge
                .send_voice_state_update(VoiceStateUpdateCommand {
                    guild_id,
                    channel_id: None,
                    self_mute: false,
                    self_deaf: false,
                })
                .await
                .map_err(CoreError::from)?;
        }
        Ok(())
    }

    async fn set_self_mute(&self, muted: bool) -> CoreResult<VoiceChannelStatus> {
        let (status, guild_id, channel_id, self_deaf) = {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| CoreError::Internal("voice state lock poisoned".to_string()))?;
            let local_uid = guard.local_user_id;
            let status = guard
                .status
                .as_mut()
                .ok_or_else(|| CoreError::from(VoiceError::NotConnected))?;
            status.self_mute = muted;
            if !muted {
                status.self_deaf = false;
            }
            let deaf = status.self_deaf;
            if let Some(me) = status.participants.iter_mut().find(|p| p.user_id == local_uid) {
                me.self_mute = muted;
                me.self_deaf = deaf;
                if muted {
                    me.speaking = false;
                    me.rms_db = MIN_RMS_DB;
                }
            }
            let cloned = status.clone();
            let gid = status.guild_id;
            let cid = status.channel_id;
            guard.mixer.set_deafened(deaf);
            (cloned, gid, cid, deaf)
        };

        if let Some(ctrl) = self.control_sender().await {
            let _ = ctrl.send(RuntimeControl::SetMute(muted)).await;
            if !muted {
                let _ = ctrl.send(RuntimeControl::SetDeaf(false)).await;
            }
        }
        if let Some(bridge) = self.signaling.as_ref() {
            let _ = bridge
                .send_voice_state_update(VoiceStateUpdateCommand {
                    guild_id,
                    channel_id: Some(channel_id),
                    self_mute: muted,
                    self_deaf,
                })
                .await;
        }
        let _ = self
            .events
            .send(BackendEvent::VoiceStatusUpdated(status.clone()));
        Ok(status)
    }

    async fn set_self_deaf(&self, deafened: bool) -> CoreResult<VoiceChannelStatus> {
        let (status, guild_id, channel_id, self_mute) = {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| CoreError::Internal("voice state lock poisoned".to_string()))?;
            let local_uid = guard.local_user_id;
            let status = guard
                .status
                .as_mut()
                .ok_or_else(|| CoreError::from(VoiceError::NotConnected))?;
            status.self_deaf = deafened;
            if deafened {
                status.self_mute = true;
            }
            let mute = status.self_mute;
            if let Some(me) = status.participants.iter_mut().find(|p| p.user_id == local_uid) {
                me.self_deaf = deafened;
                me.self_mute = mute;
                if deafened {
                    me.speaking = false;
                    me.rms_db = MIN_RMS_DB;
                }
            }
            let cloned = status.clone();
            let gid = status.guild_id;
            let cid = status.channel_id;
            guard.mixer.set_deafened(deafened);
            (cloned, gid, cid, mute)
        };

        if let Some(ctrl) = self.control_sender().await {
            let _ = ctrl.send(RuntimeControl::SetDeaf(deafened)).await;
            if deafened {
                let _ = ctrl.send(RuntimeControl::SetMute(true)).await;
            }
        }
        if let Some(bridge) = self.signaling.as_ref() {
            let _ = bridge
                .send_voice_state_update(VoiceStateUpdateCommand {
                    guild_id,
                    channel_id: Some(channel_id),
                    self_mute,
                    self_deaf: deafened,
                })
                .await;
        }
        let _ = self
            .events
            .send(BackendEvent::VoiceStatusUpdated(status.clone()));
        Ok(status)
    }

    async fn set_participant_volume(
        &self,
        user_id: Snowflake,
        volume: u8,
    ) -> CoreResult<VoiceParticipant> {
        let clamped = volume.min(MAX_VOLUME_PERCENT);
        let (participant, status) = {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| CoreError::Internal("voice state lock poisoned".to_string()))?;
            guard.user_volumes.insert(user_id, clamped);
            guard.mixer.set_user_volume(user_id, clamped);

            let status = guard
                .status
                .as_mut()
                .ok_or_else(|| CoreError::from(VoiceError::NotConnected))?;
            let p = status
                .participants
                .iter_mut()
                .find(|p| p.user_id == user_id)
                .ok_or_else(|| CoreError::NotFound(format!("voice participant {user_id}")))?;
            p.set_volume(clamped);
            (p.clone(), status.clone())
        };

        let _ = self.events.send(BackendEvent::VoiceStatusUpdated(status));
        Ok(participant)
    }

    async fn status(&self) -> CoreResult<Option<VoiceChannelStatus>> {
        let guard = self
            .state
            .lock()
            .map_err(|_| CoreError::Internal("voice state lock poisoned".to_string()))?;
        Ok(guard.status.clone())
    }

    async fn dave_state(&self) -> CoreResult<DaveSessionState> {
        let guard = self
            .state
            .lock()
            .map_err(|_| CoreError::Internal("voice state lock poisoned".to_string()))?;
        Ok(guard
            .dave
            .as_ref()
            .map_or(DaveSessionState::Disabled, DaveController::state))
    }

    async fn play_voice_message(
        &self,
        message_id: Snowflake,
        attachment: &Attachment,
    ) -> CoreResult<VoicePlaybackState> {
        self.player
            .play_attachment(message_id, attachment)
            .await
            .map_err(CoreError::from)
    }

    async fn stop_voice_message(&self) -> CoreResult<()> {
        self.player.stop().await.map_err(CoreError::from)
    }

    fn subscribe_voice_events(&self) -> broadcast::Receiver<BackendEvent> {
        self.events.subscribe()
    }
}

impl Drop for DiscordVoiceEngine {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.running.try_lock()
            && let Some(session) = guard.take()
        {
            let _ = session.stop_tx.send(true);
            session.task.abort();
        }
        self.playback_queue.clear();
    }
}

async fn perform_udp_ip_discovery(
    server_addr: SocketAddr,
    ssrc: u32,
) -> VoiceResult<(UdpSocket, SocketAddr)> {
    let bind_addr = match server_addr.ip() {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let socket = UdpSocket::bind(bind_addr).await?;
    socket.connect(server_addr).await?;

    let req = build_ip_discovery_request(ssrc);
    socket.send(&req).await?;

    let mut resp = [0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut resp))
        .await
        .map_err(|_| VoiceError::Discovery("UDP IP discovery timed out".to_string()))??;
    let external = parse_ip_discovery_response(&resp[..n], Some(ssrc))?;
    Ok((socket, external))
}

async fn send_dave_outbound<S>(ws_tx: &mut S, messages: Vec<DaveOutboundMessage>) -> VoiceResult<()>
where
    S: futures::Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    for msg in messages {
        let ws_msg = match msg.to_wire_frame() {
            DaveWireFrame::Text(txt) => WsMessage::Text(txt.into()),
            DaveWireFrame::Binary(bin) => WsMessage::Binary(bin.into()),
        };
        ws_tx
            .send(ws_msg)
            .await
            .map_err(|e| VoiceError::Gateway(format!("failed sending DAVE gateway frame: {e}")))?;
    }
    Ok(())
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

async fn run_voice_gateway_session(
    creds: VoiceGatewayCredentials,
    state: Arc<Mutex<SharedSessionState>>,
    audio_io: Arc<AudioIoHandle>,
    playback_queue: Arc<PlaybackQueue>,
    events: broadcast::Sender<BackendEvent>,
    mut stop_rx: watch::Receiver<bool>,
    mut ctrl_rx: mpsc::Receiver<RuntimeControl>,
) -> VoiceResult<()> {
    let ws_url = creds.gateway_ws_url();
    info!(
        "connecting voice gateway v8 (guild={:?}, channel={})",
        creds.guild_id, creds.channel_id
    );

    let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| VoiceError::Gateway(format!("WebSocket connect failed: {e}")))?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let capture_queue = Arc::new(CaptureQueue::new());
    let mut mic_frame_buf = [0.0f32; FRAME_SAMPLES];
    let mut mic_processor = {
        let mode = state
            .lock()
            .map_or_else(|_| InputMode::default(), |g| g.input_mode);
        MicCaptureProcessor::new(mode)?
    };
    let mut self_muted = false;
    let mut last_speaking_sent = false;
    let mut last_seq_ack: Option<u64> = None;
    let mut pending_heartbeat: Option<(u64, Instant)> = None;
    let mut heartbeat_interval: Option<tokio::time::Interval> = None;

    let mut udp_socket: Option<UdpSocket> = None;
    let mut cipher: Option<RtpTransportCipher> = None;
    let mut packetizer: Option<RtpPacketizer> = None;
    let mut udp_rx_buf = vec![0u8; 4096];
    let mut mix_buf = [0.0f32; FRAME_SAMPLES];
    let mut mix_ticker = tokio::time::interval(Duration::from_millis(20));
    let mut vu_tick_counter = 0u32;

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    let _ = ws_tx.close().await;
                    break;
                }
            }

            _ = async {
                if let Some(hb) = heartbeat_interval.as_mut() {
                    hb.tick().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                if pending_heartbeat.is_some() {
                    warn!("voice gateway missed HEARTBEAT_ACK; closing zombie connection");
                    let _ = ws_tx.close().await;
                    return Err(VoiceError::Gateway(
                        "voice gateway heartbeat ACK timeout (zombie connection)".to_string(),
                    ));
                }
                let nonce = current_unix_ms();
                pending_heartbeat = Some((nonce, Instant::now()));
                let hb = build_heartbeat_payload(nonce, last_seq_ack);
                ws_tx
                    .send(WsMessage::Text(hb.to_string().into()))
                    .await
                    .map_err(|e| VoiceError::Gateway(format!("heartbeat send failed: {e}")))?;
            }

            ws_item = ws_rx.next() => {
                let Some(frame_res) = ws_item else {
                    debug!("voice gateway stream closed by peer");
                    break;
                };
                let frame = frame_res
                    .map_err(|e| VoiceError::Gateway(format!("WebSocket read failed: {e}")))?;

                match frame {
                    WsMessage::Text(text) => {
                        let parsed: Value = serde_json::from_str(text.as_str())?;
                        if let Some(seq) = parsed.get("seq").and_then(Value::as_u64) {
                            last_seq_ack = Some(seq);
                        }
                        let Some(op_u64) = parsed.get("op").and_then(Value::as_u64) else {
                            continue;
                        };
                        let Ok(op) = u8::try_from(op_u64) else {
                            continue;
                        };
                        let data = parsed.get("d").unwrap_or(&Value::Null);

                        match op {
                            VOICE_OP_HELLO => {
                                let interval_ms = data
                                    .get("heartbeat_interval")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(13_750)
                                    .max(100);
                                let period = Duration::from_millis(interval_ms);
                                let mut iv = tokio::time::interval_at(
                                    tokio::time::Instant::now() + period,
                                    period,
                                );
                                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                                heartbeat_interval = Some(iv);

                                // Send IDENTIFY (never log raw payload containing token).
                                debug!("received voice HELLO (heartbeat_interval={}ms); sending IDENTIFY", interval_ms);
                                let identify = build_identify_payload(&creds);
                                ws_tx
                                    .send(WsMessage::Text(identify.to_string().into()))
                                    .await
                                    .map_err(|e| VoiceError::Gateway(format!("IDENTIFY send failed: {e}")))?;

                                if let Ok(mut guard) = state.lock()
                                    && let Some(st) = guard.status.as_mut()
                                {
                                    st.connection_state = VoiceConnectionState::Signaling;
                                    let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
                                }
                            }

                            VOICE_OP_READY => {
                                let ssrc = data
                                    .get("ssrc")
                                    .and_then(Value::as_u64)
                                    .and_then(|v| u32::try_from(v).ok())
                                    .ok_or_else(|| VoiceError::Gateway("READY missing u32 `ssrc`".to_string()))?;
                                let ip_str = data
                                    .get("ip")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| VoiceError::Gateway("READY missing `ip`".to_string()))?;
                                let port = data
                                    .get("port")
                                    .and_then(Value::as_u64)
                                    .and_then(|v| u16::try_from(v).ok())
                                    .ok_or_else(|| VoiceError::Gateway("READY missing u16 `port`".to_string()))?;
                                let server_ip: IpAddr = ip_str
                                    .parse()
                                    .map_err(|_| VoiceError::Gateway(format!("invalid READY ip {ip_str:?}")))?;
                                let modes: Vec<String> = data
                                    .get("modes")
                                    .and_then(Value::as_array)
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(str::to_owned))
                                            .collect()
                                    })
                                    .unwrap_or_default();

                                let mode = TransportEncryptionMode::negotiate(&modes).ok_or_else(|| {
                                    VoiceError::TransportCrypto(
                                        "server offered no supported AEAD *_rtpsize encryption mode".to_string(),
                                    )
                                })?;

                                let (sock, external_addr) =
                                    perform_udp_ip_discovery(SocketAddr::new(server_ip, port), ssrc).await?;
                                udp_socket = Some(sock);
                                packetizer = Some(RtpPacketizer::new(ssrc));

                                let select_proto = build_select_protocol_payload(external_addr, mode);
                                ws_tx
                                    .send(WsMessage::Text(select_proto.to_string().into()))
                                    .await
                                    .map_err(|e| VoiceError::Gateway(format!("SELECT_PROTOCOL send failed: {e}")))?;
                            }

                            VOICE_OP_SESSION_DESCRIPTION => {
                                let mode_str = data
                                    .get("mode")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| VoiceError::Gateway("SESSION_DESCRIPTION missing `mode`".to_string()))?;
                                let mode = TransportEncryptionMode::from_discord_str(mode_str).ok_or_else(|| {
                                    VoiceError::TransportCrypto(format!("unsupported mode {mode_str:?}"))
                                })?;
                                let secret_key: Vec<u8> = data
                                    .get("secret_key")
                                    .and_then(Value::as_array)
                                    .ok_or_else(|| VoiceError::Gateway("SESSION_DESCRIPTION missing `secret_key`".to_string()))?
                                    .iter()
                                    .map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
                                    .collect::<Option<Vec<u8>>>()
                                    .ok_or_else(|| VoiceError::TransportCrypto("malformed `secret_key` byte array".to_string()))?;

                                let dave_version = data
                                    .get("dave_protocol_version")
                                    .and_then(Value::as_u64)
                                    .and_then(|v| u16::try_from(v).ok())
                                    .unwrap_or(0);

                                cipher = Some(RtpTransportCipher::new(mode, &secret_key)?);

                                let dave_outbound = {
                                    let mut guard = state
                                        .lock()
                                        .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;
                                    let out = if let Some(dave) = guard.dave.as_mut() {
                                        dave.on_session_description(dave_version)?
                                    } else {
                                        Vec::new()
                                    };
                                    guard.sync_dave_into_status();
                                    if let Some(st) = guard.status.as_mut() {
                                        st.connection_state = VoiceConnectionState::Connected;
                                        let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
                                    }
                                    out
                                };
                                send_dave_outbound(&mut ws_tx, dave_outbound).await?;

                                // Announce speaking capability so Discord SFU starts forwarding media.
                                if let Some(pkt) = packetizer.as_ref() {
                                    let spk = build_speaking_payload(SPEAKING_FLAG_MICROPHONE, 0, pkt.ssrc());
                                    ws_tx
                                        .send(WsMessage::Text(spk.to_string().into()))
                                        .await
                                        .map_err(|e| VoiceError::Gateway(format!("SPEAKING send failed: {e}")))?;
                                    last_speaking_sent = true;
                                }

                                let _ = audio_io.start_playback(Arc::clone(&playback_queue)).await;
                                if !self_muted {
                                    let _ = audio_io.start_capture(Arc::clone(&capture_queue)).await;
                                }
                            }

                            VOICE_OP_HEARTBEAT_ACK => {
                                let ack_nonce = data
                                    .get("t")
                                    .and_then(Value::as_u64)
                                    .or_else(|| data.as_u64());
                                if let (Some(ack), Some((sent_nonce, sent_at))) = (ack_nonce, pending_heartbeat)
                                    && ack == sent_nonce
                                {
                                    let rtt_ms = sent_at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                                    pending_heartbeat = None;
                                    if let Ok(mut guard) = state.lock()
                                        && let Some(st) = guard.status.as_mut()
                                    {
                                        st.ping_ms = Some(rtt_ms);
                                        let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
                                    }
                                }
                            }

                            VOICE_OP_SPEAKING => {
                                if let (Some(uid_val), Some(ssrc_u64)) = (
                                    data.get("user_id"),
                                    data.get("ssrc").and_then(Value::as_u64),
                                ) && let Ok(user_id) = serde_json::from_value::<Snowflake>(uid_val.clone())
                                    && let Ok(ssrc) = u32::try_from(ssrc_u64)
                                {
                                    let speaking_flags = data
                                        .get("speaking")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0);
                                    let is_speaking = speaking_flags != 0;

                                    if let Ok(mut guard) = state.lock() {
                                        guard.mixer.bind_ssrc(ssrc, user_id);
                                        if let Some(dave) = guard.dave.as_mut() {
                                            dave.add_recognized_user(user_id);
                                        }
                                        guard.ensure_participant(user_id, None);
                                        if let Some(st) = guard.status.as_mut() {
                                            if let Some(p) = st.participants.iter_mut().find(|p| p.user_id == user_id) {
                                                p.speaking = is_speaking;
                                            }
                                            let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
                                        }
                                    }
                                }
                            }

                            VOICE_OP_CLIENTS_CONNECT => {
                                if let Some(arr) = data.get("user_ids").and_then(Value::as_array)
                                    && let Ok(mut guard) = state.lock()
                                {
                                    for item in arr {
                                        if let Ok(uid) = serde_json::from_value::<Snowflake>(item.clone()) {
                                            if let Some(dave) = guard.dave.as_mut() {
                                                dave.add_recognized_user(uid);
                                            }
                                            guard.ensure_participant(uid, None);
                                        }
                                    }
                                    guard.sync_dave_into_status();
                                    if let Some(st) = guard.status.as_ref() {
                                        let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
                                    }
                                }
                            }

                            VOICE_OP_CLIENT_DISCONNECT => {
                                if let Some(uid_val) = data.get("user_id")
                                    && let Ok(uid) = serde_json::from_value::<Snowflake>(uid_val.clone())
                                    && let Ok(mut guard) = state.lock()
                                {
                                    guard.mixer.remove_user(uid);
                                    if let Some(dave) = guard.dave.as_mut() {
                                        dave.remove_recognized_user(uid);
                                    }
                                    if let Some(st) = guard.status.as_mut() {
                                        st.participants.retain(|p| p.user_id != uid);
                                        let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
                                    }
                                }
                            }

                            VOICE_OP_RESUMED => {
                                if let Ok(mut guard) = state.lock()
                                    && let Some(st) = guard.status.as_mut()
                                {
                                    st.connection_state = VoiceConnectionState::Connected;
                                    let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
                                }
                            }

                            other => {
                                // Check DAVE JSON opcodes (21, 22, 24) without dropping the voice
                                // call on transient DAVE parsing or state errors.
                                match parse_json_dave_frame(other, data) {
                                    Ok(Some(dave_msg)) => {
                                        let outbound = {
                                            let mut guard = state.lock().map_err(|_| {
                                                VoiceError::Internal("voice state lock poisoned".to_string())
                                            })?;
                                            let out = match guard.dave.as_mut() {
                                                Some(dave) => match dave.handle_gateway_message(dave_msg) {
                                                    Ok(msgs) => msgs,
                                                    Err(err) => {
                                                        warn!("transient DAVE JSON opcode error (staying connected): {err}");
                                                        Vec::new()
                                                    }
                                                },
                                                None => Vec::new(),
                                            };
                                            guard.sync_dave_into_status();
                                            if let Some(st) = guard.status.as_ref() {
                                                let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
                                            }
                                            out
                                        };
                                        send_dave_outbound(&mut ws_tx, outbound).await?;
                                    }
                                    Ok(None) => {}
                                    Err(err) => {
                                        warn!("ignoring malformed JSON DAVE opcode {other}: {err}");
                                    }
                                }
                            }
                        }
                    }

                    WsMessage::Binary(bin) => {
                        match parse_binary_dave_frame(&bin) {
                            Ok(dave_msg) => {
                                match &dave_msg {
                                    crate::dave::DaveInboundMessage::MlsExternalSender { seq, .. }
                                    | crate::dave::DaveInboundMessage::MlsProposals { seq, .. }
                                    | crate::dave::DaveInboundMessage::MlsAnnounceCommitTransition { seq, .. }
                                    | crate::dave::DaveInboundMessage::MlsWelcome { seq, .. } => {
                                        last_seq_ack = Some(u64::from(*seq));
                                    }
                                    _ => {}
                                }
                                let outbound = {
                                    let mut guard = state.lock().map_err(|_| {
                                        VoiceError::Internal("voice state lock poisoned".to_string())
                                    })?;
                                    let out = match guard.dave.as_mut() {
                                        Some(dave) => match dave.handle_gateway_message(dave_msg) {
                                            Ok(msgs) => msgs,
                                            Err(err) => {
                                                // Transient MLS errors (e.g. Op 27 proposal racing ahead
                                                // of CLIENTS_CONNECT) degrade DAVE status rather than
                                                // tearing down the entire voice call.
                                                warn!("transient DAVE binary opcode error (staying connected): {err}");
                                                Vec::new()
                                            }
                                        },
                                        None => Vec::new(),
                                    };
                                    guard.sync_dave_into_status();
                                    if let Some(st) = guard.status.as_ref() {
                                        let _ = events.send(BackendEvent::VoiceStatusUpdated(st.clone()));
                                    }
                                    out
                                };
                                send_dave_outbound(&mut ws_tx, outbound).await?;
                            }
                            Err(err) => {
                                warn!("ignoring malformed binary voice gateway frame: {err}");
                            }
                        }
                    }

                    WsMessage::Ping(payload) => {
                        let _ = ws_tx.send(WsMessage::Pong(payload)).await;
                    }
                    WsMessage::Close(_) => {
                        debug!("received voice gateway Close frame");
                        break;
                    }
                    WsMessage::Pong(_) | WsMessage::Frame(_) => {}
                }
            }

            Some(ctrl) = ctrl_rx.recv() => {
                match ctrl {
                    RuntimeControl::SetMute(muted) => {
                        self_muted = muted;
                        if muted {
                            let _ = audio_io.stop_capture().await;
                            let silence_pkts = mic_processor.flush_on_mute();
                            if let (Some(sock), Some(c), Some(pkt)) = (
                                udp_socket.as_ref(),
                                cipher.as_mut(),
                                packetizer.as_mut(),
                            ) {
                                for silence in silence_pkts {
                                    let hdr = pkt.next_opus_header();
                                    if let Ok(wire) = c.encrypt_packet(&hdr, &silence) {
                                        let _ = sock.send(&wire).await;
                                    }
                                }
                            }
                            if let Ok(mut guard) = state.lock() {
                                guard.mixer.update_local_mic_level(MIN_RMS_DB, false);
                            }
                            if last_speaking_sent
                                && let Some(pkt) = packetizer.as_ref()
                            {
                                let spk = build_speaking_payload(0, 0, pkt.ssrc());
                                let _ = ws_tx.send(WsMessage::Text(spk.to_string().into())).await;
                                last_speaking_sent = false;
                            }
                        } else {
                            let _ = audio_io.start_capture(Arc::clone(&capture_queue)).await;
                        }
                    }
                    RuntimeControl::SetDeaf(deaf) => {
                        if deaf {
                            let _ = audio_io.stop_playback().await;
                            playback_queue.clear();
                        } else {
                            let _ = audio_io.start_playback(Arc::clone(&playback_queue)).await;
                        }
                    }
                    RuntimeControl::SetInputMode(mode) => {
                        mic_processor.set_input_mode(mode);
                    }
                    RuntimeControl::SetPttActive(active) => {
                        mic_processor.set_ptt_active(active);
                    }
                    RuntimeControl::SetHighPass(enabled) => {
                        mic_processor.set_high_pass_enabled(enabled);
                    }
                    RuntimeControl::InjectMicFrame(frame) => {
                        if frame.len() == FRAME_SAMPLES {
                            capture_queue.push_samples(&frame);
                        }
                    }
                }
            }

            () = capture_queue.recv_frame(&mut mic_frame_buf) => {
                if self_muted {
                    continue;
                }
                let Ok(mic_res) = mic_processor.process_frame(&mic_frame_buf) else {
                    continue;
                };

                if let Ok(mut guard) = state.lock() {
                    guard.mixer.update_local_mic_level(mic_res.smoothed_rms_db, mic_res.speaking);
                }

                if mic_res.speaking != last_speaking_sent
                    && let Some(pkt) = packetizer.as_ref()
                {
                    let flags = if mic_res.speaking {
                        SPEAKING_FLAG_MICROPHONE
                    } else {
                        0
                    };
                    let spk = build_speaking_payload(flags, 0, pkt.ssrc());
                    let _ = ws_tx.send(WsMessage::Text(spk.to_string().into())).await;
                    last_speaking_sent = mic_res.speaking;
                }

                let opus_payload = match mic_res.packet {
                    EncodedMicPacket::Audio(bytes) | EncodedMicPacket::Silence(bytes) => bytes,
                    EncodedMicPacket::Suppressed => continue,
                };

                let dave_protected = {
                    let Ok(mut guard) = state.lock() else {
                        continue;
                    };
                    match guard.dave.as_mut() {
                        Some(dave) => match dave.encrypt_opus(&opus_payload) {
                            Ok(cow) => cow.into_owned(),
                            Err(_) => continue, // Handshaking: refuse unencrypted send!
                        },
                        None => opus_payload,
                    }
                };

                if let (Some(sock), Some(c), Some(pkt)) = (
                    udp_socket.as_ref(),
                    cipher.as_mut(),
                    packetizer.as_mut(),
                ) {
                    let hdr = pkt.next_opus_header();
                    if let Ok(wire) = c.encrypt_packet(&hdr, &dave_protected) {
                        let _ = sock.send(&wire).await;
                    }
                }
            }

            udp_res = async {
                if let Some(sock) = udp_socket.as_ref() {
                    sock.recv(&mut udp_rx_buf).await
                } else {
                    std::future::pending().await
                }
            } => {
                let Ok(len) = udp_res else {
                    continue;
                };
                let datagram = &udp_rx_buf[..len];
                if is_rtcp_packet(datagram) {
                    continue;
                }
                let Some(c) = cipher.as_ref() else {
                    continue;
                };
                let Ok(decrypted_rtp) = c.decrypt_packet(datagram) else {
                    continue;
                };

                if let Ok(mut guard) = state.lock() {
                    let ssrc = decrypted_rtp.header.ssrc;
                    let maybe_user = guard.mixer.user_for_ssrc(ssrc);
                    let opus_bytes = match (maybe_user, guard.dave.as_mut()) {
                        (Some(uid), Some(dave)) => match dave.decrypt_opus(uid, &decrypted_rtp.payload) {
                            Ok(plain) => plain,
                            Err(_) => continue,
                        },
                        (None, Some(dave)) if !matches!(dave.state(), DaveSessionState::Disabled) => {
                            // SSRC not yet mapped to a user ID; wait for SPEAKING Op 5.
                            continue;
                        }
                        _ => decrypted_rtp.payload,
                    };
                    let _ = guard.mixer.push_opus_packet(
                        ssrc,
                        maybe_user,
                        decrypted_rtp.header.sequence,
                        opus_bytes,
                    );
                }
            }

            _ = mix_ticker.tick() => {
                let (mixed_any, levels_to_emit) = {
                    let Ok(mut guard) = state.lock() else {
                        continue;
                    };
                    let active = guard.mixer.mix_frame(&mut mix_buf);
                    vu_tick_counter = vu_tick_counter.wrapping_add(1);
                    let emit = if vu_tick_counter.is_multiple_of(3) {
                        let levels = guard.mixer.audio_levels_snapshot();
                        let loss = guard.mixer.packet_loss_pct();
                        if let Some(st) = guard.status.as_mut() {
                            st.packet_loss_pct = loss;
                            for (uid, rms, spk) in &levels {
                                if let Some(p) = st.participants.iter_mut().find(|p| p.user_id == *uid) {
                                    p.rms_db = *rms;
                                    p.speaking = *spk;
                                }
                            }
                            Some((st.channel_id, levels))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    (active > 0, emit)
                };

                if mixed_any {
                    playback_queue.push_samples(&mix_buf);
                }
                if let Some((channel_id, levels)) = levels_to_emit {
                    let _ = events.send(BackendEvent::VoiceAudioLevels { channel_id, levels });
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_and_guild_server_id_and_gateway_url_rules() {
        let guild_creds = VoiceGatewayCredentials {
            guild_id: Some(Snowflake(111)),
            channel_id: Snowflake(222),
            channel_name: Some("Lounge".into()),
            user_id: Snowflake(333),
            session_id: Zeroizing::new("sess".into()),
            token: Zeroizing::new("tok".into()),
            endpoint: "us-central1.discord.media:443".into(),
        };
        assert_eq!(guild_creds.server_id(), Snowflake(111));
        assert_eq!(
            guild_creds.gateway_ws_url(),
            "wss://us-central1.discord.media:443/?v=8"
        );

        let dm_creds = VoiceGatewayCredentials {
            guild_id: None,
            channel_id: Snowflake(999),
            channel_name: Some("Group DM Call".into()),
            user_id: Snowflake(333),
            session_id: Zeroizing::new("sess".into()),
            token: Zeroizing::new("tok".into()),
            endpoint: "ws://127.0.0.1:41234".into(),
        };
        // For 1:1 DM and Group DM calls (`guild_id: None`), Voice Gateway `server_id` MUST be `channel_id`!
        assert_eq!(dm_creds.server_id(), Snowflake(999));
        assert_eq!(dm_creds.gateway_ws_url(), "ws://127.0.0.1:41234/?v=8");

        let identify = build_identify_payload(&dm_creds);
        assert_eq!(identify["op"], VOICE_OP_IDENTIFY);
        assert_eq!(identify["d"]["server_id"], "999");
        assert_eq!(
            identify["d"]["max_dave_protocol_version"],
            MAX_DAVE_PROTOCOL_VERSION
        );
    }

    #[test]
    fn voice_server_update_redacts_token_in_debug() {
        let ev = VoiceServerUpdateEvent {
            guild_id: None,
            channel_id: Some(Snowflake(42)),
            endpoint: Some("voice.example.invalid".into()),
            token: Zeroizing::new("super-secret-voice-token".into()),
        };
        let dbg = format!("{ev:?}");
        assert!(!dbg.contains("super-secret-voice-token"));
        assert!(dbg.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn call_ring_state_transitions_for_dm_and_group_dm_calls() {
        let cfg = VoiceEngineConfig {
            audio_mode: AudioBackendMode::Headless,
            ..VoiceEngineConfig::new(Snowflake(100), "SysOp")
        };
        let engine = DiscordVoiceEngine::new(cfg, None).expect("engine");
        let mut rx = engine.subscribe_voice_events();

        engine.handle_call_update(CallUpdateEvent {
            channel_id: Snowflake(500),
            ringing: vec![Snowflake(100)],
            connected_user_ids: vec![Snowflake(200)],
            caller_id: Some(Snowflake(200)),
            caller_name: Some("Alice".into()),
            is_group_dm: true,
        });

        let ev = rx.recv().await.expect("call ring event");
        assert!(matches!(
            ev,
            BackendEvent::CallRingUpdated(CallRingState::IncomingRinging {
                channel_id: Snowflake(500),
                caller_id: Snowflake(200),
                is_group_dm: true,
                ..
            })
        ));

        engine.handle_call_delete(Snowflake(500));
        let ev2 = rx.recv().await.expect("call delete event");
        assert!(matches!(
            ev2,
            BackendEvent::CallRingUpdated(CallRingState::Idle)
        ));
    }

    #[tokio::test]
    async fn voice_state_follow_handles_guild_move_disconnect_and_hijack() {
        let user_id = Snowflake(1);
        let guild_id = Snowflake(2);
        let channel_id = Snowflake(3);
        let other_guild = Snowflake(5);

        let mk_joined = || {
            let cfg = VoiceEngineConfig {
                audio_mode: AudioBackendMode::Headless,
                ..VoiceEngineConfig::new(user_id, "SysOp")
            };
            let engine = DiscordVoiceEngine::new(cfg, None).expect("engine");
            {
                let mut g = engine.state.lock().expect("lock");
                g.active_session_id = Some("own-session".to_string());
                g.status = Some(VoiceChannelStatus {
                    guild_id: Some(guild_id),
                    channel_id,
                    channel_name: "Lounge".to_string(),
                    connection_state: VoiceConnectionState::Connected,
                    self_mute: false,
                    self_deaf: false,
                    push_to_talk: false,
                    ping_ms: Some(12),
                    packet_loss_pct: 0.0,
                    dave: DaveSessionState::Disabled,
                    call_ring: CallRingState::Idle,
                    participants: Vec::new(),
                });
            }
            engine
        };

        // 1. Moved to another channel in same guild -> updates channel_id.
        let e = mk_joined();
        e.handle_voice_state_update(VoiceStateUpdateEvent {
            guild_id: Some(guild_id),
            channel_id: Some(Snowflake(4)),
            user_id,
            session_id: "own-session".to_string(),
            self_mute: true,
            self_deaf: false,
            mute: false,
            deaf: false,
            self_stream: false,
            display_name: None,
        })
        .await;
        let st = e.status().await.expect("status").expect("still joined");
        assert_eq!(st.channel_id, Snowflake(4));
        assert!(st.self_mute);

        // 2. Disconnected from same guild -> leaves cleanly.
        let e = mk_joined();
        e.handle_voice_state_update(VoiceStateUpdateEvent {
            guild_id: Some(guild_id),
            channel_id: None,
            user_id,
            session_id: "own-session".to_string(),
            self_mute: false,
            self_deaf: false,
            mute: false,
            deaf: false,
            self_stream: false,
            display_name: None,
        })
        .await;
        assert!(e.status().await.expect("status").is_none());

        // 3. Connected from another client session -> leaves cleanly.
        let e = mk_joined();
        e.handle_voice_state_update(VoiceStateUpdateEvent {
            guild_id: Some(guild_id),
            channel_id: Some(channel_id),
            user_id,
            session_id: "other-client-session".to_string(),
            self_mute: false,
            self_deaf: false,
            mute: false,
            deaf: false,
            self_stream: false,
            display_name: None,
        })
        .await;
        assert!(e.status().await.expect("status").is_none());

        // 4. Leaving an unrelated guild must NOT drop the active voice session.
        let e = mk_joined();
        e.handle_voice_state_update(VoiceStateUpdateEvent {
            guild_id: Some(other_guild),
            channel_id: None,
            user_id,
            session_id: "own-session".to_string(),
            self_mute: false,
            self_deaf: false,
            mute: false,
            deaf: false,
            self_stream: false,
            display_name: None,
        })
        .await;
        assert_eq!(
            e.status().await.expect("status").expect("still joined").channel_id,
            channel_id
        );
    }

    #[test]
    fn heartbeat_payload_uses_minus_one_seq_ack_before_events() {
        let initial = build_heartbeat_payload(12345, None);
        assert_eq!(initial["op"], VOICE_OP_HEARTBEAT);
        assert_eq!(initial["d"]["t"], 12345);
        assert_eq!(initial["d"]["seq_ack"], -1);

        let sequenced = build_heartbeat_payload(67890, Some(42));
        assert_eq!(sequenced["d"]["seq_ack"], 42);
    }

    #[tokio::test]
    async fn zombie_heartbeat_ack_timeout_and_transient_dave_op27_resilience() {
        use crate::dave::OP_DAVE_MLS_PROPOSALS;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let observed_first_seq_ack = Arc::new(Mutex::new(None::<i64>));
        let observed_clone = Arc::clone(&observed_first_seq_ack);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("ws accept");
            let (mut tx, mut rx) = ws.split();

            // Send HELLO with 100 ms heartbeat interval.
            let _ = tx
                .send(WsMessage::Text(
                    json!({
                        "op": VOICE_OP_HELLO,
                        "d": { "heartbeat_interval": 100, "v": 8 }
                    })
                    .to_string()
                    .into(),
                ))
                .await;

            // Expect IDENTIFY first (heartbeat must NOT fire before first interval).
            if let Some(Ok(WsMessage::Text(first_msg))) = rx.next().await {
                let v: Value = serde_json::from_str(first_msg.as_str()).expect("json");
                assert_eq!(v["op"], VOICE_OP_IDENTIFY);
            }

            // Inject a malformed/premature Op 27 binary proposal (`seq=1, op=27, APPEND, garbage`)
            // BEFORE any DAVE session exists; client MUST degrade non-fatally and stay connected!
            let bad_op27 = vec![0x00, 0x01, OP_DAVE_MLS_PROPOSALS, 0x00, 0xFF, 0xEE];
            let _ = tx.send(WsMessage::Binary(bad_op27.into())).await;

            // Wait for the first heartbeat (never reply with HEARTBEAT_ACK so zombie detection fires).
            while let Some(Ok(msg)) = rx.next().await {
                if let WsMessage::Text(txt) = msg {
                    let v: Value = serde_json::from_str(txt.as_str()).expect("json");
                    if v["op"] == VOICE_OP_HEARTBEAT {
                        *observed_clone.lock().expect("lock") = v["d"]["seq_ack"].as_i64();
                        // Intentionally withhold HEARTBEAT_ACK so the next tick detects zombie!
                    }
                }
            }
        });

        let cfg = VoiceEngineConfig {
            audio_mode: AudioBackendMode::Headless,
            ..VoiceEngineConfig::new(Snowflake(77), "SysOp")
        };
        let engine = DiscordVoiceEngine::new(cfg, None).expect("engine");
        let mut events = engine.subscribe_voice_events();

        engine
            .connect_voice_gateway(VoiceGatewayCredentials {
                guild_id: Some(Snowflake(10)),
                channel_id: Snowflake(20),
                channel_name: Some("Test".into()),
                user_id: Snowflake(77),
                session_id: Zeroizing::new("s".into()),
                token: Zeroizing::new("t".into()),
                endpoint: format!("ws://{addr}"),
            })
            .await
            .expect("connect");

        // Wait for zombie heartbeat error banner + clean Disconnected state.
        let mut saw_zombie_error = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(BackendEvent::ErrorBanner { scope, message, .. })) =
                tokio::time::timeout(Duration::from_millis(200), events.recv()).await
                && scope == "voice"
                && message.contains("zombie")
            {
                saw_zombie_error = true;
                break;
            }
        }
        assert!(
            saw_zombie_error,
            "session must survive transient Op 27 error and cleanly detect missed HEARTBEAT_ACK zombie"
        );
        // First heartbeat occurred AFTER Op 27 (seq=1), proving session stayed alive past Op 27!
        assert_eq!(*observed_first_seq_ack.lock().expect("lock"), Some(1));

        let st = engine.status().await.expect("status").expect("status");
        assert_eq!(st.connection_state, VoiceConnectionState::Disconnected);

        engine.leave_voice().await.expect("leave");
        server.abort();
    }
}
